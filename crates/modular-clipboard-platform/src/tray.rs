//! 系统托盘常驻：独立线程 + 自己的消息循环。
//!
//! # 为什么必须独立线程（架构决策）
//!
//! `modular-clipboard-gfx/src/window.rs` 的 `EventLoop::poll` 用
//! `PeekMessageW(&mut msg, None, ...)`（`None` = 排空**整个线程队列**）
//! 取消息，随后无条件 `DispatchMessageW`。由此排除另外两种方案：
//!
//! - **方案 A（挂主窗口 `hWnd`）**：托盘的 `WM_COMMAND` / `NIN_SELECT`
//!   会被主循环先`PeekMessage` 走，`translate()` 不认识这些消息 →
//!   事件丢失。且托盘随 Vulkan 窗口一起销毁，托盘常驻无从谈起。
//! - **方案 B（同线程 message-only 窗口）**：队列仍是同一条，主循环
//!   会把托盘消息取走。窗口「不可见」并不能隔离消息队列。
//!
//! Win32 消息队列是**每线程一份**的。故本模块自建线程 + message-only
//! 窗口（`HWND_MESSAGE`），拥有独立队列：
//! - 与主渲染循环零耦合；主窗口最小化/销毁都不影响托盘；
//! - `GetMessageW` 阻塞等待，无消息时不空转烧 CPU；
//! - 事件经 channel 回传主线程，帧尾 `try_recv` 一次即可。
//!
//! # 约束
//!
//! 图标为**程序化绘制的位图**（项目 P0 规则：不得用字符/emoji 当图标），
//! 见 [`icon_rgba`]。不引入 `tray-icon` 等携带渲染后端的 crate。

use anyhow::{Context, Result};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Duration;

// ---------------------------------------------------------------------------
// 纯逻辑层：跨平台可单测，不触碰 Win32
// ---------------------------------------------------------------------------

/// 托盘产生的语义事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// 左键点击 / 双击 / 菜单「显示主窗口」→ 应唤起主窗口。
    Show,
    /// 菜单「清空历史」。
    ClearHistory,
    /// 菜单「退出」。
    Quit,
    /// 全局快捷键被按下。
    Hotkey,
}

/// 图标边长（像素）。托盘小图标标准尺寸为 16，取 32 让高分屏缩放后仍清晰。
pub const ICON_SIZE: usize = 32;

/// 托盘图标配色（BGRA，非预乘 alpha）。
///
/// 画的是一块「剪贴板」：板身 + 顶部夹子 + 三条文本线，
/// 语义与剪贴板管理器一致，且缩到 16px 仍可辨识。
const COLOR_BOARD: [u8; 4] = [0xeb, 0x6f, 0x1f, 0xff]; // B,G,R,A
const COLOR_CLIP: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
const COLOR_TEXT: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

/// 生成托盘图标的 BGRA 缓冲，长度 `ICON_SIZE * ICON_SIZE * 4`。
///
/// 用整数像素判定而非抗锯齿：托盘会把图标缩到 16px，抗锯齿边反而发虚。
pub fn icon_rgba() -> Vec<u8> {
    let n = ICON_SIZE;
    let mut buf = vec![0u8; n * n * 4];

    let clip = Rect::new(12, 4, 20, 12, 2);
    let board = Rect::new(4, 8, 28, 30, 3);
    let line1 = Rect::new(9, 15, 24, 17, 0);
    let line2 = Rect::new(9, 19, 24, 21, 0);
    let line3 = Rect::new(9, 23, 19, 25, 0);

    for y in 0..n {
        for x in 0..n {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // 夹子先判，才能盖住板身上缘。
            let color = if clip.contains(px, py) {
                COLOR_CLIP
            } else if board.contains(px, py) {
                if line1.contains(px, py) || line2.contains(px, py) || line3.contains(px, py) {
                    COLOR_TEXT
                } else {
                    COLOR_BOARD
                }
            } else {
                continue;
            };
            let i = (y * n + x) * 4;
            buf[i..i + 4].copy_from_slice(&color);
        }
    }
    buf
}

/// 轴对齐圆角矩形，纯整数判定（便于精确单测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x0: i32,
    y0: i32,
    /// 右边界，**开区间**。
    x1: i32,
    /// 下边界，**开区间**。
    y1: i32,
    r: i32,
}

impl Rect {
    const fn new(x0: i32, y0: i32, x1: i32, y1: i32, r: i32) -> Self {
        Self { x0, y0, x1, y1, r }
    }

    /// 点是否落在矩形内（含圆角切口）。
    fn contains(&self, px: f32, py: f32) -> bool {
        let (x, y) = (px as i32, py as i32);
        if x < self.x0 || x >= self.x1 || y < self.y0 || y >= self.y1 {
            return false;
        }
        if self.r <= 0 {
            return true;
        }
        // 只有四角需要圆角判定：先定位到该角的内切圆心。
        let cx = if x < self.x0 + self.r {
            self.x0 + self.r
        } else if x >= self.x1 - self.r {
            self.x1 - self.r - 1
        } else {
            return true; // 位于上下边之间，整列通过
        };
        let cy = if y < self.y0 + self.r {
            self.y0 + self.r
        } else if y >= self.y1 - self.r {
            self.y1 - self.r - 1
        } else {
            return true; // 位于左右边之间，整行通过
        };
        let (dx, dy) = (x - cx, y - cy);
        dx * dx + dy * dy <= self.r * self.r
    }
}

/// 托盘回调消息 ID。
///
/// 取 `WM_APP + 2`：`NIN_*` 落在 `WM_USER`(1024) 起的区间，
/// 选`WM_APP + 0/1` 会与将来新增的通知撞车，故留出余量。
pub const WM_TRAY_CALLBACK: u32 = 0x8000 + 2;

/// 请求弹出右键菜单的内部消息（携带光标屏幕坐标）。
const WM_TRAY_MENU: u32 = 0x8000 + 3;

/// 菜单命令 ID。取`0x1001` 起，避开 `HMENU` 自身的保留区。
/// 菜单宿主窗口句柄（托盘线程创建，窗口过程取用）。
///
/// 见 [`MENU_HOST_CLASS`]：托盘窗口是 `HWND_MESSAGE`，永远不可见、
/// 不能置前台，而 `TrackPopupMenu` 要求所属窗口已是前台。
static MENU_HOST: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 主窗口句柄（由 UI 层在窗口创建后写入）。
///
/// # 为什么需要它
///
/// 托盘的右键菜单必须**依附一个可见的前台窗口**，否则弹出后立刻
/// 失去激活、随即消失。托盘自己那个窗口是**隐藏的辅助窗口**
/// （只收消息、不显示），`SetForegroundWindow` 对它无效。
///
/// 早期实现直接对托盘窗口调 `SetForegroundWindow`，症状就是
/// 「托盘右键菜单点出来就没了」。
static MAIN_HWND: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);

/// 登记主窗口句柄，供托盘菜单置前台用。
pub fn set_main_hwnd(hwnd: isize) {
    MAIN_HWND.store(hwnd, std::sync::atomic::Ordering::Relaxed);
}

const CMD_SHOW: usize = 0x1001;
const CMD_CLEAR: usize = 0x1002;
const CMD_QUIT: usize = 0x1003;

/// Windows 通知区回调事件码（`NOTIFYICON_VERSION_4`）。
///
/// 这两个值是 Win32 协议常量，`windows` crate 未导出，只能本地定义。
mod nin {
    /// 鼠标左键点击（或键盘激活）。
    pub const SELECT: u32 = WM_USER_BASE + 0;
    /// 键盘激活（回车/空格）。
    pub const KEYSELECT: u32 = WM_USER_BASE + 1;
    const WM_USER_BASE: u32 = 1024;
}

/// 把 `WM_COMMAND` 的 `wParam` 低 16 位翻译成事件。
fn command_to_event(cmd: usize) -> Option<TrayEvent> {
    match cmd & 0xFFFF {
        CMD_SHOW => Some(TrayEvent::Show),
        CMD_CLEAR => Some(TrayEvent::ClearHistory),
        CMD_QUIT => Some(TrayEvent::Quit),
        _ => None,
    }
}

/// 托盘回调消息 `lParam` 的两段解码。
///
/// # 为什么必须分开读
///
/// 本项目用 `NOTIFYICON_VERSION_4`（见 `run`里的 `NIM_SETVERSION`），
/// 该版本下 shell 把回调信息**打包**进 `lParam`：
///
/// ```text
/// ┌──────────────────────────┬──────────────────────────┐
/// │ 高 16 位：鼠标消息码     │ 低 16 位：通知码         │
/// │ WM_LBUTTONUP /          │ NIN_SELECT / NIN_KEYSELECT│
/// │ WM_RBUTTONUP /          │                           │
/// │ WM_CONTEXTMENU …│                           │
/// └──────────────────────────┴──────────────────────────┘
/// ```
///
/// 早前两个判定都只看**低 16 位**，于是：
/// - 左键：`NIN_SELECT` 在低位 ⇒ 命中，看起来正常；
/// - 右键：鼠标码在**高位**、低位是 `NIN_SELECT` ⇒ 永不命中
///   ⇒ 右键完全没反应。
///
/// 这正是用户报告的症状「左键正常、右键没反应」——一个纯粹的
/// 打包格式误读，与菜单宿主（前两次修复的重点）毫无关系。
///
/// # 坐标也要注意
///
/// 高低位**不是**屏幕坐标：坐标在 `GET_X_LPARAM(lParam)` 宏的
/// 意义上是同一组位，但版本 4 下要用 `GET_X_LPARAM` 而不是
/// `LOWORD`——后者对负坐标会出错（右键菜单常弹在屏幕左/下边缘，
/// 那里x 或 y 为负）。见 [`unpack_cursor`]。
struct Callback {
    /// 高 16 位：鼠标消息码。
    mouse: u32,
    /// 低 16 位：通知码。
    notify: u32,
}

impl Callback {
    fn decode(lparam: isize) -> Self {
        Self {
            mouse: ((lparam >> 16) & 0xFFFF) as u32,
            notify: (lparam & 0xFFFF) as u32,
        }
    }
}

/// 托盘回调消息 → 事件（展示主窗口）。
///
/// ⚠️ 判定要**同时**看高位鼠标码与低位通知码：
/// - 版本 4 正常路径：低位是 `NIN_SELECT` / `NIN_KEYSELECT`；
/// - `NIM_SETVERSION` 失败时回退旧式语义：高位直接是
///   `WM_LBUTTONUP` / `WM_LBUTTONDBLCLK`。
///
/// 两条都映射为 [`TrayEvent::Show`]，否则用户会「点了没反应」。
fn notify_event_of(lparam: isize) -> Option<TrayEvent> {
    let cb = Callback::decode(lparam);
    match cb.notify {
        nin::SELECT | nin::KEYSELECT => Some(TrayEvent::Show),
        _ => match cb.mouse {
            WM_LBUTTONUP_CODE | WM_LBUTTONDBLCLK_CODE => Some(TrayEvent::Show),
            _ => None,
        },
    }
}

/// 托盘右键事件 → 是否应弹菜单。
///
/// ⚠️ 判定必须在**高位**（鼠标消息码），见 [`Callback`] 的说明。
/// 早前只看低位，而版本 4 下低位恒为 `NIN_SELECT`，所以右键
/// 永远不匹配。
fn is_context_menu_event(lparam: isize) -> bool {
    let cb = Callback::decode(lparam);
    // 两条路径：版本 4 的高位鼠标码，以及旧式语义下低位直接是
    // `WM_RBUTTONUP`（此时高位为 0）。
    matches!(cb.mouse, WM_RBUTTONUP_CODE | WM_CONTEXTMENU_CODE)
        || matches!(cb.notify, WM_RBUTTONUP_CODE | WM_CONTEXTMENU_CODE)
}

const WM_LBUTTONUP_CODE: u32 = 0x0202;
const WM_LBUTTONDBLCLK_CODE: u32 = 0x0203;
const WM_RBUTTONUP_CODE: u32 = 0x0205;
const WM_CONTEXTMENU_CODE: u32 = 0x007B;

/// 从 `lParam` 解出光标屏幕坐标。
///
/// # 必须走 `GET_X_LPARAM` 语义，不能用 `LOWORD`
///
/// 托盘右键菜单常弹在屏幕**左边缘或下边缘**，那里 x / y 是负数。
/// `LOWORD`/`LOWBIT` 把它当 `u16` 再转 `i32`，得到 65535 之类的
/// 巨大正值 ⇒ 菜单弹出到屏幕外看不见，表现为「点了没反应」。
/// `GET_X_LPARAM` 内部按 `i16` 解释，正确返回负值。
fn unpack_cursor(lparam: isize) -> (i32, i32) {
    // 通知区回调的坐标在 `lParam` 的低/高 16 位，但**整个 lParam**
    // 已被 shell 按版本 4 的格式打包：坐标在低 32 位（x 低16 / y 高16），
    // 鼠标码与通知码在更高位。因此这里只取低 32 位再按 i16 解释。
    let packed = (lparam & 0xFFFF_FFFF) as u32;
    let x = (packed & 0xFFFF) as u16 as i16 as i32;
    let y = ((packed >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

// ---------------------------------------------------------------------------
// 主线程侧句柄
// ---------------------------------------------------------------------------

/// 托盘控制器。`Drop` 时卸载图标并回收托盘线程。
pub struct Tray {
    rx: Receiver<TrayEvent>,
    /// 托盘线程 id，用于停止线程。
    thread_id: u32,
    /// 托盘的 message-only 窗口句柄。仅供探针注入消息与诊断使用。
    #[cfg_attr(not(windows), allow(dead_code))]
    hwnd: isize,
}

impl Tray {
    /// 非阻塞取回全部待处理事件。
    pub fn poll(&self) -> Vec<TrayEvent> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(ev) => out.push(ev),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        out
    }

    /// 阻塞等待一个事件，超时返回 `None`。
    ///
    /// 仅供探针与「等用户点托盘退出」的同步流程使用；
    /// 正常帧循环请用 [`Tray::poll`]。
    pub fn recv_timeout(&self, timeout: Duration) -> Option<TrayEvent> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// 托盘窗口句柄（原始地址）。
    ///
    /// 供探针用 `PostMessageW` 注入回调消息，从而在无人值守环境下
    /// 真正走通「消息 → 窗口过程 → 事件回传」全链路。
    #[cfg(windows)]
    pub fn hwnd_raw(&self) -> isize {
        self.hwnd
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            imp::stop(self.thread_id);
        }
        let _ = self.thread_id;
    }
}

// ---------------------------------------------------------------------------
// 启动入口
// ---------------------------------------------------------------------------

/// 启动托盘线程并添加图标。
///
/// 阻塞至图标真正出现在通知区后才返回，失败则返回 `Err`
/// （不会留下半初始化的后台线程）。
pub fn start(tooltip: &str) -> Result<Tray> {
    imp::start(tooltip)
}

#[cfg(not(windows))]
pub fn start(_tooltip: &str) -> Result<Tray> {
    anyhow::bail!("当前平台尚未实现托盘")
}

// ---------------------------------------------------------------------------
// Windows 实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
        DeleteObject,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey, VK_V,
    };
    use windows::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NOTIFYICONDATAW,
        NOTIFYICON_VERSION_4, Shell_NotifyIconW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CS_HREDRAW, CS_VREDRAW, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetMessageW, ICONINFO,
        IDC_ARROW, IDI_APPLICATION, LoadCursorW, LoadIconW, MF_SEPARATOR, MF_STRING, MSG,
        PostMessageW, RegisterClassExW, SetForegroundWindow, ShowWindow, SW_HIDE,
        SW_SHOWNOACTIVATE, TPM_RETURNCMD, TPM_RIGHTBUTTON,
        TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COMMAND, WM_HOTKEY,
        WM_NULL, WM_QUIT, WNDCLASSEXW, HWND_MESSAGE, WS_EX_TOOLWINDOW, WS_POPUP,
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

    /// 全局热键 id。任意非零值即可，只要在本进程内唯一。
    const HOTKEY_ID: i32 = 0x0BEE;

    /// 托盘图标的资源 ID。
    const ICON_ID: u32 = 1;

    /// 事件发送端。窗口过程无状态（不做子类化，见 MEMORY.md 架构决策），
    /// 因此用一个进程级`OnceLock` 把 `Sender` 交给窗口过程。
    ///
    /// 安全前提：全局只有一个托盘实例，且 `Sender` 仅用于发值类型，
    /// 不跨线程暴露内部引用。
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
        hotkey_ok: bool,
        class_name: HSTRING,
    }

    impl Resources {
/// 清理。顺序与创建严格相反：热键 → 图标 → 窗口 → 窗口类。
        unsafe fn release(&self) {
            unsafe {
                if self.hotkey_ok {
                    let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
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
                unsafe { show_context_menu(hwnd, lparam.0) };
                LRESULT(0)
            }
            WM_HOTKEY => {
                emit(TrayEvent::Hotkey);
                LRESULT(0)
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
    unsafe fn show_context_menu(hwnd: HWND, lparam: isize) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                tracing::warn!("CreatePopupMenu 失败，跳过托盘菜单");
                return;
            };

            let items: [(&str, usize); 3] = [
                ("显示主窗口", CMD_SHOW),
                ("清空历史", CMD_CLEAR),
                ("退出", CMD_QUIT),
            ];
            for (i, (label, cmd)) in items.iter().enumerate() {
                let text = HSTRING::from(*label);
                // 最后一项前插入一条分隔线，把「退出」与操作项分开。
                if i + 1 == items.len() {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                }
                if let Err(e) = AppendMenuW(menu, MF_STRING, *cmd, PCWSTR(text.as_ptr())) {
                    tracing::warn!("AppendMenuW({label}) 失败: {e}");
                }
            }

            let (x, y) = unpack_cursor(lparam);
            // 菜单弹出前把宿主置前台，否则菜单立刻消失。
            //
            // ⚠️ 宿主必须是**可见的顶层窗口**，这是本次修复的核心。
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
            let _ = SetForegroundWindow(host);

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
                    biCompression: BI_RGB.0 as u32,
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

            let src = icon_rgba();
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

    /// 托盘线程主体：建资源→ 加图标 → 回报就绪 → 消息循环 → 清理。
    /// 托盘线程就绪后回报给启动方的信息。
#[derive(Debug, Clone, Copy)]
struct Ready {
    thread_id: u32,
    /// 托盘窗口句柄的原始地址（`HWND` 不是 `Send`，故传地址）。
    hwnd: isize,
}

fn run(tx: Sender<TrayEvent>, tip: Vec<u16>, ready: Sender<Result<Ready>>) {
        // 窗口过程需要能回传事件。
        if let Ok(mut guard) = sender_cell().lock() {
            *guard = Some(tx);
        }

        // 「建立资源」与「跑消息循环」必须分开：
        // 就绪信号要在进入循环**之前**发出，否则 `start` 会一直阻塞——
        // 循环是阻塞的，信号若在循环之后发就永远发不出去。
        let res = setup(&tip).and_then(|ctx| {
            let _ = ready.send(Ok(ctx.ready.clone()));
            pump(&ctx);
            Ok(ctx.ready)
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
    fn setup(tip: &[u16]) -> Result<Ctx> {
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

            // 快捷键注册失败**不致命**：被别的软件占用是常态，记日志继续跑。
            let hotkey_ok = match RegisterHotKey(
                Some(hwnd),
                HOTKEY_ID,
                MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
                VK_V.0 as u32,
            ) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("注册全局快捷键 Ctrl+Shift+V 失败（可能已被占用）: {e}");
                    false
                }
            };

            let tid = GetCurrentThreadId();
            Ok(Ctx {
                res: Resources {
                    hwnd,
                    nid,
                    hicon,
                    hotkey_ok,
                    class_name,
                },
                ready: Ready {
                    thread_id: tid,
                    hwnd: hwnd.0 as isize,
                },
            })
        }
    }

    /// 阻塞消息循环：托盘线程的全部工作都发生在这里。
    /// 返回时（收到 `WM_QUIT`）负责回收全部 Win32 资源。
    fn pump(ctx: &Ctx) {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
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

    pub(super) fn start(tooltip: &str) -> Result<Tray> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel::<Result<Ready>>();

        let tip: Vec<u16> = tooltip.encode_utf16().take(127).collect();

        // 线程内不捕获 `tip` 的引用，只移动所有权。
        let handle = std::thread::Builder::new()
            .name("modular-clipboard-tray".into())
            .spawn(move || run(tx, tip, ready_tx))
            .context("创建托盘线程失败")?;

        match ready_rx.recv() {
            Ok(Ok(ready)) => Ok(Tray {
                rx,
                thread_id: ready.thread_id,
                hwnd: ready.hwnd,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_buffer_length_matches_size() {
        assert_eq!(icon_rgba().len(), ICON_SIZE * ICON_SIZE * 4);
    }

    #[test]
    fn icon_has_visible_pixels() {
        let buf = icon_rgba();
        let opaque = buf.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(opaque > 100, "图标应有可见像素，实际 {opaque}");
        assert!(opaque < ICON_SIZE * ICON_SIZE, "四角应透明，不应铺满");
    }

    #[test]
    fn icon_corners_are_transparent() {
        let buf = icon_rgba();
        let n = ICON_SIZE;
        // 四个角像素的 alpha 必须是 0，否则托盘显示为方块。
        for (x, y) in [(0, 0), (n - 1, 0), (0, n - 1), (n - 1, n - 1)] {
            assert_eq!(buf[(y * n + x) * 4 + 3], 0, "({x},{y}) 应透明");
        }
    }

    #[test]
    fn icon_has_board_body() {
        // 板身中部（避开文本线与夹子）必须是板色，且不透明。
        let buf = icon_rgba();
        let n = ICON_SIZE;
        let i = (13 * n + 6) * 4;
        assert_eq!(&buf[i..i + 4], &COLOR_BOARD);
    }

    #[test]
    fn icon_has_text_lines() {
        // 三条文本线之一（第16 行）应为白色。
        let buf = icon_rgba();
        let n = ICON_SIZE;
        let i = (16 * n + 12) * 4;
        assert_eq!(&buf[i..i + 4], &COLOR_TEXT, "文本线应为白色");
    }

    #[test]
    fn rect_without_radius_is_full() {
        let r = Rect::new(0, 0, 10, 10, 0);
        assert!(r.contains(0.5, 0.5));
        assert!(r.contains(9.5, 9.5));
    }

    #[test]
    fn rect_cuts_corners() {
        let r = Rect::new(0, 0, 10, 10, 3);
        assert!(!r.contains(0.5, 0.5), "左上角应被切掉");
        assert!(r.contains(5.5, 5.5), "中心应命中");
    }

    #[test]
    fn rect_respects_open_bounds() {
        let r = Rect::new(2, 2, 5, 5, 0);
        assert!(!r.contains(1.5, 3.5), "左界外");
        assert!(r.contains(2.5, 2.5), "左上角含");
        assert!(!r.contains(4.5 + 1.0, 2.5), "右界开区间");
    }

    #[test]
    fn command_ids_map_to_events() {
        assert_eq!(command_to_event(CMD_SHOW), Some(TrayEvent::Show));
        assert_eq!(command_to_event(CMD_CLEAR), Some(TrayEvent::ClearHistory));
        assert_eq!(command_to_event(CMD_QUIT), Some(TrayEvent::Quit));
        assert_eq!(command_to_event(0xDEAD), None);
    }

    /// 按 `NOTIFYICON_VERSION_4` 的格式打包一个回调 `lParam`。
    ///
    /// `lParam = (mouse << 16) | notify`，坐标不参与这两段判定。
    fn v4_lparam(mouse: u32, notify: u32) -> isize {
        (((mouse as isize) << 16) | notify as isize) as isize
    }

    #[test]
    fn notify_select_maps_to_show() {
        // 版本 4 正常路径：低位 NIN_SELECT，高位是鼠标码。
        assert_eq!(
            notify_event_of(v4_lparam(WM_LBUTTONUP_CODE, nin::SELECT)),
            Some(TrayEvent::Show)
        );
        assert_eq!(
            notify_event_of(v4_lparam(WM_LBUTTONDBLCLK_CODE, nin::SELECT)),
            Some(TrayEvent::Show)
        );
        assert_eq!(
            notify_event_of(v4_lparam(0, nin::KEYSELECT)),
            Some(TrayEvent::Show)
        );
        // 裸码也认（低位直接是通知码时）。
        assert_eq!(notify_event_of(nin::SELECT as isize), Some(TrayEvent::Show));
        assert_eq!(
            notify_event_of(nin::KEYSELECT as isize),
            Some(TrayEvent::Show)
        );
    }

    #[test]
    fn unknown_notify_event_is_none() {
        assert_eq!(notify_event_of(0xFFFF), None);
        assert_eq!(notify_event_of(-1), None);
    }

    #[test]
    fn legacy_left_click_also_maps_to_show() {
        // 未成功 NIM_SETVERSION 时 shell 发旧式 WM_LBUTTONUP/DBLCLK，
        // 若不映射成 Show 就会出现「点了托盘没反应」。
        assert_eq!(
            notify_event_of(WM_LBUTTONUP_CODE as isize),
            Some(TrayEvent::Show)
        );
        assert_eq!(
            notify_event_of(WM_LBUTTONDBLCLK_CODE as isize),
            Some(TrayEvent::Show)
        );
    }

    #[test]
    fn right_click_never_maps_to_show() {
        // 右键只弹菜单，不能顺带唤起窗口。
        assert_eq!(
            notify_event_of(v4_lparam(WM_RBUTTONUP_CODE, nin::SELECT)),
            None,
            "右键绝不能顺带弹出主窗口"
        );
        assert_eq!(
            notify_event_of(v4_lparam(WM_CONTEXTMENU_CODE, nin::SELECT)),
            None
        );
        assert_eq!(notify_event_of(WM_RBUTTONUP_CODE as isize), None);
        assert_eq!(notify_event_of(WM_CONTEXTMENU_CODE as isize), None);
    }

    /// **本条守着用户报告的那个 bug**。
    ///
    /// 版本 4 下右键的鼠标码在 `lParam` **高位**、低位是 `NIN_SELECT`。
    /// 早前 `is_context_menu_event` 只看低位，于是永远不匹配 ——
    /// 症状正是「左键正常、右键完全没反应」。
    #[test]
    fn right_click_requests_context_menu() {
        // 版本 4 正常路径：高位是右键码，低位是 NIN_SELECT。
        assert!(
            is_context_menu_event(v4_lparam(WM_RBUTTONUP_CODE, nin::SELECT)),
            "版本 4 的右键（高位 WM_RBUTTONUP）必须弹菜单"
        );
        assert!(is_context_menu_event(v4_lparam(
            WM_CONTEXTMENU_CODE,
            nin::SELECT
        )));
        // 旧式路径：低位直接是右键码。
        assert!(is_context_menu_event(WM_RBUTTONUP_CODE as isize));
        assert!(is_context_menu_event(WM_CONTEXTMENU_CODE as isize));
        // 左键**不能**被当成右键。
        assert!(!is_context_menu_event(v4_lparam(WM_LBUTTONUP_CODE, nin::SELECT)));
        assert!(!is_context_menu_event(WM_LBUTTONUP_CODE as isize));
    }

    /// 右键回调的两种形态都应当被识别。
    ///
    /// 这条是上条的补充：真实 shell 在版本 4 下可能发
    /// `WM_CONTEXTMENU` 而非 `WM_RBUTTONUP`（触屏/键盘激活路径），
    /// 漏掉任一条都会表现为「右键时好时坏」。
    #[test]
    fn context_menu_event_is_recognised_in_both_forms() {
        for mouse in [WM_RBUTTONUP_CODE, WM_CONTEXTMENU_CODE] {
            assert!(
                is_context_menu_event(v4_lparam(mouse, nin::SELECT)),
                "鼠标码 {mouse:#x} 应被识别为右键"
            );
        }
    }

    #[test]
    fn cursor_unpacking_roundtrips() {
        let (x, y) = unpack_cursor(100 | (200 << 16));
        assert_eq!((x, y), (100, 200));
    }

    /// 屏幕**左/下边缘**会出现负坐标，必须按有符号解读。
    ///
    /// 托盘右键菜单常正好弹在这些位置——若按无符号解出 65535 之类的
    /// 巨大正值，菜单会弹到屏幕外，表现为「点了没反应」。
    #[test]
    fn cursor_unpack_handles_negative_coordinates() {
        // x = -1（屏幕左缘外侧），y = -3。
        let (x, y) = unpack_cursor((-1i32 as isize & 0xFFFF) | ((-3i32 as isize) << 16));
        assert_eq!(x, -1, "x 为负时必须解成 -1，而不是 65535");
        assert_eq!(y, -3, "y 为负时必须解成 -3，而不是 65533");

        // 对照：正坐标不受影响。
        assert_eq!(unpack_cursor(100 | (200 << 16)), (100, 200));
    }

    #[test]
    fn callback_message_does_not_collide_with_nin() {
        // WM_TRAY_CALLBACK 与 WM_TRAY_MENU 必须避开 NIN_* (1024..=1031)。
        for m in [WM_TRAY_CALLBACK, WM_TRAY_MENU] {
            assert!((1024..=1031).contains(&m).eq(&false), "消息 {m} 与 NIN_* 撞车");
            assert!(m > 1031, "消息 {m} 应落在 WM_USER 之上");
        }
    }
}