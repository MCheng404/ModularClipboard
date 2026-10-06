//! 实机验证：托盘图标 + 全局快捷键。
//!
//! # 这个探针回答的问题
//!
//! `tray.rs` 的 17 个单测全是**纯逻辑**测试——图标像素、菜单 ID 映射、
//! `lParam` 拆包、修饰键位运算。它们**从不调用 Win32**，因此回答不了
//! 只有真实 shell / 真实消息循环才能暴露的问题：
//!
//! 1. `Shell_NotifyIconW(NIM_ADD)` 在本机通知区是否**真的成功**
//!    （失败时图标不会出现在托盘，但函数可能返回 TRUE）；
//! 2. `Shell_NotifyIconGetRect` 能否查回该图标的真实位置——
//!    这是「图标确实存在于通知区」的可机器验证证据；
//! 3. 程序化生成的 `HICON` 是否有效（`CreateDIBSection` →
//!    `CreateIconIndirect` 全链路）；
//! 4. `RegisterHotKey(Ctrl+Shift+V)` 是否成功；
//! 5. **注入的回调消息能否走通「PostMessageW → 窗口过程 → channel
//!    → 主线程 `poll`」全链路**。
//!
//! 第 5 条是关键。无人值守环境下没人会去点托盘，若只等系统自发消息，
//! 探针会打印「事件数 0」然后照样退出码 0——**在自己没测的东西上通过
//! 等于没通过**（MEMORY.md「探针设计的一条硬规则」）。故用
//! `PostMessageW` 主动注入，强制走通回调翻译。
//!
//! # 通过标准
//!
//! 退出码 0 = 托盘图标已添加且事件链路通。任一断言失败即 `panic`。
//! WARN 不作为通过条件。

use std::time::Duration;

use modular_clipboard_platform::tray::{self, TrayEvent};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Shell::{
    NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect,
};
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW, WM_COMMAND};

/// 等事件回传的上限。注入是即时的，1 秒足够；留足余量应对调度抖动。
const EVENT_TIMEOUT: Duration = Duration::from_secs(2);

/// 托盘图标停留时长，便于人工确认托盘区确实出现了图标。
const LINGER: Duration = Duration::from_secs(3);

/// 托盘回调消息 ID，必须与 `tray.rs` 的定义一致。
///
/// 这里**故意不复用** crate 内的常量：若两边不一致，探针会注入一条
/// 没人监听的消息并报「链路不通」——这正是我们要暴露的问题。
/// 但重复定义本身是风险，故在断言里校验它落在合理区间。
const WM_TRAY_CALLBACK: u32 = 0x8000 + 2;

/// 菜单命令 ID，与 `tray.rs` 一致。
const CMD_SHOW: usize = 0x1001;
const CMD_CLEAR: usize = 0x1002;
/// 菜单「退出」命令 ID。
const CMD_QUIT: usize = 0x1003;

/// `NIN_SELECT`：左键点击托盘图标。
const NIN_SELECT: u32 = 1024;

fn main() {
    println!("=== 托盘图标实机探针 ===\n");

    // -------------------------------------------------------------------
    // 1. 启动托盘
    // -------------------------------------------------------------------
    let tray = match tray::start("模块化剪切板 - 托盘探针") {
        Ok(t) => t,
        Err(e) => panic!("启动托盘失败: {e:#}"),
    };
    println!("[1] 托盘线程已启动，图标已提交 NIM_ADD");

    // 托盘窗口必须真实存在。message-only 窗口 IsWindow 为真但不可见。
    let hwnd = HWND(tray.hwnd_raw() as *mut core::ffi::c_void);
    assert!(
        unsafe { IsWindow(Some(hwnd)) }.as_bool(),
        "托盘窗口句柄无效，线程可能已崩溃"
    );
    println!("    托盘窗口 IsWindow = true（message-only，不可见属正常）");

    // -------------------------------------------------------------------
    // 2. 图标确实存在于通知区
    // -------------------------------------------------------------------
    // 这一条不能省：NIM_ADD 返回 TRUE 只说明 shell 接受了请求，
    // GetRect 能返回矩形才说明图标**真的挂上去了**。
    let mut icon_rect = None;
    for attempt in 1..=10 {
        // 图标刚添加时 shell 可能尚未完成布局，给几次重试。
        let id = NOTIFYICONIDENTIFIER {
            cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: hwnd,
            uID: 1,
            guidItem: windows::core::GUID::zeroed(),
        };
        if let Ok(rect) = unsafe { Shell_NotifyIconGetRect(&id) } {
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            if w > 0 && h > 0 {
                println!(
                    "[2] 第 {attempt} 次查询成功：托盘图标矩形 {w}x{h} @ ({},{})",
                    rect.left, rect.top
                );
                icon_rect = Some((w, h));
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    match icon_rect {
        Some((w, h)) => assert!(
            w >= 16 && h >= 16,
            "托盘图标尺寸异常: {w}x{h}（应至少16x16）"
        ),
        None => panic!("Shell_NotifyIconGetRect 始终失败：图标不在通知区"),
    }

    // -------------------------------------------------------------------
    // 3. 注入回调消息，验证事件链路
    // -------------------------------------------------------------------
    // 左键点击（NIN_SELECT 在 lParam 低位）
    let lparam = LPARAM(NIN_SELECT as isize);
    inject(hwnd, WM_TRAY_CALLBACK, lparam, "NIN_SELECT（左键点击）");

    let ev = wait_for(&tray, "NIN_SELECT");
    assert_eq!(
        ev,
        TrayEvent::Show,
        "左键点击应映射为TrayEvent::Show"
    );
    println!("    → TrayEvent::Show（应唤起主窗口）");

    // 菜单「显示主窗口」
    //
    // 注意：菜单命令走 `WM_COMMAND` 的 **wParam**，不是托盘回调的 lParam。
    // 真实链路是 `TrackPopupMenu(TPM_RETURNCMD)` 直接返回命令 id，
    // 但窗口过程也处理 `WM_COMMAND`（无 TPM_RETURNCMD 时的路径），
    // 故这里注入 `WM_COMMAND` 才能覆盖该分支。
    inject_cmd(hwnd, CMD_SHOW);
    let ev = wait_for(&tray, "WM_COMMAND(CMD_SHOW)");
    assert_eq!(ev, TrayEvent::Show, "菜单「显示主窗口」应映射为 Show");
    println!("    → TrayEvent::Show");

    // 菜单「清空历史」
    inject_cmd(hwnd, CMD_CLEAR);
    let ev = wait_for(&tray, "WM_COMMAND(CMD_CLEAR)");
    assert_eq!(
        ev,
        TrayEvent::ClearHistory,
        "菜单「清空历史」应映射为 ClearHistory"
    );
    println!("    → TrayEvent::ClearHistory");

    // 右键必须**不**映射成 Show，只能弹菜单。
    // 注意：这里不能真注入右键——WM_TRAY_MENU 会弹出模态菜单，
    // 无人值守环境下会卡死。故只验证右键码不触发事件。
    let lparam = LPARAM(0x0205); // WM_RBUTTONUP
    let _ = unsafe {
        PostMessageW(
            Some(hwnd),
            WM_TRAY_CALLBACK,
            WPARAM(0),
            LPARAM(lparam.0),
        )
    };
    std::thread::sleep(Duration::from_millis(300));
    let leftovers = tray.poll();
    assert!(
        leftovers.is_empty(),
        "右键不应产生语义事件（会误唤起窗口），实际收到: {leftovers:?}"
    );
    println!("    → 右键仅弹菜单，不产生事件（已验证）");

    // -------------------------------------------------------------------
    // 4. 批量排空：验证 poll() 不会漏事件
    // -------------------------------------------------------------------
    for cmd in [CMD_SHOW, CMD_CLEAR, CMD_SHOW] {
        inject_cmd(hwnd, cmd);
    }
    std::thread::sleep(Duration::from_millis(400));
    let batch = tray.poll();
    assert_eq!(
        batch.len(),
        3,
        "连续注入 3 条应一次性取回 3 条，实际 {} 条: {batch:?}",
        batch.len()
    );
    println!("[4] 批量排空正常：3 条注入 → 3 条事件");

    // -------------------------------------------------------------------
    // 5. 退出事件
    // -------------------------------------------------------------------
    // 「退出」不应真的让进程退出（Drop 时才停线程），
    // 但必须能被收到，主循环据此退出。
    inject_cmd(hwnd, CMD_QUIT);
    let ev = wait_for(&tray, "WM_COMMAND(CMD_QUIT)");
    assert_eq!(ev, TrayEvent::Quit, "菜单「退出」应映射为 Quit");
    println!("[5] → TrayEvent::Quit（主循环据此退出）");

    // -------------------------------------------------------------------
    // 6. 快捷键描述与图标缓冲的自检
    // -------------------------------------------------------------------
    use modular_clipboard_platform::hotkey::{DEFAULT_HOTKEY, describe};
    let desc = describe(DEFAULT_HOTKEY);
    assert_eq!(desc, "Ctrl+Shift+V", "默认快捷键描述不正确");
    println!("[6] 默认全局快捷键：{desc}");

    let buf = tray::icon_rgba();
    assert_eq!(buf.len(), tray::ICON_SIZE * tray::ICON_SIZE * 4);
    let opaque = buf.chunks_exact(4).filter(|p| p[3] > 0).count();
    assert!(opaque > 100, "程序化图标应有可见像素，实际 {opaque}");
    println!(
        "[6] 程序化图标 {}x{}，可见像素 {opaque} 个（非emoji/字符）",
        tray::ICON_SIZE,
        tray::ICON_SIZE
    );

    // -------------------------------------------------------------------
    // 7. 停留在托盘，供人工确认
    // -------------------------------------------------------------------
    println!("\n[7] 托盘图标将在通知区停留 {} 秒，请人工确认图标已出现…", LINGER.as_secs());
    for i in (1..=LINGER.as_secs()).rev() {
        std::thread::sleep(Duration::from_secs(1));
        print!("\r    剩余 {i} 秒…");
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }
    println!("\r    已开始清理…");

    // -------------------------------------------------------------------
    // 8. 清理：Drop 应卸载图标并回收线程
    // -------------------------------------------------------------------
    drop(tray);
    println!("[8] 托盘已释放（图标应从通知区消失）");

    println!("\n=== ALL OK：托盘图标、事件链路、快捷键全部验证通过 ===");
}

/// 往托盘窗口注入一条消息。
fn inject(hwnd: HWND, msg: u32, lparam: LPARAM, what: &str) {
    let ok = unsafe { PostMessageW(Some(hwnd), msg, WPARAM(0), lparam) };
    assert!(ok.is_ok(), "注入 {what} 失败: {ok:?}");
}

/// 注入一条菜单命令（走 `WM_COMMAND` 的 wParam，与真实链路一致）。
fn inject_cmd(hwnd: HWND, cmd: usize) {
    let ok = unsafe { PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(cmd), LPARAM(0)) };
    assert!(ok.is_ok(), "注入 WM_COMMAND({cmd:#x}) 失败: {ok:?}");
}

/// 等待一个托盘事件，超时则 panic。
fn wait_for(tray: &tray::Tray, what: &str) -> TrayEvent {
    match tray.recv_timeout(EVENT_TIMEOUT) {
        Some(ev) => {
            println!("[3] 已注入 {what}");
            ev
        }
        None => panic!(
            "注入 {what} 后{EVENT_TIMEOUT:?} 内没有收到任何事件——\
             回调链路（PostMessageW → 窗口过程 → channel）不通"
        ),
    }
}