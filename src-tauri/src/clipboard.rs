//! 剪贴板历史：监听（读）→ 去重回声抑制 → 入库，浮层唤起/收起，粘贴回填。
//!
//! 平台分层：公共逻辑（回声抑制 / 入库 / 浮层窗口管理 / 图片转码）在本文件；
//! 系统剪贴板读写、事件源、粘贴注入、焦点管理在平台模块：
//! - Windows：`clipboard_win.rs`（Win32 剪贴板 + 消息窗口 + 鼠标钩子 + SendInput）
//! - macOS：`clipboard_mac.rs`（NSPasteboard + changeCount 轮询 + CGEvent Cmd+V）

use crate::commands::DbState;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl};

#[cfg(target_os = "windows")]
mod clipboard_win;
#[cfg(target_os = "macos")]
mod clipboard_mac;

#[cfg(target_os = "windows")]
use clipboard_win as platform;
#[cfg(target_os = "macos")]
use clipboard_mac as platform;

pub use platform::{set_clipboard, set_clipboard_files, set_clipboard_image};

/// 剪贴板浮窗窗口 label（App.vue 按此路由渲染 ClipboardOverlay）
pub const CLIPBOARD_WINDOW_LABEL: &str = "clipboard";
pub const CLIPBOARD_WIDTH: f64 = 520.0;
pub const CLIPBOARD_HEIGHT: f64 = 440.0;

/// 相同内容去重：内容一致的记录直接挪到最前，不新增重复条目
/// 自复制回声抑制窗口（粘贴/复制历史项后，事件监听会再次触发，用 hash 识别并跳过）
const ECHO_SUPPRESS_SECONDS: u64 = 10;
/// 富文本读取递增重试（Office 等分阶段写入剪贴板：先文本后 HTML）
const HTML_RETRY_DELAYS_MS: [u64; 7] = [0, 40, 80, 140, 220, 360, 560];
/// 剪贴板事件去抖：一次复制可能触发多次事件（多格式逐步写入）
const SETTLE_MS: u64 = 80;

/// 跨命令共享的剪贴板浮层状态。
/// prev_focus 在 Windows 存前台窗口句柄（hwnd），macOS 存前台应用 pid。
pub struct ClipboardState {
    /// 唤起浮层前聚焦的目标（粘贴时先恢复焦点再注入粘贴键）
    pub prev_focus: Mutex<Option<isize>>,
}

impl Default for ClipboardState {
    fn default() -> Self {
        Self {
            prev_focus: Mutex::new(None),
        }
    }
}

/// 最近一次「我们自己写入」的剪贴板内容指纹（文本+HTML 哈希 + 时间戳），
/// 用于抑制监听回声：粘贴/复制历史项会再次触发剪贴板事件，不应重复入库。
static LAST_SELF_SET: Mutex<Option<(u64, std::time::Instant)>> = Mutex::new(None);

/// 事件源线程可访问的应用句柄（Windows 监听线程里需要查浮层可见性）
static EVENT_APP: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();

fn event_app_handle() -> Option<AppHandle> {
    EVENT_APP.get().cloned()
}

/// 一次延迟窗口操作任务：粘贴后恢复焦点+注入按键，或收起后归还焦点。
/// 统一交给单一 worker 线程串行执行，避免频繁 spawn 短命线程造成线程数波动。
enum DelayedWinOp {
    /// 粘贴到唤起前窗口：必要时恢复目标焦点，再按目标应用注入粘贴快捷键
    Paste { target: isize, paste_method: &'static str },
    /// 收起浮层后归还焦点给唤起前窗口（若前台仍在本进程）
    RestoreFocus { target: isize },
}

/// 延迟窗口操作 worker：所有粘贴/归还焦点在同一个线程上串行执行
static WIN_OP_TX: Mutex<Option<std::sync::mpsc::Sender<DelayedWinOp>>> = Mutex::new(None);

/// 提交一次延迟窗口操作（粘贴或归还焦点）
fn submit_win_op(op: DelayedWinOp) {
    let tx = WIN_OP_TX.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = tx.as_ref() {
        let _ = tx.send(op);
    }
}

/// 初始化延迟窗口操作 worker（应用启动时调用一次）
pub fn init_win_op_worker() {
    let (tx, rx) = std::sync::mpsc::channel::<DelayedWinOp>();
    if let Ok(mut guard) = WIN_OP_TX.lock() {
        *guard = Some(tx);
    }
    std::thread::spawn(move || {
        while let Ok(op) = rx.recv() {
            match op {
                DelayedWinOp::Paste { target, paste_method } => {
                    platform::exec_paste(target, paste_method);
                }
                DelayedWinOp::RestoreFocus { target } => {
                    platform::exec_restore_focus(target);
                }
            }
        }
    });
}

fn content_hash(text: &str, html: Option<&str>) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    html.hash(&mut h);
    h.finish()
}

fn is_self_set(text: &str, html: Option<&str>) -> bool {
    let Ok(guard) = LAST_SELF_SET.lock() else {
        return false;
    };
    let Some((hash, at)) = guard.as_ref() else {
        return false;
    };
    at.elapsed().as_secs() < ECHO_SUPPRESS_SECONDS && *hash == content_hash(text, html)
}

/// 恢复记录时清除自复制指纹：暂停期间从浮窗复制/粘贴会写入指纹，
/// 恢复后短时间内复制相同内容会被误判为回声丢弃，这里重置为全新状态。
pub fn clear_self_set_fingerprint() {
    if let Ok(mut guard) = LAST_SELF_SET.lock() {
        *guard = None;
    }
}

/// 记录文本写入回声指纹（平台写入成功后调用）
pub(crate) fn record_text_echo(text: &str, html: Option<&str>) {
    if let Ok(mut guard) = LAST_SELF_SET.lock() {
        *guard = Some((content_hash(text, html), std::time::Instant::now()));
    }
}

/// 记录图片/文件写入回声指纹（平台写入成功后调用）
pub(crate) fn record_hash_echo(hash: u64) {
    if let Ok(mut guard) = LAST_SELF_SET.lock() {
        *guard = Some((hash, std::time::Instant::now()));
    }
}

/// 剪贴板读取结果（优先级：图片 > 文件 > 文本）
pub enum ClipboardPayload {
    Text { content: String, html: Option<String> },
    Image { bytes: Vec<u8>, format: ImageFormat },
    Files { paths: Vec<String> },
}

/// 图片原始格式（落盘快照时的编码依据）
#[derive(Clone, Copy, PartialEq)]
pub enum ImageFormat {
    /// PNG 剪贴板格式（现代应用主流），原样存 .png
    Png,
    /// CF_DIB / CF_DIBV5 位图，包装 14 字节文件头存 .bmp（Windows）
    Dib,
}

/// 图片/文件字节指纹：用于回声抑制与「相同内容只留一条」去重
pub(crate) fn hash_bytes_pub(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    hash_bytes_pub(bytes)
}

/// 文件列表指纹：路径 + 各文件大小（同名不同内容也能区分），用于回声抑制
fn hash_files(paths: &[String]) -> u64 {
    let mut h = DefaultHasher::new();
    for p in paths {
        p.hash(&mut h);
        if let Ok(meta) = std::fs::metadata(p) {
            meta.len().hash(&mut h);
        }
    }
    h.finish()
}

pub(crate) fn hash_files_pub(paths: &[String]) -> u64 {
    hash_files(paths)
}

/// 判断给定哈希是否为「我们自己写入」的回声（图片/文件路径）
fn is_self_set_hash(hash: u64) -> bool {
    let Ok(guard) = LAST_SELF_SET.lock() else {
        return false;
    };
    let Some((h, at)) = guard.as_ref() else {
        return false;
    };
    at.elapsed().as_secs() < ECHO_SUPPRESS_SECONDS && *h == hash
}

/// 把 CF_DIB 数据（BITMAPINFOHEADER + 调色板 + 像素，无文件头）包装成 BMP 文件字节。
/// 只补 14 字节 BITMAPFILEHEADER，不做像素重编码。
fn dib_to_bmp(dib: &[u8]) -> Vec<u8> {
    if dib.len() < 40 {
        return dib.to_vec();
    }
    let bi_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
    let bit_count = u16::from_le_bytes([dib[14], dib[15]]) as usize;
    let clr_used = if dib.len() >= 36 {
        u32::from_le_bytes([dib[32], dib[33], dib[34], dib[35]]) as usize
    } else {
        0
    };
    let pal_entries = if clr_used != 0 {
        clr_used
    } else if bit_count <= 8 {
        1usize << bit_count
    } else {
        0
    };
    let off_bits = 14 + bi_size + pal_entries * 4;
    let file_size = 14 + dib.len();
    let mut out = Vec::with_capacity(file_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    out.extend_from_slice(&(off_bits as u32).to_le_bytes());
    out.extend_from_slice(dib);
    out
}

/// 把 BMP 文件字节剥掉 14 字节文件头，得到 CF_DIB 数据（写回剪贴板用）
fn bmp_to_dib(bmp: &[u8]) -> &[u8] {
    if bmp.len() > 14 && &bmp[0..2] == b"BM" {
        &bmp[14..]
    } else {
        bmp
    }
}

/// DIB 字节 → BMP 文件字节（补 14 字节文件头；macOS 侧解码用）
pub(crate) fn dib_file_bytes(dib: &[u8]) -> Vec<u8> {
    dib_to_bmp(dib)
}

/// 剪贴板图片快照目录：`数据根/clipboard/images/`
fn clipboard_images_dir() -> Option<std::path::PathBuf> {
    let dir = crate::paths::data_root().join("clipboard").join("images");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// 图片落盘快照：PNG 原样存 .png，DIB 补文件头存 .bmp。
/// 返回快照绝对路径（失败返回 None，由调用方决定是否降级为文本）。
fn save_image_snapshot(bytes: &[u8], format: ImageFormat, hash: u64) -> Option<String> {
    let dir = clipboard_images_dir()?;
    let ext = match format {
        ImageFormat::Png => "png",
        ImageFormat::Dib => "bmp",
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("{:016x}_{}.{}", hash, nanos, ext);
    let path = dir.join(name);
    let data = match format {
        ImageFormat::Png => bytes.to_vec(),
        ImageFormat::Dib => dib_to_bmp(bytes),
    };
    std::fs::write(&path, &data).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// 快照字节按目标扩展名转码（png/bmp 两种，格式一致则原样返回）
pub fn transcode_image_bytes(bytes: &[u8], dest_ext: &str) -> Result<Vec<u8>, String> {
    let src_is_bmp = bytes.len() > 2 && &bytes[0..2] == b"BM";
    let src_is_png = bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let src = if src_is_bmp {
        "bmp"
    } else if src_is_png {
        "png"
    } else {
        return Err("图片快照不是 BMP/PNG 格式".into());
    };
    let dest = dest_ext.trim_start_matches('.').to_ascii_lowercase();
    if dest.is_empty() || src == dest {
        return Ok(bytes.to_vec());
    }
    let img = image::load_from_memory(bytes).map_err(|e| format!("解码图片失败: {}", e))?;
    let img = if src_is_bmp { normalize_bmp_alpha(img) } else { img };
    let fmt = match dest.as_str() {
        "png" => image::ImageFormat::Png,
        "bmp" => image::ImageFormat::Bmp,
        other => return Err(format!("不支持的导出格式: {}", other)),
    };
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, fmt)
        .map_err(|e| format!("编码图片失败: {}", e))?;
    Ok(out.into_inner())
}

/// BMP 解码结果的 alpha 兜底：整图 alpha 全 0 视为不透明，
/// 有任一非 0 则是真 alpha 通道，原样保留
fn normalize_bmp_alpha(img: image::DynamicImage) -> image::DynamicImage {
    let mut rgba = img.to_rgba8();
    if !rgba.pixels().any(|p| p[3] != 0) {
        for p in rgba.pixels_mut() {
            p[3] = 255;
        }
    }
    image::DynamicImage::ImageRgba8(rgba)
}

/// 剪贴板图片载荷 + 它该落的剪贴板格式，一次判定同时产出（BMP → CF_DIB，其余 → PNG 格式）
pub(crate) enum ClipboardImagePayload {
    /// BMP 剥掉 14 字节文件头后的 DIB，写 CF_DIB（Windows）
    Dib(Vec<u8>),
    /// PNG 原始字节或转码产物，写 PNG 剪贴板格式
    Png(Vec<u8>),
}

impl ClipboardImagePayload {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Dib(d) | Self::Png(d) => d,
        }
    }
}

/// 图片文件字节 → 写入剪贴板的载荷：BMP 剥 14 字节文件头成 CF_DIB；PNG 原样；
/// 其它格式（笔记图片的 jpg/webp/gif 等）解码重编码成 PNG。
/// macOS 不认 CF_DIB，Dib 载荷在平台层再转 PNG（见 clipboard_mac.rs）。
pub(crate) fn clipboard_image_payload(bytes: &[u8]) -> Result<ClipboardImagePayload, String> {
    if bytes.len() > 2 && &bytes[0..2] == b"BM" {
        return Ok(ClipboardImagePayload::Dib(bmp_to_dib(bytes).to_vec()));
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Ok(ClipboardImagePayload::Png(bytes.to_vec()));
    }
    let img = image::load_from_memory(bytes).map_err(|e| format!("解码图片失败: {}", e))?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("编码图片失败: {}", e))?;
    Ok(ClipboardImagePayload::Png(out.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 32bpp BI_RGB 的 BMP 文件字节（自底向上像素序，同 GDI 写剪贴板的形态）
    fn bmp32_file(width: u32, height: u32, px: &[[u8; 4]]) -> Vec<u8> {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes()); // biSize = BITMAPINFOHEADER
        dib.extend_from_slice(&(width as i32).to_le_bytes());
        dib.extend_from_slice(&(height as i32).to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes()); // planes
        dib.extend_from_slice(&32u16.to_le_bytes()); // bitCount
        dib.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        dib.extend_from_slice(&(width * height * 4).to_le_bytes()); // sizeImage
        dib.extend_from_slice(&0u32.to_le_bytes()); // xPelsPerMeter
        dib.extend_from_slice(&0u32.to_le_bytes()); // yPelsPerMeter
        dib.extend_from_slice(&0u32.to_le_bytes()); // clrUsed
        dib.extend_from_slice(&0u32.to_le_bytes()); // clrImportant
        for row in (0..height as usize).rev() {
            for col in 0..width as usize {
                let [r, g, b, a] = px[row * width as usize + col];
                // BMP 像素字节序是小端 DWORD 的 B,G,R,A
                dib.extend_from_slice(&[b, g, r, a]);
            }
        }
        dib_to_bmp(&dib)
    }

    #[test]
    fn transcode_bmp_to_png_preserves_pixels() {
        let px = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ];
        let bmp = bmp32_file(2, 2, &px);
        let png = transcode_image_bytes(&bmp, ".png").expect("转码应成功");
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        // 自底向上的存储经解码还原为自顶向下，逐像素一致
        for (i, e) in px.iter().enumerate() {
            assert_eq!(decoded.get_pixel((i % 2) as u32, (i / 2) as u32).0, *e);
        }
    }

    #[test]
    fn transcode_bmp_zero_alpha_becomes_opaque() {
        let px = [[10, 20, 30, 0], [40, 50, 60, 0], [70, 80, 90, 0], [1, 2, 3, 0]];
        let bmp = bmp32_file(2, 2, &px);
        let png = transcode_image_bytes(&bmp, "png").unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        for p in decoded.pixels() {
            assert_eq!(p[3], 255, "alpha 全 0 的 DIB 应按不透明导出");
        }
        assert_eq!(decoded.get_pixel(0, 0).0, [10, 20, 30, 255]);
    }

    #[test]
    fn transcode_same_format_passthrough() {
        let bmp = bmp32_file(2, 2, &[[1, 2, 3, 255]; 4]);
        assert_eq!(transcode_image_bytes(&bmp, "bmp").unwrap(), bmp);

        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let bytes = png.into_inner();
        assert_eq!(transcode_image_bytes(&bytes, ".png").unwrap(), bytes);
    }

    /// BMP 载荷剥文件头、PNG 载荷原样直通（写剪贴板不做无谓重编码）
    #[test]
    fn clipboard_payload_bmp_strips_header_png_passthrough() {
        let bmp = bmp32_file(2, 2, &[[5, 6, 7, 255]; 4]);
        assert_eq!(
            clipboard_image_payload(&bmp).unwrap().bytes(),
            &bmp[14..],
            "BMP 载荷应为剥掉 14 字节文件头的 CF_DIB"
        );

        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let bytes = png.into_inner();
        assert_eq!(clipboard_image_payload(&bytes).unwrap().bytes(), bytes);
    }

    /// 非 BMP/PNG 源（笔记图片的 jpg/webp/gif 等）必须转码成 PNG 再上剪贴板
    #[test]
    fn clipboard_payload_transcodes_other_formats_to_png() {
        let mut jpg = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(3, 2)
            .write_to(&mut jpg, image::ImageFormat::Jpeg)
            .unwrap();
        let payload = clipboard_image_payload(&jpg.into_inner())
            .expect("JPEG 应转码为 PNG 载荷")
            .bytes()
            .to_vec();
        assert!(payload.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]));
        let decoded = image::load_from_memory(&payload).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (3, 2));

        // 坏字节（既不是 BMP/PNG 也解不出图）明确报错，不静默写坏数据
        assert!(clipboard_image_payload(b"not an image").is_err());
    }
}

// ---- 浮层窗口 ----

/// 浮层窗口原生可见性（Windows 对 WebView2 子窗口 is_visible 不准，直查 Win32）
fn overlay_native_visible(win: &tauri::WebviewWindow) -> bool {
    #[cfg(target_os = "windows")]
    {
        match win.hwnd() {
            Ok(hwnd) => unsafe {
                clipboard_win::is_window_visible_by_hwnd(hwnd.0 as isize)
            },
            Err(_) => win.is_visible().unwrap_or(false),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        win.is_visible().unwrap_or(false)
    }
}

/// 唤起/收起剪贴板浮层（全局快捷键 / 悬浮球菜单触发）
pub fn toggle_overlay(app: &AppHandle) {
    // 已存在且可见：收起走统一 hide_overlay（注销热键/钩子 + 归还唤起前焦点）。
    // 不要在此覆盖 prev_focus——浮层被激活后前台是浮层自身，覆盖会把还原目标错写成浮层。
    if let Some(win) = app.get_webview_window(CLIPBOARD_WINDOW_LABEL) {
        if overlay_native_visible(&win) {
            log::info!("剪贴板浮层：toggle 时已可见，执行收起");
            hide_overlay(app);
            return;
        }
    }
    log::info!("剪贴板浮层：toggle 唤起");

    // 显示：记录唤起前的前台目标，供关闭/粘贴时还原焦点
    let prev = platform::foreground_focus_handle();
    if let Ok(mut guard) = app.state::<ClipboardState>().prev_focus.lock() {
        *guard = prev;
    }

    let Some(win) = app.get_webview_window(CLIPBOARD_WINDOW_LABEL) else {
        // 兜底：窗口不在（预创建失败或被销毁）时现场补建并显示
        if let Ok(win) = build_overlay_window(app) {
            log::info!("剪贴板浮层窗口已补建");
            show_ready_overlay(&win, app);
        }
        return;
    };
    show_ready_overlay(&win, app);
}

/// 构建剪贴板浮层窗口（visible=false，显示由调用方决定）
fn build_overlay_window(app: &AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    let mut builder = tauri::WebviewWindowBuilder::new(
        app,
        CLIPBOARD_WINDOW_LABEL,
        WebviewUrl::App("index.html".into()),
    )
    .title("剪贴板历史")
    .inner_size(CLIPBOARD_WIDTH, CLIPBOARD_HEIGHT)
    .resizable(false)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(false)
    .background_color(tauri::window::Color(0, 0, 0, 0))
    .additional_browser_args(crate::ADDITIONAL_BROWSER_ARGS);

    // 透明窗口在 Windows 上启用系统阴影会把边缘渲染成黑色描边（黑边），
    // 面板自带 CSS 阴影，OS 层阴影关闭即可（与便签浮窗一致）
    #[cfg(target_os = "windows")]
    {
        builder = builder.shadow(false);
    }
    builder.build()
}

/// 启动时预创建剪贴板浮层窗口（隐藏常驻），并接失焦自动收起（macOS 的
/// 「点击外部收起」；Windows 由低层鼠标钩子承担，此监听对它无害）
pub fn init_overlay_window(app: &AppHandle) {
    if app.get_webview_window(CLIPBOARD_WINDOW_LABEL).is_some() {
        return;
    }
    match build_overlay_window(app) {
        Ok(win) => {
            let app_handle = app.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::Focused(false) = event {
                    // 失焦即收起（幂等；hide_overlay 内部自行判断可见性）
                    hide_overlay(&app_handle);
                }
            });
            log::info!("剪贴板浮层窗口已预创建（隐藏常驻）");
        }
        Err(e) => log::warn!("剪贴板浮层窗口预创建失败: {}", e),
    }
}

/// 显示就绪的浮层：定位到鼠标附近 + 平台显示原语 + 热键/钩子 + 通知页面刷新
fn show_ready_overlay(win: &tauri::WebviewWindow, app: &AppHandle) {
    // 先恢复内存级别再显示（webview_mem：Low 态缓存已吐，首帧前回 Normal）
    crate::webview_mem::on_shown(app, CLIPBOARD_WINDOW_LABEL);
    // 每次唤起都重新定位到鼠标附近（窗口可能被拖走过、或显示器布局变化）
    if let Some((px, py)) = platform::cursor_anchor_position(app, CLIPBOARD_WIDTH, CLIPBOARD_HEIGHT)
    {
        let _ = win
            .set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(px, py)));
    }
    platform::show_overlay_no_activate(win);
    platform::register_esc_hotkey();
    platform::install_mouse_hook();
    let _ = app.emit_to(CLIPBOARD_WINDOW_LABEL, "clipboard-shown", ());
}

/// 激活浮层：用户点击搜索框开始键盘操作时调用
pub fn activate_overlay(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(CLIPBOARD_WINDOW_LABEL) {
        platform::activate_overlay_focus(&win);
    }
}

/// 收起浮层：仅隐藏（窗口常驻，不做销毁回收）。
fn hide_overlay_window(win: &tauri::WebviewWindow) {
    // 隐藏后把常驻 renderer 的内存目标级别降低（webview_mem，轮询兜底）
    crate::webview_mem::on_hidden(win.app_handle(), win.label());
    platform::hide_window_fast(win);
}

/// 收起浮层：隐藏后若前台仍在本应用（浮层/主窗口），把焦点还给唤起前的目标
pub fn hide_overlay(app: &AppHandle) {
    platform::unregister_esc_hotkey();
    platform::uninstall_mouse_hook();
    let Some(win) = app.get_webview_window(CLIPBOARD_WINDOW_LABEL) else {
        return;
    };
    let was_visible = overlay_native_visible(&win);
    hide_overlay_window(&win);
    if !was_visible {
        return;
    }
    let prev = app
        .state::<ClipboardState>()
        .prev_focus
        .lock()
        .map(|g| *g)
        .unwrap_or(None);
    let Some(target) = prev else {
        return;
    };
    // 提交到常驻 worker 串行执行：延迟 100ms 后归还焦点
    submit_win_op(DelayedWinOp::RestoreFocus { target });
}

/// 监听线程直接收起（Esc 热键 / 鼠标钩子点击外部）：跨线程安全出口
pub(crate) fn hide_overlay_from_listener() {
    if let Some(app) = event_app_handle() {
        hide_overlay(&app);
    }
}

/// 粘贴到唤起浮层前聚焦的窗口：
/// - 本应用主窗口：隐藏浮层后直接向主窗口 JS 派发内容，由其插回原输入框
/// - 外部窗口：隐藏浮层 →（若前台被我们抢占则恢复目标焦点）→ 按目标应用发送粘贴快捷键
pub fn paste_to_previous_window(app: &AppHandle, content: &str, html: Option<&str>) {
    platform::unregister_esc_hotkey();
    platform::uninstall_mouse_hook();
    if let Some(win) = app.get_webview_window(CLIPBOARD_WINDOW_LABEL) {
        hide_overlay_window(&win);
    }
    let prev = app
        .state::<ClipboardState>()
        .prev_focus
        .lock()
        .map(|g| *g)
        .unwrap_or(None);
    let Some(target) = prev else {
        log::warn!("剪贴板粘贴：未记录到唤起前窗口，仅写入剪贴板");
        return;
    };
    log::info!("剪贴板粘贴：唤起前目标 = {target:#x}");

    // 本应用主窗口 + 文本内容：直接派发内容给主窗口 JS 插入（浮层已隐藏，无需恢复焦点）。
    // 图片/文件（content 为空）不在此分支——走下方注入路径，目标应用读剪贴板对应格式。
    if !content.is_empty() && platform::is_main_window(app, target) {
        let payload = serde_json::json!({
            "content": content,
            "html": html,
        });
        if let Err(e) = app.emit_to("main", "clipboard-paste-request", payload) {
            log::warn!("剪贴板粘贴：向主窗口派发插入请求失败: {}", e);
        }
        return;
    }

    // 提交到常驻 worker 串行执行（粘贴时序依赖窗口隐藏/焦点转移，延迟需按序进行）
    let paste_method =
        platform::resolve_paste_method(&crate::config::load().clipboard_paste_method, target);
    submit_win_op(DelayedWinOp::Paste {
        target,
        paste_method,
    });
}

/// 入库辅助：拿数据库连接并执行给定 repo 操作（监听线程跨线程访问 DbState）
fn insert_into_db<T>(
    app: &AppHandle,
    f: impl FnOnce(&rusqlite::Connection) -> Result<T, String>,
) -> Result<T, String> {
    let state = app.try_state::<DbState>().ok_or("剪贴板状态未就绪")?;
    let conn = state.0.lock().map_err(|e| e.to_string())?;
    f(&conn)
}

/// 处理一次剪贴板变化事件：读取 → 回声抑制 → 入库。
/// 由独立 worker 线程调用（去抖已在 worker 完成），这里不做额外 sleep。
fn handle_clipboard_update(app: &AppHandle) {
    // 暂停记录时跳过（暂停前已入库的数据保留）
    let cfg = crate::config::load();
    if cfg.clipboard_paused {
        return;
    }

    let payload = platform::read_clipboard_payload().or_else(|| {
        // 读取失败（复制方仍占用剪贴板）：稍等重试同一条，避免内容被锁定时静默丢失
        std::thread::sleep(std::time::Duration::from_millis(120));
        platform::read_clipboard_payload()
    });
    let Some(payload) = payload else {
        return;
    };

    let source = platform::foreground_app_name();
    let result: Result<(), String> = match payload {
        ClipboardPayload::Text { content, html } => {
            if content.trim().is_empty() {
                return;
            }
            // 自复制回声：粘贴/复制历史项后系统会再次触发事件，跳过自身写入
            if is_self_set(&content, html.as_deref()) {
                log::debug!("剪贴板监听：忽略自身文本写入回声");
                return;
            }
            insert_into_db(app, |conn| {
                crate::repo::clipboard::insert(conn, &content, html.as_deref(), source.as_deref())
                    .map_err(|e| e.to_string())
            })
        }
        ClipboardPayload::Image { bytes, format } => {
            if !cfg.clipboard_image_enabled {
                return;
            }
            let hash = hash_bytes(&bytes);
            if is_self_set_hash(hash) {
                log::debug!("剪贴板监听：忽略自身图片写入回声");
                return;
            }
            let Some(path) = save_image_snapshot(&bytes, format, hash) else {
                log::warn!("剪贴板图片快照落盘失败，已跳过");
                return;
            };
            let dedup_key = format!("{:016x}", hash);
            match insert_into_db(app, |conn| {
                crate::repo::clipboard::insert_image(conn, &dedup_key, &path, source.as_deref())
                    .map_err(|e| e.to_string())
            }) {
                Ok(true) => Ok(()),
                Ok(false) => {
                    // 相同图片已存在（去重挪到最前），删除本次落盘的冗余快照
                    let _ = std::fs::remove_file(&path);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        ClipboardPayload::Files { paths } => {
            if !cfg.clipboard_file_enabled {
                return;
            }
            let hash = hash_files(&paths);
            if is_self_set_hash(hash) {
                log::debug!("剪贴板监听：忽略自身文件写入回声");
                return;
            }
            insert_into_db(app, |conn| {
                crate::repo::clipboard::insert_files(conn, &paths, source.as_deref())
                    .map_err(|e| e.to_string())
            })
        }
    };

    if let Err(e) = result {
        log::warn!("剪贴板历史入库失败: {}", e);
    }
}

/// 启动剪贴板监听（Q8：启动零加载，不碰历史数据；仅在剪贴板变化时落库）。
/// 平台事件源（Windows 消息窗口 / macOS changeCount 轮询）→ 去抖 worker → 入库。
pub fn start_monitor(app: AppHandle) {
    let _ = EVENT_APP.set(app.clone());
    // 剪贴板内容读取 + 入库放到独立 worker 线程：去抖沉降后只处理最新状态。
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let worker_app = app.clone();
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            // 一次复制可能触发多次事件（多格式逐步写入），
            // 沉降期内持续吞掉新事件，只保留最后一次再处理。
            std::thread::sleep(std::time::Duration::from_millis(SETTLE_MS));
            while rx.try_recv().is_ok() {}
            handle_clipboard_update(&worker_app);
        }
    });

    platform::spawn_event_source(tx);
}
