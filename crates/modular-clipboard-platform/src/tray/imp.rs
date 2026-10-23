//! 托盘的 Win32 实现：图标加载、消息循环、热键注册、右键菜单。
//!
//! 这里的东西全是 Windows 私有细节，外面只通过 root 的 `imp::xxx`
//! 门面调用；非 Windows 目标下整个模块不编译（`#[cfg(windows)]`）。

use super::*;
use windows::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NOTIFYICONDATAW,
    NOTIFYICON_VERSION_4, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CS_HREDRAW, CS_VREDRAW, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetForegroundWindow,
    GetMessageW, GetWindowThreadProcessId, ICONINFO, IDC_ARROW, IDI_APPLICATION, LoadCursorW,
    LoadIconW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, RegisterClassExW, SendMessageW,
    SetForegroundWindow, ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_COMMAND, WM_HOTKEY, WM_NULL, WM_QUIT, WNDCLASSEXW, HWND_MESSAGE, WS_EX_TOOLWINDOW,
    WS_POPUP,
};
use windows::core::{HSTRING, PCWSTR};

/// 窗口类名。多实例共存时靠`RegisterClassExW` 的幂等语义区分。
const CLASS_NAME: &str = "ModularClipboardTrayWindow";

/// 菜单宿主窗口的类名。
///
/// # 为什么需要它
///
/// 托盘窗口是 `HWND_MESSAGE`（message-only）——**永远不可见**，
/// 且 Windows **禁止**对这类窗口调`SetForegroundWindow`。
/// 而 `TrackPopupMenu` 要求所属窗口已是前台，否则菜单弹出即消失。
///
/// 于是菜单需要一个**真正的顶层窗口**当宿主：
/// 弹出前 `ShowWindow(SW_SHOWNOACTIVATE)` + `SetForegroundWindow`，
/// 菜单关掉后 `ShowWindow(SW_HIDE)` 收回。
///
/// 尺寸取 1x1、样式含 `WS_EX_TOOLWINDOW`（不进任务栏/Alt+Tab），
/// 弹出期间在屏幕上几乎看不见。
const MENU_HOST_CLASS: &str = "ModularClipboardTrayMenuHost";

/// 通知托盘线程「重注册某个热键」的内部消息。
///
/// 用窗口消息而不是共享可变状态：注册必须在**托盘线程**里执行
/// （`WM_HOTKEY` 只会投递到注册窗口所属线程），而调用方在主线程。
/// `wparam` 放槽位 id，`lparam` 放打包后的 `modifiers<<32 | vk`。
const WM_TRAY_SET_HOTKEY: u32 = 0x8000 + 4;

/// 托盘图标的资源 ID。
const ICON_ID: u32 = 1;

/// 事件发送端。窗口过程无状态（不做子类化，见 MEMORY.md 架构决策），
/// 因此用一个进程级`OnceLock` 把 `Sender` 交给窗口过程。
///
/// 安全前提：全局只有一个托盘实例，且 `Sender` 仅用于发值类型，
/// 不跨线程暴露内部引用。
/// 通知托盘线程「启用/停用迷你面板」。
///
/// `wparam`：`1` 启用、`0` 停用；`lparam`：修饰键掩码。
const WM_TRAY_SET_MINI: u32 = 0x8000 + 5;
/// 鼠标钩子捕获到「修饰键 + 右键」，请求弹面板。
///
/// `wparam` = x，`lparam` = y（屏幕坐标，打包成一个 isize 传不下，
/// 故 x 放 wparam、y 放 lparam）。
const WM_TRAY_MINI: u32 = 0x8000 + 6;

/// 录制快捷键期间临时注销 / 恢复全局热键。
///
/// `wparam`：`1` 注销、`0` 恢复。
///
/// # 为什么录制要注销
///
/// `RegisterHotKey` 注册的组合会被 OS **从所有窗口没收**——
/// 按键不再产生 `WM_KEYDOWN`，而是直接发给注册窗口。于是用户想
/// 录制「当前正绑着的那个组合」（比如把 Win+V 改掉时先按一下
/// Win+V 确认）时，设置窗口**永远收不到按键**，录制像卡死。
/// 录期间注销、录完恢复，两条路都通。
const WM_TRAY_SUSPEND_HOTKEYS: u32 = 0x8000 + 7;

/// 迷你面板的运行状态。托盘线程独占读写（经消息驱动），
/// 条目列表由外部写、这里读。
fn mini_cell() -> &'static std::sync::Mutex<MiniState> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<MiniState>> = std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(MiniState::default()))
}

#[derive(Default)]
struct MiniState {
    enabled: bool,
    modifiers: u32,
    items: Option<MiniItems>,
    hook: Option<isize>,
}

/// 当前按下的修饰键是否与配置一致。
///
/// 用 `GetAsyncKeyState` 而不是钩子事件里带的按键状态：
/// 低层鼠标钩子只告诉我们「鼠标发生了什么」，
/// 键盘状态必须另外问系统。
unsafe fn mini_modifiers_match(mask: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };
    unsafe {
        let pressed = |vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY| {
            (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0
        };
        let ctrl = pressed(VK_CONTROL);
        let shift = pressed(VK_SHIFT);
        let alt = pressed(VK_MENU);
        let win = pressed(VK_LWIN) || pressed(VK_RWIN);
        // 掩码里要求的一律要按下；没要求的**不要求**没按
        // （用户可能正按着别的键在做别的事）。
        if mask & MINI_MOD_CTRL != 0 && !ctrl {
            return false;
        }
        if mask & MINI_MOD_SHIFT != 0 && !shift {
            return false;
        }
        if mask & MINI_MOD_ALT != 0 && !alt {
            return false;
        }
        if mask & MINI_MOD_WIN != 0 && !win {
            return false;
        }
        true
    }
}

/// 低层鼠标钩子回调。
///
/// 只做一件事：修饰键 + 右键按下 ⇒ 给托盘窗口投一条消息。
/// 真正的弹菜单必须在消息循环里做（`TrackPopupMenu` 是模态的）。
extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, MSLLHOOKSTRUCT, WM_RBUTTONDOWN,
    };
    if code >= 0 && wparam.0 as u32 == WM_RBUTTONDOWN {
        let mut hit = None;
        if let Ok(st) = mini_cell().lock()
            && st.enabled
            && unsafe { mini_modifiers_match(st.modifiers) }
            && let Some(hwnd) = hwnd_cell().get()
        {
            let data = lparam.0 as *const MSLLHOOKSTRUCT;
            if !data.is_null() {
                let pt = unsafe { (*data).pt };
                hit = Some((*hwnd, pt.x, pt.y));
            }
        }
        if let Some((raw, x, y)) = hit {
            let hwnd = HWND(raw as *mut core::ffi::c_void);
            let _ = unsafe {
                PostMessageW(Some(hwnd), WM_TRAY_MINI, WPARAM(x as usize), LPARAM(y as isize))
            };
            // 吃掉这次右键：否则目标程序也会弹它自己的右键菜单，
            // 两个菜单叠在一起。
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 在托盘线程里安装/卸载鼠标钩子。
unsafe fn apply_mini(enable: bool, modifiers: u32, items: MiniItems) {
    use windows::Win32::UI::WindowsAndMessaging::{
        HHOOK, SetWindowsHookExW, UnhookWindowsHookEx, WH_MOUSE_LL,
    };
    let mut st = match mini_cell().lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    // 先卸旧的：改配置时旧钩子必须摘掉，否则会留下一个
    // 用旧修饰键触发的钩子。
    if let Some(raw) = st.hook.take() {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(raw as *mut core::ffi::c_void));
        }
    }
    st.items = Some(items);
    st.modifiers = modifiers;
    st.enabled = false;
    if !enable {
        return;
    }
    match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) } {
        Ok(h) => {
            st.hook = Some(h.0 as isize);
            st.enabled = true;
            tracing::info!("迷你快捷复制面板已启用");
        }
        Err(e) => tracing::error!(%e, "安装鼠标钩子失败，迷你面板不可用"),
    }
}

/// 弹出迷你面板（系统菜单），返回被选中的条目 id。
///
/// 条目为空时不弹：空面板只会让人以为程序坏了。
unsafe fn show_mini_menu(_hwnd: HWND, x: i32, y: i32) -> Option<i64> {
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, MF_STRING, TPM_RETURNCMD, TrackPopupMenu,
    };
    use windows::core::HSTRING;

    let snapshot: Vec<(i64, String)> = match mini_cell().lock() {
        Ok(g) => match g.items.as_ref().and_then(|a| a.lock().ok()) {
            Some(v) => v.clone(),
            None => Vec::new(),
        },
        Err(e) => {
            let g = e.into_inner();
            match g.items.as_ref().and_then(|a| a.lock().ok()) {
                Some(v) => v.clone(),
                None => Vec::new(),
            }
        }
    };
    if snapshot.is_empty() {
        return None;
    }

    let host = unsafe { super::imp::create_menu_host() }?;
    let menu = match unsafe { CreatePopupMenu() } {
        Ok(m) => m,
        Err(_) => {
            unsafe {
                let _ = DestroyWindow(host);
            }
            return None;
        }
    };

    for (id, preview) in &snapshot {
        // 菜单文本要短：长文本会撑满屏幕，且系统不给省略号。
        let text: String = preview.chars().take(48).collect();
        let wide = HSTRING::from(&text);
        unsafe {
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                // 菜单 id 直接用条目 id：省掉一层 id → 条目的映射表，
                // 也就不存在「映射表过期」的问题。条目 id 是正数。
                *id as usize,
                &wide,
            );
        }
    }

    let cmd = unsafe {
        TrackPopupMenu(menu, TPM_RETURNCMD, x, y, None, host, None)
    };
    unsafe {
        let _ = DestroyMenu(menu);
        let _ = DestroyWindow(host);
    }
    if cmd.as_bool() {
        Some(cmd.0 as i64)
    } else {
        None
    }
}

/// 托盘窗口句柄。供主线程投递「重注册热键」消息用。
///
/// 进程内只有一个托盘，`OnceLock` 足够；`set` 发生在窗口创建之后、
/// 任何 `set_hotkey` 之前（见 `start` 里的写入时机）。
fn hwnd_cell() -> &'static std::sync::OnceLock<isize> {
    static CELL: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
    &CELL
}

fn sender_cell() -> &'static std::sync::Mutex<Option<Sender<TrayEvent>>> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<Option<Sender<TrayEvent>>>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(None))
}

fn emit(ev: TrayEvent) {
    if let Ok(guard) = sender_cell().lock()
        && let Some(tx) = guard.as_ref()
    {
        let _ = tx.send(ev);
    }
}

/// 托盘图标与窗口过程的全部 Win32 资源。
///
/// 刻意**不**用 `Drop`：清理顺序（图标 → 窗口 → 类）有严格要求，
/// 且 `DestroyWindow` 必须在托盘线程上执行。统一由 `run` 的收尾段处理。
struct Resources {
    hwnd: HWND,
    nid: NOTIFYICONDATAW,
    hicon: windows::Win32::UI::WindowsAndMessaging::HICON,
    /// 已成功注册的热键 id。注销时只处理这些——
    /// 对没注册成功的 id 调 `UnregisterHotKey` 是没意义的一次系统调用。
    hotkeys: Vec<i32>,
    class_name: HSTRING,
}

impl Resources {
/// 清理。顺序与创建严格相反：热键 → 图标 → 窗口 → 窗口类。
    unsafe fn release(&self) {
        unsafe {
            for id in &self.hotkeys {
                let _ = UnregisterHotKey(Some(self.hwnd), *id);
            }
            // NIM_DELETE 必须在窗口销毁前发，否则图标残留。
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.nid);
            let _ = DestroyWindow(self.hwnd);
            let _ = DestroyIcon(self.hicon);
            let _ = windows::Win32::UI::WindowsAndMessaging::UnregisterClassW(
                PCWSTR(self.class_name.as_ptr()),
                Some(GetModuleHandleW(None).map(|h| h.into()).unwrap_or_default()),
            );
        }
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY_CALLBACK => {
            // 通知区回调：低 16 位是 NIN_*/鼠标消息码。
            if let Some(ev) = notify_event_of(lparam.0) {
                emit(ev);
            } else if is_context_menu_event(lparam.0) {
                // 右键：菜单必须在**本线程**弹出（模态循环），
                // 故转发一条内部消息，由消息循环在派发途中处理。
                let _ = unsafe {
                    PostMessageW(Some(hwnd), WM_TRAY_MENU, WPARAM(0), lparam)
                };
            }
            LRESULT(0)
        }
        WM_TRAY_MENU => {
            unsafe { show_context_menu(hwnd) };
            LRESULT(0)
        }
        WM_HOTKEY => {
            // 按 id 区分槽位：两个热键注册在同一个窗口上。
            tracing::debug!(id = wparam.0 as i32, "托盘窗口过程收到 WM_HOTKEY");
            match wparam.0 as i32 {
                id if id == hotkey::Slot::PastePrevious.id() => emit(TrayEvent::HotkeyPaste),
                _ => {
                    // 趁本线程还持有系统派发 WM_HOTKEY 时授予的
                    // 「前台窗口」权限，直接把主窗口激活到最前。
                    // 转发到 UI 主线程再 SetForegroundWindow 会因
                    // 授权失效而被其他应用盖住（见 lib.rs
                    // activate_window_by_title 的说明）。
                    if crate::activate_window_by_title(crate::UI_WINDOW_TITLE) {
                        // 显示方向已在授权窗口内完成（含双进程
                        // 跨进程激活），上层不得再 toggle 一次。
                        emit(TrayEvent::HotkeyShown);
                    } else {
                        // 窗口已可见（toggle 隐藏方向 / 幂等置前）
                        // 或尚未创建（需要上层拉起新进程）。
                        emit(TrayEvent::Hotkey);
                    }
                }
            }
            LRESULT(0)
        }
        WM_TRAY_SET_HOTKEY => {
            LRESULT(unsafe { rebind_hotkey(hwnd, wparam.0 as i32, lparam.0) } as isize)
        }
        WM_TRAY_SET_MINI => {
            // items 通过 `mini_cell` 传入（PostMessage 带不了 Arc），
            // 这里只取「启用/停用 + 修饰键」。
            if let Ok(st) = mini_cell().lock() {
                let items = st
                    .items
                    .clone()
                    .unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
                drop(st);
                unsafe { apply_mini(wparam.0 != 0, lparam.0 as u32, items) };
            }
            LRESULT(0)
        }
        WM_TRAY_MINI => {
            if let Some(id) =
                unsafe { show_mini_menu(hwnd, wparam.0 as i32, lparam.0 as i32) }
            {
                emit(TrayEvent::MiniPick(id));
            }
            LRESULT(0)
        }
        WM_TRAY_SUSPEND_HOTKEYS => {
            // 录制快捷键期间：把已注册的组合从 OS 手里收回来。
            // 注册过的组合会被 OS 从所有窗口没收（不再产生
            // WM_KEYDOWN），设置窗口因此永远收不到「当前正绑着」
            // 的那个按键——录制看起来像卡死。
            //
            // `wparam = 0`（恢复）刻意不在这里做：托盘线程不持有
            // 配置，「该注册什么」只有调用方知道，由它按当前配置
            // 逐槽位调 `set_hotkey` 恢复。
            if wparam.0 != 0
                && let Ok(mut guard) = hotkey_ids().lock()
            {
                for id in guard.iter() {
                    // SAFETY: hwnd 是本窗口过程的窗口，id 来自
                    // `hotkey_ids`（本进程注册成功的那些）。
                    let _ = unsafe { UnregisterHotKey(Some(hwnd), *id) };
                }
                guard.clear();
            }
            LRESULT(1)
        }
        WM_COMMAND => {
            if let Some(ev) = command_to_event(wparam.0) {
                emit(ev);
            }
            LRESULT(0)
        }
        // 必须原样转发 DefWindowProcW：改动消息流会导致
        // CreateWindowExW 报0x8007007E（MEMORY.md 第 26 条）。
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 弹出右键菜单并派发用户选择。
unsafe fn show_context_menu(hwnd: HWND) {
    unsafe {
        // 重入保护：菜单已在显示中就忽略这条重复回调。
        if MENU_SHOWING.swap(true, std::sync::atomic::Ordering::Acquire) {
            tracing::debug!("右键菜单已在显示中，忽略重复回调（防嵌套模态）");
            return;
        }
        let _guard = MenuShowingGuard;

        let Ok(menu) = CreatePopupMenu() else {
            tracing::warn!("CreatePopupMenu 失败，跳过托盘菜单");
            return;
        };

        let items: [(modular_clipboard_core::i18n::Bi, usize); 3] = [
            (("显示主窗口", "Show main window"), CMD_SHOW),
            (("清空历史", "Clear history"), CMD_CLEAR),
            (("退出", "Quit"), CMD_QUIT),
        ];
        for (i, (label, cmd)) in items.iter().enumerate() {
            let text = HSTRING::from(modular_clipboard_core::i18n::t(*label));
            // 最后一项前插入一条分隔线，把「退出」与操作项分开。
            if i + 1 == items.len() {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            }
            if let Err(e) = AppendMenuW(menu, MF_STRING, *cmd, PCWSTR(text.as_ptr())) {
                tracing::warn!("AppendMenuW({}) 失败: {e}", modular_clipboard_core::i18n::t(*label));
            }
        }

        // 菜单弹出位置 = **当前光标屏幕坐标**。
        //
        // CAUTION: 不能用 `unpack_cursor(lparam)` 从回调参数里解坐标：
        // shell 对通知区回调的 `lParam` 传的是**消息码**（如
        // `WM_RBUTTONUP = 0x205`），并不携带坐标。早前按
        // 「坐标在低 32 位」解包，解出的是消息码本身 ——
        // x ≈ 0x205 / 0x7B、y = 0，菜单因此弹到**屏幕左上角**
        // （用户实机反馈的症状）。
        //
        // 标准做法是 `GetCursorPos`：右键点击的瞬间光标正停在
        // 托盘图标上，取到的就是图标所在屏幕位置。
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let (x, y) = (pt.x, pt.y);
        // 菜单弹出前把宿主置前台，否则菜单立刻消失。
        //
        // IMPORTANT: 宿主必须是**可见的顶层窗口**，这是本次修复的核心。
        //
        // 踩过的两个坑：
        // 1. 早前用托盘窗口自己当宿主——它是 `HWND_MESSAGE`
        //    （message-only），**永远不可见**且 Windows **禁止**
        //    对它 `SetForegroundWindow`。菜单闪一下就没。
        // 2. 改用主窗口后仍不行——托盘常驻时主窗口处于
        //    「隐藏到托盘」状态（`ShowWindow(SW_HIDE)`），
        //    对隐藏窗口置前台同样无效。于是「窗口能拖、
        //    按钮点不动」之外的又一例「改了等于没改」。
        //
        // 现在用专门的 1x1 `WS_EX_TOOLWINDOW` 宿主：
        // 弹出期间 `SW_SHOWNOACTIVATE` 显示（不抢焦点），
        // 菜单关掉后立刻隐藏。
        let host_raw = MENU_HOST.load(std::sync::atomic::Ordering::Relaxed);
        let host = if host_raw != 0 {
            HWND(host_raw as *mut std::ffi::c_void)
        } else {
            hwnd
        };
        let _ = ShowWindow(host, SW_SHOWNOACTIVATE);
        // 宿主必须**真正成为前台窗口**，否则菜单显示一帧就被系统撤掉
        // （`TrackPopupMenu` 所属窗口非前台时的标准症状）。
        //
        // 直接 `SetForegroundWindow` 会踩**前台锁**：用户右键点的是
        // 任务栏，输入归 explorer，本进程不算「刚收到输入」，置前台
        // 大概率失败——这就是「有概率闪退」的根因。辅助手段与本项目
        // `presence::focus_window` 同源：把本线程的输入队列临时接到
        // 前台线程上，让系统认为这次置前台「来自前台」。
        let mut ok = SetForegroundWindow(host).as_bool();
        if !ok {
            let fg = GetForegroundWindow();
            if !fg.0.is_null() && fg != host {
                let fg_thread = GetWindowThreadProcessId(fg, None);
                let cur_thread = GetCurrentThreadId();
                if fg_thread != 0 && fg_thread != cur_thread {
                    let _ = AttachThreadInput(cur_thread, fg_thread, true);
                    ok = SetForegroundWindow(host).as_bool();
                    let _ = AttachThreadInput(cur_thread, fg_thread, false);
                }
            }
        }
        if !ok && GetForegroundWindow() != host {
            // 菜单会闪退。除了这个症状没有任何线索，必须留日志——
            // 否则下次还是只能靠用户复述「有概率」。
            tracing::debug!("菜单宿主未能置前台（前台锁），右键菜单可能闪退");
        }

        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            x,
            y,
            None,
            host,
            None,
        );
        // 收回宿主：菜单关掉后它必须隐藏，否则桌面上留一个
        // 1x1 的窗口（虽然看不见，但会在 Alt+Tab / 任务栏管理里
        // 留下痕迹）。
        let _ = ShowWindow(host, SW_HIDE);
        let _ = DestroyMenu(menu);
        // 标准做法：菜单关闭后发 WM_NULL，否则窗口停在「非激活」态。
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));

        // TPM_RETURNCMD 让菜单直接返回命令 id，不经 WM_COMMAND。
        if cmd.0 != 0
            && let Some(ev) = command_to_event(cmd.0 as usize)
        {
            emit(ev);
        }
    }
}

/// 由程序化 RGBA 缓冲构造 `HICON`。
fn make_hicon() -> Result<windows::Win32::UI::WindowsAndMessaging::HICON> {
    unsafe {
        let n = ICON_SIZE as i32;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: n,
                // 负高度 = 自上而下，省去逐行翻转。
                biHeight: -n,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(
            None,
            &bmi as *const BITMAPINFO,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )
        .context("CreateDIBSection 失败")?;

        let src = icon_rgba()?;
        if bits.is_null() {
            let _ = DeleteObject(color.into());
            anyhow::bail!("CreateDIBSection 返回了空像素指针");
        }
        std::ptr::copy_nonoverlapping(src.as_ptr(), bits.cast::<u8>(), src.len());

        // AND 掩码全 0：实际透明度由color 位图的 alpha 通道决定。
        let mask = CreateBitmap(n, n, 1, 1, None);

        let icon_info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let hicon = match windows::Win32::UI::WindowsAndMessaging::CreateIconIndirect(
            &icon_info,
        ) {
            Ok(h) => h,
            Err(e) => {
                // 失败路径必须手动释放已创建的 GDI 对象，否则泄漏。
                let _ = DeleteObject(mask.into());
                let _ = DeleteObject(color.into());
                return Err(e).context("CreateIconIndirect 失败");
            }
        };

        // ICONINFO 不接管这两个 HBITMAP 的所有权，用完即释放。
        let _ = DeleteObject(mask.into());
        let _ = DeleteObject(color.into());
        Ok(hicon)
    }
}

/// 注册窗口类并创建 message-only 窗口。
/// 创建菜单宿主窗口（1x1、工具窗口样式、默认隐藏）。
///
/// 见 [`MENU_HOST_CLASS`] 的说明：这个窗口存在的唯一目的是给
/// `TrackPopupMenu` 当一个**可以置前台**的所属窗口。
///
/// 失败不致命 —— 返回 `None`，`show_context_menu` 会退回旧路径。
unsafe fn create_menu_host() -> Option<HWND> {
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?;
        let class_name = HSTRING::from(MENU_HOST_CLASS);
        let class_p = PCWSTR(class_name.as_ptr());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(tray_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance.into(),
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: class_p,
            hIconSm: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        };
        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            tracing::debug!("菜单宿主类注册返回 0，按已存在处理");
        }
        // 1x1 像素 + WS_EX_TOOLWINDOW：不进任务栏、不进 Alt+Tab。
        // `CreateWindowExW` 返回 `Result`，不是裸 `HWND`。
        match CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_p,
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(hwnd) => Some(hwnd),
            Err(e) => {
                tracing::debug!("菜单宿主窗口创建失败（{e}），将退回旧路径");
                None
            }
        }
    }
}

fn create_tray_window() -> Result<(HWND, HSTRING)> {
    unsafe {
        let hinstance = GetModuleHandleW(None).context("GetModuleHandleW 失败")?;
        let class_name = HSTRING::from(CLASS_NAME);
        let class_p = PCWSTR(class_name.as_ptr());

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(tray_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance.into(),
            hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: class_p,
            hIconSm: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        };

        // 类已存在时返回 0（ERROR_CLASS_ALREADY_EXISTS）属正常：
        // 同进程重复 start 不应视为失败。
        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            let err = GetLastError();
            tracing::debug!("RegisterClassExW 返回 0（错误 {err:?}），按已存在处理");
        }

        // HWND_MESSAGE：message-only 窗口，不出现在任务栏与 Alt+Tab。
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_p,
            PCWSTR::null(),
            WINDOW_STYLE(0), // 从不显示
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance.into()),
            None,
        )
        .context("创建托盘窗口失败")?;

        Ok((hwnd, class_name))
    }
}

fn build_nid(
    hwnd: HWND,
    hicon: windows::Win32::UI::WindowsAndMessaging::HICON,
    tip: &[u16],
) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: hicon,
        ..Default::default()
    };
    let n = tip.len().min(127); // szTip 是 [u16; 128]，须留NUL
    nid.szTip[..n].copy_from_slice(&tip[..n]);
    nid
}

    /// 托盘线程就绪后回报给启动方的信息。
#[derive(Debug, Clone)]
struct Ready {
    thread_id: u32,
    /// 托盘窗口句柄的原始地址（`HWND` 不是 `Send`，故传地址）。
    hwnd: isize,
    /// 启动时注册失败的快捷键描述（已含槽位名）。
    ///
    /// 必须回传：注册失败只写日志的话，用户按 Win+V 没反应，
    /// 而日志在用户看不到的地方——失败信息要能走到界面上。
    failed: Vec<String>,
}

/// 托盘线程主体：建资源→ 加图标 → 回报就绪 → 消息循环 → 清理。
fn run(
    tx: Sender<TrayEvent>,
    tip: Vec<u16>,
    ready: Sender<Result<Ready>>,
    toggle_spec: hotkey::HotkeySpec,
    paste_spec: hotkey::HotkeySpec,
) {
        // 窗口过程需要能回传事件。
        if let Ok(mut guard) = sender_cell().lock() {
            *guard = Some(tx);
        }

        // 「建立资源」与「跑消息循环」必须分开：
        // 就绪信号要在进入循环**之前**发出，否则 `start` 会一直阻塞——
        // 循环是阻塞的，信号若在循环之后发就永远发不出去。
        let res = setup(&tip, toggle_spec, paste_spec).map(|ctx| {
            // `Ready` 含失败清单（`Vec`），不是 `Copy`：发克隆，
            // 原件跟着 `ctx` 活到消息循环结束。
            let _ = ready.send(Ok(ctx.ready.clone()));
            pump(&ctx);
        });

        // 失败路径必须通知启动方，否则 `start` 同样会一直阻塞。
        if let Err(e) = res {
            let _ = ready.send(Err(e));
        }
    }

    /// 托盘线程持有的全部资源。
    struct Ctx {
        res: Resources,
        ready: Ready,
    }

    /// 建立托盘窗口、图标与快捷键。**不**进入消息循环。
    fn setup(tip: &[u16], toggle_spec: hotkey::HotkeySpec, paste_spec: hotkey::HotkeySpec) -> Result<Ctx> {
        unsafe {
            let (hwnd, class_name) = create_tray_window()?;

            // 菜单宿主：托盘窗口是 `HWND_MESSAGE`，不能置前台，
            // 而 `TrackPopupMenu` 要求所属窗口已是前台 —— 没有它
            // 右键菜单会闪一下就消失。见 [`MENU_HOST_CLASS`]。
            //
            // 创建失败不致命：菜单退回托盘窗口（旧的失败行为）。
            if let Some(host) = create_menu_host() {
                MENU_HOST.store(host.0 as isize, std::sync::atomic::Ordering::Relaxed);
            }

            let hicon = match make_hicon() {
                Ok(h) => h,
                Err(e) => {
                    let _ = DestroyWindow(hwnd);
                    return Err(e);
                }
            };

            let nid = build_nid(hwnd, hicon, tip);

            // 加载托盘资源失败时窗口与图标都要回收。
            let added = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
            if !added {
                let err = GetLastError();
                let _ = DestroyIcon(hicon);
                let _ = DestroyWindow(hwnd);
                anyhow::bail!("Shell_NotifyIconW(NIM_ADD) 失败，Win32 错误 {err:?}");
            }
            // 版本 4 启用 NIN_SELECT 等增强回调；失败只影响右键语义，
            // 不影响图标显示，故仅记日志。
            let mut ver = nid;
            ver.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            if !Shell_NotifyIconW(NIM_SETVERSION, &ver).as_bool() {
                tracing::debug!("NIM_SETVERSION 失败，回退到旧版回调语义");
            }

            // 快捷键注册失败**不致命**：被别的软件占用是常态
            // （Win+V 就常被系统自带的剪贴板历史占着），记日志继续跑。
            // 失败清单随 `Ready` 回传，由调用方决定怎么告知用户。
            let mut hotkeys = Vec::new();
            let mut failed = Vec::new();
            for (slot, spec) in [
                (hotkey::Slot::Toggle, toggle_spec),
                (hotkey::Slot::PastePrevious, paste_spec),
            ] {
                match RegisterHotKey(
                    Some(hwnd),
                    slot.id(),
                    HOT_KEY_MODIFIERS(spec.modifiers),
                    spec.vk,
                ) {
                    Ok(()) => hotkeys.push(slot.id()),
                    Err(e) => {
                        tracing::warn!(
                            "注册全局快捷键 {} 失败（可能已被占用）: {e}",
                            hotkey::describe(spec)
                        );
                        failed.push(hotkey::failed_message(slot, spec));
                    }
                }
            }

            let tid = GetCurrentThreadId();
            let _ = hwnd_cell().set(hwnd.0 as isize);
            if let Ok(mut guard) = hotkey_ids().lock() {
                *guard = hotkeys.clone();
            }
            tracing::info!(
                hwnd = hwnd.0 as isize,
                thread_id = tid,
                failed_hotkeys = failed.len(),
                "托盘窗口已创建（message-only）"
            );
            Ok(Ctx {
                res: Resources {
                    hwnd,
                    nid,
                    hicon,
                    hotkeys,
                    class_name,
                },
                ready: Ready {
                    thread_id: tid,
                    hwnd: hwnd.0 as isize,
                    failed,
                },
            })
        }
    }

    /// 阻塞消息循环：托盘线程的全部工作都发生在这里。
    /// 返回时（收到 `WM_QUIT`）负责回收全部 Win32 资源。
    /// 重注册一个热键槽位（在托盘线程里执行）。
    ///
    /// 返回是否注册成功。失败（组合被占用）时旧的绑定也被注销了——
    /// 这是有意的：留着一个用户已经删掉的旧热键，比暂时没有热键更糟。
    unsafe fn rebind_hotkey(hwnd: HWND, id: i32, packed: isize) -> bool {
        let modifiers = ((packed >> 32) & 0xFFFF_FFFF) as u32;
        let vk = (packed & 0xFFFF_FFFF) as u32;
        unsafe {
            let _ = UnregisterHotKey(Some(hwnd), id);
            match RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(modifiers), vk) {
                Ok(()) => {
                    // 同步 Resources 的记录，退出时才能正确注销。
                    if let Ok(mut guard) = hotkey_ids().lock() {
                        guard.retain(|x| *x != id);
                        guard.push(id);
                    }
                    true
                }
                Err(e) => {
                    tracing::warn!("重新注册快捷键 {id:#x} 失败（可能已被占用）: {e}");
                    false
                }
            }
        }
    }

    /// 已注册的热键 id（供窗口过程与清理共享）。
    fn hotkey_ids() -> &'static std::sync::Mutex<Vec<i32>> {
        static CELL: std::sync::OnceLock<std::sync::Mutex<Vec<i32>>> = std::sync::OnceLock::new();
        CELL.get_or_init(|| std::sync::Mutex::new(Vec::new()))
    }

    pub fn set_mini_panel(modifiers: Option<u32>, items: MiniItems) -> bool {
        let Some(raw) = hwnd_cell().get() else {
            return false;
        };
        let hwnd = HWND(*raw as *mut core::ffi::c_void);
        // Arc 不能塞进消息参数，先放进共享格子里，消息只带开关与掩码。
        if let Ok(mut st) = mini_cell().lock() {
            st.items = Some(items);
        }
        let (on, mask) = match modifiers {
            Some(m) => (1, m),
            None => (0, 0),
        };
        unsafe {
            PostMessageW(
                Some(hwnd),
                WM_TRAY_SET_MINI,
                WPARAM(on),
                LPARAM(mask as isize),
            )
        }
        .is_ok()
    }

    /// 运行时重注册一个热键槽位。
    ///
    /// 用 `SendMessageW` 而非 `PostMessageW`：调用方需要**注册结果**
    /// 才能在界面上提示「已被占用」。投递是异步的，拿不到结果。
    /// 每次调用的工作量是一次 UnregisterHotKey + RegisterHotKey，
    /// 微秒级，同步阻塞可忽略。
    ///
    /// 窗口过程把 `rebind_hotkey` 的 bool 结果原样作为 LRESULT 返回。
    pub fn set_hotkey(slot: hotkey::Slot, spec: hotkey::HotkeySpec) -> bool {
        let Some(raw) = hwnd_cell().get() else {
            return false;
        };
        let hwnd = HWND(*raw as *mut core::ffi::c_void);
        let packed = ((spec.modifiers as i64) << 32) | (spec.vk as i64);
        let result = unsafe {
            SendMessageW(
                hwnd,
                WM_TRAY_SET_HOTKEY,
                Some(WPARAM(slot.id() as usize)),
                Some(LPARAM(packed as isize)),
            )
        };
        result.0 != 0
    }

    /// 录制期间注销全部热键（同步，见公开函数的文档）。
    pub fn suspend_hotkeys() -> bool {
        let Some(raw) = hwnd_cell().get() else {
            return false;
        };
        let hwnd = HWND(*raw as *mut core::ffi::c_void);
        let result = unsafe {
            SendMessageW(
                hwnd,
                WM_TRAY_SUSPEND_HOTKEYS,
                Some(WPARAM(1)),
                None,
            )
        };
        result.0 != 0
    }

    fn pump(ctx: &Ctx) {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            tracing::info!("托盘消息循环退出（收到 WM_QUIT），正在回收资源");
            ctx.res.release();
        }
    }

    /// 通知托盘线程退出：投递 `WM_QUIT` 让 `GetMessageW` 返回 0。
    ///
    /// 用 `PostThreadMessageW` 而非 `PostMessageW`：消息队列按线程隔离，
    /// 直接给线程投递才能确保命中托盘线程的队列（MEMORY.md 坑 43的同源问题：
    /// 状态必须归属正确的所有者）。
    pub(super) unsafe fn stop(thread_id: u32) {
        let _ = unsafe {
            windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                thread_id,
                WM_QUIT,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }

    pub(super) fn start(
        tooltip: &str,
        toggle_spec: hotkey::HotkeySpec,
        paste_spec: hotkey::HotkeySpec,
    ) -> Result<Tray> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel::<Result<Ready>>();

        let tip: Vec<u16> = tooltip.encode_utf16().take(127).collect();

        // 线程内不捕获 `tip` 的引用，只移动所有权。
        let handle = std::thread::Builder::new()
            .name("modular-clipboard-tray".into())
            .spawn(move || run(tx, tip, ready_tx, toggle_spec, paste_spec))
            .context("创建托盘线程失败")?;

        match ready_rx.recv() {
            Ok(Ok(ready)) => Ok(Tray {
                rx,
                thread_id: ready.thread_id,
                hwnd: ready.hwnd,
                failed_hotkeys: ready.failed,
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                anyhow::bail!("托盘线程在就绪前退出")
            }
        }
    }

    /// 重入保护：菜单已在显示中时，`show_context_menu` 必须立即返回。
    ///
    /// shell 对一次右键可能发两条回调（WM_RBUTTONUP + WM_CONTEXTMENU），
    /// 第二条 `WM_TRAY_MENU` 若照常处理，会在前一个 `TrackPopupMenu` 的
    /// **模态循环里**再弹一个——嵌套模态菜单，用户看到「菜单出现一帧
    /// 就被顶掉」。占住标志后来袭的调用应立即原路返回。
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn context_menu_ignores_reentrant_call() {
            // 注意：这个测试**不能**放行到 TrackPopupMenu——那会真的弹
            // 一个模态菜单，把测试进程挂死。能返回即证明重入保护生效。
            let was = MENU_SHOWING.swap(true, std::sync::atomic::Ordering::Acquire);
            unsafe { show_context_menu(HWND(std::ptr::null_mut())) };
            assert!(
                MENU_SHOWING.load(std::sync::atomic::Ordering::Acquire),
                "重入路径不应复位标志（复位意味着真的走到弹菜单逻辑）"
            );
            MENU_SHOWING.store(was, std::sync::atomic::Ordering::Release);
        }
    }

