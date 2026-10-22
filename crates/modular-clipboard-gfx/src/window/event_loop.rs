//! `EventLoop`：Win32 消息泵与事件缓冲。
//!
//! `poll` 写缓冲、`egui_input` 读缓冲——顺序反了本帧输入全丢
//! （event_delivery 测试守这条接线）。

use egui::Event as EguiEvent;
use egui::Modifiers;
use egui::MouseWheelSource;
use egui::MouseWheelUnit;
use egui::PointerButton;
use egui::TouchPhase;
use egui::emath::Pos2;
use egui::emath::Rect;
use egui::emath::Vec2;
use std::time::Duration;
use std::time::Instant;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::POINT;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::Ime::GCS_COMPSTR;
use windows::Win32::UI::Input::Ime::ImmGetCompositionStringW;
use windows::Win32::UI::Input::Ime::ImmGetContext;
use windows::Win32::UI::Input::Ime::ImmReleaseContext;
use windows::Win32::UI::Input::KeyboardAndMouse::GetCapture;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::Input::KeyboardAndMouse::SetCapture;
use windows::Win32::UI::WindowsAndMessaging::DispatchMessageW;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
use windows::Win32::UI::WindowsAndMessaging::MSG;
use windows::Win32::UI::WindowsAndMessaging::PM_REMOVE;
use windows::Win32::UI::WindowsAndMessaging::PeekMessageW;
use windows::Win32::UI::WindowsAndMessaging::QS_ALLINPUT;
use windows::Win32::UI::WindowsAndMessaging::SIZE_MINIMIZED;
use windows::Win32::UI::WindowsAndMessaging::SIZE_RESTORED;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER;
use windows::Win32::UI::WindowsAndMessaging::SetWindowPos;
use windows::Win32::UI::WindowsAndMessaging::TranslateMessage;
use windows::Win32::UI::WindowsAndMessaging::WM_CAPTURECHANGED;
use windows::Win32::UI::WindowsAndMessaging::WM_CHAR;
use windows::Win32::UI::WindowsAndMessaging::WM_CLOSE;
use windows::Win32::UI::WindowsAndMessaging::WM_DPICHANGED;
use windows::Win32::UI::WindowsAndMessaging::WM_IME_CHAR;
use windows::Win32::UI::WindowsAndMessaging::WM_IME_COMPOSITION;
use windows::Win32::UI::WindowsAndMessaging::WM_IME_ENDCOMPOSITION;
use windows::Win32::UI::WindowsAndMessaging::WM_IME_STARTCOMPOSITION;
use windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_KEYUP;
use windows::Win32::UI::WindowsAndMessaging::WM_KILLFOCUS;
use windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP;
use windows::Win32::UI::WindowsAndMessaging::WM_MBUTTONDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_MBUTTONUP;
use windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE;
use windows::Win32::UI::WindowsAndMessaging::WM_MOUSEWHEEL;
use windows::Win32::UI::WindowsAndMessaging::WM_QUIT;
use windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP;
use windows::Win32::UI::WindowsAndMessaging::WM_SETFOCUS;
use windows::Win32::UI::WindowsAndMessaging::WM_SIZE;
use windows::Win32::UI::WindowsAndMessaging::WM_SYSCHAR;
use windows::Win32::UI::WindowsAndMessaging::WM_SYSCOMMAND;
use windows::Win32::UI::WindowsAndMessaging::WM_SYSKEYDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_SYSKEYUP;

use super::{DEFAULT_PREDICTED_DT, MAX_PREDICTED_DT, MIN_PREDICTED_DT, SC_CLOSE_WPARAM, SC_SYSTEM_MENU_MASK, WM_HIDE_MAIN, WM_SHOW_MAIN, Window, WindowEvent, WindowIme, dpi_scale_factor};

use super::keys::CharDecoder;
use super::keys::current_modifiers;
use super::keys::is_key_event;
use super::keys::is_press;
use super::keys::key_from_vk;
use super::keys::mouse_button_of;
use super::keys::unpack_pos;
use super::keys::unpack_size;
use super::keys::wheel_lines;

/// 事件循环。
///
/// 拥有「本帧事件」缓冲：[`EventLoop::poll`] 写，
/// [`EventLoop::egui_input`] 读。
pub struct EventLoop {
    pub(super) hwnd: HWND,
    /// 事件循环创建时刻，用作 egui 的时间原点。
    pub(super) start: Instant,
    /// 上次 poll 得到的缩放比例。
    pub(super) scale_factor: f32,
    /// 上次 poll 得到的客户区尺寸（逻辑点）。
    pub(super) inner_size: Vec2,
    /// 本帧事件缓冲。
    ///
    /// NOTE: 刻意**不在** [`Self::poll`] 开头清空，见该方法内的说明。
    pub(super) pending: Vec<WindowEvent>,
    /// 本帧观察到的修饰键状态，用于发 `ModifiersChanged`。
    pub(super) modifiers: Modifiers,
    /// 键盘焦点状态，对应 `RawInput::focused`。
    pub(super) focused: bool,
    /// 已收到 `WM_QUIT`。
    pub(super) quit: bool,
    /// 本循环是否负责收取 [`WM_QUIT`]。
    ///
    /// # 为什么需要这个标志
    ///
    /// `WM_QUIT` 是**线程级**消息，不属于任何窗口 ⇒ `PeekMessageW` 按
    /// hwnd 过滤**收不到**它，必须单独用不过滤的 `hwnd = None` 去peek。
    /// 而那一行是 `PM_REMOVE`：**谁先 poll 谁就把消息摘走**。
    ///
    /// 本项目有 1 + N 个 `EventLoop`（主窗口 + 弹窗 + 设置窗 + 卡片子窗），
    /// 它们每帧都 poll。若不加归属，弹窗/子窗完全可能先摘走 `WM_QUIT`
    /// ⇒ 它被记进**那个窗口**的 `quit` 标志与事件缓冲，而主窗口的
    /// `quit_requested()` 恒为 false ⇒ **点关闭 / 托盘退出后程序不退出**
    /// （那个窗口还顺手把自己的 CloseRequested 当成「关我」处理了：
    /// 弹窗会收搜索面板、子窗会把卡片收回工作区）。
    ///
    /// 只有主窗口用 [`Self::new_primary`] 建，其余用 [`Self::new`]。
    pub(super) owns_quit: bool,
    /// 告知 egui 的纹理尺寸上限，见 [`Window::max_texture_side`]。
    pub(super) max_texture_side: usize,
    /// 应用请求的下次重绘间隔。
    pub(super) repaint_after: Option<Duration>,
    /// UTF-16 代理项配对状态。BMP 外的字符由两条 `WM_CHAR` 组成。
    pub(super) decoder: CharDecoder,
}

impl EventLoop {
    /// 为给定窗口创建事件循环（**不**负责收取 `WM_QUIT`）。
    ///
    /// 附属窗口（弹窗 / 设置窗 / 卡片子窗）一律用这个。
    pub fn new(window: &Window) -> Self {
        Self::build(window, false)
    }

    /// 主窗口专用：额外负责收取 [`WM_QUIT`]。
    ///
    /// 全进程**只能有一个**这样的循环。`WM_QUIT` 是线程级消息，谁先
    /// `poll` 谁摘走（见 [`Self::owns_quit`] 的说明），所以这个归属必须
    /// 显式指定，不能靠「谁先跑」碰运气。
    pub fn new_primary(window: &Window) -> Self {
        Self::build(window, true)
    }

    fn build(window: &Window, owns_quit: bool) -> Self {
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
            owns_quit,
            // 从窗口复制；主窗口在 `Gpu` 就绪后还会用
            // [`Self::set_max_texture_side`] 覆盖一次（它的 `EventLoop`
            // 建在 `Gpu::new` 之前）。
            max_texture_side: window.max_texture_side(),
            repaint_after: None,
            decoder: CharDecoder::new(),
        }
    }

    /// 覆盖 egui 纹理上限。见 [`Window::set_max_texture_side`]。
    pub fn set_max_texture_side(&mut self, side: u32) {
        self.max_texture_side = (side as usize).max(2048);
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

    /// 客户区尺寸（**逻辑点**），直接问系统取当前真实值。
    ///
    /// 不用缓存的 [`Self::inner_size`]：那个字段只在收到 `WM_SIZE`
    /// 时更新，而跨进程 `MoveWindow` 触发的 `WM_SIZE` **不进本进程队列**
    /// （见 [`Self::poll`] 的说明），于是缓存会停在旧值。
    /// 这里每次都走 `GetClientRect`，是唯一可靠的来源。
    ///
    /// 客户区为 0x0（最小化 / 被 resize 到 0）时返回 `(0.0, 0.0)`，
    /// 调用方据此跳过渲染。
    fn client_size_points(&self) -> (f32, f32) {
        let mut r = RECT::default();
        if unsafe { GetClientRect(self.hwnd, &mut r) }.is_err() {
            return (0.0, 0.0);
        }
        let w = (r.right - r.left).max(0) as f32;
        let h = (r.bottom - r.top).max(0) as f32;
        let s = self.scale_factor;
        if s <= 0.0 {
            return (0.0, 0.0);
        }
        (w / s, h / s)    }

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
    /// # NOTE: 事件不丢的保证
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
        // NOTE: 这里**不再**每轮清零 `self.modifiers`（G9）。
        //
        // `pending` 是**跨轮累积**的，而 `poll_for` 一轮里最多调两次
        // `poll()` 并丢弃返回值。清零会让「上一轮读到的 `Key` 事件」在
        // 交付时拿到**全false** 的修饰键 —— 混合批次（先 Ctrl+点击、
        // 后 Ctrl 抬起）里，Key 事件就会带着错误的修饰键。
        //
        // 不清零的代价是「所有键都松开后 `self.modifiers` 仍是旧值」，
        // 但这个字段只作为「本批含 `Key` 事件时的兜底」，而：
        // 1. 循环内每个 `Key` 事件都会被事件自带的 modifiers 覆盖
        //    （那是按下瞬间的精确状态，更可靠）；
        // 2. 不含 `Key` 事件的批次走 `current_modifiers()` 实时查询。
        //
        // 所以「保留旧值」严格优于「每轮清零」。

        // 焦点可能在两轮之间丢失（Alt+Tab 到别的程序），每轮重查。
        self.focused = unsafe { GetForegroundWindow() } == self.hwnd;

        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // 单独用于探测 `WM_QUIT`（它不属于任何窗口，按 hwnd 过滤收不到）。
        let mut quit_msg: MSG = unsafe { std::mem::zeroed() };
        let mut events: Vec<WindowEvent> = Vec::new();

        // ---- 尺寸主动同步（不能只依赖 WM_SIZE）--------------------------
        //
        // NOTE: 这是「UI 只画在客户区左上角、不随窗口缩放」的真根因。
        //
        // `translate` 里的 `WM_SIZE` 分支是对的，但**它经常收不到**：
        // `MoveWindow` / `SetWindowPos` 这类改变窗口尺寸的调用，若来自
        // **另一个进程**（本项目的 `scripts/verify_render.ps1` 与
        // `measure_layout.ps1` 就是这样驱动窗口的），系统会用
        // `SendMessage` 把 `WM_SIZE` **直接发给窗口过程**，
        // 它**不经过本进程的消息队列**，`PeekMessageW` 永远读不到。
        // 实测：把客户区从 420x560 拖到 900x800 后，队列里出现过
        // `WM_NCCALCSIZE`(0x60) 与 `WM_WINDOWPOSCHANGED`(0x47)，
        // **唯独没有 `WM_SIZE`(0x5)**——于是 `inner_size` 永远停在
        // 创建时的 280x373.3 逻辑点。
        //
        // 后果链：`screen_rect` 恒为旧尺寸 → egui 按 280x373.3 布局 →
        // 顶点只覆盖视口左上角，右侧与底部留着 clear color，
        // 且**不产生任何 GPU 错误**，只是「画小了」。
        //
        // 修法：每轮用 `GetClientRect` 直接问系统真实尺寸（O(1) 系统调用），
        // 与消息无关，任何来源的 resize 都能跟上。
        // 消息分支保留，它额外携带 `wParam` 的最小化/还原语义。
        let scale_now =
            dpi_scale_factor(unsafe { GetDpiForWindow(self.hwnd) }, self.scale_factor);
        if (scale_now - self.scale_factor).abs() > f32::EPSILON {
            self.scale_factor = scale_now;
            events.push(WindowEvent::ScaleFactorChanged(scale_now as f64));
        }
        let (cur_w, cur_h) = self.client_size_points();
        let size_changed = (cur_w - self.inner_size.x).abs() > f32::EPSILON
            || (cur_h - self.inner_size.y).abs() > f32::EPSILON;
        if cur_w > 0.0 && cur_h > 0.0 && size_changed {
            self.inner_size = Vec2::new(cur_w, cur_h);
            events.push(WindowEvent::Resized {
                width: cur_w,
                height: cur_h,
            });
        }

        // IMPORTANT: **必须按 `hwnd` 过滤**。
            //
            // 早前传 `None`（不过滤），于是这个循环会抽干**整个线程**
            // 消息队列——包括属于其它窗口的消息。
            //
            // 单窗口时看不出问题；一旦有第二个窗口（卡片子窗口、
            // 设置窗口），**谁先 poll 谁就抢走全部消息**，其余窗口
            // 永远收不到输入。表现为：设置窗口画出来了但完全不响应
            // 鼠标/键盘，连 resize 都不知道（于是 `screen_rect` 停在
            // 初始值，界面尺寸不对）。
            //
            // `EventLoop` 持有自己的 `hwnd`（构造时传入），用它过滤
            // 即可。
            while unsafe { PeekMessageW(&mut msg, Some(self.hwnd), 0, 0, PM_REMOVE) }.as_bool() {
            // WM_QUIT 不属于任何窗口，按 hwnd 过滤**收不到它**。
            // 单独用不过滤的 peek 检查一次。
            //
            // CAUTION: **只有 `owns_quit` 的那个循环能做这件事**。这一行是
            // `PM_REMOVE`，全线程只有一条 `WM_QUIT`，谁摘走谁的。附属窗口
            // 也 peek 的话，弹窗/子窗可能先摘走 ⇒ 主窗口 `quit_requested()`
            // 恒 false（程序退不掉），而那个窗口还会把自己的
            // `CloseRequested` 误当成「关我」（弹窗收搜索面板 / 子窗收回卡片）。
            // 见 `EventLoop::owns_quit` 字段的说明。
            if self.owns_quit
                && unsafe { PeekMessageW(&mut quit_msg, None, WM_QUIT, WM_QUIT, PM_REMOVE) }
                    .as_bool()
            {
                self.quit = true;
                events.push(WindowEvent::CloseRequested);
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

            // 另一个进程请求唤起本窗口（单实例：用户又双击了一次图标）。
            if msg.message == WM_SHOW_MAIN {
                events.push(WindowEvent::ShowRequested);
                continue;
            }

            // 另一个进程请求隐藏本窗口（热键 toggle 的收起方向）。
            if msg.message == WM_HIDE_MAIN {
                events.push(WindowEvent::HideRequested);
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
    pub(super) fn absorb(&mut self, events: Vec<WindowEvent>) {
        // IMPORTANT: 必须**追加**而非覆盖：`poll_for` 也会走到这里，
        // 覆盖会把上一轮累积的未交付事件抹掉（见 `poll` 的说明）。
        self.pending.extend_from_slice(&events);
    }

    /// 睡眠式轮询：有消息立即返回，空闲时最多阻塞 `timeout`。
    ///
    /// 抽干线程队列里**属于本窗口**的残留 posted 消息。
    ///
    /// # 为什么需要它（2026-10-19「连点设置按钮未响应」的第二根因）
    ///
    /// [`Self::poll`] 按 `hwnd` 过滤消息。窗口销毁后，它存活期间入队的
    /// posted 消息（鼠标移动 / `WM_PAINT` / `WM_TIMER` / 窗口位置变化……）
    /// 就再没有主人的 `hwnd` 可匹配——**永远摘不走**，滞留线程队列。
    ///
    /// 设置窗口 / 弹窗 / 卡片子窗都是「开关即 create/destroy」的生命周期，
    /// 连点齿轮几十轮就是几千条孤儿消息；Windows 的线程消息队列有上限
    /// （万条量级），逼近上限时 `PostMessage` 开始失败、`GetMessage`
    /// 行为异常，系统随即把线程判定为「未响应」。
    ///
    /// 在销毁窗口**之前**（hwnd 仍有效）抽干，孤儿消息就不产生。
    /// 代价是一次 O(队列中本窗口消息数) 的扫描，窗口都要关了，无所谓。
    pub fn drain_pending(&self) {
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        let mut n = 0u32;
        // 上限防护：万一有别的来源持续投递（理论上不会——窗口即将销毁、
        // 没有泵在跑），也不能在这里转成死循环。
        while n < 10_000
            && unsafe { PeekMessageW(&mut msg, Some(self.hwnd), 0, 0, PM_REMOVE) }.as_bool()
        {
            n += 1;
        }
        if n > 0 {
            tracing::debug!(hwnd = ?self.hwnd.0, n, "抽干窗口残留消息");
        }
    }

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
        // NOTE: 不在入口缓存 `scale`——同一批消息里 WM_DPICHANGED 可能
        // 先到，之后的 MouseMoved/MouseButton 必须用**新**比例换算
        // （跨屏拖动时 DPI 变化与鼠标移动同帧到达是常态）。各分支直接
        // 读 `self.scale_factor`。
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
                let scale = self.scale_factor;
                let w = w_px / scale;
                let h = h_px / scale;
                self.inner_size = Vec2::new(w, h);
                out.push(WindowEvent::Resized {
                    width: w,
                    height: h,
                });
            }

            // DPI 变了（跨显示器拖动 / 系统改缩放）。两件事必须都做：
            //
            // 1. **按系统建议矩形 SetWindowPos**（lParam 指向 `RECT`，
            //    物理像素）——这是微软推荐做法，效果是**逻辑尺寸恒定**：
            //    100%→200% 时窗口物理尺寸翻倍，逻辑客户区不变。不做的
            //    话窗口保持物理尺寸 ⇒ 逻辑面积塌成 1/4（720×800 逻辑点
            //    变 360×400），solver 按比例压缩卡片、UI 密度突变
            //    （2026-10-19 DPI 治理，P1-1）。
            // 2. 更新 scale 并报事件。
            //
            // SetWindowPos 会再送来 WM_SIZE（物理尺寸变了），`inner_size`
            // 由那条消息按新 scale 更新，这里不动。
            WM_DPICHANGED => {
                let new_scale =
                    dpi_scale_factor(unsafe { GetDpiForWindow(self.hwnd) }, self.scale_factor);
                if (new_scale - self.scale_factor).abs() > f32::EPSILON {
                    self.scale_factor = new_scale;
                    let r = unsafe { &*(msg.lParam.0 as *const RECT) };
                    let w = r.right - r.left;
                    let h = r.bottom - r.top;
                    unsafe {
                        let _ = SetWindowPos(
                            self.hwnd,
                            None,
                            r.left,
                            r.top,
                            w,
                            h,
                            SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
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
                    pos: unpack_pos(msg.lParam.0, self.scale_factor),
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
                    pos: unpack_pos(msg.lParam.0, self.scale_factor),
                });
            }

            // 有别的窗口抢走了鼠标（如菜单弹出、窗口被切走）。补一个抬起
            // 事件，否则控件会一直停在「按下」态。
            WM_CAPTURECHANGED => {
                if unsafe { GetCapture() } != self.hwnd {
                    let pos = self.pointer_in_points(self.scale_factor).unwrap_or(Pos2::ZERO);
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

            // ---- IME 会话 ------------------------------------------------
            //
            // 只转发会话状态，不改文本：`WM_IME_CHAR` 已经把最终提交的
            // 字符送进 `TextInput`，这里再送一遍会出现重复字符。
            WM_IME_STARTCOMPOSITION => out.push(WindowEvent::Ime(WindowIme::Start)),

            WM_IME_COMPOSITION => {
                // 预编辑串不在 wParam 里，要用 ImmGetCompositionString 读。
                // GCS_COMPSTR 是「当前预编辑串」，GCS_COMPREADSTR 是选中的
                // 子串；这里只取前者，候选高亮由 IME 自己负责画。
                out.push(WindowEvent::Ime(WindowIme::Composition(
                    self.ime_composition_string(),
                )));
            }

            WM_IME_ENDCOMPOSITION => {
                // 结束时的提交结果同样走ImmGetCompositionString；
                // 拿不到就交空串，让上层按「会话结束」收尾。
                out.push(WindowEvent::Ime(WindowIme::End(
                    self.ime_composition_string(),
                )));
            }

            _ => {}
        }
    }

    /// 读当前 IME 预编辑串（`GCS_COMPSTR`）。
    ///
    /// 拿不到时返回空串而不是报错：IME 会话可能已结束、焦点在别处，
    /// 或 HIMC 为空——这些都属于正常情况，调用方按空串处理即可。
    fn ime_composition_string(&self) -> String {
        // SAFETY：`hwnd` 由本结构持有且窗口未销毁；
        // `ImmGetContext` 失败时 `himc` 为空，直接返回。
        // `ImmGetCompositionStringW` 的长度查询与读取是两次调用，
        // 期间缓冲由本函数独占，不会被别的线程改写。
        unsafe {
            // NOTE: `HIMC` 是 newtype 包装的 `isize`，**没有** `is_null()`。
            // 用 `Default::default()`（即空句柄）判断，等价于 Win32 的 NULL。
            let himc = ImmGetContext(self.hwnd);
            if himc == Default::default() {
                return String::new();
            }
            let len = ImmGetCompositionStringW(himc, GCS_COMPSTR, None, 0);
            let out = if len > 0 {
                let mut buf = vec![0u16; len as usize];
                let got = ImmGetCompositionStringW(
                    himc,
                    GCS_COMPSTR,
                    Some(buf.as_mut_ptr() as *mut _),
                    // 返回值与缓冲区长度都是 `i32`/`u32`——Win32 的
                    // `DWORD` 在 windows crate 里映射成 u32，
                    // 直接传 `len`(i32) 会类型不匹配。
                    len as u32,
                );
                if got > 0 {
                    buf.truncate(got as usize);
                    String::from_utf16_lossy(&buf)
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            // `ImmReleaseContext` 返回 BOOL 且标了 `must_use`。
            // 释放失败（IME 已被系统回收）无害，但不能留未使用的返回值
            // 触发警告——这里显式丢弃。
            let _ = ImmReleaseContext(self.hwnd, himc);
            out
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
    /// # NOTE: 为什么事件是参数而不是读`self.pending`
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
    ///
    /// # IMPORTANT: 坐标系不变式：`screen_rect * ppp` 必须等于客户区物理像素
    ///
    /// 本函数喂给 egui 的 `screen_rect` 是**逻辑点**，而渲染侧
    /// [`crate::pipeline::Uniforms`] 的 `size_in_pixels` 是**物理像素**。
    /// 着色器按`ndc = 2 * (pos * dpr) / size_in_pixels` 换算，所以三者必须
    /// 满足
    ///
    /// ```text
    /// screen_rect.size() * dpr == size_in_pixels
    /// ```
    ///
    /// 这也是 egui 官方后端（`egui-wgpu` 0.36）的做法：顶点与
    /// `screen_rect` 一律逻辑点，uniform 里放`size_in_pixels / ppp`
    /// （即逻辑点），顶点着色器**不乘** dpr——两种写法等价。
    ///
    /// 实测踩坑：本函数早先只设了 `screen_rect`（逻辑点）却**从不调**
    /// [`egui::Context::set_pixels_per_point`]，于是 egui 的 `ppp` 恒为
    /// 默认的 1.0，`dpr` 传进 uniform 也是 1.0。280 点的内容被映射到
    /// 420 像素宽的视口里，只占 2/3——**右侧 1/3 与底部 1/3 永远是
    /// clear color**，看起来就像「UI 挤在左上角」。
    ///
    /// 这类缺陷不报任何 GPU 错误，只是「画小了」，因此必须靠上面那条
    /// 不变式当判据，而不是靠肉眼看整体比例。
    pub fn egui_input(&self, ctx: &egui::Context, events: &[WindowEvent]) -> egui::RawInput {
        // IMPORTANT: 必须与下面的 `screen_rect` **成对**设置。
        // 少这一句就是「UI 只画左上角 2/3」的根因。
        //
        // egui 的 `ppp` 决定两件事：字体图集的栅格化分辨率，以及
        // `tessellate` 的 feathering/对齐；它**不**缩放顶点坐标
        // （`epaint::Vertex::pos` 的文档明确写的是逻辑点）。
        // 顶点 → 渲染目标像素的换算只发生在着色器里那一次 `* dpr`。
        //
        // # 超采样（SSAA）
        //
        // 这里乘上 [`crate::render_scale()`] 是本项目的抗锯齿方案：
        // `ppp` 提高 N 倍 ⇒ 字体图集按 N 倍分辨率栅格化、feathering
        // 也按 N 倍细分；渲染目标同步放大 N 倍，最后线性降采样回交换链。
        //
        // 为什么不用 MSAA：MSAA 只作用于**几何边缘**，而字形的锯齿来自
        // **纹理采样**（字体图集是覆盖率图），MSAA 对它毫无作用。
        // 超采样同时改善几何与文字，是本项目「圆角与字体都要顺」诉求的
        // 唯一正确解。
        //
        // IMPORTANT: `screen_rect`（逻辑点）**不变**——它只由窗口的
        // 真实 DPI 决定。变的只有 `ppp` 与渲染目标尺寸，于是布局完全
        // 不受影响，只有光栅化分辨率提高。
        // 不变式随之变为 `screen_rect.size() * ppp == 渲染目标像素`。
        //
        // `set_pixels_per_point` 内部会转成 `set_zoom_factor(ppp /
        // native_ppp)`，而本项目从不设 `native_ppp`（恒为 1.0），
        // 所以这里传 `scale_factor * render_scale` 即得该 ppp。
        //
        // 幂等：值没变时 `set_zoom_factor` 直接返回，不触发重绘请求，
        // 因此每帧调用无代价。
        ctx.set_pixels_per_point(
            self.scale_factor * crate::render_scale_for(self.scale_factor),
        );

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
                        // Win32 WM_MOUSEWHEEL 只来自物理滚轮。
                        source: MouseWheelSource::Wheel,
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
                // IME → egui 的 IME 事件。
                //
                // IMPORTANT: 必须用 `commit_string`：这里传的是**预编辑串**，
                // egui 拿到后会用它画下划线候选与预编辑文本。
                // 若改传 `preedit` 之外的模式，候选窗能弹但看不到拼音。
                //
                // 位置用光标的客户区坐标：egui 据此把系统候选窗
                // 摆到光标下方，位置错了会出现在屏幕角落。
                WindowEvent::Ime(ime) => {
                    // 候选窗要摆在光标下方，所以必须带上光标位置。
                    // 拿不到就用客户区左上角退化——位置不理想，
                    // 但不会出现「候选窗在屏幕外、用户够不着」。
                    let caret = self
                        .pointer_in_points(self.scale_factor)
                        .unwrap_or(Pos2::ZERO);
                    // egui 0.36 的类型名是 `ImeEvent`（不是 `IME`），
                    // 且没有 `Start` 变体——会话开始不通知，
                    // 只用 `Preedit{ text: "" }` 表示「IME 已收起」。
                    out.push(EguiEvent::Ime(match ime {
                        WindowIme::Start => egui::ImeEvent::Preedit {
                            text: String::new(),
                            active_range_chars: None,
                        },
                        // 这里必须用 `Preedit`：预编辑串要显示下划线，
                        // 拼音/候选高亮全靠它。误用 `Commit` 会让用户
                        // 每敲一个字母就直接上屏，无法反悔。
                        WindowIme::Composition(s) => egui::ImeEvent::Preedit {
                            text: s.clone(),
                            active_range_chars: None,
                        },
                        // 提交结果：为空表示会话结束且未提交任何内容。
                        WindowIme::End(s) => egui::ImeEvent::Commit(s.clone()),
                    }));
                    // egui 0.36 里是 `PointerMoved`（`CursorMoved` 已改名）。
                    out.push(EguiEvent::PointerMoved(caret));
                }
                WindowEvent::Focused(b) => {
                    out.push(EguiEvent::WindowFocused(*b));
                }
                // 尺寸与缩放通过 screen_rect 表达；关闭是应用层语义；
                // 最小化同理——客户区已是 0×0，`screen_rect` 会自动塌成空。
                //
                // `ShowRequested` / `HideRequested` 也是应用层语义
                //（由主循环处理成显示/隐藏），不进 egui。
                WindowEvent::Resized { .. }
                | WindowEvent::ScaleFactorChanged(_)
                | WindowEvent::Minimized(_)
                | WindowEvent::CloseRequested
                | WindowEvent::ShowRequested
                | WindowEvent::HideRequested => {}
            }
        }

        // egui 靠这个事件更新修饰键状态（含 Win 键 → command）。
        if modifiers != ctx.input(|i| i.modifiers) {
            out.push(EguiEvent::ModifiersChanged(modifiers));
        }

        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.inner_size)),
            // IMPORTANT: 必须显式给出纹理上限。留空会退回 egui 的默认值
            // 2048，而超采样下中文字体图集需要远大于此的边长——
            // 图集塞不下的表现是**字形画错**（图标形状不对），不报错。
            // 见 [`Window::max_texture_side`] 的说明。
            max_texture_side: Some(self.max_texture_side),
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
