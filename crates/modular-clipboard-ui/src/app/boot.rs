//! 启动：入口函数与启动期辅助。
//!
//! 四个 `run*` 是 crate 对外的界面入口；其余是启动顺序相关的
//! 纯函数/小助手（背景实度、清屏色、env 开关、窗口坐标 sanity）。

use modular_clipboard_app::Service;

use super::frame_loop::run_mode;

/// 背景层实度：在「用户滑块给的基准」之上，按**背景与主题的明暗冲突程度**上调。
///
/// # 为什么需要它
///
/// 半透明/磨砂把窗口背景与外部画面混在一起，而外部画面的明暗完全不由
/// 主题决定。浅色主题的文字是深色（`text = #24292E`），一旦背景被身后的
/// 纯黑桌面压暗，深色文字就落在深灰底上——对比度不足，用户只会觉得
/// 「窗口感官坏了」，却说不清哪里坏了。
///
/// # 判据
///
/// `mismatch = 主题是暗色 ? 背景亮度 : 1 - 背景亮度`：
/// 主题与背景同向（浅色配白桌面）时接近 0，保持用户要的通透；
/// 反向（浅色配黑桌面、深色配白桌面）时接近 1，把背景层推向更实。
///
/// 上限 0.88 而不是 1.0：留一点透出，否则磨砂就成了纯色底、白改一场。
pub(super) fn backdrop_alpha(base: f32, luminance: Option<f32>, is_dark: bool) -> u8 {
    let Some(l) = luminance else {
        // 还没捕获成功（首帧 / 最小化），按基准走。
        return (base.clamp(0.1, 1.0) * 255.0).round() as u8;
    };
    let mismatch = if is_dark { l } else { 1.0 - l };
    let a = base + mismatch * 0.35;
    (a.clamp(0.30, 0.88) * 255.0).round() as u8
}

/// 普通模式（无背景效果）的清屏色。与 gfx 渲染器的默认值一致，
/// 仅在用户从磨砂/半透明切回「无」时恢复用。
pub(super) const DEFAULT_CLEAR_COLOR: [f32; 4] = [0.10, 0.10, 0.12, 1.0];
/// 磨砂/半透明模式的清屏色：全透明黑。
///
/// 这两种模式的背景层由渲染层画**低 alpha**（调色板变体），交换链
/// 按像素 alpha 与桌面合成；清屏 alpha 若为 1，未被背景层覆盖的像素
/// （以及背景层之下的混合底）会变成不透明，效果直接失效。
pub(super) const TRANSPARENT_CLEAR_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

/// 离开作用域时自动清空 [`egui::TexturesDelta`]。
///
/// # 为什么需要它
///
/// `TexturesDelta` 的 `Drop` 里有一条 `debug_assert!(is_empty())`：
/// 增量必须被消费或显式 `clear()`，否则 panic。手写 `clear()` 极易漏——
/// 帧循环里 `?` 提前返回、`continue`、最小化跳过的分支各都要写一遍，
/// 漏一处就是一个**与真实错误毫无关系**的 panic，把排查方向带偏。
///
/// 我第一版就踩了这个坑：`full_app` 首帧明明成功渲染了 552 个顶点，
/// 却在第二帧因为一处漏清的 delta panic 退出。用作用域守卫从根上消除。
///
/// CAUTION: 不只是 debug 构建的问题。release 下断言编译掉，delta 被
/// **静默丢弃** ⇒ 那一帧的字体图集 / 用户纹理增量永远传不上 GPU，而
/// egui 侧的内存已经认为「传过了」（增量只在纹理变化时产出）⇒ 窗口
/// 空白或字形缺失，且**不会**在下一帧自愈（要等纹理再次变化）。
/// 因此所有「跑完 `run_ui` 之后」的分支都必须经由本守卫离开。
pub(crate) struct DeltaGuard(pub(crate) egui::TexturesDelta);

impl Drop for DeltaGuard {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// 托盘图标的悬停提示。
///
/// 放在标题后面而非直接用 `WINDOW_TITLE`：托盘提示要告诉用户
/// 「东西还在后台跑着」，而不只是产品名——这是托盘常驻类程序
/// 与普通窗口程序的体感差别。
pub(super) const TRAY_TOOLTIP: &str = "ModularClipboard"; // 托盘悬停提示（简短：tooltip 空间有限）

/// 环境开关查询（带缓存，PF-10）。
///
/// `std::env::var_os` 在 Windows 上要遍历进程环境块，而帧循环每帧要查
/// 3~4 次（`MC_DIAG` ×3、`MC_SETTINGS` ×1）——这些开关只在启动时设定，
/// 没有热切换需求，缓存结果不改变任何行为。每个不同的名字最多碰一次
/// 真实环境块，此后是一次 Mutex 加锁 + HashMap 查找。
pub(super) fn env_flag(name: &str) -> bool {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, bool>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    *guard
        .entry(name.to_string())
        .or_insert_with(|| std::env::var_os(name).is_some())
}

/// 启动图形界面。`config` 为初始配置。
pub fn run(config: modular_clipboard_core::Config) -> anyhow::Result<()> {
    run_with_options(config, false, None)
}

/// 同 [`run`]，但显式声明 `config.capture.enabled` 是否为命令行临时覆盖。
///
/// `--no-capture` 会把它置false 以便调试时不动真实剪贴板；
/// 若这个临时值被 [``Service::save_config``] 写进config.json，
/// 用户的剪贴板监听会**永久关闭**且无任何提示。
/// 传 `true` 让它跳过落盘。
pub fn run_with_capture_override(
    config: modular_clipboard_core::Config,
    capture_is_override: bool,
) -> anyhow::Result<()> {
    run_with_options(config, capture_is_override, None)
}

/// 同 [`run`]，并接受 `--data-dir` 指定的���据目录。
///
/// `data_dir` 一路传到 [`Service::with_data_dir`]：数据库、载荷目录、
/// 配置文件三者都由它派生。传 `None` 时用 `%APPDATA%/modular-clipboard`。
pub fn run_with_options(
    config: modular_clipboard_core::Config,
    capture_is_override: bool,
    data_dir: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let svc = match Service::with_data_dir(config.clone(), data_dir) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%e, "服务初始化失败，界面将以空状态运行");
            // 用内存库兜底，避免整个应用无法启动。
            Service::in_memory()
        }
    };
    run_mode(config, svc, capture_is_override, false)
}

/// 界面进程入口：连常驻进程、建窗口、关闭即退出本进程。
///
/// # 与单进程模式的两点差别
///
/// 1. **不建托盘**：托盘在常驻进程里。界面自己建托盘的话，
///    关掉界面托盘就消失，用户再也唤不回程序。
/// 2. **关闭窗口 = 退出进程**：这正是拆进程的目的——
///    进程一退，Vulkan/字体/纹理那些销毁了也拿不回的内存
///    由操作系统整体回收。
pub fn run_remote() -> anyhow::Result<()> {
    // 实例守卫：让常驻进程知道「界面已在运行」，
    // 从而在托盘点击时选择「唤起」而不是「再起一个」。
    //
    // 抢不到说明已经有一个界面在跑——此时本进程不该继续，
    // 否则两个界面会互相抢窗口前台。
    let _instance = match modular_clipboard_ipc::UiInstanceGuard::acquire() {
        Ok(g) => g,
        Err(e) => {
            tracing::info!(%e, "界面已在运行，本进程退出");
            return Ok(());
        }
    };
    // 5 秒缓冲：托盘刚拉起界面时，常驻进程的管道可能还没建好。
    let svc = Service::connect_remote(std::time::Duration::from_secs(5))?;
    // 界面配置以常驻进程那份为准（见 `App::new_remote` 的说明）。
    let config = svc.state.config.clone();
    // 界面进程**不**持有捕获开关的临时覆盖：那是常驻进程的事。
    run_mode(config, svc, false, true)
}

/// 窗口坐标是否**可安全写入配置**。
///
/// # 为什么阈值不是 `-32000`
///
/// `GetWindowRect` 对**最小化**的窗口返回任务栏图标矩形，典型值
/// `(-32000, -32000)` —— 但那是**物理像素**。而
/// `Window::screen_position_points()` 返回的是**逻辑点**
/// （`r.left as f32 / scale_factor`，见 `gfx/src/window.rs`）。
///
/// 于是 150% 缩放下传入的是 `-32000 / 1.5 = -21333.3`，而判据若写
/// `p.x <= -32000.0` 就**漏过** ⇒ 把最小化坐标写进配置 ⇒ 下次启动
/// `move_to(p.x * scale)` 把它还原成物理 `-32000` ⇒ **窗口跑到屏幕外**，
/// 用户只看到托盘图标，主界面「不见了」。
///
/// 用 `-8000` 这个阈值则在任何常见缩放（1.0 ~ 4.0）下都能拦住，
/// 同时不会误伤真实的多显示器布局（副屏在主屏左侧时，坐标最小也就
/// 约 -4000，4K 屏也不过 -3840）。
///
/// # 为什么不用 `IsIconic`
///
/// 那更准确，但本函数只拿到坐标、拿不到 `HWND`。把判据收在一处，
/// 是为了让三个写入点（主窗口 / 置顶窗口 / 卡片子窗口）口径一致。
pub(crate) fn is_sane_window_pos(p: egui::epaint::emath::Vec2) -> bool {
    p.is_finite() && p.x > -8000.0 && p.y > -8000.0
}
