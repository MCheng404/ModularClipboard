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
    /// 快捷键呼出的**显示方向**已由托盘线程在 WM_HOTKEY 授权窗口内
    /// 直接完成（窗口已 `ShowWindow` + `SetForegroundWindow`）。
    ///
    /// 与 [`Self::Hotkey`] 的区别：普通 `Hotkey` 交给上层按 toggle
    /// 语义处理（双进程 daemon 会按窗口可见性决定隐藏/显示）；
    /// `HotkeyShown` 表示窗口已经被显示并置前，上层**不得再 toggle
    /// 一次**（否则会把刚呼出的窗口立刻藏回去）。UI 进程自身的
    /// 「可见性对账」会发现窗口已可见并恢复渲染。
    HotkeyShown,
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
    // `as_chunks_mut` 而非 `chunks_exact_mut`：常量 4 用它才是惯用法
    // （clippy::chunks_exact_to_as_chunks，CI 的新 clippy 会 -D 拒绝）。
    let (pxs, _) = buf.as_chunks_mut::<4>();
    for px in pxs {
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
mod imp;


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
        let opaque = buf.as_chunks::<4>().0.iter().filter(|p| p[3] > 0).count();
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