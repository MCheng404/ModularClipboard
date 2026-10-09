//! 后台常驻：把托盘与全局快捷键翻译成应用级动作。
//!
//! # 与主窗口消息循环的关系
//!
//! [`tray::start`] 自带**独立线程 + message-only 窗口**，
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
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowThreadProcessId, IsWindow, SetForegroundWindow, SetWindowPos,
    ShowWindow, HWND_NOTOPMOST, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE, SWP_NOACTIVATE, SW_HIDE, SW_MINIMIZE,
    SW_RESTORE,
};

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
    /// 清空全部历史。
    ClearHistory,
    /// 退出进程。
    Quit,
}

/// 单个 [`TrayEvent`] 对应的动作。
///
/// `Show` 与 `Hotkey` **必须**映射到同一个 [`Action::Show`]：
/// 两条入口走同一条唤起路径，才能保证「左键能唤起，快捷键也一定
/// 能唤起」——分成两个动作就会各自演化出不同的前置条件，
/// 最终表现为「有时按快捷键没反应」。
pub const fn action_of(ev: TrayEvent) -> Action {
    match ev {
        TrayEvent::Show | TrayEvent::Hotkey => Action::Show,
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
            Action::ClearHistory => self.clear_history = true,
            Action::Quit => self.quit = true,
        }
    }

    /// 本帧是否没有任何待办事项。
    pub fn is_empty(&self) -> bool {
        !self.show && !self.clear_history && !self.quit
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
    /// 添加托盘图标并注册全局快捷键。
    ///
    /// 阻塞至图标真正出现在通知区后才返回。
    pub fn start(tooltip: &str) -> anyhow::Result<Self> {
        let tray = tray::start(tooltip)?;
        tracing::info!(
            hotkey = %hotkey::describe(hotkey::DEFAULT_HOTKEY),
            "托盘已就绪"
        );
        Ok(Self {
            tray: Some(tray),
        })
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
/// `IsWindow` 检查是防御性的：正常流程下窗口永不销毁
/// （关闭按钮走 [`CloseDecision::HideToTray`），但若将来有人改成
/// 真的销毁窗口，这里拿失效句柄调 Win32 会静默失败并留下
/// 「点了没反应」的现象，不如明确记一条日志。
pub fn focus_window(hwnd: HWND) {
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        tracing::warn!("唤起失败：主窗口句柄已失效");
        return;
    }
    unsafe {
        // 一次 SW_RESTORE 覆盖「最小化」与「隐藏」两种状态：
        // 对未最小化的隐藏窗口，它等价于 SW_SHOW。
        let _ = ShowWindow(hwnd, SW_RESTORE);
        if SetForegroundWindow(hwnd).as_bool() {
            return;
        }
        // 前台窗口限制：后台进程不能直接抢前台。
        //
        // 不用 `AllowSetForegroundWindow`——它只能由**当前前台窗口**
        // 所属线程调用去授权*其它*进程，对自己不生效（会失败）。
        // 它的适用场景是「A 窗口刚被点过，想让 B 进程弹窗」，
        // 与本场景相反。
        //
        // 正确做法是把本线程的输入队列临时接到前台线程上，
        // 让 `SetForegroundWindow` 认为自己来自前台。
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == hwnd {
            return;
        }
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let cur_thread = GetCurrentThreadId();
        if fg_thread == 0 || fg_thread == cur_thread {
            return;
        }
        let _ = AttachThreadInput(cur_thread, fg_thread, true);
        let _ = SetForegroundWindow(hwnd);
        // 必须解除：连接期间本线程的消息会被送进前台线程的队列，
        // 忘记解除会造成键盘输入落到别人的窗口上。
        let _ = AttachThreadInput(cur_thread, fg_thread, false);
    }
}

/// 隐藏主窗口（不销毁），托盘继续常驻。
pub fn hide_window(hwnd: HWND) {
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
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
        tracing::info!(置顶 = on, "窗口置顶状态已更新");
    } else {
        tracing::warn!(置顶 = on, "SetWindowPos 失败");
    }
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
        assert_eq!(hotkey::describe(hotkey::DEFAULT_HOTKEY), "Ctrl+Shift+V");
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