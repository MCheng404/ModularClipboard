//! 圆角开关（DWMWCP）。
//!
//! 与自绘投影互斥：要自绘投影就必须 `DWMWCP_DONOTROUND`，
//! 否则 DWM 圆角会把每像素 alpha 的投影裁掉一角。

use std::ffi::c_void;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::DWMWA_BORDER_COLOR;
use windows::Win32::Graphics::Dwm::DWMWA_COLOR_NONE;
use windows::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE;
use windows::Win32::Graphics::Dwm::DWMWCP_DONOTROUND;
use windows::Win32::Graphics::Dwm::DWMWCP_ROUND;
use windows::Win32::Graphics::Dwm::DWM_WINDOW_CORNER_PREFERENCE;
use windows::Win32::Graphics::Dwm::DwmSetWindowAttribute;

/// 打开 Win11 圆角，并去掉系统自动画的那条 1px 描边。
///
/// **优雅降级**：Win10 或调用失败时只记一条 debug，不 panic、不中断启动。
/// 圆角是锦上添花，缺了不影响任何功能。
/// 明确**不要**系统圆角（`DWMWCP_DONOTROUND`）。
///
/// # 为什么需要这个
///
/// 微软文档写得很清楚：使用**每像素 alpha 分层**（在窗口周围画透明像素
/// 以实现自定义阴影）的窗口，**无法**被 DWM 圆角化——系统把它当非矩形
/// 窗口处理。反过来也成立：只要 DWM 还在尝试圆角，我们自绘的投影就会
/// 被它那一刀切掉（实机表现为四个角的三角形）。
///
/// 因此「自研圆角 + 自研投影」的窗口必须**显式关掉**系统圆角，让窗口
/// 保持矩形，圆角与投影全部由渲染层画。
pub fn disable_rounded_corners(hwnd: HWND) {
    let pref = DWMWCP_DONOTROUND;
    if let Err(e) = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const DWM_WINDOW_CORNER_PREFERENCE as *const c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    } {
        tracing::debug!("关闭系统圆角失败（Win10 或 DWM 不可用，属预期）: {e:?}");
    }
}

pub fn apply_rounded_corners(hwnd: HWND) {
    // Win11 才认DWMWA_WINDOW_CORNER_PREFERENCE（属性号 33）。
    // Win10 上返回 E_INVALIDARG，属预期。
    let pref = DWMWCP_ROUND;
    if let Err(e) = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const DWM_WINDOW_CORNER_PREFERENCE as *const c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    } {
        tracing::debug!("设置圆角失败（Win10 或 DWM 不可用，属预期）: {e:?}");
    }

    // Win11 会给无边框窗口补一条 1px 描边，配上 egui 自绘的背景很违和。
    // DWMWA_COLOR_NONE(0xFFFFFFFE) 表示「不要边框」。
    let none = DWMWA_COLOR_NONE;
    let res = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &none as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    if let Err(e) = res {
        tracing::debug!("关闭系统描边失败（Win10 或 DWM 不可用，属预期）: {e:?}");
    }
}
