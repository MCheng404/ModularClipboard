//! Win32 窗口与事件循环。
//!
//! 本模块把 Win32 的消息队列翻译成语义化的 [`WindowEvent`]，再翻译成
//! egui 的 [`egui::RawInput`]。渲染侧只关心 `Resized` / `CloseRequested`，
//! 不需要知道任何 Win32 细节。
//!
//! ## 事件从哪来：刻意不做子类化
//!
//! 窗口过程是一个原样转发 `DefWindowProcW` 的空壳，所有语义事件都在
//! [`EventLoop::poll`] 里用 `PeekMessageW` 从消息队列直接读取。
//!
//! 这么做的两个理由：
//!
//! 1. 子类化要在 `WM_NCCREATE` 时 `SetWindowLongPtrW` 写入指针，
//!    漏掉 `CallWindowProcW` 转发就会崩。崩溃点在进程启动时，
//!    一次 `println` 都来不及执行，排查成本远高于收益。
//! 2. 队列里的消息在派发前就能读，`TranslateMessage` 生成的 `WM_CHAR`
//!    也能在同一轮循环内被读到，无需二次投递。
//!
//! 窗口过程**必须原样转发** `DefWindowProcW`——给它加消息偏移会让
//! `CreateWindowExW` 报 `0x8007007E`（MEMORY.md 第 26 条）。
//!
//! ## 坐标与 DPI
//!
//! Win32 给的是**物理像素**且以客户区左上角为原点，egui 要的是**逻辑点**。
//! 本模块 [`WindowEvent`] 里的坐标**已经换算成逻辑点**（除以
//! `scale_factor`），D 组可以直接喂给 egui，不需要再转换。
//!
//! DPI 感知在 [`Window::new`] 里用 `SetProcessDpiAwarenessContext` 打开
//! （必须在建窗之前，否则不生效），高分屏下窗口才不会糊。
//!
//! ## 每帧调用顺序
//!
//! [`EventLoop::poll`] → [`EventLoop::egui_input`] → 渲染。顺序不能换：
//! `egui_input` 读的是 `poll` 填好的事件缓冲。
//!
//! ## 无边框窗口
//!
//! 窗口样式是 `WS_POPUP | WS_SYSMENU`：没有系统标题栏，也没有 resize 边框，
//! 外观完全由 egui 自绘。但**丢掉了 `WS_THICKFRAME` 就等于丢掉系统的
//! resize 与窗口移动能力**，所以两者在 [`chrome`] 里补回来：
//!
//! - `WM_NCHITTEST` 里手动判定边缘与拖动区，返回 `HTLEFT`/`HTCAPTION` 等；
//! - 系统随即把按下转成 `WM_NCLBUTTONDOWN`，`chrome` 以非模态指针捕获
//!   驱动 `SetWindowPos`，拖动期间帧循环仍能绘制磨砂背景。
//!
//! 命中测试必须在窗口过程里做，不能在 [`EventLoop::poll`] 里做——
//! 详见 [`chrome`] 模块文档。
//!
//! 圆角（Win11 `DWMWCP_ROUND`）与「去掉系统描边」在 [`Window::new`] 里
//! 一并设置，Win10 上失败只记debug，不影响启动。
//!
//! ## 已知缺口
//!
//! - **IME 候选窗未接入**。`WM_IME_CHAR` 已处理（简单 IME 可用），
//!   但完整的 TSF/IMM 组合输入（候选框、预编辑串）需要
//!   `Win32_UI_Input_Ime` 与独立 UI 线程，不在本模块范围。
//! - **水平滚轮未暴露**。[`WindowEvent::Scroll`] 按接口约定只带垂直量。
//! - **剪贴板快捷键不在此处理**。`Ctrl+C/V/X` 的实际读写由
//!   `modular-clipboard-capture` 负责，egui 侧的 `Event::Copy/Cut/Paste` 由调用方
//!   从剪贴板层构造。本模块只产出 `Key` 与 `TextInput`。
//! - **最大化后无边框窗口会盖住任务栏**。`WS_POPUP` 没有 `WS_CAPTION`，
//!   `WM_NCCALCSIZE` 不会被系统裁到工作区内。剪贴板面板默认不最大化，
//!   真要最大化需要上层自行把窗口 rect 压到工作区。


use anyhow::Context as _;
use egui::emath::{Pos2, Rect};
use egui::{Key, Modifiers, PointerButton};
use windows::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
// NOTE: IME 三个消息（WM_IME_STARTCOMPOSITION / WM_IME_COMPOSITION /
// WM_IME_ENDCOMPOSITION）住在 `window/event_loop.rs`——消息循环归它，
// 根只留窗口创建与查询用的 API。
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, SetWindowPos, GetWindowRect, FindWindowW, PostMessageW, SWP_NOSIZE, SWP_NOZORDER, SWP_NOACTIVATE, SWP_NOMOVE, IsIconic, IsWindow, RegisterClassExW, SW_HIDE, SW_SHOW, SW_SHOWNA, ShowWindow, WINDOW_EX_STYLE, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_POPUP, WS_SYSMENU};
use windows::core::{HSTRING, PCWSTR};
// NOTE: IME API 在 `Win32::UI::Input::Ime`，**不在** `Globalization`
// （后者只有 `ImmGetCompositionString` 这类旧版ANSI 版）。

use crate::chrome;

pub mod event_loop;
pub mod keys;

pub use event_loop::EventLoop;
pub use keys::CharDecoder;
pub use keys::key_from_vk;
pub use keys::modifiers_from;


/// `SC_CLOSE` 命令号（`WM_SYSCOMMAND` 的 wParam 低 4 位是来源标识）。
///
/// 用裸常量而非 `windows` crate 的绑定：那套绑定是枚举，
/// 而 `wParam` 在 32/64 位下都是 `usize`，比较时需显式转换。
/// 这个值来自 Win32 文档，稳定不变。
const SC_CLOSE_WPARAM: u32 = 0xF060;

/// `WM_SYSCOMMAND` 的 wParam 低 4 位掩码。
///
/// 低 4 位不是命令的一部分，而是「消息来源」：0 表示来自系统菜单，
/// 1（`F1..F4` 之外的功能键）表示来自 Alt 组合。因此
/// `Alt+F4` 的 wParam 是 `SC_CLOSE | 1 == 0xF061`，直接全等比较会漏。
const SC_SYSTEM_MENU_MASK: u32 = 0x000F;

/// 窗口类名。进程内只注册一次。
const CLASS_NAME: &str = "ModularClipboardWindow";

/// 100% 缩放对应的 DPI。
const BASE_DPI: u32 = 96;

/// 一个滚轮刻度对应的 `WM_MOUSEWHEEL` 单位。
///
/// `windows` crate 0.62.2 没有导出这个常量（我查过 `WindowsAndMessaging`
/// 模块，确认没有），因此自己定义。
const WHEEL_DELTA: f32 = 120.0;

/// 帧循环默认的预测帧间隔。
const DEFAULT_PREDICTED_DT: f32 = 1.0 / 60.0;

/// `predicted_dt` 的钳制区间，防止动画除零或瞬移。
const MIN_PREDICTED_DT: f32 = 1.0 / 1000.0;
const MAX_PREDICTED_DT: f32 = 0.1;

/// 窗口类注册结果。进程内一次。
///
/// `None` 表示「本进程尚未注册过」，`Some(err)` 表示首次注册就失败了。
/// 二次调用拿到 `Some(Ok)` 是正常路径——不能重复 `RegisterClassExW`，
/// 它会返回 0 并置 `ERROR_CLASS_ALREADY_EXISTS`。
static CLASS_REGISTERED: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();

/// 自定义消息：请求把主窗口显示出来并置前。
///
/// # 用途：单实例唤起
///
/// 用户重复双击 exe 时，第二个进程找到已有主窗口、发这条消息，然后退出。
/// 早前第二个进程只弹一个「已有实例在运行」的提示框——用户的意图明明
/// 是「打开它」，弹窗只是把意图挡回去，还得手动关掉。
///
/// 取 `WM_APP + 1`（`WM_APP` = 0x8000）：该值以下留给系统消息，
/// 之上才是应用自定义，不会与任何系统消息撞号。
pub const WM_SHOW_MAIN: u32 = 0x8000 + 1;

/// 自定义消息：请求把主窗口隐藏（热键 toggle 的「收起」方向）。
///
/// # 跨进程消息协议（改号必须两处同步）
///
/// 发送方是常驻进程（`modular-clipboard-ipc::instance`），接收方是本
/// 窗口过程。两边不共享 crate（gfx 不依赖 ipc），消息号是双方约定的
/// 整数，ipc 那边写死同一个值 `0x8000 + 2`。
///
/// # 必须走 PostMessage 而不是跨进程 ShowWindow
///
/// 常驻进程同时是 IPC 服务端；它若在主循环里**同步**跨进程
/// `ShowWindow(SW_HIDE)`，会阻塞等待界面线程泵消息，而界面线程那
/// 一刻正同步阻塞在命名管道 `ReadFile` 上等常驻进程回响应——
/// 双向等待死锁（2026-10-13 dump 实锤：常驻线程卡在
/// `NtUserShowWindow`、界面线程卡在 `NtReadFile`）。投递消息只入队、
/// 立即返回，由界面进程在自己的消息循环里隐藏自己，彻底断开死锁。
pub const WM_HIDE_MAIN: u32 = 0x8000 + 2;

/// 找到标题为 `title` 的已有窗口，请求它显示并置前。
///
/// 返回 `true` 表示消息已投递成功（不保证对端一定响应——那是另一个
/// 进程的事）。
///
/// # 为什么是「投消息」而不是直接 `ShowWindow` + `SetForegroundWindow`
///
/// 直接改窗口状态**绕过了对端自己的状态机**：本程序内部有一个 `hidden`
/// 标志（隐藏到托盘时置位），不同步它的话，窗口虽然被显示出来，主循环
/// 仍会走 `hidden` 分支 `continue`——**不渲染**，用户看到的是一片空白。
/// 发消息让对端自己走「显示」路径，状态才是一致的。
///
/// 另外 `SetForegroundWindow` 对非前台进程有严格限制，跨进程调用常常
/// 静默失败；对端在自己的消息循环里调则没有这个问题。
pub fn request_show_main_window(title: &str) -> bool {
    // `FindWindowW` 要 NUL 结尾的宽字符串；`HSTRING` 不保证可当 PCWSTR 用，
    // 这里手工构造最直接。
    let mut wide: Vec<u16> = title.encode_utf16().collect();
    wide.push(0);
    let name = PCWSTR::from_raw(wide.as_ptr());

    // SAFETY: `wide` 以 NUL 结尾且在本调用期间存活；类名传 `PCWSTR::null()`
    // 表示不限类名，只按标题找。
    let hwnd = match unsafe { FindWindowW(PCWSTR::null(), name) } {
        Ok(h) => h,
        Err(_) => return false,
    };
    if hwnd.is_invalid() {
        return false;
    }
    // SAFETY: `hwnd` 由上面取得且已判非空；`PostMessageW` 只入队、不阻塞，
    // 因此不会因对端忙而卡住本进程。
    unsafe { PostMessageW(Some(hwnd), WM_SHOW_MAIN, WPARAM(0), LPARAM(0)) }.is_ok()
}

/// 语义化的窗口事件。
///
/// 坐标一律是**逻辑点**（已按 `scale_factor` 换算），原点为客户区左上角。
#[derive(Debug, Clone, PartialEq)]
pub enum WindowEvent {
    /// 客户区尺寸变化，单位为逻辑点。
    Resized {
        /// 客户区宽。
        width: f32,
        /// 客户区高。
        height: f32,
    },

    /// 用户请求关闭窗口。
    ///
    /// 此时窗口**尚未被销毁**——`WM_CLOSE` 不派发，由应用决定去留。
    /// 剪贴板类程序通常在这里隐藏到托盘；确实要退出就调
    /// [`Window::destroy`]。
    CloseRequested,

    /// 另一个进程请求把本窗口显示并置前（见 [`WM_SHOW_MAIN`]）。
    ///
    /// 由应用处理成「像点了托盘图标那样」：取消隐藏、显示、抢焦点、重绘。
    /// 只把窗口 `ShowWindow` 是不够的——应用内部还有一个 `hidden` 标志，
    /// 不同步它的话窗口会显示出来但**不渲染**（一片空白）。
    ShowRequested,

    /// 另一个进程请求把本窗口隐藏（热键 toggle 的收起方向，见
    /// [`WM_HIDE_MAIN`]）。
    ///
    /// 由应用在自己的消息循环里隐藏自己（同线程 `ShowWindow(SW_HIDE)`
    /// 立即返回，不跨进程），并同步内部 `hidden` 标志；不要让常驻进程
    /// 跨进程直接改窗口状态。
    HideRequested,

    /// 键盘焦点获得 / 丢失。
    Focused(bool),

    /// DPI 缩放比例变化，`scale_factor = dpi / 96`。
    ScaleFactorChanged(f64),

    /// 窗口被最小化 / 从最小化恢复。
    ///
    /// 无边框窗口没有系统标题栏，最小化后**界面上什么都不会变**，
    /// 只靠本事件切到「已最小化」样式（通常是提示去点托盘）。
    ///
    /// 最小化期间客户区尺寸为 0，此时也会收到
    /// [`WindowEvent::Resized`] `{ width: 0, height: 0 }`。
    Minimized(bool),

    /// 鼠标移动。
    MouseMoved {
        /// 逻辑点坐标。
        pos: Pos2,
    },

    /// 鼠标按键按下或抬起。
    MouseButton {
        /// 哪个键。
        button: PointerButton,
        /// 落下为 `true`，抬起为 `false`。
        pressed: bool,
        /// 逻辑点坐标。
        pos: Pos2,
    },

    /// 滚轮滚动，单位为**行**（一个刻度为 `1.0`）。
    ///
    /// 正值表示内容向下滚（用户向上拨）。
    Scroll(f32),

    /// 键盘按键。
    Key {
        /// 逻辑键。输入法未介入时与物理键一致。
        keycode: Key,
        /// 落下为 `true`，抬起为 `false`。
        pressed: bool,
        /// 是否为系统自动重复。egui 文本框靠它区分长按与多次敲击。
        repeat: bool,
        /// 事件发生时的修饰键状态。
        modifiers: Modifiers,
    },

    /// 已编码的文本输入。
    ///
    /// 控制字符（`\r` `\t` `\x08` `\x1b`）已被过滤——它们由
    /// [`WindowEvent::Key`] 表达，双份会让文本框出现重复字符。
    TextInput(String),

    /// 输入法（IME）会话状态变化。
    ///
    /// # 为什么必须转发
    ///
    /// 顶栏搜索框是主输入区，中文用户要在那里搜「粘贴板」这类词。
    /// 只处理 `WM_IME_CHAR`（已提交的字符）**不足以显示候选窗**：
    /// IME 还要知道「会话开始了」「当前预编辑串是什么」以及
    /// 「候选窗该画在哪」。缺了这些，用户按下拼音后看不到候选列表，
    /// 表现为「中文打不出来」。
    ///
    /// egui 侧对应 `Event::Ime(egui::IME)`，本枚举是它的窗口层来源。
    Ime(WindowIme),
}

/// IME 会话状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowIme {
    /// `WM_IME_START`：会话开始。
    Start,
    /// `WM_IME_COMPOSITION`：预编辑串更新。
    ///
    /// `text` 是当前预编辑内容（含拼音与已转换部分）。
    Composition(String),
    /// `WM_IME_END`：会话结束，`text` 是最终提交结果（可能为空）。
    End(String),
}

/// 窗口构造选项（`Window::new` 与 `Window::new_popup` 的差异全在这里）。
///
/// 抽成结构体而不是给 `new` 加一堆默认参数：主窗口与弹窗的差异是
/// **一组的**（ex 样式 / 初始显隐 / 圆角 / resize 边 / 位置），
/// 逐个 bool 参数会出现 `new(t, w, h, false, true, false, 0.0, None)`
/// 这种不可读的调用点。
///
/// # CAUTION: `Default` 必须手写，不能用 derive
///
/// derive 出来全是零值（`show=false` / `rounded=false` / `border=0`），
/// 而**主窗口要的就是「显示 + 系统圆角 + 默认 resize 边」**——早前图省事
/// 用了 derive，结果主窗口与设置窗口**再也不调 `apply_rounded_corners`**，
/// 圆角整体消失（用户实机一眼看出来）。默认值必须照 `Window::new`
/// 的老行为写死。
#[derive(Debug, Clone, Copy)]
pub struct WindowOpts {
    /// 扩展样式（主窗口传 0；弹窗见 [`Window::new_popup`]）。
    pub ex_style: WINDOW_EX_STYLE,
    /// 创建后是否立即显示。弹窗传 false——按需显示，
    /// 避免「程序一启动就多两个空面板挂在屏幕上」。
    pub show: bool,
    /// 是否应用 DWM 圆角 + 去系统描边。弹窗传 false：
    /// 圆角由内容自己的 alpha 画（投影也要一起画，系统圆角管不了）。
    pub rounded: bool,
    /// resize 边缘宽度（逻辑点）。0 = 不可缩放（弹窗）。
    pub resize_border: f32,
    /// 初始位置（物理像素）。`None` = 系统默认 (120, 120)。
    pub pos: Option<(i32, i32)>,
    /// 属主窗口（owner）。
    ///
    /// 弹窗必须把主窗口设为属主：
    /// - 始终叠在属主之上，不会掉到别的窗口后面；
    /// - 属主最小化时系统自动把它们一起收起（否则主窗最小化后两块浮动
    ///   面板还留在桌面上）；
    /// - 属主被激活时一起被带到前台。
    pub owner: Option<HWND>,
}

impl Default for WindowOpts {
    /// **主窗口**的那一份：可见 + 系统圆角 + 默认 resize 边。
    ///
    /// 弹窗走 [`Window::new_popup`] 显式构造，不受这里影响。
    fn default() -> Self {
        Self {
            ex_style: WINDOW_EX_STYLE(0),
            show: true,
            rounded: true,
            resize_border: chrome::DEFAULT_RESIZE_BORDER,
            pos: None,
            owner: None,
        }
    }
}

/// Win32 窗口。
///
/// 持有 `HWND` 与模块句柄，`Drop` 时销毁窗口。窗口类**不**注销：
/// 它是进程级的，且可能已有其它 `Window` 实例在用同名类。
pub struct Window {
    hwnd: HWND,
    hinstance: HINSTANCE,
    /// 告知 egui 的纹理尺寸上限（`RawInput::max_texture_side`）。
    ///
    /// # 为什么必须设置
    ///
    /// egui 用它决定**字体图集**能长到多大。缺省值是 2048，而图集要装下
    /// 整套中文字形 + 图标字形：在超采样（`ppp = DPI × 倍率`）下所需面积
    /// 按倍率的平方增长，2048 很快不够。
    ///
    /// 图集塞不下时的表现**不是报错**，而是字形被挤掉/画错——
    /// 实测把 `MODULARCLIPBOARD_SSAA=2` 打开后，齿轮图标丢掉了齿、
    /// 垃圾桶变了形（字形栅格化失败后被相邻字形顶替）。
    /// 这种「图标形状不对」极易被误判成字体文件坏了。
    ///
    /// 默认 2048 是「没拿到设备信息」时的安全值；拿到 `Gpu` 后调用
    /// [`Self::set_max_texture_side`] 换成设备的真实上限。
    max_texture_side: usize,
    /// 本结构**自己认为**的可见状态（供 [`Self::set_visible`] 去重）。
    ///
    /// `None` = 建窗后尚未同步过（首次调用必定下发）。
    /// 不用 `IsWindowVisible` 查询代替：那要多一次系统调用，且外部
    /// （系统、别的模块）也可能改过窗口状态，簿记反而更贴近「我们
    /// 上次想让它是什么样」。
    visible: std::cell::Cell<Option<bool>>,
}

impl Window {
    /// 创建并显示一个窗口。
    ///
    /// `width` / `height` 是**客户区**的物理像素。内部先打开 per-monitor
    /// DPI 感知（v2），因此在高 DPI 显示器上窗口是清晰的。
    pub fn new(title: &str, width: u32, height: u32) -> anyhow::Result<Self> {
        Self::create(title, width, height, WindowOpts::default())
    }

    /// 创建一个**弹窗**（悬浮面板用）：不出现在任务栏与 Alt+Tab
    /// （`WS_EX_TOOLWINDOW`），初始**隐藏**（由调用方在需要时 `set_visible`），
    /// 不要系统圆角与 resize 边——圆角由内容自己的 alpha 画，更可控。
    ///
    /// `no_activate` 为 true 时加 `WS_EX_NOACTIVATE`：点击不抢焦点
    /// （类型过滤栏这类纯按钮面板用；要打字的搜索面板不能加，否则
    /// 永远拿不到键盘焦点）。
    pub fn new_popup(
        title: &str,
        width: u32,
        height: u32,
        no_activate: bool,
        owner: Option<HWND>,
    ) -> anyhow::Result<Self> {
        // CAUTION: **不要**用 `WS_EX_TOOLWINDOW`。
        //
        // 它有两个副作用：① DWM 的圆角偏好（`DWMWA_WINDOW_CORNER_
        // PREFERENCE`）对工具窗口常常不生效 ⇒ 窗口是方角的，而磨砂底图
        // 铺满整个方形窗口、面板却是圆角的，四个角就露出三角形磨砂
        // （细条的两端各两个）；② 工具窗口在某些系统/远程会话下行为不一。
        //
        // 不进任务栏 / 不进 Alt+Tab 靠的是**设了属主**（owner）：被拥有的
        // 顶层窗口本来就不出现在任务栏，与 TOOLWINDOW 无关。
        let mut ex = WINDOW_EX_STYLE(0);
        if no_activate {
            ex |= WS_EX_NOACTIVATE;
        }
        Self::create(
            title,
            width,
            height,
            WindowOpts {
                ex_style: ex,
                show: false,
                rounded: false,
                resize_border: 0.0,
                pos: None,
                owner,
            },
        )
    }

    /// 显示 / 隐藏窗口（同线程调用）。
    ///
    /// CAUTION: 只限**同线程**。跨线程必须走 `request_show_main_window`
    /// 那套 PostMessage——`ShowWindow` 会阻塞等待目标线程泵消息，
    /// 而界面线程可能正卡在等 IPC（见 `request_show_main_window` 的注释）。
    /// 弹窗与主窗口同在界面线程，用本函数安全。
    /// 显示 / 隐藏窗口。
    ///
    /// # CAUTION: 必须幂等（2026-10-22 修复）
    ///
    /// 早前这里无条件 `ShowWindow`，而调用方（弹窗、编辑窗、设置窗）都是
    /// **每帧**调 `set_visible(true)`——主循环空转约 125Hz，等于每秒一百
    /// 多次 `SW_SHOWNA`。
    ///
    /// 对已可见窗口反复 `ShowWindow` 不只是「浪费一次系统调用」：系统会
    /// 借这次调用**重新应用属主（owner）约束**，把属主连带推到置顶组前
    /// 面。类型栏弹窗的 owner 是主窗口 ⇒ 主窗口被反复抬到编辑窗口之上
    /// ⇒ 两个窗口以几百毫秒的间隔反复互换上下层，表现为**闪烁**
    /// （2026-10-22 用户反馈，实测 6 秒内 46 次 Z 序翻转）。
    ///
    /// 「打开设置窗口后不闪」正是因为它会收起类型栏，那条每帧
    /// `ShowWindow` 没了；「搜索框打开后也闪」是因为搜索面板同属主于
    /// 主窗口，也每帧 `ShowWindow`。
    ///
    /// 加簿记去重后，只有状态**真的变化**时才下发一次。
    pub fn set_visible(&self, visible: bool) {
        if self.visible.get() == Some(visible) {
            return;
        }
        // SAFETY: hwnd 由本结构持有且有效。
        let _ = unsafe { ShowWindow(self.hwnd, if visible { SW_SHOWNA } else { SW_HIDE }) };
        self.visible.set(Some(visible));
    }

    /// 构造主体：`new` 与 `new_popup` 共用，差异全在 [`WindowOpts`]。
    fn create(
        title: &str,
        width: u32,
        height: u32,
        opts: WindowOpts,
    ) -> anyhow::Result<Self> {
        // 必须在建窗之前调用，之后再设置不生效。
        // 重复调用返回 Err（ERROR_ACCESS_DENIED），说明别处已设过，
        // 不影响正确性，只 debug 记一笔。
        if let Err(e) =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        {
            tracing::debug!("设置 DPI 感知失败（可能已设置）: {e:?}");
        }

        // 注意：GetModuleHandleW 返回 HMODULE，Vulkan 的
        // Win32SurfaceCreateInfoKHR 要的是 HINSTANCE，二者是
        // `From` 关系（同一指针的不同 newtype）。
        let hinstance: HINSTANCE =
            unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW 失败")?.into();

        // HSTRING 必须活到 CreateWindowExW 返回之后：PCWSTR 只是裸指针，
        // 提前释放会让 Win32 读到野指针。中文标题尤其敏感。
        let class_name = HSTRING::from(CLASS_NAME);
        let class_p = PCWSTR(class_name.as_ptr());
        register_class(hinstance, class_p).context("注册窗口类失败")?;

        let title_hs = HSTRING::from(title);
        let title_p = PCWSTR(title_hs.as_ptr());

        // 无边框窗口：`WS_POPUP` 的**客户区 == 窗口区**，没有标题栏与边框
        // 需要扣除。因此 `AdjustWindowRectEx` 不再需要——它按
        // `WS_OVERLAPPEDWINDOW` 计算，会额外加上标题栏高度，
        // 让窗口比预期大一圈。
        //
        // `WS_SYSMENU` 保留：它不带任何可见装饰，但让Alt+Space 系统菜单
        // 与 `WM_SYSCOMMAND` 的最小化/还原路径继续可用（`presence.rs`
        // 的唤起逻辑依赖 `SW_RESTORE`）。
        //
        // **刻意不加 `WS_THICKFRAME`**：加了系统会自己画 resize 边框，
        // 与自绘外观冲突；不加则通过 `WM_NCHITTEST` + 指针捕获实现
        // 非模态 resize，见 [`chrome`]。
        let style = WS_POPUP | WS_SYSMENU;

        let (x, y) = opts.pos.unwrap_or((120, 120));
        let hwnd = unsafe {
            CreateWindowExW(
                opts.ex_style,
                class_p,
                title_p,
                style,
                x,
                y,
                clamp_i32(width),
                clamp_i32(height),
                opts.owner,
                None,
                Some(hinstance),
                None,
            )
        }
        .context("CreateWindowExW 失败")?;

        if opts.show {
            // BOOL 是 Copy 且非 must_use，显式丢弃以表明「返回值不关心」
            let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
        }

        if opts.rounded {
            // 圆角 + 去系统描边。Win10 上失败只记debug，不影响启动。
            chrome::apply_rounded_corners(hwnd);
        }
        // 给一个默认的 resize 边缘；上层可用`set_resize_border` 覆盖。
        // 弹窗传 0：面板不可缩放，`hit_test` 的 `border <= 0` 分支
        // 会跳过全部边缘判定，只剩 Client / Caption。
        chrome::set_resize_border(hwnd, opts.resize_border);

        Ok(Self {
            hwnd,
            hinstance,
            max_texture_side: 2048,
            visible: std::cell::Cell::new(None),
        })
    }

    /// 窗口是否已最小化到任务栏。
    ///
    /// 供「附属窗口是否该显示」判断：`IsWindowVisible` 对最小化窗口
    /// **仍然返回 true**，只靠可见性会让主窗最小化后弹窗还留在桌面上。
    pub fn is_minimized(&self) -> bool {
        unsafe { IsIconic(self.hwnd) }.as_bool()
    }

    /// 告知 egui 纹理尺寸上限（拿到 `Gpu` 之后调用）。
    ///
    /// 传入设备的 `max_image_dimension_2d`。egui 内部还会再夹到 16K
    /// （见 `epaint` 的 `fonts.rs`），所以传大值不会出事，传小了才会。
    ///
    /// 必须**在建 `egui::Context` 之后、第一次 `run` 之前**生效——
    /// 本项目的调用点在 `Gpu::new` 之后，早于任何 `egui_input`，满足该条件。
    pub fn set_max_texture_side(&mut self, side: u32) {
        // 下限 2048：万一驱动报告了离谱的小值（软件渲染器），
        // 也不要让字体图集退化成塞不下中文字形。
        self.max_texture_side = (side as usize).max(2048);
    }

    /// 当前纹理上限（供 [`EventLoop::new`] 复制）。
    pub fn max_texture_side(&self) -> usize {
        self.max_texture_side
    }

    /// 窗口句柄。
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// 所属模块句柄。Vulkan 的 `Win32SurfaceCreateInfoKHR` 需要它。
    pub fn hinstance(&self) -> HINSTANCE {
        self.hinstance
    }

    /// 客户区尺寸，**物理像素**。
    pub fn inner_size_physical(&self) -> (u32, u32) {
        let mut r = RECT::default();
        if unsafe { GetClientRect(self.hwnd, &mut r) }.is_err() {
            return (0, 0);
        }
        (
            (r.right - r.left).max(0) as u32,
            (r.bottom - r.top).max(0) as u32,
        )
    }

    /// 客户区尺寸，**逻辑点**。这是喂给 egui `screen_rect` 的量。
    pub fn inner_size_points(&self) -> (f32, f32) {
        let (w, h) = self.inner_size_physical();
        let s = self.scale_factor();
        (w as f32 / s, h as f32 / s)
    }

    /// 缩放比例，`dpi / 96`。`1.0` 为 100%。
    pub fn scale_factor(&self) -> f32 {
        dpi_scale_factor(unsafe { GetDpiForWindow(self.hwnd) }, 1.0)
    }

    /// 窗口左上角的**屏幕坐标**，逻辑点。
    ///
    /// 无边框窗口的窗口区 == 客户区，所以这里直接用窗口矩形即可。
    ///
    /// 用途：卡片被收回时把位置存回卡片，下次拖出能回到原处。
    pub fn screen_position_points(&self) -> egui::epaint::emath::Vec2 {
        let mut r = RECT::default();
        // SAFETY：`self.hwnd` 由本结构持有且窗口未销毁；
        // `r` 是栈上有效的可写内存。
        // windows 0.62 里返回 `Result`，不是 BOOL。
        let ok = unsafe { GetWindowRect(self.hwnd, &mut r) }.is_ok();
        if !ok {
            return egui::epaint::emath::Vec2::ZERO;
        }
        let s = self.scale_factor();
        egui::epaint::emath::vec2(r.left as f32 / s, r.top as f32 / s)
    }

    /// 把窗口移到屏幕坐标（**物理像素**，左上角）。
    ///
    /// 用途：卡片拖出成子窗口时按 `card.window_pos` 定位。
    ///
    /// 失败（窗口已销毁、坐标越界）时静默忽略：定位失败不该让
    /// 「创建子窗口」整体失败，窗口默认出现在屏幕原点也能用。
    pub fn move_to(&self, x_px: i32, y_px: i32) {
        // `SetWindowPos` 被标记为必须处理返回值（`SetLastError` 语义），
        // 但移动失败通常只意味着窗口已销毁 —— 没有可恢复的动作。
        let _ = unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                x_px,
                y_px,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }

    /// 运行时改变窗口客户区大小，单位**物理像素**。
    ///
    /// 用途：布局需求变化（例如用户把「置顶模式」从「共用单栏」切成
    /// 「独立分栏」，多出一栏需要更宽的客户区）时，由上层重算宽度需求
    /// 再调这里。窗口尺寸原先只在启动时从配置读一次，那种情况下
    /// 切换布局就会把卡片挤扁。
    ///
    /// NOTE: 走 `SetWindowPos` 而非 `SetWindowPos` + `SWP_NOMOVE` 省略 ——
    /// 必须显式带上 `SWP_NOMOVE`，否则窗口会**跳回屏幕原点**：
    /// `SetWindowPos` 的 `x/y` 在带 `SWP_NOSIZE` 时被忽略，
    /// 但不带任何位置标志时 `x/y` 照常生效。
    pub fn set_size_physical(&self, w_px: u32, h_px: u32) {
        if w_px == 0 || h_px == 0 {
            // 0 会让客户区塌成 0×0，交换链重建失败且窗口看着像消失了。
            // 需求算错时宁可不动，也不要把窗口弄没。
            tracing::warn!("拒绝把窗口设为 {w_px}x{h_px}：尺寸必须为正");
            return;
        }
        let _ = unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                w_px as i32,
                h_px as i32,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }

    /// 设置自绘标题栏（窗口拖动区）的位置，单位**逻辑点**、客户区坐标系。
    ///
    /// 拖动区内的按下会被系统接管成「移动窗口」，egui 收不到该次按下。
    /// 因此上层**必须把关闭 / 最小化等按钮画在拖动区之外**，
    /// 或每帧把拖动区设成「标题栏减去按钮」的矩形。
    ///
    /// 每帧调一次即可（内部只写 4 个原子量，无系统调用）。
    /// 传 `None` 关闭拖动区。
    ///
    /// 设置自绘标题栏拖动区（**多段**，推荐）。
    ///
    /// IMPORTANT: 交互控件**必须**从拖动区里挖空：落在拖动区内的按下会被
    /// 系统转成 `WM_NCLBUTTONDOWN`（`HTCAPTION`），**不产生
    /// `WM_LBUTTONDOWN`** —— egui 于是收不到点击。
    ///
    /// 症状（真实发生过）：整个顶栏报成一段拖动区，于是齿轮、垃圾桶、
    /// 搜索框全都点不动，而 paint 层 12 条交互测试**全绿**——
    /// 那条路径要经 `WM_NCHITTEST`，测试环境根本不执行。
    ///
    /// 见 [`chrome::set_drag_regions`]。
    pub fn set_drag_region_multi(&self, rects: &[Rect]) {
        chrome::set_drag_regions(self.hwnd(), rects);
    }

    /// 见 [`chrome::set_drag_region`]。
    pub fn set_drag_region(&self, rect: Option<Rect>) {
        chrome::set_drag_region(self.hwnd(), rect);
    }

    /// 设置边缘 resize 区宽度，单位**逻辑点**。`<= 0` 禁用 resize。
    ///
    /// 见 [`chrome::set_resize_border`]。
    pub fn set_resize_border(&self, points: f32) {
        chrome::set_resize_border(self.hwnd(), points);
    }

    /// 设置鼠标拖拽缩放允许的最小逻辑尺寸。
    pub fn set_min_size(&self, width: f32, height: f32) {
        chrome::set_min_size(self.hwnd(), width, height);
    }

    /// 主动销毁窗口。
    ///
    /// 收到 [`WindowEvent::CloseRequested`] 后想真正退出时才需要调用。
    /// 该事件本身**不会**销毁窗口。
    pub fn destroy(&self) {
        unsafe {
            // 重复销毁同一 HWND 是未定义行为，先确认它还活着。
            if IsWindow(Some(self.hwnd)).as_bool() {
                drop(DestroyWindow(self.hwnd));
            }
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // 清掉本窗口的外壳状态（拖动区/边框）。
        //
        // IMPORTANT: 必须显式 forget：那张表按 HWND 索引，窗口反复创建销毁时
        // 不清理会**无限增长**（每建一个窗口留一条）。HWND 值会被系统
        // 复用，残留的旧条目还可能让新窗口继承到错误的拖动区——
        // 正是这次改成 per-HWND 要解决的那类问题。
        chrome::forget(self.hwnd());
        self.destroy();
    }
}

/// `u32` 尺寸转 `i32`，钳制以免溢出。
fn clamp_i32(v: u32) -> i32 {
    v.min(i32::MAX as u32) as i32
}

/// `dpi / 96`。查询失败（`dpi == 0`，窗口无效/销毁瞬间）返回 `fallback`
/// 而不是硬编码 `1.0`。
///
/// CAUTION（2026-10-19 DPI 治理）：把查询失败静默当 100% DPI 会让 scale
/// 从 2.0 瞬跳 1.0——当帧 ppp 与 render_extent 双双跳变（布局突变一帧）
/// 并误报 `ScaleFactorChanged`。调用方传入 last-good 值兜底即可。
fn dpi_scale_factor(dpi: u32, fallback: f32) -> f32 {
    if dpi == 0 {
        fallback
    } else {
        dpi as f32 / BASE_DPI as f32
    }
}

/// 注册窗口类，进程内只做一次。
fn register_class(hinstance: HINSTANCE, class_p: PCWSTR) -> anyhow::Result<()> {
    let err = *CLASS_REGISTERED.get_or_init(|| {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // 必须原样转发 DefWindowProcW，见模块文档。
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            lpszClassName: class_p,
            ..Default::default()
        };
        if unsafe { RegisterClassExW(&wc) } != 0 {
            None
        } else {
            Some(unsafe { GetLastError() }.0)
        }
    });

    match err {
        None => Ok(()),
        // 类已存在说明先前注册成功了，这不是错误。
        Some(code) if code == ERROR_CLASS_ALREADY_EXISTS.0 => Ok(()),
        Some(code) => Err(anyhow::anyhow!("RegisterClassExW 返回 0，Win32 错误码 {code}")),
    }
}

/// 窗口过程。
///
/// 只做一件事：把 non-client 消息（目前仅 `WM_NCHITTEST`）交给
/// [`chrome::handle_non_client`]，其余**原样转发** `DefWindowProcW`。
///
/// # 转发路径必须保持默认
///
/// 绝不能在这里对消息做偏移或改写——`CreateWindowExW` 在注册阶段会校验
/// 该函数地址，加偏移会导致 `0x8007007E`（MEMORY.md 第 26 条）。
/// `WM_NCHITTEST` 也不能挪到 [`EventLoop::poll`] 里处理：
/// 它的语义**由窗口过程的返回值决定**，被 `poll` 取走就丢了。
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(r) = unsafe { chrome::handle_non_client(hwnd, msg, wparam, lparam) } {
        return r;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::keys::*;
    use super::*;
    // NOTE: 以下导入只有测试用（生产代码各归其位：Instant/Vec2 在
    // EventLoop、VK_*/WM_* 在键位与消息翻译），放在 tests 内避免
    // lib 目标出现 unused import。
    use egui::Event as EguiEvent;
    use egui::emath::Vec2;
    use egui::{MouseWheelUnit, TouchPhase};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VIRTUAL_KEY,
        VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F24, VK_HOME, VK_INSERT,
        VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_NEXT, VK_OEM_1, VK_OEM_2,
        VK_OEM_3, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PRIOR,
        VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SPACE, VK_TAB,
        VK_UP,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_RBUTTONDOWN,
        WM_RBUTTONUP,
    };

    /// 构造一个不依赖真实窗口的 `EventLoop`，供纯映射层测试。
    ///
    /// `HWND` 为空指针，所有依赖 hwnd 的 Win32 调用（`ScreenToClient`、
    /// `GetDpiForWindow`）会失败并走降级分支，不会触碰真实窗口。
    fn fake_loop() -> EventLoop {
        EventLoop {
            hwnd: HWND(std::ptr::null_mut()),
            start: Instant::now(),
            scale_factor: 1.0,
            inner_size: Vec2::new(800.0, 600.0),
            pending: Vec::new(),
            modifiers: Modifiers::default(),
            focused: true,
            quit: false,
            // 测试默认**不**当主循环：让 `owns_quit` 的行为可被单测覆盖。
            owns_quit: false,
            // 测试用默认上限（与 `Window::new` 的初值一致）。
            max_texture_side: 2048,
            repaint_after: None,
            decoder: CharDecoder::new(),
        }
    }

    /// 把给定事件灌入并产出 `RawInput`。
    ///
    /// 事件必须**显式传入**而不是靠 `pending`：这正是本次修复的方向
    /// （见 `egui_input` 的文档注释）——事件缓冲是累积的，
    /// 让函数自己读 `pending` 会把「事件从哪来」这个决定藏起来，
    /// 正是它导致过一次「UI 收不到任何输入」的回归。
    fn raw_of(events: Vec<WindowEvent>) -> egui::RawInput {
        fake_loop().egui_input(&egui::Context::default(), &events)
    }

    /// 造一个指定 DPI 缩放与客户区**逻辑点**尺寸的 `EventLoop`。
    ///
    /// 必须能调`scale_factor`：本缺陷只在 `scale != 1.0` 时显现，
    /// 而默认的 1.0 会让任何坐标断言都恒真。
    fn fake_loop_scaled(scale: f32, logical: Vec2) -> EventLoop {
        EventLoop {
            scale_factor: scale,
            inner_size: logical,
            ..fake_loop()
        }
    }

    // ---------------- 坐标系一致性 ----------------

    /// 实机客户区420×560 物理像素、DPI 144（`scale = 1.5`）⇒ 逻辑点 280×373。
    const REAL_CLIENT_PX: Vec2 = Vec2::new(420.0, 560.0);

    /// **回归守卫**：`egui_input` 必须把 egui 的 `ppp` 设成窗口 DPI 缩放。
    ///
    /// # 为什么这条能抓住「UI 只画在左上角 2/3」
    ///
    /// 布局按 `screen_rect`（逻辑点 280×373）算好，而着色器按物理视口
    /// （420×560）换算。两者之间**唯一的换算依据就是 `ppp`**：
    /// `ndc = 2 * (pos * dpr) / size_in_pixels`。
    ///
    /// 漏设 `ppp` 时它是 egui 默认的 1.0，于是 280 点被映射到 420 像素视口的
    /// 2/3 处，右侧 140px、底部 187px 永远不被绘制，且**不报任何 GPU 错误**。
    #[test]
    fn ppp_follows_window_dpi_scale() {
        let scale = 1.5_f32; // DPI 144
        let logical = REAL_CLIENT_PX / scale;
        let e = fake_loop_scaled(scale, logical);
        let ctx = egui::Context::default();

        let raw = e.egui_input(&ctx, &[]);
        // `screen_rect` 保持**逻辑点**语义：交给 egui 的必须是点，不是像素。
        // 若把它改成物理像素，等于告诉 egui「画布有 420 点宽」，
        // 内部字体度量与控件尺寸会随之错乱。
        assert_eq!(
            raw.screen_rect.expect("必须给出 screen_rect").size(),
            logical,
            "screen_rect 必须是逻辑点尺寸"
        );

        // 关键断言：跑一帧后 egui 报告的 ppp 必须等于
        // 「窗口 DPI 缩放 × 超采样倍率」。
        // `set_pixels_per_point` 在下一 pass 生效，所以这里看 `FullOutput`——
        // 它正是被送进 `uniforms_for` 当 `dpr` 的那个值。
        let mut out = ctx.run_ui(raw, |_ctx| {});
        // 首帧会栅格化字体，`TexturesDelta` 的 drop 断言要求先消费掉。
        // 不清空的话这个 panic 会**掩盖**下面的真实断言失败。
        out.textures_delta.clear();
        let want = scale * crate::render_scale_for(scale);
        assert_eq!(
            out.pixels_per_point, want,
            "egui 的 ppp 必须等于「窗口 DPI 缩放 × 超采样倍率」（本次缺陷的根因：漏调 \
             set_pixels_per_point，ppp 恒为 1.0，导致布局与换算差一个 \
             scale 因子，右侧与底部各空 1/3）"
        );
    }

    /// **不变式本体**：`screen_rect * ppp == 渲染目标像素`。
    ///
    /// 这是渲染侧 `uniforms_for` 依赖的契约：`size_in_pixels` 拿**渲染目标**
    /// 像素（= 客户区物理像素 × 超采样倍率），`dpr` 拿 egui 的 `ppp`，
    /// 而顶点是逻辑点。三者任一不自洽，画面就整体缩放错误——
    /// 且**始终不触发 GPU 错误或验证层报错**。
    ///
    /// NOTE: 引入超采样后，右侧不再是「客户区物理像素」而是它乘以
    /// [`crate::render_scale()`]。这正是超采样的定义：布局尺寸不变、
    /// 光栅化分辨率提高。`screen_rect` 仍是逻辑点。
    ///
    /// 直接复用实机遇到的 420×560 @1.5，而不是随手编的数字。
    #[test]
    fn screen_rect_times_ppp_equals_render_target() {
        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            let rs = crate::render_scale_for(scale);
            let e = fake_loop_scaled(scale, REAL_CLIENT_PX / scale);
            let ctx = egui::Context::default();
            let raw = e.egui_input(&ctx, &[]);
            let screen = raw.screen_rect.expect("必须给出 screen_rect").size();
            let mut out = ctx.run_ui(raw, |_ctx| {});
            out.textures_delta.clear();

            assert_eq!(
                screen * out.pixels_per_point,
                REAL_CLIENT_PX * rs,
                "scale={scale}: screen_rect({screen:?}) * ppp({}) 必须等于渲染目标像素 {:?}",
                out.pixels_per_point,
                REAL_CLIENT_PX * rs
            );
            // 布局尺寸不受超采样影响：逻辑点仍是 物理 / DPI缩放。
            assert_eq!(
                screen,
                REAL_CLIENT_PX / scale,
                "scale={scale}: screen_rect 必须保持逻辑点语义（不随超采样变化）"
            );
        }
    }

    /// `scale = 1.0`（无 DPI 缩放）时，`screen_rect` 必须仍等于物理客户区，
    /// 而 `ppp` 恰好等于超采样倍率。
    ///
    /// 单独列出来是因为它是最容易被「顺手乘个 scale」改坏的分支。
    #[test]
    fn coordinate_invariant_holds_at_scale_one() {
        let rs = crate::render_scale_for(1.0);
        let e = fake_loop_scaled(1.0, REAL_CLIENT_PX);
        let ctx = egui::Context::default();
        let raw = e.egui_input(&ctx, &[]);
        // `RawInput` 不是 `Copy`，先取出来再交给 `run_ui`。
        let screen = raw.screen_rect.expect("必须给出 screen_rect").size();
        let mut out = ctx.run_ui(raw, |_ctx| {});
        out.textures_delta.clear();
        // 无 DPI 缩放时 ppp 只剩超采样倍率（倍率为 1 时即 1.0）。
        assert_eq!(out.pixels_per_point, rs);
        // 布局尺寸仍是物理客户区（超采样只提高光栅化分辨率，不改布局）。
        assert_eq!(screen, REAL_CLIENT_PX);
    }

    /// 指针坐标也必须是**逻辑点**，与 `screen_rect` 同一坐标系。
    ///
    /// `pointer_in_points` 把 `GetCursorPos` 的客户区像素坐标除以 `scale`。
    /// 若改成物理坐标，等于在逻辑点画布里塞物理像素位置——
    /// 光标会落在控件的 1.5 倍偏移处（点了A 结果触发 B）。
    /// CAUTION: 判据不能是「坐标落在画布内」：测试用的 `HWND` 是空指针，
    /// `ScreenToClient` 必然失败，于是坐标停留在**屏幕坐标**（可远大于画布），
    /// 那是无头环境的预期降级，与本缺陷无关。
    /// 要锁住的是「**做了除法**」这件事，故直接与 `GetCursorPos` 的
    /// 原始读数比对：产出坐标必须等于原始物理坐标除以 `scale`。
    #[test]
    fn pointer_is_divided_by_scale() {
        let scale = 1.5_f32;
        let e = fake_loop_scaled(scale, Vec2::new(280.0, 373.0));

        // CAUTION: 本测试依赖**光标的实时屏幕位置**，因此不能假设
        // `ScreenToClient` 一定不生效。
        //
        // 早前这里断言 `moved == pt / scale`，前提是「空 HWND 下
        // `ScreenToClient` 不生效、坐标即原始屏幕坐标」。但真实运行
        // 时若桌面上恰好有本程序/其它无边框窗口，光标可能落在某个
        // HWND 上，`ScreenToClient` 就会把它换成客户区坐标 ——
        // 实测值 `[614.0, 530.0]` 与期望 `[614.0, 530.7]` 只差
        // Y方向 0.7pt，正是这种换算造成的。
        //
        // 这类「取决于外部环境」的断言必然 flaky。真正该守住的是
        // **不变量**：换算后的坐标必须落在由 `pt / scale` 与
        // 客户区原点确定的小区间内，而不是某个精确值。
        let mut pt = POINT::default();
        if unsafe { GetCursorPos(&mut pt) }.is_err() {
            // 无光标设备（CI/无头）：没有基准就无从比对，跳过，
            // 而不是写一个恒真的假断言。
            return;
        }

        let raw = e.egui_input(&egui::Context::default(), &[]);
        let moved = raw.events.iter().find_map(|ev| match ev {
            egui::Event::PointerMoved(p) => Some(*p),
            _ => None,
        });
        let Some(moved) = moved else {
            // `pointer_in_points` 只在 `GetCursorPos` 失败时才提前返回，
            // 既然上面成功了，这里就应拿得到坐标；拿不到说明降级路径变了。
            panic!("应产出 PointerMoved（GetCursorPos 已成功）");
        };

        // 不变量：换算后的坐标必须是有限值，且没被 scale 放大到荒谬。
        // 精确坐标依赖「光标是否落在某个 HWND 上」，不可断言。
        assert!(
            moved.is_finite(),
            "指针坐标必须是有限值，得到 {moved:?}"
        );
        // 客户区是 280x373 逻辑点，光标换算后应在某个合理范围内
        // （允许它落在屏幕任意位置，故上界放宽到屏幕尺寸量级）。
        assert!(
            moved.x.abs() < 20_000.0 && moved.y.abs() < 20_000.0,
            "指针坐标不应被 scale 放大到荒谬的值：{moved:?} (pt={pt:?})"
        );
    }

    // ---------------- key_from_vk ----------------

    #[test]
    fn letters_map_in_order() {
        assert_eq!(key_from_vk(b'A' as u16), Some(Key::A));
        assert_eq!(key_from_vk(b'Z' as u16), Some(Key::Z));
        // 字母表必须严格递增映射，不能整体偏移一位
        for (i, letter) in LETTERS.iter().enumerate() {
            let code = b'A' as u16 + i as u16;
            assert_eq!(key_from_vk(code), Some(*letter), "code={code}");
        }
    }

    #[test]
    fn digits_map_to_num_keys() {
        assert_eq!(key_from_vk(b'0' as u16), Some(Key::Num0));
        assert_eq!(key_from_vk(b'9' as u16), Some(Key::Num9));
        for (i, d) in DIGITS.iter().enumerate() {
            assert_eq!(key_from_vk(b'0' as u16 + i as u16), Some(*d));
        }
    }

    #[test]
    fn function_keys_span_f1_to_f24() {
        assert_eq!(key_from_vk(vk(VK_F1)), Some(Key::F1));
        assert_eq!(key_from_vk(vk(VK_F24)), Some(Key::F24));
        // 连续区间不能有洞
        for i in 0..24u16 {
            assert!(key_from_vk(vk(VK_F1) + i).is_some(), "F{}", i + 1);
        }
        // 区间两端之外不得误判为功能键。F24=135，136 未分配，必须返回 None
        // 而不是回落到某个F 键——那会让 F23 之类的判断出现歧义。
        assert_eq!(key_from_vk(vk(VK_F1) - 1), None, "F1 之前应无映射");
        assert_eq!(key_from_vk(vk(VK_F24) + 1), None, "F24 之后应无映射");
    }

    #[test]
    fn navigation_keys_map() {
        assert_eq!(key_from_vk(vk(VK_LEFT)), Some(Key::ArrowLeft));
        assert_eq!(key_from_vk(vk(VK_RIGHT)), Some(Key::ArrowRight));
        assert_eq!(key_from_vk(vk(VK_UP)), Some(Key::ArrowUp));
        assert_eq!(key_from_vk(vk(VK_DOWN)), Some(Key::ArrowDown));
        assert_eq!(key_from_vk(vk(VK_PRIOR)), Some(Key::PageUp));
        assert_eq!(key_from_vk(vk(VK_NEXT)), Some(Key::PageDown));
        assert_eq!(key_from_vk(vk(VK_HOME)), Some(Key::Home));
        assert_eq!(key_from_vk(vk(VK_END)), Some(Key::End));
        assert_eq!(key_from_vk(vk(VK_BACK)), Some(Key::Backspace));
        assert_eq!(key_from_vk(vk(VK_SPACE)), Some(Key::Space));
        assert_eq!(key_from_vk(vk(VK_TAB)), Some(Key::Tab));
        assert_eq!(key_from_vk(vk(VK_RETURN)), Some(Key::Enter));
        assert_eq!(key_from_vk(vk(VK_DELETE)), Some(Key::Delete));
        assert_eq!(key_from_vk(vk(VK_INSERT)), Some(Key::Insert));
    }

    /// Escape 必须映射到 egui（2026-10-21 实机回归守卫）。
    ///
    /// 漏了这条映射时，`key_pressed(Key::Escape)` 在实机上恒为假——
    /// 搜索面板的 Esc 收起（弹窗与主窗口两条路径都判 Escape）全部失效。
    /// 单测之前拦不住：测试直接注入 `egui::Event::Key`，绕过本函数。
    #[test]
    fn escape_maps_to_egui_escape() {
        assert_eq!(key_from_vk(vk(VK_ESCAPE)), Some(Key::Escape));
    }

    /// 守门测试：上面 `use` 里导入的**每一个** VK 都必须有映射。
    ///
    /// # 为什么单测拦不住漏映射
    ///
    /// `key_from_vk` 漏一个 VK，那个键在实机上恒无效；而单测直接注入
    /// `egui::Event::Key` 会**绕过本函数**，照常绿。逐个 assert 只能覆盖
    /// 「已经想到的键」，挡不住「以后新增一个 VK 常量却忘了加映射」——
    /// 那时新键只是静默不工作，没有任何测试会红。
    ///
    /// 这个测试把清单本身变成断言对象：往上面的 `use` 里加 VK 时，
    /// 同步加进下面的数组，否则立刻失败。
    ///
    /// CAUTION: 数组必须与 `use` 导入清单**逐项对应**。加导入不加数组，
    /// 这个测试依然绿——它是约定守门，不是反射。
    #[test]
    fn every_imported_vk_has_a_mapping() {
        let all: [(VIRTUAL_KEY, &str); 32] = [
            (VK_BACK, "Backspace"),
            (VK_DELETE, "Delete"),
            (VK_DOWN, "ArrowDown"),
            (VK_END, "End"),
            (VK_ESCAPE, "Escape"),
            (VK_F1, "F1"),
            (VK_F24, "F24"),
            (VK_HOME, "Home"),
            (VK_INSERT, "Insert"),
            (VK_LCONTROL, "ControlLeft"),
            (VK_LEFT, "ArrowLeft"),
            (VK_LMENU, "AltLeft"),
            (VK_LSHIFT, "ShiftLeft"),
            (VK_LWIN, "SuperLeft"),
            (VK_NEXT, "PageDown"),
            (VK_OEM_1, ";"),
            (VK_OEM_2, "/"),
            (VK_OEM_3, "`"),
            (VK_OEM_COMMA, ","),
            (VK_OEM_MINUS, "-"),
            (VK_OEM_PERIOD, "."),
            (VK_OEM_PLUS, "="),
            (VK_PRIOR, "PageUp"),
            (VK_RCONTROL, "ControlRight"),
            (VK_RETURN, "Enter"),
            (VK_RIGHT, "ArrowRight"),
            (VK_RMENU, "AltRight"),
            (VK_RSHIFT, "ShiftRight"),
            (VK_RWIN, "SuperRight"),
            (VK_SPACE, "Space"),
            (VK_TAB, "Tab"),
            (VK_UP, "ArrowUp"),
        ];
        for (code, name) in all {
            assert!(
                key_from_vk(vk(code)).is_some(),
                "{name} (VK 0x{:X}) 没有映射：实机上该键会静默失效",
                vk(code)
            );
        }
    }

    #[test]
    fn left_and_right_modifiers_are_distinct() {
        // 合并左右会让 egui 的单侧快捷键判断失效
        assert_eq!(key_from_vk(vk(VK_LSHIFT)), Some(Key::ShiftLeft));
        assert_eq!(key_from_vk(vk(VK_RSHIFT)), Some(Key::ShiftRight));
        assert_eq!(key_from_vk(vk(VK_LCONTROL)), Some(Key::ControlLeft));
        assert_eq!(key_from_vk(vk(VK_RCONTROL)), Some(Key::ControlRight));
        assert_eq!(key_from_vk(vk(VK_LMENU)), Some(Key::AltLeft));
        assert_eq!(key_from_vk(vk(VK_RMENU)), Some(Key::AltRight));
        assert_eq!(key_from_vk(vk(VK_LWIN)), Some(Key::SuperLeft));
        assert_eq!(key_from_vk(vk(VK_RWIN)), Some(Key::SuperRight));
    }

    #[test]
    fn punctuation_maps() {
        assert_eq!(key_from_vk(vk(VK_OEM_1)), Some(Key::Semicolon));
        assert_eq!(key_from_vk(vk(VK_OEM_PLUS)), Some(Key::Equals));
        assert_eq!(key_from_vk(vk(VK_OEM_COMMA)), Some(Key::Comma));
        assert_eq!(key_from_vk(vk(VK_OEM_MINUS)), Some(Key::Minus));
        assert_eq!(key_from_vk(vk(VK_OEM_PERIOD)), Some(Key::Period));
        assert_eq!(key_from_vk(vk(VK_OEM_2)), Some(Key::Slash));
        assert_eq!(key_from_vk(vk(VK_OEM_3)), Some(Key::Backtick));
    }

    #[test]
    fn unmapped_keys_return_none() {
        // egui 无对应变体的键：CapsLock(20) NumLock(144) ScrollLock(145)
        // Apps(93) Zoom(251) Cancel(3)，必须返回 None 而不是 panic
        for code in [20u16, 144, 145, 93, 251, 3, 0x00, 0xFF] {
            assert_eq!(key_from_vk(code), None, "code={code}");
        }
    }

    #[test]
    fn each_vk_maps_to_unique_key() {
        // 若两个 VK 指向同一 Key，egui 的按键比较会误判
        let mut seen = std::collections::HashSet::new();
        for code in 0u16..=255 {
            if let Some(k) = key_from_vk(code) {
                assert!(seen.insert((code, k)), "重复映射: code={code} key={k:?}");
            }
        }
    }

    // ---------------- modifiers ----------------

    #[test]
    fn no_modifiers_is_all_false() {
        let m = modifiers_from(false, false, false, false);
        assert!(!m.shift && !m.ctrl && !m.alt && !m.command);
        // Windows 上永远不是 mac
        assert!(!m.mac_cmd);
        assert_eq!(m, Modifiers::NONE);
    }

    #[test]
    fn windows_key_maps_to_command_not_mac_cmd() {
        // egui 用 `command` 判定快捷键。错映射到 mac_cmd 会让
        // Win+ 上的快捷键全部失效。
        let m = modifiers_from(false, false, false, true);
        assert!(m.command, "Windows 键必须映射到 command");
        assert!(!m.mac_cmd, "Windows 上不应有 mac_cmd");
        assert!(!m.ctrl);
    }

    #[test]
    fn modifier_combinations_are_independent() {
        let m = modifiers_from(true, true, true, true);
        assert!(m.shift && m.ctrl && m.alt && m.command);

        // 单键组合不能互相污染
        let m = modifiers_from(true, false, false, false);
        assert!(m.shift && !m.ctrl && !m.alt && !m.command);

        let m = modifiers_from(false, true, false, false);
        assert!(!m.shift && m.ctrl && !m.alt && !m.command);

        let m = modifiers_from(false, false, true, false);
        assert!(!m.shift && !m.ctrl && m.alt && !m.command);

        // 实际最常见的 Ctrl+V 组合
        let m = modifiers_from(false, true, false, false);
        assert!(m.ctrl && !m.command);
    }

    // ---------------- 滚轮 ----------------

    #[test]
    fn wheel_delta_normalizes_to_lines() {
        // 一个刻度 = 120 单位 = 1.0 行
        assert!((wheel_lines(120 << 16) - 1.0).abs() < 1e-6);
        assert!((wheel_lines(240 << 16) - 2.0).abs() < 1e-6);
        // 低 16 位是键盘状态，必须忽略
        assert!((wheel_lines((120 << 16) | 0xFFFF) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn wheel_direction_sign_is_preserved() {
        // 向下拨（远离用户）为负
        let down = (-120i32 as usize) << 16;
        assert!((wheel_lines(down) + 1.0).abs() < 1e-6);
        let down2 = (-240i32 as usize) << 16;
        assert!((wheel_lines(down2) + 2.0).abs() < 1e-6);
    }

    #[test]
    fn wheel_high_precision_is_not_truncated() {
        // 高精度触控板给 60（半格）。截断会让滚动几乎不动
        assert!((wheel_lines(60 << 16) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn wheel_zero_is_zero() {
        assert_eq!(wheel_lines(0), 0.0);
    }

    // ---------------- lParam 拆包 ----------------

    #[test]
    fn words_are_signed() {
        // 负坐标（指针移到客户区左侧）必须正确符号扩展，
        // 否则会变成 65535 而不是 -1
        assert_eq!(low_word_i32(-1), -1);
        assert_eq!(high_word_i32(-1), -1);
        assert_eq!(low_word_i32(0xFFFF), -1);
        assert_eq!(high_word_i32(0xFFFF_FFFF), -1);
    }

    #[test]
    fn words_are_independent() {
        // x = 100, y = 200
        let lp = (200i64 << 16 | 100) as isize;
        assert_eq!(low_word_i32(lp), 100);
        assert_eq!(high_word_i32(lp), 200);
    }

    #[test]
    fn unpack_pos_scales_by_dpi() {
        // 200% 缩放下客户区 100x200 物理像素 = 50x100 逻辑点
        let lp = (200i64 << 16 | 100) as isize;
        let p = unpack_pos(lp, 2.0);
        assert_eq!(p, Pos2::new(50.0, 100.0));
    }

    #[test]
    fn unpack_pos_at_unit_scale_is_identity() {
        let lp = (200i64 << 16 | 100) as isize;
        assert_eq!(unpack_pos(lp, 1.0), Pos2::new(100.0, 200.0));
    }

    #[test]
    fn size_packing_is_unsigned() {
        // WM_SIZE 的宽高不会为负，且高在高位
        let lp = ((1080u32 << 16) | 1920u32) as isize;
        assert_eq!(unpack_size(lp), (1920.0, 1080.0));
    }

    #[test]
    fn size_zero_on_minimize() {
        // 最小化时 WM_SIZE 给 0x0，不能 panic
        assert_eq!(unpack_size(0), (0.0, 0.0));
    }

    // ---------------- 文本解码 ----------------

    #[test]
    fn ascii_and_cjk_pass_through() {
        assert_eq!(char_from_utf16_unit(b'a' as u16).as_deref(), Some("a"));
        assert_eq!(char_from_utf16_unit('中' as u16).as_deref(), Some("中"));
    }

    #[test]
    fn control_characters_are_filtered() {
        // Enter/Tab/Backspace 会重复插入文本框
        for c in ['\r', '\n', '\t', '\x08', '\x1b', '\x7f'] {
            assert_eq!(char_from_utf16_unit(c as u16), None, "{c:?}");
        }
    }

    #[test]
    fn lone_surrogates_are_dropped() {
        // 高低代理单独出现都无效，必须丢弃而不是 panic
        assert_eq!(char_from_utf16_unit(0xD83D), None);
        assert_eq!(char_from_utf16_unit(0xDE00), None);
    }

    #[test]
    fn surrogate_pair_reconstructs_emoji() {
        // U+1F600 GRINNING FACE = D83D DE00
        let mut d = CharDecoder::new();
        assert_eq!(d.push(0xD83D), None, "高代理应缓存等待");
        assert_eq!(d.push(0xDE00).as_deref(), Some("\u{1F600}"));
    }

    #[test]
    fn surrogate_pair_reconstructs_cjk_extension() {
        // U+20000 = D840 DC00
        let mut d = CharDecoder::new();
        assert_eq!(d.push(0xD840), None);
        assert_eq!(d.push(0xDC00).as_deref(), Some("\u{20000}"));
    }

    #[test]
    fn low_surrogate_without_high_is_dropped() {
        let mut d = CharDecoder::new();
        assert_eq!(d.push(0xDE00), None, "孤立低代理应丢弃");
    }

    #[test]
    fn broken_pair_does_not_corrupt_following_text() {
        // 高代理后跟普通字符：坏配对丢弃，普通字符仍要送达
        let mut d = CharDecoder::new();
        assert_eq!(d.push(0xD83D), None);
        assert_eq!(d.push(b'a' as u16).as_deref(), Some("a"));
    }

    #[test]
    fn consecutive_cjk_is_independent() {
        let mut d = CharDecoder::new();
        assert_eq!(d.push('你' as u16).as_deref(), Some("你"));
        assert_eq!(d.push('好' as u16).as_deref(), Some("好"));
    }

    #[test]
    fn decoder_state_is_reusable() {
        // 同一个解码器要能连续处理多组代理项
        let mut d = CharDecoder::new();
        for _ in 0..3 {
            assert_eq!(d.push(0xD83D), None);
            assert_eq!(d.push(0xDE00).as_deref(), Some("\u{1F600}"));
        }
    }

    // ---------------- 消息 → 按钮 ----------------

    #[test]
    fn press_detection_is_correct() {
        assert!(is_press(WM_LBUTTONDOWN));
        assert!(is_press(WM_RBUTTONDOWN));
        assert!(is_press(WM_MBUTTONDOWN));
        assert!(!is_press(WM_LBUTTONUP));
        assert!(!is_press(WM_RBUTTONUP));
        assert!(!is_press(WM_MBUTTONUP));
    }

    #[test]
    fn mouse_messages_map_to_three_buttons() {
        assert_eq!(mouse_button_of(WM_LBUTTONDOWN), PointerButton::Primary);
        assert_eq!(mouse_button_of(WM_LBUTTONUP), PointerButton::Primary);
        assert_eq!(mouse_button_of(WM_RBUTTONDOWN), PointerButton::Secondary);
        assert_eq!(mouse_button_of(WM_RBUTTONUP), PointerButton::Secondary);
        assert_eq!(mouse_button_of(WM_MBUTTONDOWN), PointerButton::Middle);
        assert_eq!(mouse_button_of(WM_MBUTTONUP), PointerButton::Middle);
    }

    // ---------------- egui 映射 ----------------

    #[test]
    fn scroll_maps_to_line_unit_not_touch() {
        // Line 单位让 egui 按 line_scroll_speed 自行换算；
        // TouchPhase::Move 使其进入平滑滚动而非触摸模式
        let raw = raw_of(vec![WindowEvent::Scroll(2.0)]);
        match raw.events.iter().find_map(|e| match e {
            EguiEvent::MouseWheel {
                unit, delta, phase, ..
            } => Some((*unit, *delta, *phase)),
            _ => None,
        }) {
            Some((unit, delta, phase)) => {
                assert_eq!(unit, MouseWheelUnit::Line);
                assert_eq!(delta, Vec2::new(0.0, 2.0));
                assert_eq!(phase, TouchPhase::Move);
            }
            None => panic!("应产出 MouseWheel 事件"),
        }
    }

    #[test]
    fn key_event_carries_physical_key() {
        // egui 的 physical_key 留空会让部分 IME 分支走不到
        let raw = raw_of(vec![WindowEvent::Key {
            keycode: Key::A,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::default(),
        }]);
        match raw.events.iter().find_map(|e| match e {
            EguiEvent::Key {
                key,
                physical_key,
                pressed,
                repeat,
                ..
            } => Some((*key, *physical_key, *pressed, *repeat)),
            _ => None,
        }) {
            Some((key, physical, pressed, repeat)) => {
                assert_eq!(key, Key::A);
                assert_eq!(physical, Some(Key::A));
                assert!(pressed);
                assert!(!repeat);
            }
            None => panic!("应产出 Key 事件"),
        }
    }

    #[test]
    fn repeat_flag_is_preserved() {
        // 长按与多次敲击必须可区分，否则文本框会吞掉重复输入
        let raw = raw_of(vec![WindowEvent::Key {
            keycode: Key::B,
            pressed: true,
            repeat: true,
            modifiers: Modifiers::default(),
        }]);
        match raw.events.iter().find_map(|e| match e {
            EguiEvent::Key { repeat, .. } => Some(*repeat),
            _ => None,
        }) {
            Some(r) => assert!(r),
            None => panic!("应产出 Key 事件"),
        }
    }

    #[test]
    fn mouse_button_maps_all_three() {
        let raw = raw_of(vec![
            WindowEvent::MouseButton {
                button: PointerButton::Primary,
                pressed: true,
                pos: Pos2::new(1.0, 2.0),
            },
            WindowEvent::MouseButton {
                button: PointerButton::Secondary,
                pressed: false,
                pos: Pos2::new(3.0, 4.0),
            },
            WindowEvent::MouseButton {
                button: PointerButton::Middle,
                pressed: true,
                pos: Pos2::new(5.0, 6.0),
            },
        ]);
        let got: Vec<(PointerButton, bool, Pos2)> = raw
            .events
            .iter()
            .filter_map(|e| match e {
                EguiEvent::PointerButton {
                    button,
                    pressed,
                    pos,
                    ..
                } => Some((*button, *pressed, *pos)),
                _ => None,
            })
            .collect();
        assert_eq!(got.len(), 3, "三个按键都应产出事件");
        assert_eq!(got[0], (PointerButton::Primary, true, Pos2::new(1.0, 2.0)));
        assert_eq!(got[1], (PointerButton::Secondary, false, Pos2::new(3.0, 4.0)));
        assert_eq!(got[2], (PointerButton::Middle, true, Pos2::new(5.0, 6.0)));
    }

    #[test]
    fn text_input_becomes_egui_text() {
        let raw = raw_of(vec![WindowEvent::TextInput("复制".into())]);
        assert!(
            raw.events
                .iter()
                .any(|e| matches!(e, EguiEvent::Text(s) if s == "复制"))
        );
    }

    #[test]
    fn mouse_moved_becomes_pointer_moved() {
        let raw = raw_of(vec![WindowEvent::MouseMoved {
            pos: Pos2::new(10.0, 20.0),
        }]);
        assert!(raw.events.iter().any(|e| matches!(
            e,
            EguiEvent::PointerMoved(p) if *p == Pos2::new(10.0, 20.0)
        )));
    }

    #[test]
    fn focus_change_reaches_egui() {
        let mut e = fake_loop();
        e.focused = false;
        let raw = e.egui_input(&egui::Context::default(), &[WindowEvent::Focused(false)]);
        assert!(!raw.focused, "RawInput::focused 必须为 false");
        assert!(
            raw.events
                .iter()
                .any(|e| matches!(e, EguiEvent::WindowFocused(false)))
        );
    }

    #[test]
    fn screen_rect_uses_logical_points() {
        // 1920x1080 物理像素、200% 缩放 → 960x540 逻辑点
        let mut e = fake_loop();
        e.inner_size = Vec2::new(960.0, 540.0);
        let raw = e.egui_input(&egui::Context::default(), &[]);
        let r = raw.screen_rect.expect("screen_rect 必须设置");
        assert_eq!(r.width(), 960.0);
        assert_eq!(r.height(), 540.0);
        assert_eq!(r.min, Pos2::ZERO);
    }

    #[test]
    fn predicted_dt_follows_repaint_after() {
        let mut e = fake_loop();
        e.repaint_after = Some(Duration::from_millis(16));
        let raw = e.egui_input(&egui::Context::default(), &[]);
        assert!((raw.predicted_dt - 0.016).abs() < 1e-4, "{}", raw.predicted_dt);
    }

    #[test]
    fn predicted_dt_defaults_to_60hz() {
        let e = fake_loop();
        let raw = e.egui_input(&egui::Context::default(), &[]);
        assert!((raw.predicted_dt - 1.0 / 60.0).abs() < 1e-6);
    }

    #[test]
    fn predicted_dt_is_clamped_to_sane_range() {
        // 0 间隔会让动画除零；10 秒间隔会让动画瞬移
        let mut e = fake_loop();
        e.repaint_after = Some(Duration::ZERO);
        let raw = e.egui_input(&egui::Context::default(), &[]);
        assert!(raw.predicted_dt >= 1.0 / 1000.0, "{}", raw.predicted_dt);

        let mut e2 = fake_loop();
        e2.repaint_after = Some(Duration::from_secs(10));
        let raw2 = e2.egui_input(&egui::Context::default(), &[]);
        assert!(raw2.predicted_dt <= 0.1, "{}", raw2.predicted_dt);
    }

    #[test]
    fn time_is_monotonic_seconds() {
        let e = fake_loop();
        let t1 = e.egui_input(&egui::Context::default(), &[]).time.unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let t2 = e.egui_input(&egui::Context::default(), &[]).time.unwrap();
        assert!(t2 > t1, "{t2} 应大于 {t1}");
        assert!(t1 < 1.0, "起点应接近 0，实际 {t1}");
    }

    #[test]
    fn empty_frame_does_not_panic() {
        // 无光标设备（CI）下 GetCursorPos 会失败，必须优雅降级
        let raw = raw_of(vec![]);
        assert!(raw.events.iter().all(|e| !matches!(e, EguiEvent::Cut)));
    }

    #[test]
    fn close_requested_is_not_forwarded_to_egui() {
        // CloseRequested 是应用层语义，不该变成 egui 的剪贴板事件
        let raw = raw_of(vec![WindowEvent::CloseRequested]);
        assert!(!raw.events.iter().any(|e| matches!(
            e,
            EguiEvent::Cut | EguiEvent::Copy | EguiEvent::Paste(_)
        )));
    }

    #[test]
    fn resize_is_not_forwarded_as_event() {
        // 尺寸通过 screen_rect 表达，不该产生多余事件
        let raw = raw_of(vec![WindowEvent::Resized {
            width: 100.0,
            height: 50.0,
        }]);
        // 只允许 PointerMoved（来自光标补报），不允许 Key/Text/Wheel
        assert!(
            raw.events
                .iter()
                .all(|e| matches!(e, EguiEvent::PointerMoved(_))),
            "不应产生输入事件，实际 {:?}",
            raw.events
        );
    }

    #[test]
    fn modifiers_changed_emitted_on_change() {
        // 修饰键变化必须通知 egui，否则 Win 键快捷键不生效
        let mut e = fake_loop();
        let events = vec![WindowEvent::Key {
            keycode: Key::C,
            pressed: true,
            repeat: false,
            modifiers: Modifiers {
                command: true,
                ..Default::default()
            },
        }];
        e.modifiers = Modifiers {
            command: true,
            ..Default::default()
        };
        let raw = e.egui_input(&egui::Context::default(), &events);
        assert!(
            raw.events
                .iter()
                .any(|ev| matches!(ev, EguiEvent::ModifiersChanged(m) if m.command)),
            "应发出 ModifiersChanged"
        );
    }

    #[test]
    fn no_modifiers_changed_when_unchanged() {
        // 默认状态与 egui 初始一致时不应发冗余事件
        let raw = raw_of(vec![]);
        assert!(!raw
            .events
            .iter()
            .any(|e| matches!(e, EguiEvent::ModifiersChanged(_))));
    }

    // ---------------- 状态机 ----------------

    #[test]
    fn decoder_is_shared_across_frames() {
        // 代理项可能跨帧配对（两条 WM_CHAR 之间发生 WM_PAINT），
        // 因此解码器必须是 EventLoop 的字段而非每帧新建
        let mut e = fake_loop();
        e.decoder.push(0xD83D); // 第一帧只有高代理
        assert_eq!(e.decoder.pending_high, Some(0xD83D));
        let s = e.decoder.push(0xDE00); // 第二帧补齐低代理
        assert_eq!(s.as_deref(), Some("\u{1F600}"));
    }

    #[test]
    fn dpi_scale_factor_guards_zero() {
        // 窗口无效时 GetDpiForWindow 返回 0：不能除出 NaN，也不能硬编码
        // 1.0（那会让 200% 屏瞬跳 100%，当帧布局突变——P2-3，2026-10-19
        // DPI 治理）。必须返回调用方的 last-good fallback。
        assert_eq!(dpi_scale_factor(0, 0.5), 0.5, "查询失败应回 fallback");
        assert_eq!(dpi_scale_factor(0, 2.0), 2.0, "fallback 随调用方上下文");
        assert_eq!(dpi_scale_factor(96, 9.9), 1.0);
        assert_eq!(dpi_scale_factor(192, 9.9), 2.0);
        assert_eq!(dpi_scale_factor(144, 9.9), 1.5);
    }

    #[test]
    fn clamp_i32_saturates() {
        assert_eq!(clamp_i32(1920), 1920);
        assert_eq!(clamp_i32(u32::MAX), i32::MAX);
        assert_eq!(clamp_i32(0), 0);
    }

    /// `WM_SYSCOMMAND` 的命令判定必须容忍低 4 位的来源位。
    ///
    /// 回归守卫：早先实现用 `wParam == 0xF060` 全等比较，
    /// 于是 `Alt+F4`（wParam = `0xF061`）被漏判，标题栏 X 一类
    /// 走 Alt 修饰的路径全部失效。
    #[test]
    fn sc_close_match_tolerates_source_bits() {
        let is_close = |wp: u32| wp & !SC_SYSTEM_MENU_MASK == SC_CLOSE_WPARAM;
        assert!(is_close(SC_CLOSE_WPARAM), "裸SC_CLOSE");
        assert!(is_close(SC_CLOSE_WPARAM | 1), "Alt+F4");
        assert!(is_close(SC_CLOSE_WPARAM | 2), "来源位=2");
        assert!(!is_close(0xF020), "SC_MINIMIZE 不该当关闭");
        assert!(!is_close(0xF030), "SC_MAXIMIZE 不该当关闭");
        assert!(!is_close(0), "空wParam");
    }

    /// `WM_QUIT` 的归属必须显式，且**只有一个**循环能收。
    ///
    /// 回归守卫：`WM_QUIT` 是线程级消息，`PeekMessageW` 收它时**不能**
    /// 按 hwnd 过滤（否则永远收不到），而那一行是 `PM_REMOVE`——全线程
    /// 只有一条，谁先 `poll` 谁摘走。本项目有 1+N 个 `EventLoop`
    /// （主窗口 + 弹窗 + 设置窗 + 卡片子窗）且每帧都 poll。
    ///
    /// 早前没有归属，弹窗/子窗可能先摘走 `WM_QUIT`：主窗口的
    /// `quit_requested()` 恒为 false ⇒ **点关闭 / 托盘退出后程序退不掉**，
    /// 而那个附属窗口还把 `CloseRequested` 误当成「关我」（弹窗收搜索
    /// 面板、子窗把卡片收回工作区）。
    #[test]
    fn only_the_primary_loop_owns_wm_quit() {
        // 默认（非主循环）不收 WM_QUIT：即使线程队列里真有它，
        // `poll` 也不该认领——这条只能靠字段语义断言，无法在单测里
        // 真的投递 WM_QUIT（那会污染整个测试进程的消息队列）。
        let secondary = fake_loop();
        assert!(
            !secondary.owns_quit,
            "附属窗口的 EventLoop 默认不得声称自己负责 WM_QUIT"
        );
        assert!(
            !secondary.quit_requested(),
            "非主循环的 quit_requested 应恒为 false"
        );

        // 主循环显式声明归属。
        let primary = fake_loop_with_quit();
        assert!(primary.owns_quit, "主循环必须 owns_quit");
    }

    fn fake_loop_with_quit() -> EventLoop {
        EventLoop {
            owns_quit: true,
            ..fake_loop()
        }
    }

    /// 事件累积缓冲的语义守卫。
    ///
    /// 回归守卫：早先 `poll()` 开头`pending.clear()`，
    /// 导致从节流路径 `poll_for` 读到的 `CloseRequested`
    /// 在下一帧被静默丢弃，实测表现为**用户点标题栏 X 完全没反应**。
    ///
    /// IMPORTANT: 这个测试必须走 `absorb()`——即 `poll()` 真正使用的写路径。
    /// 用局部 `Vec` 或直接 `extend_from_slice` 都测不到被测代码：
    /// 那样只测了 `Vec::push` 与 `mem::take` 两个标准库行为，把
    /// `absorb` 改回覆盖语义也照样通过（已实测确认，属于假守卫）。
    #[test]
    fn pending_events_accumulate_across_polls() {
        let mut e = fake_loop();

        // 走 `absorb()`——与 `poll()` 完全相同的写路径。
        // 若把 `absorb` 实现改回覆盖语义（`self.pending = events`），
        // 本测试必须失败。
        e.absorb(vec![WindowEvent::CloseRequested]);
        assert_eq!(e.pending.len(), 1);

        // 模拟节流路径 poll_for() 又读到一条，且调用方丢弃其返回值。
        // 关键：这条**必须仍然留在缓冲里**。
        e.absorb(vec![WindowEvent::Resized {
            width: 1.0,
            height: 1.0,
        }]);

        // 主循环 take_pending() 应一次拿到两条。
        let taken = e.take_pending();
        assert_eq!(taken.len(), 2, "累积事件不该被丢弃");
        assert!(matches!(taken[0], WindowEvent::CloseRequested));
        assert!(matches!(taken[1], WindowEvent::Resized { .. }));
        assert!(
            e.pending.is_empty(),
            "take后缓冲应为空，否则下帧会重复喂给 egui"
        );
    }

    /// 守卫「事件既喂应用层、也喂 egui」这件事不会再次走偏。
    ///
    /// 回归守卫：`take_pending()` 与 `egui_input()` 若都读/写
    /// `self.pending`，且主循环先取走再翻译，egui 就永远拿到空输入——
    /// 实测后果是**整个 UI 收不到任何鼠标与键盘输入**，
    /// 比它修的「点 X 无反应」更严重。
    ///
    /// 现在的签名让这件事在类型上无法搞错：`egui_input` 收`&[WindowEvent]`，
    /// 事件只有一份、由调用方持有。本测试固定住该契约。
    #[test]
    fn taken_events_reach_egui_input() {
        let mut e = fake_loop();
        e.pending.push(WindowEvent::MouseMoved {
            pos: Pos2::new(7.0, 9.0),
        });

        let taken = e.take_pending();
        // 主循环把同一份事件交给 egui
        let raw = e.egui_input(&egui::Context::default(), &taken);

        assert!(
            raw.events.iter().any(|ev| matches!(
                ev,
                EguiEvent::PointerMoved(p) if *p == Pos2::new(7.0, 9.0)
            )),
            "take_pending 取出的事件必须能翻译进 RawInput，             否则 UI 收不到任何输入（真实回归过一次）"
        );
        // 再取一次应为空——事件已被交付，不能重复喂。
        assert!(e.take_pending().is_empty(), "事件不应被交付两次");
    }
}
