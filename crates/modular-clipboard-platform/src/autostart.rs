//! 开机自启动（Windows：当前用户的 `Run` 注册表项）。
//!
//! # 为什么用 HKCU 而不是 HKLM / 任务计划
//!
//! - **HKCU** 不需要管理员权限，也不触发 UAC——一个剪贴板工具
//!   要求提权才能开自启是不可接受的。
//! - **任务计划**能做「延迟启动」「登录后 N 秒」，但创建任务的 API
//!   面大得多，且用户很难自己检查和删除。注册表项在
//!   「任务管理器 → 启动」里看得见、关得掉，符合用户直觉。
//!
//! # 双进程下的自启命令
//!
//! 程序拆成常驻进程与界面进程之后，自启动只该起**常驻进程**
//! （`--daemon`）：开机就弹一个窗口是把用户当成了开发机。
//! 常驻进程起来后会守在托盘，用户点它才出界面。
//!
//! 所以写入的命令形如：
//!
//! ```text
//! "C:\path\to\modular-clipboard.exe" --daemon
//! ```
//!
//! 路径**必须带引号**：`Program Files` 这类带空格的路径不加引号会被
//! 拆成两个参数，开机时静默失败（用户只会觉得「自启没生效」）。

use anyhow::{Context, Result};

/// 当前用户的「启动」注册表项。
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 注册表值名。用产品名，便于用户在注册表/任务管理器里找到它。
pub const VALUE_NAME: &str = "ModularClipboard";

/// 自启动时用的命令行参数（只启动常驻进程，不弹界面）。
pub const LAUNCH_ARGS: &[&str] = &["--daemon"];

/// 是否设置为开机自启动。
///
/// `args` 是写在命令里的参数（见 [`LAUNCH_ARGS`]）；传空表示不带参数。
#[cfg(windows)]
pub fn set_enabled(on: bool, args: &[&str]) -> Result<()> {
    use windows::Win32::Foundation::WIN32_ERROR;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SAM_FLAGS, REG_SZ, RegCloseKey,
        RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
    };
    use windows::core::PCWSTR;

    let wide_value: Vec<u16> = VALUE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(run_key_wide().as_ptr()),
            None,
            REG_SAM_FLAGS(KEY_READ.0 | KEY_WRITE.0),
            &mut key,
        )
    };
    if opened != WIN32_ERROR(0) {
        return Err(anyhow::anyhow!(
            "打开 Run 注册表项失败（错误码 {opened:?}）"
        ));
    }

    let result: Result<()> = if on {
        let cmd = command_line(args)?;
        let wide_data: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(wide_data.as_ptr().cast::<u8>(), wide_data.len() * 2)
        };
        // REG_SZ 的字节数要**含结尾的 NUL**——这里 `bytes` 已包含它。
        let status = unsafe {
            RegSetValueExW(
                key,
                PCWSTR(wide_value.as_ptr()),
                None,
                REG_SZ,
                Some(bytes),
            )
        };
        status_to_result(status, format!("写入自启动注册表项失败（{cmd}）"))
    } else {
        let status = unsafe { RegDeleteValueW(key, PCWSTR(wide_value.as_ptr())) };
        // 值本来就不存在也算成功：目标是「不要自启」，
        // 不是「必须删掉了点什么」。
        if status == WIN32_ERROR(2) {
            Ok(()) // ERROR_FILE_NOT_FOUND
        } else {
            status_to_result(status, "删除自启动注册表项失败".into())
        }
    };

    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

#[cfg(windows)]
fn status_to_result(status: windows::Win32::Foundation::WIN32_ERROR, what: String) -> Result<()> {
    if status == windows::Win32::Foundation::WIN32_ERROR(0) {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{what}（错误码 {status:?}）"))
    }
}

#[cfg(windows)]
fn run_key_wide() -> Vec<u16> {
    RUN_KEY.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 当前是否已经设置为开机自启动。
#[cfg(windows)]
pub fn is_enabled() -> bool {
    use windows::Win32::Foundation::WIN32_ERROR;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE, RegCloseKey, RegOpenKeyExW,
        RegQueryValueExW,
    };
    use windows::core::PCWSTR;

    let wide_value: Vec<u16> = VALUE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(run_key_wide().as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
    };
    if opened != WIN32_ERROR(0) {
        return false;
    }
    let mut ty = REG_VALUE_TYPE::default();
    let mut len = 0u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(wide_value.as_ptr()),
            None,
            Some(&mut ty),
            None,
            Some(&mut len),
        )
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    // 查得到且长度 > 0 ⇒ 值存在。内容不必读，读也只是为了显示。
    status == WIN32_ERROR(0) && len > 0
}

/// 拼出写进注册表的命令行：可执行文件 + 参数，路径带引号。
pub fn command_line(args: &[&str]) -> Result<String> {
    let exe = std::env::current_exe().context("无法确定本程序路径")?;
    let quoted = format!("\"{}\"", exe.display());
    if args.is_empty() {
        return Ok(quoted);
    }
    Ok(format!("{quoted} {}", args.join(" ")))
}

#[cfg(not(windows))]
pub fn set_enabled(_on: bool, _args: &[&str]) -> Result<()> {
    anyhow::bail!("当前平台尚未实现开机自启动")
}

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路径必须带引号：不带的话 `Program Files` 会被拆成两个参数，
    /// 开机时静默失败——用户只会觉得「自启没生效」，查不到原因。
    #[test]
    fn command_line_quotes_the_exe_path() {
        let cmd = command_line(LAUNCH_ARGS).expect("应能取到本程序路径");
        assert!(cmd.starts_with('"'), "路径必须带引号，实际：{cmd}");
        assert!(cmd.ends_with("--daemon"), "应带常驻参数，实际：{cmd}");
        assert!(cmd[1..].contains('"'), "闭合引号缺失：{cmd}");
    }

    /// 自启动只该起常驻进程——开机弹窗口是把用户当开发机。
    #[test]
    fn launch_args_are_daemon_only() {
        assert_eq!(LAUNCH_ARGS, ["--daemon"]);
    }

    #[test]
    fn command_line_without_args_is_just_the_exe() {
        let cmd = command_line(&[]).expect("应能取到本程序路径");
        assert!(
            !cmd.trim_end_matches('"').contains(' '),
            "无参时不应有空格分隔的参数：{cmd}"
        );
    }
}
