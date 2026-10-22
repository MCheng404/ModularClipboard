//! 系统主题探测（Windows 注册表 `AppsUseLightTheme`）。
//!
//! 只在启动时调一次；读不到回退深色并告警，不 panic。

use super::{ThemeMode};

/// 探测系统主题。只返回 [`ThemeMode::Dark`] 或 [`ThemeMode::Light`]。
///
/// Windows 上读 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`
/// 的 `AppsUseLightTheme`（DWORD，0 = 深色，1 = 浅色）。
/// 读不到（非 Windows、键不存在、类型不符）一律回退深色并告警，
/// 不 panic、不阻塞。
///
/// **只在启动时调一次**，不要放进每帧路径。
pub fn detect_system_theme() -> ThemeMode {
    match read_apps_use_light_theme() {
        Some(light) => {
            if light {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            }
        }
        None => {
            tracing::warn!("读取系统主题失败，回退深色");
            ThemeMode::Dark
        }
    }
}

/// 读 `AppsUseLightTheme`。返回 `None` 表示读不到。
#[cfg(windows)]
pub(super) fn read_apps_use_light_theme() -> Option<bool> {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW,
    };

    const SUB_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    const VALUE_NAME: &str = "AppsUseLightTheme";

    let sub_key: Vec<u16> = SUB_KEY.encode_utf16().chain(std::iter::once(0)).collect();
    let value_name: Vec<u16> = VALUE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;

    // 安全性：`RegGetValueW` 只读取注册表，不写；两个 PCWSTR 以 NUL 结尾
    // 且指向本函数的 Vec，生命周期覆盖整个调用。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(sub_key.as_ptr()),
            windows::core::PCWSTR(value_name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut data).cast()),
            Some(&mut size),
        )
    };

    if status != ERROR_SUCCESS {
        tracing::debug!(code = status.0, "RegGetValueW 失败");
        return None;
    }
    Some(data != 0)
}

#[cfg(not(windows))]
pub(super) fn read_apps_use_light_theme() -> Option<bool> {
    None
}
