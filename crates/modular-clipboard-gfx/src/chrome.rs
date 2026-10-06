//! 自绘窗口外壳：无边框命中测试 + Win11 圆角。
//!
//! # 为什么单独一个模块
//!
//! `window.rs` 的窗口过程是**空壳**（原样转发 `DefWindowProcW`），
//! 所有语义事件都在 `EventLoop::poll` 里用 `PeekMessageW` 读队列。
//! 但**命中测试不能放进那条路**——见下节。本模块把「外壳」相关的
//! 纯逻辑与 Win32 调用集中在一处，`window.rs` 只留一个转发点。
//!
//! # 为什么命中测试必须在窗口过程里，不能在 `poll()` 里
//!
//! `WM_NCHITTEST` 的语义**由窗口过程的返回值决定**：返回 `HTCAPTION`
//! 系统才知道「这里可以拖动窗口」，返回 `HTLEFT` 才知道「这里是左边框」。
//! 这个值只能通过窗口过程的 `LRESULT` 交给系统。
//!
//! `EventLoop::poll` 用 `PeekMessageW` 把消息从队列里**取走**再
//! `DispatchMessageW`。若在 `poll()` 里"处理"它，窗口过程根本没被调用，
//! 系统拿到的返回值是 0（`HTNOWHERE`），命中测试静默失效——
//! 表现为「无边框窗口哪都拖不动、哪都拉不动」，且不报任何错。
//!
//! 所以 [`handle_non_client`] 从 `wnd_proc` 调用，
//! `poll()` 对 `WM_NCHITTEST` **不做任何事**（照常 `DispatchMessageW`）。
//!
//! # 拖动方案：交给系统（方案 A）
//!
//! 命中测试在拖动区返回 `HTCAPTION`，系统随即把下一次按下转成
//! `WM_NCLBUTTONDOWN(wParam = HTCAPTION)`，而 `DefWindowProcW`
//! 对它会自动进入**系统原生的移动循环**。
//!
//! 因此**不需要** `ReleaseCapture()` + `SendMessageW(WM_NCLBUTTONDOWN,
//! HTCAPTION, 0)` 那套手工模拟。手工模拟反而有两个坑：
//!
//! 1. `SendMessageW` 会**重入** `wnd_proc`，若不加标志位守卫就是无限递归；
//! 2. 手写 `SetWindowPos` 循环要自己实现跨显示器边界、边缘吸附、
//!    拖到顶部最大化、多显示器断开后的坐标重映射。
//!
//! 「不使用系统窗口」指的是不画系统标题栏的**外观**，不是不用系统的
//! **移动机制**。原生手感（吸附、跨屏、拖顶最大化）是白拿的，不要重新发明。
//!
//! resize 同理：边缘返回 `HTLEFT`/`HTTOPLEFT` 等，`DefWindowProcW`
//! 自动进入系统原生缩放循环。
//!
//! # 状态为什么是全局的
//!
//! 命中测试在窗口过程里执行，而窗口过程是 `extern "system" fn`，
//! **拿不到 `Window` 实例**。本项目不做子类化（见 `window.rs` 模块文档的
//! 架构决策），所以没有 `SetWindowLongPtrW` 写的用户数据可用。
//!
//! 拖动区与边框宽度因此放在进程级全局里。这在本项目成立：
//! **一个进程只有一个主窗口**。若将来要支持多窗口，必须改成子类化——
//! 那时这些全局量要变成 per-HWND 存储。

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use egui::emath::{Pos2, Rect, Vec2};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTLEFT, HTRIGHT,
    HTTOP, HTTOPLEFT, HTTOPRIGHT, WM_NCHITTEST,
};

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
    /// 上 / 下 / 左 / 右边缘，系统接管缩放。
    Top,
    Bottom,
    Left,
    Right,
    /// 四角。
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
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
        }
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
pub fn hit_test(pos: Pos2, size: Vec2, border: f32, drag: Option<Rect>) -> HitZone {
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

    if let Some(r) = drag
        && r.width() > 0.0
        && r.height() > 0.0
        && r.contains(pos)
    {
        return HitZone::Caption;
    }

    HitZone::Client
}

// ---------------------------------------------------------------------------
// 全局外壳状态（窗口过程拿不到实例，见模块文档）
// ---------------------------------------------------------------------------

/// 拖动区，单位**逻辑点**。`DRAG_X1 <= DRAG_X0` 表示禁用。
static DRAG_X0: AtomicI32 = AtomicI32::new(0);
static DRAG_Y0: AtomicI32 = AtomicI32::new(0);
static DRAG_X1: AtomicI32 = AtomicI32::new(0);
static DRAG_Y1: AtomicI32 = AtomicI32::new(0);

/// 边缘宽度，以 `f32` 的**位模式**存（原子量里没有 f32）。
static RESIZE_BORDER_BITS: AtomicU32 = AtomicU32::new(0);

/// 设置自绘标题栏（拖动区）的位置，单位逻辑点、客户区坐标系。
///
/// 传 `None` 关闭拖动区（此时只有边缘能resize，标题栏区域归 egui）。
///
/// # 上层必须排除自己的按钮
///
/// 落在矩形**内**的按下会被系统拿走变成拖动，egui 收不到。
/// 因此关闭 / 最小化 / 设置这类按钮必须**画在拖动区之外**，
/// 或者每帧把拖动区设成「标题栏减去按钮」的矩形。
///
/// 每帧调一次即可：内部只写 4 个原子量，没有系统调用。
pub fn set_drag_region(rect: Option<Rect>) {
    match rect {
        Some(r) => {
            DRAG_X0.store(r.min.x.round() as i32, Ordering::Relaxed);
            DRAG_Y0.store(r.min.y.round() as i32, Ordering::Relaxed);
            DRAG_X1.store(r.max.x.round() as i32, Ordering::Relaxed);
            DRAG_Y1.store(r.max.y.round() as i32, Ordering::Relaxed);
        }
        None => {
            // 用 x1 <= x0 表示禁用，避免额外一个「是否有效」标志位。
            DRAG_X1.store(0, Ordering::Relaxed);
            DRAG_X0.store(0, Ordering::Relaxed);
            DRAG_Y0.store(0, Ordering::Relaxed);
            DRAG_Y1.store(0, Ordering::Relaxed);
        }
    }
}

/// 当前拖动区。
fn drag_region() -> Option<Rect> {
    let x0 = DRAG_X0.load(Ordering::Relaxed);
    let y0 = DRAG_Y0.load(Ordering::Relaxed);
    let x1 = DRAG_X1.load(Ordering::Relaxed);
    let y1 = DRAG_Y1.load(Ordering::Relaxed);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Rect::from_min_max(
        Pos2::new(x0 as f32, y0 as f32),
        Pos2::new(x1 as f32, y1 as f32),
    ))
}

/// 设置边缘 resize 区宽度，单位**逻辑点**。`<= 0` 表示禁用 resize。
pub fn set_resize_border(points: f32) {
    RESIZE_BORDER_BITS.store(points.to_bits(), Ordering::Relaxed);
}

/// 当前边缘宽度，逻辑点。
fn resize_border() -> f32 {
    f32::from_bits(RESIZE_BORDER_BITS.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Win32 层
// ---------------------------------------------------------------------------

/// 从 `lParam` 取**有符号** 16 位x 坐标。
///
/// `windows` crate 不提供 `GET_X_LPARAM` / `LOWORD` 宏（MEMORY.md 坑 35）。
/// 必须按有符号解释：指针可以移出客户区，`65535` 是 `-1` 而不是 `65535`。
fn x_lparam(lp: isize) -> i32 {
    (lp as u32 as u16 as i16) as i32
}

/// 从 `lParam` 取有符号 16 位 y 坐标。
fn y_lparam(lp: isize) -> i32 {
    ((lp as u32 >> 16) as u16 as i16) as i32
}

/// 窗口过程里的 non-client 分支。
///
/// 返回 `Some(lresult)` 表示已处理，`None` 表示原样转发
/// `DefWindowProcW`。**转发路径必须保持默认处理**——窗口过程被改动会
/// 让 `CreateWindowExW` 报 `0x8007007E`（MEMORY.md 坑 26）。
///
/// # Safety
///
/// 由 `wnd_proc` 在窗口过程上下文中调用。`hwnd` 必须是真实窗口。
pub(crate) unsafe fn handle_non_client(
    hwnd: HWND,
    msg: u32,
    _wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    if msg != WM_NCHITTEST {
        return None;
    }

    // 取不到客户区尺寸就不能判定边缘，返回 HTCLIENT 让事件落回 egui。
    // 比返回错误的 HT* 安全：最坏结果是那一次拖动不生效。
    let mut cr = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut cr) }.is_err() {
        return Some(LRESULT(HTCLIENT as isize));
    }
    let scale = {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
    };
    let size = Vec2::new(
        (cr.right - cr.left).max(0) as f32 / scale,
        (cr.bottom - cr.top).max(0) as f32 / scale,
    );

    // `WM_NCHITTEST` 的 lParam 是**屏幕**坐标，要换算成客户区坐标。
    let mut pt = POINT {
        x: x_lparam(lparam.0),
        y: y_lparam(lparam.0),
    };
    // BOOL 是 must_use；失败时 pt 保持屏幕坐标，判定结果无意义但无害。
    let _ = unsafe { ScreenToClient(hwnd, &mut pt) };
    let pos = Pos2::new(pt.x as f32 / scale, pt.y as f32 / scale);

    let zone = hit_test(pos, size, resize_border(), drag_region());
    Some(LRESULT(zone.hit_code() as isize))
}

/// 打开 Win11 圆角，并去掉系统自动画的那条 1px 描边。
///
/// **优雅降级**：Win10 或调用失败时只记一条 debug，不 panic、不中断启动。
/// 圆角是锦上添花，缺了不影响任何功能。
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

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Vec2 = Vec2::new(400.0, 600.0);
    const B: f32 = 6.0;

    fn p(x: f32, y: f32) -> Pos2 {
        Pos2::new(x, y)
    }

    /// 中心永远应该是客户区。
    #[test]
    fn center_is_client() {
        assert_eq!(hit_test(p(200.0, 300.0), SIZE, B, None), HitZone::Client);
    }

    #[test]
    fn no_drag_region_means_only_edges_are_special() {
        let drag = Rect::from_min_max(p(0.0, 0.0), p(400.0, 32.0));
        // 拖动区被忽略（传 None）
        assert_eq!(hit_test(p(200.0, 10.0), SIZE, B, None), HitZone::Client);
        // 传了才生效
        assert_eq!(
            hit_test(p(200.0, 10.0), SIZE, B, Some(drag)),
            HitZone::Caption
        );
    }

    #[test]
    fn four_corners_map_to_four_zones() {
        assert_eq!(hit_test(p(0.0, 0.0), SIZE, B, None), HitZone::TopLeft);
        assert_eq!(hit_test(p(399.9, 0.0), SIZE, B, None), HitZone::TopRight);
        assert_eq!(hit_test(p(0.0, 599.9), SIZE, B, None), HitZone::BottomLeft);
        assert_eq!(
            hit_test(p(399.9, 599.9), SIZE, B, None),
            HitZone::BottomRight
        );
    }

    #[test]
    fn four_edges_map_to_four_zones() {
        // 取每条边的中点，避开角
        assert_eq!(hit_test(p(200.0, 1.0), SIZE, B, None), HitZone::Top);
        assert_eq!(hit_test(p(200.0, 598.0), SIZE, B, None), HitZone::Bottom);
        assert_eq!(hit_test(p(1.0, 300.0), SIZE, B, None), HitZone::Left);
        assert_eq!(hit_test(p(398.0, 300.0), SIZE, B, None), HitZone::Right);
    }

    /// 边缘优先于标题栏——否则顶部边缘永远拉不动。
    #[test]
    fn edges_take_priority_over_drag_region() {
        let drag = Rect::from_min_max(p(0.0, 0.0), p(400.0, 40.0));
        assert_eq!(hit_test(p(200.0, 2.0), SIZE, B, Some(drag)), HitZone::Top);
        // 离开边缘后才落到标题栏
        assert_eq!(
            hit_test(p(200.0, 20.0), SIZE, B, Some(drag)),
            HitZone::Caption
        );
    }

    #[test]
    fn drag_region_excludes_everything_outside() {
        let drag = Rect::from_min_max(p(10.0, 4.0), p(390.0, 30.0));
        assert_eq!(hit_test(p(9.0, 10.0), SIZE, B, Some(drag)), HitZone::Client);
        assert_eq!(hit_test(p(200.0, 31.0), SIZE, B, Some(drag)), HitZone::Client);
        assert_eq!(hit_test(p(200.0, 10.0), SIZE, B, Some(drag)), HitZone::Caption);
    }

    #[test]
    fn zero_border_disables_resize() {
        for (x, y) in [(0.0, 0.0), (1.0, 1.0), (399.0, 599.0)] {
            assert_eq!(
                hit_test(p(x, y), SIZE, 0.0, None),
                HitZone::Client,
                "border=0 时 ({x},{y}) 不该是边缘"
            );
        }
    }

    #[test]
    fn negative_border_is_treated_as_disabled() {
        assert_eq!(hit_test(p(0.0, 0.0), SIZE, -5.0, None), HitZone::Client);
    }

    /// 小窗口：角会重叠，必须钳制而不是让某个分支永远不成立。
    #[test]
    fn tiny_window_still_has_all_four_corners() {
        let size = Vec2::new(8.0, 8.0);
        let b = 6.0; // 大于 size/2
        assert_eq!(hit_test(p(0.0, 0.0), size, b, None), HitZone::TopLeft);
        assert_eq!(hit_test(p(7.9, 0.0), size, b, None), HitZone::TopRight);
        assert_eq!(hit_test(p(0.0, 7.9), size, b, None), HitZone::BottomLeft);
        assert_eq!(hit_test(p(7.9, 7.9), size, b, None), HitZone::BottomRight);
    }

    #[test]
    fn degenerate_size_does_not_panic() {
        for size in [Vec2::ZERO, Vec2::new(f32::NAN, 10.0), Vec2::new(-5.0, -5.0)] {
            let _ = hit_test(p(0.0, 0.0), size, B, None);
        }
    }

    #[test]
    fn nan_border_does_not_panic() {
        let _ = hit_test(p(1.0, 1.0), SIZE, f32::NAN, None);
    }

    /// 指针在窗口外一律 Client：否则鼠标移出去时窗口仍被"抓住"。
    #[test]
    fn outside_is_always_client() {
        let drag = Rect::from_min_max(p(-100.0, -100.0), p(900.0, 900.0));
        for pos in [p(-1.0, 10.0), p(10.0, -1.0), p(401.0, 10.0), p(10.0, 601.0)] {
            assert_eq!(
                hit_test(pos, SIZE, B, Some(drag)),
                HitZone::Client,
                "{pos:?} 在窗口外却不是 Client"
            );
        }
    }

    #[test]
    fn zero_area_drag_region_is_ignored() {
        let empty = Rect::from_min_max(p(10.0, 10.0), p(10.0, 10.0));
        assert_eq!(hit_test(p(10.0, 10.0), SIZE, B, Some(empty)), HitZone::Client);
    }

    #[test]
    fn every_zone_has_a_distinct_hit_code() {
        let zones = [
            HitZone::Client,
            HitZone::Caption,
            HitZone::Top,
            HitZone::Bottom,
            HitZone::Left,
            HitZone::Right,
            HitZone::TopLeft,
            HitZone::TopRight,
            HitZone::BottomLeft,
            HitZone::BottomRight,
        ];
        let mut codes: Vec<u32> = zones.iter().map(|z| z.hit_code()).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before, "有区域的 hit_code重复了：{codes:?}");
    }

    #[test]
    fn hit_codes_match_win32_constants() {
        // 硬编码期望值，而不是引用 windows crate 的常量——
        // 否则常量写错时测试会跟着一起错（MEMORY.md 坑 84）。
        assert_eq!(HitZone::Client.hit_code(), 1);
        assert_eq!(HitZone::Caption.hit_code(), 2);
        assert_eq!(HitZone::Top.hit_code(), 12);
        assert_eq!(HitZone::Bottom.hit_code(), 15);
        assert_eq!(HitZone::Left.hit_code(), 10);
        assert_eq!(HitZone::Right.hit_code(), 11);
        assert_eq!(HitZone::TopLeft.hit_code(), 13);
        assert_eq!(HitZone::TopRight.hit_code(), 14);
        assert_eq!(HitZone::BottomLeft.hit_code(), 16);
        assert_eq!(HitZone::BottomRight.hit_code(), 17);
    }

    // ---- lParam拆包 ----

    #[test]
    fn lparam_coords_are_signed() {
        // x = -1（移出客户区左侧），y = -1
        let lp = (0xFFFFu32 | (0xFFFFu32 << 16)) as isize;
        assert_eq!(x_lparam(lp), -1);
        assert_eq!(y_lparam(lp), -1);

        // 正常值
        let lp = ((300u32) | (400u32 << 16)) as isize;
        assert_eq!(x_lparam(lp), 300);
        assert_eq!(y_lparam(lp), 400);
    }

    #[test]
    fn lparam_high_bit_is_negative() {
        // y 的 16 位最高位置 1 => -32768
        let lp = ((0u32) | (0x8000u32 << 16)) as isize;
        assert_eq!(y_lparam(lp), -32768);
    }

    // ---- 全局状态往返 ----

    #[test]
    fn drag_region_round_trips() {
        set_drag_region(Some(Rect::from_min_max(p(0.0, 0.0), p(400.0, 32.0))));
        assert_eq!(
            drag_region(),
            Some(Rect::from_min_max(p(0.0, 0.0), p(400.0, 32.0)))
        );
        set_drag_region(None);
        assert_eq!(drag_region(), None, "None 必须真的禁用拖动区");
    }

    #[test]
    fn resize_border_round_trips() {
        set_resize_border(9.5);
        assert_eq!(resize_border(), 9.5);
        set_resize_border(DEFAULT_RESIZE_BORDER);
        assert_eq!(resize_border(), DEFAULT_RESIZE_BORDER);
    }
}