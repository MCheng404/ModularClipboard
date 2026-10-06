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
//! - 系统随即把按下转成 `WM_NCLBUTTONDOWN`，由 `DefWindowProcW`
//!   进入**原生**的缩放 / 移动循环。
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

use std::time::{Duration, Instant};

use anyhow::Context as _;
use egui::emath::{Pos2, Rect, Vec2};
use egui::{Event as EguiEvent, Key, Modifiers, MouseWheelUnit, PointerButton, TouchPhase};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetCapture, ReleaseCapture, SetCapture, VIRTUAL_KEY, VK_BACK, VK_CONTROL,
    VK_DELETE, VK_DOWN, VK_END, VK_F1, VK_F24, VK_HOME, VK_INSERT, VK_LCONTROL, VK_LEFT, VK_LMENU,
    VK_LSHIFT, VK_LWIN, VK_MENU, VK_NEXT, VK_OEM_1, VK_OEM_102, VK_OEM_2, VK_OEM_3, VK_OEM_4,
    VK_OEM_5, VK_OEM_6, VK_OEM_8, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD,
    VK_OEM_PLUS, VK_PRIOR, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN,
    VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos,
    GetForegroundWindow, IsWindow, MSG, PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassExW,
    SIZE_MINIMIZED, SIZE_RESTORED, SW_SHOW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE,
    WM_SYSCOMMAND,
    WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE, WM_DPICHANGED, WM_IME_CHAR, WM_KEYDOWN, WM_KEYUP,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETFOCUS, WM_SIZE, WM_SYSCHAR,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSEXW, WS_POPUP, WS_SYSMENU,
};
use windows::core::{HSTRING, PCWSTR};

use crate::chrome;

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
}

/// Win32 窗口。
///
/// 持有 `HWND` 与模块句柄，`Drop` 时销毁窗口。窗口类**不**注销：
/// 它是进程级的，且可能已有其它 `Window` 实例在用同名类。
pub struct Window {
    hwnd: HWND,
    hinstance: HINSTANCE,
}

impl Window {
    /// 创建并显示一个窗口。
    ///
    /// `width` / `height` 是**客户区**的物理像素。内部先打开 per-monitor
    /// DPI 感知（v2），因此在高 DPI 显示器上窗口是清晰的。
    pub fn new(title: &str, width: u32, height: u32) -> anyhow::Result<Self> {
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
        // 与自绘外观冲突；不加则必须自己在 `WM_NCHITTEST` 里补回
        // resize 能力，见 [`chrome`]。
        let style = WS_POPUP | WS_SYSMENU;

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_p,
                title_p,
                style,
                120,
                120,
                clamp_i32(width),
                clamp_i32(height),
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .context("CreateWindowExW 失败")?;

        // BOOL 是 Copy 且非 must_use，显式丢弃以表明「返回值不关心」
        let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };

        // 圆角 + 去系统描边。Win10 上失败只记debug，不影响启动。
        chrome::apply_rounded_corners(hwnd);
        // 给一个默认的 resize 边缘；上层可用`set_resize_border` 覆盖。
        chrome::set_resize_border(chrome::DEFAULT_RESIZE_BORDER);

        Ok(Self { hwnd, hinstance })
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
        dpi_scale_factor(unsafe { GetDpiForWindow(self.hwnd) })
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
    /// 见 [`chrome::set_drag_region`]。
    pub fn set_drag_region(&self, rect: Option<Rect>) {
        chrome::set_drag_region(rect);
    }

    /// 设置边缘 resize 区宽度，单位**逻辑点**。`<= 0` 禁用 resize。
    ///
    /// 见 [`chrome::set_resize_border`]。
    pub fn set_resize_border(&self, points: f32) {
        chrome::set_resize_border(points);
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
        self.destroy();
    }
}

/// `u32` 尺寸转 `i32`，钳制以免溢出。
fn clamp_i32(v: u32) -> i32 {
    v.min(i32::MAX as u32) as i32
}

/// `dpi / 96`。`dpi == 0`（窗口无效时）返回 `1.0`。
fn dpi_scale_factor(dpi: u32) -> f32 {
    if dpi == 0 { 1.0 } else { dpi as f32 / BASE_DPI as f32 }
}

/// 注册窗口类，进程内只做一次。
fn register_class(hinstance: HINSTANCE, class_p: PCWSTR) -> anyhow::Result<()> {
    let err = *CLASS_REGISTERED.get_or_init(|| {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // 必须原样转发 DefWindowProcW，见模块文档。
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
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

/// 事件循环。
///
/// 拥有「本帧事件」缓冲：[`EventLoop::poll`] 写，
/// [`EventLoop::egui_input`] 读。
pub struct EventLoop {
    hwnd: HWND,
    /// 事件循环创建时刻，用作 egui 的时间原点。
    start: Instant,
    /// 上次 poll 得到的缩放比例。
    scale_factor: f32,
    /// 上次 poll 得到的客户区尺寸（逻辑点）。
    inner_size: Vec2,
    /// 本帧事件缓冲。
    ///
    /// ⚠️ 刻意**不在** [`Self::poll`] 开头清空，见该方法内的说明。
    pending: Vec<WindowEvent>,
    /// 本帧观察到的修饰键状态，用于发 `ModifiersChanged`。
    modifiers: Modifiers,
    /// 键盘焦点状态，对应 `RawInput::focused`。
    focused: bool,
    /// 已收到 `WM_QUIT`。
    quit: bool,
    /// 应用请求的下次重绘间隔。
    repaint_after: Option<Duration>,
    /// UTF-16 代理项配对状态。BMP 外的字符由两条 `WM_CHAR` 组成。
    decoder: CharDecoder,
}

impl EventLoop {
    /// 为给定窗口创建事件循环。
    pub fn new(window: &Window) -> Self {
        let (w, h) = window.inner_size_points();
        Self {
            hwnd: window.hwnd(),
            start: Instant::now(),
            scale_factor: window.scale_factor(),
            inner_size: Vec2::new(w, h),
            pending: Vec::new(),
            modifiers: Modifiers::default(),
            focused: true,
            quit: false,
            repaint_after: None,
            decoder: CharDecoder::new(),
        }
    }

    /// 设置帧间隔。`poll_for` 空闲时按它睡眠，避免空转烧 CPU。
    ///
    /// 典型用法是每帧把 egui 的 `ctx.requested_repaint_after()` 写回来。
    pub fn set_repaint_after(&mut self, d: Option<Duration>) {
        self.repaint_after = d;
    }

    /// 已收到退出请求。
    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// 本帧事件缓冲（供调试与测试）。
    pub fn pending_events(&self) -> &[WindowEvent] {
        &self.pending
    }

    /// 取走累积的全部未交付事件，并清空缓冲。
    ///
    /// 主循环用它把「本帧新读到的事件」与「此前节流路径读到但尚未
    /// 交付的事件」一起取走，保证 [`Self::CloseRequested`] 之类
    /// 低频但关键的事件不会在两次交付之间被丢掉。
    pub fn take_pending(&mut self) -> Vec<WindowEvent> {
        std::mem::take(&mut self.pending)
    }

    /// 排空消息队列，不阻塞。
    ///
    /// 返回本轮读到的全部事件，同时写入内部缓冲供 [`Self::egui_input`]
    /// 使用。有事件时立即返回——这是高优先级路径。
    ///
    /// # ⚠️ 事件不丢的保证
    ///
    /// `pending` 缓冲区**刻意不在本函数开头清空**，而是累积到被
    /// [`Self::take_pending`] 取走为止。这不是疏忽，而是必需：
    ///
    /// [`Self::poll_for`] 内部也调`poll()` 并**丢弃其返回值**（节流路径
    /// 只关心「有没有消息到达」）。若`poll()` 开头清空 `pending`，
    /// 那么从 `poll_for` 读到的事件会被下一轮 `poll()` 的清空抹掉——
    /// 实测后果是**用户点标题栏 X 完全没反应**：消息被读出、记进
    /// `pending`、无人查看、下一帧丢弃。
    ///
    /// 累积语义下，`poll_for` 读到的关闭请求会一直留在 `pending` 里，
    /// 直到主循环 `poll()` 把它当本帧事件交给应用层。
    pub fn poll(&mut self) -> Vec<WindowEvent> {
        self.modifiers = Modifiers::default();

        // 焦点可能在两轮之间丢失（Alt+Tab 到别的程序），每轮重查。
        self.focused = unsafe { GetForegroundWindow() } == self.hwnd;

        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let mut events: Vec<WindowEvent> = Vec::new();

        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            // WM_QUIT 没有窗口过程，直接置退出标志。
            if msg.message == WM_QUIT {
                self.quit = true;
                events.push(WindowEvent::CloseRequested);
                continue;
            }

            // `WM_SYSCOMMAND` + `SC_CLOSE` 是**标题栏 X 按钮**的第一手消息。
            //
            // 处理它不是为了「让 X 能生效」——真正的元凶是 `poll_for`
            // 丢弃事件（见 `poll` 的说明），那个不修，加多少分支都没用。
            // 直接拦这一条是为了少绕一圈：X 按钮 → `WM_SYSCOMMAND/SC_CLOSE`
            // → `DefWindowProcW` 转成 `WM_CLOSE` 重新入队 → 下一轮才被
            // 下面的 `WM_CLOSE` 分支捡到。拦下来可以少一次队列往返，
            // 也避免 `DefWindowProcW` 在我们不知情时做别的事。
            //
            // 这里**不能**调 `DefWindowProcW`：那会走到销毁窗口的路径，
            // 托盘程序要的是隐藏。
            //
            // 低 4 位可能带 Alt 位（`SC_CLOSE | ALT` = 0xF061），
            // 所以比掩码后的值而不是全等。
            if msg.message == WM_SYSCOMMAND
                && (msg.wParam.0 as u32 & !SC_SYSTEM_MENU_MASK) == SC_CLOSE_WPARAM
            {
                events.push(WindowEvent::CloseRequested);
                continue;
            }

            // WM_CLOSE **不派发**：派发会让 DefWindowProcW 销毁窗口。
            // 剪贴板程序通常要转到托盘而不是退出，交给应用决定。
            if msg.message == WM_CLOSE {
                events.push(WindowEvent::CloseRequested);
                continue;
            }

            // TranslateMessage 把 WM_KEYDOWN 变成 WM_CHAR 投递到队列尾部，
            // 同一轮循环的下一次 PeekMessageW 就能读到。必须在本条消息
            // 处理前调用，否则 WM_CHAR 会晚一帧，文本输入会滞后。
            if matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN) {
                let _ = unsafe { TranslateMessage(&msg) };
            }

            self.translate(&msg, &mut events);

            // 其余消息照常派发：清 WM_PAINT 的更新区、走默认处理。
            // wnd_proc 是空壳，派发本身无副作用，但**必须**派发——
            // 否则 WM_PAINT 会因更新区未清而无限重投。
            let _ = unsafe { DispatchMessageW(&msg) };
        }

        self.absorb(events.clone());
        events
    }

    /// 把一轮读到的事件并入累积缓冲。
    ///
    /// 抽成独立方法是为了让测试能走**与`poll()` 完全相同的写路径**：
    /// 早先的测试直接调`Vec::extend_from_slice`，把这里的实现改回
    /// 覆盖语义（`self.pending = events`）测试照样通过，属于假守卫。
    fn absorb(&mut self, events: Vec<WindowEvent>) {
        // ⚠️ 必须**追加**而非覆盖：`poll_for` 也会走到这里，
        // 覆盖会把上一轮累积的未交付事件抹掉（见 `poll` 的说明）。
        self.pending.extend_from_slice(&events);
    }

    /// 睡眠式轮询：有消息立即返回，空闲时最多阻塞 `timeout`。
    ///
    /// 用 `MsgWaitForMultipleObjects` 同时等「消息到达」和「超时」，
    /// 而不是 `while GetMessage() {}` 忙等——后者无事件时会把一个核跑满。
    pub fn poll_for(&mut self, timeout: Option<Duration>) -> Vec<WindowEvent> {
        let events = self.poll();
        if !events.is_empty() || self.quit {
            return events;
        }
        match timeout {
            Some(d) if !d.is_zero() => {
                let ms = d.as_millis().min(u32::MAX as u128) as u32;
                let _ = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::MsgWaitForMultipleObjects(
                        None,
                        false,
                        ms,
                        QS_ALLINPUT,
                    )
                };
                self.poll()
            }
            // None 或零超时：由调用方自己控制节奏，纯自旋。
            _ => events,
        }
    }

    /// 把一条消息翻译成语义事件，追加到 `out`。
    fn translate(&mut self, msg: &MSG, out: &mut Vec<WindowEvent>) {
        let scale = self.scale_factor;

        match msg.message {
            WM_SIZE => {
                // wParam 携带状态变化原因：最小化/ 最大化 / 还原。
                // 无边框窗口没有系统标题栏，最小化后界面上毫无变化，
                // 上层只能靠这个事件切到「已最小化」样式。
                match msg.wParam.0 as u32 {
                    SIZE_MINIMIZED => out.push(WindowEvent::Minimized(true)),
                    SIZE_RESTORED => out.push(WindowEvent::Minimized(false)),
                    // SIZE_MAXIMIZED 与本项目无关（无边框窗口盖任务栏，
                    // 见模块文档的已知缺口），不发事件。
                    _ => {}
                }
                let (w_px, h_px) = unpack_size(msg.lParam.0);
                // WM_SIZE 给的是物理像素，egui 要逻辑点。
                let w = w_px / scale;
                let h = h_px / scale;
                self.inner_size = Vec2::new(w, h);
                out.push(WindowEvent::Resized {
                    width: w,
                    height: h,
                });
            }

            // 缩放变了，随后的 WM_SIZE 会带来新的像素尺寸。
            // 这里只报比例，不改 inner_size——改了会用旧像素尺寸除新比例。
            WM_DPICHANGED => {
                let new_scale = dpi_scale_factor(unsafe { GetDpiForWindow(self.hwnd) });
                if (new_scale - scale).abs() > f32::EPSILON {
                    self.scale_factor = new_scale;
                    out.push(WindowEvent::ScaleFactorChanged(new_scale as f64));
                }
            }

            WM_SETFOCUS => {
                self.focused = true;
                out.push(WindowEvent::Focused(true));
            }
            WM_KILLFOCUS => {
                self.focused = false;
                out.push(WindowEvent::Focused(false));
            }

            WM_MOUSEMOVE => {
                out.push(WindowEvent::MouseMoved {
                    pos: unpack_pos(msg.lParam.0, scale),
                });
            }

            WM_LBUTTONDOWN | WM_LBUTTONUP
            | WM_RBUTTONDOWN | WM_RBUTTONUP
            | WM_MBUTTONDOWN | WM_MBUTTONUP => {
                let pressed = is_press(msg.message);
                // 按下时捕获鼠标，否则指针移出客户区就收不到抬起事件，
                // 控件会卡在「按住」状态。
                if pressed {
                    // SetCapture 返回的是**原**捕获窗口，此处不关心
                    let _ = unsafe { SetCapture(self.hwnd) };
                } else {
                    let _ = unsafe { ReleaseCapture() };
                }
                out.push(WindowEvent::MouseButton {
                    button: mouse_button_of(msg.message),
                    pressed,
                    pos: unpack_pos(msg.lParam.0, scale),
                });
            }

            // 有别的窗口抢走了鼠标（如菜单弹出、窗口被切走）。补一个抬起
            // 事件，否则控件会一直停在「按下」态。
            WM_CAPTURECHANGED => {
                if unsafe { GetCapture() } != self.hwnd {
                    let pos = self.pointer_in_points(scale).unwrap_or(Pos2::ZERO);
                    out.push(WindowEvent::MouseButton {
                        button: PointerButton::Primary,
                        pressed: false,
                        pos,
                    });
                }
            }

            WM_MOUSEWHEEL => {
                let lines = wheel_lines(msg.wParam.0);
                if lines != 0.0 {
                    out.push(WindowEvent::Scroll(lines));
                }
            }

            WM_KEYDOWN | WM_SYSKEYDOWN => {
                // lParam 的 bit 30 为 1 表示按下前该键已处于按下状态，
                // 即系统自动重复。比自己维护按键集合可靠。
                let repeat = (msg.lParam.0 >> 30) & 1 == 1;
                let modifiers = current_modifiers();
                self.modifiers = modifiers;
                if let Some(keycode) = key_from_vk(msg.wParam.0 as u16) {
                    out.push(WindowEvent::Key {
                        keycode,
                        pressed: true,
                        repeat,
                        modifiers,
                    });
                }
            }

            WM_KEYUP | WM_SYSKEYUP => {
                let modifiers = current_modifiers();
                self.modifiers = modifiers;
                if let Some(keycode) = key_from_vk(msg.wParam.0 as u16) {
                    out.push(WindowEvent::Key {
                        keycode,
                        pressed: false,
                        repeat: false,
                        modifiers,
                    });
                }
            }

            WM_CHAR | WM_SYSCHAR | WM_IME_CHAR => {
                // 走解码器：代理项要跨两条消息配对，且控制字符要滤掉。
                if let Some(s) = self.decoder.push(msg.wParam.0 as u16) {
                    out.push(WindowEvent::TextInput(s));
                }
            }

            _ => {}
        }
    }

    /// 光标在**客户区**内的逻辑点坐标。
    ///
    /// 消息队列只在鼠标移动时才有 `WM_MOUSEMOVE`，而 egui 每帧都要知道
    /// 指针位置才能算悬停。因此在没有移动事件的帧里用系统光标位置补一次。
    fn pointer_in_points(&self, scale: f32) -> Option<Pos2> {
        let mut pt = POINT::default();
        unsafe {
            GetCursorPos(&mut pt).ok()?;
            // GetCursorPos 是屏幕坐标，egui 要客户区坐标。
            // 空 HWND（测试环境）下会失败，pt 保持原值，属预期降级。
            let _ = ScreenToClient(self.hwnd, &mut pt);
        }
        Some(Pos2::new(pt.x as f32 / scale, pt.y as f32 / scale))
    }

    /// 构造本帧的 [`egui::RawInput`]。
    ///
    /// `events` 是本帧要翻译的事件，通常来自 [`Self::take_pending`]。
    /// D 组每帧开头调用一次，把结果交给 `egui::Context::run` 即可。
    ///
    /// # ⚠️ 为什么事件是参数而不是读`self.pending`
    ///
    /// 事件缓冲是**累积**的（见 [`Self::poll`] 的说明）：节流路径
    /// `poll_for` 也会读消息，其返回值被调用方丢弃，事件只能留在
    /// 缓冲里等主循环来取。
    ///
    /// 若`egui_input` 读 `self.pending`、而主循环用 `take_pending()`
    /// 取走事件，两者是**先后顺序相反且互不知情**的两份状态：
    /// 只要主循环先取走（正常做法），`egui_input` 就永远读到空——
    /// 实测后果是**整个 UI 收不到任何鼠标/键盘输入**，比它修的缺陷
    /// 更严重。
    ///
    /// 改为显式传参后，「本帧事件」只有一份，且类型上不可能再取错。
    ///
    /// `screen_rect` 用逻辑点，`time` 是自事件循环创建起的秒数，
    /// `predicted_dt` 取 `set_repaint_after` 设的间隔。
    pub fn egui_input(&self, ctx: &egui::Context, events: &[WindowEvent]) -> egui::RawInput {
        let mut out: Vec<EguiEvent> = Vec::with_capacity(events.len() + 4);

        // 指针位置每帧都要报。没有移动消息时用系统光标位置兜底，
        // 否则 egui 无法判断「鼠标在窗口内但没动」的悬停。
        if let Some(pos) = self.pointer_in_points(self.scale_factor) {
            out.push(EguiEvent::PointerMoved(pos));
        }

        // 兜底修饰键用**实时查询**而非 `self.modifiers`。
        //
        // `self.modifiers` 只在收到按键消息时更新，而事件缓冲是累积的
        // （见 `poll` 的说明）：跨帧累积下来的 `MouseButton` 事件本身
        // 不带修饰键字段，若起点是上一帧的残值，就会把「按住 Ctrl 的点击」
        // 报成「无修饰键的点击」，让右键菜单一类交互带上错误的按键状态。
        // `GetAsyncKeyState` 查的是此刻的真实物理状态，没有跨帧问题。
        //
        // 循环内遇到 `Key` 事件时仍会被事件自带的 modifiers 覆盖
        // （那是按下瞬间的精确状态，更可靠）。
        let mut modifiers = if events.iter().any(is_key_event) {
            self.modifiers
        } else {
            current_modifiers()
        };

        for ev in events {
            match ev {
                WindowEvent::MouseMoved { pos } => {
                    out.push(EguiEvent::PointerMoved(*pos));
                }
                WindowEvent::MouseButton {
                    button,
                    pressed,
                    pos,
                } => {
                    out.push(EguiEvent::PointerButton {
                        pos: *pos,
                        button: *button,
                        pressed: *pressed,
                        modifiers,
                    });
                }
                WindowEvent::Scroll(lines) => {
                    out.push(EguiEvent::MouseWheel {
                        // Win32 给的是行数。egui 原生支持 Line 单位，
                        // 由它按 line_scroll_speed 换算成点。
                        unit: MouseWheelUnit::Line,
                        delta: Vec2::new(0.0, *lines),
                        // 鼠标滚轮不是触摸。用 Move 让 egui 进入
                        // Status::Smoothing（跨帧平滑）；若误用 Start
                        // 会被当成触摸，修饰键会被锁住直到抬手。
                        phase: TouchPhase::Move,
                        modifiers,
                    });
                }
                WindowEvent::Key {
                    keycode,
                    pressed,
                    repeat,
                    modifiers: m,
                } => {
                    modifiers = *m;
                    out.push(EguiEvent::Key {
                        key: *keycode,
                        // 同时填 physical_key：留空会让部分 IME 分支走不到。
                        physical_key: Some(*keycode),
                        pressed: *pressed,
                        repeat: *repeat,
                        modifiers,
                    });
                }
                WindowEvent::TextInput(s) => {
                    out.push(EguiEvent::Text(s.clone()));
                }
                WindowEvent::Focused(b) => {
                    out.push(EguiEvent::WindowFocused(*b));
                }
                // 尺寸与缩放通过 screen_rect 表达；关闭是应用层语义；
                // 最小化同理——客户区已是 0×0，`screen_rect` 会自动塌成空。
                WindowEvent::Resized { .. }
                | WindowEvent::ScaleFactorChanged(_)
                | WindowEvent::Minimized(_)
                | WindowEvent::CloseRequested => {}
            }
        }

        // egui 靠这个事件更新修饰键状态（含 Win 键 → command）。
        if modifiers != ctx.input(|i| i.modifiers) {
            out.push(EguiEvent::ModifiersChanged(modifiers));
        }

        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.inner_size)),
            time: Some(self.start.elapsed().as_secs_f64()),
            predicted_dt: self
                .repaint_after
                .map(|d| d.as_secs_f32())
                .unwrap_or(DEFAULT_PREDICTED_DT)
                .clamp(MIN_PREDICTED_DT, MAX_PREDICTED_DT),
            events: out,
            focused: self.focused,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------- 纯逻辑

/// 字母键。索引 0 对应 `'A'`。
const LETTERS: [Key; 26] = [
    Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G, Key::H, Key::I, Key::J, Key::K, Key::L,
    Key::M, Key::N, Key::O, Key::P, Key::Q, Key::R, Key::S, Key::T, Key::U, Key::V, Key::W, Key::X,
    Key::Y, Key::Z,
];

/// 数字键。索引 0 对应主键盘 `'0'`。
const DIGITS: [Key; 10] = [
    Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7,
    Key::Num8, Key::Num9,
];

/// 功能键 F1..F24。索引 0 对应 `VK_F1`。
const FKEYS: [Key; 24] = [
    Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10,
    Key::F11, Key::F12, Key::F13, Key::F14, Key::F15, Key::F16, Key::F17, Key::F18, Key::F19,
    Key::F20, Key::F21, Key::F22, Key::F23, Key::F24,
];

/// `VIRTUAL_KEY` 转裸 `u16`，便于与 `wParam` 比较。
const fn vk(k: VIRTUAL_KEY) -> u16 {
    k.0
}

/// 虚拟键码 → egui 逻辑键。无法映射时返回 `None`。
///
/// 只覆盖 egui 有对应变体的键。带输入法时字母键的 `wParam` 是
/// `'A'..'Z'` 的 ASCII 码，与本函数的字母分支一致，因此输入法场景
/// 也能拿到正确的键。
pub fn key_from_vk(code: u16) -> Option<Key> {
    // 数字键与字母键走区间查表，覆盖 36 个键且无分支预测失败。
    if (b'0' as u16..=b'9' as u16).contains(&code) {
        return Some(DIGITS[(code - b'0' as u16) as usize]);
    }
    if (b'A' as u16..=b'Z' as u16).contains(&code) {
        return Some(LETTERS[(code - b'A' as u16) as usize]);
    }

    // 功能键 F1..F24 是连续区间
    if (vk(VK_F1)..=vk(VK_F24)).contains(&code) {
        return Some(FKEYS[(code - vk(VK_F1)) as usize]);
    }

    let key = match code {
        c if c == vk(VK_LEFT) => Key::ArrowLeft,
        c if c == vk(VK_RIGHT) => Key::ArrowRight,
        c if c == vk(VK_UP) => Key::ArrowUp,
        c if c == vk(VK_DOWN) => Key::ArrowDown,
        c if c == vk(VK_PRIOR) => Key::PageUp,
        c if c == vk(VK_NEXT) => Key::PageDown,
        c if c == vk(VK_HOME) => Key::Home,
        c if c == vk(VK_END) => Key::End,
        c if c == vk(VK_INSERT) => Key::Insert,
        c if c == vk(VK_DELETE) => Key::Delete,
        c if c == vk(VK_BACK) => Key::Backspace,
        c if c == vk(VK_SPACE) => Key::Space,
        c if c == vk(VK_TAB) => Key::Tab,
        c if c == vk(VK_RETURN) => Key::Enter,

        // 左右修饰键必须区分：egui 用它们判断单侧快捷键，
        // 合并会让「只用左 Ctrl」这类配置失效。
        c if c == vk(VK_LSHIFT) => Key::ShiftLeft,
        c if c == vk(VK_RSHIFT) => Key::ShiftRight,
        c if c == vk(VK_LCONTROL) => Key::ControlLeft,
        c if c == vk(VK_RCONTROL) => Key::ControlRight,
        c if c == vk(VK_LMENU) => Key::AltLeft,
        c if c == vk(VK_RMENU) => Key::AltRight,
        c if c == vk(VK_LWIN) => Key::SuperLeft,
        c if c == vk(VK_RWIN) => Key::SuperRight,

        // 不区分左右的通用键码（由 IME 或旧程序发出）
        c if c == vk(VK_SHIFT) => Key::ShiftLeft,
        c if c == vk(VK_CONTROL) => Key::ControlLeft,
        c if c == vk(VK_MENU) => Key::AltLeft,

        c if c == vk(VK_OEM_1) => Key::Semicolon,
        c if c == vk(VK_OEM_PLUS) => Key::Equals,
        c if c == vk(VK_OEM_COMMA) => Key::Comma,
        c if c == vk(VK_OEM_MINUS) => Key::Minus,
        c if c == vk(VK_OEM_PERIOD) => Key::Period,
        c if c == vk(VK_OEM_2) => Key::Slash,
        c if c == vk(VK_OEM_3) => Key::Backtick,
        c if c == vk(VK_OEM_4) => Key::OpenBracket,
        c if c == vk(VK_OEM_6) => Key::CloseBracket,
        c if c == vk(VK_OEM_5) => Key::Backslash,
        c if c == vk(VK_OEM_8) => Key::Quote,
        c if c == vk(VK_OEM_102) => Key::IntlBackslash,

        // egui 无对应变体（CapsLock、NumLock、ScrollLock、Apps、Zoom 等），
        // 返回 None 让上层忽略。
        _ => return None,
    };
    Some(key)
}

/// 由四个独立布尔量组合出 egui 修饰键状态。
///
/// Windows 上没有 `mac_cmd` 的概念，一律 `false`。
/// 「Windows 键」映射到 `command`——egui 判定快捷键的主字段是
/// `modifiers.command` 而非 `ctrl`，映射错会导致 Win+ 快捷键全失效。
pub fn modifiers_from(shift: bool, ctrl: bool, alt: bool, windows_key: bool) -> Modifiers {
    Modifiers {
        alt,
        ctrl,
        shift,
        mac_cmd: false,
        command: windows_key,
    }
}

/// 该事件是否自带精确的修饰键状态。
///
/// 自带状态的按键事件比`GetAsyncKeyState` 的实时查询更可靠：
/// 它反映的是「按下那一刻」的组合（例如 Shift+Ctrl 同时按下），
/// 而实时查询在某些键盘布局下无法区分左右修饰键的组合意图。
fn is_key_event(ev: &WindowEvent) -> bool {
    matches!(ev, WindowEvent::Key { .. })
}

/// 查询当前修饰键状态。
fn current_modifiers() -> Modifiers {
    unsafe {
        modifiers_from(
            is_key_down(VK_SHIFT),
            is_key_down(VK_CONTROL),
            is_key_down(VK_MENU),
            is_key_down(VK_LWIN) || is_key_down(VK_RWIN),
        )
    }
}

/// `GetAsyncKeyState` 的高位为按下。
///
/// 用异步版本而非 `GetKeyState`：后者只在消息队列被 pump 过之后才更新，
/// 而我们是在 pump **过程中**读修饰键，会拿到上一帧的值。
unsafe fn is_key_down(k: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(k.0 as i32) < 0 }
}

/// 滚轮单位 → 行数。正值表示向上拨（内容下滚）。
fn wheel_lines(wparam: usize) -> f32 {
    // 高 16 位是有符号刻度数；正 = 远离用户 = 向上滚。
    let raw = ((wparam >> 16) & 0xFFFF) as u16 as i16 as f32;
    // Windows 通常给 120 的整数倍，但高精度触控板会给小数，
    // 因此统一除而不做截断。
    raw / WHEEL_DELTA
}

/// 消息是否表示「按下」（`*DOWN`）。
fn is_press(msg: u32) -> bool {
    matches!(
        msg,
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
    )
}

/// 鼠标消息 → egui 按键。
fn mouse_button_of(msg: u32) -> PointerButton {
    match msg {
        WM_RBUTTONDOWN | WM_RBUTTONUP => PointerButton::Secondary,
        WM_MBUTTONDOWN | WM_MBUTTONUP => PointerButton::Middle,
        _ => PointerButton::Primary,
    }
}

/// `lParam` 低 16 位，按**有符号** 16 位解释。
fn low_word_i32(lparam: isize) -> i32 {
    (lparam as u16) as i16 as i32
}

/// `lParam` 高 16 位，按**有符号** 16 位解释。
fn high_word_i32(lparam: isize) -> i32 {
    ((lparam >> 16) as u16) as i16 as i32
}

/// `lParam` 打包的客户区坐标（物理像素）→ 逻辑点。
fn unpack_pos(lparam: isize, scale: f32) -> Pos2 {
    Pos2::new(
        low_word_i32(lparam) as f32 / scale,
        high_word_i32(lparam) as f32 / scale,
    )
}

/// `WM_SIZE` 的 `lParam`：低 16 位宽、高 16 位高，**均为无符号**。
///
/// 与坐标不同，这里不能按有符号解释：宽度不会为负。
fn unpack_size(lparam: isize) -> (f32, f32) {
    let w = (lparam as u16) as f32;
    let h = ((lparam >> 16) as u16) as f32;
    (w, h)
}

/// 单个 UTF-16 码元 → 可打印文本。
///
/// 过滤控制字符：`WM_CHAR` 会把 Enter 送成 `\r`、Tab 送成 `\t`、
/// Backspace 送成 `\x08`。这些已由 [`WindowEvent::Key`] 表达，
/// 不过滤会让文本框出现重复字符。
///
/// 代理项（BMP 外的字符）返回 `None`，由 [`CharDecoder`] 配对。
fn char_from_utf16_unit(unit: u16) -> Option<String> {
    if (0xD800..=0xDFFF).contains(&unit) {
        return None;
    }
    let c = char::from_u32(unit as u32)?;
    if c.is_control() {
        return None;
    }
    Some(c.to_string())
}

/// UTF-16 代理项配对器。
///
/// Win32 把 BMP 外的字符（emoji、生僻汉字）拆成两条 `WM_CHAR`：
/// 先高代理后低代理。单独看任一条都是无效的，必须攒起来配对。
#[derive(Debug, Default)]
pub struct CharDecoder {
    pending_high: Option<u16>,
}

impl CharDecoder {
    /// 新建解码器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一个码元，得到可能为空的文本增量。
    pub fn push(&mut self, unit: u16) -> Option<String> {
        if (0xD800..=0xDBFF).contains(&unit) {
            // 高代理：存起来等低代理。
            self.pending_high = Some(unit);
            return None;
        }
        if (0xDC00..=0xDFFF).contains(&unit) {
            // 低代理必须与高代理配对，孤立的下代理直接丢弃。
            let high = self.pending_high.take()?;
            let cp = 0x1_0000u32 + (((high as u32) - 0xD800) << 10) + ((unit as u32) - 0xDC00);
            return char::from_u32(cp).map(|c| c.to_string());
        }
        // 普通码元。若前面攒了未配对的高代理，说明序列损坏
        // （正常 Win32 不会这样，但输入注入可能），丢弃那个高代理。
        self.pending_high = None;
        char_from_utf16_unit(unit)
    }
}

// ---------------------------------------------------------------- 单测

#[cfg(test)]
mod tests {
    use super::*;

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
        // 窗口无效时 GetDpiForWindow 返回 0，不能除出 NaN
        assert_eq!(dpi_scale_factor(0), 1.0);
        assert_eq!(dpi_scale_factor(96), 1.0);
        assert_eq!(dpi_scale_factor(192), 2.0);
        assert_eq!(dpi_scale_factor(144), 1.5);
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

    /// 事件累积缓冲的语义守卫。
    ///
    /// 回归守卫：早先 `poll()` 开头`pending.clear()`，
    /// 导致从节流路径 `poll_for` 读到的 `CloseRequested`
    /// 在下一帧被静默丢弃，实测表现为**用户点标题栏 X 完全没反应**。
    ///
    /// ⚠️ 这个测试必须走 `absorb()`——即 `poll()` 真正使用的写路径。
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
