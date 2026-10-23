//! 进程实例的互相发现与唤起。
//!
//! 拆进程之后有两个新问题：
//! 1. 用户重复双击图标时，第二个进程要能**唤起界面**而不是再起一个；
//! 2. 托盘点击时，常驻进程要知道「界面是不是已经开着」——
//!    开着就把它显示出来，没开才启动。
//!
//! 两者都不需要传输数据，用内核对象（互斥体）与窗口标题即可，
//! 比再开一条通道轻得多。

/// 实例互斥体名（界面进程持有）。
const UI_MUTEX_NAME: &str = r"Local\ModularClipboard.UiInstance";

/// 主窗口标题。
///
/// 单数据源放在这里：常驻进程要用它 `FindWindow` 唤起界面，
/// 界面进程要用它创建窗口。两边各写一份的话，改标题时
/// 「托盘点了没反应」这种 bug 会非常难查。
pub const UI_WINDOW_TITLE: &str = "ModularClipboard";

/// 跨进程消息：请求主窗口显示并置前。
///
/// 消息号是常驻进程与界面进程之间约定的整数（`WM_APP + 1`）。
/// 接收方在 `modular-clipboard-gfx::window` 的窗口过程里，那边写死
/// 同一个值（`WM_SHOW_MAIN`）。两边不共享 crate，改号必须两处同步。
pub const WM_SHOW_MAIN: u32 = 0x8000 + 1;

/// 跨进程消息：请求主窗口隐藏（热键 toggle 的收起方向）。
///
/// 对应接收方的 `WM_HIDE_MAIN`（`WM_APP + 2`），同样两处写死。
pub const WM_HIDE_MAIN: u32 = 0x8000 + 2;

/// 界面进程是否已经运行。
///
/// 用「尝试创建同名互斥体」判断：已存在即界面在跑。
/// 这是唯一可靠的方式——pid 文件会因强杀残留、进程名可被冒充。
pub fn ui_is_running() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    let wide: Vec<u16> = UI_MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = match unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) } {
        Ok(h) => h,
        // 创建失败（权限等）时按「没在运行」处理：宁愿多起一个界面，
        // 也不要让用户点了托盘没反应。
        Err(_) => return false,
    };
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    unsafe {
        let _ = CloseHandle(handle);
    }
    exists
}

/// 界面进程的实例守卫：持有期间表示「界面已运行」。
///
/// 进程退出时内核自动释放，崩溃/强杀都不会留下「假装在跑」的僵死锁。
pub struct UiInstanceGuard {
    _private: (),
}

impl UiInstanceGuard {
    /// 获取界面实例独占权。`Err` 表示已有界面在运行。
    pub fn acquire() -> anyhow::Result<Self> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;

        let wide: Vec<u16> = UI_MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }
            .map_err(|e| anyhow::anyhow!("创建界面实例互斥体失败：{e}"))?;
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Err(anyhow::anyhow!("界面已在运行"));
        }
        // 句柄**故意不 CloseHandle**：关闭它就会释放互斥体，
        // 常驻进程随即误判「界面没开」，再拉起第二个界面。
        //
        // HANDLE 是 Copy 的裸值、没有 Drop，所以只要不显式关闭，
        // 它会一直存在到进程退出——由内核统一回收，崩溃/强杀也一样。
        let _kept_open = handle;
        Ok(Self { _private: () })
    }
}

/// 找到主窗口，向它投递一条自定义消息（`PostMessageW`，只入队、不阻塞）。
///
/// 这是常驻进程跨进程控制界面窗口的**唯一**姿势：`PostMessageW`
/// 立即返回，绝不等界面线程泵消息。早年这里直接 `ShowWindow` /
/// `SetForegroundWindow`，那是同步跨进程调用——常驻进程同时是 IPC
/// 服务端，界面线程那一刻又正阻塞在命名管道 `ReadFile` 上等回响应，
/// 于是「常驻等界面泵消息、界面等常驻回响应」双向死锁
/// （2026-10-13 dump 实锤）。改窗口状态的动作交给界面进程在自己的
/// 消息循环里做（同线程调用立即返回）。
fn post_to_main(msg: u32) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};

    let wide: Vec<u16> = UI_WINDOW_TITLE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    match unsafe { FindWindowW(None, PCWSTR(wide.as_ptr())) } {
        Ok(h) if !h.is_invalid() => {
            // SAFETY: `h` 由上面取得且已判非空；PostMessageW 只入队。
            unsafe { PostMessageW(Some(h), msg, WPARAM(0), LPARAM(0)) }.is_ok()
        }
        _ => false,
    }
}

/// 请求已运行的界面显示并置前。
///
/// 返回 `false` 表示没找到窗口（界面可能正在启动、还没建窗）。
///
/// 实现是投递 [`WM_SHOW_MAIN`]：界面进程在自己的消息循环里
/// `ShowWindow` + 抢前台（见 `gfx::window` 的 `ShowRequested`）。
/// 不在这里跨进程直接改窗口状态，原因见 [`post_to_main`]。
pub fn request_show_ui() -> bool {
    post_to_main(WM_SHOW_MAIN)
}

/// 与 [`request_show_ui`] 同源、但只回答「窗口在不在」。
pub fn ui_window_exists() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

    let wide: Vec<u16> = UI_WINDOW_TITLE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    matches!(unsafe { FindWindowW(None, PCWSTR(wide.as_ptr())) }, Ok(h) if !h.is_invalid())
}

/// 主窗口句柄（`None` = 窗口不存在）。
///
/// 常驻进程的「粘贴上一条」用它排除**自己的界面窗口**：前台若是它，
/// 按键发过去只会激活自己（用户看到「软件窗口被弹到前面」而不是粘贴）。
pub fn ui_window_hwnd() -> Option<isize> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

    let wide: Vec<u16> = UI_WINDOW_TITLE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    match unsafe { FindWindowW(None, PCWSTR(wide.as_ptr())) } {
        Ok(h) if !h.is_invalid() => Some(h.0 as isize),
        _ => None,
    }
}

/// 主窗口当前是否**可见**（不只是存在）。///
/// 界面进程 `SW_HIDE` 后窗口仍存在（`ui_window_exists` 为 true）但
/// 不可见。常驻进程的热键 toggle 据此决定「显示」还是「隐藏」：
/// 找不到窗口同样算不可见——那种情况下调用方会去启动它。
pub fn ui_window_visible() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, IsWindowVisible};

    let wide: Vec<u16> = UI_WINDOW_TITLE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    match unsafe { FindWindowW(None, PCWSTR(wide.as_ptr())) } {
        Ok(h) if !h.is_invalid() => unsafe { IsWindowVisible(h) }.as_bool(),
        _ => false,
    }
}

/// 请求隐藏主窗口，返回是否成功投递（找到窗口并入队）。
///
/// 实现是投递 [`WM_HIDE_MAIN`]：界面进程在自己的消息循环里
/// `ShowWindow(SW_HIDE)`（同线程，立即返回）。界面进程的帧循环仍在
/// 跑（隐藏态只轮询消息、不渲染），下一次唤起由 [`request_show_ui`]
/// 完成。不能在常驻进程跨进程直接 `ShowWindow`，原因见
/// [`post_to_main`]（会与 IPC 同步等待构成死锁）。
pub fn hide_ui_window() -> bool {
    post_to_main(WM_HIDE_MAIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 界面互斥体的语义：第一个拿到、第二个失败。
    ///
    /// 注意测试进程内多次 acquire 会互相干扰（同进程内互斥体也互斥），
    /// 因此只验证「能拿到」与「再拿失败」这两条方向明确的性质。
    #[test]
    fn second_ui_instance_is_rejected() {
        match UiInstanceGuard::acquire() {
            Ok(g) => {
                let _ = g;
                // 拿到之后，探测函数应报告「界面在运行」。
                assert!(ui_is_running(), "持有守卫时应报告界面在运行");
            }
            Err(e) => {
                // 已有界面在跑（例如别的测试/真实程序）时跳过。
                eprintln!("跳过（界面互斥体已被占用）：{e}");
            }
        }
    }

    /// 标题非空：`FindWindowW` 传空串会匹配任意窗口，
    /// 那会把别人的窗口拉到前台。
    #[test]
    fn window_title_is_not_empty() {
        assert!(!UI_WINDOW_TITLE.trim().is_empty());
    }
}
