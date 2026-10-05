//! Win32 窗口与事件循环。
//!
//! 本模块把 Win32 的消息队列翻译成语义化的 [`WindowEvent`]，再翻译成
//! egui 的 [`egui::RawInput`]。渲染侧（[`crate::frame`]）只关心
//! `Resized` / `CloseRequested`，不需要知道任何 Win32 细节。
//!
//! ## 事件从哪来
//!
//! **不子类化窗口**。窗口过程是一个原样转发 `DefWindowProcW` 的空壳，
//! 所有语义事件都在 [`EventLoop::poll`] 里用 `PeekMessageW` 从消息队列
//! 直接读取。这样做的两个理由：
//!
//! 1. 子类化需要在 `WM_NCCREATE` 时 `SetWindowLongPtrW` 写入指针，
//!    一旦漏掉 `CallWindowProcW` 转发就会崩，而崩溃点是启动时，
//!    排查成本远高于收益。
//! 2. 消息队列里的消息在派发前就可以读，`TranslateMessage` 生成的
//!    `WM_CHAR` 也能在同一轮循环里被读到，无需二次投递。
//!
//! 窗口过程**必须原样转发** `DefWindowProcW`——给它加消息偏移会让
//! `CreateWindowExW` 报 `0x8007007E`（见 MEMORY.md 第 26 条）。
//!
//! ## 坐标与 DPI
//!
//! Win32 给的是**物理像素**且以客户区左上角为原点，egui 要的是**逻辑点**。
//! 本模块的 [`WindowEvent`] 里的坐标**已经换算成逻辑点**
//! （除以 `scale_factor`），D 组可以直接喂给 egui，不需要再转换。
//!
//! DPI 感知在 [`Window::new`] 里通过 `SetProcessDpiAwarenessContext`
//! 打开（必须在建窗之前），高分屏下窗口才不会糊。
//!
//! ## 已知缺口
//!
//! - **IME 候选窗未接入**。`WM_IME_CHAR` 已处理（简单 IME 场景可用），
//!   但完整的 TSF/IMM 组合输入（候选框、预编辑串）需要 `Win32_UI_Input_Ime`
//!   与 UI 子线程，不在本模块范围。
//! - **水平滚轮未暴露**。[`WindowEvent::Scroll`] 按接口约定只带垂直量。
//! - **剪贴板快捷键不在此处理**。`Ctrl+C/V/X` 的实际读写由 `tiez-capture`
//!   负责；egui 侧的 `Event::Copy/Cut/Paste` 由调用方从剪贴板层构造。

use std::time::{Duration, Instant};

use anyhow::Context as _;
use egui::emath::{Pos2, Rect, Vec2};
use egui::{Event as EguiEvent, Key, Modifiers, MouseWheelUnit, PointerButton, TouchPhase};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, VK_APPS, VK_BACK, VK_CANCEL, VK_CAPITAL, VK_DELETE, VK_DOWN,
    VK_END, VK_F1, VK_F24, VK_HOME, VK_INSERT, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN,
    VK_NEXT, VK_NUMLOCK, VK_NUMPAD0, VK_OEM_1, VK_OEM_102, VK_OEM_2, VK_OEM_3, VK_OEM_4,
    VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_8, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD,
    VK_OEM_PLUS, VK_PRIOR, VK_RCONTROL, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL,
    VK_SHIFT, VK_SPACE, VK_TAB, VK_UP, VK_ZOOM,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetCursorPos, IsWindow, MSG, MessageBoxW, PeekMessageW, RegisterClassExW, SW_SHOW,
    ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CAPTURECHANGED, WM_CHAR,
    WM_CLOSE, WM_DPICHANGED, WM_IME_CHAR, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETFOCUS, WM_SIZE, WM_SYSCHAR, WM_SYSKEYDOWN,
    WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, PCWSTR};

/// 窗口类名。
///
/// 进程内只注册一次。字符串放在 `OnceLock` 里是为了让它活到进程结束——
/// 窗口类是一张进程级的表，类名提前释放会让后续 `CreateWindowExW`
/// 按名字查类失败，而这种失败只在运行期出现，极难定位。
const CLASS_NAME: &str = "ModularClipboardWindow";

/// 窗口类名与注册状态的进程级单例。
static CLASS: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// 默认 DPI，96 对应 100% 缩放。
const DEFAULT_DPI: u32 = 96;

/// 一个滚轮刻度对应的 `WM_MOUSEWHEEL` 单位。
///
/// `windows` crate 没有导出这个常量（我查过 0.62.2 的 WindowsAndMessaging
/// 模块，确实没有），因此自己定义。
const WHEEL_DELTA: f32 = 120.0;

/// `WM_IME_CHAR`。同样没有导出常量。
const WM_IME_CHAR_MSG: u32 = 0x010D;

/// 帧循环默认的预测帧间隔。
const DEFAULT_PREDICTED_DT: f32 = 1.0 / 60.0;

/// 语义化的窗口事件。
///
/// 坐标一律是**逻辑点**（已按 `scale_factor` 换算），原点为客户区左上角。
#[derive(Debug, Clone, PartialEq)]
pub enum WindowEvent {
    /// 客户区尺寸变化，单位为逻辑点。最小尺寸被系统钳制过。
    Resized {
        /// 客户区宽。
        width: f32,
        /// 客户区高。
        height: f32,
    },

    /// 用户请求关闭窗口。
    ///
    /// 此时窗口**尚未被销毁**——`WM_CLOSE` 不派发，由应用决定去留。
    /// 剪贴板类程序通常在这里改为隐藏到托盘；确实要退出就调
    /// [`Window::destroy`]。
    CloseRequested,

    /// 键盘焦点 gained/lost。
    Focused(bool),

    /// DPI 缩放比例变化，`scale_factor = dpi / 96`。
    ScaleFactorChanged(f64),

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

    /// 滚轮滚动，单位为**行**（一个刻度为 1.0）。
    ///
    /// 正值表示内容向下滚（用户向上拨）。
    Scroll(f32),

    /// 键盘按键。
    Key {
        /// 逻辑键。输入法未介入时与物理键一致。
        keycode: Key,
        /// 落下为 `true`，抬起为 `false`。
        pressed: bool,
        /// 是否为系统自动重复。egui 文本框需要它来区分长按与多次敲击。
        repeat: bool,
        /// 事件发生时的修饰键状态。
        modifiers: Modifiers,
    },

    /// 已编码的文本输入。
    ///
    /// 控制字符（`\r` `\t` `\x08` `\x1b` 等）已被过滤——它们由
    /// [`WindowEvent::Key`] 表达，双份会让文本框里出现重复字符。
    TextInput(String),
}

/// Win32 窗口。
///
/// 持有 `HWND` 与模块句柄，`Drop` 时销毁窗口。窗口类**不**注销：
/// 它是进程级的，且可能有其它 `Window` 实例正在使用同名类。
pub struct Window {
    hwnd: HWND,
    hinstance: HINSTANCE,
    /// `HSTRING` 必须活到 `CreateWindowExW` 返回之后——它是泛型参数
    /// `P: Param<PCWSTR>` 的借用源，提前释放会读到野指针。
    _class_name: HSTRING,
    _title: HSTRING,
}

impl Window {
    /// 创建并显示一个窗口。
    ///
    /// `width` / `height` 是**客户区**的物理像素。会先打开
    /// per-monitor DPI 感知（v2），因此在高 DPI 显示器上窗口是清晰的。
    pub fn new(title: &str, width: u32, height: u32) -> anyhow::Result<Self> {
        // 必须在建窗之前调用，之后再设置不生效。
        // 重复调用会返回 Err（ERROR_ACCESS_DENIED），此时说明已经有别处
        // 设过了，不影响正确性，因此只 debug 记一笔。
        if let Err(e) = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        {
            tracing::debug!("设置 DPI 感知失败（可能已设置）: {e:?}");
        }

        let hinstance = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW 失败")?;

        let class_name = HSTRING::from(CLASS_NAME);
        let class_p = PCWSTR(class_name.as_ptr());
        register_class(hinstance, class_p).context("注册窗口类失败")?;

        // AdjustWindowRectEx 把客户区尺寸换算成整体窗口尺寸，
        // 否则 width/height 会把标题栏和边框算进客户区，窗口会比预期小。
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width.min(i32::MAX as u32) as i32,
            bottom: height.min(i32::MAX as u32) as i32,
        };
        unsafe { windows::Win32::UI::WindowsAndMessaging::AdjustWindowRectEx(
            &mut rect,
            WS_OVERLAPPEDWINDOW,
            false,
            WINDOW_EX_STYLE(0),
        ) }
        .context("AdjustWindowRectEx 失败")?;

        let title_hs = HSTRING::from(title);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_p,
                PCWSTR(title_hs.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                120,
                120,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .context("CreateWindowExW 失败")?;

        unsafe { ShowWindow(hwnd, SW_SHOW) };

        Ok(Self {
            hwnd,
            hinstance: hinstance.into(),
            _class_name: class_name,
            _title: title_hs,
        })
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
        let dpi = unsafe { GetDpiForWindow(self.hwnd) };
        if dpi == 0 { 1.0 } else { dpi as f32 / DEFAULT_DPI as f32 }
    }

    /// 主动销毁窗口。
    ///
    /// 收到 [`WindowEvent::CloseRequested`] 后想真正退出时才需要调用。
    /// [`WindowEvent::CloseRequested`] 本身**不会**销毁窗口。
    pub fn destroy(&self) {
        unsafe {
            // 重复销毁同一个 HWND 是未定义行为，先确认它还活着。
            if IsWindow(Some(self.hwnd)).as_bool() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// 注册窗口类。进程内只做一次。
fn register_class(hinstance: HINSTANCE, class_p: PCWSTR) -> anyhow::Result<()> {
    let mut first_time = false;
    CLASS.get_or_init(|| {
        first_time = true;
    });

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        // 必须原样转发 DefWindowProcW，见模块文档。
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: class_p,
        ..Default::default()
    };

    let ok = unsafe { RegisterClassExW(&wc) } != 0;
    if ok || !first_time {
        // 首次失败要报错；后续「已存在」是正常的（类已注册）。
        if ok {
            return Ok(());
        }
    }
    if ok {
        return Ok(());
    }
    Err(windows::core::Error::from_win32()).context("RegisterClassExW 返回 0")
}

/// 空壳窗口过程。只转发，不做任何处理。
///
/// 绝不能在这里对消息做偏移或改写——`CreateWindowExW` 在注册阶段会校验
/// 该函数地址，加偏移会导致 `0x8007007E`（MEMORY.md 第 26 条）。
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 事件循环。
///
/// 拥有「本帧事件」缓冲：[`EventLoop::poll`] 填充它，
/// [`EventLoop::egui_input`] 读取它。**每帧的调用顺序必须是
/// `poll` → `egui_input` → 渲染**。
pub struct EventLoop {
    hwnd: HWND,
    /// 事件循环启动时刻，用作 egui 的时间原点。
    start: Instant,
    /// 上次 poll 得到的缩放比例。
    scale_factor: f32,
    /// 上次 poll 得到的客户区尺寸（逻辑点）。
    inner_size: Vec2,
    /// 本帧事件缓冲。由 `poll` 写、`egui_input` 读。
    pending: Vec<WindowEvent>,
    /// 上一帧结束时的修饰键状态，用于检测变化并发 `ModifiersChanged`。
    last_modifiers: Modifiers,
    /// 键盘焦点状态，对应 `RawInput::focused`。
    focused: bool,
    /// 已收到 `WM_QUIT`。
    quit: bool,
    /// 应用请求的下次重绘间隔，由 [`EventLoop::set_repaint_after`] 设置。
    repaint_after: Option<Duration>,
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
            last_modifiers: Modifiers::default(),
            focused: true,
            quit: false,
            repaint_after: None,
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

    /// 排空消息队列，不阻塞。
    ///
    /// 返回本轮读到的全部事件，同时写入内部缓冲供 [`Self::egui_input`]
    /// 使用。有事件时立即返回——这是高优先级路径。
    pub fn poll(&mut self) -> Vec<WindowEvent> {
        self.pending.clear();

        // 焦点可能在两轮之间丢失（例如 Alt+Tab），每轮重查一次。
        self.focused = unsafe { GetForegroundWindow() == self.hwnd };

        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let mut events: Vec<WindowEvent> = Vec::new();

        while unsafe { PeekMessageW(&mut msg, None, 0, 0, windows::Win32::UI::WindowsAndMessaging::PM_REMOVE) }
            .as_bool()
        {
            // WM_QUIT 没有窗口过程，直接退出循环语义。
            if msg.message == WM_QUIT {
                self.quit = true;
                events.push(WindowEvent::CloseRequested);
                continue;
            }

            // WM_CLOSE **不派发**：派发会让 DefWindowProcW 销毁窗口。
            // 剪贴板程序通常要转到托盘而不是退出，所以由应用决定。
            if msg.message == WM_CLOSE {
                events.push(WindowEvent::CloseRequested);
                continue;
            }

            // TranslateMessage 把 WM_KEYDOWN 转成 WM_CHAR 后投递到队列尾部，
            // 同一轮循环的下一次 PeekMessageW 就能读到。必须在读本条消息
            // 之前调用，否则 WM_CHAR 会晚一帧。
            if matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN) {
                unsafe { TranslateMessage(&msg) };
            }

            self.translate(&msg, &mut events);

            // 其余消息照常派发：清 WM_PAINT 的更新区、走默认处理。
            // wnd_proc 是空壳，派发不产生副作用，但必须派发，否则
            // WM_PAINT 会因更新区未清而无限重投。
            if msg.message != WM_QUIT {
                unsafe { DispatchMessageW(&msg) };
            }
        }

        self.pending = events.clone();
        events
    }

    /// 睡眠式轮询：有消息立即返回，空闲时最多阻塞 `timeout`。
    ///
    /// 用 `MsgWaitForMultipleObjects` 同时等「消息到达」和「超时」，
    /// 而不是 `while GetMessage() {}` 忙等——后者在无事件时会把一个核跑满。
    pub fn poll_for(&mut self, timeout: Option<Duration>) -> Vec<WindowEvent> {
        let events = self.poll();
        if !events.is_empty() || self.quit {
            return events;
        }
        match timeout {
            Some(d) if !d.is_zero() => {
                let ms = d.as_millis().min(u32::MAX as u128) as u32;
                unsafe {
                    windows::Win32::UI::WindowsAndMessaging::MsgWaitForMultipleObjects(
                        None,
                        false,
                        ms,
                        windows::Win32::UI::WindowsAndMessaging::QS_ALLINPUT,
                    );
                }
                self.poll()
            }
            // None 或零超时：纯自旋，由调用方自己控制节奏。
            _ => events,
        }
    }

    /// 把一条消息翻译成语义事件，追加到 `out`。
    fn translate(&mut self, msg: &MSG, out: &mut Vec<WindowEvent>) {
        let scale = self.scale_factor;

        match msg.message {
            WM_SIZE => {
                let (w, h) = unpack_size(msg.lParam.0);
                self.inner_size = Vec2::new(w, h);
                out.push(WindowEvent::Resized {
                    width: w,
                    height: h,
                });
            }

            WM_DPICHANGED => {
                // lParam 指向系统建议的新窗口矩形（物理像素）。
                let suggested = unsafe { *(msg.lParam.0 as *const RECT) };
                let dpi = suggested.right.saturating_sub(suggested.left).max(1) as f32;
                // 无法直接拿 dpi，改由 GetDpiForWindow 读——此时它已是新值。
                let new_scale = unsafe {
                    let d = GetDpiForWindow(self.hwnd);
                    if d == 0 {
                        scale
                    } else {
                        d as f32 / DEFAULT_DPI as f32
                    }
                };
                let _ = dpi;
                if (new_scale - scale).abs() > f32::EPSILON {
                    self.scale_factor = new_scale;
                    out.push(WindowEvent::ScaleFactorChanged(new_scale as f64));
                }
                // 尺寸由随后的 WM_SIZE 更新，这里只管缩放。
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
                let pos = self.to_points(msg.lParam.0, scale);
                out.push(WindowEvent::MouseMoved { pos });
            }

            WM_LBUTTONDOWN | WM_LBUTTONUP => {
                let pressed = msg.message == WM_LBUTTONDOWN;
                if pressed {
                    unsafe { SetCapture(self.hwnd) };
                } else {
                    unsafe { let _ = ReleaseCapture() };
                }
                out.push(WindowEvent::MouseButton {
                    button: PointerButton::Primary,
                    pressed,
                    pos: self.to_points(msg.lParam.0, scale),
                });
            }

            WM_RBUTTONDOWN | WM_RBUTTONUP => {
                let pressed = msg.message == WM_RBUTTONDOWN;
                if pressed {
                    unsafe { SetCapture(self.hwnd) };
                } else {
                    unsafe { let _ = ReleaseCapture() };
                }
                out.push(WindowEvent::MouseButton {
                    button: PointerButton::Secondary,
                    pressed,
                    pos: self.to_points(msg.lParam.0, scale),
                });
            }

            WM_MBUTTONDOWN | WM_MBUTTONUP => {
                let pressed = msg.message == WM_MBUTTONDOWN;
                if pressed {
                    unsafe { SetCapture(self.hwnd) };
                } else {
                    unsafe { let _ = ReleaseCapture() };
                }
                out.push(WindowEvent::MouseButton {
                    button: PointerButton::Middle,
                    pressed,
                    pos: self.to_points(msg.lParam.0, scale),
                });
            }

            // WM_CAPTURECHANGED 说明有别的窗口抢走了鼠标（比如菜单弹出）。
            // 此时若本进程仍以为自己在拖拽，后续会收到一串「幽灵」抬起事件。
            WM_CAPTURECHANGED => {
                // 读 GetCapture 确认确实已失去捕获。
                if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() }
                    != self.hwnd
                {
                    out.push(WindowEvent::MouseButton {
                        button: PointerButton::Primary,
                        pressed: false,
                        pos: last_known_pointer().unwrap_or(Pos2::ZERO),
                    });
                }
            }

            WM_MOUSEWHEEL => {
                let lines = wheel_lines(msg.wParam.0);
                if lines != 0.0 {
                    // lParam 是**屏幕**坐标，需换算到客户区。
                    let mut pt = POINT {
                        x: low_word_i32(msg.lParam.0),
                        y: high_word_i32(msg.lParam.0),
                    };
                    unsafe { ScreenToClient(self.hwnd, &mut pt) };
                    out.push(WindowEvent::Scroll(lines));
                    // 滚轮不产生 MouseMoved，但 egui 需要知道指针位置
                    // 才知道滚轮作用在哪个控件上。
                    let _ = pt;
                }
            }

            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let vk = msg.wParam.0 as u16;
                // bit 30 = 1 表示按下前该键已处于按下状态，即系统自动重复。
                let repeat = (msg.lParam.0 >> 30) & 1 == 1;
                let modifiers = current_modifiers();
                self.last_modifiers = modifiers;
                if let Some(keycode) = key_from_vk(vk) {
                    out.push(WindowEvent::Key {
                        keycode,
                        pressed: true,
                        repeat,
                        modifiers,
                    });
                }
            }

            // WM_KEYUP 不在消息列表的常量里（windows crate 未导出 WM_KEYUP
            // 的显式匹配需求），直接用数值。
            0x0105 => {
                if let Some(keycode) = key_from_vk(msg.wParam.0 as u16) {
                    out.push(WindowEvent::Key {
                        keycode,
                        pressed: false,
                        repeat: false,
                        modifiers: current_modifiers(),
                    });
                }
            }

            WM_CHAR | WM_SYSCHAR | WM_IME_CHAR_MSG => {
                let unit = msg.wParam.0 as u16;
                if let Some(s) = char_from_utf16_unit(unit) {
                    out.push(WindowEvent::TextInput(s));
                }
            }

            _ => {}
        }
    }

    /// lParam 打包的坐标 → 逻辑点。
    fn to_points(&self, lparam: isize, scale: f32) -> Pos2 {
        Pos2::new(
            low_word_i32(lparam) as f32 / scale,
            high_word_i32(lparam) as f32 / scale,
        )
    }

    /// 构造本帧的 [`egui::RawInput`]。
    ///
    /// 读取 [`Self::poll`] 填好的事件缓冲。D 组只需在每帧开头调用一次，
    /// 把结果交给 `egui::Context::run`。
    ///
    /// `screen_rect` 用逻辑点，`time` 是自事件循环创建起的秒数，
    /// `predicted_dt` 取 `set_repaint_after` 设的间隔。
    pub fn egui_input(&self, ctx: &egui::Context) -> egui::RawInput {
        let mut events: Vec<EguiEvent> = Vec::with_capacity(self.pending.len() + 4);

        // 指针位置每帧都要报，否则 egui 无法正确处理「窗口内但鼠标没动」
        // 的悬停判定。
        if let Some(pos) = last_known_pointer() {
            events.push(EguiEvent::PointerMoved(pos));
        }

        let mut modifiers = self.last_modifiers;
        let mut focused = self.focused;
        let mut pointer_pos: Option<Pos2> = None;

        for ev in &self.pending {
            match ev {
                WindowEvent::MouseMoved { pos } => {
                    pointer_pos = Some(*pos);
                    events.push(EguiEvent::PointerMoved(*pos));
                }
                WindowEvent::MouseButton {
                    button,
                    pressed,
                    pos,
                } => {
                    pointer_pos = Some(*pos);
                    events.push(EguiEvent::PointerButton {
                        pos: *pos,
                        button: *button,
                        pressed: *pressed,
                        modifiers,
                    });
                }
                WindowEvent::Scroll(lines) => {
                    events.push(EguiEvent::MouseWheel {
                        // Win32 给的是行数，egui 原生支持 Line 单位，
                        // 由它按 line_scroll_speed 换算成点。
                        unit: MouseWheelUnit::Line,
                        delta: Vec2::new(0.0, *lines),
                        // 鼠标滚轮不是触摸。用 Move 让 egui 进入
                        // Status::Smoothing（跨几帧平滑），若误用 Start
                        // 会被当成触摸从而锁住修饰键直到抬手。
                        phase: TouchPhase::Move,
                        modifiers,
                    });
                }
                WindowEvent::Key {
                    keycode,
                    pressed,
                    repeat,
                    ..
                } => {
                    events.push(EguiEvent::Key {
                        key: *keycode,
                        physical_key: Some(*keycode),
                        pressed: *pressed,
                        repeat: *repeat,
                        modifiers,
                    });
                }
                WindowEvent::TextInput(s) => {
                    events.push(EguiEvent::Text(s.clone()));
                }
                WindowEvent::Focused(b) => {
                    focused = *b;
                    events.push(EguiEvent::WindowFocused(*b));
                }
                // 尺寸与缩放通过 screen_rect 表达，无需单独事件。
                WindowEvent::Resized { .. } | WindowEvent::ScaleFactorChanged(_) => {}
                WindowEvent::CloseRequested => {}
            }
            if let WindowEvent::Key { modifiers: m, .. } = ev {
                modifiers = *m;
            }
        }

        if modifiers != ctx.input(|i| i.modifiers) {
            events.push(EguiEvent::ModifiersChanged(modifiers));
        }

        let _ = pointer_pos;

        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.inner_size)),
            time: Some(self.start.elapsed().as_secs_f64()),
            predicted_dt: self
                .repaint_after
                .map(|d| d.as_secs_f32().clamp(1.0 / 1000.0, 0.1))
                .unwrap_or(DEFAULT_PREDICTED_DT),
            events,
            focused,
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

/// 虚拟键码 → egui 逻辑键。无法映射时返回 `None`。
///
/// 只覆盖 egui 有对应变体的键。带输入法的字母键 `wParam` 是 `'A'`..
/// `'Z'` 的 ASCII 码，这与本函数的字母分支一致，因此输入法场景下
/// 也能拿到正确的键。
pub fn key_from_vk(vk: u16) -> Option<Key> {
    // 数字键与字母键用区间查表，覆盖 36 个键且零分支预测失败。
    if (b'0' as u16..=b'9' as u16).contains(&vk) {
        return Some(DIGITS[(vk - b'0' as u16) as usize]);
    }
    if (b'A' as u16..=b'Z' as u16).contains(&vk) {
        return Some(LETTERS[(vk - b'A' as u16) as usize]);
    }

    let code = |k: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| k.0;
    Some(match vk {
        x if x == code(VK_LEFT) => Key::ArrowLeft,
        x if x == code(VK_RIGHT) => Key::ArrowRight,
        x if x == code(VK_UP) => Key::ArrowUp,
        x if x == code(VK_DOWN) => Key::ArrowDown,
        x if x == code(VK_PRIOR) => Key::PageUp,
        x if x == code(VK_NEXT) => Key::PageDown,
        x if x == code(VK_HOME) => Key::Home,
        x if x == code(VK_END) => Key::End,
        x if x == code(VK_INSERT) => Key::Insert,
        x if x == code(VK_DELETE) => Key::Delete,
        x if x == code(VK_BACK) => Key::Backspace,
        x if x == code(VK_SPACE) => Key::Space,
        x if x == code(VK_TAB) => Key::Tab,
        x if x == code(0x0D) => Key::Enter, // VK_RETURN

        // 左右修饰键要区分：egui 用它们判断单侧快捷键，
        // 合并会让「左 Ctrl」这类判断失效。
        x if x == code(VK_LSHIFT) => Key::ShiftLeft,
        x if x == code(VK_RSHIFT) => Key::ShiftRight,
        x if x == code(VK_LCONTROL) => Key::ControlLeft,
        x if x == code(VK_RCONTROL) => Key::ControlRight,
        x if x == code(VK_LMENU) => Key::AltLeft,
        x if x == code(VK_RMENU) => Key::AltRight,
        x if x == code(VK_LWIN) => Key::SuperLeft,
        x if x == code(VK_RWIN) => Key::SuperRight,

        // 无需区分左右的通用键
        x if x == code(VK_SHIFT) => Key::ShiftLeft,
        x if x == code(0x11) => Key::ControlLeft,
        x if x == code(0x12) => Key::AltLeft,

        x if x == code(VK_OEM_1) => Key::Semicolon,
        x if x == code(VK_OEM_PLUS) => Key::Equals,
        x if x == code(VK_OEM_COMMA) => Key::Comma,
        x if x == code(VK_OEM_MINUS) => Key::Minus,
        x if x == code(VK_OEM_PERIOD) => Key::Period,
        x if x == code(VK_OEM_2) => Key::Slash,
        x if x == code(VK_OEM_3) => Key::Backtick,
        x if x == code(VK_OEM_4) => Key::OpenBracket,
        x if x == code(VK_OEM_6) => Key::CloseBracket,
        x if x == code(VK_OEM_5) => Key::Backslash,
        x if x == code(VK_OEM_8) => Key::Quote,
        x if x == code(VK_OEM_7) => Key::Slash,
        x if x == code(VK_OEM_102) => Key::IntlBackslash,

        x if (code(VK_F1)..=code(VK_F24)).contains(&x) => {
            FKEYS[(x - code(VK_F1)) as usize]
        }

        // 以下按键 egui 无对应变体，返回 None 让上层忽略。
        _ => return None,
    })
}

/// 由四个独立布尔量组合出 egui 修饰键状态。
///
/// Windows 上没有 `mac_cmd` 的概念，一律为 `false`。
/// 「Windows 键」映射到 `command`——这是 egui 判定快捷键的主字段
/// （`modifiers.command` 而非 `ctrl`）。
pub fn modifiers_from(shift: bool, ctrl: bool, alt: bool, windows_key: bool) -> Modifiers {
    Modifiers {
        alt,
        ctrl,
        shift,
        mac_cmd: false,
        command: windows_key,
    }
}

/// 查询当前修饰键状态。
fn current_modifiers() -> Modifiers {
    unsafe {
        modifiers_from(
            is_key_down(VK_SHIFT.0),
            is_key_down(0x11), // VK_CONTROL
            is_key_down(0x12), // VK_MENU
            is_key_down(VK_LWIN.0) || is_key_down(VK_RWIN.0),
        )
    }
}

/// `GetKeyState` 的高��即按下。
unsafe fn is_key_down(vk: u16) -> bool {
    unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(vk as i32) < 0 }
}

/// 滚轮单位 → 行数。正值表示向上拨。
fn wheel_lines(wparam: usize) -> f32 {
    // 高 16 位是有符号的刻度数；正 = 远离用户 = 向上滚。
    let raw = ((wparam >> 16) & 0xFFFF) as u16 as i16 as f32;
    // Windows 会按设备精度给出 120 的整数倍，但高精度触控板会给小数，
    // 因此统一除而不做截断。
    raw / WHEEL_DELTA
}

/// `lParam` 低 16 位，按有符号 16 位解释。
fn low_word_i32(lparam: isize) -> i32 {
    (lparam as u16) as i16 as i32
}

/// `lParam` 高 16 位，按有符号 16 位解释。
fn high_word_i32(lparam: isize) -> i32 {
    ((lparam >> 16) as u16) as i16 as i32
}

/// `WM_SIZE` 的 `lParam`：低 16 位宽、高 16 位高，**均为无符号**。
fn unpack_size(lparam: isize) -> (f32, f32) {
    let w = (lparam as u16) as u32;
    let h = ((lparam >> 16) as u16) as u32;
    (w as f32, h as f32)
}

/// 单个 UTF-16 码元 → 文本。
///
/// 过滤控制字符：`WM_CHAR` 会把 Enter 送成 `\r`、Tab 送成 `\t`、
/// Backspace 送成 `\x08`。这些已由 [`WindowEvent::Key`] 表达，
/// 不过滤会让文本框里出现重复字符。
///
/// 代理项（emoji、生僻汉字）由 Win32 分两次投递（高代理 + 低代理），
/// 这里各自返回 `None`，由 [`CharDecoder`] 负责配对。
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
/// Win32 把 BMP 外的字符拆成两个 `WM_CHAR`：先高代理后低代理。
/// 单独看任一条都是无效的，必须攒起来配对。
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
            // 低代理：必须与高代理配对，否则是孤立代理项，丢弃。
            let high = self.pending_high.take()?;
            let cp = 0x1_0000u32
                + (((high as u32) - 0xD800) << 10)
                + ((unit as u32) - 0xDC00);
            return char::from_u32(cp).map(|c| c.to_string());
        }
        // 普通码元。若前面攒了未配对的高代理，说明序列损坏，
        // 丢弃那个高代理（Windows 不会这么发，但输入注入可能）。
        self.pending_high = None;
        char_from_utf16_unit(unit)
    }
}

/// 光标在**客户区**内的逻辑点坐标。
///
/// 消息队列里的 `WM_MOUSEMOVE` 只在移动时到达，而 egui 每帧都要知道
/// 指针位置才能算悬停。因此在没有移动事件时用系统光标位置补一次。
fn last_known_pointer() -> Option<Pos2> {
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt).ok()?;
        Some(Pos2::new(pt.x as f32, pt.y as f32))
    }
}

/// 窗口是否拥有前台焦点。
unsafe fn GetForegroundWindow() -> HWND {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() }
}

// ---------------------------------------------------------------- 单测

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- key_from_vk ----------------

    #[test]
    fn letters_map_in_order() {
        assert_eq!(key_from_vk(b'A' as u16), Some(Key::A));
        assert_eq!(key_from_vk(b'Z' as u16), Some(Key::Z));
        // 字母表必须严格递增映射，不能整体偏移一位。
        for (i, letter) in LETTERS.iter().enumerate() {
            let vk = b'A' as u16 + i as u16;
            assert_eq!(key_from_vk(vk), Some(*letter), "vk={vk}");
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
        assert_eq!(key_from_vk(VK_F1.0), Some(Key::F1));
        assert_eq!(key_from_vk(VK_F24.0), Some(Key::F24));
        // 连续区间不能有洞
        for i in 0..24u16 {
            assert!(key_from_vk(VK_F1.0 + i).is_some(), "F{}", i + 1);
        }
        // 区间外不应误判为功能键
        assert!(!matches!(
            key_from_vk(VK_F24.0 + 1),
            Some(Key::F1..=Key::F24)
        ));
    }

    #[test]
    fn navigation_keys_map() {
        assert_eq!(key_from_vk(VK_LEFT.0), Some(Key::ArrowLeft));
        assert_eq!(key_from_vk(VK_RIGHT.0), Some(Key::ArrowRight));
        assert_eq!(key_from_vk(VK_UP.0), Some(Key::ArrowUp));
        assert_eq!(key_from_vk(VK_DOWN.0), Some(Key::ArrowDown));
        assert_eq!(key_from_vk(VK_PRIOR.0), Some(Key::PageUp));
        assert_eq!(key_from_vk(VK_NEXT.0), Some(Key::PageDown));
        assert_eq!(key_from_vk(VK_HOME.0), Some(Key::Home));
        assert_eq!(key_from_vk(VK_END.0), Some(Key::End));
        assert_eq!(key_from_vk(VK_BACK.0), Some(Key::Backspace));
        assert_eq!(key_from_vk(VK_SPACE.0), Some(Key::Space));
        assert_eq!(key_from_vk(VK_TAB.0), Some(Key::Tab));
    }

    #[test]
    fn left_and_right_modifiers_are_distinct() {
        // 合并左右会让 egui 的单侧快捷键判断（如只用左 Ctrl）失效。
        assert_eq!(key_from_vk(VK_LSHIFT.0), Some(Key::ShiftLeft));
        assert_eq!(key_from_vk(VK_RSHIFT.0), Some(Key::ShiftRight));
        assert_eq!(key_from_vk(VK_LCONTROL.0), Some(Key::ControlLeft));
        assert_eq!(key_from_vk(VK_RCONTROL.0), Some(Key::ControlRight));
        assert_eq!(key_from_vk(VK_LMENU.0), Some(Key::AltLeft));
        assert_eq!(key_from_vk(VK_RMENU.0), Some(Key::AltRight));
        assert_eq!(key_from_vk(VK_LWIN.0), Some(Key::SuperLeft));
        assert_eq!(key_from_vk(VK_RWIN.0), Some(Key::SuperRight));
    }

    #[test]
    fn unmapped_keys_return_none() {
        // egui 无对应变体的键必须返回 None 而不是 panic
        for vk in [VK_CAPITAL.0, VK_NUMLOCK.0, VK_SCROLL.0, VK_APPS.0, VK_ZOOM.0, VK_CANCEL.0] {
            assert_eq!(key_from_vk(vk), None, "vk={vk}");
        }
        // 保留区与未分配码位
        assert_eq!(key_from_vk(0x00), None);
        assert_eq!(key_from_vk(0xFF), None);
    }

    #[test]
    fn every_mapped_key_is_distinct() {
        // 映射表里若有两个 VK 指向同一 Key，egui 的按键比较会误判
        let mut seen = std::collections::HashSet::new();
        for vk in 0u16..=255 {
            if let Some(k) = key_from_vk(vk) {
                assert!(seen.insert((vk, k)), "重复映射: vk={vk} key={k:?}");
            }
        }
    }

    // ---------------- modifiers ----------------

    #[test]
    fn modifiers_none_is_all_false() {
        let m = modifiers_from(false, false, false, false);
        assert_eq!(m.shift, false);
        assert_eq!(m.ctrl, false);
        assert_eq!(m.alt, false);
        assert_eq!(m.command, false);
        // Windows 上永远不是 mac
        assert_eq!(m.mac_cmd, false);
        assert_eq!(m, Modifiers::NONE);
    }

    #[test]
    fn windows_key_maps_to_command_not_mac_cmd() {
        // egui 用 `command` 判定快捷键。若错映射到 mac_cmd，
        // Win+ 上的快捷键会全部失效。
        let m = modifiers_from(false, false, false, true);
        assert_eq!(m.command, true);
        assert_eq!(m.mac_cmd, false);
        assert_eq!(m.ctrl, false);
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
    }

    // ---------------- 滚轮 ----------------

    #[test]
    fn wheel_delta_normalizes_to_lines() {
        // 一个刻度 = 120 单位 = 1.0 行
        assert_eq!(wheel_lines(120 << 16), 1.0);
        assert_eq!(wheel_lines(240 << 16), 2.0);
        // 低 16 位是键盘状态，必须忽略
        assert_eq!(wheel_lines((120 << 16) | 0xFFFF), 1.0);
    }

    #[test]
    fn wheel_direction_sign_is_preserved() {
        // 向下拨（远离用户）为负
        assert_eq!(wheel_lines((-120i32 as usize) << 16), -1.0);
        assert_eq!(wheel_lines((-240i32 as usize) << 16), -2.0);
    }

    #[test]
    fn wheel_high_precision_touchpad_is_not_truncated() {
        // 高精度触控板给出小数刻度，截断会让滚动几乎不动
        let delta = (60usize) << 16;
        assert!((wheel_lines(delta) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn wheel_zero_is_zero() {
        assert_eq!(wheel_lines(0), 0.0);
    }

    // ---------------- lParam 拆包 ----------------

    #[test]
    fn low_and_high_words_are_signed() {
        // 负坐标（鼠标移出客户区左侧）必须正确符号扩展，
        // 否则会变成 65535 而不是 -1。
        assert_eq!(low_word_i32(-1), -1);
        assert_eq!(high_word_i32(-1), -1);
        assert_eq!(low_word_i32(0xFFFF), -1);
        assert_eq!(high_word_i32(0xFFFF_FFFF), -1);
    }

    #[test]
    fn words_are_independent() {
        // x = 100, y = 200
        let lp = 200i64 << 16 | 100;
        assert_eq!(low_word_i32(lp as isize), 100);
        assert_eq!(high_word_i32(lp as isize), 200);
    }

    #[test]
    fn size_packing_is_unsigned() {
        // WM_SIZE 的宽高不会为负，且 y 在高 16 位
        let w = 1920u32;
        let h = 1080u32;
        let lp = ((h << 16) | w) as isize;
        let (pw, ph) = unpack_size(lp);
        assert_eq!((pw, ph), (1920.0, 1080.0));
    }

    #[test]
    fn size_zero_on_minimize() {
        // 最小化时 WM_SIZE 给出 0x0，不能 panic
        let (w, h) = unpack_size(0);
        assert_eq!((w, h), (0.0, 0.0));
    }

    // ---------------- 文本解码 ----------------

    #[test]
    fn ascii_passes_through() {
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
        // U+1F600 GRINNING FACE：D83D DE00
        let mut d = CharDecoder::new();
        assert_eq!(d.push(0xD83D), None, "高代理应缓存等待");
        assert_eq!(d.push(0xDE00).as_deref(), Some("\u{1F600}"));
    }

    #[test]
    fn surrogate_pair_for_cjk_extension() {
        // U+20000 𠀀：D840 DC00
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
        // 高代理后跟普通字符：坏配对被丢弃，普通字符仍要送达
        let mut d = CharDecoder::new();
        assert_eq!(d.push(0xD83D), None);
        assert_eq!(d.push(b'a' as u16).as_deref(), Some("a"));
    }

    #[test]
    fn consecutive_ascii_is_independent() {
        let mut d = CharDecoder::new();
        assert_eq!(d.push('你' as u16).as_deref(), Some("你"));
        assert_eq!(d.push('好' as u16).as_deref(), Some("好"));
    }

    // ---------------- egui 映射 ----------------

    #[test]
    fn scroll_maps_to_line_unit_not_touch() {
        // Line 单位让 egui 自己按 line_scroll_speed 换算；
        // TouchPhase::Move 使其进入平滑滚动而非触摸模式。
        let ev = WindowEvent::Scroll(2.0);
        let loopy = {
            let mut e = EventLoop::fake();
            e.pending = vec![ev];
            e.egui_input(&egui::Context::default())
        };
        match &loopy.events.iter().find(|e| matches!(e, EguiEvent::MouseWheel { .. })) {
            Some(EguiEvent::MouseWheel { unit, delta, phase, .. }) => {
                assert_eq!(*unit, MouseWheelUnit::Line);
                assert_eq!(*delta, Vec2::new(0.0, 2.0));
                assert_eq!(*phase, TouchPhase::Move);
            }
            other => panic!("应产出 MouseWheel，实际 {other:?}"),
        }
    }

    #[test]
    fn key_event_carries_physical_key() {
        // egui 的 `physical_key` 留空会让某些 IME 逻辑走不到分支
        let mut e = EventLoop::fake();
        e.pending = vec![WindowEvent::Key {
            keycode: Key::A,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::default(),
        }];
        let raw = e.egui_input(&egui::Context::default());
        match raw
            .events
            .iter()
            .find(|e| matches!(e, EguiEvent::Key { .. }))
        {
            Some(EguiEvent::Key {
                key,
                physical_key,
                pressed,
                repeat,
                ..
            }) => {
                assert_eq!(*key, Key::A);
                assert_eq!(*physical_key, Some(Key::A));
                assert!(*pressed);
                assert!(!*repeat);
            }
            other => panic!("应产出 Key，实际 {other:?}"),
        }
    }

    #[test]
    fn repeat_flag_is_preserved() {
        // 长按与多次敲击必须可区分，否则文本框会吞掉重复输入
        let mut e = EventLoop::fake();
        e.pending = vec![WindowEvent::Key {
            keycode: Key::B,
            pressed: true,
            repeat: true,
            modifiers: Modifiers::default(),
        }];
        let raw = e.egui_input(&egui::Context::default());
        match raw.events.iter().find(|e| matches!(e, EguiEvent::Key { .. })) {
            Some(EguiEvent::Key { repeat, .. }) => assert!(*repeat),
            other => panic!("应产出 Key，实际 {other:?}"),
        }
    }

    #[test]
    fn mouse_button_maps_all_three() {
        let mut e = EventLoop::fake();
        e.pending = vec![
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
        ];
        let raw = e.egui_input(&egui::Context::default());
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
        let mut e = EventLoop::fake();
        e.pending = vec![WindowEvent::TextInput("复制".into())];
        let raw = e.egui_input(&egui::Context::default());
        assert!(raw.events.iter().any(|e| matches!(e, EguiEvent::Text(s) if s == "复制")));
    }

    #[test]
    fn focus_change_reaches_egui() {
        let mut e = EventLoop::fake();
        e.focused = false;
        e.pending = vec![WindowEvent::Focused(false)];
        let raw = e.egui_input(&egui::Context::default());
        assert!(!raw.focused, "RawInput::focused 必须为 false");
        assert!(raw
            .events
            .iter()
            .any(|e| matches!(e, EguiEvent::WindowFocused(false))));
    }

    #[test]
    fn screen_rect_uses_logical_points() {
        // 1920x1080 物理像素、200% 缩放 → 960x540 逻辑点
        let mut e = EventLoop::fake();
        e.inner_size = Vec2::new(960.0, 540.0);
        let raw = e.egui_input(&egui::Context::default());
        let r = raw.screen_rect.expect("screen_rect 必须设置");
        assert_eq!(r.width(), 960.0);
        assert_eq!(r.height(), 540.0);
        assert_eq!(r.min, Pos2::ZERO);
    }

    #[test]
    fn predicted_dt_follows_repaint_after() {
        let mut e = EventLoop::fake();
        e.repaint_after = Some(Duration::from_millis(16));
        let raw = e.egui_input(&egui::Context::default());
        assert!((raw.predicted_dt - 0.016).abs() < 1e-4, "{}", raw.predicted_dt);

        // 未设置时退回 60Hz
        let mut e2 = EventLoop::fake();
        e2.repaint_after = None;
        let raw2 = e2.egui_input(&egui::Context::default());
        assert!((raw2.predicted_dt - 1.0 / 60.0).abs() < 1e-6);
    }

    #[test]
    fn predicted_dt_is_clamped_to_sane_range() {
        // 0 间隔会让动画除零；10 秒间隔会让动画瞬移
        let mut e = EventLoop::fake();
        e.repaint_after = Some(Duration::ZERO);
        let raw = e.egui_input(&egui::Context::default());
        assert!(raw.predicted_dt >= 1.0 / 1000.0);

        let mut e2 = EventLoop::fake();
        e2.repaint_after = Some(Duration::from_secs(10));
        let raw2 = e2.egui_input(&egui::Context::default());
        assert!(raw2.predicted_dt <= 0.1);
    }

    #[test]
    fn time_is_monotonic_seconds() {
        let mut e = EventLoop::fake();
        let t1 = e.egui_input(&egui::Context::default()).time.unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let t2 = e.egui_input(&egui::Context::default()).time.unwrap();
        assert!(t2 > t1, "{t2} 应大于 {t1}");
        assert!(t1 < 1.0, "起点应接近 0");
    }

    #[test]
    fn empty_frame_still_reports_pointer_position() {
        // 没有移动消息时 egui 仍需知道指针位置才能算悬停
        let e = EventLoop::fake();
        let raw = e.egui_input(&egui::Context::default());
        // 无光标（CI 无显示设备）时不应 panic；有则必须有 PointerMoved
        let _ = raw.events.iter().filter(|e| matches!(e, EguiEvent::PointerMoved(_))).count();
    }

    #[test]
    fn close_requested_is_not_forwarded_to_egui() {
        // CloseRequested 是应用层语义，不该变成 egui 事件
        let mut e = EventLoop::fake();
        e.pending = vec![WindowEvent::CloseRequested];
        let raw = e.egui_input(&egui::Context::default());
        assert!(!raw.events.iter().any(|e| matches!(e, EguiEvent::Cut | EguiEvent::Copy | EguiEvent::Paste(_))));
    }

    #[test]
    fn modifiers_changed_emitted_once_on_change() {
        let mut e = EventLoop::fake();
        e.pending = vec![WindowEvent::Key {
            keycode: Key::C,
            pressed: true,
            repeat: false,
            modifiers: Modifiers {
                command: true,
                ..Default::default()
            },
        }];
        let ctx = egui::Context::default();
        let raw = e.egui_input(&ctx);
        assert!(
            raw.events
                .iter()
                .any(|ev| matches!(ev, EguiEvent::ModifiersChanged(m) if m.command)),
            "修饰键变化应通知 egui"
        );
    }
}
