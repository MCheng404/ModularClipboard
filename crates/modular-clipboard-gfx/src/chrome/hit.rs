//! 命中判定：窗口内某个逻辑点落在标题栏 / 边 / 角 / 面板的哪里。
//!
//! 纯函数，不碰 Win32 状态——`hit_test` 只看「点 + 窗口尺寸 +
//! 缩放边宽 + 拖动区」，这让它能被单元测试直接覆盖。

use egui::emath::Pos2;
use egui::emath::Rect;
use egui::emath::Vec2;
use windows::Win32::UI::WindowsAndMessaging::HTBOTTOM;
use windows::Win32::UI::WindowsAndMessaging::HTBOTTOMLEFT;
use windows::Win32::UI::WindowsAndMessaging::HTBOTTOMRIGHT;
use windows::Win32::UI::WindowsAndMessaging::HTCAPTION;
use windows::Win32::UI::WindowsAndMessaging::HTCLIENT;
use windows::Win32::UI::WindowsAndMessaging::HTLEFT;
use windows::Win32::UI::WindowsAndMessaging::HTRIGHT;
use windows::Win32::UI::WindowsAndMessaging::HTTOP;
use windows::Win32::UI::WindowsAndMessaging::HTTOPLEFT;
use windows::Win32::UI::WindowsAndMessaging::HTTOPRIGHT;
use windows::Win32::UI::WindowsAndMessaging::HTTRANSPARENT;
use windows::Win32::UI::WindowsAndMessaging::IDC_ARROW;
use windows::Win32::UI::WindowsAndMessaging::IDC_SIZENESW;
use windows::Win32::UI::WindowsAndMessaging::IDC_SIZENS;
use windows::Win32::UI::WindowsAndMessaging::IDC_SIZENWSE;
use windows::Win32::UI::WindowsAndMessaging::IDC_SIZEWE;

// 非客户区指针消息（裸常量，避免 crate 枚举类型差异）。
// 用裸常量与 window.rs 里 SC_CLOSE 的处理风格一致。
pub(crate) const WM_NCMOUSEMOVE: u32 = 0x00A0;
pub(crate) const WM_NCLBUTTONDOWN: u32 = 0x00A1;
pub(crate) const WM_NCLBUTTONUP: u32 = 0x00A2;
pub(crate) const WM_NCLBUTTONDBLCLK: u32 = 0x00A3;
// 这几条客户区消息在 SetCapture 后也可能落到拖动窗口上。
pub(crate) const WM_MOUSEMOVE_C: u32 = 0x0200;
pub(crate) const WM_LBUTTONUP_C: u32 = 0x0202;
pub(crate) const WM_CAPTURECHANGED_C: u32 = 0x0215;
// WM_SETCURSOR（0x0020）不在 WM_NC* 段里，但同样是窗口过程
// 决定光标形状的消息，这里一并接管（见 handle_non_client）。
pub(crate) const WM_SETCURSOR: u32 = 0x0020;

/// 边缘 resize 区的默认宽度，**逻辑点**。
///
/// 取6 是 Windows 11 原生无边框窗口的观感值。物理像素由
/// [`GetDpiForWindow`] 换算，所以高分屏上不会细得点不中。
pub const DEFAULT_RESIZE_BORDER: f32 = 6.0;

// ---------------------------------------------------------------------------
// 纯逻辑：命中测试（可直接单测，不碰任何 Win32）
// ---------------------------------------------------------------------------

/// 指针落在窗口的哪个区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitZone {
    /// 客户区，事件照常交给 egui。
    Client,
    /// 拖动区（自绘标题栏），系统接管窗口移动。
    Caption,
    /// 上 / 下 / 左 / 右边缘，非模态指针捕获驱动缩放。
    Top,
    Bottom,
    Left,
    Right,
    /// 四角。
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    /// **点击穿透**：指针不在窗口的有效内容上（弹窗的投影 / 透明边距）。
    ///
    /// 悬浮面板的窗口矩形要比面板本身大一圈来容纳投影，那圈区域
    /// 画出来是透明的。命中判定按**窗口矩形**而不是按像素 alpha，
    /// 于是不处理的话，点在投影上会被这个窗口吃掉——用户想点
    /// 面板后面的东西却没反应（面板越靠边越明显）。
    /// 返回 `HTTRANSPARENT` 后点击落到下面的窗口，行为与「那里
    /// 没有这个窗口」完全一致。
    Transparent,
}

impl HitZone {
    /// 该区域对应的 `WM_NCHITTEST` 返回值。
    pub fn hit_code(self) -> u32 {
        match self {
            HitZone::Client => HTCLIENT,
            HitZone::Caption => HTCAPTION,
            HitZone::Top => HTTOP,
            HitZone::Bottom => HTBOTTOM,
            HitZone::Left => HTLEFT,
            HitZone::Right => HTRIGHT,
            HitZone::TopLeft => HTTOPLEFT,
            HitZone::TopRight => HTTOPRIGHT,
            HitZone::BottomLeft => HTBOTTOMLEFT,
            HitZone::BottomRight => HTBOTTOMRIGHT,
            // NOTE: windows crate 里 `HTTRANSPARENT` 是 `i32`，
            // 其余命中码是 `u32`——这里显式转换而不是让整个 match 迁就它。
            HitZone::Transparent => HTTRANSPARENT as u32,
        }
    }

    /// 该区域应显示的系统光标资源 ID。
    ///
    /// - 边/角：对应的 resize 光标（`IDC_SIZEWE` 等）；
    /// - 拖动区：箭头（与原生窗口的标题栏光标一致，移动窗口
    ///   本来就是箭头）；
    /// - 客户区：返回 `None`——那里由 egui 的光标决定
    ///   （手型 / I-beam 等）。
    pub fn cursor_id(self) -> Option<u16> {
        Some(match self {
            HitZone::Left | HitZone::Right => IDC_SIZEWE.0 as u16,
            HitZone::Top | HitZone::Bottom => IDC_SIZENS.0 as u16,
            HitZone::TopLeft | HitZone::BottomRight => IDC_SIZENWSE.0 as u16,
            HitZone::TopRight | HitZone::BottomLeft => IDC_SIZENESW.0 as u16,
            HitZone::Caption => IDC_ARROW.0 as u16,
            // 透明区：光标由下面的窗口负责（它才是命中目标）。
            HitZone::Client | HitZone::Transparent => return None,
        })
    }
}

/// 弹窗命中判定：面板之外 = 点击穿透。**纯逻辑**，可直接单测。
///
/// `panel = None`（主窗口）⇒ `None`，走常规 [`hit_test`]。
/// `Some(rect)` 且点在 rect 外 ⇒ [`HitZone::Transparent`]（点击落到
/// 下面的窗口）；点在 rect 内 ⇒ `None`（照常由 `hit_test` 判）。
///
/// 抽成独立函数而不是塞在 `hit_zone_at_screen` 里：那条路径要真实
/// HWND + `ScreenToClient`，单测里根本执行不到——而「面板外必须穿透」
/// 恰恰是最该有测试的一条（漏了就是「点投影把底下的窗口吃掉」）。
pub fn popup_zone(pos: Pos2, panel: Option<Rect>) -> Option<HitZone> {
    match panel {
        Some(r) if !r.contains(pos) => Some(HitZone::Transparent),
        _ => None,
    }
}

/// 判定 `pos`（**逻辑点**，客户区坐标系）落在哪个区域。
///
/// `drag` 是自绘标题栏的矩形（逻辑点）。`None` 表示**没有拖动区**——
/// 此时除边缘外全部是 `Client`。
///
/// # 优先级：角 > 边 > 标题栏 > 客户区
///
/// 边缘优先于拖动区，与原生窗口一致：原生窗口的上边框本来就压在
/// 标题栏之上，两者共存时用户预期是「最上面几条像素是缩放」。
/// 反过来（标题栏优先）会让顶部边缘永远拉不动。
///
/// # 退化输入
///
/// - `border <= 0` ⇒ 只有 `Client` 与 `Caption`（无 resize）。
/// - 窗口比 `2 * border` 还小（角与角重叠）⇒ `border` 被钳到
///   `size / 2`，保证四角判定不会互相吞掉，且中心区始终存在。
///   命中测试。`drag` 是自绘标题栏拖动区（逻辑点、客户区坐标系）。
///
/// # IMPORTANT: 调用方**必须**把交互控件从 `drag` 里排除
///
/// 返回 `HitZone::Caption` 会让系统把下一次按下转成
/// `WM_NCLBUTTONDOWN`（非客户区消息），**根本不会产生
/// `WM_LBUTTONDOWN`** —— 于是 egui 收不到点击。
///
/// 早前 UI 层把**整个顶栏**报成拖动区：
/// `f.state.drag_region = Some(bar);`，而 `bar` 里含齿轮、垃圾桶、
/// 搜索框。结果点这些控件毫无反应（表现是「设置打不开」），
/// 但 paint 层的交互测试**全部通过** —— 测试环境没有
/// `WM_NCHITTEST`，这条路径在测试里根本不执行。
pub fn hit_test(pos: Pos2, size: Vec2, border: f32, drag: &[Rect]) -> HitZone {
    let inside = pos.x >= 0.0 && pos.y >= 0.0 && pos.x < size.x && pos.y < size.y;

    if !inside {
        // 指针在窗口外：交给系统（会变成 HTNOWHERE / 激活相邻窗口）。
        // 绝不能在这里返回 Caption 之类的非 client 值——
        // 那会让鼠标移到窗口外时窗口仍被"抓住"。
        return HitZone::Client;
    }

    // NaN / 负尺寸不该出现，但边界函数不能因此 panic。
    let w = if size.x.is_finite() && size.x > 0.0 { size.x } else { 0.0 };
    let h = if size.y.is_finite() && size.y > 0.0 { size.y } else { 0.0 };
    let mut b = if border.is_finite() && border > 0.0 { border } else { 0.0 };
    b = b.min(w / 2.0).min(h / 2.0);

    if b > 0.0 {
        let near_left = pos.x < b;
        let near_right = pos.x >= w - b;
        let near_top = pos.y < b;
        let near_bottom = pos.y >= h - b;

        // 角先判：否则小窗口下角会被边吞掉。
        match (near_top, near_bottom, near_left, near_right) {
            (true, false, true, false) => return HitZone::TopLeft,
            (true, false, false, true) => return HitZone::TopRight,
            (false, true, true, false) => return HitZone::BottomLeft,
            (false, true, false, true) => return HitZone::BottomRight,
            _ => {}
        }
        if near_top {
            return HitZone::Top;
        }
        if near_bottom {
            return HitZone::Bottom;
        }
        if near_left {
            return HitZone::Left;
        }
        if near_right {
            return HitZone::Right;
        }
    }

    // 拖动区是**多段**的：控件从中挖空，点任意一段才算标题栏。
    //
    // IMPORTANT: 这里必须逐段判`contains`。早前只有一段（整个顶栏），
    // 于是齿轮、垃圾桶、搜索框全被算成 `Caption`，
    // 系统不产生 `WM_LBUTTONDOWN`，egui 收不到点击。
    for r in drag {
        if r.width() > 0.0 && r.height() > 0.0 && r.contains(pos) {
            return HitZone::Caption;
        }
    }

    HitZone::Client
}
