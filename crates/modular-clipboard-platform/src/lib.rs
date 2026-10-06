//! 平台集成层：快捷键、自动粘贴、前台窗口探测、外部调用、开机自启。
//!
//! 全部平台 API 收敛在此。上层只调用语义化方法，便于将来移植到其他平台。

use anyhow::Result;
use std::time::Duration;

pub mod hotkey;
pub mod tray;

/// 复用捕获层的窗口探测能力，避免重复实现 Win32 调用。
use modular_clipboard_capture::{foreground_class_name, source_process_name};

/// 自动粘贴到前台窗口。
///
/// 原理：先把内容写入剪贴板，等待目标应用完成 OLE 数据握手后，
/// 再向前台窗口发送 Ctrl+V。这是剪贴板管理器「一键粘贴」的标准做法。
///
/// 延迟是必要���：若在写剪贴板后立即发送按键，Office、浏览器等应用
/// 尚未完成粘贴事件处理，按键会丢失。250ms 是兼顾可靠性的经验值。
#[cfg(windows)]
pub fn auto_paste(delay: Duration) -> Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
        VIRTUAL_KEY, VK_CONTROL, VK_V,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};

    let target = unsafe { GetForegroundWindow() };
    if target.0.is_null() {
        tracing::debug!("无前台窗口，跳过自动粘贴");
        return Ok(());
    }

    // 交还前台焦点，避免按键仍被本进程接收。
    let _ = unsafe { SetForegroundWindow(target) };
    std::thread::sleep(delay);

    let make = |vk: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };

    unsafe {
        let inputs = [
            make(VK_CONTROL, false),
            make(VK_V, false),
            make(VK_V, true),
            make(VK_CONTROL, true),
        ];
        let size = std::mem::size_of::<INPUT>() as i32;
        let sent = SendInput(&inputs, size);
        if sent as usize != inputs.len() {
            tracing::warn!("SendInput 仅发送 {}/{} 个事件", sent, inputs.len());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn auto_paste(_delay: Duration) -> Result<()> {
    tracing::warn!("当前平台尚未实现自动粘贴");
    Ok(())
}

/// 判断前台窗口是否为密码/凭据输入框。
///
/// Windows 没有公开 API 直接查询，因此使用窗口类名启发式。
/// 常见凭据对话框类名包含 Credential / Logon / Security 等字样。
///
/// 这属于**尽力而为**，无法覆盖所有场景，因此默认仍需配合来源进程黑名单。
#[cfg(windows)]
pub fn is_password_field() -> bool {
    let class = foreground_class_name().to_ascii_lowercase();
    const HINTS: [&str; 5] = ["credential", "password", "logon", "security", "consent"];
    HINTS.iter().any(|h| class.contains(h))
}

#[cfg(not(windows))]
pub fn is_password_field() -> bool {
    false
}

/// URL 判定：带http/https 协议前缀，且前缀后有非空内容，且不含空白。
pub fn looks_like_url(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 9 || s.chars().any(char::is_whitespace) {
        return false;
    }
    ["https://", "http://"]
        .iter()
        .any(|scheme| s.strip_prefix(scheme).is_some_and(|rest| !rest.is_empty()))
}

/// Windows 绝对路径判定：盘符 + `:\`。
pub fn looks_like_path(s: &str) -> bool {
    let s = s.trim();
    s.len() >= 3
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && s[1..].starts_with(":\\")
}

/// 用系统默认方式打开（路径或 URL）。
pub fn open_external(target: &str) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED: u32 = 0x0000_0008 | 0x0000_0020; // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        std::process::Command::new("explorer")
            .arg(target)
            .creation_flags(DETACHED)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open").arg(target).spawn()?;
        Ok(())
    }
}

/// 用默认浏览器打开 URL。
pub fn open_url(url: &str) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED: u32 = 0x0000_0008 | 0x0000_0020;
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(DETACHED)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        open_external(url)
    }
}

/// 在资源管理器中定位并选中文件。
pub fn reveal_in_explorer(path: &str) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED: u32 = 0x0000_0008 | 0x0000_0020;
        std::process::Command::new("explorer")
            .arg(format!("/select,{path}"))
            .creation_flags(DETACHED)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        open_external(path)
    }
}

/// 执行外部命令（联动规则入口）。
///
/// `arg` 中的 `{selection}` 会被替换为选中文本。
pub fn run_command(program: &str, arg: &str, selection: &str) -> Result<()> {
    let substituted = arg.replace("{selection}", selection);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED: u32 = 0x0000_0008 | 0x0000_0020;
        std::process::Command::new("cmd")
            .args(["/C", program, &substituted])
            .creation_flags(DETACHED)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{program} {substituted}"))
            .spawn()?;
        Ok(())
    }
}

/// 开机自启：写入当前用户 Run 注册表项，无需管理员权限。
#[cfg(windows)]
pub fn set_autostart(exe_path: &str, enable: bool) -> Result<()> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_EXPAND_SZ,
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
    };
    use windows::core::HSTRING;

    let subkey = HSTRING::from("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let value_name = HSTRING::from("modular-clipboard");

    unsafe {
        // Run 键必然存在，用 RegOpenKeyExW 打开即可，无需 RegCreateKeyExW
        // （后者在windows crate 中额外依赖 Win32_Security feature）。
        let mut hkey = HKEY::default();
        let status = RegOpenKeyExW(HKEY_CURRENT_USER, &subkey, None, KEY_SET_VALUE, &mut hkey);
        if status != ERROR_SUCCESS {
            anyhow::bail!("打开注册表 Run 键失败: {status:?}");
        }

        let outcome = if enable {
            // 值需为带引号的完整命令行，REG_EXPAND_SZ 支持环境变量展开。
            let data = HSTRING::from(format!("\"{exe_path}\""));
            // RegSetValueExW 期望 UTF-16 LE 字节序列，末尾包含 NUL。
            // RegSetValueExW 期望 UTF-16 LE 字节序列，末尾需含 NUL 终止符。
            // 用 HSTRING::as_ptr 读取其内部的 UTF-16 缓冲区。
            let wide = data.as_ptr();
            let len = data.len() + 1;
            let mut buf: Vec<u8> = Vec::with_capacity(len * 2);
            for i in 0..len {
                let ch = if i < data.len() { *wide.add(i) } else { 0 };
                buf.extend_from_slice(&ch.to_le_bytes());
            }
            let st = RegSetValueExW(hkey, &value_name, None, REG_EXPAND_SZ, Some(&buf));
            if st != ERROR_SUCCESS {
                Err(anyhow::anyhow!("写入自启项失败: {st:?}"))
            } else {
                Ok(())
            }
        } else {
            let st = RegDeleteValueW(hkey, &value_name);
            // 值不存在时视为已关闭，属正常状态。
            if st != ERROR_SUCCESS && st.0 != 2 {
                Err(anyhow::anyhow!("删除自启项失败: {st:?}"))
            } else {
                Ok(())
            }
        };
        let _ = RegCloseKey(hkey);
        outcome?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn set_autostart(_exe_path: &str, _enable: bool) -> Result<()> {
    anyhow::bail!("当前平台尚未实现开机自启")
}

/// 查询是否已设置开机自启。
#[cfg(windows)]
pub fn is_autostart_enabled() -> bool {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, RRF_RT_REG_SZ, RegCloseKey, RegGetValueW,
        RegOpenKeyExW,
    };
    use windows::core::HSTRING;

    let subkey = HSTRING::from("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let value_name = HSTRING::from("modular-clipboard");

    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, &subkey, None, KEY_QUERY_VALUE, &mut hkey)
            != ERROR_SUCCESS
        {
            return false;
        }
        let mut len: u32 = 0;
        let st = RegGetValueW(
            hkey,
            windows::core::PCWSTR::null(),
            &value_name,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut len),
        );
        let _ = RegCloseKey(hkey);
        st == ERROR_SUCCESS && len > 0
    }
}

#[cfg(not(windows))]
pub fn is_autostart_enabled() -> bool {
    false
}

/// 前台进程名。
pub fn foreground_process() -> String {
    source_process_name()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_detection() {
        assert!(looks_like_url("https://example.com"));
        assert!(looks_like_url("http://a.b/c?d=1"));
        assert!(!looks_like_url("hello world"));
        assert!(!looks_like_url("ftp://x"));
        assert!(!looks_like_url("https://"));
        assert!(!looks_like_url(""));
    }

    #[test]
    fn path_detection() {
        assert!(looks_like_path(r"C:\Users\test\file.txt"));
        assert!(looks_like_path(r"D:\a.txt"));
        assert!(!looks_like_path("no path here"));
        assert!(!looks_like_path("C:relative.txt"));
    }

    #[test]
    fn password_heuristic_does_not_panic() {
        let _ = is_password_field();
    }

    #[test]
    fn autostart_query_does_not_panic() {
        let _ = is_autostart_enabled();
    }
}