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
//! # 拖动方案：命中测试仍借系统，移动由我们自己驱动（非模态）
//!
//! 命中测试在拖动区返回 `HTCAPTION`，系统随即把下一次按下转成
//! `WM_NCLBUTTONDOWN(wParam = HTCAPTION)`。
//!
//! **但不能让 `DefWindowProcW` 处理这条消息**——它会进入一个
//! **系统原生的模态移动循环**，该循环在松开鼠标前独占消息分发。
//! 本项目的 Vulkan 帧循环跑在普通消息循环里，模态移动期间完全不执行，
//! 于是窗口在动、**背景磨砂却停在拖动开始前那一帧**：自研磨砂每帧都要
//! 重新抓取窗口身后的桌面，帧循环一停，捕获内容就过期。
//!
//! 因此 [`handle_non_client`] 拦截 `WM_NCLBUTTONDOWN(HTCAPTION)`：
//! 不转发 `DefWindowProcW`（杜绝模态循环），改用 `SetCapture` 接管指针；
//! 随后的 `WM_NCMOUSEMOVE` / `WM_MOUSEMOVE` 走**普通消息队列**，
//! 我们在其中按光标位移 `SetWindowPos` 移动窗口。这样主帧循环每轮照常
//! 抓取、模糊、present，拖动时背景实时跟随。
//!
//! 边缘与角落的 resize 也走同一套非模态指针捕获：系统缩放循环
//! 同样会暂停 Vulkan 帧循环；而无 `WS_THICKFRAME` 的 `WS_POPUP`
//! 窗口也不能依赖系统一定启动缩放。尺寸变化通过 `SetWindowPos`
//! 触发 `WM_SIZE`，由普通消息循环重建交换链。
//! 双击标题栏最大化 / 还原由我们自己调 `ShowWindow` 保留。
//!
//! 代价：失去 Aero Snap「拖到屏幕边缘自动分屏」（系统模态循环独有，
//! 非模态自移无法白拿）。跨显示器的负坐标移动天然可用。
//!
//! ## 光标指示：WM_SETCURSOR + 帧循环防覆盖
//!
//! resize 边/角的光标由本模块在 [`handle_non_client`] 里显式接管
//! （`WM_SETCURSOR` 分支按命中码设 `IDC_SIZEWE` / `IDC_SIZENS` 等）。
//! 两层原因：无 `WS_THICKFRAME` 的 `WS_POPUP` 窗口上，系统默认
//! 处理不保证显示 resize 光标；且主窗口帧循环每帧都会用 egui 的
//! 光标覆盖（边缘处是 Default → 箭头）。UI 侧每帧调
//! [`shell_cursor_id`] 先问外壳、[`pointer_is_over`] 判断指针归属，
//! 避免把别的窗口（子卡片、设置窗）的外壳光标覆盖回箭头。
//!
//! # 状态是 **per-HWND** 的
//!
//! 命中测试在窗口过程里执行，而窗口过程是 `extern "system" fn`，
//! **拿不到 `Window` 实例**；本项目也不做子类化（见 `window.rs` 的
//! 架构决策），没有 `SetWindowLongPtrW` 写的用户数据可用。
//!
//! 所以退而求其次：用一张**按 HWND 索引**的静态表（`CHROME`）。
//! 窗口过程拿得到 `hwnd`，足以把状态区分开。
//!
//! NOTE: **曾经是进程级全局，这是错的**。那时本项目只有一个主窗口，
//! 后来加了卡片子窗口与设置窗口，于是：
//!
//! - 新窗口**继承**主窗口的拖动区 ⇒ 设置窗口左上角一整块被判成
//!   `HTCAPTION`，点击变成拖动，控件点不动；
//! - `Window::new` 会调 `set_resize_border(默认)` ⇒ **新建一个窗口
//!   会把主窗口的边框设置也改掉**。
//!
//! 症状是「两个窗口互相串味」，而且只在多窗口时出现——单窗口时
//! 完全测不出来。改成 per-HWND 后这类问题从根上消失。
//!
//! 表项在 `Window::drop` 里由 [`forget`] 清理：HWND 会被系统复用，
//! 不清理的话新窗口可能捡到旧窗口的拖动区。

use std::ffi::c_void;

use egui::emath::{Pos2, Rect, Vec2};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetCursorPos, GetWindowRect, IsZoomed, LoadCursorW, SetCursor,
    SetWindowPos, ShowWindow, WindowFromPoint,
    SWP_NOACTIVATE, SWP_NOZORDER, SW_MAXIMIZE, SW_RESTORE,
    HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTLEFT, HTRIGHT,
    HTTOP, HTTOPLEFT, HTTOPRIGHT, HTTRANSPARENT, WM_NCHITTEST,
    IDC_ARROW, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE,
};

// 非客户区指针消息（裸常量，避免 crate 枚举类型差异）。
// 用裸常量与 window.rs 里 SC_CLOSE 的处理风格一致。
const WM_NCMOUSEMOVE: u32 = 0x00A0;
const WM_NCLBUTTONDOWN: u32 = 0x00A1;
const WM_NCLBUTTONUP: u32 = 0x00A2;
const WM_NCLBUTTONDBLCLK: u32 = 0x00A3;
// 这几条客户区消息在 SetCapture 后也可能落到拖动窗口上。
const WM_MOUSEMOVE_C: u32 = 0x0200;
const WM_LBUTTONUP_C: u32 = 0x0202;
const WM_CAPTURECHANGED_C: u32 = 0x0215;
// WM_SETCURSOR（0x0020）不在 WM_NC* 段里，但同样是窗口过程
// 决定光标形状的消息，这里一并接管（见 handle_non_client）。
const WM_SETCURSOR: u32 = 0x0020;

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

/// 设置弹窗的有效内容矩形（逻辑点、客户区坐标系）。
///
/// 传 `None` 关闭（窗口矩形整体可点，主窗口即此）。
///
/// # 为什么需要
///
/// 悬浮面板的窗口要画投影，窗口矩形比面板大一圈；那圈区域的像素是
/// 透明的，但**命中判定不看像素**（那要读回 GPU，慢且不可靠）。
/// 不登记的话，点在投影上会被这个「看不见的窗口」吃掉——用户想点
/// 面板后面的东西却毫无反应，面板越靠屏幕边越明显。
///
/// 每帧调一次即可：只改内存里一个 `Option<Rect>`，没有系统调用。
pub fn set_panel_shape(hwnd: HWND, rect: Option<Rect>) {
    set_chrome_of(hwnd, |c| c.panel = rect);
}

/// 该窗口登记的有效内容矩形（未登记 = 整块可点）。
pub fn panel_shape(hwnd: HWND) -> Option<Rect> {
    chrome_of(hwnd).panel
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

// ---------------------------------------------------------------------------
// 全局外壳状态（窗口过程拿不到实例，见模块文档）
// ---------------------------------------------------------------------------

/// 拖动区的最大段数。
///
/// 为什么需要「多段」：拖动区中间要**挖空**掉交互控件。
/// 早前只支持单一矩形，于是 UI 层只能把整个顶栏报上来 ——
/// 而顶栏里有齿轮、垃圾桶、搜索框，点它们会被系统判成标题栏
/// （`HTCAPTION`）→ 不产生 `WM_LBUTTONDOWN` → egui 收不到点击。
/// 症状是「设置打不开、搜索框点不动」，而 paint 层测试全绿。
///
/// 段数预算：顶栏左/右 2 段 + 共用单栏时内嵌置顶区左/右 2 段 +
/// 历史主栏左/右 2 段 = 6 段。当初只按顶栏估了 4，共用单栏下
/// 列表边距段被截断，表现为「列表左右留白拖不动窗口」。
const MAX_DRAG_SEGS: usize = 8;

/// 一个窗口的边框与拖动区。
///
/// 见模块文档「状态为什么曾经是全局的」——现在按 HWND 存。
#[derive(Clone, Default)]
struct WindowChrome {
    /// 拖动区段（逻辑点）。空 = 无拖动区。
    drag: Vec<Rect>,
    /// resize 边缘宽度（逻辑点）。0 = 不能 resize。
    border: f32,
    /// 窗口特定的最小尺寸（逻辑点）；未设置时沿用通用下限。
    min_size: Option<Vec2>,
    /// 弹窗的**有效内容矩形**（逻辑点、客户区坐标系）。
    ///
    /// `Some(r)` 表示「窗口矩形大于面板本身」——超出的那圈（投影 /
    /// 安全边距）不是可点区域，命中判定返回 [`HitZone::Transparent`]。
    /// `None`（主窗口）= 整块客户区都可点，行为与从前一致。
    panel: Option<Rect>,
}

/// 按 HWND 索引的窗口外壳状态。
///
/// **单线程**访问：`wnd_proc` 与 `set_*` 都在创建窗口的那个线程上跑，
/// 消息泵不会并发进入。用 `Mutex` 只是为了拿到内部可变性。
static CHROME: std::sync::OnceLock<std::sync::Mutex<Vec<(isize, WindowChrome)>>> =
    std::sync::OnceLock::new();

/// 取某窗口的外壳状态（没有就用默认值）。
///
/// NOTE: 窗口在 `WM_NCHITTEST` 时**可能还没登记**（创建早期）。此时返回
/// 默认（无拖动区、无边框）——比沿用别人的设置安全得多：最坏只是
/// 那一瞬间不能拖动/缩放。
fn chrome_of(hwnd: HWND) -> WindowChrome {
    let key = hwnd.0 as isize;
    let lock = CHROME.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let Ok(g) = lock.lock() else {
        return WindowChrome::default();
    };
    g.iter()
        .find(|(h, _)| *h == key)
        .map(|(_, c)| c.clone())
        .unwrap_or_default()
}

/// 写入某窗口的外壳状态。
fn set_chrome_of(hwnd: HWND, update: impl FnOnce(&mut WindowChrome)) {
    let key = hwnd.0 as isize;
    let lock = CHROME.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let Ok(mut g) = lock.lock() else {
        return;
    };
    match g.iter_mut().find(|(h, _)| *h == key) {
        Some((_, c)) => update(c),
        None => {
            let mut c = WindowChrome::default();
            update(&mut c);
            g.push((key, c));
        }
    }
}

/// 窗口销毁时清掉它的条目，否则表会随窗口创建销毁无限增长。
pub fn forget(hwnd: HWND) {
    let key = hwnd.0 as isize;
    if let Some(lock) = CHROME.get()
        && let Ok(mut g) = lock.lock() {
            g.retain(|(h, _)| *h != key);
        }
    // 同步清掉可能残留的拖动状态，避免 HWND 复用时新窗口被误判为拖动中。
    clear_moving(hwnd);
}

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
/// IMPORTANT: **必须传 `hwnd`**：拖动区是**每窗口**的。
///
/// 早前这里不收 `hwnd`（状态在进程级全局里），于是设置窗口会继承
/// 主窗口的拖动区——主窗口顶栏那几段空白的坐标落到设置窗口上，
/// 那里的点击被判成 `HTCAPTION`，系统发 `WM_NCLBUTTONDOWN` 而非
/// `WM_LBUTTONDOWN`，egui 收不到：**设置窗口左上角一整块点不动**。
///
/// 每帧调一次即可：只改内存里的一个小 Vec，没有系统调用。
pub fn set_drag_regions(hwnd: HWND, rects: &[Rect]) {
    let segs: Vec<Rect> = rects
        .iter()
        .take(MAX_DRAG_SEGS)
        .filter(|r| r.width() > 0.0 && r.height() > 0.0)
        // 取整：Win32 命中判定按整数像素，亚像素矩形会漏边。
        .map(|r| {
            Rect::from_min_max(
                Pos2::new(r.min.x.round(), r.min.y.round()),
                Pos2::new(r.max.x.round(), r.max.y.round()),
            )
        })
        .collect();
    set_chrome_of(hwnd, |c| c.drag = segs);
}

/// 设置自绘标题栏（拖动区）的位置，单位逻辑点、客户区坐标系。
///
/// 传空切片关闭拖动区（此时只有边缘能 resize，标题栏区域归 egui）。
///
/// # 上层**必须**把自己的控件排除在拖动区外
///
/// 落在矩形**内**的按下会被系统拿走变成拖动，egui 收不到。
/// 因此关闭 / 最小化 / 设置 / 搜索框这类控件必须**挖空**：
/// 传「标题栏减去控件」的**多段**矩形。
///
/// # 只有单段接口的老坑
///
/// 本函数早期只接受 `Option<Rect>`，于是 UI 层把**整个顶栏**报上来，
/// 点齿轮毫无反应（被系统判成 `HTCAPTION`）。而 paint 层交互测试
/// 全部通过 —— 那条路径要经`WM_NCHITTEST`，测试环境根本不执行。
/// 所以「测试全绿但实机不可点」在这里是**真实发生过**的。
pub fn set_drag_region(hwnd: HWND, rect: Option<Rect>) {
    match rect {
        Some(r) => set_drag_regions(hwnd, std::slice::from_ref(&r)),
        None => set_drag_regions(hwnd, &[]),
    }
}

/// 一次性设置多段拖动区（推荐）。
pub fn set_drag_region_multi(hwnd: HWND, rects: &[Rect]) {
    set_drag_regions(hwnd, rects);
}

/// 当前拖动区的所有段（**指定窗口**）。
fn drag_regions(hwnd: HWND) -> Vec<Rect> {
    chrome_of(hwnd).drag
}

fn resize_border(hwnd: HWND) -> f32 {
    chrome_of(hwnd).border
}

/// 设置边缘 resize 的宽度（逻辑点）。
///
/// 每帧可调（跟随边框宽度设置）。
///
/// NOTE: 同样按 `hwnd` 隔离：`Window::new` 会在建窗时调它，早前因为
/// 是全局量，**新建一个窗口会把主窗口的边框设置一起改掉**。
pub fn set_resize_border(hwnd: HWND, points: f32) {
    set_chrome_of(hwnd, |c| c.border = points.max(0.0));
}

/// 设置窗口缩放时的最小逻辑尺寸；设置页需要足够宽容纳导航与控件。
pub fn set_min_size(hwnd: HWND, width: f32, height: f32) {
    if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 {
        set_chrome_of(hwnd, |c| c.min_size = Some(Vec2::new(width, height)));
    }
}

// ---------------------------------------------------------------------------
// 自定义窗口移动（非模态，让拖动期间 Vulkan 帧循环继续跑）
// ---------------------------------------------------------------------------

/// 正在拖动窗口的位置或尺寸。所有坐标均为物理像素、屏幕坐标。
/// 始终从起始矩形与光标总位移计算，避免丢失移动事件时累积漂移。
#[derive(Clone, Copy)]
struct MoveState {
    start_cursor: POINT,
    start_rect: RECT,
    zone: HitZone,
    min_width: i32,
    min_height: i32,
}

/// 把起始窗口矩形沿选中的边/角拖动，并保证不会缩成不可交互的 0×0。
fn dragged_rect(mut rect: RECT, delta: POINT, zone: HitZone, min_w: i32, min_h: i32) -> RECT {
    let min_w = min_w.max(1);
    let min_h = min_h.max(1);
    match zone {
        HitZone::Caption => {
            rect.left = rect.left.saturating_add(delta.x);
            rect.right = rect.right.saturating_add(delta.x);
            rect.top = rect.top.saturating_add(delta.y);
            rect.bottom = rect.bottom.saturating_add(delta.y);
        }
        _ => {
            if matches!(zone, HitZone::Left | HitZone::TopLeft | HitZone::BottomLeft) {
                rect.left = rect.left.saturating_add(delta.x).min(rect.right - min_w);
            }
            if matches!(zone, HitZone::Right | HitZone::TopRight | HitZone::BottomRight) {
                rect.right = rect.right.saturating_add(delta.x).max(rect.left + min_w);
            }
            if matches!(zone, HitZone::Top | HitZone::TopLeft | HitZone::TopRight) {
                rect.top = rect.top.saturating_add(delta.y).min(rect.bottom - min_h);
            }
            if matches!(zone, HitZone::Bottom | HitZone::BottomLeft | HitZone::BottomRight) {
                rect.bottom = rect.bottom.saturating_add(delta.y).max(rect.top + min_h);
            }
        }
    }
    rect
}

/// Win32 非客户区按下的 HT 值映射为可拖拽区域；其他命中保留系统默认处理。
fn draggable_zone(hit: u32) -> Option<HitZone> {
    Some(match hit {
        HTCAPTION => HitZone::Caption,
        HTLEFT => HitZone::Left,
        HTRIGHT => HitZone::Right,
        HTTOP => HitZone::Top,
        HTBOTTOM => HitZone::Bottom,
        HTTOPLEFT => HitZone::TopLeft,
        HTTOPRIGHT => HitZone::TopRight,
        HTBOTTOMLEFT => HitZone::BottomLeft,
        HTBOTTOMRIGHT => HitZone::BottomRight,
        _ => return None,
    })
}

/// 按 HWND 索引的拖动状态。与 [`CHROME`] 同理，仅在窗口线程访问。
static MOVING: std::sync::OnceLock<std::sync::Mutex<Vec<(isize, MoveState)>>> =
    std::sync::OnceLock::new();

fn moving_table() -> &'static std::sync::Mutex<Vec<(isize, MoveState)>> {
    MOVING.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn moving_state(hwnd: HWND) -> Option<MoveState> {
    let key = hwnd.0 as isize;
    let g = moving_table().lock().ok()?;
    g.iter().find(|(h, _)| *h == key).map(|(_, s)| *s)
}

fn set_moving(hwnd: HWND, state: MoveState) {
    let key = hwnd.0 as isize;
    let Ok(mut g) = moving_table().lock() else {
        return;
    };
    match g.iter_mut().find(|(h, _)| *h == key) {
        Some(slot) => slot.1 = state,
        None => g.push((key, state)),
    }
}

fn clear_moving(hwnd: HWND) {
    let key = hwnd.0 as isize;
    if let Ok(mut g) = moving_table().lock() {
        g.retain(|(h, _)| *h != key);
    }
}

/// 按下标题栏/边/角：接管指针，避免系统模态移动/缩放循环冻结画面。
unsafe fn begin_custom_move(hwnd: HWND, zone: HitZone) {
    unsafe {
        let mut cursor = POINT::default();
        let mut rc = RECT::default();
        if GetCursorPos(&mut cursor).is_err() || GetWindowRect(hwnd, &mut rc).is_err() {
            return;
        }
        let dpi = GetDpiForWindow(hwnd);
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        // 客户区保持可操作；起始尺寸若更小，不要在开始拖动时突然放大。
        let min_size = chrome_of(hwnd).min_size.unwrap_or(Vec2::new(320.0, 240.0));
        let min_width = ((min_size.x * scale).round() as i32).min(rc.right - rc.left).max(1);
        let min_height = ((min_size.y * scale).round() as i32).min(rc.bottom - rc.top).max(1);
        set_moving(hwnd, MoveState {
            start_cursor: cursor,
            start_rect: rc,
            zone,
            min_width,
            min_height,
        });
        // 捕获后移出窗口仍收到鼠标移动和松开消息。
        let _ = SetCapture(hwnd);
    }
}

/// 指针移动：以起始矩形计算位置/尺寸，让常规事件循环继续渲染。
unsafe fn apply_custom_move(hwnd: HWND) {
    unsafe {
        let Some(st) = moving_state(hwnd) else { return };
        let mut cursor = POINT::default();
        if GetCursorPos(&mut cursor).is_err() { return; }
        let rc = dragged_rect(
            st.start_rect,
            POINT {
                x: cursor.x.saturating_sub(st.start_cursor.x),
                y: cursor.y.saturating_sub(st.start_cursor.y),
            },
            st.zone,
            st.min_width,
            st.min_height,
        );
        // Win32 会合并鼠标消息；连续两条落在相同像素时不要反复发
        // WM_WINDOWPOSCHANGED/WM_SIZE，否则每帧可能白白重建交换链。
        let mut current = RECT::default();
        if GetWindowRect(hwnd, &mut current).is_ok()
            && current.left == rc.left && current.top == rc.top
            && current.right == rc.right && current.bottom == rc.bottom
        {
            return;
        }
        let _ = SetWindowPos(
            hwnd,
            None,
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 松开：清状态再释放捕获（ReleaseCapture 会同步触发 CAPTURECHANGED）。
unsafe fn end_custom_move(hwnd: HWND) {
    clear_moving(hwnd);
    unsafe { let _ = ReleaseCapture(); }
}

/// 双击标题栏：最大化 / 还原（保留原生手感）。
unsafe fn toggle_maximize(hwnd: HWND) {
    unsafe {
        let restore = IsZoomed(hwnd).as_bool();
        let _ = ShowWindow(
            hwnd,
            if restore {
                SW_RESTORE
            } else {
                SW_MAXIMIZE
            },
        );
    }
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

/// 把**屏幕坐标**换算成客户区坐标并跑一次完整命中测试。
///
/// 与 `WM_NCHITTEST` 分支用**同一套**判定（客户区尺寸、DPI 换算、
/// `ScreenToClient`、`hit_test`），避免两处逻辑分叉——
/// 一处改了另一处没改，光标指示就会和实际可 resize 的区域对不上。
///
/// 取不到客户区尺寸时返回 `Client`（与 `WM_NCHITTEST` 分支一致：
/// 最坏结果是那一次判定退化为客户区，比误判成边缘安全）。
fn hit_zone_at_screen(hwnd: HWND, pt_screen: POINT) -> HitZone {
    let mut cr = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut cr) }.is_err() {
        return HitZone::Client;
    }
    let scale = {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
    };
    let size = Vec2::new(
        (cr.right - cr.left).max(0) as f32 / scale,
        (cr.bottom - cr.top).max(0) as f32 / scale,
    );
    let mut pt = pt_screen;
    // BOOL 是 must_use；失败时 pt 保持屏幕坐标，判定结果无意义但无害。
    let _ = unsafe { ScreenToClient(hwnd, &mut pt) };
    let pos = Pos2::new(pt.x as f32 / scale, pt.y as f32 / scale);
    // 弹窗：面板之外的透明边距（投影区）一律穿透。
    // 必须在 hit_test **之前**判——边缘 resize / 拖动区在弹窗上都不该
    // 生效（弹窗 resize_border = 0、无拖动区），先判穿透语义才干净。
    if let Some(zone) = popup_zone(pos, panel_shape(hwnd)) {
        return zone;
    }
    hit_test(pos, size, resize_border(hwnd), &drag_regions(hwnd))
}

/// 指针当前是否落在这个窗口上。
///
/// 用 `WindowFromPoint` 判定：多窗口（主窗口 + 拖出的卡片子窗口 +
/// 设置窗口）重叠时，只有**最上层**窗口命中。
///
/// 用途：每帧设置光标前先问一句——指针不在本窗口上时，本窗口的
/// 光标逻辑**不得**碰全局光标。否则主窗口的帧循环会在指针悬停在
/// 子窗口 resize 边缘时把它覆盖回箭头（「resize 无光标指示」的
/// 成因之一）。
pub fn pointer_is_over(hwnd: HWND) -> bool {
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_err() {
        return false;
    }
    unsafe { WindowFromPoint(pt) == hwnd }
}

/// 设置一个系统预定义光标（`LoadCursorW` + `SetCursor`）。
///
/// # Safety
///
/// `res_id` 必须是系统预定义光标资源 ID（`IDC_*`）。
unsafe fn set_system_cursor(res_id: u16) {
    unsafe {
        // SAFETY: IDC_* 是系统预定义资源 ID；返回的句柄由系统持有、
        // 不需要销毁（与 presence.rs 的 set_cursor_icon 同一模式）。
        if let Ok(h) = LoadCursorW(None, PCWSTR(res_id as *const u16)) {
            let _ = SetCursor(Some(h));
        }
    }
}

/// 本窗口此刻应该显示的系统光标资源 ID（供 UI 帧循环每帧查询）。
///
/// 返回 `Some`：指针落在窗口外壳（resize 边/角、拖动区）上，或正在
/// 拖动本窗口——此时光标由外壳决定，UI 层不应再用 egui 的光标覆盖。
/// 返回 `None`：指针在客户区或不在本窗口上，交给 egui/系统。
pub fn shell_cursor_id(hwnd: HWND) -> Option<u16> {
    // 拖动中：指针可能已移出窗口，`WindowFromPoint` 与当前位置判定
    // 都会失效；按拖动开始时命中的区域保持光标，直到松手。
    if let Some(st) = moving_state(hwnd) {
        return st.zone.cursor_id();
    }
    let mut pt = POINT::default();
    if unsafe { GetCursorPos(&mut pt) }.is_err() {
        return None;
    }
    // 指针不在本窗口上：不越权设置（可能是别的窗口的外壳/客户区）。
    if unsafe { WindowFromPoint(pt) } != hwnd {
        return None;
    }
    hit_zone_at_screen(hwnd, pt).cursor_id()
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
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    // ---- 非模态移动与缩放，不进入会阻塞帧循环的系统模态循环 ----
    match msg {
        WM_NCLBUTTONDOWN => {
            if let Some(zone) = draggable_zone(wparam.0 as u32) {
                if unsafe { IsZoomed(hwnd) }.as_bool() {
                    return None;
                }
                unsafe { begin_custom_move(hwnd, zone) };
                return Some(LRESULT(0));
            }
            return None;
        }
        WM_NCLBUTTONDBLCLK => {
            if wparam.0 as u32 == HTCAPTION {
                unsafe { toggle_maximize(hwnd) };
                return Some(LRESULT(0));
            }
            return None;
        }
        WM_NCMOUSEMOVE | WM_MOUSEMOVE_C => {
            if moving_state(hwnd).is_some() {
                unsafe { apply_custom_move(hwnd) };
                return Some(LRESULT(0));
            }
            return None;
        }
        WM_NCLBUTTONUP | WM_LBUTTONUP_C => {
            if moving_state(hwnd).is_some() {
                unsafe { end_custom_move(hwnd) };
                return Some(LRESULT(0));
            }
            return None;
        }
        WM_CAPTURECHANGED_C => {
            // 指针捕获被系统/其他窗口抢走：清状态即可，不要再 ReleaseCapture
            //（捕获已经不是我们的了）。返回 None 让默认处理走完。
            clear_moving(hwnd);
            return None;
        }
        WM_SETCURSOR => {
            // 无边框窗口（WS_POPUP、无 WS_THICKFRAME）上，系统默认
            // 不保证按命中码显示 resize 光标；而且主窗口的帧循环每帧
            // 都会用 egui 的光标（边缘处是 Default）覆盖一次——
            // 两件事叠加就是「resize 时没有任何光标指示」。
            // 这里显式接管：命中 resize 边/角或拖动区时按区域设光标。
            //
            // 拖动中：lParam 里的命中码按当前指针位置算，指针移出
            // 窗口后不可靠；改用拖动开始时记录的区域。
            if let Some(st) = moving_state(hwnd)
                && let Some(code) = st.zone.cursor_id()
            {
                unsafe { set_system_cursor(code) };
                return Some(LRESULT(1)); // TRUE：不再走默认处理
            }
            // WM_SETCURSOR 的 lParam 低 16 位是命中测试码。
            let hit = lparam.0 as u32 & 0xFFFF;
            if let Some(code) = draggable_zone(hit).and_then(HitZone::cursor_id) {
                unsafe { set_system_cursor(code) };
                return Some(LRESULT(1));
            }
            // HTCLIENT 等客户区命中：交给 DefWindowProc / egui 每帧
            // 的光标设置（手型、I-beam 走那条路）。
            return None;
        }
        _ => {}
    }

    if msg != WM_NCHITTEST {
        return None;
    }

    // `WM_NCHITTEST` 的 lParam 是**屏幕**坐标；与
    // [`shell_cursor_id`] 共用同一套命中判定。
    let pt = POINT {
        x: x_lparam(lparam.0),
        y: y_lparam(lparam.0),
    };
    let zone = hit_zone_at_screen(hwnd, pt);
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
        assert_eq!(hit_test(p(200.0, 300.0), SIZE, B, &[]), HitZone::Client);
    }

    #[test]
    fn no_drag_region_means_only_edges_are_special() {
        let drag = Rect::from_min_max(p(0.0, 0.0), p(400.0, 32.0));
        // 拖动区被忽略（传 None）
        assert_eq!(hit_test(p(200.0, 10.0), SIZE, B, &[]), HitZone::Client);
        // 传了才生效
        assert_eq!(
            hit_test(p(200.0, 10.0), SIZE, B, &[drag]),
            HitZone::Caption
        );
    }

    #[test]
    fn four_corners_map_to_four_zones() {
        assert_eq!(hit_test(p(0.0, 0.0), SIZE, B, &[]), HitZone::TopLeft);
        assert_eq!(hit_test(p(399.9, 0.0), SIZE, B, &[]), HitZone::TopRight);
        assert_eq!(hit_test(p(0.0, 599.9), SIZE, B, &[]), HitZone::BottomLeft);
        assert_eq!(
            hit_test(p(399.9, 599.9), SIZE, B, &[]),
            HitZone::BottomRight
        );
    }

    #[test]
    fn four_edges_map_to_four_zones() {
        // 取每条边的中点，避开角
        assert_eq!(hit_test(p(200.0, 1.0), SIZE, B, &[]), HitZone::Top);
        assert_eq!(hit_test(p(200.0, 598.0), SIZE, B, &[]), HitZone::Bottom);
        assert_eq!(hit_test(p(1.0, 300.0), SIZE, B, &[]), HitZone::Left);
        assert_eq!(hit_test(p(398.0, 300.0), SIZE, B, &[]), HitZone::Right);
    }

    /// 边缘优先于标题栏——否则顶部边缘永远拉不动。
    #[test]
    fn edges_take_priority_over_drag_region() {
        let drag = Rect::from_min_max(p(0.0, 0.0), p(400.0, 40.0));
        assert_eq!(hit_test(p(200.0, 2.0), SIZE, B, &[drag]), HitZone::Top);
        // 离开边缘后才落到标题栏
        assert_eq!(
            hit_test(p(200.0, 20.0), SIZE, B, &[drag]),
            HitZone::Caption
        );
    }

    #[test]
    fn drag_region_excludes_everything_outside() {
        let drag = Rect::from_min_max(p(10.0, 4.0), p(390.0, 30.0));
        assert_eq!(hit_test(p(9.0, 10.0), SIZE, B, &[drag]), HitZone::Client);
        assert_eq!(hit_test(p(200.0, 31.0), SIZE, B, &[drag]), HitZone::Client);
        assert_eq!(hit_test(p(200.0, 10.0), SIZE, B, &[drag]), HitZone::Caption);
    }

    #[test]
    fn zero_border_disables_resize() {
        for (x, y) in [(0.0, 0.0), (1.0, 1.0), (399.0, 599.0)] {
            assert_eq!(
                hit_test(p(x, y), SIZE, 0.0, &[]),
                HitZone::Client,
                "border=0 时 ({x},{y}) 不该是边缘"
            );
        }
    }

    #[test]
    fn negative_border_is_treated_as_disabled() {
        assert_eq!(hit_test(p(0.0, 0.0), SIZE, -5.0, &[]), HitZone::Client);
    }

    /// 小窗口：角会重叠，必须钳制而不是让某个分支永远不成立。
    #[test]
    fn tiny_window_still_has_all_four_corners() {
        let size = Vec2::new(8.0, 8.0);
        let b = 6.0; // 大于 size/2
        assert_eq!(hit_test(p(0.0, 0.0), size, b, &[]), HitZone::TopLeft);
        assert_eq!(hit_test(p(7.9, 0.0), size, b, &[]), HitZone::TopRight);
        assert_eq!(hit_test(p(0.0, 7.9), size, b, &[]), HitZone::BottomLeft);
        assert_eq!(hit_test(p(7.9, 7.9), size, b, &[]), HitZone::BottomRight);
    }

    #[test]
    fn degenerate_size_does_not_panic() {
        for size in [Vec2::ZERO, Vec2::new(f32::NAN, 10.0), Vec2::new(-5.0, -5.0)] {
            let _ = hit_test(p(0.0, 0.0), size, B, &[]);
        }
    }

    #[test]
    fn nan_border_does_not_panic() {
        let _ = hit_test(p(1.0, 1.0), SIZE, f32::NAN, &[]);
    }

    /// 指针在窗口外一律 Client：否则鼠标移出去时窗口仍被"抓住"。
    #[test]
    fn outside_is_always_client() {
        let drag = Rect::from_min_max(p(-100.0, -100.0), p(900.0, 900.0));
        for pos in [p(-1.0, 10.0), p(10.0, -1.0), p(401.0, 10.0), p(10.0, 601.0)] {
            assert_eq!(
                hit_test(pos, SIZE, B, &[drag]),
                HitZone::Client,
                "{pos:?} 在窗口外却不是 Client"
            );
        }
    }

    #[test]
    fn zero_area_drag_region_is_ignored() {
        let empty = Rect::from_min_max(p(10.0, 10.0), p(10.0, 10.0));
        assert_eq!(hit_test(p(10.0, 10.0), SIZE, B, &[empty]), HitZone::Client);
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
    fn drag_zone_round_trip_and_unhandled_hit() {
        for zone in [HitZone::Caption, HitZone::Top, HitZone::Bottom, HitZone::Left,
            HitZone::Right, HitZone::TopLeft, HitZone::TopRight,
            HitZone::BottomLeft, HitZone::BottomRight] {
            assert_eq!(draggable_zone(zone.hit_code()), Some(zone));
        }
        assert_eq!(draggable_zone(HTCLIENT), None);
    }

    #[test]
    fn resize_all_edges_and_corners() {
        let start = RECT { left: 100, top: 200, right: 500, bottom: 600 };
        let delta = POINT { x: 30, y: -40 };
        for (zone, expected) in [
            (HitZone::Left, (130, 200, 500, 600)),
            (HitZone::Right, (100, 200, 530, 600)),
            (HitZone::Top, (100, 160, 500, 600)),
            (HitZone::Bottom, (100, 200, 500, 560)),
            (HitZone::TopLeft, (130, 160, 500, 600)),
            (HitZone::TopRight, (100, 160, 530, 600)),
            (HitZone::BottomLeft, (130, 200, 500, 560)),
            (HitZone::BottomRight, (100, 200, 530, 560)),
            (HitZone::Caption, (130, 160, 530, 560)),
        ] {
            let r = dragged_rect(start, delta, zone, 320, 240);
            assert_eq!((r.left, r.top, r.right, r.bottom), expected, "{zone:?}");
        }
    }

    #[test]
    fn resizing_clamps_minimum_and_preserves_opposite_edge() {
        let start = RECT { left: -200, top: 50, right: 200, bottom: 450 };
        let r = dragged_rect(start, POINT { x: 900, y: 900 }, HitZone::TopLeft, 320, 240);
        assert_eq!((r.left, r.top, r.right, r.bottom), (-120, 210, 200, 450));
        let r = dragged_rect(start, POINT { x: -900, y: -900 }, HitZone::BottomRight, 320, 240);
        assert_eq!((r.left, r.top, r.right, r.bottom), (-200, 50, 120, 290));
        // 当窗口原本就比最小尺寸小，不应在按下时瞬间跳到阈值。
        let small = RECT { left: 0, top: 0, right: 200, bottom: 120 };
        let r = dragged_rect(small, POINT { x: 50, y: 50 }, HitZone::Caption, 320, 240);
        assert_eq!((r.left, r.top, r.right, r.bottom), (50, 50, 250, 170));
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

    /// resize 边/角必须映射到对应的系统光标资源 ID，客户区返回 None。
    ///
    /// 期望值硬编码（与 `hit_codes_match_win32_constants` 同一策略）：
    /// IDC_SIZENWSE=32642, IDC_SIZENESW=32643, IDC_SIZEWE=32644,
    /// IDC_SIZENS=32645, IDC_ARROW=32512。
    #[test]
    fn zones_map_to_cursor_ids() {
        assert_eq!(HitZone::Left.cursor_id(), Some(32644));
        assert_eq!(HitZone::Right.cursor_id(), Some(32644));
        assert_eq!(HitZone::Top.cursor_id(), Some(32645));
        assert_eq!(HitZone::Bottom.cursor_id(), Some(32645));
        assert_eq!(HitZone::TopLeft.cursor_id(), Some(32642));
        assert_eq!(HitZone::BottomRight.cursor_id(), Some(32642));
        assert_eq!(HitZone::TopRight.cursor_id(), Some(32643));
        assert_eq!(HitZone::BottomLeft.cursor_id(), Some(32643));
        // 拖动区与原生标题栏一致：箭头。
        assert_eq!(HitZone::Caption.cursor_id(), Some(32512));
        // 客户区由 egui 决定，外壳不接管。
        assert_eq!(HitZone::Client.cursor_id(), None);
    }

    /// `draggable_zone` 与 `cursor_id` 组合后，每条非客户区命中码
    /// 都能给出光标；HTCLIENT 不行（客户区不归外壳管）。
    #[test]
    fn every_draggable_hit_code_has_a_cursor() {
        for zone in [
            HitZone::Caption,
            HitZone::Top,
            HitZone::Bottom,
            HitZone::Left,
            HitZone::Right,
            HitZone::TopLeft,
            HitZone::TopRight,
            HitZone::BottomLeft,
            HitZone::BottomRight,
        ] {
            assert!(
                draggable_zone(zone.hit_code())
                    .and_then(HitZone::cursor_id)
                    .is_some(),
                "{zone:?} 的命中码必须能给出光标"
            );
        }
        assert_eq!(draggable_zone(HTCLIENT), None);
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
        let lp = (0x8000u32 << 16) as isize;
        assert_eq!(y_lparam(lp), -32768);
    }

    // ---- per-HWND 状态 ----

    /// 测试用的假句柄。
    ///
    /// `hit_test` 不碰 Win32，只需要一个能区分身份的值。用明显不像
    /// 真句柄的数（0xA 起），即便哪天误传给 Win32 也容易看出来。
    ///
    /// IMPORTANT: **每条测试必须用自己专属的句柄**。
    ///
    /// `CHROME` 是**进程级**全局表（`OnceLock<Mutex<Vec<…>>>`）——
    /// per-HWND 指的是「按 HWND 索引」，不是「每窗一份存储」。
    /// cargo 默认**并行**跑测试，两条测试若共用同一个假句柄，就会
    /// 互相看见对方的写入：实测 `multi_drag_regions_round_trip` 在
    /// `drag_regions_are_isolated_per_window` 的 `forget(A)` 落地时
    /// 读到空 vec，panic 于 `index out of bounds: len is 0`。
    /// 那看起来是「测试偶发不稳」，真因是测试之间串了状态。
    const TEST_HWND_A: HWND = HWND(0xA11 as *mut std::ffi::c_void);
    const TEST_HWND_B: HWND = HWND(0xB22 as *mut std::ffi::c_void);
    const TEST_HWND_C: HWND = HWND(0xC33 as *mut std::ffi::c_void);
    // 以下三条各自的专用句柄，见上面的说明。
    const HWND_ROUND_TRIP: HWND = HWND(0xD44 as *mut std::ffi::c_void);
    const HWND_MULTI: HWND = HWND(0xE55 as *mut std::ffi::c_void);
    const HWND_BORDER: HWND = HWND(0xF66 as *mut std::ffi::c_void);

    #[test]
    fn drag_region_round_trips() {
        let r = Rect::from_min_max(p(0.0, 0.0), p(400.0, 32.0));
        set_drag_region(HWND_ROUND_TRIP, Some(r));
        assert_eq!(drag_regions(HWND_ROUND_TRIP), vec![r]);
        set_drag_region(HWND_ROUND_TRIP, None);
        assert!(
            drag_regions(HWND_ROUND_TRIP).is_empty(),
            "None 必须真的禁用拖动区"
        );
    }

    /// **窗口之间不得串味**——这是改成 per-HWND 要解决的那个 bug。
    ///
    /// 早前拖动区是进程级全局：设置窗口会继承主窗口的拖动区，
    /// 于是它左上角一整块被判成 `HTCAPTION`，点击变成拖窗、
    /// 控件点不动。这条守住「A 窗的设置不影响 B 窗」。
    /// 弹窗：面板**之外**的点必须穿透，面板之内照常走常规判定。
///
/// 这条守的是「悬浮面板的投影区吃掉底下窗口的点击」：弹窗客户区比
/// 面板大一圈投影边距，而命中判定按**窗口矩形**而非像素 alpha。
#[test]
fn popup_panel_outside_is_click_through() {
    let panel = Rect::from_min_size(Pos2::new(12.0, 12.0), Vec2::new(100.0, 40.0));
    // 面板内 → 交回常规判定（None）。
    assert_eq!(popup_zone(Pos2::new(60.0, 30.0), Some(panel)), None);
    // 四条边距（投影区）→ 穿透。
    assert_eq!(
        popup_zone(Pos2::new(5.0, 30.0), Some(panel)),
        Some(HitZone::Transparent),
        "左侧投影区应穿透"
    );
    assert_eq!(
        popup_zone(Pos2::new(60.0, 5.0), Some(panel)),
        Some(HitZone::Transparent),
        "上侧投影区应穿透"
    );
    assert_eq!(
        popup_zone(Pos2::new(118.0, 30.0), Some(panel)),
        Some(HitZone::Transparent),
        "右侧投影区应穿透"
    );
    assert_eq!(
        popup_zone(Pos2::new(60.0, 58.0), Some(panel)),
        Some(HitZone::Transparent),
        "下侧投影区应穿透"
    );
    // 未登记面板形状（主窗口）→ 永远交回常规判定。
    assert_eq!(popup_zone(Pos2::new(0.0, 0.0), None), None);
    // 穿透区对应 HTTRANSPARENT（点击落到下面的窗口）。
    assert_eq!(HitZone::Transparent.hit_code(), HTTRANSPARENT as u32);
    // 穿透区不设光标：那里的光标归下面的窗口管。
    assert_eq!(HitZone::Transparent.cursor_id(), None);
}

    #[test]
    fn drag_regions_are_isolated_per_window() {
        let r = Rect::from_min_max(p(0.0, 0.0), p(400.0, 32.0));
        set_drag_regions(TEST_HWND_A, &[r]);
        assert_eq!(drag_regions(TEST_HWND_A).len(), 1, "A 窗应有 1 段");
        assert!(
            drag_regions(TEST_HWND_B).is_empty(),
            "B 窗绝不能继承 A 窗的拖动区——那会让 B 的控件点不动"
        );

        // 边框同理：改 A 不能动 B。
        set_resize_border(TEST_HWND_A, 6.0);
        set_resize_border(TEST_HWND_B, 2.0);
        assert_eq!(resize_border(TEST_HWND_A), 6.0, "A 的边框不应被 B 改掉");
        assert_eq!(resize_border(TEST_HWND_B), 2.0, "B 的边框不应被 A 改掉");

        // 未登记过的窗口必须拿到**默认值**，而不是别人的设置。
        assert!(
            drag_regions(TEST_HWND_C).is_empty(),
            "未登记窗口必须无拖动区（安全默认）"
        );
        assert_eq!(resize_border(TEST_HWND_C), 0.0, "未登记窗口默认无边框");

        // forget 之后彻底清掉。
        forget(TEST_HWND_A);
        assert!(
            drag_regions(TEST_HWND_A).is_empty(),
            "forget 之后必须彻底清掉，否则 HWND 复用时新窗会捡到旧设置"
        );
    }

    /// **多段**拖动区要能原样往返。
    ///
    /// 单一矩形的接口撑不住「控件从中挖空」的需求 ——
    /// 顶栏里的齿轮/垃圾桶/搜索框必须排除在外，否则实机点不动。
    #[test]
    fn multi_drag_regions_round_trip() {
        let segs = [
            Rect::from_min_max(p(0.0, 0.0), p(30.0, 32.0)),
            Rect::from_min_max(p(80.0, 0.0), p(110.0, 32.0)),
            Rect::from_min_max(p(160.0, 0.0), p(200.0, 32.0)),
        ];
        set_drag_regions(HWND_MULTI, &segs);
        assert_eq!(drag_regions(HWND_MULTI).len(), 3, "三段都应保留");
        for (i, s) in segs.iter().enumerate() {
            assert_eq!(drag_regions(HWND_MULTI)[i], *s, "第 {i} 段应原样往返");
        }
        set_drag_regions(HWND_MULTI, &[]);
        assert!(drag_regions(HWND_MULTI).is_empty());
    }

    /// 拖动区里的点判`Caption`，区外的点判 `Client`。
    ///
    /// 这是「控件能否被点到」的**真正判据**：`Caption` 会让系统
    /// 发 `WM_NCLBUTTONDOWN` 而不产生 `WM_LBUTTONDOWN`，egui 收不到点击。
    #[test]
    fn hit_zone_differs_inside_and_outside_drag() {
        let segs = [
            Rect::from_min_max(p(0.0, 0.0), p(30.0, 32.0)),
            Rect::from_min_max(p(160.0, 0.0), p(200.0, 32.0)),
        ];
        // 段内 → Caption（可拖）
        assert_eq!(hit_test(p(15.0, 16.0), SIZE, 0.0, &segs), HitZone::Caption);
        assert_eq!(hit_test(p(180.0, 16.0), SIZE, 0.0, &segs), HitZone::Caption);
        // 段间（模拟齿轮/垃圾桶所在处）→ Client（可点）
        assert_eq!(
            hit_test(p(100.0, 16.0), SIZE, 0.0, &segs),
            HitZone::Client,
            "两段之间的控件区必须判 Client，否则实机点不动"
        );
    }

    #[test]
    fn min_size_is_per_window_and_rejects_invalid_values() {
        const A: HWND = HWND(0x1234 as *mut std::ffi::c_void);
        const B: HWND = HWND(0x5678 as *mut std::ffi::c_void);
        set_min_size(A, 720.0, 400.0);
        assert_eq!(chrome_of(A).min_size, Some(Vec2::new(720.0, 400.0)));
        assert_eq!(chrome_of(B).min_size, None);
        set_min_size(A, f32::NAN, -1.0);
        assert_eq!(chrome_of(A).min_size, Some(Vec2::new(720.0, 400.0)));
        forget(A);
        assert_eq!(chrome_of(A).min_size, None);
    }

    #[test]
    fn resize_border_round_trips() {
        set_resize_border(HWND_BORDER, 9.5);
        assert_eq!(resize_border(HWND_BORDER), 9.5);
        set_resize_border(HWND_BORDER, DEFAULT_RESIZE_BORDER);
        assert_eq!(resize_border(HWND_BORDER), DEFAULT_RESIZE_BORDER);
        // 负值禁用（钳到 0），不该出现负边框。
        set_resize_border(HWND_BORDER, -3.0);
        assert_eq!(resize_border(HWND_BORDER), 0.0, "负边框应钳到 0");
    }
}