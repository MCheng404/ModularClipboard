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
//! ## 已知缺口
//!
//! - **IME 候选窗未接入**。`WM_IME_CHAR` 已处理（简单 IME 可用），
//!   但完整的 TSF/IMM 组合输入（候选框、预编辑串）需要
//!   `Win32_UI_Input_Ime` 与独立 UI 线程，不在本模块范围。
//! - **水平滚轮未暴露**。[`WindowEvent::Scroll`] 按接口约定只带垂直量。
//! - **剪贴板快捷键不在此处理**。`Ctrl+C/V/X` 的实际读写由
//!   `modular-clipboard-capture` 负责，egui 侧的 `Event::Copy/Cut/Paste` 由调用方
//!   从剪贴板层构造。本模块只产出 `Key` 与 `TextInput`。
//! - **自绘标题栏未实现**，窗口带系统标题栏（`WS_OVERLAPPEDWINDOW`）。

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
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetCursorPos, GetForegroundWindow, IsWindow, MSG, PM_REMOVE, PeekMessageW,
    QS_ALLINPUT, RegisterClassExW, SW_SHOW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE,
    WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE, WM_DPICHANGED, WM_IME_CHAR, WM_KEYDOWN, WM_KEYUP,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETFOCUS, WM_SIZE, WM_SYSCHAR,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, PCWSTR};

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

        // AdjustWindowRectEx 把客户区尺寸换算为整体窗口尺寸。
        // 不调的话 width/height 会把标题栏和边框算进客户区，
        // 窗口会比预期小一圈。
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: clamp_i32(width),
            bottom: clamp_i32(height),
        };
        unsafe { AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0)) }
            .context("AdjustWindowRectEx 失败")?;

        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_p,
                title_p,
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

        // BOOL 是 Copy 且非 must_use，显式丢弃以表明「返回值不关心」
        let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };

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

    /// 排空消息队列，不阻塞。
    ///
    /// 返回本轮读到的全部事件，同时写入内部缓冲供 [`Self::egui_input`]
    /// 使用。有事件时立即返回——这是高优先级路径。
    pub fn poll(&mut self) -> Vec<WindowEvent> {
        self.pending.clear();
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

        self.pending = events.clone();
        events
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
    /// 读取 [`Self::poll`] 填好的事件缓冲。D 组每帧开头调用一次，
    /// 把结果交给 `egui::Context::run` 即可。
    ///
    /// `screen_rect` 用逻辑点，`time` 是自事件循环创建起的秒数，
    /// `predicted_dt` 取 `set_repaint_after` 设的间隔。
    pub fn egui_input(&self, ctx: &egui::Context) -> egui::RawInput {
        let mut events: Vec<EguiEvent> = Vec::with_capacity(self.pending.len() + 4);

        // 指针位置每帧都要报。没有移动消息时用系统光标位置兜底，
        // 否则 egui 无法判断「鼠标在窗口内但没动」的悬停。
        if let Some(pos) = self.pointer_in_points(self.scale_factor) {
            events.push(EguiEvent::PointerMoved(pos));
        }

        let mut modifiers = self.modifiers;

        for ev in &self.pending {
            match ev {
                WindowEvent::MouseMoved { pos } => {
                    events.push(EguiEvent::PointerMoved(*pos));
                }
                WindowEvent::MouseButton {
                    button,
                    pressed,
                    pos,
                } => {
                    events.push(EguiEvent::PointerButton {
                        pos: *pos,
                        button: *button,
                        pressed: *pressed,
                        modifiers,
                    });
                }
                WindowEvent::Scroll(lines) => {
                    events.push(EguiEvent::MouseWheel {
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
                    events.push(EguiEvent::Key {
                        key: *keycode,
                        // 同时填 physical_key：留空会让部分 IME 分支走不到。
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
                    events.push(EguiEvent::WindowFocused(*b));
                }
                // 尺寸与缩放通过 screen_rect 表达；关闭是应用层语义。
                WindowEvent::Resized { .. }
                | WindowEvent::ScaleFactorChanged(_)
                | WindowEvent::CloseRequested => {}
            }
        }

        // egui 靠这个事件更新修饰键状态（含 Win 键 → command）。
        if modifiers != ctx.input(|i| i.modifiers) {
            events.push(EguiEvent::ModifiersChanged(modifiers));
        }

        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.inner_size)),
            time: Some(self.start.elapsed().as_secs_f64()),
            predicted_dt: self
                .repaint_after
                .map(|d| d.as_secs_f32())
                .unwrap_or(DEFAULT_PREDICTED_DT)
                .clamp(MIN_PREDICTED_DT, MAX_PREDICTED_DT),
            events,
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

    /// 把 `pending` 灌入并产出 `RawInput`。
    fn raw_of(pending: Vec<WindowEvent>) -> egui::RawInput {
        let mut e = fake_loop();
        e.pending = pending;
        e.egui_input(&egui::Context::default())
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
        e.pending = vec![WindowEvent::Focused(false)];
        let raw = e.egui_input(&egui::Context::default());
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
        let raw = e.egui_input(&egui::Context::default());
        let r = raw.screen_rect.expect("screen_rect 必须设置");
        assert_eq!(r.width(), 960.0);
        assert_eq!(r.height(), 540.0);
        assert_eq!(r.min, Pos2::ZERO);
    }

    #[test]
    fn predicted_dt_follows_repaint_after() {
        let mut e = fake_loop();
        e.repaint_after = Some(Duration::from_millis(16));
        let raw = e.egui_input(&egui::Context::default());
        assert!((raw.predicted_dt - 0.016).abs() < 1e-4, "{}", raw.predicted_dt);
    }

    #[test]
    fn predicted_dt_defaults_to_60hz() {
        let e = fake_loop();
        let raw = e.egui_input(&egui::Context::default());
        assert!((raw.predicted_dt - 1.0 / 60.0).abs() < 1e-6);
    }

    #[test]
    fn predicted_dt_is_clamped_to_sane_range() {
        // 0 间隔会让动画除零；10 秒间隔会让动画瞬移
        let mut e = fake_loop();
        e.repaint_after = Some(Duration::ZERO);
        let raw = e.egui_input(&egui::Context::default());
        assert!(raw.predicted_dt >= 1.0 / 1000.0, "{}", raw.predicted_dt);

        let mut e2 = fake_loop();
        e2.repaint_after = Some(Duration::from_secs(10));
        let raw2 = e2.egui_input(&egui::Context::default());
        assert!(raw2.predicted_dt <= 0.1, "{}", raw2.predicted_dt);
    }

    #[test]
    fn time_is_monotonic_seconds() {
        let e = fake_loop();
        let t1 = e.egui_input(&egui::Context::default()).time.unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let t2 = e.egui_input(&egui::Context::default()).time.unwrap();
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
        e.pending = vec![WindowEvent::Key {
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
        let raw = e.egui_input(&egui::Context::default());
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
}
