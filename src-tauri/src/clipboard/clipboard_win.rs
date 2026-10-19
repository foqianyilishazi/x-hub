//! 剪贴板 Windows 实现（Win32 剪贴板 + 消息窗口监听 + 鼠标钩子 + SendInput）。
//! 原 clipboard.rs 的全部 Win32 专属代码抽取至此，与 clipboard_mac.rs 接口对齐；
//! 公共逻辑（回声抑制 / 入库 / 浮层管理 / 图片转码）留在 clipboard.rs。

use super::{ClipboardPayload, ImageFormat};
use std::sync::Mutex;
use tauri::AppHandle;
use windows_sys::Win32::Foundation::{GlobalFree, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, RemoveClipboardFormatListener,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows_sys::Win32::System::Threading::AttachThreadInput;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, RegisterHotKey, SendInput, UnregisterHotKey, INPUT, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC, MapVirtualKeyW,
    VK_CONTROL, VK_ESCAPE, VK_INSERT, VK_LMENU, VK_LCONTROL, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowLongPtrW,
    GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, PostMessageW,
    RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, SetWindowsHookExW, SetWindowPos,
    ShowWindow, TranslateMessage, UnhookWindowsHookEx, UnregisterClassW, GWL_EXSTYLE,
    HWND_MESSAGE, HWND_TOPMOST, MSG, MSLLHOOKSTRUCT, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNA, SW_RESTORE, WH_MOUSE_LL, WNDCLASSW, WS_EX_NOACTIVATE,
    WM_APP, WM_CLIPBOARDUPDATE, WM_HOTKEY, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN,
};

// ---- 状态（Windows 专属） ----

/// 全局鼠标钩子句柄（浮层可见时启用，点击浮层外部即收起）
pub static MOUSE_HOOK: Mutex<Option<isize>> = Mutex::new(None);

/// 浮层窗口句柄缓存：钩子回调运行在监听线程，禁止跨线程 Tauri 调用，
/// 显示浮层时写入，钩子回调只读它做矩形判断。
pub static OVERLAY_HWND: Mutex<Option<isize>> = Mutex::new(None);

/// 剪贴板监听消息窗口句柄（Esc 兜底热键注册到该窗口，由监听线程消息循环分发 WM_HOTKEY）
static LISTENER_HWND: Mutex<Option<isize>> = Mutex::new(None);
/// Esc 兜底关闭热键 ID
const ESC_HOTKEY_ID: i32 = 0x5C43;

// ---- 读取 ----

pub fn read_utf16_null_terminated(ptr: *const u16, max_chars: usize) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    unsafe {
        while len < max_chars && *ptr.add(len) != 0 {
            len += 1;
        }
    }
    if len == 0 {
        return String::new();
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf16_lossy(slice)
}

/// 从 CF_HTML 数据中提取实际片段（StartFragment/EndFragment 偏移优先，标签兜底）
pub fn extract_cf_html_fragment(bytes: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(bytes);
    let parse = |key: &str| -> Option<usize> {
        let i = s.find(key)?;
        let digits: String = s[i + key.len()..]
            .chars()
            .take(10)
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse::<usize>().ok()
    };
    if let (Some(a), Some(b)) = (parse("StartFragment:"), parse("EndFragment:")) {
        if b > a && b <= s.len() {
            return Some(s[a..b].to_string());
        }
    }
    let a = s.find("<!--StartFragment-->")?;
    let b = s.find("<!--EndFragment-->")?;
    let start = a + "<!--StartFragment-->".len();
    if b > start {
        Some(s[start..b].to_string())
    } else {
        None
    }
}

/// 直查 Win32 IsWindowVisible（Windows 上 WebView2 子窗口的 tauri is_visible 不准）
pub fn is_window_visible_by_hwnd(hwnd: isize) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    unsafe { IsWindowVisible(hwnd as *mut core::ffi::c_void) != 0 }
}

pub fn read_clipboard() -> Option<(String, Option<String>)> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }

        let mut text: Option<String> = None;
        let handle = GetClipboardData(CF_UNICODETEXT as u32);
        if !handle.is_null() {
            let ptr = GlobalLock(handle);
            if !ptr.is_null() {
                let t = read_utf16_null_terminated(ptr as *const u16, 1_000_000);
                if !t.trim().is_empty() {
                    text = Some(t);
                }
                GlobalUnlock(handle);
            }
        }

        let html = read_html_fragment_unlocked();

        CloseClipboard();
        if text.is_none() && html.is_none() {
            None
        } else {
            Some((text.unwrap_or_default(), html))
        }
    }
}

/// 读取剪贴板 HTML 片段（调用方需已 OpenClipboard）
unsafe fn read_html_fragment_unlocked() -> Option<String> {
    let fmt = RegisterClipboardFormatW(windows_sys::core::w!("HTML Format"));
    if fmt == 0 {
        return None;
    }
    let h = GetClipboardData(fmt);
    if h.is_null() {
        return None;
    }
    let ptr = GlobalLock(h);
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while len < 2_000_000 && *(ptr as *const u8).add(len) != 0 {
        len += 1;
    }
    let html = if len > 0 {
        let bytes = std::slice::from_raw_parts(ptr as *const u8, len);
        extract_cf_html_fragment(bytes)
    } else {
        None
    };
    GlobalUnlock(h);
    html
}

/// 富文本分阶段写入重试（Office 等先文本后 HTML，见常量 HTML_RETRY_DELAYS_MS）
pub fn read_clipboard_with_retry() -> Option<(String, Option<String>)> {
    let (text, html) = read_clipboard()?;
    if html.is_some() || text.trim().is_empty() {
        return Some((text, html));
    }
    let has_html_format = unsafe {
        let fmt = RegisterClipboardFormatW(windows_sys::core::w!("HTML Format"));
        fmt != 0 && IsClipboardFormatAvailable(fmt) != 0
    };
    if !has_html_format {
        return Some((text, html));
    }
    for &d in &super::HTML_RETRY_DELAYS_MS[1..] {
        if d > 0 {
            std::thread::sleep(std::time::Duration::from_millis(d));
        }
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                let html = read_html_fragment_unlocked();
                CloseClipboard();
                if let Some(h) = html {
                    if !h.trim().is_empty() {
                        return Some((text, Some(h)));
                    }
                }
            }
        }
    }
    Some((text, html))
}

/// 读取剪贴板图片（调用方需已 OpenClipboard）：PNG 优先，否则 CF_DIBV5 → CF_DIB
unsafe fn read_image_unlocked() -> Option<(Vec<u8>, ImageFormat)> {
    let png_fmt = RegisterClipboardFormatW(windows_sys::core::w!("PNG"));
    if png_fmt != 0 {
        let h = GetClipboardData(png_fmt);
        if !h.is_null() {
            if let Some(bytes) = read_global_bytes(h) {
                return Some((bytes, ImageFormat::Png));
            }
        }
    }
    for fmt in [CF_DIBV5 as u32, CF_DIB as u32] {
        let h = GetClipboardData(fmt);
        if !h.is_null() {
            if let Some(bytes) = read_global_bytes(h) {
                return Some((bytes, ImageFormat::Dib));
            }
        }
    }
    None
}

/// 读取剪贴板文件列表（CF_HDROP，调用方需已 OpenClipboard）
unsafe fn read_files_unlocked() -> Option<Vec<String>> {
    let h = GetClipboardData(CF_HDROP as u32);
    if h.is_null() {
        return None;
    }
    let hdrop = h as HDROP;
    let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
    if count == 0 {
        return None;
    }
    let mut paths = Vec::with_capacity(count as usize);
    for i in 0..count {
        let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0) as usize;
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u16; len + 1];
        let got = DragQueryFileW(hdrop, i, buf.as_mut_ptr(), (len + 1) as u32) as usize;
        if got > 0 {
            paths.push(String::from_utf16_lossy(&buf[..got]));
        }
    }
    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

/// 读取 Global 内存块为字节（GlobalSize 取准确长度，调用方需已 OpenClipboard）
unsafe fn read_global_bytes(h: windows_sys::Win32::Foundation::HGLOBAL) -> Option<Vec<u8>> {
    let ptr = GlobalLock(h);
    if ptr.is_null() {
        return None;
    }
    let size = GlobalSize(h);
    let bytes = if size > 0 {
        std::slice::from_raw_parts(ptr as *const u8, size).to_vec()
    } else {
        Vec::new()
    };
    GlobalUnlock(h);
    if bytes.is_empty() {
        None
    } else {
        Some(bytes)
    }
}

pub fn try_read_image() -> Option<(Vec<u8>, ImageFormat)> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let result = read_image_unlocked();
        CloseClipboard();
        result
    }
}

pub fn try_read_files() -> Option<Vec<String>> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let result = read_files_unlocked();
        CloseClipboard();
        result
    }
}

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

// ---- 写入 ----

/// 构造 CF_HTML 数据（含 StartHTML/EndHTML/StartFragment/EndFragment 偏移）
pub fn build_cf_html(fragment: &str) -> Vec<u8> {
    let body = format!("<html><body><!--StartFragment-->{}</body>", fragment);
    let prefix_len = format!(
        "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
        0usize, 0usize, 0usize, 0usize
    )
    .len();
    let start_frag = prefix_len + "<html><body><!--StartFragment-->".len();
    let end_frag = start_frag + fragment.len();
    let end_html = prefix_len + body.len();
    let header = format!(
        "Version:0.9\r\nStartHTML:{:010}\r\nEndHTML:{:010}\r\nStartFragment:{:010}\r\nEndFragment:{:010}\r\n",
        prefix_len, end_html, start_frag, end_frag
    );
    let mut out = header.into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

pub fn set_clipboard(text: &str, html: Option<&str>) -> Result<(), String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("无法打开系统剪贴板".into());
        }
        if EmptyClipboard() == 0 {
            CloseClipboard();
            return Err("无法清空系统剪贴板".into());
        }

        // CF_UNICODETEXT
        let mut buf: Vec<u16> = text.encode_utf16().collect();
        buf.push(0);
        let mem = GlobalAlloc(GMEM_MOVEABLE, buf.len() * 2);
        if mem.is_null() {
            CloseClipboard();
            return Err("剪贴板内存分配失败".into());
        }
        let dst = GlobalLock(mem);
        if dst.is_null() {
            GlobalFree(mem);
            CloseClipboard();
            return Err("剪贴板内存锁定失败".into());
        }
        std::ptr::copy_nonoverlapping(buf.as_ptr(), dst as *mut u16, buf.len());
        GlobalUnlock(mem);
        if SetClipboardData(CF_UNICODETEXT as u32, mem).is_null() {
            GlobalFree(mem);
            CloseClipboard();
            return Err("写入剪贴板失败".into());
        }

        // CF_HTML（可选，失败不阻塞纯文本写入）
        if let Some(h) = html {
            let fmt = RegisterClipboardFormatW(windows_sys::core::w!("HTML Format"));
            if fmt != 0 {
                let bytes = build_cf_html(h);
                let mem = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len());
                if !mem.is_null() {
                    let dst = GlobalLock(mem);
                    if !dst.is_null() {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst as *mut u8, bytes.len());
                        GlobalUnlock(mem);
                        if SetClipboardData(fmt, mem).is_null() {
                            GlobalFree(mem);
                        }
                    } else {
                        GlobalFree(mem);
                    }
                }
            }
        }

        CloseClipboard();

        super::record_text_echo(text, html);
        Ok(())
    }
}

/// 分配 Global 内存并写入字节、SetClipboardData（调用方需已 OpenClipboard 且 EmptyClipboard）
unsafe fn set_global_data(format: u32, data: &[u8]) -> Result<(), String> {
    let mem = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, data.len());
    if mem.is_null() {
        return Err("剪贴板内存分配失败".into());
    }
    let dst = GlobalLock(mem);
    if dst.is_null() {
        GlobalFree(mem);
        return Err("剪贴板内存锁定失败".into());
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, data.len());
    GlobalUnlock(mem);
    if SetClipboardData(format, mem).is_null() {
        GlobalFree(mem);
        return Err("写入剪贴板失败".into());
    }
    Ok(())
}

pub fn set_clipboard_image(path: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取图片快照失败: {}", e))?;
    let payload = super::clipboard_image_payload(&bytes)?;
    let hash = super::hash_bytes_pub(payload.bytes());
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("无法打开系统剪贴板".into());
        }
        if EmptyClipboard() == 0 {
            CloseClipboard();
            return Err("无法清空系统剪贴板".into());
        }
        let result = match payload {
            super::ClipboardImagePayload::Dib(data) => set_global_data(CF_DIB as u32, &data),
            super::ClipboardImagePayload::Png(data) => {
                let fmt = RegisterClipboardFormatW(windows_sys::core::w!("PNG"));
                if fmt == 0 {
                    Err("无法注册 PNG 格式".into())
                } else {
                    set_global_data(fmt, &data)
                }
            }
        };
        CloseClipboard();
        result?;
    }
    super::record_hash_echo(hash);
    Ok(())
}

/// 构造 CF_HDROP 数据：DROPFILES 结构 + 宽字符路径表（每条以 null 结尾，末尾额外 null）
fn build_hdrop(paths: &[&str]) -> Vec<u8> {
    let header_size = std::mem::size_of::<DROPFILES>();
    let mut payload: Vec<u16> = Vec::new();
    for p in paths {
        payload.extend(p.encode_utf16());
        payload.push(0);
    }
    payload.push(0); // 列表结束标记

    let total = header_size + payload.len() * 2;
    let mut out = vec![0u8; total];
    let df = DROPFILES {
        pFiles: header_size as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: 0,
        fWide: 1,
    };
    let df_bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(&df as *const DROPFILES as *const u8, header_size)
    };
    out[..header_size].copy_from_slice(df_bytes);
    for (i, u) in payload.iter().enumerate() {
        let b = u.to_le_bytes();
        out[header_size + i * 2..header_size + i * 2 + 2].copy_from_slice(&b);
    }
    out
}

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
    let data = build_hdrop(&existing);
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("无法打开系统剪贴板".into());
        }
        if EmptyClipboard() == 0 {
            CloseClipboard();
            return Err("无法清空系统剪贴板".into());
        }
        let result = set_global_data(CF_HDROP as u32, &data);
        CloseClipboard();
        result?;
    }
    super::record_hash_echo(hash);
    Ok(())
}

// ---- 粘贴注入 ----

/// 发送一次按键（基于扫描码，比虚拟键码更兼容各类应用/终端）
unsafe fn send_key(scan: u16, flags: u32) {
    let mut input: INPUT = std::mem::zeroed();
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: 0,
        wScan: scan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
}

/// 发送粘贴快捷键：ctrl_v / ctrl_shift_v / shift_insert
pub fn send_paste_keystroke(method: &str) {
    unsafe {
        let ctrl = MapVirtualKeyW(VK_CONTROL as u32, MAPVK_VK_TO_VSC) as u16;
        let shift = MapVirtualKeyW(VK_SHIFT as u32, MAPVK_VK_TO_VSC) as u16;
        let v = MapVirtualKeyW('V' as u32, MAPVK_VK_TO_VSC) as u16;
        let insert = MapVirtualKeyW(VK_INSERT as u32, MAPVK_VK_TO_VSC) as u16;
        match method {
            "ctrl_shift_v" => {
                send_key(ctrl, KEYEVENTF_SCANCODE);
                send_key(shift, KEYEVENTF_SCANCODE);
                send_key(v, KEYEVENTF_SCANCODE);
                send_key(v, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP);
                send_key(shift, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP);
                send_key(ctrl, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP);
            }
            "shift_insert" => {
                send_key(shift, KEYEVENTF_SCANCODE);
                send_key(insert, KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY);
                send_key(insert, KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP);
                send_key(shift, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP);
            }
            _ => {
                // ctrl_v
                send_key(ctrl, KEYEVENTF_SCANCODE);
                send_key(v, KEYEVENTF_SCANCODE);
                send_key(v, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP);
                send_key(ctrl, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP);
            }
        }
    }
}

/// 释放所有修饰键（Ctrl/Shift/Alt/Win 的左右键）
fn release_modifier_keys() {
    unsafe {
        let keys = [
            VK_SHIFT, VK_LSHIFT, VK_RSHIFT, VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_MENU,
            VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN,
        ];
        for k in keys {
            keybd_event(k as u8, 0, KEYEVENTF_KEYUP, 0);
        }
    }
}

/// 强行把目标窗口带到前台并聚焦（AttachThreadInput 绕开前台锁定 + 还原最小化窗口）。
fn force_focus_window(hwnd: *mut core::ffi::c_void) {
    unsafe {
        if hwnd.is_null() || IsWindowVisible(hwnd) == 0 {
            return;
        }
        let should_restore = IsIconic(hwnd) != 0;
        let fg = GetForegroundWindow();
        if fg as isize != hwnd as isize {
            let mut fg_pid: u32 = 0;
            let mut target_pid: u32 = 0;
            let fg_tid = GetWindowThreadProcessId(fg, &mut fg_pid);
            let target_tid = GetWindowThreadProcessId(hwnd, &mut target_pid);
            if fg_tid != 0 && target_tid != 0 && fg_tid != target_tid {
                AttachThreadInput(fg_tid, target_tid, 1);
                SetForegroundWindow(hwnd);
                if should_restore {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                BringWindowToTop(hwnd);
                AttachThreadInput(fg_tid, target_tid, 0);
            } else {
                SetForegroundWindow(hwnd);
                if should_restore {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                BringWindowToTop(hwnd);
            }
        }
    }
}

/// 前台进程名（给定窗口句柄），用于识别终端等特殊粘贴目标
fn process_name_of_hwnd(hwnd: isize) -> Option<String> {
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd as *mut core::ffi::c_void, &mut pid) };
    if pid == 0 {
        return None;
    }
    process_name_of_pid(pid)
}

/// 已知终端/命令行窗口进程名
const TERMINAL_NAMES: [&str; 24] = [
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "conhost.exe",
    "openconsole.exe",
    "windowsterminal.exe",
    "wt.exe",
    "wezterm.exe",
    "alacritty.exe",
    "kitty.exe",
    "mintty.exe",
    "hyper.exe",
    "conemu64.exe",
    "cmder.exe",
    "tabby.exe",
    "warp.exe",
    "ghostty.exe",
    "mobaxterm.exe",
    "tabs.exe",
    "terminus.exe",
    "fluentterminal.exe",
    "ssh.exe",
    "sshd.exe",
    "windows terminal",
];

fn is_terminal_name(name: &str) -> bool {
    TERMINAL_NAMES
        .iter()
        .any(|t| name.eq_ignore_ascii_case(t))
}

/// 解析最终粘贴方式：auto 时按目标窗口是否终端自动选择 Ctrl+Shift+V / Ctrl+V
pub fn resolve_paste_method(cfg: &str, target_hwnd: isize) -> &'static str {
    match cfg {
        "ctrl_shift_v" => "ctrl_shift_v",
        "shift_insert" => "shift_insert",
        "ctrl_v" => "ctrl_v",
        _ => {
            let name = process_name_of_hwnd(target_hwnd).unwrap_or_default();
            if is_terminal_name(&name) {
                "ctrl_shift_v"
            } else {
                "ctrl_v"
            }
        }
    }
}

/// 判断唤起浮层前聚焦的窗口是否为本应用主窗口（主窗粘贴走 JS 派发路径）
pub fn is_main_window(app: &AppHandle, prev_hwnd: isize) -> bool {
    crate::main_window(app)
        .and_then(|w| w.hwnd().ok())
        .map(|h| h.0 as isize == prev_hwnd)
        .unwrap_or(false)
}

/// worker：粘贴到唤起前窗口（恢复焦点 + 按目标应用注入快捷键）
pub fn exec_paste(hwnd: isize, paste_method: &'static str) {
    std::thread::sleep(std::time::Duration::from_millis(50));
    release_modifier_keys();
    let fg = unsafe { GetForegroundWindow() } as isize;
    let own_pid = std::process::id();
    let mut fg_pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(fg as *mut core::ffi::c_void, &mut fg_pid);
    }
    log::info!(
        "剪贴板粘贴：隐藏后前台 hwnd={:#x} pid={}，本进程 pid={}，目标 hwnd={:#x}",
        fg,
        fg_pid,
        own_pid,
        hwnd
    );
    if fg_pid == own_pid {
        force_focus_window(hwnd as *mut core::ffi::c_void);
        log::info!("剪贴板粘贴：前台仍在本进程，已恢复目标窗口焦点");
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    let fg_after = unsafe { GetForegroundWindow() } as isize;
    let visible = unsafe { IsWindowVisible(hwnd as *mut core::ffi::c_void) } != 0;
    log::info!(
        "剪贴板粘贴：沉降后前台 hwnd={:#x}，目标可见={}",
        fg_after,
        visible
    );
    if !visible {
        log::warn!("剪贴板粘贴：目标窗口不可见，跳过输入注入");
        return;
    }
    log::info!("剪贴板粘贴：采用粘贴方式 {:?}，注入按键", paste_method);
    send_paste_keystroke(paste_method);
}

/// worker：收起浮层后归还焦点给唤起前窗口（若前台仍在本进程）
pub fn exec_restore_focus(hwnd: isize) {
    std::thread::sleep(std::time::Duration::from_millis(100));
    let fg = unsafe { GetForegroundWindow() } as isize;
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(fg as *mut core::ffi::c_void, &mut pid);
    }
    if pid == std::process::id() {
        force_focus_window(hwnd as *mut core::ffi::c_void);
    }
}

// ---- 前台标识 ----

/// 前台窗口句柄（作为 prev_focus 标识）
pub fn foreground_focus_handle() -> Option<isize> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd as isize)
    }
}

/// 取前台窗口所属进程名（作为历史来源应用，随剪贴板变化时读取）
pub fn foreground_app_name() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }
        process_name_of_pid(pid)
    }
}

/// 进程名缓存容量上限
const MAX_PROCESS_CACHE: usize = 256;

/// 按 PID 取进程名（带缓存，避免每次复制全量枚举进程卡顿）
fn process_name_of_pid(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    static CACHE: std::sync::OnceLock<Mutex<std::collections::HashMap<u32, String>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Ok(cached) = cache.lock() {
        if let Some(name) = cached.get(&pid) {
            return Some(name.clone());
        }
    }
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        false,
    );
    let name = sys
        .process(sysinfo::Pid::from_u32(pid))
        .map(|p| p.name().to_string_lossy().to_string());
    if let Some(name) = &name {
        if let Ok(mut cached) = cache.lock() {
            if cached.len() >= MAX_PROCESS_CACHE {
                cached.clear();
            }
            cached.insert(pid, name.clone());
        }
    }
    name
}

// ---- Esc 热键 / 低层鼠标钩子（浮层可见期间） ----

const WM_XHUB_CLIPBOARD_DISMISS: u32 = WM_APP + 0x5C01;
const WM_XHUB_INSTALL_MOUSE_HOOK: u32 = WM_APP + 0x5C02;
const WM_XHUB_UNINSTALL_MOUSE_HOOK: u32 = WM_APP + 0x5C03;

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code >= 0 {
        let msg = wparam as u32;
        if msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN {
            let clicked_outside = {
                let Some(hwnd) = *OVERLAY_HWND.lock().unwrap_or_else(|e| e.into_inner()) else {
                    return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
                };
                if IsWindowVisible(hwnd as *mut core::ffi::c_void) == 0 {
                    return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
                }
                let st = lparam as *const MSLLHOOKSTRUCT;
                let pt = (*st).pt;
                let mut rc: RECT = std::mem::zeroed();
                GetWindowRect(hwnd as *mut core::ffi::c_void, &mut rc);
                pt.x < rc.left - 5 || pt.x > rc.right + 5 || pt.y < rc.top - 5 || pt.y > rc.bottom + 5
            };
            if clicked_outside {
                if let Some(hwnd) = *LISTENER_HWND.lock().unwrap_or_else(|e| e.into_inner()) {
                    let _ = PostMessageW(
                        hwnd as *mut core::ffi::c_void,
                        WM_XHUB_CLIPBOARD_DISMISS,
                        0,
                        0,
                    );
                }
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

pub fn install_mouse_hook() {
    let hwnd = match *LISTENER_HWND.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(h) => h,
        None => return,
    };
    unsafe {
        PostMessageW(hwnd as *mut core::ffi::c_void, WM_XHUB_INSTALL_MOUSE_HOOK, 0, 0);
    }
}

pub fn uninstall_mouse_hook() {
    let hwnd = match *LISTENER_HWND.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(h) => h,
        None => return,
    };
    unsafe {
        PostMessageW(hwnd as *mut core::ffi::c_void, WM_XHUB_UNINSTALL_MOUSE_HOOK, 0, 0);
    }
}

fn install_mouse_hook_impl() {
    let Ok(mut guard) = MOUSE_HOOK.lock() else {
        return;
    };
    if guard.is_some() {
        return;
    }
    let hmod = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), hmod, 0) };
    if hook.is_null() {
        log::warn!("剪贴板浮层：注册低层鼠标钩子失败");
        return;
    }
    *guard = Some(hook as isize);
    log::info!("剪贴板浮层：低层鼠标钩子已启用（监听线程）");
}

fn uninstall_mouse_hook_impl() {
    let Ok(mut guard) = MOUSE_HOOK.lock() else {
        return;
    };
    if let Some(hook) = guard.take() {
        unsafe {
            UnhookWindowsHookEx(hook as *mut core::ffi::c_void);
        }
    }
}

pub fn register_esc_hotkey() {
    let hwnd = match *LISTENER_HWND.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(h) => h,
        None => return,
    };
    let ok = unsafe {
        RegisterHotKey(hwnd as *mut core::ffi::c_void, ESC_HOTKEY_ID, 0, VK_ESCAPE as u32) != 0
    };
    if !ok {
        log::warn!("剪贴板浮层：注册 Esc 兜底热键失败（可能被其他程序占用）");
    }
}

pub fn unregister_esc_hotkey() {
    let hwnd = match *LISTENER_HWND.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(h) => h,
        None => return,
    };
    unsafe {
        UnregisterHotKey(hwnd as *mut core::ffi::c_void, ESC_HOTKEY_ID);
    }
}

// ---- 浮层显示原语 ----

/// 无激活显示浮层：附加 WS_EX_NOACTIVATE 后以 SW_SHOWNA 显示并置顶
pub fn show_overlay_no_activate(win: &tauri::WebviewWindow) {
    if let Ok(hwnd) = win.hwnd() {
        if let Ok(mut guard) = OVERLAY_HWND.lock() {
            *guard = Some(hwnd.0 as isize);
        }
        unsafe {
            let ex = GetWindowLongPtrW(hwnd.0, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd.0, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE as isize);
            ShowWindow(hwnd.0, SW_SHOWNA);
            SetWindowPos(
                hwnd.0,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW | SWP_NOACTIVATE,
            );
        }
        crate::win_taskbar::apply(win);
        return;
    }
    crate::win_taskbar::show(win);
}

/// 激活浮层：清除 WS_EX_NOACTIVATE 并强制前台
pub fn activate_overlay_focus(win: &tauri::WebviewWindow) {
    if let Ok(hwnd) = win.hwnd() {
        unsafe {
            let ex = GetWindowLongPtrW(hwnd.0, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd.0, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE as isize));
        }
        let _ = win.set_focus();
        force_focus_window(hwnd.0);
    } else {
        let _ = win.set_focus();
    }
}

/// 隐藏浮层（Win32 快路径），并清理钩子句柄缓存
pub fn hide_window_fast(win: &tauri::WebviewWindow) {
    if let Ok(mut guard) = OVERLAY_HWND.lock() {
        *guard = None;
    }
    if let Ok(hwnd) = win.hwnd() {
        unsafe {
            ShowWindow(hwnd.0, SW_HIDE);
        }
        return;
    }
    let _ = win.hide();
}

/// 计算浮层初始位置：光标附近（工作区范围内），物理像素
pub fn cursor_anchor_position(
    _app: &AppHandle,
    width: f64,
    height: f64,
) -> Option<(i32, i32)> {
    unsafe {
        let mut pt: POINT = std::mem::zeroed();
        if GetCursorPos(&mut pt) == 0 {
            return None;
        }
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let rc: RECT = info.rcWork;
        let w = width as i32;
        let h = height as i32;
        let x = (pt.x - w / 2).clamp(rc.left, rc.right - w);
        let y = (pt.y - 30).clamp(rc.top, rc.bottom - h);
        Some((x, y))
    }
}

// ---- 事件源（消息窗口 + AddClipboardFormatListener） ----

/// 监听消息窗口类名（进程内唯一）
const LISTENER_CLASS: *const u16 = windows_sys::core::w!("XHubClipboardListenerWnd");

unsafe extern "system" fn listener_wndproc(
    hwnd: *mut core::ffi::c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 事件源线程：消息窗口 + AddClipboardFormatListener + GetMessageW 循环
pub fn spawn_event_source(tx: std::sync::mpsc::Sender<()>) {
    std::thread::spawn(move || {
        let app = super::event_app_handle();
        log::info!("剪贴板监听线程启动（事件驱动 WM_CLIPBOARDUPDATE）");
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let class = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(listener_wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: LISTENER_CLASS,
            };
            if RegisterClassW(&class) == 0 {
                let err = std::io::Error::last_os_error();
                log::warn!("注册剪贴板监听窗口类失败: {}", err);
            }
            let hwnd = CreateWindowExW(
                0,
                LISTENER_CLASS,
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                log::warn!("创建剪贴板监听消息窗口失败");
                return;
            }
            if AddClipboardFormatListener(hwnd) == 0 {
                log::warn!("AddClipboardFormatListener 注册失败");
                DestroyWindow(hwnd);
                return;
            }
            log::info!("剪贴板格式监听已注册");
            if let Ok(mut guard) = LISTENER_HWND.lock() {
                *guard = Some(hwnd as isize);
            }

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) != 0 {
                if msg.message == WM_CLIPBOARDUPDATE {
                    let _ = tx.send(());
                } else if msg.message == WM_HOTKEY && msg.wParam as i32 == ESC_HOTKEY_ID {
                    if app
                        .as_ref()
                        .and_then(|a| a.get_webview_window(super::CLIPBOARD_WINDOW_LABEL))
                        .map(|w| w.is_visible().unwrap_or(false))
                        .unwrap_or(false)
                    {
                        super::hide_overlay_from_listener();
                    }
                } else if msg.message == WM_XHUB_INSTALL_MOUSE_HOOK {
                    install_mouse_hook_impl();
                } else if msg.message == WM_XHUB_UNINSTALL_MOUSE_HOOK {
                    uninstall_mouse_hook_impl();
                } else if msg.message == WM_XHUB_CLIPBOARD_DISMISS {
                    super::hide_overlay_from_listener();
                } else {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            if let Ok(mut guard) = LISTENER_HWND.lock() {
                *guard = None;
            }
            RemoveClipboardFormatListener(hwnd);
            DestroyWindow(hwnd);
            UnregisterClassW(LISTENER_CLASS, hinstance);
        }
    });
}
