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
use crate::hotkey;
use std::sync::mpsc::{Receiver, Sender, channel};
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
    /// 「显示主窗口」快捷键被按下。
    Hotkey,
    /// 迷你面板里选中了某个条目（携带条目 id）。
    ///
    /// 面板本身在托盘线程里弹出（系统菜单），但**复制动作必须由
    /// 持有数据的那一侧执行**——托盘线程没有数据库。
    MiniPick(i64),
    /// 「粘贴上一条」快捷键被按下。
    ///
    /// 与 [`Self::Hotkey`] 分开：两个槽位可以绑到不同组合，
    /// 语义也不一样（一个是唤界面、一个是直接把上一条粘到前台窗口）。
    HotkeyPaste,
}

/// 图标边长（像素）。托盘小图标标准尺寸为 16，取 32 让高分屏缩放后仍清晰。
pub const ICON_SIZE: usize = 32;

/// 内嵌的托盘图标（32×32，透明 PNG）。
///
/// 由 `scripts/make_icons.py` 生成，与应用程序图标（`assets/icon.ico`）
/// **同一套几何**——只在小尺寸上做了简化：文本线两条而不是三条，
/// 因为 16px 下板身只有十几像素高，三条线会糊成一片灰。
///
/// # 为什么用资源文件而不是代码画像素
///
/// 前几版是拿整数矩形判定逐个像素画出来的（`Rect::contains`）。那样
/// 做有两个死结：抗锯齿画不出来（托盘把 32px 缩到 16px 时边缘发虚），
/// 而且改一次造型就要改一次绘图代码——设计变更被绑在代码逻辑上。
/// 换成资源文件后，改图标 = 重跑脚本 + 换文件。
const TRAY_PNG: &[u8] = include_bytes!("../assets/tray.png");

/// 板身主色（BGRA）。
///
/// 测试用它断言「内嵌的确实是我们的图标」，而不是某个占位图。
/// 值取自 `scripts/make_icons.py` 里的 `BLUE = #1F6FEB`。
#[cfg(test)]
const COLOR_BOARD: [u8; 4] = [0xeb, 0x6f, 0x1f, 0xff];

/// 生成托盘图标的 BGRA 缓冲，长度 `ICON_SIZE * ICON_SIZE * 4`。
///
/// # 为什么返回 BGRA
///
/// `CreateDIBSection` 的 32bpp 位图在内存里通道序是 **B,G,R,A**
/// （与文件里的 RGBA 相反），而调用方会把它整块 `memcpy` 进 DIB。
/// 在这里换好通道，省掉调用侧一次全缓冲遍历。
///
/// 返回 `Result` 而不是直接 panic：图标解码失败只该让「没有托盘图标」，
/// 不该把整个常驻进程带走——剪贴板监听比托盘图标重要得多。
pub fn icon_rgba() -> Result<Vec<u8>> {
    let img = image::load_from_memory(TRAY_PNG)
        .context("内置托盘图标解码失败")?
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    if (w, h) != (ICON_SIZE as u32, ICON_SIZE as u32) {
        anyhow::bail!(
            "内置托盘图标尺寸为 {w}×{h}，应为 {ICON_SIZE}×{ICON_SIZE}（重跑 scripts/make_icons.py）"
        );
    }
    let mut buf = img.into_raw();
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA -> BGRA
    }
    Ok(buf)
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

/// 右键菜单是否正在显示（防重入）。
///
/// shell 对一次右键可能发**两条**回调（`WM_RBUTTONUP` 与
/// `WM_CONTEXTMENU` 都被 `is_context_menu_event` 认作右键），
/// 于是消息循环会收到两条 `WM_TRAY_MENU`。若照单全收，第二条会在
/// 第一条的 `TrackPopupMenu` **模态循环里**被派发 → 嵌套再弹一个菜单
/// → 用户看到「菜单出现一帧就被顶掉」（2026-10-13 用户实测）。
static MENU_SHOWING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 重入标志的 RAII 复位：函数内任何 return 路径都保证归位
/// （早退路径漏复位会让菜单**永远**弹不出来）。
struct MenuShowingGuard;

impl Drop for MenuShowingGuard {
    fn drop(&mut self) {
        MENU_SHOWING.store(false, std::sync::atomic::Ordering::Release);
    }
}

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
    pub const SELECT: u32 = WM_USER_BASE;
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
/// # 坐标怎么取
///
/// 回调 `lParam` 里**不携带**屏幕坐标（它是消息码的打包体），
/// 菜单弹出位置改用 `GetCursorPos` 直接取当前光标（见
/// `show_context_menu`），不再从这里解坐标。
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
/// NOTE: 判定要**同时**看高位鼠标码与低位通知码：
/// - 版本 4 正常路径：低位是 `NIN_SELECT` / `NIN_KEYSELECT`；
/// - `NIM_SETVERSION` 失败时回退旧式语义：高位直接是
///   `WM_LBUTTONUP` / `WM_LBUTTONDBLCLK`。
///
/// 两条都映射为 [`TrayEvent::Show`]，否则用户会「点了没反应」。
fn notify_event_of(lparam: isize) -> Option<TrayEvent> {
    let cb = Callback::decode(lparam);

    // NOTE: 顺序要紧：**先排除右键**，再处理左键。
    //
    // 版本 4 下右键回调的 `lParam` 里，通知码同样是 `NIN_SELECT`
    // （shell 对所有鼠标事件都发 `NIN_SELECT`），只有高位的鼠标码
    // 区分左右键。若先匹配 `NIN_SELECT => Show`，右键会同时
    // 「弹菜单」并「弹出主窗口」——用户看到的是菜单闪一下、
    // 窗口却莫名其妙跑出来。
    if is_context_menu_event(lparam) {
        return None;
    }
    match cb.notify {
        nin::SELECT | nin::KEYSELECT => Some(TrayEvent::Show),
        // 旧式语义（`NIM_SETVERSION` 失败时）：`lParam` 低位直接是
        // 鼠标消息码，此时 `cb.mouse == 0`，所以低位也要认。
        m if m == WM_LBUTTONUP_CODE || m == WM_LBUTTONDBLCLK_CODE => Some(TrayEvent::Show),
        _ => match cb.mouse {
            WM_LBUTTONUP_CODE | WM_LBUTTONDBLCLK_CODE => Some(TrayEvent::Show),
            _ => None,
        },
    }
}

/// 托盘右键事件 → 是否应弹菜单。
///
/// IMPORTANT: 判定必须在**高位**（鼠标消息码），见 [`Callback`] 的说明。
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
    /// 启动时注册失败的快捷键（已含槽位名与组合文本）。
    failed_hotkeys: Vec<String>,
}

impl Tray {
    /// 非阻塞取回全部待处理事件。
    pub fn poll(&self) -> Vec<TrayEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.try_recv() {
            out.push(ev);
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

    /// 启动时注册失败的快捷键描述（空 = 全部注册成功）。
    ///
    /// 调用方（常驻进程）把它转成给用户看的信息——
    /// 失败发生在托盘线程里，只有这里能把结果递出去。
    pub fn failed_hotkeys(&self) -> &[String] {
        &self.failed_hotkeys
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

/// 迷你面板触发所需的修饰键位（与 Win32 `MOD_*` 同值）。
///
/// 公开在模块外：掩码由调用方（常驻进程按配置）组装后传进来，
/// 托盘这边只负责比对。
pub const MINI_MOD_CTRL: u32 = 0x0002;
pub const MINI_MOD_SHIFT: u32 = 0x0004;
pub const MINI_MOD_ALT: u32 = 0x0001;
pub const MINI_MOD_WIN: u32 = 0x0008;

/// 从配置计算迷你面板的修饰键掩码。
///
/// `None` = 不启用。两种情形都算不启用：开关没开；开关开了但一个
/// 修饰键都没勾——后者会让**每次右键**都弹面板，用户的右键等于被
/// 抢走，比没有这个功能更糟。
///
/// 判定集中在这里：常驻进程与单进程模式的界面都要做同一个判断，
/// 两边各写一份迟早会漂移。
pub fn mini_mask(cfg: &modular_clipboard_core::config::MiniPanelConfig) -> Option<u32> {
    if !cfg.enabled {
        return None;
    }
    let mut mask = 0u32;
    if cfg.ctrl {
        mask |= MINI_MOD_CTRL;
    }
    if cfg.shift {
        mask |= MINI_MOD_SHIFT;
    }
    if cfg.alt {
        mask |= MINI_MOD_ALT;
    }
    if cfg.win {
        mask |= MINI_MOD_WIN;
    }
    if mask == 0 {
        tracing::warn!("迷你面板已启用但没勾任何修饰键，视为停用");
        return None;
    }
    Some(mask)
}

/// 迷你面板的条目快照：`(条目 id, 预览文本)`。
///
/// 由持有数据的那一侧（常驻进程）更新，托盘线程只读它弹菜单。
/// 用 `Arc<Mutex<..>>` 而不是每次弹菜单时临时传：钩子是异步触发的，
/// 触发的那一刻没有机会去问「现在有哪些条目」。
pub type MiniItems = std::sync::Arc<std::sync::Mutex<Vec<(i64, String)>>>;

/// 启用 / 停用迷你面板。
///
/// `modifiers` 是触发所需的修饰键位掩码，取值见 [`MINI_MOD_CTRL`] 等。
/// `None` 表示停用（卸载钩子）。
pub fn set_mini_panel(modifiers: Option<u32>, items: MiniItems) -> bool {
    imp::set_mini_panel(modifiers, items)
}

#[cfg(not(windows))]
pub fn set_mini_panel(_modifiers: Option<u32>, _items: MiniItems) -> bool {
    false
}

/// 运行时改绑定（**同步**，返回注册是否成功）。
///
/// `false` 有两种可能：组合已被其它程序占用，或托盘窗口已经不在了
/// （正在退出）。调用方应把它变成用户可见的提示——静默失败等于
/// 「改了键位但按键没反应」的老 bug。
pub fn set_hotkey(slot: hotkey::Slot, spec: hotkey::HotkeySpec) -> bool {
    imp::set_hotkey(slot, spec)
}

/// 录制快捷键期间临时注销全部全局热键。
///
/// # 为什么需要它
///
/// `RegisterHotKey` 注册的组合会被 OS **从所有窗口没收**：按键不再
/// 产生 `WM_KEYDOWN`，而是直接送给注册窗口。于是用户在设置里点
/// 「录制」后按下**当前正绑着的组合**（比如想把 Win+V 换掉，先按
/// 一下 Win+V 确认）时，设置窗口永远收不到这个按键——录制像卡死，
/// 用户反馈就是「录制有问题」。
///
/// 恢复**不在本函数里做**：托盘线程不持有配置，「原来绑的什么」
/// 只有调用方知道。恢复 = 按当前配置逐槽位调 [`set_hotkey`]。
///
/// 用 `SendMessageW` 而不是 `PostMessageW`：必须与随后的恢复注册
/// **严格有序**——sent message 优先级高于队列里的 posted message，
/// 用 PostMessage 会出现「恢复先跑、紧接着被注销覆盖、热键全丢」。
pub fn suspend_hotkeys() -> bool {
    imp::suspend_hotkeys()
}

#[cfg(not(windows))]
pub fn suspend_hotkeys() -> bool {
    false
}

/// 启动托盘线程并添加图标，同时注册两个热键槽位。
///
/// 阻塞至图标真正出现在通知区后才返回，失败则返回 `Err`
/// （不会留下半初始化的后台线程）。
///
/// # 为什么要显式传进来
///
/// 托盘线程只能注册**它自己创建窗口时**拿到的组合。若让托盘去读
/// 配置文件，就要把配置跨线程搬过去；而调用方（常驻进程）本来
/// 就持有配置，传进来最直接。
pub fn start_with_hotkeys(
    tooltip: &str,
    toggle: hotkey::HotkeySpec,
    paste_previous: hotkey::HotkeySpec,
) -> Result<Tray> {
    imp::start(tooltip, toggle, paste_previous)
}

#[cfg(not(windows))]
pub fn start_with_hotkeys(
    _tooltip: &str,
    _toggle: hotkey::HotkeySpec,
    _paste: hotkey::HotkeySpec,
) -> Result<Tray> {
    anyhow::bail!("当前平台尚未实现托盘")
}

#[cfg(not(windows))]
pub fn set_hotkey(_slot: hotkey::Slot, _spec: hotkey::HotkeySpec) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Windows 实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
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
                    _ => emit(TrayEvent::Hotkey),
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

    /// 托盘线程主体：建资源→ 加图标 → 回报就绪 → 消息循环 → 清理。
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

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_buffer_length_matches_size() {
        assert_eq!(
            icon_rgba().unwrap().len(),
            ICON_SIZE * ICON_SIZE * 4,
            "缓冲长度必须与 CreateDIBSection 申请的字节数一致"
        );
    }

    #[test]
    fn icon_has_visible_pixels() {
        let buf = icon_rgba().unwrap();
        let opaque = buf.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(opaque > 100, "图标应有可见像素，实际 {opaque}");
        assert!(opaque < ICON_SIZE * ICON_SIZE, "四角应透明，不应铺满");
    }

    #[test]
    fn icon_corners_are_transparent() {
        let buf = icon_rgba().unwrap();
        let n = ICON_SIZE;
        // 四个角像素的 alpha 必须是 0，否则托盘显示为方块。
        for (x, y) in [(0, 0), (n - 1, 0), (0, n - 1), (n - 1, n - 1)] {
            assert_eq!(buf[(y * n + x) * 4 + 3], 0, "({x},{y}) 应透明");
        }
    }

    /// 板身中部必须是板色：这一条守住「换图标时别把配色换了」。
    ///
    /// 坐标取自托盘版几何（板身 x 0.11~0.89、白线 y 0.38~0.47 与
    /// 0.59~0.68，见 `scripts/make_icons.py::draw_tray_master`）。
    #[test]
    fn icon_has_board_body() {
        let buf = icon_rgba().unwrap();
        let n = ICON_SIZE;
        // (6, 11)：板身内、两条白线之间、夹子下方。
        let i = (11 * n + 6) * 4;
        assert_eq!(&buf[i..i + 4], &COLOR_BOARD, "板身应为主题蓝（BGRA）");
    }

    /// 白线必须真的画出来了，而且要**两条都**在位。
    ///
    /// 判据是「接近白」而不是「精确等于 255」：白线在 32px 下只有
    /// 两像素高，LANCZOS 缩放会让其中一行落在过渡带上（实测 237,244,254）。
    /// 卡死精确值会让这条测试在图标做了任何微调后误报。
    #[test]
    fn icon_has_text_lines() {
        let buf = icon_rgba().unwrap();
        let n = ICON_SIZE;
        for y in [13usize, 20usize] {
            let i = (y * n + 12) * 4;
            let px = &buf[i..i + 4];
            assert!(
                px[0] >= 240 && px[1] >= 240 && px[2] >= 240 && px[3] == 255,
                "第 {y} 行应是文本线（白），实际 {px:?}"
            );
        }
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
        (mouse as isize) << 16 | notify as isize
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
    fn callback_message_does_not_collide_with_nin() {
        // WM_TRAY_CALLBACK 与 WM_TRAY_MENU 必须避开 NIN_* (1024..=1031)。
        for m in [WM_TRAY_CALLBACK, WM_TRAY_MENU] {
            assert!((1024..=1031).contains(&m).eq(&false), "消息 {m} 与 NIN_* 撞车");
            assert!(m > 1031, "消息 {m} 应落在 WM_USER 之上");
        }
    }

    /// 迷你面板的启用判定：开关 + 至少一个修饰键。
    ///
    /// 「开了但一个修饰键都没勾」必须视为停用——否则**每次右键**都会
    /// 弹面板，用户的右键等于被抢走，比没有这个功能更糟。
    #[test]
    fn mini_mask_requires_enabled_and_a_modifier() {
        use modular_clipboard_core::config::MiniPanelConfig;

        // 默认关闭（即使默认勾了 Ctrl）。
        let cfg = MiniPanelConfig::default();
        assert_eq!(mini_mask(&cfg), None, "默认关闭时不应启用");

        // 开启且有 Ctrl。
        let mut cfg = cfg;
        cfg.enabled = true;
        assert_eq!(mini_mask(&cfg), Some(MINI_MOD_CTRL));

        // 开了但没有任何修饰键 ⇒ 停用。
        cfg.ctrl = false;
        assert_eq!(mini_mask(&cfg), None, "无修饰键应视为停用");

        // 多修饰键按位或。
        cfg.alt = true;
        cfg.win = true;
        let mask = mini_mask(&cfg).expect("应启用");
        assert_ne!(mask & MINI_MOD_ALT, 0);
        assert_ne!(mask & MINI_MOD_WIN, 0);
        assert_eq!(mask & MINI_MOD_CTRL, 0);
        assert_eq!(mask & MINI_MOD_SHIFT, 0);
    }
}