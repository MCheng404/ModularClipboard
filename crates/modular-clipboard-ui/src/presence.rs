//! 后台常驻：把托盘与全局快捷键翻译成应用级动作。
//!
//! # 与主窗口消息循环的关系
//!
//! [`tray::start_with_hotkeys`] 自带**独立线程 + message-only 窗口**，
//! 事件经 channel 回传主线程（见 `tray` 模块的架构决策）。
//! 本模块只在帧循环里做一次 `try_recv` 排空，**不新建消息循环**，
//! 也不改动 `gfx::window` 的消息处理。
//!
//! # 调用链
//!
//! ```text
//! 托盘线程: 窗口过程 → channel
//! 主线程:   Resident::poll → Frame::absorb → 动作
//! ```

use std::time::Duration;

use modular_clipboard_platform::hotkey;
use modular_clipboard_platform::tray::{self, TrayEvent};
use modular_clipboard_core::config::WindowBackdrop;
use windows::Win32::Foundation::HWND;
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsWindow, SetForegroundWindow, SetWindowPos,
    ShowWindow, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SW_HIDE, SW_MINIMIZE,
    SW_RESTORE, IDC_ARROW, IDC_IBEAM, IDC_HAND,
};
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, LoadCursorW, SetCursor, SetWindowDisplayAffinity, SetWindowLongPtrW,
    GWL_EXSTYLE, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WS_EX_LAYERED,
};

/// 查询光标当前**屏幕坐标**（物理像素）。失败返回 `None`。
///
/// 用途：磨砂光晕（`ui.glow`）要「光标靠近窗口时在光标位置渲染」，
/// 而 egui 的 `pointer.hover_pos()` 只在光标**位于本窗口内**时有效——
/// 靠近但尚未进入的那一段窗口收不到任何鼠标事件，只能自己查。
/// 系统 UI 语言的完整 LANGID（`GetUserDefaultUILanguage`）。
///
/// 供 [`modular_clipboard_core::i18n::from_system_langid`] 判定
/// 「跟随系统」时该用中文还是英文。取 UI 语言而不是区域设置：
/// 用户把系统显示语言切成 English 时，界面跟着切才是「跟随系统」。
pub fn system_langid() -> u16 {
    // SAFETY: 纯查询，无入参，无未定义行为。
    unsafe { GetUserDefaultUILanguage() }
}

pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut pt = windows::Win32::Foundation::POINT::default();
    // SAFETY: `pt` 是栈上有效的可写内存。CAUTION: `GetCursorPos` 在
    // `WindowsAndMessaging` 而不是 `Input::KeyboardAndMouse`（容易找错模块）。
    let ok = unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt) };
    ok.is_ok().then_some((pt.x, pt.y))
}

/// 窗口隐藏时的轮询间隔。
///
/// 隐藏后不再渲染，但**必须继续醒来查托盘事件**，否则永远唤不回来。
/// 200ms 是权衡：唤醒延迟无感（点击到出现），
/// 同时不把一个核跑满（100% 与 5Hz 的耗电差两个数量级）。
pub const HIDDEN_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 退出前给托盘线程跑完 `NIM_DELETE` 的时间。
///
/// [`Drop`] 只投递 `WM_QUIT` 后立即返回，托盘线程要等自己从
/// `GetMessageW` 醒来才能撤图标。进程直接结束会让图标残留到
/// shell 察觉为止，用户重启后可能看到「已退出却还在托盘」。
const TRAY_SHUTDOWN_GRACE: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// 纯逻辑层：不触碰 Win32，可直接单测
// ---------------------------------------------------------------------------

/// 托盘/快捷键事件翻译出的应用级动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 唤起主窗口（托盘左键、菜单「显示主窗口」、全局快捷键）。
    Show,
    /// 把上一个条目直接粘贴到前台窗口（不唤起界面）。
    PastePrevious,
    /// 迷你面板里选中了某个条目（携带 id），直接复制它。
    MiniPick(i64),
    /// 清空全部历史。
    ClearHistory,
    /// 退出进程。
    Quit,
}

/// 单个 [`TrayEvent`] 对应的动作。
///
/// `Show`、`Hotkey` 与 `HotkeyShown` **必须**映射到同一个
/// [`Action::Show`]：三条入口走同一条唤起路径，才能保证「左键能唤起，
/// 快捷键也一定能唤起」——分成两个动作就会各自演化出不同的前置条件，
/// 最终表现为「有时按快捷键没反应」。`HotkeyShown` 在托盘线程已直接
/// 完成显示与置前，这里再做一次 `focus_window` 是幂等的双保险
/// （单进程模式下还能在首次置前失败时兜底）。
pub const fn action_of(ev: TrayEvent) -> Action {
    match ev {
        TrayEvent::Show | TrayEvent::Hotkey | TrayEvent::HotkeyShown => Action::Show,
        // 「粘贴上一条」不经过界面：直接写剪贴板 + 发 Ctrl+V 给前台窗口。
        // 唤起界面再让用户选一条，就失去这个快捷键的意义了。
        TrayEvent::HotkeyPaste => Action::PastePrevious,
        TrayEvent::MiniPick(id) => Action::MiniPick(id),
        TrayEvent::ClearHistory => Action::ClearHistory,
        TrayEvent::Quit => Action::Quit,
    }
}

/// 窗口收到关闭请求时的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// 隐藏到托盘，进程继续在后台常驻。
    HideToTray,
    /// 真正退出进程。
    Quit,
}

/// 关闭请求该隐藏还是该退出。
///
/// # 为什么 `WM_CLOSE` 隐藏、`WM_QUIT` 退出
///
/// `gfx::window` 把两者都翻译成同一个 `WindowEvent::CloseRequested`
/// （刻意如此：它不替应用决定去留）。区分依据是
/// [`gfx::window::EventLoop::quit_requested`],它**只在** `WM_QUIT` 时置位。
/// 纯逻辑层因此把该标志作为独立入参——若靠「事件类型」判断，
/// 就必须去读 `EventLoop` 的内部状态，测不了。
///
/// 托盘不可用时（图标没挂上）只能退出：否则用户点关闭按钮后
/// 窗口消失、进程还在、也没有任何办法把它唤回来——应用变成僵尸。
pub const fn decide_on_close(tray_available: bool, wm_quit: bool) -> CloseDecision {
    if wm_quit || !tray_available {
        CloseDecision::Quit
    } else {
        CloseDecision::HideToTray
    }
}

/// 一帧内累积的托盘意图。
///
/// [`tray::Tray::poll`] 是一次性排空 channel，返回的是**这期间积压的
/// 全部事件**，可能重复（用户连点两下托盘）。若逐条直接执行，
/// 会重复唤起窗口、重复清库。归约到一份意图即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Frame {
    /// 需要唤起主窗口（只需一次）。
    pub show: bool,
    /// 需要把上一个条目粘贴到前台窗口。
    pub paste_previous: bool,
    /// 迷你面板选中的条目 id。
    pub mini_pick: Option<i64>,
    /// 需要清空历史。
    pub clear_history: bool,
    /// 需要退出进程。
    pub quit: bool,
}

impl Frame {
    /// 吸收一个托盘事件。
    ///
    /// `Quit` 吸收后本帧的其余动作仍会保留在结构体里，由调用方决定
    /// 是否执行——**这里不做短路**。理由：`ClearHistory` 已经产生的
    /// 意图被静默丢弃会让「清空」看起来时灵时不灵；
    /// 是否提前退出属于流程控制，不属于「意图归约」。
    pub fn absorb(&mut self, ev: TrayEvent) {
        match action_of(ev) {
            Action::Show => self.show = true,
            Action::PastePrevious => self.paste_previous = true,
            Action::MiniPick(id) => self.mini_pick = Some(id),
            Action::ClearHistory => self.clear_history = true,
            Action::Quit => self.quit = true,
        }
    }

    /// 本帧是否没有任何待办事项。
    pub fn is_empty(&self) -> bool {
        !self.show
            && !self.paste_previous
            && self.mini_pick.is_none()
            && !self.clear_history
            && !self.quit
    }
}

/// 从一批事件构造 [`Frame`]。
pub fn frame_of(events: impl IntoIterator<Item = TrayEvent>) -> Frame {
    let mut f = Frame::default();
    for ev in events {
        f.absorb(ev);
    }
    f
}

// ---------------------------------------------------------------------------
// Win32 层
// ---------------------------------------------------------------------------

/// 托盘控制器。帧循环每帧调 [`Resident::poll`] 取事件。
pub struct Resident {
    /// `Option` 而非直接持有：显式 [`Resident::shutdown`] 需要把它
    /// 移出来，才能在销毁渲染资源**之前**撤掉图标。
    /// `Tray` 不实现 `Default`，因此用 `Option` + `take` 而非 `mem::take`。
    tray: Option<tray::Tray>,
}

impl Resident {
    /// 添加托盘图标并按给定组合注册全局快捷键。
    ///
    /// 阻塞至图标真正出现在通知区后才返回。
    ///
    /// 组合**必须由调用方传入**（而不是在这里读配置）：单进程模式的
    /// 托盘注册发生在启动早期，此时配置刚反序列化完，调用方本来就
    /// 持有它；在这里再读一次要么重复 IO，要么读到旧值。
    pub fn start(
        tooltip: &str,
        toggle: hotkey::HotkeySpec,
        paste_previous: hotkey::HotkeySpec,
    ) -> anyhow::Result<Self> {
        let tray = tray::start_with_hotkeys(tooltip, toggle, paste_previous)?;
        tracing::info!(
            toggle = %hotkey::describe(toggle),
            paste = %hotkey::describe(paste_previous),
            "托盘已就绪"
        );
        Ok(Self {
            tray: Some(tray),
        })
    }

    /// 启动时注册失败的快捷键描述（空 = 全部成功）。
    pub fn failed_hotkeys(&self) -> &[String] {
        self.tray
            .as_ref()
            .map(|t| t.failed_hotkeys())
            .unwrap_or_default()
    }

    /// 取出这段时间内积压的全部事件，并归约成一份意图。
    ///
    /// 已 shutdown 时返回空帧——退出路径上还会再取一次，不能 panic。
    pub fn poll(&self) -> Frame {
        match self.tray.as_ref() {
            Some(t) => frame_of(t.poll()),
            None => Frame::default(),
        }
    }

    /// 主动撤除托盘图标并回收托盘线程。可重复调用。
    pub fn shutdown(&mut self) {
        let Some(tray) = self.tray.take() else {
            return;
        };
        drop(tray);
        // 托盘线程收到 WM_QUIT 后才会发 NIM_DELETE。
        // 不给这段时间，进程结束会让图标残留到 shell 察觉，
        // 用户重启后可能看到「已退出却还在托盘」。
        std::thread::sleep(TRAY_SHUTDOWN_GRACE);
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        // 走自动析构的正常退出路径时也要撤图标。
        // 不 sleep：此刻渲染资源已在销毁流程中，没有可观测的等待价值。
        drop(self.tray.take());
    }
}

/// 唤起主窗口：从最小化/隐藏恢复并置于前台。
///
/// 托盘左键、「显示」菜单项、重复双击 exe、`WM_SHOW_MAIN` 都走这里；
/// 失焦隐藏后的唤回也走这里。
///
/// `IsWindow` 检查是防御性的：正常流程下窗口永不销毁
/// （关闭按钮走 [`CloseDecision::HideToTray`]），但若将来有人改成
/// 真的销毁窗口，这里拿失效句柄调 Win32 会静默失败并留下
/// 「点了没反应」的现象，不如明确记一条日志。
///
/// `SetForegroundWindow` 受 Windows 前台锁限制：后台进程通常无权直接
/// 抢前台，只会闪一下任务栏。对全局快捷键场景，托盘线程已在 WM_HOTKEY
/// 授权窗口内直接激活过一次（见 `platform::activate_window_by_title`）；
/// 本函数是其余唤起路径的兜底，必须把**整个显示+激活序列**包在
/// `AttachThreadInput` 里，任何一步缺失都会在某些 Windows 版本上
/// 「弹出来了但压在当前应用下面」：
///
/// 1. 附加输入队列（必须在 `ShowWindow` **之前**）；
/// 2. `ShowWindow(SW_RESTORE)` 恢复最小化/隐藏；
/// 3. `SetWindowPos` 按置顶身份插回正确的 Z 序层；
/// 4. `SetForegroundWindow` + `BringWindowToTop` + `SetFocus`；
/// 5. 断开输入队列；
/// 6. 仍失败则模拟一次 Alt 键（系统据此认为用户主动发起了前台切换）
///    再重试，最后用 `FlashWindowEx` 让任务栏图标闪烁。
pub fn focus_window(hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, SetFocus, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, FlashWindowEx, FLASHW_ALL, FLASHW_TIMERNOFG, FLASHWINFO, HWND_TOP,
        SWP_SHOWWINDOW, WS_EX_TOPMOST,
    };

    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        tracing::warn!("唤起失败：主窗口句柄已失效");
        return;
    }
    unsafe {
        let cur_thread = GetCurrentThreadId();
        let fg = GetForegroundWindow();
        // fg 为空（当前没有任何前台窗口）或已是本窗口时，无需附加。
        let fg_thread = if fg.0.is_null() || fg == hwnd {
            0
        } else {
            GetWindowThreadProcessId(fg, None)
        };
        let attached = fg_thread != 0
            && fg_thread != cur_thread
            && AttachThreadInput(cur_thread, fg_thread, true).as_bool();

        // 一次 SW_RESTORE 覆盖「最小化」与「隐藏」两种状态：
        // 对未最小化的隐藏窗口，它等价于 SW_SHOW。
        let _ = ShowWindow(hwnd, SW_RESTORE);

        // 按当前置顶身份插回 Z 序：置顶窗口必须进 HWND_TOPMOST 层，
        // 否则即便 ShowWindow 了，仍会被失焦期间升起的其他窗口压住；
        // SWP_SHOWWINDOW 保证隐藏态也被显示，SWP_NOACTIVATE 让真正的
        // 激活交给下面的 SetForegroundWindow。
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let z_after = if ex_style & WS_EX_TOPMOST.0 != 0 {
            HWND_TOPMOST
        } else {
            HWND_TOP
        };
        let _ = SetWindowPos(
            hwnd,
            Some(z_after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW | SWP_NOACTIVATE,
        );

        let mut ok = SetForegroundWindow(hwnd).as_bool();
        let _ = BringWindowToTop(hwnd);
        // SetFocus 对其他线程的窗口通常失败，附加输入队列后才生效；
        // 失败可忽略（SetForegroundWindow 已承担键盘焦点转移）。
        let _ = SetFocus(Some(hwnd));

        // 必须解除：连接期间本线程的消息会被送进前台线程的队列，
        // 忘记解除会造成键盘输入落到别人的窗口上。
        if attached {
            let _ = AttachThreadInput(cur_thread, fg_thread, false);
        }

        if !ok {
            // Alt 键技巧：系统把「按住 Alt」视为用户正在亲自切换窗口，
            // 此后的 SetForegroundWindow 不再受前台锁限制。
            keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
            keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
            ok = SetForegroundWindow(hwnd).as_bool();
            let _ = BringWindowToTop(hwnd);
        }

        if !ok {
            // 最终兜底：任务栏图标闪烁到窗口变为前台为止，
            // 至少让用户明确知道有窗口在请求注意。
            let fi = FLASHWINFO {
                cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                hwnd,
                dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
                uCount: 3,
                dwTimeout: 0,
            };
            let _ = FlashWindowEx(&fi);
        }
    }
}

/// 当前前台窗口是否属于**本进程**。
///
/// 失焦隐藏的二次确认：主窗口失去焦点时，新前台若仍是本进程窗口
/// （设置窗、托盘线程弹出的系统右键菜单/迷你面板等），这次「失焦」
/// 并不是切到别的程序，不应把主窗口藏起来。
///
/// 取不到前台窗口（焦点在桌面/无窗口）时按「非本进程」处理——
/// 那种情况隐藏到托盘符合「失焦即收起」的预期。
pub fn foreground_is_own_process() -> bool {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() {
        return false;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(fg, Some(&mut pid)) };
    pid != 0 && pid == unsafe { GetCurrentProcessId() }
}

/// 隐藏主窗口（不销毁），托盘继续常驻。
pub fn hide_window(hwnd: HWND) {    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        tracing::warn!("隐藏失败：主窗口句柄已失效");
        return;
    }
    // BOOL 是 Copy 类型，`drop` 对它不做任何事且会告警，用 `let _ =`。
    let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
}

/// 最小化主窗口（不销毁、不隐藏）。
///
/// # 与 `hide_window` 的区别
///
/// `SW_MINIMIZE` 只把窗口收进任务栏，**`IsWindowVisible` 仍为真**——
/// 托盘的「显示主窗口」仍能把它恢复。反过来 `SW_HIDE` 会让窗口彻底
/// 从任务栏与 Alt+Tab 消失，语义完全不同，两者不能互相顶替。
///
/// 无边框窗口（`WS_POPUP`）同样可以最小化：最小化是窗口管理器提供
/// 的行为，与有没有系统标题栏无关。
/// 按 egui 给出的光标形状设置鼠标指针。
///
/// egui 的 `PlatformOutput::cursor_icon` 早前被整个丢弃
/// （`run_ui` 的返回值只取了 `shapes` / `textures_delta` / `ppp`），
/// 于是**光标永远是默认箭头**——在搜索框上、在可点条目上、在文本上
/// 全都一模一样，「这里能点」没有任何视觉提示。
///
/// # 两个必须守住的边界
///
/// 1. **指针不在本窗口上时不动光标**。`SetCursor` 是 Win32 全局状态，
///    本窗口的帧循环若在指针悬停于其它窗口（拖出的卡片子窗口、
///    设置窗口）时也去设箭头，就会把那些窗口外壳的 resize 光标
///    覆盖掉——「resize 无光标指示」的成因之一。
/// 2. **指针落在窗口外壳（resize 边/角、拖动区）上时，听外壳的**。
///    egui 0.36 没有 `Resize*` 系列变体，指针悬停在边缘时它报
///    `Default`；直接套用下面的映射会把它覆盖回箭头——
///    这正是「调整窗口大小时无任何光标指示」的根因。
///    缩放光标由 `chrome` 模块（`WM_SETCURSOR` + [`shell_cursor_id`]）
///    决定，这里只负责把外壳没接管的客户区光标映射成 Win32 资源。
///
/// 只在**形状变化时**才调 `SetCursor`：它是 Win32 全局状态，
/// 每帧设一次既无必要也会和别的窗口抢光标。
pub fn set_cursor_icon(hwnd: HWND, icon: egui::CursorIcon) {
    use std::sync::atomic::{AtomicU32, Ordering};
    // 上一次设置的光标。0 = 从未设过。
    static LAST: AtomicU32 = AtomicU32::new(0);

    // 指针不在本窗口上：光标归别的窗口 / 系统管，这里绝不能碰。
    if !modular_clipboard_gfx::chrome::pointer_is_over(hwnd) {
        return;
    }

    // 指针在窗口外壳（resize 边/角、拖动区）或拖动中：由外壳决定。
    let code: u16 = if let Some(shell) = modular_clipboard_gfx::chrome::shell_cursor_id(hwnd) {
        shell
    } else {
        // IDC_* 在 windows crate 里是 PCWSTR 常量，这里要裸的u16 资源 ID。
        match icon {
            egui::CursorIcon::Default => IDC_ARROW.0 as u16,
            egui::CursorIcon::Text | egui::CursorIcon::VerticalText => IDC_IBEAM.0 as u16,
            egui::CursorIcon::PointingHand => IDC_HAND.0 as u16,
            egui::CursorIcon::Help
            | egui::CursorIcon::ContextMenu
            | egui::CursorIcon::Alias
            | egui::CursorIcon::Copy
            | egui::CursorIcon::Move
            | egui::CursorIcon::Cell
            | egui::CursorIcon::Crosshair
            | egui::CursorIcon::Grab
            | egui::CursorIcon::Grabbing => IDC_ARROW.0 as u16,
            // 其余（None / Progress / Wait / NoDrop / NotAllowed）本项目
            // 不会产生，一律退回默认箭头。
            _ => IDC_ARROW.0 as u16,
        }
    };
    let key = code as u32;
    if LAST.swap(key, Ordering::Relaxed) == key {
        return;
    }
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return;
    }
    // SAFETY: IDC_* 是系统预定义的光标资源 ID，`LoadCursorW` 返回的
    // 句柄由系统持有、不需要销毁。
    unsafe {
        if let Ok(h) = LoadCursorW(None, PCWSTR(code as *const u16)) {
            let _ = SetCursor(Some(h));
        }
    }
}

/// 把窗口提升到置顶组最上层（不激活、不移动、不缩放）。
///
/// 置顶窗口之间按**激活顺序**排 Z 序：设置窗口打开时在顶，但用户
/// 点击主窗口后主窗口激活、被提到置顶组顶部，会把设置窗口压到下面
/// ——「设置窗口被主窗口遮住」。本函数用 `SetWindowPos(HWND_TOPMOST,
/// SWP_NOACTIVATE)` 把目标窗口重新提回最上，**不抢焦点**（键盘/鼠标
/// 焦点仍归当前操作窗口），只保证目标窗口不被其它置顶窗口遮住。
/// 把窗口抬到其所在 Z 序 band 的顶部。
///
/// `topmost` = 目标窗口应处的 band：
/// - `true`：抬进**置顶组**最上（`HWND_TOPMOST`）；
/// - `false`：抬进**普通组**最上（`HWND_TOP`）。
///
/// # 为什么必须带这个参数（2026-10-21 用户反馈修复）
///
/// 早前本函数**无条件**用 `HWND_TOPMOST`。弹窗/设置窗口的打开流程是
/// 「先 `apply_topmost(配置值)` → 再 `sync_and_render`（内部 `just_opened`
/// 调本函数）」：主窗口没开置顶时，第一歩是无操作（簿记已是 false），
/// 第二歩却把窗口按进置顶组，而 `topmost` 簿记仍为 false ⇒ 之后每次
/// `apply_topmost(false)` 都因「无变化」被幂等去重跳过 ⇒ **弹窗永远
/// 置顶**（用户取消「始终置顶」后搜索面板/类型栏/通知/设置窗口仍压在
/// 所有窗口之上）。
///
/// `HWND_TOP` 对**属主窗口**是安全的：被拥有的顶层窗口本来就恒定叠在
/// 属主之上，抬到普通组顶部不会让它掉到主窗口底下。
pub fn raise_to_top(hwnd: HWND, topmost: bool) {
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return;
    }
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
    let after = if topmost { HWND_TOPMOST } else { HWND_TOP };
    // SAFETY: hwnd 已由上面的 IsWindow 校验；只调整 Z 序与置顶标志，
    // flags 不移动、不缩放、不激活。
    let _ = unsafe { SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, flags) };
}

/// 查询主显示器**工作区**（不含任务栏，物理像素）。
///
/// 用于把窗口钳制在屏幕内：主窗口被「记住位置」恢复到屏幕右侧时，
/// 锚定其右上角的设置窗口会整体摆到屏幕外——右上角的关闭按钮
/// 直接点在屏外，表现为「点击关闭没反应」（2026-10-11 实测）。
pub fn work_area_of(_hwnd: HWND) -> (i32, i32, i32, i32) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    let mut wa = RECT::default();
    let _ = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut _ as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    (wa.left, wa.top, wa.right, wa.bottom)
}

/// 把窗口位置钳制到其所在监视器的工作区内。
///
/// 返回钳制后的 (x, y)。`w`/`h` 为窗口物理像素尺寸。
pub fn clamp_to_work_area(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    let (l, t, r, b) = work_area_of(hwnd);
    // 窗口比工作区还大时退化为左上角对齐，避免 clamp 区间为空。
    let (min_x, max_x) = if w >= r - l { (l, l) } else { (l, r - w) };
    let (min_y, max_y) = if h >= b - t { (t, t) } else { (t, b - h) };
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

/// 设置/取消窗口置顶。
///
/// 用 `SetWindowPos` 改扩展样式里的 `WS_EX_TOPMOST`。**不能**用
/// `HWND_TOPMOST` 常量直接替换 `hwnd` 参数——那会同时改 Z 序，
/// 在无边框窗口上会让 resize 行为出现异常。
pub fn set_topmost(hwnd: HWND, on: bool) {
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        tracing::warn!("设置置顶失败：主窗口句柄已失效");
        return;
    }
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED;
    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    // SAFETY: hwnd 已由上面的 IsWindow 校验；flags 只改 Z 序不改位置尺寸。
    let ok = unsafe {
        SetWindowPos(
            hwnd,
            Some(after),
            0,
            0,
            0,
            0,
            flags,
        )
    };
    if ok.is_ok() {
        // 置顶成功不打日志：该函数会被频繁调用（启动、配置切换、各窗口
        // 同步），每次切换都记一条 info 只会刷屏。失败保留 warn 即可。
    } else {
        tracing::warn!(置顶 = on, "SetWindowPos 失败");
    }
}

// ---------------------------------------------------------------------------
// 背景效果（自研磨砂 / 半透明）
// ---------------------------------------------------------------------------

/// 应用窗口背景效果（磨砂 / 半透明 / 无），幂等可重入。
///
/// # 自研方案（不再依赖 DWM backdrop）
///
/// 早前的「磨砂」是 `DWMWA_SYSTEMBACKDROP_TYPE`（Win11 22H2+ Acrylic）——
/// 效果与系统深度耦合（Win10 不可用、外观不可调、与暗色渲染层偏色）。
/// 现改为**自研管线**：窗口后方桌面由 `gfx::backdrop` 实时捕获、GPU 卷积
/// 后作底图（见 `renderer.rs` 的 `BackdropFx`），窗口按**像素 alpha** 合成。
/// 本函数只负责两件事：捕获自排除 + 窗口样式归位。
///
/// - [`WindowBackdrop::Acrylic`]：`SetWindowDisplayAffinity(
///   WDA_EXCLUDEFROMCAPTURE)`——DWM 构建**捕获表面**时跳过本窗口，
///   桌面 DC 的 BitBlt 读到的才是身后内容而非自己的上一帧
///   （否则模糊形成反馈回路，表现为拖影）。副作用（刻意接受）：
///   磨砂模式下窗口不出现在录屏/截图里。窗口**必须非分层**——
///   `WS_EX_LAYERED` 的旧式合成路径会与交换链像素 alpha 冲突；
/// - [`WindowBackdrop::Translucent`]：逐像素半透明——背景层由渲染层
///   画低 alpha（调色板变体），文字/卡片不透明，交换链 PRE_MULTIPLIED
///   交给 DWM 合成。不再用整窗 `LWA_ALPHA` 淡出（那会把文字一起变透明，
///   低不透明度下不可读）；
/// - [`WindowBackdrop::None`]：清除捕获排除与分层样式，恢复纯色不透明。
///
/// 调用方在帧循环里比对配置后调用（与 [`set_topmost`] 相同的幂等
/// 模式），失败只记日志——背景效果是锦上添花，不能影响主流程。
pub fn apply_backdrop(hwnd: HWND, mode: WindowBackdrop) {
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        tracing::warn!("设置背景效果失败：主窗口句柄已失效");
        return;
    }
    // 窗口一律非分层：逐像素 alpha 由交换链（PRE_MULTIPLIED）承担，
    // 分层窗口的 LWA_ALPHA 是整窗淡出，与磨砂/半透明的新语义互斥。
    // 幂等：样式已正确时跳过，避免每帧触发窗口重算。
    let _ = unsafe { set_layered(hwnd, false) };
    let affinity = match mode {
        // 捕获自排除：让桌面捕获「看见」窗口身后而不是窗口自己。
        // （`MC_NO_EXCLUDE` 只是验证用的临时开关：关闭排除后外部截图
        //  才能拍到窗口本身，用于量化验证模糊效果。）
        WindowBackdrop::Acrylic => {
            if std::env::var_os("MC_NO_EXCLUDE").is_some() {
                WDA_NONE
            } else {
                WDA_EXCLUDEFROMCAPTURE
            }
        }
        WindowBackdrop::Translucent | WindowBackdrop::None => WDA_NONE,
    };
    // SAFETY: hwnd 已由上面的 IsWindow 校验。
    if let Err(e) = unsafe { SetWindowDisplayAffinity(hwnd, affinity) } {
        tracing::warn!("设置窗口捕获排除失败（需 Win10 2004+）: {e:?}");
    }
}

/// 设置/清除窗口的 `WS_EX_LAYERED` 扩展样式（幂等）。
///
/// # SAFETY: `hwnd` 须由调用方先经 `IsWindow` 校验。
unsafe fn set_layered(hwnd: HWND, on: bool) -> windows::core::Result<()> {
    // SAFETY: 调用方已保证 hwnd 是有效窗口。
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as i64;
    let has = style & (WS_EX_LAYERED.0 as i64) != 0;
    if has == on {
        return Ok(());
    }
    let new_style = if on {
        style | (WS_EX_LAYERED.0 as i64)
    } else {
        style & !(WS_EX_LAYERED.0 as i64)
    };
    // SAFETY: 同上；只改扩展样式标志位。
    let _ = unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style as isize) };
    Ok(())
}

pub fn minimize_window(hwnd: HWND) {
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        tracing::warn!("最小化失败：主窗口句柄已失效");
        return;
    }
    let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
    tracing::info!("窗口已最小化");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_click_and_hotkey_share_one_action() {
        // 两条入口必须归一：分开展会导致「有时能唤起有时不能」。
        assert_eq!(action_of(TrayEvent::Show), Action::Show);
        assert_eq!(action_of(TrayEvent::Hotkey), Action::Show);
        // 托盘线程已在热键授权窗口内直接激活过主窗口：
        // 映射到 Show 是幂等置前兜底，不能变成「无动作」或隐藏。
        assert_eq!(action_of(TrayEvent::HotkeyShown), Action::Show);
    }

    #[test]
    fn menu_commands_map_directly() {
        assert_eq!(action_of(TrayEvent::ClearHistory), Action::ClearHistory);
        assert_eq!(action_of(TrayEvent::Quit), Action::Quit);
    }

    #[test]
    fn frame_collapses_repeated_activations() {
        // 用户连点三下托盘，只应唤起一次。
        let f = frame_of([TrayEvent::Show, TrayEvent::Show, TrayEvent::Hotkey]);
        assert!(f.show);
        assert!(!f.clear_history);
        assert!(!f.quit);
        assert!(!f.is_empty());
    }

    #[test]
    fn frame_merges_mixed_events() {
        let f = frame_of([
            TrayEvent::Hotkey,
            TrayEvent::ClearHistory,
            TrayEvent::Show,
            TrayEvent::Quit,
        ]);
        assert!(f.show);
        assert!(f.clear_history);
        assert!(f.quit);
    }

    #[test]
    fn empty_frame_reports_empty() {
        // 空帧是「无事件」的正常情形，帧循环每帧都要处理它。
        let f = frame_of([]);
        assert!(f.is_empty());
        assert_eq!(f, Frame::default());
    }

    #[test]
    fn absorb_does_not_short_circuit_on_quit() {
        // Quit 之后若仍有 ClearHistory，意图必须保留。
        //静默丢弃会让「清空」看起来时灵时不灵。
        let f = frame_of([TrayEvent::ClearHistory, TrayEvent::Quit]);
        assert!(f.clear_history);
        assert!(f.quit);
    }

    #[test]
    fn close_hides_to_tray_when_tray_is_available() {
        // WM_CLOSE（用户点关闭按钮）→ 隐藏，进程常驻。这是后台常驻的核心。
        assert_eq!(
            decide_on_close(true, false),
            CloseDecision::HideToTray
        );
    }

    #[test]
    fn wm_quit_always_exits_even_with_tray() {
        // WM_QUIT 来自 PostQuitMessage / WM_CLOSE 被派发的路径，
        // 语义上就是「我要退出」，不能被改成隐藏。
        assert_eq!(decide_on_close(true, true), CloseDecision::Quit);
    }

    #[test]
    fn close_exits_when_tray_unavailable() {
        // 托盘没挂上时隐藏 = 应用变僵尸：窗口没了、进程还在、
        // 也没有任何入口能把它唤回来。只能退出。
        assert_eq!(decide_on_close(false, false), CloseDecision::Quit);
    }

    #[test]
    fn hotkey_description_is_stable() {
        // 设置界面会展示这个字符串，改动它等于改了用户可见文案。
        // 默认已从 Ctrl+Shift+V 改为 Win+V（对齐系统剪贴板历史）。
        assert_eq!(hotkey::describe(hotkey::DEFAULT_HOTKEY), "Win+V");
    }

    #[test]
    fn hidden_poll_interval_is_not_too_greedy() {
        // 隐藏态每帧都在跑：间隔过小会白烧CPU。
        // 下界取 50ms（20Hz），对唤醒体验已完全够用。
        assert!(
            HIDDEN_POLL_INTERVAL >= Duration::from_millis(50),
            "隐藏态轮询过密: {HIDDEN_POLL_INTERVAL:?}"
        );
        // 上界保证点托盘后不会感到迟钝。
        assert!(
            HIDDEN_POLL_INTERVAL <= Duration::from_millis(500),
            "隐藏态唤醒延迟过长: {HIDDEN_POLL_INTERVAL:?}"
        );
    }
}