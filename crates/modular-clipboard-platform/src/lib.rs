//! 平台集成层：快捷键、自动粘贴、前台窗口探测、外部调用、开机自启。
//!
//! 全部平台 API 收敛在此。上层只调用语义化方法，便于将来移植到其他平台。

use anyhow::Result;
use std::time::Duration;

pub mod autostart;
pub mod hotkey;
pub mod tray;

/// 主界面窗口标题（`CreateWindowExW` 用的就是它）。
///
/// 必须与 `modular-clipboard-ipc::instance::UI_WINDOW_TITLE` 保持一致；
/// 这里不依赖 ipc crate（platform 是更底层的 crate），重复定义一份。
/// 托盘线程按标题 `FindWindowW` 找到主窗口。
pub const UI_WINDOW_TITLE: &str = "ModularClipboard";

/// 复用捕获层的窗口探测能力，避免重复实现 Win32 调用。
use modular_clipboard_capture::{foreground_class_name, source_process_name};

/// Win 键当前是否按下。
///
/// egui 的  不报告 Win 键（ 在 Windows 上指的是 Ctrl），
/// 而  这类组合恰恰是剪贴板工具最常用的。录制快捷键时单独
/// 问一次系统，才能把 Win 组合录出来。
#[cfg(windows)]
pub fn win_key_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LWIN, VK_RWIN};
    // 最高位为 1 表示当前按下。
    let l = unsafe { GetAsyncKeyState(VK_LWIN.0 as i32) };
    let r = unsafe { GetAsyncKeyState(VK_RWIN.0 as i32) };
    (l as u16 & 0x8000) != 0 || (r as u16 & 0x8000) != 0
}

#[cfg(not(windows))]
pub fn win_key_down() -> bool {
    false
}

/// 自动粘贴到前台窗口。
///
/// 原理：先把内容写入剪贴板，等待目标应用完成 OLE 数据握手后，
/// 再向前台窗口发送 Ctrl+V。这是剪贴板管理器「一键粘贴」的标准做法。
///
/// 延迟是必要的：若在写剪贴板后立即发送按键，Office、浏览器等应用
/// 尚未完成粘贴事件处理，按键会丢失。250ms 是兼顾可靠性的经验值。
/// 当前前台窗口句柄（`0` = 没有前台窗口）。
///
/// 单独暴露是为了让调用方在**事件发生的那一刻」抓住目标窗口：
/// 常驻进程收到「粘贴上一条」时前台还是用户正在用的程序，等 250ms
/// 后再查（`auto_paste` 内部）可能已经是别的窗口了。
#[cfg(windows)]
pub fn foreground_window() -> isize {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow().0 as isize }
}

#[cfg(not(windows))]
pub fn foreground_window() -> isize {
    0
}

/// 按窗口标题找到主窗口，并在**当前线程正持有前台授权时**把它恢复并
/// 激活到最前；返回「显示方向是否已由本函数处理」。
///
/// # 返回值语义
///
/// - `true`：窗口原本处于隐藏/最小化状态，本函数已完成 `SW_RESTORE`
///   （置前可能仍失败，但上层不得再 toggle 隐藏）。
/// - `false`：找不到窗口（尚未创建，上层应拉起新进程），或窗口已在
///   正常显示（交由上层按 toggle 隐藏 / 幂等置前）。
///
/// # 为什么必须在 WM_HOTKEY 处理线程里直接调
///
/// 系统派发 `WM_HOTKEY` 时会临时把「允许设置前台窗口」的权限授予
/// **接收该消息的线程**（即托盘线程）。若托盘线程只把事件转发给 UI
/// 主线程（`PostMessage` 是异步的），等主线程帧循环处理时授权上下文
/// 早已失效——这正是「快捷键呼出时窗口被其他应用盖在下面、只在
/// 任务栏闪烁」的根因。
///
/// 在热键处理线程里直接 `FindWindowW` + 激活，单进程（同进程跨线程）
/// 与双进程（daemon 托盘线程 → UI 进程窗口）都成立：热键授权允许
/// 跨进程 `SetForegroundWindow`。UI 主线程的「可见性对账」会发现
/// 窗口已可见并恢复渲染；`WM_SHOW_MAIN` 若随后到达，`focus_window`
/// 再做一次幂等置前兜底。
pub fn activate_window_by_title(title: &str) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, FindWindowW, IsIconic, IsWindowVisible, SetForegroundWindow,
        ShowWindow, SW_RESTORE,
    };

    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let hwnd = match unsafe { FindWindowW(None, PCWSTR(wide.as_ptr())) } {
        Ok(h) if !h.is_invalid() => h,
        _ => {
            tracing::debug!("按标题未找到窗口：{title}（可能尚未创建）");
            return false;
        }
    };
    unsafe {
        // 窗口已处于正常显示状态（非隐藏、非最小化）时不干预：
        // 双进程模式下热键是 toggle——可见时上层紧接着会把它隐藏，
        // 这里若先前置会「闪一下又消失」。
        // 可见但被其他窗口盖住的情况，由 UI 主线程的
        // `focus_window`（加强版）负责置前。
        if IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() {
            return false;
        }
        // 最小化（IsIconic）或被 SW_HIDE 隐藏（IsWindowVisible=false）
        // 都先还原：SW_RESTORE 同时覆盖这两种状态。
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let fg_ok = SetForegroundWindow(hwnd).as_bool();
        let _ = BringWindowToTop(hwnd);
        if !fg_ok {
            tracing::debug!("热键线程 SetForegroundWindow 未成功（前台锁），主线程将再兜底");
        }
        // 只要执行了显示方向就返回 true：置前失败由主线程兜底，
        // 但上层绝不能再把窗口 toggle 回隐藏。
        true
    }
}

#[cfg(not(windows))]
pub fn activate_window_by_title(_title: &str) -> bool {
    false
}

/// 自动粘贴：把剪贴板内容以 Ctrl+V 发给当前前台窗口。
#[cfg(windows)]
pub fn auto_paste(delay: Duration) -> Result<()> {
    auto_paste_to(Some(foreground_window()), delay)
}

#[cfg(not(windows))]
pub fn auto_paste(_delay: Duration) -> Result<()> {
    tracing::warn!("当前平台尚未实现自动粘贴");
    Ok(())
}

/// 自动粘贴到**指定窗口**（`None` = 当前前台）。
///
/// # 为什么要显式指定
///
/// 调用方（常驻进程）在收到热键的那一刻前台还是用户正在用的程序；
/// 若等内部 250ms 延迟结束再查一次，用户可能已经切走，按键会粘到
/// 错误的窗口上。
#[cfg(windows)]
pub fn auto_paste_to(target: Option<isize>, delay: Duration) -> Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
        VIRTUAL_KEY, VK_CONTROL, VK_V,
    };
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};

    let target = match target {
        Some(raw) if raw != 0 => HWND(raw as *mut core::ffi::c_void),
        _ => unsafe { GetForegroundWindow() },
    };
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
///
/// 走 `ShellExecuteW` 而非 `cmd /C start`：URL 经 shell 解析时 `&` 会被
/// 当命令分隔符（带查询串的链接被截断）、`%XX` 被变量展开（百分号编码
/// 必坏），且剪贴板内容可借此注入任意命令。ShellExecuteW 把 URL 作为
/// 单个参数交给系统，元字符全部是普通字符。
pub fn open_url(url: &str) -> Result<()> {
    #[cfg(windows)]
    {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::{SHOW_WINDOW_CMD, SW_SHOWNORMAL};
        use windows::core::{HSTRING, PCWSTR};
        let file = HSTRING::from(url);
        let verb = HSTRING::from("open");
        // SAFETY: 纯 FFI 调用，参数均为本函数构造的有效宽字符串。
        let result = unsafe {
            ShellExecuteW(
                None,
                &verb,
                &file,
                PCWSTR::null(),
                PCWSTR::null(),
                SHOW_WINDOW_CMD(SW_SHOWNORMAL.0),
            )
        };
        // ShellExecuteW 的历史遗留约定：返回值 > 32 才算成功，
        // ≤ 32 是 SE_ERR_* 错误码。
        if result.0 as isize <= 32 {
            anyhow::bail!("打开 URL 失败（ShellExecuteW 返回 {}）：{url}", result.0 as isize);
        }
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

/// 自定义调用程序的公共校验：路径非空且**真实存在**。
///
/// 不存在的程序由 `spawn` 报出来的是 OS error 2（「系统找不到指定的文件」），
/// 用户看不出说的是哪个程序；这里提前给出带路径的明确消息。
fn validate_custom_program(what: &str, custom: &str) -> Result<()> {
    let p = std::path::Path::new(custom);
    if !p.exists() {
        anyhow::bail!("{what}路径不存在: {custom}");
    }
    Ok(())
}

/// 用默认或自定义浏览器打开 URL。
///
/// `custom` 为空 = 系统默认浏览器（[`open_url`] 的 `ShellExecuteW`）；
/// 非空 = 以 argv 直传方式启动自定义程序（Chrome / Firefox / Edge 均可）。
///
/// CAUTION: 自定义路径同样**不经 shell**：URL 是不可信输入，
/// `cmd /C start` 会把 `&` 当命令分隔符、`%XX` 当变量展开。
pub fn open_url_with(url: &str, custom: &str) -> Result<()> {
    let custom = custom.trim();
    if custom.is_empty() {
        return open_url(url);
    }
    validate_custom_program("浏览器", custom)?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED: u32 = 0x0000_0008 | 0x0000_0020;
        std::process::Command::new(custom)
            .arg(url)
            .creation_flags(DETACHED)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = url;
        open_url(url)
    }
}

/// 用默认或自定义资源管理器定位路径。
///
/// `custom` 为空 = 系统 `explorer /select,<path>`（定位并选中）；
/// 非空 = 启动自定义文件管理器并传路径（Directory Opus / Total Commander
/// 等），`/select` 语法只有真 explorer 认识，第三方程序直接传路径。
///
/// 多文件条目传**首个路径**（调用方负责取第一行）。
pub fn reveal_with(path: &str, custom: &str) -> Result<()> {
    let custom = custom.trim();
    if custom.is_empty() {
        return reveal_in_explorer(path);
    }
    validate_custom_program("资源管理器", custom)?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED: u32 = 0x0000_0008 | 0x0000_0020;
        std::process::Command::new(custom)
            .arg(path)
            .creation_flags(DETACHED)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        open_external(path)
    }
}

/// **打开**一个目录（在文件管理器里进入它，而不是定位并选中它）。
///
/// 与 [`reveal_with`] 的区别是语义：`reveal` 是「找到这个文件」（父目录加
/// 选中），本函数是「进到这个目录里」——关于页「数据目录」点超链接要的是
/// 后者。
///
/// `custom` 为空 = 系统关联（`ShellExecuteW` open，目录默认关联到
/// explorer）；非空 = 启动自定义文件管理器并把目录作为参数。
pub fn open_dir_with(path: &str, custom: &str) -> Result<()> {
    let custom = custom.trim();
    if !custom.is_empty() {
        validate_custom_program("资源管理器", custom)?;
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED: u32 = 0x0000_0008 | 0x0000_0020;
            std::process::Command::new(custom)
                .arg(path)
                .creation_flags(DETACHED)
                .spawn()?;
            return Ok(());
        }
    }
    // 系统关联：对目录用 open 动作即「进入目录」。
    open_url(path)
}

/// 执行外部命令（联动规则入口）。
///
/// `arg` 中的 `{selection}` 会被替换为选中文本。
///
/// # 为什么不经 shell
///
/// 剪贴板内容是**不可信输入**。经 `cmd /C`（或 `sh -c`）执行时，
/// 内容里的 `& | > ^ %` 会被当语法字符：既能截断命令（带 `&` 的 URL
/// 模板必坏），也能注入任意命令（`x&calc` 启动计算器）。本函数按
/// **argv 直传**：替换后的模板作为单个参数交给程序，元字符全是普通字符。
///
/// 代价：模板里的 shell 语法（管道、重定向、`&&`）不再生效。`{selection}`
/// 的契约本来就是「作为单个参数值替换」，直传才是这个契约的正确实现；
/// 需要 shell 语法的场景应配一个 `.bat`/`.sh` 脚本作为 program。
pub fn run_command(program: &str, arg: &str, selection: &str) -> Result<()> {
    let substituted = arg.replace("{selection}", selection);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED: u32 = 0x0000_0008 | 0x0000_0020;
        std::process::Command::new(program)
            .arg(&substituted)
            .creation_flags(DETACHED)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(program)
            .arg(&substituted)
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
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, RRF_RT_ANY, RegCloseKey, RegGetValueW,
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
        // DF-04：`set_autostart` 写的是 `REG_EXPAND_SZ`，读侧若限定
        // `RRF_RT_ANY`，RegGetValueW 会以 ERROR_UNSUPPORTED_TYPE 失败
        // ——读函数永远看不到写函数写的值（自启已开却显示未开）。
        // RRF_RT_ANY 接受任意类型；仅用长度判断「值存在且非空」。
        let st = RegGetValueW(
            hkey,
            windows::core::PCWSTR::null(),
            &value_name,
            RRF_RT_ANY,
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

#[cfg(not(windows))]
pub fn auto_paste_to(_target: Option<isize>, _delay: Duration) -> Result<()> {
    tracing::warn!("当前平台尚未实现自动粘贴");
    Ok(())
}
