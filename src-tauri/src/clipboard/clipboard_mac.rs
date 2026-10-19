//! 剪贴板 macOS 实现（NSPasteboard + CGEvent + NSWorkspace）。
//!
//! 与 Windows 版的机制差异（见 clipboard.rs 顶部公共说明）：
//! - 事件源：macOS 没有 AddClipboardFormatListener 等价物，用 NSPasteboard
//!   `changeCount` 轮询（约 400ms，业界剪贴板工具通行做法）；
//! - 粘贴回填：CGEvent 合成 Cmd+V；合成输入事件需要「辅助功能」权限
//!   （AXIsProcessTrusted），未授权时仅写入剪贴板并记日志；
//! - 浮层焦点：无 WS_EX_NOACTIVATE 等价物，浮层显示即激活（点击外部收起
//!   由窗口失焦事件驱动，Esc 由浮层自身键盘监听处理）；
//! - 前台窗口标识：prev_focus 存的是前台应用 pid（Windows 存 hwnd）。

use super::{ClipboardPayload, ImageFormat};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::ClassType;
use objc2_app_kit::{
    NSBitmapImageRep, NSPasteboard, NSPasteboardTypeFileURL, NSPasteboardTypeHTML,
    NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF, NSPasteboardWriting,
    NSRunningApplication, NSWorkspace,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString, NSURL};
use std::time::Duration;
use tauri::AppHandle;

// ---- CGEvent / 辅助功能权限 FFI（无现成绑定的部分直接声明） ----

/// kCGEventFlagMaskCommand
const KCG_EVENT_FLAG_MASK_COMMAND: u64 = 1 << 20;
/// 虚拟键码 V（HID 键盘布局）
const VKEY_V: u16 = 9;
/// kCGHIDEventTap（会话级事件注入点）
const KCG_HID_EVENT_TAP: i32 = 0;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(stateID: i32) -> *mut core::ffi::c_void;
    fn CGEventCreateKeyboardEvent(
        source: *mut core::ffi::c_void,
        virtualKey: u16,
        keyDown: bool,
    ) -> *mut core::ffi::c_void;
    fn CGEventSetFlags(event: *mut core::ffi::c_void, flags: u64);
    fn CGEventPost(tap: i32, event: *mut core::ffi::c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *mut core::ffi::c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// 系统剪贴板（NSPasteboard.generalPasteboard）
fn pasteboard() -> Retained<NSPasteboard> {
    unsafe { NSPasteboard::generalPasteboard() }
}

/// 读取纯文本 + HTML 片段。HTML 优先 "Apple HTML pasteboard type"（NSPasteboardTypeHTML），
/// 兜底 "public.html"（部分跨平台框架写的是这个 UTI）。
pub fn read_clipboard() -> Option<(String, Option<String>)> {
    let pb = pasteboard();
    let text: Option<String> = unsafe { pb.stringForType(NSPasteboardTypeString) }
        .map(|s| s.to_string());
    let text = text.filter(|t| !t.trim().is_empty());
    let html = read_html(&pb);
    if text.is_none() && html.is_none() {
        None
    } else {
        Some((text.unwrap_or_default(), html))
    }
}

fn read_html(pb: &NSPasteboard) -> Option<String> {
    for ty in [unsafe { NSPasteboardTypeHTML }, &html_uti()] {
        if let Some(data) = unsafe { pb.dataForType(ty) } {
            let s = String::from_utf8_lossy(&data.to_vec()).into_owned();
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn html_uti() -> Retained<NSString> {
    NSString::from_str("public.html")
}

/// macOS 无分阶段写入问题（NSPasteboard 单事务），无需重试
pub fn read_clipboard_with_retry() -> Option<(String, Option<String>)> {
    read_clipboard()
}

/// 读取图片：PNG 直通；TIFF 转 PNG（NSBitmapImageRep）；返回统一 PNG 字节
pub fn try_read_image() -> Option<(Vec<u8>, ImageFormat)> {
    let pb = pasteboard();
    // 1. PNG 类型直通
    for ty in [unsafe { NSPasteboardTypePNG }, &NSString::from_str("public.png")] {
        if let Some(data) = unsafe { pb.dataForType(ty) } {
            let bytes = data.to_vec();
            if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
                return Some((bytes, ImageFormat::Png));
            }
        }
    }
    // 2. TIFF → PNG（截图工具只写 TIFF）
    if let Some(data) = unsafe { pb.dataForType(NSPasteboardTypeTIFF) } {
        if let Some(png) = tiff_data_to_png(&data) {
            return Some((png, ImageFormat::Png));
        }
    }
    None
}

/// TIFF 字节 → PNG 字节（NSBitmapImageRep 解码再编码，无 image crate tiff feature 依赖）
fn tiff_data_to_png(tiff: &NSData) -> Option<Vec<u8>> {
    let rep = NSBitmapImageRep::imageRepWithData(tiff)?;
    let png = unsafe {
        rep.representationUsingType_properties(
            objc2_app_kit::NSBitmapImageFileType::PNG,
            &NSDictionary::new(),
        )
    }?;
    Some(png.to_vec())
}

/// PNG 字节 → TIFF 字节（写剪贴板时给老应用兜底，它们只认 TIFF 类型）
fn png_data_to_tiff(png: &NSData) -> Option<Retained<NSData>> {
    let rep = NSBitmapImageRep::imageRepWithData(png)?;
    unsafe {
        rep.representationUsingType_properties(
            objc2_app_kit::NSBitmapImageFileType::TIFF,
            &NSDictionary::new(),
        )
    }
}

/// 读取文件列表：pasteboardItems 里的 public.file-url（file:// URL 字符串 → 本地路径）
pub fn try_read_files() -> Option<Vec<String>> {
    let pb = pasteboard();
    let items = unsafe { pb.pasteboardItems() }?;
    let file_url_ty: &'static objc2_app_kit::NSPasteboardType = unsafe { NSPasteboardTypeFileURL };
    let mut paths = Vec::new();
    for item in items.iter() {
        let Some(url_str) = (unsafe { item.stringForType(file_url_ty) }) else {
            continue;
        };
        if let Some(p) = file_url_to_path(&url_str.to_string()) {
            paths.push(p);
        }
    }
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// file:// URL 字符串 → 本地路径（percent-decode + 去掉 localhost 主机段）
fn file_url_to_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let path = percent_encoding::percent_decode_str(rest)
        .decode_utf8()
        .ok()?
        .into_owned();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

/// 读取剪贴板并按类型分派：图片 > 文件 > 文本（与 Windows 版语义一致）
pub fn read_clipboard_payload() -> Option<ClipboardPayload> {
    if let Some((bytes, format)) = try_read_image() {
        return Some(ClipboardPayload::Image { bytes, format });
    }
    if let Some(paths) = try_read_files() {
        return Some(ClipboardPayload::Files { paths });
    }
    let (text, html) = read_clipboard_with_retry()?;
    Some(ClipboardPayload::Text { content: text, html })
}

/// 写入文本 + 可选 HTML（UTF-8 字节直接放 public.html）
pub fn set_clipboard(text: &str, html: Option<&str>) -> Result<(), String> {
    let pb = pasteboard();
    unsafe { pb.clearContents() };
    let ns_text = NSString::from_str(text);
    let ok = unsafe { pb.setString_forType(&ns_text, NSPasteboardTypeString) };
    if !ok {
        return Err("写入系统剪贴板失败".into());
    }
    if let Some(h) = html {
        if !h.trim().is_empty() {
            let data = NSData::with_bytes(h.as_bytes());
            unsafe {
                pb.setData_forType(Some(&data), NSPasteboardTypeHTML);
                pb.setData_forType(Some(&data), &html_uti());
            }
        }
    }
    super::record_text_echo(text, html);
    Ok(())
}

/// 写入图片：统一 PNG 类型 + TIFF 兜底（老应用兼容）。
/// 公共签名与 Windows 版一致（按快照路径写入）；Dib 载荷在 macOS 无 CF_DIB 概念，
/// 统一转 PNG 后写。
pub fn set_clipboard_image(path: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取图片快照失败: {}", e))?;
    let payload = super::clipboard_image_payload(&bytes)?;
    let png_bytes: Vec<u8> = match payload {
        super::ClipboardImagePayload::Dib(dib) => {
            // BMP（剥头的 DIB）→ 补文件头解码 → PNG
            let bmp = super::dib_file_bytes(&dib);
            let img = image::load_from_memory(&bmp).map_err(|e| format!("解码图片失败: {e}"))?;
            let mut out = std::io::Cursor::new(Vec::new());
            img.write_to(&mut out, image::ImageFormat::Png)
                .map_err(|e| format!("编码图片失败: {e}"))?;
            out.into_inner()
        }
        super::ClipboardImagePayload::Png(p) => p,
    };
    let hash = super::hash_bytes_pub(&png_bytes);
    let pb = pasteboard();
    unsafe { pb.clearContents() };
    let data = NSData::with_bytes(&png_bytes);
    let ok = unsafe { pb.setData_forType(Some(&data), NSPasteboardTypePNG) };
    if !ok {
        return Err("写入系统剪贴板失败".into());
    }
    if let Some(tiff) = png_data_to_tiff(&data) {
        unsafe { pb.setData_forType(Some(&tiff), NSPasteboardTypeTIFF) };
    }
    super::record_hash_echo(hash);
    Ok(())
}

/// 写入文件引用（NSURL file URLs，writeObjects 是 Finder/应用认的标准形态）
pub fn set_clipboard_files(paths: &[String]) -> Result<(), String> {
    let existing: Vec<&str> = paths
        .iter()
        .map(|s| s.as_str())
        .filter(|p| std::path::Path::new(p).exists())
        .collect();
    if existing.is_empty() {
        return Err("文件已不存在，无法粘贴".into());
    }
    let owned: Vec<String> = existing.iter().map(|s| s.to_string()).collect();
    let hash = super::hash_files_pub(&owned);
    let pb = pasteboard();
    unsafe { pb.clearContents() };
    let objs: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = existing
        .iter()
        .map(|p| {
            let ns = NSString::from_str(p);
            let url = NSURL::fileURLWithPath(&ns);
            ProtocolObject::from_retained(url)
        })
        .collect();
    let refs: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = objs.iter().map(|o| &**o).collect();
    let ok = unsafe { pb.writeObjects(&NSArray::from_slice(&refs)) };
    if !ok {
        return Err("写入系统剪贴板失败".into());
    }
    super::record_hash_echo(hash);
    Ok(())
}

// ---- 前台应用与粘贴注入 ----

/// 前台应用 pid（作为 prev_focus 标识；无前台应用返回 None）
pub fn foreground_focus_handle() -> Option<isize> {
    frontmost_app().map(|app| unsafe { app.processIdentifier() } as isize)
}

/// 前台应用名（作为剪贴板历史「来源应用」）
pub fn foreground_app_name() -> Option<String> {
    let app = frontmost_app()?;
    let name = unsafe { app.localizedName() }?;
    Some(name.to_string())
}

fn frontmost_app() -> Option<Retained<NSRunningApplication>> {
    let ws = unsafe { NSWorkspace::sharedWorkspace() };
    unsafe { ws.frontmostApplication() }
}

/// 判断 prev_focus（pid）是否属于本进程（主窗口粘贴走 JS 派发路径）
pub fn is_main_window(_app: &AppHandle, prev: isize) -> bool {
    prev == std::process::id() as isize
}

/// 粘贴方式：macOS 所有应用（含终端）都认 Cmd+V，统一返回 cmd_v
pub fn resolve_paste_method(_cfg: &str, _prev: isize) -> &'static str {
    "cmd_v"
}

/// worker：恢复目标应用前台 + 注入 Cmd+V
pub fn exec_paste(prev: isize, _method: &'static str) {
    // 先给系统时间完成浮层隐藏与焦点转移
    std::thread::sleep(Duration::from_millis(50));
    if !unsafe { AXIsProcessTrusted() } {
        log::warn!(
            "剪贴板粘贴：未授予「辅助功能」权限，无法注入 Cmd+V（内容已写入剪贴板，可手动 ⌘V）；\
             在 系统设置 → 隐私与安全性 → 辅助功能 中允许 x-hub"
        );
        return;
    }
    activate_app(prev);
    // 沉降：等焦点真正移交
    std::thread::sleep(Duration::from_millis(150));
    send_cmd_v();
}

/// worker：归还唤起前应用焦点（若前台仍是本进程）
pub fn exec_restore_focus(prev: isize) {
    std::thread::sleep(Duration::from_millis(100));
    let front_pid = frontmost_app()
        .as_ref()
        .map(|a| unsafe { a.processIdentifier() } as isize);
    if front_pid == Some(std::process::id() as isize) {
        activate_app(prev);
    }
}

/// 把指定 pid 的应用带到前台（NSRunningApplication activate）
fn activate_app(pid: isize) {
    let app =
        unsafe { NSRunningApplication::runningApplicationWithProcessIdentifier(pid as i32) };
    let Some(app) = app else {
        log::warn!("剪贴板粘贴：目标应用（pid {pid}）已退出，无法恢复焦点");
        return;
    };
    // ActivateIgnoringOtherApps 在 macOS 14+ 已弃用（无效果但无害）：本进程从前台让位，
    // 常规 activate 即可完成焦点移交；保留 Ignoring 标志位兼容旧系统
    #[allow(deprecated)]
    let opts = objc2_app_kit::NSApplicationActivationOptions::ActivateIgnoringOtherApps;
    unsafe { app.activateWithOptions(opts) };
}

/// 合成 Cmd+V（keydown + keyup 都带 Command 标志）
fn send_cmd_v() {
    unsafe {
        let source = CGEventSourceCreate(0); // kCGEventSourceStateCombinedSessionState
        if source.is_null() {
            log::warn!("剪贴板粘贴：CGEventSourceCreate 失败");
            return;
        }
        let down = CGEventCreateKeyboardEvent(source, VKEY_V, true);
        if down.is_null() {
            CFRelease(source);
            return;
        }
        let up = CGEventCreateKeyboardEvent(source, VKEY_V, false);
        CGEventSetFlags(down, KCG_EVENT_FLAG_MASK_COMMAND);
        if !up.is_null() {
            CGEventSetFlags(up, KCG_EVENT_FLAG_MASK_COMMAND);
        }
        CGEventPost(KCG_HID_EVENT_TAP, down);
        CGEventPost(KCG_HID_EVENT_TAP, up);
        CFRelease(down);
        if !up.is_null() {
            CFRelease(up);
        }
        CFRelease(source);
    }
}

/// 事件源线程：轮询 NSPasteboard changeCount（约 400ms），变化即通知 worker
pub fn spawn_event_source(tx: std::sync::mpsc::Sender<()>) {
    std::thread::spawn(move || {
        log::info!("剪贴板监听线程启动（NSPasteboard changeCount 轮询）");
        let pb = pasteboard();
        let mut last = unsafe { pb.changeCount() };
        loop {
            std::thread::sleep(Duration::from_millis(400));
            let cur = unsafe { pb.changeCount() };
            if cur != last {
                last = cur;
                let _ = tx.send(());
            }
        }
    });
}

/// 浮层定位：光标附近 + 所在显示器工作区钳制（tauri 跨平台 API，物理像素）
pub fn cursor_anchor_position(
    app: &AppHandle,
    width: f64,
    height: f64,
) -> Option<(i32, i32)> {
    let pos = app.cursor_position().ok()?;
    let (cx, cy) = (pos.x as i32, pos.y as i32);
    let monitors = app.available_monitors().ok()?;
    let monitor = monitors
        .iter()
        .find(|m| {
            let p = m.position();
            let s = m.size();
            cx >= p.x
                && cx < p.x + s.width as i32
                && cy >= p.y
                && cy < p.y + s.height as i32
        })
        .cloned()
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let wa = monitor.work_area();
    let w = width as i32;
    let h = height as i32;
    let x = (cx - w / 2).clamp(wa.position.x, wa.position.x + wa.size.width as i32 - w);
    let y = (cy - 30).clamp(wa.position.y, wa.position.y + wa.size.height as i32 - h);
    Some((x, y))
}

/// Esc 热键（Windows 专用的无激活兜底）：macOS 浮层持焦点，前端自身监听 Esc
pub fn register_esc_hotkey() {}
pub fn unregister_esc_hotkey() {}

/// 低层鼠标钩子（Windows 专用）：macOS 用窗口失焦事件收起（见 clipboard.rs）
pub fn install_mouse_hook() {}
pub fn uninstall_mouse_hook() {}

/// 激活浮层（用户点击搜索框开始键盘操作时调用）
pub fn activate_overlay_focus(win: &tauri::WebviewWindow) {
    let _ = win.set_focus();
}

/// 无激活显示：macOS 直接 show（失焦收起由事件驱动）
pub fn show_overlay_no_activate(win: &tauri::WebviewWindow) {
    crate::win_taskbar::show(win);
}

/// 隐藏浮层（macOS 直接 hide）
pub fn hide_window_fast(win: &tauri::WebviewWindow) {
    let _ = win.hide();
}

/// ClassType 仅为 NSURL 静态类型引用保留（当前实现无需运行时类查询）
#[allow(unused)]
use ClassType as _;
