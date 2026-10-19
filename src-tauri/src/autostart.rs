//! 开机自启动管理：注册表 Run 键方式（登录时静默拉起，主窗不弹出、驻留托盘）。
//!
//! 历史版本曾提供「计划任务 + 最高权限」的管理员启动模式；因 Windows UIPI 隔离，
//! 管理员权限进程无法从资源管理器接收文件拖放（速达拖拽导入失效），该模式已移除。
//! `apply` 时会顺带清理旧版残留的计划任务。

#[cfg(target_os = "windows")]
use crate::process::NoConsoleWindow;
#[cfg(target_os = "windows")]
use std::process::Command;

/// 自启动在命令行里追加的隐藏启动参数（主窗不弹出、直接驻留托盘）
pub const HIDDEN_ARG: &str = "--autostart-hidden";

/// 判断本次进程是否由自启动拉起（命令行含 HIDDEN_ARG）。
/// 供前端决定是否主动显示主窗口：自启动时不打扰用户，直接驻留托盘。
pub fn is_hidden_launch() -> bool {
    std::env::args().any(|a| a == HIDDEN_ARG)
}

// ---- Windows 常量 ----
/// Run 子键（相对 HKCU，winreg 用；HKCU\Software\Microsoft\Windows\CurrentVersion\Run）
#[cfg(target_os = "windows")]
const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(target_os = "windows")]
const RUN_VALUE_NAME: &str = "x-hub";
/// 旧版管理员自启动注册的计划任务名（仅用于清理，不再创建）
#[cfg(target_os = "windows")]
const LEGACY_TASK_NAME: &str = "x-hub-autostart";
/// 任务管理器「启动」/ 安全软件禁用某启动项时写的标记子键：值首字节 0x03=禁用，0x02=启用。
/// 删除该值即回落到默认「启用」。Run 键在、但这里被置 0x03 → 登录时 Explorer 静默跳过。
#[cfg(target_os = "windows")]
const APPROVED_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

#[cfg(target_os = "windows")]
fn hkcu() -> winreg::RegKey {
    use winreg::enums::HKEY_CURRENT_USER;
    winreg::RegKey::predef(HKEY_CURRENT_USER)
}

/// 当前 exe 的完整路径（Run 键需要绝对路径）
#[cfg(target_os = "windows")]
fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 自启动命令行：`"C:\...\x-hub.exe" --autostart-hidden`
#[cfg(target_os = "windows")]
fn launch_command_line() -> String {
    format!("\"{}\" {}", exe_path(), HIDDEN_ARG)
}

// ---------- 普通模式：注册表 Run 键 ----------

#[cfg(target_os = "windows")]
fn write_run_key() -> Result<(), String> {
    use winreg::enums::KEY_SET_VALUE;
    let key = hkcu()
        .create_subkey_with_flags(RUN_SUBKEY, KEY_SET_VALUE)
        .map_err(|e| format!("打开 Run 项失败: {e}"))?
        .0;
    key.set_value(RUN_VALUE_NAME, &launch_command_line())
        .map_err(|e| format!("写入 Run 键失败: {e}"))?;
    log::info!("已写入开机自启动 Run 键");
    Ok(())
}

#[cfg(target_os = "windows")]
fn remove_run_key() {
    use winreg::enums::KEY_SET_VALUE;
    if let Ok(key) = hkcu().open_subkey_with_flags(RUN_SUBKEY, KEY_SET_VALUE) {
        let _ = key.delete_value(RUN_VALUE_NAME);
    }
}

/// 读取 Run 键里登记的启动命令行；键不存在/读不到返回 None（winreg 按 UTF-16 正确解码，兼容非 ASCII 路径）。
#[cfg(target_os = "windows")]
fn read_run_command() -> Option<String> {
    let key = hkcu().open_subkey(RUN_SUBKEY).ok()?;
    key.get_value::<String, _>(RUN_VALUE_NAME).ok()
}

/// 从启动命令行取出 exe 路径（剥外层引号与后续参数）。
#[cfg(target_os = "windows")]
fn exe_path_from_command(cmd_line: &str) -> Option<&str> {
    let s = cmd_line.trim();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        Some(rest[..end].trim())
    } else {
        // 本程序写入始终带引号；无引号仅兜底解析第三方/手工写入的项
        s.split_whitespace().next()
    }
}

/// 当前 exe 是否就是 Run 键指向的那个路径（大小写不敏感）。
#[cfg(target_os = "windows")]
fn run_path_matches_current() -> bool {
    let Some(cmd) = read_run_command() else {
        return false;
    };
    let Some(reg_exe) = exe_path_from_command(&cmd) else {
        return false;
    };
    let cur = exe_path();
    !cur.is_empty() && reg_exe.eq_ignore_ascii_case(cur.as_str())
}

/// 是否被系统/安全软件在启动项里禁用（StartupApproved\Run 首字节 0x03）。
#[cfg(target_os = "windows")]
fn is_os_disabled() -> bool {
    let Ok(key) = hkcu().open_subkey(APPROVED_SUBKEY) else {
        return false;
    };
    match key.get_raw_value(RUN_VALUE_NAME) {
        Ok(rv) => rv.bytes.first().copied() == Some(0x03),
        Err(_) => false,
    }
}

/// 清除禁用标记：删掉 StartupApproved\Run 下对应值，Explorer 按默认「启用」处理。
#[cfg(target_os = "windows")]
fn clear_os_disabled() {
    use winreg::enums::KEY_SET_VALUE;
    if let Ok(key) = hkcu().open_subkey_with_flags(APPROVED_SUBKEY, KEY_SET_VALUE) {
        let _ = key.delete_value(RUN_VALUE_NAME);
    }
}

/// 探测真实自启动状态：`(registered, os_disabled)`。
/// registered = Run 键存在且指向当前 exe；os_disabled = 被启动项禁用。
#[cfg(target_os = "windows")]
pub fn probe() -> (bool, bool) {
    (run_path_matches_current(), is_os_disabled())
}

/// macOS：LaunchAgents plist 存在且 ProgramArguments 指向当前二进制。
/// macOS 无 StartupApproved 等价的系统级禁用标记，os_disabled 恒 false。
#[cfg(target_os = "macos")]
pub fn probe() -> (bool, bool) {
    (launch_agent_matches_current(), false)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn probe() -> (bool, bool) {
    (false, false)
}

/// 启动自愈：用户已开启自启动、但 Run 键缺失或指向别的路径（程序被移动/换目录/被清理工具删除）时，
/// 用当前 exe 重写 Run 键。只动 Run 键，不碰旧计划任务（避免每次启动弹 UAC）。
/// 返回是否做了修复。
#[cfg(target_os = "windows")]
pub fn ensure_registered() -> Result<bool, String> {
    if read_run_command().is_none() {
        write_run_key()?;
        log::info!("[自启动] 自愈：Run 键缺失，已按当前 exe 路径重写");
        return Ok(true);
    }
    if !run_path_matches_current() {
        write_run_key()?;
        log::info!("[自启动] 自愈：Run 键路径与当前 exe 不符，已重写");
        return Ok(true);
    }
    Ok(false)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn ensure_registered() -> Result<bool, String> {
    Ok(false)
}

/// macOS 自启动自愈：LaunchAgent 缺失或路径不符时按当前二进制重写
#[cfg(target_os = "macos")]
pub fn ensure_registered() -> Result<bool, String> {
    if !launch_agent_matches_current() {
        write_launch_agent()?;
        log::info!("[自启动] 自愈：LaunchAgent 缺失或路径不符，已按当前二进制重写");
        return Ok(true);
    }
    Ok(false)
}

// ---------- 旧版管理员模式残留清理 ----------

/// 隐藏控制台地运行一行 cmd 命令，返回是否成功
#[cfg(target_os = "windows")]
fn run_cmd_line(line: &str) -> bool {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", line]);
    cmd.no_console_window();
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// 提权运行临时 bat（触发一次 UAC 授权），等待完成
#[cfg(target_os = "windows")]
fn run_bat_elevated(bat_path: &std::path::Path, log_tag: &str) -> bool {
    let script = format!(
        "Start-Process -Wait -Verb RunAs -WindowStyle Hidden -FilePath \"{}\"",
        bat_path.to_string_lossy()
    );
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script]);
    cmd.no_console_window();
    let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
    if ok {
        log::info!("[自启动] {} 提权操作完成", log_tag);
    } else {
        log::warn!("[自启动] {} 提权操作被取消或失败", log_tag);
    }
    ok
}

/// 旧版最高权限计划任务是否仍存在（schtasks /Query 退出码 0 表示存在）
#[cfg(target_os = "windows")]
fn legacy_task_exists() -> bool {
    let mut cmd = Command::new("schtasks");
    cmd.args(["/Query", "/TN", LEGACY_TASK_NAME]);
    cmd.no_console_window();
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// 清理旧版管理员自启动残留的计划任务（最高权限任务普通权限删不动时走一次 UAC 授权）
#[cfg(target_os = "windows")]
fn remove_legacy_task() {
    if !legacy_task_exists() {
        return;
    }
    let line = format!("schtasks /Delete /TN \"{}\" /F", LEGACY_TASK_NAME);
    if run_cmd_line(&line) {
        log::info!("[自启动] 已清理旧版管理员自启动计划任务");
        return;
    }
    let bat = std::env::temp_dir().join("x-hub-autostart-task-del.bat");
    if std::fs::write(&bat, &line).is_ok() && run_bat_elevated(&bat, "清理旧版自启动任务") {
        log::info!("[自启动] 已通过 UAC 授权清理旧版管理员自启动计划任务");
    }
    let _ = std::fs::remove_file(&bat);
}

// ---------- 对外接口 ----------

/// 应用自启动开关：先清掉已有注册（Run 键 + 旧版任务残留），启用时写入 Run 键。
pub fn apply(enabled: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        remove_run_key();
        remove_legacy_task();
        if enabled {
            write_run_key()?;
            // 重新启用时清掉「任务管理器/安全软件把本项置为禁用」的标记：Run 键在但被禁用时，
            // 登录仍不会拉起——正是「明明开了自启动、重启却没自启」的典型成因。
            clear_os_disabled();
        }
        Ok(())
    }
    // macOS：LaunchAgents（登录时拉起，与 Run 键同语义）
    #[cfg(target_os = "macos")]
    {
        remove_launch_agent();
        if enabled {
            write_launch_agent()?;
        }
        Ok(())
    }
    // 其余平台：自启动仅支持当前平台时返回错误提示
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = enabled;
        Err("当前平台不支持开机自启动".into())
    }
}

// ---------- macOS：LaunchAgents ----------

/// LaunchAgent 标签与 plist 路径
#[cfg(target_os = "macos")]
const LA_LABEL: &str = "x-hub.autostart";

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents/x-hub.plist"))
}

/// 当前二进制完整路径（.app 内层二进制，LaunchAgent 直接拉它即等效双击启动）
#[cfg(target_os = "macos")]
fn exe_path_mac() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 写入 LaunchAgent plist：RunAtLoad + --autostart-hidden（登录即静默驻留托盘）
#[cfg(target_os = "macos")]
fn write_launch_agent() -> Result<(), String> {
    use std::io::Write as _;

    let Some(path) = launch_agent_path() else {
        return Err("无法定位用户 LaunchAgents 目录".into());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 LaunchAgents 目录失败: {e}"))?;
    }
    let exe = exe_path_mac();
    if exe.is_empty() {
        return Err("无法定位当前程序路径".into());
    }
    // XML 转义（路径可能含 & 等）
    let esc = exe.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LA_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{esc}</string>
        <string>{HIDDEN_ARG}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>AssociatedBundleIdentifiers</key>
    <string>x-hub</string>
</dict>
</plist>
"#
    );
    #[allow(unused_mut)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&path)
            .map_err(|e| format!("写入 LaunchAgent 失败: {e}"))?
    };
    file.write_all(plist.as_bytes())
        .map_err(|e| format!("写入 LaunchAgent 失败: {e}"))?;
    // 通知 launchd 重新加载（失败不阻断：下次登录自然生效）
    let _ = std::process::Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(&path)
        .status();
    let _ = std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&path)
        .status();
    log::info!("已写入开机自启动 LaunchAgent: {}", path.display());
    Ok(())
}

/// 移除 LaunchAgent（unload + 删文件）
#[cfg(target_os = "macos")]
fn remove_launch_agent() {
    use crate::process::NoConsoleWindow;
    let Some(path) = launch_agent_path() else {
        return;
    };
    if path.exists() {
        let mut cmd = std::process::Command::new("launchctl");
        cmd.args(["unload", "-w"]).arg(&path);
        cmd.no_console_window();
        let _ = cmd.status();
        let _ = std::fs::remove_file(&path);
        log::info!("已移除开机自启动 LaunchAgent");
    }
}

/// LaunchAgent 是否存在且 ProgramArguments 指向当前二进制（路径大小写不敏感比对）
#[cfg(target_os = "macos")]
fn launch_agent_matches_current() -> bool {
    let Some(path) = launch_agent_path() else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    let cur = exe_path_mac();
    if cur.is_empty() {
        return false;
    }
    // plist 的 ProgramArguments 首项即二进制路径（XML 转义还原）
    let unesc = content
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    unesc
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case(&format!("<string>{}</string>", cur)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_command_line_contains_hidden() {
        // 命令行必须始终带隐藏参数（自启动时不打扰用户，驻留托盘）
        #[cfg(target_os = "windows")]
        {
            let cli = launch_command_line();
            assert!(cli.contains(HIDDEN_ARG));
            // 路径带空格时必须有引号包裹
            assert!(cli.starts_with('"'));
        }
    }

    // 注意：不要在测试里调用 apply()——它会真实操作系统注册表与计划任务，
    // cargo test 会把本机已注册的自启动 Run 键删掉。
}
