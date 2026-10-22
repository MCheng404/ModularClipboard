//! 应用入口：窗口 + 事件循环 + 帧循环。
//!
//! # 一帧的完整时序
//!
//! ```text
//! EventLoop::poll            取 Win32 消息，处理 Resized / CloseRequested
//! EventLoop::egui_input      翻译成 RawInput（必须在 poll 之后）
//! Context::run_ui            跑业务 UI，产出 FullOutput
//! Window::set_drag_region    上报自绘标题栏拖动区（必须在 run_ui 之后）
//! Context::tessellate       图元 → ClippedPrimitive
//! FrameRenderer::acquire     取交换链图像；返回 None 表示需重建
//! Painter::paint             顶点上传 + 纹理上传 + 描述符绑定 + record
//! FrameRenderer::present     提交并呈现
//! EventLoop::poll_for        按 egui 请求的延时等待下一帧
//! ```
//!
//! # 三个容易踩的顺序问题
//!
//! 1. **`poll` 必须在 `egui_input` 之前**。`egui_input` 读的是 `poll` 填好的
//!    内部缓冲；顺序反了会丢掉本帧的全部输入（表现为「点击没反应」）。
//! 2. **`paint` 必须在 `acquire` 之后、`present` 之前**。顶点写入依赖
//!    `current_slot()`，而 `acquire` 已等待过该槽位栅栏——这是「往该槽位
//!    写数据」安全的前提。
//! 3. **`textures_delta` 必须被消费或显式 `clear()`**。egui 在
//!    `TexturesDelta::drop` 里断言增量为空，忘记清空会在 debug 构建下误报。
//! 4. **`set_drag_region` 必须每帧、且在 `run_ui` 之后**。拖动区由
//!    `view::draw` 按本帧布局算出，顺序反了会用到上一帧的矩形；
//!    只在启动时设一次则会在窗口 resize / 模块折叠后错位。

use modular_clipboard_core::WindowPos;
use modular_clipboard_app::Service;
use modular_clipboard_gfx::frame::{FrameRenderer, PipelineBundle, PresentResult};
use modular_clipboard_gfx::window::{EventLoop, Window, WindowEvent};
use modular_clipboard_gfx::Gpu;
use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

use crate::presence::{self, CloseDecision, Resident};
use crate::renderer::{Painter, WINDOW_TITLE};
use crate::theme;

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
fn backdrop_alpha(base: f32, luminance: Option<f32>, is_dark: bool) -> u8 {
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
const DEFAULT_CLEAR_COLOR: [f32; 4] = [0.10, 0.10, 0.12, 1.0];
/// 磨砂/半透明模式的清屏色：全透明黑。
///
/// 这两种模式的背景层由渲染层画**低 alpha**（调色板变体），交换链
/// 按像素 alpha 与桌面合成；清屏 alpha 若为 1，未被背景层覆盖的像素
/// （以及背景层之下的混合底）会变成不透明，效果直接失效。
const TRANSPARENT_CLEAR_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

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
const TRAY_TOOLTIP: &str = "ModularClipboard"; // 托盘悬停提示（简短：tooltip 空间有限）

/// 环境开关查询（带缓存，PF-10）。
///
/// `std::env::var_os` 在 Windows 上要遍历进程环境块，而帧循环每帧要查
/// 3~4 次（`MC_DIAG` ×3、`MC_SETTINGS` ×1）——这些开关只在启动时设定，
/// 没有热切换需求，缓存结果不改变任何行为。每个不同的名字最多碰一次
/// 真实环境块，此后是一次 Mutex 加锁 + HashMap 查找。
fn env_flag(name: &str) -> bool {
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

/// 两种模式共用的帧循环。
///
/// `remote` 为真时界面不建托盘、关闭窗口即退出进程，
/// 其余（渲染、绘制、事件）完全一致。
fn run_mode(
    config: modular_clipboard_core::Config,
    svc: Service,
    capture_is_override: bool,
    remote: bool,
) -> anyhow::Result<()> {
    // NOTE: 先取 `show_tray` 快照：`config` 后面会被移动进 `App`，
    // 到那时就访问不到了。
    //
    // 远端模式下**恒为 false**：托盘归常驻进程所有。界面自己建托盘的话，
    // 关掉界面托盘就没了，用户再也唤不回程序。
    let want_tray = config.ui.show_tray && !remote;

    // 热键规格在这里算好再传给托盘：`config` 稍后会被移进 `App`，
    // 到那时就访问不到了，而托盘注册需要它。
    let toggle_spec =
        modular_clipboard_platform::hotkey::spec_or_default(&config.hotkey_toggle);
    let paste_spec =
        modular_clipboard_platform::hotkey::spec_or_default(&config.hotkey_paste_previous);

    // `Window::new` 的宽高是**物理像素**——`UiConfig::window_width/height`
    // 也是物理像素（见 config.rs 的字段文档），两者同口径，直接透传。
    //
    // NOTE（P2-8 口径澄清，2026-10-19）：这里曾写「配置里存的是逻辑点」，
    // 与 config.rs 直接矛盾。以 config.rs 为准：width/height 物理像素、
    // window_pos 屏幕**逻辑点**（历史语义，保留兼容）。
    let window = Window::new(
        WINDOW_TITLE,
        config.ui.window_width.max(1.0) as u32,
        config.ui.window_height.max(1.0) as u32,
    )?;
    // 恢复上次退出时的窗口位置。
    //
    // NOTE: 早前位置只存在内存里，每次启动都回屏幕左上角——
    // 用户拖动的位置在重启后丢失。位置存`UiConfig::window_pos`。
    //
    // `Option` 是必要的：`0.0` 是合法坐标（屏幕左上角），
    // 不能用它当「未设置」的哨兵值。
    if let Some(p) = config.ui.window_pos {
        let s = window.scale_factor();
        // P1-2（2026-10-19 DPI 治理）：位置以「退出时那台显示器 DPI 下的
        // 逻辑点」持久化，恢复时乘**当前** scale——混合 DPI 换屏/换机后
        // 物理位置按比例漂移（200% 屏存的 500，100% 屏恢复成 250）。
        // 彻底修法是持久化物理像素（需 config 语义迁移），这里先加护栏：
        // **钳制到工作区内**——漂移最多导致贴边，不会跑到屏外点不着。
        let w_px = (config.ui.window_width.max(1.0) * s) as i32;
        let h_px = (config.ui.window_height.max(1.0) * s) as i32;
        let (x, y) = crate::presence::clamp_to_work_area(
            window.hwnd(),
            (p.x * s) as i32,
            (p.y * s) as i32,
            w_px,
            h_px,
        );
        window.move_to(x, y);
    }
    // CAUTION: `CreateWindowExW` 创建的窗口**默认不可见**，
    // 必须显式 `ShowWindow` 才会出现在桌面上。
    //
    // 实测踩坑：早前这里从不调用它，窗口**从不显示**——
    // 实机启动 `modular-clipboard.exe` 后 `MainWindowTitle` 为空，
    // 用户只看到托盘图标，会以为程序没启动。
    //
    // `start_minimized` 配置项此前**从未被读取**（字段存在于
    // `UiConfig` 但代码里grep 不到），现已接上。
    //
    // 显示动作本身挪到了全部初始化之后（见「启动期窗口开关」一节，
    // PF-3）：在这里就 focus_window 会让窗口在 GPU 初始化 + 字体读入
    // + DB 打开期间以「无响应」状态可见。

    // IMPORTANT: 必须是 `new_primary` 而不是 `new`——`WM_QUIT` 是线程级
    // 消息、`PM_REMOVE` 谁先poll 谁摘走，归属必须显式指定（见
    // `EventLoop::owns_quit`）。用 `new` 的话弹窗/子窗可能先摘走它，
    // 主窗口 `quit_requested()` 恒 false ⇒ **点关闭 / 托盘退出后程序
    // 退不掉**，而那个附属窗口还把自己的 CloseRequested 误当成「关我」。
    let mut events = EventLoop::new_primary(&window);
    tracing::info!(
        width = window.inner_size_physical().0,
        height = window.inner_size_physical().1,
        scale = window.scale_factor(),
        "窗口已创建"
    );

    // ---- Vulkan 设备 ----------------------------------------------------
    let gpu = Gpu::new(WINDOW_TITLE, window.hinstance(), window.hwnd())?;
    // IMPORTANT: 必须在**第一次 `egui_input` 之前**告知纹理上限。
    //
    // egui 用这个值决定字体图集能长多大；缺省 2048 在超采样下不够
    // （图集面积按倍率平方增长），塞不下时的表现是**字形画错**
    // （齿轮丢齿、垃圾桶变形），且不报任何错。
    //
    // NOTE: 设在 `events` 上而不是 `window` 上——主窗口的 `EventLoop`
    // 建在 `Gpu::new` 之前，此刻它已经用默认值构造好了。
    events.set_max_texture_side(gpu.properties.limits.max_image_dimension2_d);

    // ---- 管线 ----------------------------------------------------------
    //
    // 渲染通道的附件格式必须与交换链最终选中的格式一致，否则 framebuffer
    // 与渲染通道不兼容，驱动会在 `cmd_begin_render_pass` 时崩。
    //
    // `modular_clipboard_gfx::pick_format` 是 `pub(crate)`，外部拿不到；而
    // `FrameRenderer::new` 内部会用它选交换链格式并**校验**与本处的
    // 渲染通道是否一致，不一致直接 `bail`。因此这里只需按同样的偏好
    // 预选，真实的一致性由那一道校验兜底。
    let surface_format = pick_surface_format(&gpu)?;
    let render_pass = modular_clipboard_gfx::pipeline::RenderPass::new(&gpu.device, surface_format)?;
    let desc_layout = modular_clipboard_gfx::pipeline::DescriptorLayout::new(
        &gpu.device,
        gpu.desc_caps.all_bindings_update_after_bind(),
    )?;
    let pipe_layout = modular_clipboard_gfx::pipeline::PipelineLayout::new(
        &gpu.device,
        std::slice::from_ref(&desc_layout.handle),
    )?;
    let shader = modular_clipboard_gfx::shader::ShaderModule::new(&gpu.device)?;
    let pipeline = modular_clipboard_gfx::pipeline::GraphicsPipeline::new(
        &gpu.device,
        render_pass.handle,
        pipe_layout.handle,
        &shader,
    )?;

    let mut fr = FrameRenderer::new(
        &gpu,
        PipelineBundle::new(
            render_pass.handle,
            pipe_layout.handle,
            pipeline.handle,
            desc_layout.handle,
        ),
    )?;
    // 交换链按 `Gpu` 启动时的表面快照建立。窗口尺寸在那之前已定，
    // 但主动重建一次可保证 framebuffer 与真实客户区严格一致。
    fr.rebuild_swapchain(Default::default())?;
    let mut painter = Painter::new(&gpu, fr.slot_count())?;
    tracing::info!(extent = ?fr.extent(), slots = fr.slot_count(), "渲染器就绪");

    // ---- 共享渲染资源（供卡片子窗口复用）------------------------------
    //
    // 子窗口与主窗口**共用同一个 `Gpu`**（Vulkan 设备创建代价高，
    // 且多设备会多占显存），但各自持有一条交换链。
    //
    // IMPORTANT: 必须在 `App::new(config, …)` **之前**取字体路径：那之后
    // `config` 就被移动进 `App` 了。
    let font_path = config.ui.font_path.clone();
    // IMPORTANT: 启动期要用的配置必须在这里取：`config` 下一行就被移动进 `App`。
    let start_minimized = config.ui.start_minimized;
    let always_on_top = config.ui.always_on_top;
    let shared =
        crate::multiwindow::SharedGfx::create(&gpu)?.with_font_path(font_path.as_deref());

    // ---- egui -----------------------------------------------------------
    let mut svc = svc;
    // 标记临时覆盖（见 `App::new` 的说明）：远端模式下 `set_capture_override`
    // 只是界面侧的标记，真正落盘由常驻进程决定。
    svc.set_capture_override(capture_is_override);
        // 界面语言：配置显式指定用配置，否则跟随系统（详见
        // `core::i18n::resolve`）。要在**画任何东西之前**解析完——
        // 首帧的全部文案都按这个语言取。
        modular_clipboard_core::i18n::set_locale(modular_clipboard_core::i18n::resolve(
            &config.ui.language,
            modular_clipboard_core::i18n::from_system_langid(crate::presence::system_langid()),
        ));
    let mut app = App::from_service(config, svc);
    let ctx = app.ctx.clone();

    // 恢复上次退出时的置顶窗口位置。
    //
    // IMPORTANT: 必须在第一次 `sync_with` 之前：那个函数按卡片上的
    // `window_pos` 建窗，之后再改就只影响下次启动了。
    let ui_cfg = app.svc.state.config.ui.clone();
    app.ws.apply_saved_positions(&ui_cfg);

    // ---- 托盘常驻--------------------------------------------------------
    //
    // 托盘自带独立线程与消息循环（见 `presence` 模块文档），
    // 这里只是每帧收一次事件。启动失败**不阻断应用**：
    // 托盘只是入口方式，没有它界面照样能用，只是关掉就没了。
    // 登记主窗口句柄：托盘右键菜单需要把它置前台才能显示。
    //
    // CAUTION: 必须在托盘启动**之前**登记——否则用户在这段时间内右键
    // 托盘图标，`TrackPopupMenu` 会挂在隐藏的托盘辅助窗口上，
    // 菜单闪一下就消失。
    modular_clipboard_platform::tray::set_main_hwnd(window.hwnd().0 as isize);

    // IMPORTANT: 必须尊重 `ui.show_tray` —— 设置界面里那个开关。
    //
    // 早前这里无条件 `Resident::start`，于是 `show_tray` 只被**写入**、
    // 从不被读取：用户在设置里关掉「显示托盘图标」，托盘照样在；
    // 开着也无所谓。配置项形同虚设。
    // 迷你面板的条目快照槽位：单进程模式下由界面进程自己维护
    // （常驻进程模式由 daemon 维护，界面不碰）。
    let mini_items: modular_clipboard_platform::tray::MiniItems =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let resident = if want_tray {
        match Resident::start(TRAY_TOOLTIP, toggle_spec, paste_spec) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::error!(%e, "托盘启动失败，关闭按钮将直接退出程序");
                None
            }
        }
    } else if remote {
        // 界面进程永远不建托盘：托盘归常驻进程。界面自己建的话，
        // 关掉界面托盘就没了，用户再也唤不回程序。
        tracing::info!("界面进程不创建托盘（托盘与后台记录都在常驻进程里）");
        None
    } else {
        tracing::info!("配置已关闭托盘图标，不启动托盘");
        None
    };

    // 单进程模式：托盘归本进程所有，热键与迷你面板也归本进程管。
    // （常驻进程模式下这两件事都在 daemon 里做，界面不碰。）
    if let Some(r) = resident.as_ref() {
        // 启动注册失败的快捷键要让用户知道——他按 Win+V 没反应，
        // 只会以为程序坏了。
        let failed = r.failed_hotkeys().join("；");
        app.svc.state.hotkey_error = (!failed.is_empty()).then_some(failed);
        modular_clipboard_app::daemon::sync_mini_items(&app.svc, &mini_items);
        modular_clipboard_app::daemon::apply_mini_panel(&app.svc.state.config, &mini_items);
    }

    // ---- 启动期窗口开关 ------------------------------------------------
    //
    // 这两个配置项早前只被**写入**、从不被读取：用户在设置里改了、
    // 也落盘了，重启后毫无效果。
    //
    // IMPORTANT: 必须放在托盘创建**之后**：`start_minimized` 的语义是
    // 「启动后隐藏到托盘」，而若托盘没起来还把窗口藏掉，程序就成了
    // 「没有窗口、没有托盘」的僵尸——用户只看到进程在，却什么都
    // 点不到。宁可退回「正常显示」。
    if always_on_top {
        presence::set_topmost(window.hwnd(), true);
    }
    if start_minimized {
        if resident.is_some() {
            // 不是 `SW_MINIMIZE`：那只是「最小化」，窗口仍在任务栏留
            // 条目、也能被 Alt+Tab 切回，与「隐藏到托盘」不符。
            // 直接 `SW_HIDE`，托盘是唯一入口。
            presence::hide_window(window.hwnd());
            tracing::info!("配置 start_minimized：启动后隐藏到托盘");
        } else {
            tracing::warn!("配置要求启动时隐藏，但托盘不可用——改为正常显示，否则程序无法唤回");
        }
    }

    // ---- 显示窗口（PF-3）------------------------------------------------
    //
    // 刻意放在全部初始化**之后**：Vulkan 设备、管线、共享渲染资源、
    // DB 打开、35MB 字体读入、托盘启动全排在前面。早前窗口在
    // `Window::new` 之后立即 `focus_window`，于是初始化期间（冷启动
    // 可达数百 ms～秒级）窗口已可见但主循环尚未开始——零消息泵，
    // 拖动/关闭全部无响应，用户看到的是一扇「卡死」的窗壳，
    // Windows 还可能给它挂上假死转圈。初始化中途失败时更糟：
    // 窗口闪一下才退出。
    //
    // 用 `presence::focus_window` 而非裸 ShowWindow：它已处理
    // Windows 的前台窗口限制（`SetForegroundWindow` 失败时
    // 走 `AttachThreadInput`），托盘点击唤起也需要它。
    //
    // `start_minimized` 已在上面处理（隐藏到托盘），这里只负责显示。
    presence::focus_window(window.hwnd());

    // ---- 帧循环 --------------------------------------------------------
    let mut frame_no: u64 = 0;
    let mut quit = false;
    // 上一帧 egui 给出的重绘延时（`None` = 明确表示空闲，见
    // `renderer::repaint_delay`）。渲染闸门（PF-4）用它判断「egui 是否
    // 还在要求持续重绘」——光标闪烁、悬停动画都会让它非 `None`。
    let mut last_delay: Option<std::time::Duration> = None;
    // 已应用到 Win32 的置顶状态。用来每帧比对配置，避免重复调
    // `SetWindowPos`（那会触发一次窗口样式变更）。
    let mut applied_topmost = always_on_top;
    // 已应用的热键组合与迷你面板掩码（单进程模式）。
    // 与 `applied_topmost` 同一模式：配置变了才重注册 / 重装钩子。
    let mut applied_toggle = toggle_spec;
    let mut applied_paste = paste_spec;
    let mut applied_mini =
        modular_clipboard_platform::tray::mini_mask(&app.svc.state.config.mini_panel);
    // 已应用到 Win32 的背景效果（磨砂/半透明）。
    // 哨兵值 `None`：首帧必然与配置比对并应用一次。
    // 透明度不再进 Win32（整窗 LWA_ALPHA 已被逐像素半透明取代），
    // 它只影响渲染层的调色板变体——那部分每帧直接读配置。
    let mut applied_backdrop = modular_clipboard_core::config::WindowBackdrop::None;
    // 卡片子窗口集合（详情/视图等被拖出主窗的卡片）。
    let mut children = crate::childwin::ChildWindows::new();
    // 悬浮弹窗（搜索面板 / 类型过滤栏）：懒创建、隐藏时保留
    // （类型栏常驻可见；搜索面板收起频繁，销毁重建有可感延迟）。
    let mut popups = crate::popup::Popups::default();
    // 设置窗口（懒创建）。
    //
    // NOTE: **懒创建**而不是建时就创建：设置窗口只在用户点齿轮时才需要，
    // 每个窗口都有独立的交换链 + 字体图集（显存开销实测量级不小）。
    // 程序启动就建一个永不显示的窗口是纯浪费。
    //
    // 失败也不致命——设置打不开不该让主程序起不来（降级为无设置）。
    let mut settings_win: Option<crate::settingswin::SettingsWindow<'_>> = None;
    // 图片编辑窗口（懒创建，2026-10-21）：与设置窗口同款「即关即毁」。
    // `paint_state.image_editor` 决定它该不该存在，原图字节在
    // `paint_state.image_editor_src`（由 `Op::OpenImageEditor` 备好）。
    let mut img_edit: Option<crate::imgedit::ImageEditor<'_>> = None;
    // 窗口隐藏到托盘后仍在跑帧循环（要继续接收托盘事件），
    // 但不再渲染。`GetClientRect` 对隐藏窗口仍返回原尺寸，
    // 靠尺寸判断不出隐藏态，必须显式记这个标志。
    //
    // ⚠️ 初值必须反映**启动时的真实可见性**。
    //
    // 早前这里恒为 `false`，而上面 `start_minimized` 分支已经把窗口
    // `SW_HIDE` 了 ⇒ 主循环对一个**不可见**的窗口跑完整渲染
    // （run_ui + tessellate + 一次完整的 acquire/paint/present），
    // CPU 与 GPU 全部白烧；且 `hidden` 与 Win32 真实可见性长期不一致，
    // 后续任何依赖它的判断都会错。
    let mut hidden = start_minimized && resident.is_some();
    // 窗口是否处于最小化。由 `WindowEvent::Minimized` 维护。
    //
    // 最小化时客户区变成 0x0，第3 步的尺寸检查会跳过渲染；但那个检查
    // **也**会在「窗口刚好被 resize 到 0」时命中，两者需要不同的日志
    // 措辞，因此单独记一个标志。只用于日志与拖动区，不参与控制流。
    let mut minimized = false;
    // 上一帧记进日志的拖动区。只在**变化时**打日志——每帧都打会把
    // debug 日志刷成流水账，而拖动区平时是稳定的。
    // 拖动区段数（仅用于日志去重）。
    let mut last_drag_count = usize::MAX;
    // 上次把主窗口位置落盘的时刻（实时记忆用防抖）。
    //
    // 位置只在退出时保存的话，强杀/崩溃（任务管理器、断电）会丢掉
    // 用户最后的拖动结果。这里每帧比对，变化超过阈值后防抖落盘——
    // 拖动停止后最多 500ms 内写入，崩溃最多丢最后一次拖动。
    let mut last_pos_save: Option<std::time::Instant> = None;

    while !quit {
        // ---- 0. 退出标志（必须在**所有** continue 之前）----------
        //
        // NOTE: 早前这个判断在循环末尾（第 4 步之后），而本循环有三条
        // `continue`（隐藏态 / 客户区 0×0 / `acquire` 过期）会绕开它。
        //
        // `Op::Quit` 由**设置窗口**产生（「退出程序」按钮）。设置窗口是
        // 独立顶层窗口，主窗口隐藏后它仍在 `drive_attach_windows` 里被
        // 继续驱动、仍可点。于是完整故障链是：
        //
        //   打开设置 → 主窗口点 ✕ 隐藏到托盘 → 设置窗口仍开着
        //   → 点「退出程序」→ `should_quit = true`
        //   → 主循环永远在 `hidden` 分支 continue
        //   → **程序退不掉**，只能任务管理器强杀。
        //
        // 这也正是「藏起来唤不回来」家族里最实的一条。
        //
        // 放在循环开头后，最迟下一帧就会生效（延迟一帧无感）。
        // 常驻进程失联（被任务管理器杀掉、崩溃）：界面自己退出，
        // 而不是留一扇永远不更新的窗口让用户以为程序还活着。
        if app.svc.daemon_gone() {
            tracing::warn!("与常驻进程的连接已断开，界面退出");
            break;
        }

        if app.should_quit {
            // 设置里的「退出程序」是**整个程序**退出，不只是关掉界面：
            // 远端模式下还要让常驻进程一起收工，否则用户点了退出，
            // 托盘图标与后台记录都还在（他会以为程序没退干净）。
            //
            // 「关闭窗口 ✕」不走这里——那条路径由 `decide_on_close`
            // 处理，远端模式下只退出界面进程，常驻进程继续干活。
            if app.svc.is_remote()
                && let Err(e) = app.svc.request_quit()
            {
                tracing::warn!(%e, "通知常驻进程退出失败");
            }
            break;
        }

        // ---- 0.5 窗口可见性对账 ----
        //
        // `hidden` 是本进程的标志，但**外部也能改可见性**：常驻进程的
        // 热键 toggle 会直接 `ShowWindow(SW_HIDE)` 收起本窗口，
        // `request_show_ui`（托盘/重复双击）又会直接 `ShowWindow(SW_SHOW)`
        // 把它显示出来。两边不同步的后果：
        // - 被外部隐藏后标志仍为 false ⇒ 继续渲染一个不可见的窗口，
        //   白烧 GPU（正是本循环 `hidden` 分支要避免的）；
        // - 被外部显示后标志仍为 true ⇒ 窗口可见却停在旧帧不渲染，
        //   用户看到一扇「卡住」的窗。
        // 每帧以 Win32 的真实状态为准。
        let visible_now = unsafe { IsWindowVisible(window.hwnd()) }.as_bool();
        if hidden && visible_now {
            // 被外部（热键 / 托盘）显示：恢复正常渲染。
            // 尺寸可能已变（最大化/还原切换），与托盘唤起同理立刻同步，
            // 避免用旧 framebuffer present 一帧。
            hidden = false;
            rebuild(&mut fr, &mut painter, window.inner_size_physical())?;
            ctx.request_repaint();
        } else if !hidden && !visible_now {
            // 被外部隐藏：转入隐藏态（不渲染，只轮询等下一次唤起）。
            hidden = true;
        }

        // ---- 1. 事件 ----
        //
        // `saw_close` 与 `wm_quit` 必须分开记：`gfx::window` 把
        // `WM_CLOSE` 与 `WM_QUIT` 都翻译成同一个 `CloseRequested`，
        // 而两者处置完全相反——前者隐藏到托盘，后者退出。
        // 区分依据是 `quit_requested()`，它**只在** `WM_QUIT` 时置位。
        let mut saw_close = false;
        // 另一个进程请求唤起主窗口（单实例：用户又双击了一次图标）。
        let mut saw_show = false;
        // 2026-10-17：尺寸变更只打标记，不在事件循环里逐条重建交换链。
        // 拖动窗口时 WM_SIZE 每几毫秒来一次，同一轮循环里可能积压多条
        // → 逐条 `rebuild` 每次都 `device_wait_idle` + 销毁重建全部帧
        // 资源（实测拖动一次十几次重建）。标记在事件循环结束后统一消费
        // （见 ---- 1.4 ----）。
        let mut resize_pending = false;
        // `take_pending` 而非 `poll()` 的返回值：节流路径 `poll_for` 也会读
        // 消息并把事件记进累积缓冲，把它们一起取走才能保证关闭请求不丢。
        // 见 `gfx::window::EventLoop::poll` 的说明。
        //
        // 取出的事件**必须**留到 `egui_input` 一起用（见第 2 步）。
        // 只喂应用层而不喂 egui 的话，UI 会收不到任何鼠标/键盘输入。
        let frame_events = events.take_pending();
        for ev in &frame_events {
            match ev {
                WindowEvent::CloseRequested => {
                    saw_close = true;
                    tracing::info!(
                        wm_quit = events.quit_requested(),
                        tray = resident.is_some(),
                        "收到关闭请求"
                    );
                }
                WindowEvent::ShowRequested => {
                    // 单实例唤起：另一个进程（用户重复双击）请求显示本窗口。
                    // 与托盘「显示」走同一条路径——见下面的 `f.show` 分支。
                    saw_show = true;
                    tracing::info!("收到唤起请求（重复启动）");
                }
                WindowEvent::HideRequested => {
                    // 热键 toggle 的收起方向：常驻进程不直接改窗口状态
                    //（那会跨进程同步等待、与 IPC 同步读构成死锁），而是
                    // 投递 WM_HIDE_MAIN，由本进程在自己的消息循环里隐藏。
                    // 同线程 ShowWindow 立即返回，不跨进程。
                    presence::hide_window(window.hwnd());
                    hidden = true;
                    // 主窗口隐藏时连带收起设置窗，避免它孤立留在屏幕上
                    //（设置窗是本进程的附属顶层窗口，主窗没了它还漂着）。
                    // 只改状态：本帧 hidden 分支仍会 drive_attach_windows，
                    // 那里检测到 show_settings=false 即销毁设置窗。
                    app.paint_state.show_settings = false;
                    tracing::info!("收到隐藏请求（热键 toggle），已隐藏主窗口并收起设置窗");
                }
                WindowEvent::Resized { .. } => {
                    // 客户区尺寸变了 → 交换链必须重建。但**不在这里重建**：
                    // 打标记，等本轮事件全部处理完后统一做一次（逻辑见
                    // 标记声明处与 ---- 1.4 ----）。
                    resize_pending = true;
                }
                WindowEvent::Focused(gained) => {
                    // ⚠️ 这里**必须接** `hide_on_focus_lost`。
                    //
                    // 它是一个「只写不读」的死开关：全仓只有定义、
                    // 默认值、设置面板渲染、`set_bool_field` 写入四处，
                    // 帧循环对 `Focused` 事件**没有任何分支** ——
                    // 用户开关它、落盘了、重启了，什么都不会发生。
                    //
                    // ⚠️ 隐藏**必须有唤回路径**兜底，与 `decide_on_close`
                    // 的保护一致：没有任何入口能把窗口叫回来时，隐藏
                    // 等于程序消失，用户只能任务管理器强杀。
                    //
                    // ⚠️ 判据**不能**只写 `resident.is_some()`：常驻进程
                    // 模式下界面进程自己不建托盘（托盘归 daemon），那个
                    // 判断恒为 false——开关在默认的双进程启动下永远失效
                    // （2026-10-13 用户实测）。托盘在**本进程**（单进程
                    // 模式）或在**常驻进程**（远端模式）都算有兜底。
                    let gained = *gained;
                    // 设置窗口开着时不躲：它是本进程的附属顶层窗口，
                    // 用户点过去同样让主窗口失焦——那种「失焦」不是切到
                    // 别的程序。躲起来只会剩一扇设置窗漂在桌上，
                    // 像主程序已经没了。
                    //
                    // 再加一道进程级确认：新前台窗口仍是本进程的
                    //（设置窗之外的托盘右键菜单、迷你面板等），同样不躲。
                    if !gained
                        && app.svc.state.config.ui.hide_on_focus_lost
                        && (resident.is_some() || remote)
                        && !hidden
                        && settings_win.is_none()
                        && !presence::foreground_is_own_process()
                    {
                        presence::hide_window(window.hwnd());
                        hidden = true;
                        tracing::info!("失焦且已开启「失焦时隐藏」，已隐藏到托盘");
                    }
                }
                WindowEvent::Minimized(min) => {
                    // `min` 是 `&bool`：事件是以 `&WindowEvent` 形式遍历的。
                    let min = *min;
                    // 无边框窗口没有系统标题栏的缩略/动画反馈，
                    // 最小化后界面**毫无变化**——用户容易以为程序没响应。
                    // 这里显式感知：记日志，并请求重绘让状态栏能刷新。
                    tracing::info!(minimized = min, "窗口最小化状态变化");
                    if !min {
                        // 恢复时客户区尺寸可能变了（最大化/还原切换），
                        // 与托盘唤起同理，立刻同步一次避免用旧尺寸 present。
                        rebuild(&mut fr, &mut painter, window.inner_size_physical())?;
                        ctx.request_repaint();
                    }
                    minimized = min;
                }
                _ => {}
            }
        }

        // 刻意**不**对 `quit_requested()` 提前 break：`WM_QUIT` 的退出
        // 语义由下面的 `decide_on_close` 统一处理，提前 break 会让
        // 那条分支永远走不到，只存在于单测里。

        // ---- 1.4 尺寸变更合帧重建 ----
        //
        // 2026-10-17：拖动时一轮循环可能收到多条 Resized（WM_SIZE 高频）。
        // 全部合并成**一次**重建：以最新查询到的客户区尺寸为准，与
        // `fr.extent()` 相同则跳过（不带真实变化的重复 WM_SIZE 很常见）。
        // 放在 `run_ui` 之前，让本帧布局就用上新尺寸——否则会用旧
        // screen_rect 排一帧，present 时又被交换链裁掉。
        //
        // ---- 3 ---- 的兜底检查保留：它兜「事件与查询不一致」的残余
        // 情况（如 SetWindowPos 合并消息），两者互补不重复。
        if resize_pending {
            let (cw, ch) = window.inner_size_physical();
            if cw > 0 && ch > 0 && (cw, ch) != (fr.extent().width, fr.extent().height) {
                rebuild(&mut fr, &mut painter, (cw, ch))?;
            }
        }

        // ---- 1.5 托盘与全局快捷键 ----
        //
        // 放在窗口事件之后、业务 UI 之前：唤起动作要在本帧绘制出内容，
        // 否则用户会看到「窗口弹出来了但还是空的，下一帧才有东西」。
        if let Some(r) = resident.as_ref() {
            let f = r.poll();
            //
            // 下面三处失败一律 `notify`（toast 通道）而不只写日志。
            //
            // 理由：这三项都是**用户刚点的托盘动作**，而托盘交互发生
            // 在**主窗口之外**——失败时主窗口甚至可能收着。日志只在
            // `%APPDATA%/…/app.log` 里，普通用户永远不会去看，于是
            // 「点了没反应」与「点了失败了」在用户眼里完全一样。
            //
            // 与 `Op::ClearAll` / `Op::Copy` 等**界面内**路径同款
            // 处置（它们也都 notify），保持失败反馈的一致性。
            if f.clear_history {
                if let Err(e) = app.clear_history() {
                    tracing::warn!(%e, "托盘清空历史失败");
                    app.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("清空失败:", "Clear failed:"))));
                }
                // IMPORTANT: 必须请求重绘。
                //
                // `clear_all` 内部已 `reload_list()`，数据层是新的；
                // 但窗口可能正处于「空闲」状态（鼠标不动、egui 也没
                // 别的理由重绘），于是**画面仍显示旧列表**，直到用户
                // 碰一下鼠标才刷新 —— 表现为「清了但没清」。
                //
                // 下一行的 `f.show` 分支本来就有 `request_repaint()`，
                // 这里补上的是「只清空、不唤起」的那条路径。
                ctx.request_repaint();
            }
            // 「粘贴上一条」：不唤起界面，直接把最新一条写回剪贴板
            // 并发 Ctrl+V 给前台窗口（单进程模式走这里，
            // 常驻进程模式下由 `daemon` 自己处理）。
            if f.paste_previous {
                if let Some(id) = app.svc.state.items.first().map(|i| i.id) {
                    if let Err(e) = app.svc.copy_and_paste(id) {
                        tracing::warn!(%e, "粘贴上一条失败");
                        app.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("粘贴失败:", "Paste failed:"))));
                    }
                } else {
                    tracing::debug!("历史为空，忽略「粘贴上一条」");
                }
            }
            // 迷你面板里点了某条：写回剪贴板。
            // 不自动粘贴——用户按住修饰键点的右键，粘贴到哪由他自己决定。
            if let Some(id) = f.mini_pick {
                if let Err(e) = app.svc.copy_item(id) {
                    tracing::warn!(%e, "迷你面板复制条目失败");
                    app.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("复制失败:", "Copy failed:"))));
                }
                // 复制可能改变列表顺序（最近使用），请求重绘。
                ctx.request_repaint();
            }
            if f.show {
                presence::focus_window(window.hwnd());
                hidden = false;
                // 窗口刚恢复，尺寸可能已变（最大化/还原切换）。
                // 立刻同步一次，避免用旧尺寸 present 一帧。
                rebuild(&mut fr, &mut painter, window.inner_size_physical())?;
                ctx.request_repaint();
            }
            if f.quit {
                tracing::info!("收到托盘退出请求");
                quit = true;
            }
        }

        // 单实例唤起：另一个进程（用户重复双击 exe）请求显示本窗口。
        //
        // IMPORTANT: 这段**不能**并进上面的托盘分支。托盘可能被用户关掉
        // （设置里的「显示托盘图标」），而「重复启动时把窗口调出来」与
        // 托盘无关——关掉托盘的用户照样会双击图标。
        //
        // 走与托盘「显示」完全相同的路径（`focus_window` 内部会
        // `ShowWindow` + 抢前台），否则 `hidden` 标志不同步，窗口会显示
        // 出来但**不渲染**（一片空白）。
        if saw_show {
            presence::focus_window(window.hwnd());
            hidden = false;
            rebuild(&mut fr, &mut painter, window.inner_size_physical())?;
            ctx.request_repaint();
        }

        // 关闭请求：托盘可用时隐藏到托盘，否则退出。
        //
        // 这里**不能**无条件 break——`WM_CLOSE` 不销毁窗口，
        // 隐藏后循环要继续跑，否则托盘事件再也没人收。
        //
        // ⚠️ 「托盘可用」的判据**不能**只写 `resident.is_some()`：
        // 常驻进程模式下界面进程自己不建托盘（托盘归 daemon），那个
        // 判断恒为 false ⇒ 远端模式点 ✕ 会走 Quit 分支，而 Quit 的
        // 上半段还会 `request_quit()` 把常驻进程一起退掉——**点一下
        // 关闭整个程序消失，托盘白建**（2026-10-13 探针实测：
        // WM_CLOSE 后窗口销毁 + daemon 退出）。与 `should_quit` 注释里
        // 「远端模式下只退出界面进程，常驻进程继续干活」的既定语义矛盾。
        //
        // 正确判据 = 「程序有唤回入口」：托盘在本进程（单进程模式）
        // **或** 在常驻进程里（远端模式）。
        if saw_close {
            let tray_backed = resident.is_some() || remote;
            match presence::decide_on_close(tray_backed, events.quit_requested()) {
                CloseDecision::HideToTray => {
                    presence::hide_window(window.hwnd());
                    hidden = true;
                    tracing::info!("已隐藏到托盘，后台继续监听剪贴板");
                }
                CloseDecision::Quit => {
                    tracing::info!("无托盘兜底，关闭即退出");
                    quit = true;
                }
            }
        }
        if quit {
            break;
        }

        // ---- 1.7 后台捕获消费 ----
        //
        // 从 `draw_frame` 提到这里（PF-4）：下面的渲染闸门要能跳过
        // `run_ui`，但捕获入库必须**每轮都跑**——隐藏/空闲时它才是
        // 「后台持续记录剪贴板」的本体，跳过等于停止记录。
        //
        // 有变化时安排一次延时重绘：连续多条捕获（批量复制文件）只画
        // 一帧，不为每条捕获都全量重绘。
        if app.svc.pump() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        // ---- 1.8 卡片子窗口 ----
        //
        // 让子窗口集合与工作区的 `CardHost` 保持一致：
        // 标了 `Window` 但还没建窗的卡片 → 建；被收回的 → 销毁。
        //
        // 放在业务 UI **之前**是必须的：本帧的绘制才会把新窗口
        // 的卡片内容算进去。反过来的话新窗口会晚一帧才有内容。
        // 传主窗口的位置与尺寸：置顶窗口要摆在**主窗口左边**。
        let main_pos = window.screen_position_points();
        let (mw, mh) = window.inner_size_points();
        let main_rect = Some((
            main_pos,
            egui::epaint::emath::vec2(mw, mh),
        ));
        children.sync_with(&mut app.ws, &shared, window.scale_factor(), main_rect);

        // ---- 1.9 渲染闸门（PF-4）----
        //
        // 空闲时整套 `run_ui + tessellate + acquire/paint/present` 产出
        // 与上一帧完全相同的画面，纯浪费：静态界面以 ~120Hz 重绘，
        // 单核持续占用、笔记本耗电发热（磨砂窗口还多付一次 30fps 捕获）。
        //
        // 打开闸门（必须渲染）的条件，任一成立即渲染：
        //   1. 本帧有窗口事件——鼠标/键盘/resize/焦点（`take_pending` 非空）；
        //   2. egui 有未消费的重绘请求——光标闪烁、悬停动画、以及上面
        //      `request_repaint_after(250ms)` 的延时重绘（没有这项，
        //      延时重绘永远等不到一帧，列表会停在旧内容）；
        //   3. 上一帧 egui 自己要求继续重绘（`delay.is_some()`）；
        //   4. 置顶/背景效果配置与已应用值不一致——设置窗口改的配置
        //      走的是另一个 egui Context，不产生本窗事件，不比对就会
        //      「改了但主窗口不应用」；
        //   5. **有补间在飞**（`anim::busy`）——主题切换的颜色渐变、
        //      hover 淡入淡出都靠它推进；漏掉这项会让动画只在事件
        //      驱动的帧上走，鼠标不动时卡在半路。
        let skip_render = frame_events.is_empty()
            && !ctx.has_requested_repaint()
            && last_delay.is_none()
            && !crate::anim::busy(&ctx)
            && app.svc.state.config.ui.always_on_top == applied_topmost
            && app.svc.state.config.ui.window_backdrop == applied_backdrop;
        if skip_render {
            // 睡固定间隔：没有要画的内容，8ms 轮询足以让输入延迟感知不到。
            events.poll_for(Some(std::time::Duration::from_millis(8)));
            // 附属窗口**照样驱动**：它们仍留在屏幕上，不渲染就会被 Win32
            // 标「无响应」——与隐藏态分支同理。
            let topmost = app.svc.state.config.ui.always_on_top;
            drive_attach_windows(
                &mut app,
                &mut children,
                &mut settings_win,
                &mut popups,
                &mut img_edit,
                &shared,
                &window,
                frame_no,
                topmost,
                !hidden && !window.is_minimized(),
            );
            frame_no += 1;
            continue;
        }

        // ---- 2. 业务 UI ----
        //
        // **隐藏时也要跑**：后台捕获的消费（`svc.pump`，见 1.7）已经把
        // 内容写进数据库，但列表的刷新要靠这一帧的绘制。隐藏态的循环
        // 保持（见 2.5），数据不丢。
        //
        // `frame_events` 是上面 `take_pending()` 取出的本帧事件，
        // 事件**只有这一份**：既喂应用层（第 1 步的关闭/resize 判断），
        // 也喂 egui。漏掉后者会让 UI 收不到任何鼠标/键盘输入。
        let raw_input = events.egui_input(&ctx, &frame_events);
        // 诊断用：`run_ui` 按值消费 `raw_input`，之后拿不到 `screen_rect`。
        let diag_screen_rect = if env_flag("MC_DIAG") {
            raw_input.screen_rect
        } else {
            None
        };
        // 背景亮度先于绘制：本帧的背景层实度要用它算（见 `backdrop_alpha`）。
        // 取的是**上一帧捕获**的值，一帧延迟对这种粗粒度决策无影响。
        app.backdrop_lum = painter.backdrop_luminance();
        let mut output = ctx.run_ui(raw_input, |ui| {
            app.draw_frame(ui);
        });
        let ppp = output.pixels_per_point;

        // ---- 2.1 消费 PlatformOutput ----------------------------------
        //
        // NOTE: 早前 `run_ui` 的返回值只取了 `shapes` / `textures_delta` /
        // `ppp`，`platform_output` 整个被丢弃 —— 于是**光标永远是默认
        // 箭头**，在搜索框上、在可点条目上毫无区别，「这里能点」没有
        // 任何视觉提示。
        presence::set_cursor_icon(window.hwnd(), output.platform_output.cursor_icon);

        // NOTE: `OutputCommand::CopyText`（egui 里 Ctrl+C 复制选中文本）
        // **故意不接**，说明如下。
        //
        // 接线本身很简单（`ClipboardReader::write_text` 已在
        // `Service::copy_item` 里用着），但**本程序会自己监听剪贴板**：
        // 写进去的内容会触发自己的捕获钩子，于是「在搜索框里选中几个字
        // 按 Ctrl+C」会**多出一条历史记录**。
        //
        // capture 层目前**没有自写防护**——既没有「忽略自己刚写的内容」
        // 的时间窗，也没有内容比对。要接就必须先补那层防护，否则回环。
        //
        // 宁可功能缺失，也不要让用户每按一次 Ctrl+C 就多一条垃圾记录。
        for cmd in &output.platform_output.commands {
            if let egui::OutputCommand::CopyText(t) = cmd {
                tracing::debug!(len = t.len(), "收到 CopyText，未接线：需先给 capture 层加自写防护");
            }
        }
        // 在移动 output.shapes 之前先把重绘延时取出来。
        let delay = crate::renderer::repaint_delay(&output);
        last_delay = delay;

        // ---- 2.2 窗口外壳接线 ----
        //
        // 必须在 `run_ui` **之后**：拖动区是 `view::draw` 根据本帧实际
        // 布局算出来的，`run_ui` 之前拿到的必然是上一帧的值。
        //
        // **必须每帧调**（`set_drag_region` 内部只写 4 个原子量，无系统
        // 调用，代价可忽略）：只在启动时设一次的话，窗口 resize 或
        // 模块折叠/浮动之后矩形就对不上了，表现为「刚启动能拖，
        // 拖一会儿就拖不动了」。
        let drag = app.drag_regions();
        window.set_drag_region_multi(drag);
        if drag.len() != last_drag_count {
            tracing::debug!(段数 = drag.len(), "拖动区已更新");
            last_drag_count = drag.len();
        }

        // ---- 2.3 标题栏窗口按钮 ----
        //
        // 关闭按钮**复用** `presence::hide_window` + `hidden`，与上面
        // `CloseDecision::HideToTray` 分支走同一条路径：
        // 剪贴板类工具直接退出会让用户以为「记录停了」。
        // 这里刻意**不**改 `decide_on_close`——四条关闭路径已由
        // `scripts/verify_close_paths.ps1` 验证过，不该为接UI 按钮重开。
        if app.take_close_requested() {
            // 判据必须与 WM_CLOSE 路径一致为 `resident.is_some() || remote`：
            // 远端（双进程）模式下本进程不建托盘（resident 恒 None），
            // 只写 resident.is_some() 会误判「无托盘兜底」→ 直接退出整个
            // 界面进程（2026-10-13 日志曾出现「无托盘兜底，直接退出」）。
            match presence::decide_on_close(resident.is_some() || remote, false) {
                CloseDecision::HideToTray => {
                    presence::hide_window(window.hwnd());
                    hidden = true;
                    tracing::info!("标题栏关闭按钮：已隐藏到托盘");
                }
                CloseDecision::Quit => {
                    tracing::info!("标题栏关闭按钮：无托盘兜底，直接退出");
                    quit = true;
                }
            }
        }
        // 注：原先这里还有「最小化」按钮的处理。它已从顶栏去掉——
        // 与「关闭到托盘」的实际效果几乎相同（都让窗口消失），
        // 区别仅在任务栏是否留条目，而本程序常驻托盘，用户极少需要。
        if quit {
            break;
        }

        // ---- 2.4 布局变化后自适应窗口宽度 ----
        //
        // 置顶模式在设置里切换时栏数会变（共用单栏 ↔ 独立分栏），
        // 而窗口尺寸只在启动时从配置读过一次。两种请求语义：
        // - 切到独立分栏：放不下就放宽，否则卡片被 `solve_scaled`
        //   按比例压缩；**只放大不收缩**（够宽就不动，用户手动拉宽过
        //   的窗口不会被改小）；
        // - 切回共用单栏：精确恢复默认宽度（切出去时窗口被自动撑宽
        //   并写回了配置，不恢复的话回不到默认）。
        {
            let (cw, ch) = window.inner_size_physical();
            let scale = window.scale_factor().max(0.01);
            if let Some(w_px) = app.take_resize_width_to((cw, ch), scale) {
                window.set_size_physical(w_px, ch);
                // 同步回配置，否则下次启动又回到旧尺寸 ——
                // 用户看到「切了模式窗口变宽、重启又变回去」。
                app.svc.state.config.ui.window_width = w_px as f32;
                tracing::info!(width_px = w_px, "置顶模式切换，已调整主窗口宽度");
            }
        }

        // ---- 2.5 隐藏态：不渲染，只保持循环 ----
        //
        // 窗口隐藏时 `GetClientRect` **仍返回原尺寸**（非 0），
        // 所以下面第3 步的尺寸检查抓不到隐藏态——必须单独判。
        //
        // 不跳过渲染的代价是实打实的：present 不产生任何可见输出，
        // 却仍在跑完整的 tessellate + queue_submit + device_wait_idle，
        // 等于纯烧 CPU 与带宽。
        if hidden {
            // 增量由守卫在离开作用域时清空。
            let _guard = DeltaGuard(std::mem::take(&mut output.textures_delta));
            drop(output);
            // 睡固定间隔而非用 egui 的 delay：隐藏时没有下一次绘制，
            // egui 也不会请求重绘，用它的 delay 会退化成 0 延时忙等。
            events.poll_for(Some(presence::HIDDEN_POLL_INTERVAL));
            // 附属窗口**照样驱动**：主窗口藏起来不代表附属窗口该冻住
            // —— 它们仍留在屏幕上，不渲染就会被 Win32 标「无响应」。
            let topmost = app.svc.state.config.ui.always_on_top;
            drive_attach_windows(
                &mut app,
                &mut children,
                &mut settings_win,
                &mut popups,
                &mut img_edit,
                &shared,
                &window,
                frame_no,
                topmost,
                !hidden && !window.is_minimized(),
            );
            continue;
        }

        // ---- 3. 尺寸兜底检查 ----
        // 客户区可能被最小化到 0×0，此时交换链拿不到可呈现的图像。
        let (cw, ch) = window.inner_size_physical();
        if cw == 0 || ch == 0 {
            // IMPORTANT: 这里必须用 `events.poll_for` 而不是 `thread::sleep`。
            //
            // `sleep` 不处理消息，于是**最小化期间队列里的消息全部积压**：
            //   - `WM_SIZE(SIZE_MINIMIZED)` 读不到 → 上层收不到
            //     `WindowEvent::Minimized`，无边框窗口又没有任何视觉反馈，
            //     用户完全无从判断程序还在跑；
            //   - `WM_SYSCOMMAND(SC_RESTORE)` 也读不到 → 窗口**恢复不了**。
            //     实测：`PostMessage(SC_RESTORE)` 后 `IsIconic` 恒为 TRUE、
            //     客户区恒为 0x0；改用 `ShowWindow(SW_RESTORE)` 才恢复得动
            //     （它走 SendMessage 语义，绕过队列）。
            //     结论：这条路径必须继续泵消息，节流交给 `poll_for`。
            //
            // 0×0 有两种成因（最小化 / 被 resize 到 0），日志要能区分，
            // 否则用户报「窗口没了」时无从判断是哪种。
            if minimized {
                tracing::debug!("窗口已最小化，跳过渲染");
            } else {
                tracing::warn!("客户区尺寸为 {cw}x{ch}，跳过渲染");
            }
            // 增量由守卫在离开作用域时清空。
            let _guard = DeltaGuard(std::mem::take(&mut output.textures_delta));
            drop(output);
            events.poll_for(Some(std::time::Duration::from_millis(50)));
            // 附属窗口**照样驱动**：主窗口藏起来不代表附属窗口该冻住
            // —— 它们仍留在屏幕上，不渲染就会被 Win32 标「无响应」。
            let topmost = app.svc.state.config.ui.always_on_top;
            drive_attach_windows(
                &mut app,
                &mut children,
                &mut settings_win,
                &mut popups,
                &mut img_edit,
                &shared,
                &window,
                frame_no,
                topmost,
                !hidden && !window.is_minimized(),
            );
            continue;
        }
        if (cw, ch) != (fr.extent().width, fr.extent().height) {
            // Resized 事件已处理过；这里兜住「尺寸查询与事件不一致」的情况
            // （例如 SetWindowPos 引起的多消息合并）。
            rebuild(&mut fr, &mut painter, (cw, ch))?;
        }

        // ---- 4. 帧渲染 ----
        //
        // 把这一帧要用的东西全部从 `output` 里取出来，之后让它整体 drop。
        // `tessellate` 按值消耗 shapes，因此要先 `mem::take` 才能整体 drop。
        let mut textures_delta = DeltaGuard(std::mem::take(&mut output.textures_delta));
        let primitives = ctx.tessellate(std::mem::take(&mut output.shapes), ppp);
        drop(output);

        let Some(acquired) = fr.acquire()? else {            rebuild(&mut fr, &mut painter, (cw, ch))?;
            tracing::debug!("acquire 返回过期，已重建交换链");
            // 附属窗口**照样驱动**：主窗口藏起来不代表附属窗口该冻住
            // —— 它们仍留在屏幕上，不渲染就会被 Win32 标「无响应」。
            let topmost = app.svc.state.config.ui.always_on_top;
            drive_attach_windows(
                &mut app,
                &mut children,
                &mut settings_win,
                &mut popups,
                &mut img_edit,
                &shared,
                &window,
                frame_no,
                topmost,
                !hidden && !window.is_minimized(),
            );
            continue;
        };

        // 磨砂（自研 Acrylic）走 backdrop 路径：先铺「身后桌面的实时
        // 模糊」底图再叠加界面。半透明/无走普通路径——半透明的透出
        // 由交换链像素 alpha 完成，不需要捕获。
        if app.svc.state.config.ui.window_backdrop
            == modular_clipboard_core::config::WindowBackdrop::Acrylic
        {
            painter.paint_backdrop(
                &mut fr,
                window.hwnd().0 as isize,
                modular_clipboard_gfx::blur::radius_for_strength(
                    app.svc.state.config.ui.window_blur_strength,
                ),
                &primitives,
                ppp,
                window.scale_factor(),
                &mut textures_delta.0,
            )?;
        } else {
            painter.paint(
                &mut fr,
                &primitives,
                ppp,
                window.scale_factor(),
                &mut textures_delta.0,
            )?;
        }

        if fr.present(acquired)? == PresentResult::Outdated {
            // 呈现时才发现过期：下一轮 acquire 会返回 None 再重建。
            // 这里不立刻重建，避免连续两次 device_wait_idle。
            tracing::debug!("present 报告过期，下一轮重建");
        }

        // ---- 4.4 窗口置顶状态同步 ----
        //
        // `always_on_top` 能在设置里随时改，而 Win32 的置顶是**窗口属性**，
        // 不会因为配置变了自己变。每帧比对一次，改了就调 `SetWindowPos`。
        //
        // 用 `applied_topmost` 记住已应用的值，避免每帧都调
        // `SetWindowPos`（那会触发一次窗口样式变更）。
        if app.svc.state.config.ui.always_on_top != applied_topmost {
            applied_topmost = app.svc.state.config.ui.always_on_top;
            presence::set_topmost(window.hwnd(), applied_topmost);
        }

        // ---- 4.4b 背景效果同步（自研磨砂 / 半透明）----
        //
        // 与置顶同一模式：配置变了才调 Win32（捕获排除 + 样式归位）。
        // 同时同步渲染层：磨砂开 → 启用桌面捕获 + 模糊资源；关 → 释放。
        // 清屏色也要跟上：半透明模式的背景层带 alpha，若清屏 alpha=1，
        // 未被背景层覆盖的像素会变成不透明（磨砂/半透明失效）。
        let backdrop = app.svc.state.config.ui.window_backdrop;
        if backdrop != applied_backdrop {
            applied_backdrop = backdrop;
            presence::apply_backdrop(window.hwnd(), backdrop);
            let acrylic = backdrop == modular_clipboard_core::config::WindowBackdrop::Acrylic;
            painter.set_backdrop_enabled(acrylic);
            fr.set_clear_color(if backdrop
                == modular_clipboard_core::config::WindowBackdrop::None
            {
                DEFAULT_CLEAR_COLOR
            } else {
                TRANSPARENT_CLEAR_COLOR
            });
        }

        // ---- 4.4c 全局热键与迷你面板（仅单进程模式）----
        //
        // 常驻进程模式下这两件事由常驻进程做（见 `app::daemon` 的
        // `SaveConfig` 分支），界面这边不能碰——热键注册必须发生在
        // 托盘线程的窗口上，而界面进程没有托盘。
        if resident.is_some() {
            let toggle = modular_clipboard_platform::hotkey::spec_or_default(
                &app.svc.state.config.hotkey_toggle,
            );
            let paste = modular_clipboard_platform::hotkey::spec_or_default(
                &app.svc.state.config.hotkey_paste_previous,
            );
            if toggle != applied_toggle || paste != applied_paste {
                applied_toggle = toggle;
                applied_paste = paste;
                let failed =
                    modular_clipboard_app::daemon::rebind_hotkeys(&app.svc.state.config);
                app.svc.state.hotkey_error =
                    (!failed.is_empty()).then(|| failed.join("；"));
            }
            // 迷你面板：条目快照每帧同步（面板弹出时读的就是它，
            // 至多 10 条短文本，量级可忽略）；掩码变了才重装钩子
            // （它在每一次右键上都会被回调，不能无脑重装）。
            let mini =
                modular_clipboard_platform::tray::mini_mask(&app.svc.state.config.mini_panel);
            if mini.is_some() {
                modular_clipboard_app::daemon::sync_mini_items(&app.svc, &mini_items);
            }
            if mini != applied_mini {
                applied_mini = mini;
                modular_clipboard_platform::tray::set_mini_panel(mini, mini_items.clone());
            }
        }

        // ---- 4.5 附属窗口（卡片子窗口 + 设置窗口）----
        let topmost = app.svc.state.config.ui.always_on_top;
        drive_attach_windows(
            &mut app,
            &mut children,
            &mut settings_win,
            &mut popups,
            &mut img_edit,
            &shared,
            &window,
            frame_no,
            topmost,
            !hidden && !window.is_minimized(),
        );

        frame_no += 1;
        if frame_no.is_multiple_of(300) {
            tracing::debug!(
                frame = frame_no,
                vertices = painter.stats.vertices,
                draw_calls = painter.stats.draw_calls,
                rebuilds = painter.stats.rebuilds,
                "帧统计"
            );
        }
        if env_flag("MC_DIAG") && frame_no.is_multiple_of(30) {
            let (pw, ph) = window.inner_size_physical();
            tracing::warn!(
                frame = frame_no,
                client_px = ?(pw, ph),
                extent_px = ?(fr.extent().width, fr.extent().height),
                ppp,
                screen_rect = ?diag_screen_rect,
                "DIAG 尺寸链路"
            );
        }

        // ---- 4.6 窗口位置实时记忆（防抖落盘）----
        //
        // 只靠退出路径保存的话，强杀/崩溃（任务管理器、断电、蓝屏）
        // 会丢掉用户最后的拖动结果。这里每帧比对一次主窗口位置：
        // 与上次记录相差超过 1 逻辑点、且距上次落盘 ≥500ms 才写盘。
        //
        // CAUTION: 不落盘时**不更新** `last_pos_save`：否则最小化返回的
        // `(-32000,-32000)` 触发一次「无效坐标」后，防抖计时被清掉，
        // 接下来 500ms 内恢复窗口到合法位置也写不进去。
        //
        // 写入的是逻辑点（`screen_position_points` 已除 scale），
        // 与启动恢复路径（`move_to(p.x * s)`）同单位，无需额外换算。
        let pos = window.screen_position_points();
        let moved = match app.svc.state.config.ui.window_pos {
            Some(p) => (p.x - pos.x).abs() > 1.0 || (p.y - pos.y).abs() > 1.0,
            None => true,
        };
        // 边缘拖拽也改变客户区尺寸；同时记住宽高，否则重启后恢复旧尺寸。
        // 最小化期间 GetClientRect 可返回 0，绝不能写进配置。
        let (size_w, size_h) = window.inner_size_physical();
        let size_changed = !minimized && size_w > 0 && size_h > 0
            && ((app.svc.state.config.ui.window_width - size_w as f32).abs() > 1.0
                || (app.svc.state.config.ui.window_height - size_h as f32).abs() > 1.0);
        if moved || size_changed {
            let now = std::time::Instant::now();
            let due = last_pos_save
                .is_none_or(|t| now.duration_since(t) >= std::time::Duration::from_millis(500));
            if due {
                let pos_changed = moved && app.persist_window_pos(pos);
                if pos_changed || size_changed {
                    if size_changed {
                        app.svc.state.config.ui.window_width = size_w as f32;
                        app.svc.state.config.ui.window_height = size_h as f32;
                    }
                    if let Err(e) = app.svc.save_config() {
                        tracing::warn!(%e, "实时保存窗口位置和尺寸失败");
                    } else {
                        last_pos_save = Some(now);
                    }
                }
            }
        }

        // ---- 5. 节流 ----
        // 无事件时不要空转烧 CPU。egui 请求了延时重绘就按它等待。
        //
        // NOTE: `Some(ZERO)` 是「立即重绘」，直接传给 `poll_for` 会走
        // 纯自旋分支（`Some(d) if !d.is_zero()` 不成立），CPU 占用 100%。
        // 但也不能当成「无限等待」——那样聚焦的输入框光标不会闪、
        // 刚输入的字符不会立刻上屏。这里给 1ms 下限：
        // 既避免忙等，又保证输入延迟感知不到。
        let wait = match delay {
            Some(d) if d.is_zero() => Some(std::time::Duration::from_millis(1)),
            // CAUTION: **`None` 必须给一个下限，不能原样传下去。**
            //
            // `None` 是 egui **空闲时**的常态：`Context::run` 在没有
            // outstanding 重绘请求时会把 `repaint_delay` 置为
            // `Duration::MAX`，`repaint_delay()` 于是返回 `None`
            // （见 `egui-0.36.2/src/context.rs`）。
            //
            // 而 `poll_for(None)` 走 `_ => events` 分支——`PeekMessageW`
            // 之后**立即返回**，从不调用 `MsgWaitForMultipleObjects`。
            // 早前这里把 `None` 原样传下去，于是：
            //
            //   鼠标不动、无动画时，主循环以 CPU 极限速度反复执行
            //   run_ui + tessellate + acquire/paint/present。
            //
            // 现象：主窗口可见且空闲时单核 100%、风扇转、笔记本掉电。
            // 上面「不要空转烧 CPU」的注释只在 `Some(ZERO)` 那支成立，
            // `None` 这条更常见的路径完全没堵住。
            //
            // 8ms ≈ 120Hz，既不会让输入有可感延迟，也不会忙等。
            None => Some(std::time::Duration::from_millis(8)),
            other => other,
        };
        events.poll_for(wait);
    }

    // ---- 清理 ------------------------------------------------------------
    //
    // 顺序要紧：先撤托盘图标（用户能立刻看到它消失），
    // 再停捕获并保存配置，最后才销毁渲染资源。
    if let Some(mut r) = resident {
        r.shutdown();
    }
    // 先记下主窗口位置，再关子窗口——`close_all` 之后
    // 读不到任何窗口的位置了。
    //
    // IMPORTANT: 必须在 `shutdown()` 之前取：它会 `save_config()` 把配置落盘，
    // 顺序反了位置就存不进文件（下次启动又回默认位置）。
    let main_pos = window.screen_position_points();
    // 置顶窗口若开着，也记下它的位置。
    //
    // 直接遍历子窗口找置顶那张卡——比先从工作区查 Id 再回查子窗口简单，
    // 也避免「工作区有卡片但子窗口还没建」时的不一致。
    for child in children.iter() {
        let is_pinned = app
            .ws
            .get(child.card_id)
            .is_some_and(|c| c.kind == crate::card::CardKind::Pinned);
        if is_pinned {
            app.set_pinned_window_pos(child.window.screen_position_points());
            break;
        }
    }
    children.close_all();
    // 退出路径也存一次（兜底：窗口全程没移动过、实时记忆没触发时，
    // 这里确保位置仍落盘）。返回值 `false` 说明窗口处于最小化等
    // 无效状态，跳过即可。
    let _ = app.persist_window_pos(main_pos);
    let (final_w, final_h) = window.inner_size_physical();
    if !minimized && final_w > 0 && final_h > 0 {
        app.svc.state.config.ui.window_width = final_w as f32;
        app.svc.state.config.ui.window_height = final_h as f32;
    }
    app.shutdown();
    // `painter` 与 `fr` 借用 `gpu`，必须在 `gpu` 之前 drop。
    drop(painter);
    drop(fr);
    pipeline.destroy(&gpu.device);
    shader.destroy(&gpu.device);
    pipe_layout.destroy(&gpu.device);
    desc_layout.destroy(&gpu.device);
    render_pass.destroy(&gpu.device);
    tracing::info!("共渲染 {frame_no} 帧，正常退出");
    Ok(())
}

/// 按 `modular_clipboard_gfx` 的偏好规则预选表面格式。
///
/// 必须与 `modular_clipboard_gfx::pick_format` 选到同一个：8bit BGR/RGB 优先，
/// 驱动附属窗口：卡片子窗口 + 设置窗口。
///
/// # 为什么必须抽成函数、在**所有**跳过路径上调用
///
/// 主循环有三条 `continue` 路径会跳过第 4.5/4.6 步：
/// 隐藏态、客户区 0×0（最小化）、`acquire` 返回过期。
///
/// 早前附属窗口只在那条「正常路径」上被驱动，于是：
///
/// - 主窗口「隐藏到托盘」后，之前拖出的卡片窗口 / 设置窗口**仍留在
///   屏幕上**，但既不渲染也不泵消息 —— 画面冻结、点不动，Win32 约
///   5 秒后把它们标成「无响应」，用户看到的是一堆僵住的窗口；
/// - 主窗口 resize 期间（`acquire` 频繁返回 None）子窗口跟着卡帧；
/// - 隐藏期间若齿轮被点（`show_settings = true`），因为走不到这一步，
///   设置窗口永远创建不出来，要等主窗恢复才突然弹出。
///
/// 抽成函数是为了让「该驱动附属窗口」这件事只有一个入口，漏不掉。
#[allow(clippy::too_many_arguments)]
fn drive_attach_windows<'a>(
    app: &mut App,
    children: &mut crate::childwin::ChildWindows<'a>,
    settings_win: &mut Option<crate::settingswin::SettingsWindow<'a>>,
    popups: &mut crate::popup::Popups<'a>,
    img_edit: &mut Option<crate::imgedit::ImageEditor<'a>>,
    shared: &crate::multiwindow::SharedGfx<'a>,
    window: &Window,
    frame_no: u64,
    topmost: bool,
    main_visible: bool,
) {
    // 悬浮弹窗（搜索面板 / 类型过滤栏）--------------------------------------
    //
    // 位置来自 `FloatLayout`（主窗口客户区坐标，允许负值 = 窗外），
    // 加主窗口屏幕位置得到屏幕坐标。**在卡片之后驱动**：弹窗要读
    // 本帧刚算出的 `search_expanded` / 类型列表，也要让主窗口下一帧
    // 的搜索状态机拿到弹窗回报的悬停/焦点。
    {
        let (mw, mh) = window.inner_size_points();
        let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(mw, mh));
        let floats = crate::paint::FloatLayout::compute(
            &app.svc.state.config.ui,
            &app.svc.state.items,
            app.paint_state.search_expanded,
            area,
            // 类型栏带文字标签时的实测尺寸（见 UiState::rail_panel_size）。
            app.paint_state.rail_panel_size,
        );
        let anchor = window.screen_position_points();
        let s = window.scale_factor();
        // 主窗口隐藏（失焦隐藏 / 收托盘 / 关闭按钮）时弹窗必须一起收：
        // 它们是**贴着主窗口**的浮动面板，主窗口不在了却留着两块面板
        // 浮在桌面上，是最刺眼的一种「渲染 bug」。
        let want_search = main_visible && app.paint_state.search_expanded;
        // 弹窗把主窗口设为**属主**：始终叠在主窗之上、随主窗最小化一起
        // 收起、主窗被激活时一起到前台（不设属主的话它们是三块互不相干
        // 的顶层窗口，Z 序各自为政）。
        let owner = Some(window.hwnd());
        popups.sync_search(
            app,
            want_search,
            floats.search,
            anchor,
            s,
            shared,
            topmost,
            owner,
        );
        // 弹窗回报「指针是否在面板上 / 输入框是否持有焦点」。
        //
        // IMPORTANT: `Popups::search_hover_focus` 此前**没有任何生产调用方**
        // ——`search_popup_hover` 只在测试里被写过，运行时恒为 false。
        // 后果是状态机少了「悬停面板不折叠」这条判据（面板能不能留住
        // 全靠焦点那条），而且焦点回报只在弹窗真正绘制的那帧才更新
        // （闸门跳过时是旧值）。这里每帧读一次：`pointer_is_over` 是
        // 系统查询、`memory().focused()` 是内存读，代价可忽略。
        if app.paint_state.search_expanded {
            let edit_id = app.paint_state.search_edit_id.unwrap_or(egui::Id::NULL);
            let (hover, focus) = popups.search_hover_focus(edit_id);
            // 反馈翻转必须唤醒主窗口。
            //
            // IMPORTANT: 折叠判据（悬停面板 / 折叠倒计时）活在主窗口的
            // `draw()`（状态机）里，而指针在弹窗与桌面之间移动时主窗口
            // **收不到任何事件** ⇒ 渲染闸门（skip_render）把状态机冻结
            // 在旧值上——「指针已经离开面板了但面板不收」（2026-10-21
            // 用户反馈「无法自动关闭」的根因之一）。翻转时对主 ctx
            // 请求一帧，让状态机有机会走一拍。
            if hover != app.paint_state.search_popup_hover
                || focus != app.paint_state.search_popup_focus
            {
                app.ctx.request_repaint();
            }
            app.paint_state.search_popup_hover = hover;
            app.paint_state.search_popup_focus = focus;
        }
        // 隐藏时**必须清**弹窗回报的两项：否则上一次「正在打字」
        // 的 true 会一直留在状态里，主窗口的搜索状态机认为有人持有
        // 焦点，面板再也收不回去。
        if !app.paint_state.search_expanded {
            app.paint_state.search_popup_hover = false;
            app.paint_state.search_popup_focus = false;
        }
        popups.sync_rail(
            app,
            // 设置窗口打开时收起类型栏：它是**贴着主窗口**的悬浮装饰，
            // 而设置窗口就摆在主窗口右侧——类型栏正好压在设置的导航列上
            // （实机表现：导航里冒出几个悬浮图标按钮，像「按钮错位」）。
            main_visible && floats.rail.is_some() && !app.paint_state.show_settings,
            floats.rail,
            anchor,
            s,
            shared,
            topmost,
            owner,
        );
        // ---- 通知弹窗（`ui.notice_popup` 开启时的通知呈现）----
        //
        // 生命周期判据与另两个弹窗不同：它跟着**通知本身**存活
        // （NOTICE_TTL），不是用户状态。因此：
        // 1. 过期消费放在这里（每帧必到的路径）——弹窗模式下没人画
        //    窗内 toast，`draw_notice_toast` 不再负责清 `notice`；
        // 2. 主窗口隐藏时一起收（通知是主窗口操作的反馈，主窗不在
        //    就没有「刚操作完」的语境）。
        if app.svc.state.config.ui.notice_popup
            && app
                .svc
                .state
                .notice
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= crate::paint::NOTICE_TTL)
        {
            app.svc.state.notice = None;
        }
        let want_notice = main_visible
            && app.svc.state.config.ui.notice_popup
            && app.svc.state.notice.is_some();
        popups.sync_notice(
            app,
            want_notice,
            app.paint_state.notice_panel,
            anchor,
            s,
            shared,
            topmost,
            owner,
        );
    }

    // 卡片子窗口
    //
    // 每个子窗口有**独立的 `egui::Context`、独立的交换链**，
    // 所以要各自走一遍「事件 → run_ui → tessellate → 绘制」。
    //
    // NOTE: 渲染失败**不中断主循环**：一个附属窗口出问题
    // （客户区变 0、交换链过期、驱动抽风）不该让主窗黑屏。
    // 这里记警告后继续，下一帧重试。
    for child in children.iter_mut() {
        // 置顶同步：主窗口置顶时附属窗口必须跟随，否则被压住。
        // `apply_topmost` 幂等，仅状态变化时调 `SetWindowPos`。
        child.apply_topmost(topmost);
        if let Err(e) = child.render(&mut *app) {
            tracing::warn!(card = ?child.card_id, %e, "子窗口渲染失败，跳过本帧");
        }
    }

// 设置窗口---------------------------------------------
    //
    // 懒创建：第一次需要时才建窗口（每次都建会泄漏交换链）。
    //
    // NOTE: 生命周期由 `paint_state.show_settings` 决定，而它可能被
    // 三处改：齿轮点击（`Op::ToggleSettings`）、设置面板里的关闭
    // 按钮、以及用户点标题栏 ✕。前两处在绘制里改，第三处由
    // `sync_and_render` 的返回值告知——**必须**在此把它同步回
    // `false`，否则窗口藏了而状态说「开着」，再点齿轮就没反应。
    if settings_win.is_none() && app.paint_state.show_settings {
        let (mw, _mh) = window.inner_size_points();
        match crate::settingswin::SettingsWindow::create(
            window.screen_position_points(),
            mw,
            window.scale_factor(),
            shared,
        ) {
Ok(w) => {
                    tracing::info!("设置窗口已创建");
                    *settings_win = Some(w);
                }
            Err(e) => {
                // 每帧都会走到这里，必须降频，否则日志刷屏。
                if frame_no.is_multiple_of(60) {
                    tracing::warn!(%e, "创建设置窗口失败，本帧不再重试");
                }
                app.paint_state.show_settings = false;
            }
        }
    }

    // 设置窗口的**生命周期**：关闭 = 销毁，下次打开重建。
    //
    // 早前关闭只是 `SW_HIDE`，窗口与其全部资源**永远留着**：它自带一个
    // `egui::Context`（含一份中文字体数据与字形图集）与一条交换链。
    // 实测（2026-10-12，私有提交）：打开设置 244MB / 不打开 179MB ——
    // 那 65MB 在关闭后**一分钱都没还**，因为对象还在。
    //
    // 取舍：重建要付一次「读字体 + 建交换链」的代价（百毫秒量级），
    // 换来的是关闭后真正腾出内存。托盘常驻工具里，关掉的设置窗口
    // 不该比开着的更占地方——这是默认值该有的行为。
    //
    // NOTE: 重建路径是现成的（`SettingsWindow::create` 本来就只在需要
    // 时才调用），销毁由 `Drop` 收尾（连 `vk::SurfaceKHR` 一起），
    // 因此这里什么都不用额外清理。
    let mut destroy_settings = false;
    if let Some(w) = settings_win.as_mut() {
        // 开场先判断：本帧已经不需要它了（齿轮被再次点击 / 别处改了状态），
        // 直接销毁，连 `sync_and_render` 都不必跑。
        let want_open = app.paint_state.show_settings;
        if !want_open {
            destroy_settings = true;
        } else {
            // 置顶同步：设置窗口跟随主窗口置顶状态（幂等）。
            w.apply_topmost(topmost);
            match w.sync_and_render(&mut *app, true) {
                Ok(true) => {
                    // 用户关了设置窗口（标题栏 ✕）⇒ 状态同步回 false。
                    app.paint_state.show_settings = false;
                    destroy_settings = true;
                    tracing::info!("设置窗口已被用户关闭");
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(%e, "设置窗口渲染失败，跳过本帧");
                }
            }
        }
    }
    if destroy_settings {
        // Drop 在这里发生：交换链、表面、egui Context（含字体数据）一并释放。
        *settings_win = None;
        tracing::debug!("设置窗口资源已释放");
    }

    // 图片编辑窗口（2026-10-21）------------------------------------------
    //
    // 生命周期与设置窗口同款「即关即毁」：编辑是一次性动作，关掉不该
    // 留着一套交换链 + 字体图集（实测同类窗口 65MB 量级）。
    //
    // 原图字节由 `Op::OpenImageEditor` 备在 `image_editor_src` 里，
    // 建窗时取走——状态里不留第二份（几 MB 的图不该有两个副本）。
    let want_img_edit = app.paint_state.image_editor.is_some();
    if !want_img_edit {
        *img_edit = None;
    } else if img_edit.is_none()
        && let Some(id) = app.paint_state.image_editor
    {
        match app.paint_state.image_editor_src.take() {
            Some((bytes, w, h)) => {
                // 摆在主窗口**右侧**并顶部对齐：编辑时要能同时看到
                // 原图（主窗口仍在），左右并排而非叠放。
                let (mw, _mh) = window.inner_size_points();
                let pos = window.screen_position_points() + egui::vec2(mw + 12.0, 0.0);
                match crate::imgedit::ImageEditor::create(
                    id,
                    bytes,
                    (w, h),
                    pos,
                    window.scale_factor(),
                    shared,
                ) {
                    Ok(w2) => {
                        tracing::info!(id, "图片编辑窗口已创建");
                        *img_edit = Some(w2);
                    }
                    Err(e) => {
                        // 建窗失败：状态置回 None，否则每帧重试刷日志。
                        app.paint_state.image_editor = None;
                        app.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("无法打开图片编辑窗口:", "Cannot open the image editor:"))));
                    }
                }
            }
            None => {
                // 有 id 没有字节（OpenImageEditor 失败过 / 已被取走）：
                // 没有数据建不出窗口，直接收意图。
                app.paint_state.image_editor = None;
            }
        }
    }
    if let Some(ed) = img_edit.as_mut() {
        // 置顶跟随主窗口（幂等）。主窗口隐藏时编辑窗口也一起收——
        // 它是主窗口操作的延伸，主窗不在就没有「正在编辑」的语境。
        let want_open = main_visible && app.paint_state.image_editor.is_some();
        ed.apply_topmost(topmost);
        if let Err(e) = ed.sync_and_render(&mut *app, want_open) {
            tracing::warn!(%e, "图片编辑窗口渲染失败，跳过本帧");
        }
    }
}

/// 单格式时按规范原样采用。真实的一致性由 `FrameRenderer::new` 校验。
/// 挑一个表面可用格式。
///
/// 提到 `pub(crate)` 是因为 [`crate::multiwindow::SharedGfx::create`]
/// 也要用同一份逻辑——主窗口与子窗口若各选一次，格式可能不一致，
/// 而不一致会在 `FrameRenderer::new` 的校验里报错。
pub(crate) fn pick_surface_format(gpu: &Gpu) -> anyhow::Result<ash::vk::Format> {
    let formats = &gpu.surface_formats;
    anyhow::ensure!(!formats.is_empty(), "表面未报告任何可用格式");
    if formats.len() == 1 {
        return Ok(formats[0].format);
    }
    formats
        .iter()
        .map(|f| f.format)
        .find(|f| {
            matches!(
                *f,
                ash::vk::Format::B8G8R8A8_UNORM | ash::vk::Format::R8G8B8A8_UNORM
            )
        })
        .ok_or_else(|| anyhow::anyhow!("表面不支持可用的颜色格式"))
}

/// 重建交换链并让绘制器跟着重整资源。
///
/// 两者必须借用同一个 `Gpu`，因此共享生命周期参数 `a`——
/// `Painter<'a>` 内部也持有 `&'a Gpu`。
fn rebuild<'a>(
    fr: &mut FrameRenderer<'a>,
    painter: &mut Painter<'a>,
    extent: (u32, u32),
) -> anyhow::Result<()> {
    // IMPORTANT: **必须**传目标尺寸（2026-10-22 修复）。
    //
    // 早前这里传 `Default::default()`（零尺寸），`rebuild_swapchain`
    // 于是走 `FALLBACK_EXTENT`(1280x800) 分支——只有 `caps.current_extent`
    // 碰巧有效时才会等于真实客户区。一旦驱动对当前表面报告 0/越界的
    // current_extent，交换链就被建成一个与窗口不符的尺寸，下一帧
    // “尺寸不符”再次成立 ⇒ 无限重建，每帧一次全局 `device_wait_idle`。
    fr.rebuild_swapchain(ash::vk::Extent2D {
        width: extent.0,
        height: extent.1,
    })?;
    painter.on_swapchain_rebuilt(fr)
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

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


/// 应用状态。
///
/// 只持有业务状态与 [`egui::Context`]；渲染资源在 [`run`] 的栈上。
/// 两者分离让 [`App`] 可以在没有 GPU 的环境下被单测。
pub struct App {
    /// egui 上下文。内部是 `Arc`，克隆代价极低。
    pub ctx: egui::Context,
    svc: Service,
    /// 上一次已经提示过的「快捷键注册失败」内容。
    ///
    /// 用于去重：失败信息在修复前一直挂在状态里，没有这个字段就会
    /// 每帧弹一次提示。
    last_hotkey_error: Option<String>,
    /// 已解析生效的调色板。
    ///
    /// 由 [`theme::set_theme`] 在构造时产出：它同时把令牌灌进 egui
    /// `Visuals` 并返回自绘要用的那一份，因此「控件观感」与「自绘图形」
    /// 必然同源。每帧把它交给 [`crate::paint::draw`]。
    ///
    /// 主题切换时它是**显示中**的调色板（补间插值的结果），目标值在
    /// `pal_to`——见 `draw_frame` 里的补间推进（F-34 / UI-P1-3）。
    pal: theme::Palette,
    /// 主题补间的源调色板（切换前那一份）。未切换时等于 `pal_to`。
    pal_from: theme::Palette,
    /// 主题补间的目标调色板（切换后那一份）。
    pal_to: theme::Palette,
    /// **基础**调色板：主题原色，字号令牌为 1.0×（未叠加 `font_scale`）。
    ///
    /// # 为什么必须与显示用的 `pal` 分开存
    ///
    /// 字体缩放是 `Palette::with_font_scale` 的**乘性**派生：
    /// 在已缩放的调色板上再乘一次会叠乘（1.15 × 1.15 = 1.3225），
    /// 字号每改一次就翻一档。因此每次重算都必须从这份「1.0× 原色」
    /// 派生，而不是从当前显示值派生。
    pal_base: theme::Palette,
    /// 新卡片架构的工作区（卡片的唯一权威来源）。
    ///
    /// `pub` 是因为帧循环要用它同步子窗口：
    /// [`crate::childwin::ChildWindows::sync_with`] 要读每张卡片的
    /// [`crate::card::CardHost`] 并写回窗口位置。
    pub ws: crate::workspace::Workspace,
    /// 新卡片架构的界面状态。
    ///
    /// `pub(crate)`：悬浮弹窗（`crate::popup`）要读写搜索展开态与
    /// 弹窗回报的悬停/焦点——它们与主窗口共用这一份状态（弹窗有自己的
    /// `egui::Context`，但**配置 / 查询词 / 选中项**只有一份）。
    pub(crate) paint_state: crate::paint::UiState,
    /// 诊断用帧计数（仅 `MC_DIAG` 打开时用）。
    diag_frames: u64,
    /// 界面退出请求。
    should_quit: bool,
    /// 窗口身后桌面的平均亮度（0~1），由帧循环从渲染器取来。
    ///
    /// `None` = 本帧没有成功捕获（磨砂未开 / 最小化 / 首帧）。
    /// 只影响「背景层画多实」这一个决策，见 [`backdrop_alpha`]。
    pub backdrop_lum: Option<f32>,
    /// MCP 服务连通性探测器（后台线程 + 原子状态）。
    ///
    /// F-15 / UI-P0-1：`McpProbe` 实现了完整的四态探测，但启动以来
    /// **全仓没有任何地方启动过它**——`UiState::mcp_conn` 的文档写着
    /// 「由 App 每帧从探测器同步过来」，实际没有写入方，覆盖层状态行
    /// 于是永远显示「未连接·总开关已关闭」，能力徽标永远处于未连接
    /// 样式，即使 MCP 服务实际在运行。用户无从判断服务真实状态。
    mcp_probe: crate::mcp_status::McpProbe,
}

impl App {
    /// 按配置初始化业务状态与视觉风格。
    ///
    /// `data_dir` 对应 `--data-dir`：`None` 时用默认数据目录。
    pub fn new(
        config: modular_clipboard_core::Config,
        capture_is_override: bool,
        data_dir: Option<&std::path::Path>,
    ) -> Self {
        let mut svc = match Service::with_data_dir(config.clone(), data_dir) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(%e, "服务初始化失败，界面将以空状态运行");
                // 用内存库兜底，避免整个应用无法启动。
                Service::in_memory()
            }
        };
        // 标记 `--no-capture` 这类临时覆盖，使 save_config 跳过落盘——
        // 否则退出时会把 false 写进用户的 config.json，
        // 让剪贴板监听**永久关闭**且无任何提示。
        svc.set_capture_override(capture_is_override);
        Self::from_service(config, svc)
    }

    /// 界面进程入口：连到常驻进程，以它那份配置为界面配置。
    ///
    /// # 为什么配置要以来常驻进程为准
    ///
    /// 界面进程是**临时的**：关掉就没了。若它读自己的 config.json
    /// 再用它初始化窗口，而常驻进程同时也在写同一个文件，
    /// 两边就会互相覆盖（典型的「读 A 写 B」）。统一以来常驻进程为准，
    /// 配置就只有一个权威来源。
    pub fn new_remote() -> anyhow::Result<Self> {
        // 5 秒缓冲：托盘刚拉起界面时，常驻进程的管道可能还没建好。
        let svc = Service::connect_remote(std::time::Duration::from_secs(5))?;
        let config = svc.state.config.clone();
        Ok(Self::from_service(config, svc))
    }

    /// 用**已有的**服务实例构造界面（两种模式共用）。
    pub fn from_service(config: modular_clipboard_core::Config, mut svc: Service) -> Self {
        let ctx = egui::Context::default();
        // 自启动的真相在**注册表**里，配置只是缓存。
        //
        // 用户完全可能在「任务管理器 → 启动」里把它关掉，
        // 那时配置仍写着 true，界面就会显示一个假状态。启动时对一次账，
        // 顺手把配置修正过来。
        let actual = modular_clipboard_platform::autostart::is_enabled();
        if svc.state.config.ui.autostart != actual {
            tracing::info!(
                config = svc.state.config.ui.autostart,
                registry = actual,
                "自启动状态以注册表为准，已修正配置"
            );
            svc.state.config.ui.autostart = actual;
        }
        let _ = config;
        theme::install_cjk_font(&ctx, config.ui.font_path.as_deref());
        // 图标字体必须排在 CJK 之后：`install_cjk_font` 用 `set_fonts`
        // 整体替换 `FontDefinitions`，先装图标会被它冲掉。
        //
        // 早前这里因`install_icon_font` 内部调用 `ctx.fonts(|f| ..)`
        // 而被临时停用——egui 0.36 明确标注 `fonts()`
        // *"Not valid until first call to Context::run_ui()"*，
        // 在 `App::new` 里调用直接 panic：
        //   No fonts available until first call to Context::run()
        // 现已改用 `ctx.add_font(FontInsert{..})`（egui 文档：*keep the
        // existing fonts*，即追加而非替换），因此可以在 `App::new` 里安全调用。
        if !theme::install_icon_font(&ctx) {
            tracing::warn!("图标字体注册失败，界面图标将缺失");
        }

        // 载入界面偏好。未指定时跟随系统（`resolve` 内部会探测）。
        //
        // 解析与灌入一次做完，返回的 `Palette` 存进 `App.pal` 给每帧自绘用——
        // 两处必须同源，早前`view::draw` 每帧自己算一遍，既浪费
        // （55 个字段逐个构造）又可能与灌进 Visuals 的那次解析分叉。
        //
        // 字体缩放（`UiConfig::font_scale`）叠加在主题原色之上：
        // 解析链返回的是 1.0× 原色，这里派生显示用的缩放版并
        // **再灌一次** egui——否则控件层（输入框/按钮内文字）拿到的
        // 还是未缩放字号，与自绘层不一致。
        //
        // 主题解析链（一级 → 二级 → 强调色 → 覆盖）只有这一个入口；
        // 运行时的任何一层变化走 `App::retint`（同一函数）。
        let font_scale = theme::clamp_font_scale(svc.state.config.ui.font_scale);
        let init_mode = theme::ThemeMode::from_dark_mode(svc.state.config.ui.dark_mode);
        let init_effective = theme::resolve(init_mode, theme::detect_system_theme());
        let pal_base = theme::resolve_palette(init_effective, &svc.state.config.ui);
        let pal = pal_base.with_font_scale(font_scale);
        theme::apply(&ctx, &pal);
        tracing::info!(
            dark = pal.is_dark,
            font_scale,
            configured = ?svc.state.config.ui.dark_mode,
            secondary = ?svc.state.config.ui.secondary_theme,
            accent = ?svc.state.config.ui.accent,
            overrides = svc.state.config.ui.theme_overrides.colors.len()
                + svc.state.config.ui.theme_overrides.nums.len(),
            "主题已解析"
        );

        let mut ws = crate::workspace::Workspace::default();
        // 恢复上次保存的卡片窗口位置。
        //
        // NOTE: 这里**不恢复** `config.ui.layout`（旧 `LayoutState` 布局）：
        // 那套四面板停靠/浮动体系连同 `layout.rs` 已随`view.rs` 一并删除，
        // 新架构的几何由 `solver` 每帧求解、由 `card.window_pos` 持久化。
        // 继续做那个往返只会把一份没人读的配置原样写回去。
        ws.apply_saved_positions(&svc.state.config.ui);

        svc.start_capture();

        // 启动 MCP 连通性探测（F-15 / UI-P0-1）：后台线程每 3s 探一次
        // `GET /health`，界面每帧只读原子量。必须真的启动——
        // 探测器实现齐全但从未被接线的话，MCP 覆盖层永远显示未连接。
        let mcp_probe = crate::mcp_status::McpProbe::start();
        mcp_probe.update(&svc.state.config.mcp);

        Self {
            ctx,
            svc,
            last_hotkey_error: None,
            pal: pal.clone(),
            pal_from: pal.clone(),
            pal_to: pal,
            pal_base,
            ws,
            paint_state: crate::paint::UiState::default(),
            diag_frames: 0,
            should_quit: false,
            backdrop_lum: None,
            mcp_probe,
        }
    }

    /// 画一帧业务 UI。
    pub fn draw_frame(&mut self, ui: &mut egui::Ui) {
        // `MC_SETTINGS=1` 时默认打开设置界面。
        //
        // 用途是**截图验证**：设置面板是覆盖层，不点齿轮按钮就拍不到。
        // 做成环境变量而不是命令行参数，是为了让截图脚本不用改传参逻辑。
        // CAUTION: 必须**只打开一次**。早期实现每帧置 `true`，带该环境
        // 变量启动的实例里设置窗口永远关不掉——点 ✕ / 齿轮把
        // `show_settings` 置回 `false`，下一帧这里又改回 `true`，
        // 用户实测「设置窗口无法关闭，从主窗口点击也无法关闭」
        // （2026-10-11）。首帧打开一次后置位，之后关闭走正常逻辑。
        if env_flag("MC_SETTINGS") && !self.paint_state.settings_startup_opened {
            self.paint_state.show_settings = true;
            self.paint_state.settings_startup_opened = true;
        }

        // MCP 连通状态同步（F-15 / UI-P0-1）：目标没变时 `update` 是
        // 空操作（不加锁不克隆）；状态是原子读。两者都远低于一帧预算，
        // 但探测线程的节奏是自己的（3s 一次 + 400ms 超时）。
        self.mcp_probe.update(&self.svc.state.config.mcp);
        self.paint_state.mcp_conn = self.mcp_probe.state();

        // 主题补间推进（F-34 / UI-P1-3）：mix 0 → 1。settled 后是恒等
        // 变换（from == to），成本只有一次 Palette 字段级克隆。
        //
        // egui 控件层（Visuals）也每帧重灌：只补间自绘层的话，
        // ScrollArea/tooltip 会瞬跳而卡片在渐变，两层不同步更难看。
        // 补值来自 `Palette::lerp`——只有颜色参与，几何令牌取目标值。
        //
        // IMPORTANT: 这里的目标恒为 1.0 ⇒ 本调用是**惰性**的（到 1.0 后
        // 什么都不做）。真正让补间「重播」的是 `Op::SetThemeMode` 里的
        // `anim::restart`（见那里的 CAUTION）。时长必须与重启处一致，
        // 因此共用 `theme::PALETTE_TWEEN`。
        let pal_mix = crate::anim::tween_f32(
            &self.ctx,
            egui::Id::unique("pal_mix"),
            1.0,
            crate::theme::PALETTE_TWEEN,
            crate::anim::Ease::InOutCubic,
        );
        if pal_mix < 1.0 {
            self.pal = self.pal_from.lerp(&self.pal_to, pal_mix);
            crate::theme::apply(&self.ctx, &self.pal);
        } else if self.pal != self.pal_to {
            // 补间刚走完的那一帧：`tween_f32` 到点后直接返回目标值，
            // 上面的分支不再执行，显示值会停在**最后一帧的近似值**上
            // （差几个色阶）。这里把它精确落到目标，之后逐字段比较
            // 恒等、不再重复灌入。
            self.pal = self.pal_to.clone();
            crate::theme::apply(&self.ctx, &self.pal);
        }

        // ---- 新卡片架构 ------------------------------------------------
        //
        // 顺序是硬性的：**先求解、再应用、后绘制**。
        // `solver::solve` 是纯函数（不改工作区），`apply` 才写入矩形。
        // 绘制层只读 `card.rect`，不做任何二次判断——这正是新架构
        // 消除「布局算一遍、绘制再判一遍」的关键。
        let area = ui.max_rect();
        let ppp = ui.ctx().pixels_per_point();
        // IMPORTANT: 必须传真实的 `ppp`：落位要对齐**设备**像素栅格，
        // 否则高DPI 下卡片边界落在半像素上，描边发虚。
        //
        // 2026-10-15 第三轮：悬浮搜索面板与类型过滤栏已改为**独立弹窗**
        // （`crate::popup`），漂在窗口外，卡片区因此**不再需要让位**
        // ——恢复满宽（`solve_scaled_inset` 已随之删除）。
        let sol = crate::solver::solve_scaled(&self.ws, area, ppp);
        crate::solver::apply(&mut self.ws, &sol);

        if env_flag("MC_DIAG") {
            self.diag_frames += 1;
            if self.diag_frames.is_multiple_of(60) {
                let rs: Vec<String> = self
                    .ws
                    .cards
                    .iter()
                    .map(|c| format!("{:?}={:.1}", c.kind, c.rect.width()))
                    .collect();
                tracing::warn!(
                    frame = self.diag_frames,
                    area = ?area,
                    ppp,
                    card_w = ?rs,
                    "DIAG 卡片布局"
                );
            }
        }

        // 磨砂 / 半透明模式：背景层整体降 alpha；内容层（卡片/文字）
        // 保持不透明。滑块越大背景越实。
        //
        // 两条模式各自的**基准**区间（0.3~1.0 → 背景 alpha）：
        // - 磨砂：0.18~0.60。身后是**已模糊**的桌面，透出比例高也不损
        //   可读性；上限 0.6 保证模糊始终可见。
        // - 半透明：0.55~0.85。身后是**未经模糊的**真实桌面，需要更实
        //   的背景才能保住对比度。
        //
        // 再按**背景亮度**上调实度（见 [`backdrop_alpha`]）：纯色（无壁纸）
        // 桌面是最常见的极端——浅色主题配纯黑桌面时，若背景层只有 33%，
        // 合成出来是深灰底 + 深色文字，对比度直接崩掉。
        // 无模式用原 palette。
        let backdrop = self.svc.state.config.ui.window_backdrop;
        let opacity = self.svc.state.config.ui.window_opacity;
        // 卡片不透明度：磨砂 / 半透明下把行卡片底色也降 alpha，让模糊底图
        // 透过卡片显示（否则只有行间缝隙能看出磨砂，卡片本身仍是实底）。
        // 无背景时不生效——不透明底图后面没有东西可透。
        let card_alpha = (self.svc.state.config.ui.card_opacity.clamp(0.3, 1.0) * 255.0)
            .round() as u8;
        let is_dark = self.pal.is_dark;
        let backdrop_pal;
        let pal_ref: &theme::Palette = match backdrop {
            modular_clipboard_core::config::WindowBackdrop::Acrylic => {
                let base = opacity.clamp(0.3, 1.0) * 0.6;
                let alpha = backdrop_alpha(base, self.backdrop_lum, is_dark);
                backdrop_pal = self
                    .pal
                    .with_backdrop_alpha(alpha)
                    .with_card_alpha(card_alpha);
                &backdrop_pal
            }
            modular_clipboard_core::config::WindowBackdrop::Translucent => {
                let base = (opacity.clamp(0.3, 1.0) * 0.85).max(0.55);
                let alpha = backdrop_alpha(base, self.backdrop_lum, is_dark);
                backdrop_pal = self
                    .pal
                    .with_backdrop_alpha(alpha)
                    .with_card_alpha(card_alpha);
                &backdrop_pal
            }
            modular_clipboard_core::config::WindowBackdrop::None => &self.pal,
        };

        {
            let mut frame = crate::paint::Frame {
                ui,
                state: &mut self.paint_state,
                svc: &mut self.svc,
                ws: &self.ws,
                pal: pal_ref,
                scale: ppp,
                area,
                // `None` = 画主窗口（顶栏 + 全部停靠卡片）。
                card_id: None,
                hit_ns: crate::paint::HitNs::Main,
            };
            crate::paint::draw(&mut frame);
        }

        // 录制快捷键：**必须在绘制之后**读按键，因为 egui 的事件只在
        // 这一帧的输入里，且要先让输入框/按钮消费掉它们自己的点击。
        let ctx = self.ctx.clone();
        self.capture_recording(&ctx);

        // 绘制层只识别操作、不改布局：统一在这里落到工作区与服务上。
        //
        // NOTE: 主窗口自己的 Op **不需要**在这里 `request_repaint()`：
        // 本函数就运行在主窗口这一帧的绘制里，画面立刻就带上新状态。
        let _ = self.apply_ops();

        // 快捷键注册失败（常驻进程侧的反馈）要浮到界面上。
        self.surface_hotkey_error();

        // CAUTION: 这里**不要**写「`should_quit` 时停捕获并存配置」——
        // 那属于退出流程，而 `shutdown()` 已经做了同样的两件事。
        // 放在绘制函数里意味着只要标志为真就**每帧重复**
        // `save_config()`（一次全量 JSON 序列化写盘）。
        //
        // 早前这段因为 `should_quit` 从没被置真而从不执行，
        // 所以看不出来；接上退出按钮后它就成了真实的性能问题。
        // 退出收尾统一由帧循环退出后的 `App::shutdown()` 负责。
    }

/// 修饰键的**物理键**变体。
///
/// egui 0.36 的 `Key` 枚举含这些（eframe 从 winit 的物理键映射而来），
/// 而修饰键的逻辑状态在 `Event::Key::modifiers` 里——两者都会来。
/// 录制时按到修饰键本身必须忽略：不过滤就会把它当成「主键」，
/// `name()` 返回 `ControlLeft` 这类多字符文本，于是弹
/// 「不支持把 ControlLeft 设为快捷键」并**退出录制态**，
/// 用户接下来的组合再也录不进去（2026-10-13 用户实测）。
fn is_modifier_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::ControlLeft
            | egui::Key::ControlRight
            | egui::Key::ShiftLeft
            | egui::Key::ShiftRight
            | egui::Key::AltLeft
            | egui::Key::AltRight
            | egui::Key::SuperLeft
            | egui::Key::SuperRight
    )
}

/// 录制模式下捕获按键，生成新的快捷键绑定。
///
/// # 为什么要在 UI 线程里做
///
/// 全局热键的注册在托盘/常驻进程，但**录制**必须在界面这边：
/// 键是从 egui 的输入事件里读的，而输入只属于窗口所在的进程。
/// 录完写进配置，常驻进程收到配置保存后会自己重注册
/// （见 `app::daemon::rebind_hotkeys`）。
fn capture_recording(&mut self, ctx: &egui::Context) {
    // 只关心「是否在录制」：候选组合记在 paint_state 里，
    // 槽位在确认（Op::FinishRecording）时才用到。
    let Some(_slot) = self.paint_state.hotkey_recording else {
            return;
        };

        // Esc 取消：录到一半想反悔是常态，没有退路会很烦。
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.paint_state.hotkey_recording = None;
            self.paint_state.recording_combo = None;
            // 反悔之后老键必须立刻能用。
            self.set_hotkeys_suspended(false);
            return;
        }

        // 候选组合**只记录不应用**：录制覆盖层实时显示它，用户点击
        // 任意位置确认后才落地（`Op::FinishRecording`）。按一下就改的
        // 设计会让手抖直接换掉热键。
        let captured = ctx.input(|i| {
            i.events.iter().find_map(|ev| match ev {
                egui::Event::Key {
                    key,
                    modifiers,
                    pressed: true,
                    ..
                } if !Self::is_modifier_key(*key) => Some((*key, *modifiers)),
                _ => None,
            })
        });
        let Some((key, mods)) = captured else {
            return;
        };
        // 挡掉方向键这类非字符键：提示但**不退出录制态**——用户可能
        // 只是按错键，退出录制反而要重新点一次「录制」（旧实现在这里
        // 直接退出，被用户评为「离谱的逻辑」）。
        let name = key.name();
        let mut chars = name.chars();
        let _first = chars.next();
        if chars.next().is_some() && !name.starts_with('F') {
            self.svc.notify(modular_clipboard_core::i18n::t(("不支持把 {name} 设为快捷键", "{name} is not a supported shortcut")).replace("{name}", name));
            return;
        }
        let hotkey = modular_clipboard_core::config::Hotkey {
            key: name.to_string(),
            ctrl: mods.ctrl,
            shift: mods.shift,
            alt: mods.alt,
            // egui 的 `command` 在 Windows 上就是 Ctrl；Win 键它不报告，
            // 因此单独问一次系统——Win+V 这类组合必须能录出来。
            win: modular_clipboard_platform::win_key_down(),
        };
        if !hotkey.ctrl && !hotkey.shift && !hotkey.alt && !hotkey.win {
            self.svc.notify(modular_clipboard_core::i18n::t(("至少按住一个修饰键（Ctrl/Shift/Alt/Win）", "Hold at least one modifier (Ctrl/Shift/Alt/Win)")).into());
            return;
        }
        // 后按的覆盖先按的：用户改主意就再按一次新的。
        self.paint_state.recording_combo = Some(hotkey);
    }

    /// 把一个槽位的绑定落地：写配置、清草稿、恢复注册、落盘。
    ///
    /// 文本输入（`Op::SetHotkeyText` 解析后）、设置卡片「重置」与
    /// 录制确认三条路共用它——分开写过三遍，必漂。
    fn apply_hotkey(
        &mut self,
        slot: crate::paint::HotkeySlot,
        hotkey: modular_clipboard_core::config::Hotkey,
    ) {
        match slot {
            crate::paint::HotkeySlot::Toggle => {
                self.svc.state.config.hotkey_toggle = hotkey;
            }
            crate::paint::HotkeySlot::PastePrevious => {
                self.svc.state.config.hotkey_paste_previous = hotkey;
            }
        }
        // 草稿一并清掉：留着旧文本，下次回车会把上次手打的
        // 内容再应用一遍（「录成了框里却还有字」）。
        slot.draft_mut(&mut self.paint_state).clear();
        // 录制期间热键是注销状态。常驻进程模式下
        // `after_config_change` 的 SaveConfig 会让它重注册；
        // 单进程模式只会写盘，得在这儿立即恢复——
        // 用户录完很可能马上按键测试，不能让他吃空窗。
        if !self.svc.is_remote() {
            let failed = modular_clipboard_app::daemon::rebind_hotkeys(&self.svc.state.config);
            self.svc.state.hotkey_error = (!failed.is_empty()).then(|| failed.join("；"));
        }
        self.after_config_change();
    }

    /// 是否正在录制快捷键（设置窗口据此走 GPU 模糊路径）。
    pub fn hotkey_recording_open(&self) -> bool {
        self.paint_state.hotkey_recording.is_some()
    }

    /// 录制快捷键期间暂停 / 恢复全局热键。
    ///
    /// # 为什么要暂停
    ///
    /// `RegisterHotKey` 注册的组合会被 OS 从所有窗口没收：按键不再
    /// 产生 `WM_KEYDOWN`，而是直接送给注册窗口。于是用户点「录制」后
    /// 按下**当前正绑着的组合**（想把 Win+V 换掉时先按一下确认）时，
    /// 设置窗口永远收不到——录制像卡死，用户反馈「录制有问题」。
    ///
    /// 恢复由持有托盘的一侧按当前配置重注册：常驻进程走 IPC 命令，
    /// 单进程模式托盘就在本进程。
    fn set_hotkeys_suspended(&mut self, suspend: bool) {
        if let Err(e) = self.svc.suspend_hotkeys(suspend) {
            tracing::warn!(%e, suspend, "暂停/恢复全局热键失败");
        }
    }

    /// 把「快捷键注册失败」这条反馈浮到界面上（状态栏提示）。
    ///
    /// 只在**内容变化**时提示：失败值在修复前一直挂在状态里，
    /// 每帧都弹一遍等于状态栏永远停着这条消息。
    ///
    /// 来源有两个：常驻进程把它放进快照（注册发生在托盘线程里，
    /// 只有那边拿得到结果）；单进程模式由本进程直接写入。
    /// 两者落点都是 `svc.state.hotkey_error`，这里不区分来源。
    fn surface_hotkey_error(&mut self) {
        if self.svc.state.hotkey_error == self.last_hotkey_error {
            return;
        }
        self.last_hotkey_error = self.svc.state.hotkey_error.clone();
        if let Some(msg) = self.svc.state.hotkey_error.clone() {
            self.svc.notify(msg);
        }
    }

    /// 把绘制层收集到的操作落到工作区与服务上。
    ///
    /// 绘制层**只识别、不改状态**——所有变更集中在这里，
    /// 于是「点了折叠按钮」这件事不会散落在几十个绘制函数里。
    ///
    /// 返回「本帧是否真的改了共享状态」。**调用方必须看这个返回值**：
    /// 弹窗 / 设置窗 / 卡片子窗各有**独立的 `egui::Context` 与消息队列**，
    /// 它们改了 `paint_state`（选中、类型过滤、搜索词…）或服务层（列表、
    /// 通知）之后，主窗口既收不到事件也不会请求重绘 ⇒ 主循环的渲染闸门
    /// （`skip_render`）判定「没事可做」而跳过整帧 ⇒ **主窗口画面停在旧
    /// 状态**，直到用户碰一下鼠标才刷新（表现为「点了类型过滤但列表不变」）。
    /// 因此非主窗口的三个入口都要在返回 `true` 时 `ctx.request_repaint()`。
    fn apply_ops(&mut self) -> bool {
        use crate::paint::Op;
        let ops = self.paint_state.take_ops();
        // 有 Op 被上报 = 用户确实做过交互（Op 只在交互时 push）。
        let mut changed = !ops.is_empty();
        // 「任意操作取消选中」（2026-10-15 第三轮原型定稿）：
        // 选中是临时状态，任何**行操作**（置顶 / 删除 / 复制 / 打开 /
        // 清空）之后都不该继续高亮——否则用户会以为选中项还有后续
        // 作用（实际下一帧它就只是背景高亮）。
        //
        // CAUTION: 配置类 Op（SetBool / SetThemeMode…）**不**清选中：
        // 那些来自设置窗口，与主窗口的选中态无关。
        let row_op = ops.iter().any(|o| {
            matches!(
                o,
                Op::TogglePin(_)
                    | Op::Delete(_)
                    | Op::Copy(_)
                    | Op::OpenUrl(_)
                    | Op::RevealPath(_)
                    | Op::ClearAll
            )
        });
        for op in ops {
            match op {
                Op::SetBool { group, field, value } => {
                    let is_capture_enabled =
                        matches!(group, crate::paint::ConfigGroup::Capture) && field == "enabled";
                    set_bool_field(&mut self.svc.state.config, group, field, value);
                    // 「记录剪贴板」开关必须在运行时立即生效，不能等重启：
                    // 否则用户开/关开关后，捕获线程维持旧状态（开着时新复制
                    // 全部丢弃、关了时线程继续空转），复制监听表现为「改了无效」。
                    if is_capture_enabled {
                        if value {
                            self.svc.start_capture();
                        } else {
                            self.svc.stop_capture();
                        }
                    }
                    self.after_config_change();
                }
                Op::SetNumber { group, field, value } => {
                    set_number_field(&mut self.svc.state.config, group, field, value);
                    // 字体缩放必须在运行时立即生效：配置写进去只是记账，
                    // 真正让界面变大变小的是「基础调色板 × 缩放」这一派生
                    // （早前 `font_scale` **只有写入方、没有读取方**，
                    // 改它完全没反应——用户反馈「字体缩放修改失效」）。
                    if matches!(group, crate::paint::ConfigGroup::Ui) && field == "font_scale" {
                        self.apply_font_scale();
                    }
                    self.after_config_change();
                }
                Op::SetText { group, field, value } => {
                    set_text_field(&mut self.svc.state.config, group, field, &value);
                    self.after_config_change();
                }
                Op::SetLanguage(lang) => {
                    // 写配置 + 立即生效（`set_locale` 是全局原子量，
                    // 下一帧所有 `t()` 取的就是新语言）。
                    self.svc.state.config.ui.language = lang.to_string();
                    modular_clipboard_core::i18n::set_locale(
                        modular_clipboard_core::i18n::resolve(
                            lang,
                            modular_clipboard_core::i18n::from_system_langid(
                                crate::presence::system_langid(),
                            ),
                        ),
                    );
                    self.after_config_change();
                }
                Op::CopyMcpPairPrompt => {
                    // 把按当前 MCP 配置动态拼接的接入说明复制到剪贴板。
                    //
                    // CAUTION: `write_clipboard_text` 内部在写之前武装抑制
                    // （同 `copy_item` 规则），防止捕获线程把自己写的内容再记一条。
                    let prompt = crate::paint::mcp_pair_prompt(&self.svc.state.config);
                    match self.svc.write_clipboard_text(&prompt) {
                        Ok(()) => self.svc.notify(modular_clipboard_core::i18n::t(("已复制 AI 配对提示词", "AI pairing prompt copied")).into()),
                        Err(e) => self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("复制配对提示词失败:", "Failed to copy the pairing prompt:")))),
                    }
                }
                Op::SetOptNumber { group, field, value } => {
                    set_opt_number_field(&mut self.svc.state.config, group, field, value);
                    self.after_config_change();
                }
                Op::SetBackdrop(mode) => {
                    // CAUTION: 背景效果必须**运行时立即生效**，不能等重启：
                    // 用户从设置里切换无/磨砂/半透明后马上就能看到。
                    // 配置只负责记住选择，真正调用 Win32 API 的时机在
                    // 帧循环的幂等比对（`applied_backdrop`）。
                    self.svc.state.config.ui.window_backdrop = mode;
                    self.after_config_change();
                }
                Op::SetOpacity(opacity) => {
                    self.svc.state.config.ui.window_opacity = opacity.clamp(0.3, 1.0);
                    self.after_config_change();
                }
                Op::SetCardOpacity(opacity) => {
                    // 卡片透明度只影响绘制（下一帧读配置即生效），
                    // 钳制与背景不透明度同一区间。
                    self.svc.state.config.ui.card_opacity = opacity.clamp(0.3, 1.0);
                    self.after_config_change();
                }
                Op::SetBlurStrength(level) => {
                    // 磨砂强度只改卷积半径，运行时下一帧卷积即生效（渲染层
                    // 检测到半径变化会强制重捕获，无需重建资源）。
                    self.svc.state.config.ui.window_blur_strength =
                        level.clamp(1, modular_clipboard_gfx::blur::BLUR_LEVELS);
                    self.after_config_change();
                }
                Op::SetCardShadow(s) => {
                    // 卡片阴影只需改配置：行卡片每帧读配置画投影，下一帧
                    // 即生效。值域钳制在绘制侧做（见 `draw_card_shadow`），
                    // 这里不重复——配置被手工改超界时绘制侧兜底。
                    self.svc.state.config.ui.card_shadow = s;
                    self.after_config_change();
                }
                Op::SetRowMeta(m) => {
                    // 右下角信息组合：顺序/显隐只影响绘制，下一帧读配置
                    // 即生效；after_config_change 负责落盘。
                    self.svc.state.config.ui.row_meta = m;
                    self.after_config_change();
                }
                Op::SetTypeRail(m) => {
                    // 类型过滤栏：只影响绘制（FloatLayout 每帧读配置），
                    // 下一帧即生效；尺寸在绘制侧钳制，这里原样存。
                    self.svc.state.config.ui.type_rail = m;
                    self.after_config_change();
                }
                Op::SetSearchPanel(m) => {
                    // 搜索面板：停靠位置与高度只影响 FloatLayout 几何，
                    // 下一帧即生效（弹窗 resize_to 会跟着改窗口尺寸）；
                    // 高度在几何侧钳制，这里原样存。
                    self.svc.state.config.ui.search_panel = m;
                    self.after_config_change();
                }
                
                Op::SetThemeMode(mode) => {
                    // 主题必须**运行时立即生效**：记录选择 + 重灌调色板。
                    //
                    // CAUTION: 必须同时更新 `App.pal`（自绘层每帧从它取色）
                    // 与主窗口 `Visuals`（egui 控件层）。`retint` 两件事
                    // 一起做；设置窗口自己的 `Visuals` 由 `settingswin` 每帧
                    // 从 `App.pal` 同步，这里不用管。
                    self.svc.state.config.ui.dark_mode = mode.to_dark_mode();
                    // 补间切换（F-34 / UI-P1-3）与解析链重跑都在 `retint`
                    // 里——主题的每一层变化（一级 / 强调色 / 二级 / 参数 /
                    // 覆盖）共用同一条路径，分头写必然漂移。
                    self.retint();
                    self.after_config_change();
                }
                Op::SetAccent(accent) => {
                    // 强调色：只改配置 + 重跑解析链。accent 一族由
                    // `theme::derive_accent` 派生（含 hover/active/选中底），
                    // 背景与文字层不动。
                    self.svc.state.config.ui.accent = accent;
                    self.retint();
                    self.after_config_change();
                }
                Op::SetSecondaryTheme(theme) => {
                    // 二级主题：整包换风格层（深浅两套变体由解析链按当前
                    // 一级模式选取，见 `theme::apply_secondary`）。
                    self.svc.state.config.ui.secondary_theme = theme;
                    self.retint();
                    self.after_config_change();
                }
                Op::SetThemeParams(params) => {
                    // 二级主题参数：两个旋钮可能同帧连改，整包替换。
                    self.svc.state.config.ui.theme_params = params;
                    self.retint();
                    self.after_config_change();
                }
                Op::SetTokenOverride { name, value } => {
                    // 令牌覆盖：按值类型分表写（颜色 → `colors`，数值 →
                    // `nums`）。时长令牌不可覆盖（编辑器不暴露，见
                    // `draw_token_overrides`）——真收到了就忽略。
                    let ov = &mut self.svc.state.config.ui.theme_overrides;
                    match value {
                        crate::theme::TokenValue::Color(c) => {
                            // CAUTION: 存**非预乘** RGBA（`to_srgba_unmultiplied`
                            // 的逆）——配置是给人看的 JSON，预乘值存进去，
                            // 用户手改时看到的不是他认为的那个颜色。
                            let [r, g, b, a] = c.to_srgba_unmultiplied();
                            ov.colors.insert(name, [r, g, b, a]);
                        }
                        crate::theme::TokenValue::Num(n) => {
                            ov.nums.insert(name, n);
                        }
                        crate::theme::TokenValue::Dur(_) => {}
                    }
                    self.retint();
                    self.after_config_change();
                }
                Op::ClearTokenOverride(name) => {
                    let ov = &mut self.svc.state.config.ui.theme_overrides;
                    ov.colors.remove(&name);
                    ov.nums.remove(&name);
                    self.retint();
                    self.after_config_change();
                }
                Op::ClearAllTokenOverrides => {
                    let ov = &mut self.svc.state.config.ui.theme_overrides;
                    ov.colors.clear();
                    ov.nums.clear();
                    self.retint();
                    self.after_config_change();
                }
                Op::StartRecording(slot) => {
                    self.paint_state.hotkey_recording = Some(slot);
                    // 清这一槽位的草稿与上一次的候选组合：录制覆盖层
                    // 不画输入框，旧文本/旧候选会以「幽灵状态」冒出来。
                    slot.draft_mut(&mut self.paint_state).clear();
                    self.paint_state.recording_combo = None;
                    // 先把已注册的组合从 OS 手里收回来，否则用户按到
                    // 「当前正绑着」的组合时按键被送走，录不到。
                    self.set_hotkeys_suspended(true);
                    tracing::info!(?slot, "开始录制快捷键");
                }
                Op::FinishRecording => {
                    // 录制覆盖层上点击任意位置 = 确认。有候选组合才落地，
                    // 没有（用户什么都没按就点了）等同取消。
                    //
                    // CAUTION: 这里**不能** `return`：函数尾部的「搜索文本
                    // 同步到服务层」与「行操作清选中」还没跑。录制确认与
                    // 搜索词改动完全可能出现在同一帧（用户在搜索弹窗里
                    // 按 Esc 关面板 ⇒ 先落 FinishRecording 那一支），
                    // 早前的 `return` 会让搜索词**这一帧不同步**——虽然
                    // 下一帧会补上，但中间那一帧列表是按旧词筛的。
                    if let Some(slot) = self.paint_state.hotkey_recording.take() {
                        match self.paint_state.recording_combo.take() {
                            Some(hotkey) => {
                                tracing::info!(?slot, keys = %hotkey.display(), "录制确认");
                                self.apply_hotkey(slot, hotkey);
                            }
                            None => {
                                tracing::debug!(?slot, "录制未捕获到组合，视为取消");
                                self.set_hotkeys_suspended(false);
                            }
                        }
                    }
                }
                Op::SetHotkeyText { slot, text } => {
                    match modular_clipboard_core::config::Hotkey::parse(&text) {
                        Ok(hotkey) => {
                            match slot {
                                crate::paint::HotkeySlot::Toggle => {
                                    self.svc.state.config.hotkey_toggle = hotkey;
                                }
                                crate::paint::HotkeySlot::PastePrevious => {
                                    self.svc.state.config.hotkey_paste_previous = hotkey;
                                }
                            }
                            slot.draft_mut(&mut self.paint_state).clear();
                            self.after_config_change();
                            self.svc.notify(format!("{} {text}", modular_clipboard_core::i18n::t(("已设为", "Set to"))));
                        }
                        Err(e) => {
                            // 解析失败要说清楚**为什么**：只提示「无效」
                            // 用户就得自己猜是不是键名写错了。
                            self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("快捷键无效：", "Invalid shortcut:"))));
                        }
                    }
                }
                Op::SetAutostart(on) => {
                    // 先做、再改配置：注册表写不进去时界面不该假装开了。
                    match modular_clipboard_platform::autostart::set_enabled(
                        on,
                        modular_clipboard_platform::autostart::LAUNCH_ARGS,
                    ) {
                        Ok(()) => {
                            self.svc.state.config.ui.autostart = on;
                            self.after_config_change();
                            self.svc.notify(if on {
                                "已设置开机自启动（开机后从托盘唤起）".into()
                            } else {
                                "已取消开机自启动".into()
                            });
                        }
                        Err(e) => {
                            tracing::error!(%e, "设置开机自启动失败");
                            self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("设置失败：", "Failed to apply:"))));
                        }
                    }
                }
                Op::SetHotkey { slot, hotkey } => {
                    // 写入配置并落盘；录制确认也走同一条落地路径。
                    self.apply_hotkey(slot, hotkey);
                }
                Op::Select(id) => {
                    self.svc.state.selected = Some(id);
                }
                Op::TogglePin(id) => {
                    if let Err(e) = self.svc.toggle_pin(id) {
                        self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("置顶失败:", "Pin failed:"))));
                    }
                }
                Op::Delete(id) => {
                    match self.svc.delete_item(id) {
                        Ok(()) => {
                            // 条目没了 ⇒ 挂在它上面的编辑态必须一起收
                            // （内联编辑器 / 图片编辑窗口 / 撤销备份）。
                            // 不收的话「切行提交」会拿残留缓冲去写一个
                            // 已经不存在的 id：store 静默 0 行变更，
                            // 用户以为自己刚改的字保存过了。
                            self.paint_state.clear_edit_state(id);
                        }
                        Err(e) => self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("删除失败:", "Delete failed:")))),
                    }
                }
                Op::Copy(id) => {
                    // 成功也要反馈：旧实现只在失败时 notify，用户点「复制」
                    // 成功时界面**毫无变化」——剪贴板管理器最核心的动作
                    // 反而最没有确认感（2026-10-20 用户反馈「复制没反应」，
                    // 排查后发现是成功路径无反馈）。提示走 toast/弹窗通道
                    // （`ui.notice_popup` 决定呈现），3 秒自消。
                    match self.svc.copy_item(id) {
                        Ok(()) => self.svc.notify(modular_clipboard_core::i18n::t(("已复制", "Copied")).into()),
                        Err(e) => self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("复制失败:", "Copy failed:")))),
                    }
                }
                // ---- 内容编辑（2026-10-21）----
                Op::BeginEdit(id) => {
                    // 读**完整载荷**（不是 preview——那最多 512 字符），
                    // 编辑器里要看到并能改全文。失败（载荷被淘汰 / IPC
                    // 故障）时不进入编辑态：开一个空编辑器比打不开更糟。
                    match self.svc.load_payload_bytes(id) {
                        Ok(bytes) => {
                            let text = String::from_utf8_lossy(&bytes).to_string();
                            self.paint_state.editing = Some(id);
                            self.paint_state.edit_buf = text;
                            self.paint_state.edit_focus_pending = true;
                            // 选中它：编辑中的行应保持选中态（否则行内
                            // Enter 复制等键盘流的对象是别的条目）。
                            self.svc.state.selected = Some(id);
                        }
                        Err(e) => {
                            self.svc
                                .notify(modular_clipboard_core::i18n::t(("无法编辑：内容已被淘汰（{e}）", "Cannot edit: content was evicted ({e})")).replace("{e}", &e.to_string()));
                        }
                    }
                }
                Op::CommitEditText { id, text } => {
                    // 空内容**拒绝保存**而不是落一条空条目：清空后回车
                    // 是最容易误触的一条路径（全选 + 删除 + Enter），
                    // 存进去的是一个既没有预览也复制不出东西的占位行，
                    // 而用户真正的意图多数是「删掉这条」。
                    //
                    // 判据用 `trim()`：纯空白同样没有内容（换行/制表符
                    // 会被预览折叠成看不见的东西，比空还难排查）。
                    //
                    // 拒绝时**保留编辑态**：编辑器继续开着，用户要么补
                    // 字要么按 Esc 退出——直接关掉会让他以为保存成功了。
                    // 真要删条目请走行上的删除按钮（那里有明确反馈）。
                    if text.trim().is_empty() {
                        self.svc.notify(modular_clipboard_core::i18n::t(("内容为空，未保存", "Empty content, not saved")).into());
                    } else {
                        self.commit_edit(id, text.into_bytes(), None);
                    }
                }
                Op::CommitEditImage { id, bytes, dims } => {
                    self.commit_edit(id, bytes, Some(dims));
                }
                Op::CancelEdit => {
                    self.paint_state.editing = None;
                    self.paint_state.edit_buf.clear();
                    self.paint_state.edit_focus_pending = false;
                }
                Op::UndoEdit(id) => {
                    // 单级撤销：用编辑前的备份再走一次落地。备份不存在
                    // （原文已被淘汰 / 从未编辑过）时忽略并通知——
                    // 静默失败会让用户以为撤销过了。
                    //
                    // CAUTION: 不能 `return`——`apply_ops` 返回 bool
                    // （本帧是否有变化），提前返回会把它后面的搜索同步
                    // 等收尾逻辑整段跳过。用 if/else 结构表达。
                    let backup = self.paint_state.edit_backup.clone();
                    match backup {
                        Some(bk) if bk.id == id => {
                            self.commit_edit(id, bk.payload, bk.dims);
                            // 撤销后清掉备份：按钮随之消失（再点一次
                            // 没有意义——备份已是当前内容）。
                            self.paint_state.edit_backup = None;
                        }
                        _ => {
                            self.svc.notify(modular_clipboard_core::i18n::t(("没有可撤销的修改", "Nothing to undo")).into());
                        }
                    }
                }
                Op::OpenImageEditor(id) => {
                    // 读载荷 → 开编辑窗口。尺寸取自内存条目（`payload.dims`）：
                    // 载荷是裸 RGBA8，字节里没有宽高（arboard 链路的老问题）。
                    let dims = self
                        .svc
                        .state
                        .items
                        .iter()
                        .find(|it| it.id == id)
                        .and_then(|it| it.payload.as_ref().and_then(|p| p.dims));
                    match (self.svc.load_payload_bytes(id), dims) {
                        (Ok(bytes), Some(d)) if d.0 > 0 && d.1 > 0 => {
                            self.paint_state.image_editor = Some(id);
                            // 原图交给帧循环建窗（建窗即取走）。
                            self.paint_state.image_editor_src = Some((bytes.clone(), d.0, d.1));
                            // 顺手记下原文备份：窗口保存时直接用，不必
                            // 再读一次盘（编辑窗口本来就持有原图）。
                            self.paint_state.edit_backup = Some(crate::paint::EditBackup {
                                id,
                                kind: modular_clipboard_core::ClipKind::Image,
                                payload: bytes,
                                dims: Some(d),
                            });
                        }
                        (Ok(_), _) => {
                            self.svc
                                .notify(modular_clipboard_core::i18n::t(("无法编辑图片：缺少尺寸信息", "Cannot edit image: missing dimensions")).into());
                        }
                        (Err(e), _) => {
                            self.svc
                                .notify(modular_clipboard_core::i18n::t(("无法编辑图片：内容已被淘汰（{e}）", "Cannot edit image: content was evicted ({e})")).replace("{e}", &e.to_string()));
                        }
                    }
                }
                Op::CloseImageEditor => {
                    self.paint_state.image_editor = None;
                    self.paint_state.image_editor_src = None;
                }
                Op::SetPastePreviousMode(m) => {
                    // 与 SetPinnedMode 同款：重复点当前项不触发写入。
                    if self.svc.state.config.paste_previous_mode != m {
                        self.svc.state.config.paste_previous_mode = m;
                        if let Err(e) = self.svc.save_config() {
                            tracing::warn!(%e, "保存粘贴模式失败");
                        }
                    }
                }
                Op::SetPinnedMode(mode) => {
                    // 重复点当前已选项不触发任何调整（`draw_segmented`
                    // 对已选项也会上报 clicked）。不判这一下的话，
                    // 「共用单栏」状态下点一下已选项就会把用户手动
                    // 拉宽的窗口打回默认宽。
                    if self.ws.pinned_mode() != mode && self.ws.set_pinned_mode(mode) {
                        // 栏数变了 ⇒ 宽度需求变了。窗口尺寸只在启动时
                        // 从配置读过一次，不跟着变的话 `solve_scaled`
                        // 会按比例压缩卡片，用户看到的是「卡片被挤扁」。
                        //
                        // 切到「独立分栏」时额外用**舒适宽度**兜底：
                        // needed_width 只按 min_width 算（两栏约 340pt），
                        // 默认窗口 520pt 会被判「够宽」而一动不动——
                        // 用户要的是「切换后自动放宽以显示更多内容」。
                        // 这条请求**只放大不收缩**（手动拉宽过的窗口
                        // 不会被改小）。
                        //
                        // 切回「共用单栏」则**恢复默认宽度**：切出去时
                        // 窗口被自动撑宽（并写回了配置），只按 needed
                        // 判断的话「够宽就不动」，回来时窗口停在撑宽的
                        // 宽度上——用户看到的是「切了个模式窗口变宽了
                        // 且回不去」。
                        //
                        // 这里只**记下需求**，真正的 resize 留给帧循环：
                        // `apply_ops` 拿不到 `ppp`（DPI 换算要用），
                        // 且窗口对象不在 `self` 里。
                        let request = if mode == crate::card::PinnedMode::OwnCard {
                            let needed = crate::solver::needed_width(&self.ws);
                            let comfortable = crate::solver::comfortable_width(&self.ws);
                            WidthRequest::AtLeast(needed.max(comfortable))
                        } else {
                            WidthRequest::RestoreDefault
                        };
                        self.paint_state.resize_width_to = Some(request);
                    }
                }
                Op::ToggleSettings => {
                    self.paint_state.show_settings = !self.paint_state.show_settings;
                }
                Op::DetailImageZoom(id) => {
                    // 进入原图模式——绘制层下一帧直接走 `draw_detail_image_zoom`。
                    // 幂等：已是该 id 的 zoom 不必再清 orig（缓存仍然有效）。
                    if self.paint_state.detail_image_zoom != Some(id) {
                        self.paint_state.detail_image_zoom = Some(id);
                        self.paint_state.detail_image_orig = None;
                    }
                }
                Op::DetailImageZoomClear => {
                    self.paint_state.detail_image_zoom = None;
                    self.paint_state.detail_image_orig = None;
                }
                Op::ClearAll => {
                    match self.svc.clear_all() {
                        Ok(()) => {
                            // 全清之后没有一条条目还在 ⇒ 编辑态整体作废
                            // （理由同 `Op::Delete` 分支）。
                            self.paint_state.clear_all_edit_state();
                            // 成功也给通知：长按 5 秒是重操作，用户需要
                            // 明确的「生效了」反馈——没有反馈时会以为
                            // 「没反应」而反复重试（2026-10-22 反馈链）。
                            self.svc.notify(modular_clipboard_core::i18n::t((
                                "已清空全部历史",
                                "All history cleared",
                            )).to_string());
                        }
                        Err(e) => self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("清空失败:", "Clear failed:")))),
                    }
                }
                Op::Quit => {
                    // 只**置标志**，真正退出由帧循环消费。
                    //
                    // CAUTION: 不能在这里直接 `std::process::exit`：那会跳过
                    // 收尾（停捕获、`save_config`、销毁 Vulkan 设备、
                    // 托盘 `shutdown`），配置里的改动会丢。
                    tracing::info!("设置界面请求退出程序");
                    self.should_quit = true;
                }
                Op::OpenLink(url) => {
                    // 关于页的项目 / 作者主页：用系统默认浏览器打开。
                    //
                    // 失败必须告知用户，与下一支 `Op::OpenUrl` 同款处置。
                    // 两者是同一个动作（打开 URL）的两条入口，只是一条
                    // 读设置里的自定义浏览器路径；早前只有 `OpenUrl`
                    // 通知，于是「关于页的链接打不开」彻底静默——而
                    // 打不开的原因（默认浏览器未关联 / 被策略禁用）
                    // 恰恰是用户最需要知道的那类。
                    if let Err(e) = modular_clipboard_platform::open_url(url) {
                        tracing::warn!("打开链接失败 {url}: {e}");
                        self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("打开链接失败:", "Failed to open link:"))));
                    }
                }
                Op::OpenUrl(url) => {
                    // 双击链接型条目：尊重设置里的自定义浏览器路径
                    // （留空 = 系统默认浏览器）。失败必须告知用户——
                    // 双击没反应是最难自查的一类故障。
                    let custom = self.svc.state.config.ui.browser_path.trim();
                    if let Err(e) = modular_clipboard_platform::open_url_with(&url, custom) {
                        self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("打开链接失败:", "Failed to open link:"))));
                    }
                }
                Op::OpenDataDir => {
                    // 关于页「数据目录」超链接：进入目录（不是定位选中）。
                    let dir = self.svc.data_dir().display().to_string();
                    let custom = self.svc.state.config.ui.explorer_path.trim();
                    if let Err(e) = modular_clipboard_platform::open_dir_with(&dir, custom) {
                        self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("打开资源管理器失败:", "Failed to open the file manager:"))));
                    }
                }
                Op::RevealPath(path) => {
                    // 双击文件型条目：尊重设置里的自定义资源管理器路径
                    // （留空 = 系统 explorer /select 定位）。
                    let custom = self.svc.state.config.ui.explorer_path.trim();
                    if let Err(e) = modular_clipboard_platform::reveal_with(&path, custom) {
                        self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("打开资源管理器失败:", "Failed to open the file manager:"))));
                    }
                }
                Op::SetTypeFilterKind { kind, multi } => {
                    // 类型过滤是界面本地状态：只改 paint_state，列表由
                    // 绘制层按它筛行。切换规则（单选替换 / 多选增删 /
                    // 两种模式互斥）收在 UiState::toggle_type_filter。
                    let before = (
                        self.paint_state.type_filter,
                        self.paint_state.type_filter_multi.clone(),
                    );
                    self.paint_state.toggle_type_filter(kind, multi);
                    let after = (
                        self.paint_state.type_filter,
                        self.paint_state.type_filter_multi.clone(),
                    );
                    // 真的变了才清选中（与「任意操作取消选中」同一语义：
                    // 过滤让「哪一行被选中」失去意义）。多选模式下点一个
                    // 已选项 = 取消它，也算变化。
                    if before != after {
                        self.svc.state.selected = None;
                    }
                }
                // 窗口级按钮只**置标志**，由帧循环消费。
                //
                // CAUTION: 不能在这里直接关窗口：`apply_ops`  borrows `self`，
                // 而关窗口要动 `gfx::Window`（在 `run()` 的栈上，不在 self
                // 里），且此刻帧循环还持有窗口。置标志、把动作留给
                // 帧循环，是唯一不违反借用规则的顺序。
                Op::CloseWindow => {
                    self.paint_state.close_requested = true;
                }
            }
        }

        // 搜索文本同步到服务层。
        //
        // IMPORTANT: 必须比较**内容**而不是「非空」：早前版本写成
        // `if !q.is_empty() || !svc.state.query.is_empty()`，
        // 结果用户删掉全部文本（query 变空）时条件不成立，
        // 过滤永远解不掉——列表卡在「无结果」状态。
        //
        // 只在内容真的变化时调 `search()`：它内部会 `reload_list()`
        // 重新查库，每帧调一次是无谓的 IO。
        if self.paint_state.query != self.svc.state.query {
            let q = self.paint_state.query.clone();
            // 搜索词变化同样清选中（与「任意操作取消选中」同一语义：
            // 过滤让「哪一行被选中」失去意义）。
            self.svc.state.selected = None;
            self.svc.search(q);
            changed = true;
        }
        if row_op {
            self.svc.state.selected = None;
        }
        changed
    }

    /// 编辑落地：备份原文 → 写新内容 → 关编辑器 → 通知（2026-10-21）。
    ///
    /// # 为什么备份放在这里而不是调用方
    ///
    /// 「撤销」需要的是**改之前**的载荷，而备份动作与落地动作必须严格
    /// 先读后写。三个入口（内联文本 / 图片窗口 / 撤销还原）都走本方法，
    /// 备份逻辑只有一份——分散到三处必然有一处漏掉读旧值，撤销时
    /// 还原出来的是编辑后的内容（「撤销了但没变化」）。
    ///
    /// # 备份失败不阻断编辑
    ///
    /// 原文载荷可能已被淘汰（`load_payload_bytes` 失败）：此时用户改的
    /// 字照样保存，只是没有撤销可用（左下角不出现撤销按钮）。为一个
    /// 已丢失原文的条目拒绝保存更糟。
    ///
    /// 备份已存在且同 id 时**不重读**（图片编辑窗口打开时已备份过）：
    /// 少一次 IPC 往返，也保证备份的一定是「本次编辑前」那一份。
    fn commit_edit(&mut self, id: modular_clipboard_core::EntryId, payload: Vec<u8>, dims: Option<(u32, u32)>) {
        // kind：图片编辑（带 dims）⇒ Image；文本编辑沿用条目当前 kind
        // （Html / Markdown 编辑后仍是那一类，只是内容变了）。
        let kind = if dims.is_some() {
            modular_clipboard_core::ClipKind::Image
        } else {
            self.svc
                .state
                .items
                .iter()
                .find(|it| it.id == id)
                .map(|it| it.kind)
                .unwrap_or(modular_clipboard_core::ClipKind::Text)
        };
        let has_fresh_backup = self
            .paint_state
            .edit_backup
            .as_ref()
            .is_some_and(|b| b.id == id);
        if !has_fresh_backup {
            // 先读后写：此刻库里还是**旧**内容。
            if let Ok(old_bytes) = self.svc.load_payload_bytes(id) {
                let (old_kind, old_dims) = self
                    .svc
                    .state
                    .items
                    .iter()
                    .find(|it| it.id == id)
                    .map(|it| (it.kind, it.payload.as_ref().and_then(|p| p.dims)))
                    .unwrap_or((kind, None));
                self.paint_state.edit_backup = Some(crate::paint::EditBackup {
                    id,
                    kind: old_kind,
                    payload: old_bytes,
                    dims: old_dims,
                });
            }
        }
        match self.svc.update_content(id, kind, payload, dims) {
            Ok(()) => {
                // 关掉一切编辑态：内联编辑器、图片编辑窗口。
                self.paint_state.editing = None;
                self.paint_state.edit_buf.clear();
                self.paint_state.edit_focus_pending = false;
                self.paint_state.image_editor = None;
                self.paint_state.image_editor_src = None;
                self.svc.notify(modular_clipboard_core::i18n::t(("已保存修改", "Changes saved")).into());
            }
            Err(e) => {
                self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("保存失败:", "Save failed:"))));
            }
        }
    }

    /// 画一个悬浮弹窗的内容（`crate::popup` 调用）。
    ///
    /// 与 [`Self::draw_child_frame`] 同一套路：把 `paint::Frame` 的字段
    /// 组装收敛在 app.rs，`popup` 不需要知道 `App` 的内部结构。    ///
    /// 弹窗客户区 = 面板 + 四周一圈投影边距（`popup::SHADOW_MARGIN`），
    /// 面板矩形取中央那块；投影由 `paint` 画在边距里（alpha=0 处即桌面）。
    pub fn draw_popup(&mut self, ui: &mut egui::Ui, kind: crate::popup::PopupKind) {
        let ppp = ui.ctx().pixels_per_point();
        let margin = crate::popup::SHADOW_MARGIN;
        let client = ui.max_rect();
        let mut panel = egui::Rect::from_min_max(
            egui::pos2(client.min.x + margin, client.min.y + margin),
            egui::pos2(client.max.x - margin, client.max.y - margin),
        );
        // ---- 入场动效（2026-10-17）：面板从停靠方向滑入 6pt --------------
        //
        // 弹窗窗口跨展开/收起**存活**（`Popup::sync_and_render` 只切
        // `set_visible`），ctx 里的补间条目不会自己重置 ⇒ 只有程序生命周期
        // 内第一次展开有动画。重播由 `sync_and_render` 的 `just_opened`
        // 清掉 `("popup_in", kind)` 触发（见那里的说明）。
        //
        // 只做位移、不淡内容：弹窗画布透明，半透明文字会直接压到桌面上，
        // 160ms 的可读性风险不值得（观感增强不许牺牲可读性）。投影与
        // 面板用同一个矩形，两者同步位移。
        let k = crate::anim::fade_in_once(
            ui.ctx(),
            egui::Id::unique(("popup_in", kind)),
            std::time::Duration::from_millis(160),
            crate::anim::Ease::OutCubic,
        );
        if k < 1.0 {
            use modular_clipboard_core::config::TypeRailPos;
            let slide = (1.0 - k) * 6.0;
            let dir = match kind {
                // 搜索面板贴在窗口**上方**：从更上方落下。
                // 停靠下方时从更下方升起（`search_panel.pos`）。
                crate::popup::PopupKind::Search => {
                    if self.svc.state.config.ui.search_panel.pos
                        == modular_clipboard_core::config::SearchPanelPos::Top
                    {
                        egui::vec2(0.0, -slide)
                    } else {
                        egui::vec2(0.0, slide)
                    }
                }
                // 类型栏按停靠边：从那一侧的外沿滑入。
                crate::popup::PopupKind::Rail => match self.svc.state.config.ui.type_rail.pos {
                    TypeRailPos::Right => egui::vec2(slide, 0.0),
                    TypeRailPos::Left => egui::vec2(-slide, 0.0),
                    TypeRailPos::Top => egui::vec2(0.0, -slide),
                    TypeRailPos::Bottom => egui::vec2(0.0, slide),
                },
                // 通知弹窗固定在窗口上方：与 toast 同向（上方落下）。
                crate::popup::PopupKind::Notice => egui::vec2(0.0, -slide),
            };
            panel = panel.translate(dir);
        }
        // ---- 没有磨砂底图 ----
        //
        // 弹窗一律**不透明**面板：它不走主窗口那套 GPU 磨砂通道（原因见
        // `crate::popup::Popup::sync_and_render`），因此这里也没有底图可画。
        // 背景层不能降 alpha——降了就只剩一圈描边、内部全透。
        let pal_ref: &crate::theme::Palette = &self.pal;
        // ---- 自研投影（画在**客户区级** painter 上）----
        //
        // 窗口 = 内容 + 一圈 `SHADOW_MARGIN`，那圈是透明的；高斯投影画在
        // 内容矩形外围、向外渐隐到全透明 ⇒ 不需要 DWM，也不需要抓桌面。
        // 用 `painter_at(panel)` 会把投影裁掉一圈，所以这里必须用
        // `ui.painter()`。
        crate::paint::draw_popup_shadow(ui.painter(), panel, pal_ref);
        // CAUTION: 类型列表必须在构造 `frame`（借走 `&mut self.svc`）**之前**
        // 算好——frame 持有 svc 的可变借用，再去读同一份配置就是二借用。
        let rail_kinds = crate::paint::FloatLayout::kinds(
            &self.svc.state.config.ui,
            &self.svc.state.items,
        );
        let mut frame = crate::paint::Frame {
            ui,
            state: &mut self.paint_state,
            svc: &mut self.svc,
            ws: &self.ws,
            pal: pal_ref,
            scale: ppp,
            area: panel,
            card_id: None,
            hit_ns: crate::paint::HitNs::Popup(kind),
        };
        match kind {
            crate::popup::PopupKind::Search => {
                crate::paint::draw_search_panel(&mut frame, panel);
                // 搜索面板把「输入框焦点 / 指针是否在面板上」回报给主窗口：
                // 焦点住在**弹窗自己的** `Context` 里，主窗口的
                // `memory()` 恒为空——不回报的话状态机认为「没人持有焦点」，
                // 面板会在用户点进输入框的瞬间自己收回去。
                self.paint_state.search_popup_focus = frame
                    .state
                    .search_edit_id
                    .is_some_and(|id| ui.memory(|m| m.focused()) == Some(id));
            }
            crate::popup::PopupKind::Rail => {
                crate::paint::draw_type_rail(&mut frame, panel, &rail_kinds);
            }
            crate::popup::PopupKind::Notice => {
                // 通知弹窗：与窗内 toast 同一则通知、同一套节奏常量
                // （见 `paint::notice::draw_notice_panel`）。
                crate::paint::draw_notice_panel(&mut frame, panel);
            }
        }
        // 弹窗里的操作（点类型栏按钮、清搜索、Esc）也要落地。
        //
        // IMPORTANT: 搜索面板打字 / 点类型过滤按钮都改的是**主窗口**那份
        // `paint_state`（搜索词、类型过滤），列表由主窗口绘制。弹窗有自己的
        // `Context` 与事件队列 ⇒ 主窗口收不到事件、也不请求重绘，渲染闸门
        // 会整帧跳过 ⇒ **「在搜索框里打字，列表不跟着筛」**。返回 `true` 时
        // 必须对主 `Context` 请求重绘。
        if self.apply_ops() {
            self.ctx.request_repaint();
        }
        // ---- 补间续帧（弹窗闸门不统计 anim）------------------------------
        //
        // `RedrawGate` 的判据里没有「有补间在飞」这一项（主窗口闸门有），
        // 而入场滑移、类型栏悬停/激活、清除按钮悬停全是自绘补间 ⇒ 不主动
        // 预约帧的话，动画会在第一帧后卡住，等下一次鼠标事件才跳变。
        //
        // `request_repaint()` 必须在绘制闭包内调用：egui 只在 pass 结束
        // 时把它写进 `FullOutput`（repaint_delay = ZERO），闸门读的正是
        // 这个值。busy 为假时什么都不请求——动画落定后闸门自然回到
        // 10Hz 心跳。
        if crate::anim::busy(ui.ctx()) {
            ui.ctx().request_repaint();
        }
    }

    /// 画一个卡片子窗口的内容。
    ///
    /// 与 [`Self::draw_frame`] 分开而不是加参数：`draw_frame` 还要跑
    /// 主窗口专属的逻辑（`svc.pump()`、拖动区上报、操作落地），
    /// 那些对子窗口没有意义——子窗口只是一张卡片的容器。
    pub fn draw_child_frame(&mut self, ui: &mut egui::Ui, card_id: crate::card::CardId) {
        let area = ui.max_rect();
        let ppp = ui.ctx().pixels_per_point();
        let mut frame = crate::paint::Frame {
            ui,
            state: &mut self.paint_state,
            svc: &mut self.svc,
            ws: &self.ws,
            pal: &self.pal,
            scale: ppp,
            area,
            card_id: Some(card_id),
            hit_ns: crate::paint::HitNs::Child(card_id),
        };
        crate::paint::draw_child(&mut frame);
        // 子窗口里的操作（点叉收回、点拖动条收回）也要落地。
        //
        // IMPORTANT: 返回 `true` 必须对**主窗口**请求重绘——子窗口有自己的
        // `Context` 与事件队列，它改了 `ws`/`paint_state` 之后主窗口收不到
        // 任何事件，渲染闸门会跳过整帧 ⇒ 画面停在旧状态（用户看到的是
        // 「点了收回但卡片还在」）。
        if self.apply_ops() {
            self.ctx.request_repaint();
        }
    }

    /// 画设置窗口的一帧（由 [`crate::settingswin`] 调用）。
    ///
    /// 与 [`Self::draw_child_frame`] 同一套路：把 `paint::Frame` 的
    /// 字段组装收敛在这里，`settingswin` 不需要知道 `App` 的内部结构。
    ///
    /// IMPORTANT: 设置窗口里的 `apply_ops` 必须在**之后**跑：改配置要先落盘
    /// （`after_config_change`），而落盘时机取决于绘制是否已完成。
    pub fn draw_settings_window(&mut self, ui: &mut egui::Ui) {
        let area = ui.max_rect();
        let ppp = ui.ctx().pixels_per_point();
        let mut frame = crate::paint::Frame {
            ui,
            state: &mut self.paint_state,
            svc: &mut self.svc,
            ws: &self.ws,
            pal: &self.pal,
            scale: ppp,
            area,
            card_id: None,
            hit_ns: crate::paint::HitNs::Settings,
        };
        crate::paint::draw_settings(&mut frame);

        // 录制快捷键可能是在**设置窗口**里发起的：按键事件落在
        // 这个窗口自己的 egui 上下文里，主窗口那边读的是另一个
        // 上下文，看不到。两边各读一次，谁来键谁来录。
        let ctx = ui.ctx().clone();
        self.capture_recording(&ctx);

        // 设置项的改动要立刻落盘——用户在设置窗口里点了开关就
        // 关掉窗口，不该丢。
        //
        // IMPORTANT: 返回 `true` 时必须对**主窗口**请求重绘。设置窗改了
        // 配置（主题、字号、模糊强度、置顶…）之后，主窗口既收不到事件、
        // 也不请求重绘，渲染闸门会整帧跳过 ⇒ 主窗口停在旧配置的画面上。
        // 闸门里只比对了 `always_on_top` 与 `window_backdrop` 两个字段，
        // 其余配置项全靠这条兜住。
        if self.apply_ops() {
            self.ctx.request_repaint();
        }
    }

    /// 设置窗口打开时调用：设置界面状态归零。
    ///
    /// 每次打开都从「常规」分类、空搜索、非最近修改视图开始；
    /// 配合 `settingswin` 首帧丢弃积压输入，杜绝打开瞬间被
    /// 「幽灵点击」改掉分类的竞态。
    pub(crate) fn reset_settings_view(&mut self) {
        self.paint_state.settings_cat = crate::paint::SettingsCat::General;
        self.paint_state.settings_query.clear();
        self.paint_state.settings_show_recent = false;
    }

    /// 本帧的上报拖动区。由外壳每帧读一次并转给窗口层。
    pub fn drag_regions(&self) -> &[egui::Rect] {
        // 新架构的拖动区由 `paint::topbar::draw_topbar` 每帧写入。
        // 旧的 `local.drag_region` 已不再更新——保留读旧字段会拿到
        // 永远为 None 的值，表现为「窗口拖不动」。
        //
        // IMPORTANT: 是**多段**：顶栏里的齿轮、垃圾桶、搜索框必须从拖动区里
        // 挖空，否则它们会被系统判成标题栏（`HTCAPTION`）→
        // 不产生 `WM_LBUTTONDOWN` → egui 收不到点击。
        &self.paint_state.drag_regions
    }

    /// 取出并清掉「标题栏关闭按钮」请求。
    ///
    /// 读的是 [`crate::paint::UiState`] 上的标志，由 `Op::CloseWindow`
    /// 写入。**不是**旧的 `local`——那个字段只由已删除的 `view::draw`
    /// 写入，永远是false，于是标题栏关闭按钮点了没反应。
    pub fn take_close_requested(&mut self) -> bool {
        std::mem::take(&mut self.paint_state.close_requested)
    }

    /// 是否收到退出请求。
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// 当前生效的字体缩放倍率（已钳制到合法区间）。
    ///
    /// `UiConfig::font_scale` 是**唯一**数据源；钳制走
    /// [`theme::clamp_font_scale`]，与写入侧同一份实现——两处各写一份
    /// 的话，配置被手工改成 5.0 时读取侧会照单全收，字号直接爆掉。
    fn font_scale(&self) -> f32 {
        theme::clamp_font_scale(self.svc.state.config.ui.font_scale)
    }

    /// 让字体缩放**立即**生效（重算显示调色板 + 灌入 egui）。
    ///
    /// # 为什么不像主题那样走补间
    ///
    /// 字号变化会让每段文字重新排版：走 220ms 渐变意味着这 13 帧里
    /// 文字逐帧换字号，观感是「抖」而不是「淡入」。所以这里直接跳到
    /// 目标值。
    ///
    /// # 为什么要清掉主题补间
    ///
    /// 若此刻正有一段主题补间在飞，它每帧会把 `self.pal` 覆盖成
    /// `pal_from.lerp(pal_to, mix)`——其中不含新的字号，刚设的字号
    /// 下一帧就被抹掉。清掉状态后帧循环的 `tween_f32` 会以
    /// from == to == 1.0 重新插入（惰性），不再覆盖。
    fn apply_font_scale(&mut self) {
        self.pal = self.pal_base.with_font_scale(self.font_scale());
        self.pal_from = self.pal.clone();
        self.pal_to = self.pal.clone();
        crate::anim::clear(&self.ctx, egui::Id::unique("pal_mix"));
        crate::theme::apply(&self.ctx, &self.pal);
        tracing::info!(font_scale = self.font_scale(), "字体缩放已应用");
    }

    /// 取出「布局变化后需要的客户区宽度」请求。
    ///
    /// 返回 `Some(w)` 表示帧循环应当把窗口设为 `w`（物理像素）。
    /// `None` 表示不需要动。
    ///
    /// CAUTION: 必须**按请求语义**判断，不能无脑设成目标宽度——
    /// `AtLeast` 无脑设的话每次切换模式窗口都会跳一下，来回切两次
    /// 就晃（见 [`resize_width_target`]）。
    pub fn take_resize_width_to(&mut self, current_px: (u32, u32), scale: f32) -> Option<u32> {
        resize_width_target(self.paint_state.resize_width_to.take(), current_px, scale)
    }

    /// 清空全部历史（托盘菜单「清空历史」入口）。
    ///
    /// 与界面上的「清空」按钮走同一个 [`Service::clear_all`]，
    /// 保证两条入口的行为完全一致——包括失败时的提示方式。
    pub fn clear_history(&mut self) -> anyhow::Result<()> {
        self.svc.clear_all()
    }

    /// 把主窗口当前位置写回配置。
    ///
    /// 由 [`Self::shutdown`] 调用。**不覆盖**用户设置里已有的
    /// 置顶窗位置——那个由 `childwin` 在关窗时自己写。
    ///
    /// 取不到位置时跳过而不是写 `Some(0,0)`：窗口可能因最小化/
    /// 隐藏而没有有效位置，写进去会让下次启动跑到屏幕左上角。
    ///
    /// `App` **不持有窗口**（窗口在 `run()` 的栈上），所以位置由
    /// 调用方从窗口句柄读出后注入。
    ///
    /// 返回 `true` 表示已写入配置；坐标无效（最小化 `-32000`、非有限值）
    /// 时返回 `false` 且不写入——调用方据此决定是否推进防抖计时。
    pub fn persist_window_pos(&mut self, p: egui::epaint::emath::Vec2) -> bool {
        // CAUTION: 必须拒绝**无效坐标**，否则下次启动窗口跑到屏幕外。
        //
        // `screen_position_points()` 走 `GetWindowRect`，而它对
        // **最小化**的窗口返回的是任务栏图标的矩形 —— 典型值
        // `(-32000, -32000)`。托盘「退出」时若窗口正处于最小化，
        // 这个值就被写进配置，下次启动 `move_to(-32000, …)` ⇒
        // 窗口在屏幕外，用户只看到托盘图标，主界面「不见了」。
        //
        if !is_sane_window_pos(p) {
            tracing::debug!(?p, "窗口坐标无效（很可能处于最小化），不写回配置");
            return false;
        }
        self.svc.state.config.ui.window_pos = Some(WindowPos::new(p.x, p.y));
        true
    }

    /// 配置改动后的收尾：立刻落盘。
    ///
    /// 设置界面里点一下就写文件——用户不该担心「关掉程序才生效」。
    /// 写失败只记日志：设置面板不该因为磁盘问题卡住。
    fn after_config_change(&mut self) {
        if let Err(e) = self.svc.save_config() {
            tracing::warn!(%e, "保存配置失败");
            self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t(("保存设置失败:", "Failed to save settings:"))));
        }
    }

    /// 重跑主题解析链，并走 220ms 补间切换到新调色板。
    ///
    /// 主题的**每一层**变化都走这里：一级模式（[`Op::SetThemeMode`]）、
    /// 强调色（[`Op::SetAccent`]）、二级主题（[`Op::SetSecondaryTheme`]）、
    /// 二级主题参数（[`Op::SetThemeParams`]）、令牌覆盖
    /// （[`Op::SetTokenOverride`] 一族）。五者共一条路径是刻意的：
    /// 解析链只有一份（`theme::resolve_palette`）、补间机制只有一份
    /// （`pal_mix`）——分头写必然漂移，2026-10-14 的「切主题没反应」
    /// 就是这么来的（只灌了 `Visuals`，没更新 `App.pal`）。
    ///
    /// # 为什么强调色 / 二级主题也要补间
    ///
    /// 整窗瞬跳在纯平毛玻璃审美下刺眼（背景模糊内容不变、全部前景色
    /// 瞬换，见 F-34）。换主题色与换二级主题是同一类「大面积变色」，
    /// 没有理由一个渐变一个硬切。
    fn retint(&mut self) {
        let mode = crate::theme::ThemeMode::from_dark_mode(self.svc.state.config.ui.dark_mode);
        let system = crate::theme::detect_system_theme();
        let effective = crate::theme::resolve(mode, system);
        // 解析链：一级模式 → 二级主题 → 强调色 → 令牌覆盖。
        // 字体缩放不在这条链里（幂等派生，见 `theme::resolve_palette`）。
        let base = crate::theme::resolve_palette(effective, &self.svc.state.config.ui);
        let scale = self.font_scale();
        // 灌**缩放版**：`apply` 的入参就是每帧自绘用的那一份，
        // 与初始化处（`App::new` 末尾的 `theme::apply`）保持同一状态。
        // 帧循环在补间期间与落定帧还会再灌一次（`draw_frame` 的
        // `pal_mix` 分支），这里是让「变化当帧」的 Visuals 就正确。
        crate::theme::apply(&self.ctx, &base.with_font_scale(scale));
        self.pal_base = base;
        // `pal_from` 取**当前显示值**而非上一次的目标：连续切换时
        // 从当下续接，不会跳。
        self.pal_from = self.pal.clone();
        self.pal_to = self.pal_base.with_font_scale(scale);
        // CAUTION（「主题切换失效」的根因，2026-10-14）：`pal_mix` 这个
        // 补间的目标恒为 1.0，而 `anim::tween_f32` 在目标不变时是
        // **惰性**的——首帧插入即 from == to == 1.0，之后永远返回 1.0，
        // `draw_frame` 里 `if pal_mix < 1.0` 的分支再也不会执行
        // ⇒ `self.pal` 从不更新。于是 Visuals 变了而自绘层停在旧主题。
        // 修法：切换时显式 `anim::restart` 把进度重置为 0→1。
        crate::anim::restart(
            &self.ctx,
            egui::Id::unique("pal_mix"),
            0.0,
            1.0,
            crate::theme::PALETTE_TWEEN,
            crate::anim::Ease::InOutCubic,
        );
    }

    /// 当前生效调色板。
    ///
    /// `settingswin` 每帧用它同步设置窗口自己的 `Visuals`——设置窗口
    /// 有独立的 `egui::Context`，`set_theme` 只灌主窗口那份。
    pub(crate) fn palette(&self) -> &theme::Palette {
        &self.pal
    }

    /// 发一条通知（toast / 弹窗，由 `ui.notice_popup` 决定呈现）。
    ///
    /// `pub(crate)` 是给**编辑窗口**（`imgedit`）用的：它有自己的绘制
    /// 代码但够不到 `App::svc`（私有）。失败反馈必须走与主窗口同一通道
    /// ——「保存失败只在日志里」对用户等于没有反馈。
    pub(crate) fn notify(&mut self, msg: String) {
        self.svc.notify(msg);
    }

    /// MCP 调用设置覆盖层是否打开。
    ///
    /// `settingswin` 用它决定这一帧要不要走「自研 GPU 模糊」绘制路径：
    /// 覆盖层打开时，设置窗口自身的内容要先卷积成模糊底图。
    pub(crate) fn mcp_overlay_open(&self) -> bool {
        self.paint_state.mcp_overlay_open
    }

    /// 记下置顶窗口位置（供下次启动恢复）。
    pub fn set_pinned_window_pos(&mut self, p: egui::epaint::emath::Vec2) {        if !is_sane_window_pos(p) {
            tracing::debug!(?p, "置顶窗口坐标无效（很可能处于最小化），不写回配置");
            return;
        }
        self.svc.state.config.ui.pinned_window_pos = Some(WindowPos::new(p.x, p.y));
    }

    pub fn shutdown(&mut self) {
        // 远端模式**不能**停捕获：捕获线程在常驻进程里，
        // 界面关掉只是「不看了」，不该把后台记录也停掉——
        // 那会让用户「关掉窗口 = 剪贴板不再记录」，与常驻的初衷相反。
        if !self.svc.is_remote() {
            self.svc.stop_capture();
        }
        // 旧版这里还调`persist_layout()` 把 `LayoutState` 写回
        // `config.ui.layout`。那套布局连同 `layout.rs` 已删除，
        // 保留只会把一份没人读的旧格式配置原样写回磁盘。
        if let Err(e) = self.svc.save_config() {
            tracing::warn!(%e, "退出时保存配置失败");
        }
    }
}

/// 改一个布尔配置字段。
///
/// # 为什么用「分组 + 字段名字符串」而不是每项一个 `Op` 变体
///
/// 配置项会随版本增删（现在 20+ 项）。若每个都加一个 `Op` 变体，
/// `apply_ops` 会膨胀成几十个分支，且新增项要改三处。
/// 字符串字段名把新增成本降到「paint 里多一行 + 这里多一行」。
///
/// 代价是**失去编译期检查**——字段名写错会静默无效。
/// [`tests::unknown_config_field_is_ignored`] 守住「拼错不 panic」。
fn set_bool_field(cfg: &mut modular_clipboard_core::Config, g: crate::paint::ConfigGroup, field: &str, v: bool) {
    use crate::paint::ConfigGroup as G;
    match (g, field) {
        (G::Capture, "enabled") => cfg.capture.enabled = v,
        (G::Capture, "track_source_app") => cfg.capture.track_source_app = v,
        (G::Capture, "skip_password_fields") => cfg.capture.skip_password_fields = v,
        (G::Capture, "dedup") => cfg.capture.dedup = v,
        (G::Ui, "always_on_top") => cfg.ui.always_on_top = v,
        (G::Ui, "hide_on_focus_lost") => cfg.ui.hide_on_focus_lost = v,
        (G::Ui, "show_tray") => cfg.ui.show_tray = v,
        (G::Ui, "start_minimized") => cfg.ui.start_minimized = v,
        // ---- 第三轮新增（图片直显 / Markdown 渲染） ----
        (G::Ui, "image_inline") => cfg.ui.image_inline = v,
        (G::Ui, "md_rendered") => cfg.ui.md_rendered = v,
        // 通知呈现：弹窗（true）/ 窗口内 toast（false）。
        (G::Ui, "notice_popup") => cfg.ui.notice_popup = v,
        // NOTE: type_rail 的参数走 `Op::SetTypeRail` 整包落地，
        // 没有按字段名的写入方。
    (G::Ui, "autostart") => cfg.ui.autostart = v,
    (G::MiniPanel, "enabled") => cfg.mini_panel.enabled = v,
    (G::MiniPanel, "ctrl") => cfg.mini_panel.ctrl = v,
    (G::MiniPanel, "shift") => cfg.mini_panel.shift = v,
    (G::MiniPanel, "alt") => cfg.mini_panel.alt = v,
    (G::MiniPanel, "win") => cfg.mini_panel.win = v,
        (G::Storage, "cleanup_on_start") => cfg.storage.cleanup_on_start = v,
        (G::Mcp, "enabled") => cfg.mcp.enabled = v,
        (G::Mcp, "resize_window") => cfg.mcp.resize_window = v,
        (G::Mcp, "read_layout") => cfg.mcp.read_layout = v,
        (G::Mcp, "click_element") => cfg.mcp.click_element = v,
        (G::Mcp, "type_text") => cfg.mcp.type_text = v,
        (G::Mcp, "scroll_view") => cfg.mcp.scroll_view = v,
        (G::Mcp, "screen_capture") => cfg.mcp.screen_capture = v,
        (G::Mcp, "window_focus") => cfg.mcp.window_focus = v,
        (G::Mcp, "drag_drop") => cfg.mcp.drag_drop = v,
        _ => tracing::warn!(?g, field, "未知的布尔配置字段，已忽略"),
    }
}

/// 改一个数值配置字段。
fn set_number_field(
    cfg: &mut modular_clipboard_core::Config,
    g: crate::paint::ConfigGroup,
    field: &str,
    v: f64,
) {
    use crate::paint::ConfigGroup as G;
    match (g, field) {
        (G::Capture, "poll_interval_ms") => cfg.capture.poll_interval_ms = v.max(50.0) as u64,
        (G::Capture, "dedup_window_secs") => cfg.capture.dedup_window_secs = v.max(0.0) as i64,
        (G::Ui, "font_scale") => cfg.ui.font_scale = (v.clamp(0.75, 2.0)) as f32,
        (G::MiniPanel, "count") => cfg.mini_panel.count = (v.clamp(1.0, 10.0)) as usize,
    (G::MiniPanel, "preview_chars") => {
        cfg.mini_panel.preview_chars = (v.clamp(10.0, 100.0)) as usize
    }
        (G::Storage, "max_bytes") => cfg.storage.max_bytes = v.max(1.0) as u64,
        (G::Storage, "max_payload_bytes") => cfg.storage.max_payload_bytes = v.max(1.0) as u64,
        (G::Mcp, "port") => cfg.mcp.port = v.clamp(0.0, 65535.0) as u16,
        _ => tracing::warn!(?g, field, "未知的数值配置字段，已忽略"),
    }
}

/// 改一个文本配置字段（MCP 服务地址 / 密钥）。
fn set_text_field(cfg: &mut modular_clipboard_core::Config, g: crate::paint::ConfigGroup, field: &str, v: &str) {
    use crate::paint::ConfigGroup as G;
    match (g, field) {
        (G::Mcp, "host") => cfg.mcp.host = v.trim().to_string(),
        (G::Mcp, "token") => cfg.mcp.token = v.trim().to_string(),
        // 第三轮：双击跳转的调用路径。**不 trim 掉内部空格**——Windows
        // 路径合法地包含空格（`C:\Program Files\...`），只去首尾。
        (G::Ui, "browser_path") => cfg.ui.browser_path = v.trim().to_string(),
        (G::Ui, "explorer_path") => cfg.ui.explorer_path = v.trim().to_string(),
        _ => tracing::warn!(?g, field, "未知的文本配置字段，已忽略"),
    }
}

/// 改一个可选数值配置字段（`None` = 不限）。
fn set_opt_number_field(
    cfg: &mut modular_clipboard_core::Config,
    g: crate::paint::ConfigGroup,
    field: &str,
    v: Option<f64>,
) {
    use crate::paint::ConfigGroup as G;
    match (g, field) {
        (G::Storage, "max_items") => {
            cfg.storage.max_items = v.map(|x| x.max(1.0) as usize);
        }
        _ => tracing::warn!(?g, field, "未知的可选数值配置字段，已忽略"),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

/// 窗口宽度调整请求（`Op::SetPinnedMode` 写入，帧循环消费）。
///
/// 两种语义分开是因为「切出去」和「切回来」要的行为**相反**：
/// 无脑一个「目标宽度」会把这两种情况都变成「总是设成它」。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WidthRequest {
    /// 至少放宽到这么多**逻辑点**。当前够宽时不动（只放大不收缩）。
    ///
    /// 用于切到「独立分栏」：多出一栏要宽度，而用户手动拉宽过的
    /// 窗口不该被改小。
    AtLeast(f32),
    /// 精确恢复**默认宽度**（物理像素）。
    ///
    /// 用于切回「共用单栏」：独立分栏会自动把窗口撑宽（并写回配置），
    /// 切回来若沿用「够宽就不动」，窗口就永远停在撑宽的宽度上。
    /// 默认宽度取自 [`modular_clipboard_core::config::UiConfig::default`]，
    /// 不写死数字——配置默认值变化时「恢复默认」跟着变。
    RestoreDefault,
}

/// 主窗口默认宽度（物理像素）。
///
/// 单一数据源是 `UiConfig::default().ui.window_width`。
pub fn default_window_width_px() -> u32 {
    modular_clipboard_core::config::UiConfig::default().window_width as u32
}

/// 算出「该把窗口宽度设成多少物理像素」，不该动就返回 `None`。
///
/// 抽成纯函数是为了能直接测：`take_resize_width_to` 需要 `&mut App`，
/// 而构造 App 要 config + 临时数据目录，放进单元测试太重。
///
/// 语义见 [`WidthRequest`]。`scale` 只对 `AtLeast` 有用（逻辑点 →
/// 物理像素）；`RestoreDefault` 的目标本身就是物理像素。
fn resize_width_target(
    request: Option<WidthRequest>,
    current_px: (u32, u32),
    scale: f32,
) -> Option<u32> {
    let request = request?;
    match request {
        WidthRequest::AtLeast(needed) => {
            if needed <= 0.0 || scale <= 0.0 {
                return None;
            }
            let current_points = current_px.0 as f32 / scale;
            // 已经够宽就不动。
            //
            // 留 1pt 容差：浮点误差会让「刚好等于」时在两个分支间反复跳，
            // 表现为每次切模式窗口都抖一下。
            if current_points >= needed - 1.0 {
                return None;
            }
            Some(((needed * scale).round() as u32).max(1))
        }
        WidthRequest::RestoreDefault => {
            let target = default_window_width_px().max(1);
            // 已经就是默认宽度（1px 容差）则不动：省掉一次
            // `set_size_physical` + 交换链重建。
            if current_px.0.abs_diff(target) <= 1 {
                return None;
            }
            Some(target)
        }
    }
}

#[cfg(test)]
mod resize_tests {
    use super::{resize_width_target, WidthRequest};

    /// 放不下时必须给出目标物理宽度。
    ///
    /// 这条直接对应用户的反馈：「置顶模式选择后主窗口大小不会自己调整」
    /// —— 切到独立分栏多出一栏，需求超过可用宽，`solve_scaled`
    /// 就会按比例把卡片压扁。
    #[test]
    fn widens_when_current_is_too_narrow() {
        // 需求 600 逻辑点，当前只有 400 逻辑点（= 600 物理像素 @1.5）
        let target = resize_width_target(Some(WidthRequest::AtLeast(600.0)), (600, 800), 1.5);
        assert_eq!(target, Some(900), "应放宽到 600×1.5 = 900 物理像素");
    }

    /// 已经够宽时**不动**窗口。
    ///
    /// 这条守的是「只放大不收缩」：用户可能故意把窗口开到很大，
    /// 不该因为一次模式切换被改回来。无脑设成 `needed` 的话，
    /// 来回切两次模式窗口就会来回跳。
    #[test]
    fn leaves_window_alone_when_already_wide_enough() {
        assert_eq!(
            resize_width_target(Some(WidthRequest::AtLeast(400.0)), (900, 800), 1.5),
            None
        );
        // 刚好等于也要算「够宽」（1pt 容差内），否则浮点误差会抖。
        assert_eq!(
            resize_width_target(Some(WidthRequest::AtLeast(400.0)), (600, 800), 1.5),
            None
        );
    }

    /// 切回共用单栏：宽度**恢复默认**（物理像素），不管当前多宽。
    ///
    /// 用户反馈：「置顶模式切换回共用单栏时，宽度恢复为默认」。
    /// 独立分栏会自动撑宽窗口（并写回配置），切回来若按「够宽就不动」
    /// 判断，窗口就停在撑宽的宽度上回不去。
    #[test]
    fn restores_default_width_when_back_to_shared_column() {
        // 当前被独立分栏撑宽到 1180 物理像素（如 1.5x 下的舒适宽度）。
        let target = resize_width_target(Some(WidthRequest::RestoreDefault), (1180, 800), 1.5);
        assert_eq!(
            target,
            Some(super::default_window_width_px()),
            "切回共用单栏必须恢复默认宽度"
        );
        // 比默认窄时同样恢复到默认（篱它不负责「只放大」）。
        let narrow = resize_width_target(Some(WidthRequest::RestoreDefault), (400, 800), 1.0);
        assert_eq!(narrow, Some(super::default_window_width_px()));
    }

    /// 已经是默认宽度时不动（省一次 resize + 交换链重建）。
    #[test]
    fn restore_default_is_noop_at_default_width() {
        let d = super::default_window_width_px();
        assert_eq!(
            resize_width_target(Some(WidthRequest::RestoreDefault), (d, 800), 1.0),
            None
        );
        // 1px 容差内也算「已经是默认」。
        assert_eq!(
            resize_width_target(Some(WidthRequest::RestoreDefault), (d + 1, 800), 1.0),
            None
        );
    }

    /// 没有请求、或参数非法时不动。
    #[test]
    fn no_request_means_no_resize() {
        assert_eq!(resize_width_target(None, (300, 800), 1.5), None);
        assert_eq!(
            resize_width_target(Some(WidthRequest::AtLeast(0.0)), (300, 800), 1.5),
            None
        );
        assert_eq!(
            resize_width_target(Some(WidthRequest::AtLeast(-50.0)), (300, 800), 1.5),
            None
        );
        // scale = 0 会除出 inf / 除零，必须挡住。
        assert_eq!(
            resize_width_target(Some(WidthRequest::AtLeast(600.0)), (300, 800), 0.0),
            None
        );
    }

    /// `RestoreDefault` 不依赖 `scale`（目标本就是物理像素）——
    /// 防的是「将来有人给它也乘了 scale」这种静默错误。
    #[test]
    fn restore_default_ignores_scale() {
        let a = resize_width_target(Some(WidthRequest::RestoreDefault), (1180, 800), 1.0);
        let b = resize_width_target(Some(WidthRequest::RestoreDefault), (1180, 800), 1.5);
        assert_eq!(a, b, "恢复默认与 DPI 缩放无关");
    }

    /// 结果必须是**正**整数：0 会让客户区塌成 0×0，
    /// 下一帧的交换链重建拿不到可呈现的图像，窗口看着像消失了。
    ///
    /// 场景取「极小需求 × 极小缩放」—— 正常布局（需求数百点、
    /// 缩放 1.0~2.0）永远走不到这里，这是纯防御性分支：
    /// `3.0 × 0.1 = 0.3` 四舍五入就是 0，靠 `.max(1)` 兜住。
    #[test]
    fn target_is_never_zero() {
        let t = resize_width_target(Some(WidthRequest::AtLeast(3.0)), (0, 10), 0.1)
            .expect("应给出目标宽度");
        assert!(t >= 1, "目标宽度必须至少 1 像素，实际 {t}");
    }
}

#[cfg(test)]
mod tests {

    /// 背景层实度必须随「主题 × 背景明暗冲突」双向调整。
    ///
    /// 这是磨砂/半透明**可读性**的唯一判据（浅色主题 + 纯黑桌面是最
    /// 常见的崩坏场景），且它是纯函数——不测就等于把「文字看不看得清」
    /// 交给运气。
    #[test]
    fn backdrop_alpha_adapts_to_theme_background_mismatch() {
        // 浅色主题 + 纯黑背景：冲突最大，背景层要被推到接近上限。
        let light_on_black = backdrop_alpha(0.6, Some(0.0), false);
        // 浅色主题 + 纯白背景：同向，保持用户要的通透。
        let light_on_white = backdrop_alpha(0.6, Some(1.0), false);
        assert!(
            light_on_black > light_on_white + 60,
            "浅色配黑底({light_on_black}) 应明显比配白底({light_on_white}) 更实"
        );

        // 暗色主题方向相反：白底才是冲突方。
        let dark_on_white = backdrop_alpha(0.6, Some(1.0), true);
        let dark_on_black = backdrop_alpha(0.6, Some(0.0), true);
        assert!(
            dark_on_white > dark_on_black + 60,
            "暗色配白底({dark_on_white}) 应明显比配黑底({dark_on_black}) 更实"
        );
    }

    /// 上限 0.88（不是 1.0）：全实就成了纯色底，白改一场。
    #[test]
    fn backdrop_alpha_never_reaches_full_opacity() {
        // 基准已经是 1.0 且明暗完全冲突时，仍必须留一点透出。
        let worst = backdrop_alpha(1.0, Some(if false { 0.0 } else { 1.0 }), false);
        assert!(
            worst <= (0.88_f32 * 255.0).round() as u8,
            "最坏情况 {worst} 不该超过 0.88 的 alpha"
        );
    }

    /// 还没捕获到背景（首帧/最小化）时按用户基准走，别凭空变实。
    #[test]
    fn backdrop_alpha_falls_back_to_base_without_capture() {
        let base = 0.5_f32;
        let got = backdrop_alpha(base, None, false);
        assert_eq!(got, (base * 255.0).round() as u8);
    }

    /// 输出恒被钳在 [0.30, 0.88]：越界输入（用户滑块给 2.0 之类）必须
    /// 被吃掉，否则调色板变体拿到越界 alpha。
    #[test]
    fn backdrop_alpha_clamps_out_of_range_input() {
        // 无捕获时下限是基准钳位 0.1。
        let low = backdrop_alpha(2.0, None, false);
        assert!(
            low >= (0.1_f32 * 255.0).round() as u8,
            "base=2.0 无捕获应被钳到基准下限（得到 {low}）"
        );
        // 有捕获时下限抬到 0.30（可读性底线）。
        let with_cap = backdrop_alpha(0.0, Some(0.5), false);
        assert!(
            with_cap >= (0.30_f32 * 255.0).round() as u8,
            "有捕获时下限 0.30（得到 {with_cap}）"
        );
    }

    /// 最小化窗口的坐标必须在**任何常见缩放**下都被拒绝。
    ///
    /// # 为什么这条重要
    ///
    /// `GetWindowRect` 对最小化窗口返回 `(-32000, -32000)` **物理像素**，
    /// 而 `screen_position_points()` 返回的是**逻辑点**（除以 scale）。
    /// 于是 150% 缩放下传入 `-21333.3`。
    ///
    /// 早前判据写 `p.x <= -32000.0` ⇒ **漏过** ⇒ 把屏幕外坐标写进配置
    /// ⇒ 下次启动 `move_to(p.x * scale)` 还原成物理 `-32000`
    /// ⇒ 窗口跑到屏幕外，用户只看到托盘图标。
    #[test]
    fn minimized_window_pos_is_rejected_at_every_scale() {
        // 常见 DPI 缩放：100% / 125% / 150% / 175% / 200%
        for scale in [1.0f32, 1.25, 1.5, 1.75, 2.0] {
            let logical = -32000.0f32 / scale;
            let p = egui::epaint::emath::vec2(logical, logical);
            assert!(
                !is_sane_window_pos(p),
                "scale={scale} 时最小化坐标 {logical} 必须被拒绝                  （判据不能只在 scale==1.0 才生效）"
            );
        }
    }

    /// 真实的多显示器布局（副屏在主屏左侧）不能被误判为无效。
    #[test]
    fn real_negative_positions_are_accepted() {
        // 4K 副屏放在主屏左侧：x 最小约 -3840。
        for p in [
            egui::epaint::emath::vec2(-3840.0, 100.0),
            egui::epaint::emath::vec2(-1920.0, -100.0),
            egui::epaint::emath::vec2(0.0, 0.0),
            egui::epaint::emath::vec2(2560.0, 1440.0),
        ] {
            assert!(is_sane_window_pos(p), "真实坐标不应被拒绝：{p:?}");
        }
    }

    /// 非有限值（NaN / Infinity）必须被拒绝。
    #[test]
    fn non_finite_positions_are_rejected() {
        for p in [
            egui::epaint::emath::vec2(f32::NAN, 0.0),
            egui::epaint::emath::vec2(0.0, f32::NAN),
            egui::epaint::emath::vec2(f32::INFINITY, 0.0),
            egui::epaint::emath::vec2(0.0, f32::NEG_INFINITY),
        ] {
            assert!(!is_sane_window_pos(p), "非有限坐标必须拒绝：{p:?}");
        }
    }

    use super::*;
    use ash::vk;

    /// 预选格式的规则必须与 `modular_clipboard_gfx::pick_format` 一致。
    ///
    /// 两处失配的后果是「渲染通道格式 ≠ 交换链格式」，
    /// 驱动在 `cmd_begin_render_pass` 时崩（本项目的历史坑）。
    /// `FrameRenderer::new` 会校验并 bail，但那时窗口已经建好了——
    /// 早失败比晚失败好，所以这里也钉一道。
    /// 字段名写错时**静默忽略**，不 panic。
    ///
    /// 配置项走「分组 + 字段名字符串」而非每项一个 `Op` 变体，
    /// 代价就是**失去编译期检查**——字段名打错会静默失效。
    /// 这条测试守住「拼错不炸」，同时提醒维护者：
    /// 新增配置项时，`paint` 里的字段名与这里的字符串必须一致。
    #[test]
    fn unknown_config_field_is_ignored() {
        let mut cfg = modular_clipboard_core::Config::default();
        let before = cfg.capture.poll_interval_ms;
        set_number_field(
            &mut cfg,
            crate::paint::ConfigGroup::Capture,
            "不存在的字段",
            12345.0,
        );
        assert_eq!(
            cfg.capture.poll_interval_ms, before,
            "未知字段不应改动任何配置"
        );
    }

    /// 数值字段的越界值必须被夹到合法区间。
    #[test]
    fn numeric_config_fields_are_clamped() {
        let mut cfg = modular_clipboard_core::Config::default();
        // 监听间隔下限 50ms：给 1ms 应被夹到 50。
        set_number_field(&mut cfg, crate::paint::ConfigGroup::Capture, "poll_interval_ms", 1.0);
        assert_eq!(cfg.capture.poll_interval_ms, 50);
        // 字体缩放上限 2.0：给 99 应被夹到 2.0。
        set_number_field(&mut cfg, crate::paint::ConfigGroup::Ui, "font_scale", 99.0);
        assert!((cfg.ui.font_scale - 2.0).abs() < 1e-6);
    }

    /// `max_items = None` 表示「不限」，与 `Some(0)` 不是一回事。
    #[test]
    fn opt_number_supports_unlimited() {
        let mut cfg = modular_clipboard_core::Config::default();
        set_opt_number_field(
            &mut cfg,
            crate::paint::ConfigGroup::Storage,
            "max_items",
            None,
        );
        assert!(
            cfg.storage.max_items.is_none(),
            "None 应表示「不限条数」"
        );
    }

    /// 窗口位置记忆：合法坐标写入并返回 `true`，无效坐标（最小化 /
    /// 非有限值）拒绝并返回 `false`。
    ///
    /// 守着实机缺陷：最小化时 `GetWindowRect` 返回 `(-32000,-32000)`，
    /// 若把它写进配置，下次启动窗口跑到屏幕外。返回 `false` 的语义
    /// 让调用方（主循环实时记忆）**不推进防抖计时**——否则无效坐标
    /// 触发一次写入尝试后，接下来半秒内窗口恢复到合法位置也写不进去。
    #[test]
    fn persist_window_pos_writes_valid_and_rejects_invalid() {
        let dir = std::env::temp_dir().join(format!("mcb-pos-test-{}", std::process::id()));
        let mut app = App::new(modular_clipboard_core::Config::default(), false, Some(&dir));
        let _ = std::fs::remove_dir_all(&dir);

        // 合法坐标 → 写入 + true。
        assert!(app.persist_window_pos(egui::vec2(320.0, 180.0)));
        let saved = app
            .svc
            .state
            .config
            .ui
            .window_pos
            .expect("合法坐标应写入 window_pos");
        assert!(
            (saved.x - 320.0).abs() < 1e-6 && (saved.y - 180.0).abs() < 1e-6,
            "写入的坐标应与入参一致，实际 {saved:?}"
        );

        // 最小化坐标 → 拒绝 + false + 配置不变。
        let before = app.svc.state.config.ui.window_pos;
        assert!(!app.persist_window_pos(egui::vec2(-32000.0, -32000.0)));
        assert_eq!(app.svc.state.config.ui.window_pos, before);

        // 非有限值 → 同样拒绝。
        assert!(!app.persist_window_pos(egui::vec2(f32::NAN, 5.0)));
        assert_eq!(app.svc.state.config.ui.window_pos, before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 设置窗口的「记录剪贴板」开关必须**运行时**启停捕获线程。
    ///
    /// 早期实现只改 `cfg.capture.enabled` 并落盘，从不调
    /// `start_capture`/`stop_capture`——于是运行中打开开关后
    /// 捕获线程仍然不存在，用户复制什么都记录不到（重启才生效），
    /// 表现为「复制监听获取不到、开关改了没反应」。
    #[test]
    fn theme_switch_actually_changes_palette_and_restarts_tween() {
        let dir = std::env::temp_dir().join(format!("mc-ui-theme-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cfg = modular_clipboard_core::Config::default();
        // 固定起点：跟随系统会让断言依赖跑测试的机器。
        cfg.ui.dark_mode = Some(true);
        let mut app = App::new(cfg, false, Some(&dir));
        assert!(app.palette().is_dark, "初始应为深色");

        // 切到浅色。
        app.paint_state.ops.push(crate::paint::Op::SetThemeMode(
            crate::theme::ThemeMode::Light,
        ));
        app.apply_ops();
        assert!(!app.pal_to.is_dark, "目标调色板必须是浅色");
        // 「主题切换失效」的根因是补间从不重启（目标恒为 1.0 ⇒ 惰性）。
        // 重启后它必须在飞——否则下面推进时间也到不了目标。
        assert!(
            crate::anim::busy(&app.ctx),
            "切主题必须重启 pal_mix 补间（否则永远停在旧主题）"
        );

        // 推进超过补间时长：显示中的调色板必须**精确**落到浅色。
        let ctx = app.ctx.clone();
        let mut out = ctx.run_ui(
            egui::RawInput {
                time: Some(10.0),
                ..Default::default()
            },
            |ui| app.draw_frame(ui),
        );
        out.textures_delta.clear();
        assert!(
            !app.palette().is_dark,
            "补间走完后显示调色板必须变成浅色"
        );
        assert_eq!(
            app.palette().surface,
            crate::theme::Palette::light().surface,
            "走完后必须精确落在目标色（不是最后一帧的近似值）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 字体缩放必须**运行时**生效（配置 → 调色板 → egui 三层同源）。
    ///
    /// 守着实机缺陷：`UiConfig::font_scale` 早前**只有写入方、没有读取方**，
    /// 改它完全没反应（用户反馈「字体缩放修改失效」）。
    #[test]
    fn font_scale_takes_effect_at_runtime() {
        let dir = std::env::temp_dir().join(format!("mc-ui-fontscale-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.ui.dark_mode = Some(true);
        cfg.ui.font_scale = 1.0;
        let mut app = App::new(cfg, false, Some(&dir));
        let base_md = crate::theme::Palette::dark().font_md;
        assert!((app.palette().font_md - base_md).abs() < 1e-3, "默认 1.0× 不缩放");

        // 设成 1.5×（预设「超大」）→ 立即生效。
        app.paint_state.ops.push(crate::paint::Op::SetNumber {
            group: crate::paint::ConfigGroup::Ui,
            field: "font_scale",
            value: 1.5,
        });
        app.apply_ops();
        assert!(
            (app.palette().font_md - base_md * 1.5).abs() < 1e-3,
            "字号应放大到 1.5×，实际 {}",
            app.palette().font_md
        );
        // 承载文字的几何**必须**跟着放大：固定 28pt 高的输入框装不下
        // 25.5pt 的字（文字被裁 = 用户反馈的「布局错乱」）。
        assert!(
            (app.palette().control_height
                - crate::theme::Palette::dark().control_height * 1.5)
                .abs()
                < 1e-3,
            "控件高度应随字号放大"
        );
        // 圆角不放大（1.5× 的圆角会让卡片变胶囊）。
        assert!(
            (app.palette().radius_md - crate::theme::Palette::dark().radius_md).abs() < 1e-3
        );

        // 再改一次不能叠乘（1.5 × 1.2 必须是 1.2×，不是 1.8×）——
        // 这正是 `pal_base` 必须单独存一份的理由。
        app.paint_state.ops.push(crate::paint::Op::SetNumber {
            group: crate::paint::ConfigGroup::Ui,
            field: "font_scale",
            value: 1.2,
        });
        app.apply_ops();
        assert!(
            (app.palette().font_md - base_md * 1.2).abs() < 1e-3,
            "第二次缩放必须从基础调色板派生，实际 {}",
            app.palette().font_md
        );

        // 越界值被钳制（不是照单全收）。
        app.paint_state.ops.push(crate::paint::Op::SetNumber {
            group: crate::paint::ConfigGroup::Ui,
            field: "font_scale",
            value: 99.0,
        });
        app.apply_ops();
        assert!(
            (app.palette().font_md - base_md * crate::theme::FONT_SCALE_MAX).abs() < 1e-3,
            "越界应钳到上限"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 切主题不能把用户设的字体缩放打回 1.0×。
    #[test]
    fn theme_switch_preserves_font_scale() {
        let dir = std::env::temp_dir().join(format!("mc-ui-theme-scale-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.ui.dark_mode = Some(true);
        cfg.ui.font_scale = 1.3;
        let mut app = App::new(cfg, false, Some(&dir));
        let base_md = crate::theme::Palette::dark().font_md;
        assert!((app.palette().font_md - base_md * 1.3).abs() < 1e-3);

        app.paint_state.ops.push(crate::paint::Op::SetThemeMode(
            crate::theme::ThemeMode::Light,
        ));
        app.apply_ops();
        assert!(
            (app.pal_to.font_md - base_md * 1.3).abs() < 1e-3,
            "切主题后目标调色板的字号必须仍带 1.3× 缩放，实际 {}",
            app.pal_to.font_md
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 提交**空内容**必须被拒绝：库里原文不动、编辑态保留、给出提示。
    ///
    /// 守的是最容易误触的一条路径——全选 + 删除 + Enter。若照单全收，
    /// 落库的是一个既没有预览也复制不出东西的空条目，而用户真正的意图
    /// 多半是「删掉这条」。
    #[test]
    fn empty_edit_text_is_refused_and_keeps_editor_open() {
        let dir = std::env::temp_dir().join(format!("mc-ui-empty-edit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.capture.enabled = false;
        let mut app = App::new(cfg, false, Some(&dir));
        // `ingest` 的第一道闸是 `capture.enabled`（关着就不再记录），
        // 但开着的 cfg 会让 `App::new` 起一条捕获线程去**真的读系统
        // 剪贴板**（测试里会往列表塞进不可控的条目）。故建完再开：
        // 只为让 ingest 落库，不启动线程。
        app.svc.state.config.capture.enabled = true;

        // 塞一条真实记录：走 `ingest` 才有可读回的载荷。
        let text = "编辑前的原文";
        app.svc.ingest(modular_clipboard_core::CapturedPayload {
            kind: modular_clipboard_core::ClipKind::Text,
            bytes: text.as_bytes().to_vec(),
            html: None,
            text: Some(text.into()),
            source_app: "test".into(),
            files: Vec::new(),
            dims: None,
        });
        app.svc.reload_list();
        let id = app.svc.state.items[0].id;

        // 纯空白也算空（换行/制表符会被预览折成看不见的东西）。
        let blank = "  \n\t ".to_string();
        app.paint_state.editing = Some(id);
        app.paint_state.edit_buf = blank.clone();
        app.paint_state.ops.push(crate::paint::Op::CommitEditText {
            id,
            text: blank,
        });
        app.apply_ops();

        assert!(
            app.svc
                .state
                .notice
                .as_ref()
                .is_some_and(|(m, _)| m.contains("内容为空")),
            "空内容必须给出提示，实际 {:?}",
            app.svc.state.notice.as_ref().map(|(m, _)| m)
        );
        assert_eq!(
            app.paint_state.editing,
            Some(id),
            "拒绝保存时编辑器应继续开着（关掉会让用户以为保存成功了）"
        );
        let still = app
            .svc
            .load_payload_bytes(id)
            .expect("条目还在，应能读回原文");
        assert_eq!(still, text.as_bytes().to_vec(), "原文不应被空内容覆盖");

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 删除条目必须把它身上的编辑态一起收掉。
    ///
    /// 不收的后果是「切行提交」拿残留缓冲去写一个已经被删掉的 id：
    /// store 静默 0 行变更，界面没有任何报错，用户以为刚改的字存住了。
    #[test]
    fn deleting_item_clears_its_edit_state() {
        let dir = std::env::temp_dir().join(format!("mc-ui-del-edit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.capture.enabled = false;
        let mut app = App::new(cfg, false, Some(&dir));
        // 同 `empty_edit_text_is_refused_and_keeps_editor_open`：只为让
        // ingest 落库而开，不起捕获线程（见那里的说明）。
        app.svc.state.config.capture.enabled = true;

        let payload = |s: &str| modular_clipboard_core::CapturedPayload {
            kind: modular_clipboard_core::ClipKind::Text,
            bytes: s.as_bytes().to_vec(),
            html: None,
            text: Some(s.into()),
            source_app: "test".into(),
            files: Vec::new(),
            dims: None,
        };
        app.svc.ingest(payload("第一条"));
        app.svc.ingest(payload("第二条"));
        app.svc.reload_list();
        let ids: Vec<_> = app.svc.state.items.iter().map(|it| it.id).collect();
        let (victim, keeper) = (ids[0], ids[1]);

        // victim 正在被编辑（含撤销备份），keeper 也在编辑中。
        app.paint_state.editing = Some(keeper);
        app.paint_state.edit_buf = "第二条改过了".into();
        app.paint_state.image_editor = Some(victim);
        app.paint_state.image_editor_src = Some((vec![1, 2, 3], 2, 2));
        app.paint_state.edit_backup = Some(crate::paint::EditBackup {
            id: victim,
            kind: modular_clipboard_core::ClipKind::Text,
            payload: "第一条".as_bytes().to_vec(),
            dims: None,
        });

        app.paint_state.ops.push(crate::paint::Op::Delete(victim));
        app.apply_ops();

        assert!(
            app.svc.state.items.iter().all(|it| it.id != victim),
            "条目应已被删除"
        );
        assert!(
            app.paint_state.image_editor.is_none(),
            "被删条目的图片编辑窗口必须关掉"
        );
        assert!(
            app.paint_state.image_editor_src.is_none(),
            "被删条目的原图副本必须释放"
        );
        assert!(
            app.paint_state.edit_backup.is_none(),
            "被删条目的撤销备份必须丢弃"
        );
        // 另一条的编辑不受影响。
        assert_eq!(
            app.paint_state.editing,
            Some(keeper),
            "删 A 不该打断正在编辑的 B"
        );
        assert_eq!(app.paint_state.edit_buf, "第二条改过了");

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_construction_does_not_touch_locale() {
        // 守门（2026-10-22 连翻两次车）：App 构造链里绝不能解析/设置
        // 全局语言。CI runner 是英文系统，构造时一翻转，所有假定中文
        // 断言/热区 key 的并行测试全线阵亡——而本机中文系统永远复现不到。
        // 语言只允许在生产 UI 入口 `run()` 与 `Op::SetLanguage` 里改。
        modular_clipboard_core::i18n::set_locale(modular_clipboard_core::i18n::Locale::ZhCn);
        let dir = std::env::temp_dir().join(format!("mc-ui-locale-guard-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _app = App::new(modular_clipboard_core::Config::default(), false, Some(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            modular_clipboard_core::i18n::locale(),
            modular_clipboard_core::i18n::Locale::ZhCn,
            "App 构造动了全局语言"
        );
    }

    #[test]
    fn capture_enabled_switch_restarts_thread_at_runtime() {
        let dir = std::env::temp_dir().join(format!("mc-ui-capture-switch-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let mut cfg = modular_clipboard_core::Config::default();
        cfg.capture.enabled = false;
        let mut app = App::new(cfg, false, Some(&dir));
        assert!(
            !app.svc.is_capturing(),
            "capture.enabled=false 启动时不应有捕获线程"
        );

        // 打开「记录剪贴板」→ 线程应立即启动，无需重启。
        app.paint_state.ops.push(crate::paint::Op::SetBool {
            group: crate::paint::ConfigGroup::Capture,
            field: "enabled",
            value: true,
        });
        app.apply_ops();
        assert!(
            app.svc.is_capturing(),
            "打开「记录剪贴板」后应立即启动捕获线程"
        );

        // 关闭 → 线程应立即停止，不再空转。
        app.paint_state.ops.push(crate::paint::Op::SetBool {
            group: crate::paint::ConfigGroup::Capture,
            field: "enabled",
            value: false,
        });
        app.apply_ops();
        assert!(
            !app.svc.is_capturing(),
            "关闭「记录剪贴板」后应立即停止捕获线程"
        );

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 录制：按下的组合键只**记录候选**，不立即应用、也不退出录制态。
    ///
    /// # 为什么用独立 ctx 跑一帧
    ///
    /// 录制按钮在**设置窗口**里，按键事件落在设置窗口自己的
    /// `egui::Context` 上。主窗口的上下文里什么都看不到——
    /// 这正是用户报「录制没反应」的根因形态。这里用独立 ctx
    /// 复刻「设置窗口收到按键」的那一帧。
    ///
    /// # 为什么按一下不算完
    ///
    /// 组合先显示在录制覆盖层上给用户看，点击任意位置确认后才落地
    /// （用户定稿交互）。按一下就改会让手抖直接换掉热键。
    #[test]
    fn recording_records_combo_without_applying() {
        let dir = std::env::temp_dir().join(format!("mc-ui-hk-record-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.capture.enabled = false;
        let mut app = App::new(cfg, false, Some(&dir));
        app.paint_state.hotkey_recording = Some(crate::paint::HotkeySlot::Toggle);

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            time: Some(1.0),
            events: vec![egui::Event::Key {
                key: egui::Key::K,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
            }],
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            // 与 `draw_settings_window` 里同一姿势：绘制之后读本窗口输入。
            app.capture_recording(ui.ctx());
        });
        out.textures_delta.clear();

        // 只记录：录制态保持（覆盖层还开着），配置纹丝不动。
        assert_eq!(
            app.paint_state.hotkey_recording,
            Some(crate::paint::HotkeySlot::Toggle),
            "按一下不算完：录制态要保持到用户点击确认"
        );
        assert!(
            app.paint_state.ops.is_empty(),
            "只记录候选，不该产出任何 Op，实得 {:?}",
            app.paint_state.ops
        );
        let combo = app
            .paint_state
            .recording_combo
            .clone()
            .expect("应按住组合并记录候选");
        assert_eq!(combo.key, "K", "主键应按名原样记录");
        assert!(combo.ctrl && combo.shift, "按住的修饰键应捕获：{combo:?}");
        assert!(!combo.alt && !combo.win, "没按的修饰键不该被带上：{combo:?}");
        // 配置没被碰——应用发生在确认那一刻。
        assert_eq!(
            app.svc.state.config.hotkey_toggle.key, "V",
            "未确认前配置不应被改写"
        );

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 确认录制：`Op::FinishRecording` 把候选组合落进配置并退出录制态。
    #[test]
    fn finish_recording_applies_the_recorded_combo() {
        let dir = std::env::temp_dir().join(format!("mc-ui-hk-finish-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.capture.enabled = false;
        let mut app = App::new(cfg, false, Some(&dir));
        app.paint_state.hotkey_recording = Some(crate::paint::HotkeySlot::Toggle);
        app.paint_state.recording_combo = Some(modular_clipboard_core::config::Hotkey {
            key: "K".into(),
            ctrl: true,
            shift: true,
            alt: false,
            win: false,
        });

        app.paint_state
            .ops
            .push(crate::paint::Op::FinishRecording);
        app.apply_ops();

        assert_eq!(app.paint_state.hotkey_recording, None, "确认后应退出录制态");
        assert!(app.paint_state.recording_combo.is_none(), "候选应被取走");
        let hk = &app.svc.state.config.hotkey_toggle;
        assert_eq!(hk.key, "K");
        assert!(hk.ctrl && hk.shift, "候选组合应落进配置：{hk:?}");

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 什么都没按就点击确认：等同取消——退出录制态但配置不动。
    ///
    /// 覆盖层上「点击任意位置停止录制」，用户点进来发现不想改了，
    /// 直接点一下就该干净退出，不能留下半开的状态。
    #[test]
    fn finish_recording_without_combo_cancels() {
        let dir = std::env::temp_dir().join(format!("mc-ui-hk-nocomb-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.capture.enabled = false;
        let mut app = App::new(cfg, false, Some(&dir));
        let before = app.svc.state.config.hotkey_toggle.clone();
        app.paint_state.hotkey_recording = Some(crate::paint::HotkeySlot::Toggle);

        app.paint_state
            .ops
            .push(crate::paint::Op::FinishRecording);
        app.apply_ops();

        assert_eq!(app.paint_state.hotkey_recording, None, "应退出录制态");
        assert_eq!(
            app.svc.state.config.hotkey_toggle, before,
            "没录到组合时配置不应被改写"
        );

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 录制中按到**修饰键本身**必须被忽略——它不是「主键」。
    ///
    /// egui 0.36 的 `Key` 枚举含 ControlLeft / ShiftLeft / SuperLeft 等
    /// **物理键**变体（eframe 从 winit 映射而来），修饰键的逻辑状态则在
    /// `Event::Key::modifiers` 里，两者都会产生事件。不过滤的后果：
    /// 用户按字母 → 按 Ctrl → 录到的是「ControlLeft」→ 弹
    /// 「不支持把 ControlLeft 设为快捷键」并**退出录制态**，
    /// 后面的组合再也录不进去（2026-10-13 用户实测反馈）。
    #[test]
    fn recording_ignores_modifier_key_presses() {
        let dir = std::env::temp_dir().join(format!("mc-ui-hk-mods-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.capture.enabled = false;
        let mut app = App::new(cfg, false, Some(&dir));
        app.paint_state.hotkey_recording = Some(crate::paint::HotkeySlot::Toggle);

        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        // 跑一帧并注入一个按键事件；返回本帧收集到的 Op。
        let frame = |app: &mut App, ev: egui::Event| {
            let input = egui::RawInput {
                screen_rect: Some(rect),
                time: Some(1.0),
                events: vec![ev],
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                app.capture_recording(ui.ctx());
            });
            out.textures_delta.clear();
            std::mem::take(&mut app.paint_state.ops)
        };
        let key = |key: egui::Key, ctrl: bool, shift: bool| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl,
                shift,
                ..Default::default()
            },
        };

        // 帧 1：Ctrl 物理键（modifiers.ctrl 为真）→ 必须被忽略。
        let ops = frame(&mut app, key(egui::Key::ControlLeft, true, false));
        assert!(
            ops.is_empty(),
            "按修饰键本身不该产出任何 Op，实得 {ops:?}"
        );
        assert_eq!(
            app.paint_state.hotkey_recording,
            Some(crate::paint::HotkeySlot::Toggle),
            "修饰键必须被忽略，录制态不能被打断"
        );

        // 帧 2：Shift 物理键 → 同样忽略。
        let ops = frame(&mut app, key(egui::Key::ShiftRight, true, true));
        assert!(ops.is_empty(), "Shift 物理键同样该被忽略，实得 {ops:?}");
        assert_eq!(
            app.paint_state.hotkey_recording,
            Some(crate::paint::HotkeySlot::Toggle),
        );

        // 帧 3：主键 K（带着 ctrl+shift）→ 记为候选组合（仍不应用）。
        let ops = frame(&mut app, key(egui::Key::K, true, true));
        assert!(ops.is_empty(), "只记录候选，不该产出 Op，实得 {ops:?}");
        let combo = app
            .paint_state
            .recording_combo
            .clone()
            .expect("修饰键之后的主键应被记下");
        assert_eq!(combo.key, "K");
        assert!(combo.ctrl && combo.shift, "修饰键应来自 modifiers：{combo:?}");
        assert_eq!(
            app.paint_state.hotkey_recording,
            Some(crate::paint::HotkeySlot::Toggle),
            "只记录候选：录制态保持到用户点击确认"
        );

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只按主键没按修饰键：拒绝并**保持**录制态（用户可能正在按过程中）。
    ///
    /// 若这里直接退出录制，用户「先按主键再补修饰键」的自然顺序
    /// 会录不到东西——而反过来先修饰键后主键才是多数人的习惯，
    /// 两条路径都得通。
    #[test]
    fn recording_rejects_bare_key_and_stays_in_recording() {
        let dir = std::env::temp_dir().join(format!("mc-ui-hk-bare-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut cfg = modular_clipboard_core::Config::default();
        cfg.capture.enabled = false;
        let mut app = App::new(cfg, false, Some(&dir));
        app.paint_state.hotkey_recording = Some(crate::paint::HotkeySlot::Toggle);

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            time: Some(1.0),
            events: vec![egui::Event::Key {
                key: egui::Key::K,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            app.capture_recording(ui.ctx());
        });
        out.textures_delta.clear();

        assert_eq!(
            app.paint_state.hotkey_recording,
            Some(crate::paint::HotkeySlot::Toggle),
            "无修饰键应拒绝但保持录制态"
        );
        assert!(
            app.paint_state.ops.is_empty(),
            "拒绝时不应产出任何 Op，实得 {:?}",
            app.paint_state.ops
        );

        app.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn surface_format_prefers_8bit_bgr_or_rgb() {
        let formats = [
            vk::SurfaceFormatKHR {
                format: vk::Format::B8G8R8A8_SRGB,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
            vk::SurfaceFormatKHR {
                format: vk::Format::B8G8R8A8_UNORM,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
        ];
        // 着色器不做线性化，必须选非 sRGB 变体，否则画面偏暗。
        assert_eq!(pick_from(&formats), Some(vk::Format::B8G8R8A8_UNORM));

        // 单格式时按规范原样采用，哪怕它不在偏好列表里。
        let single = [formats[0]];
        assert_eq!(pick_from(&single), Some(vk::Format::B8G8R8A8_SRGB));

        // 空列表必须报错而不是 panic——否则启动失败时栈回溯会误导方向。
        assert_eq!(pick_from(&[]), None);
    }

    #[test]
    fn no_8bit_format_is_an_error() {
        // 多个候选但全是 HDR 格式时必须报错，而不是随便挑一个——
        // 挑错的后果是渲染通道与交换链格式不一致，驱动在
        // cmd_begin_render_pass 时崩（0xC0000005），而不是这里报错。
        let formats = [
            vk::SurfaceFormatKHR {
                format: vk::Format::R16G16B16A16_SFLOAT,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
            vk::SurfaceFormatKHR {
                format: vk::Format::A2B10G10R10_UNORM_PACK32,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
        ];
        assert_eq!(pick_from(&formats), None);
    }

    #[test]
    fn single_format_is_taken_verbatim_even_if_unusual() {
        // Vulkan 规范要求：表面只报告一个格式时，必须原样采用。
        // 哪怕它是 HDR——总比直接失败好，驱动能渲染就行。
        let single = [vk::SurfaceFormatKHR {
            format: vk::Format::R16G16B16A16_SFLOAT,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        }];
        assert_eq!(pick_from(&single), Some(vk::Format::R16G16B16A16_SFLOAT));
    }

    #[test]
    fn empty_format_list_is_rejected() {
        // 空列表必须报错而不是 panic——否则启动失败时栈回溯会误导方向。
        assert_eq!(pick_from(&[]), None);
    }

    /// 从表面格式列表里选出一个可用格式。空列表/无偏好项返回 `None`。
    ///
    /// 与 `pick_surface_format` 的逻辑一一对应，改一处必须改另一处。
    fn pick_from(formats: &[vk::SurfaceFormatKHR]) -> Option<vk::Format> {
        if formats.is_empty() {
            return None;
        }
        // 单格式时按规范原样采用。
        if formats.len() == 1 {
            return Some(formats[0].format);
        }
        formats
            .iter()
            .map(|f| f.format)
            .find(|f| matches!(*f, vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM))
    }

    /// 浅色主题必须真的把 `dark_mode` 翻过来，且**深字浅底**。
    ///
    /// 这条原来测的是本文件里的 `light_visuals`，那个函数已删——
    /// 配色改由 [`theme::apply`] 从 `Palette` 灌入，手写第二份配色必然
    /// 与令牌表漂移。测试改测真正生效的那条路径。
    ///
    /// 方向曾写反过一次（断言 `lum(text) > lum(panel_fill)`，那是深色
    /// 模式的要求）：浅色模式下若文字比背景亮，那就是白底白字——
    /// 界面能跑但什么都看不见。
    #[test]
    fn light_theme_is_dark_text_on_light_background() {
        let ctx = egui::Context::default();
        let pal = theme::set_theme(&ctx, theme::ThemeMode::Light);
        assert!(!pal.is_dark, "浅色调色板的 is_dark 必须为否");

        // `set_theme` 必须真的把 Visuals 翻成浅色，而不只是返回调色板。
        assert!(
            !ctx.style_of(egui::Theme::Light).visuals.dark_mode,
            "egui Visuals 的 dark_mode 应为浅色"
        );

        let v = ctx.style_of(egui::Theme::Light).visuals.clone();
        let lum = |c: egui::Color32| {
            let s = c.to_srgba_unmultiplied();
            0.299 * f32::from(s[0]) + 0.587 * f32::from(s[1]) + 0.114 * f32::from(s[2])
        };
        let text = v
            .override_text_color
            .unwrap_or(v.widgets.noninteractive.fg_stroke.color);
        assert!(
            lum(text) < lum(v.panel_fill),
            "浅色模式下文字亮度({:.0}) 必须低于背景亮度({:.0})，否则是白底白字",
            lum(text),
            lum(v.panel_fill)
        );
    }

    /// 深浅两套必须真的**不同**——否则「切换」这个动作等于没做。
    ///
    /// 逐个令牌对比，而不是只比`panel_fill`：两个调色板可能背景一致
    /// 但文字/边框不同，只比一个字段会漏。
    #[test]
    fn dark_and_light_palettes_differ() {
        let dark = theme::Palette::dark();
        let light = theme::Palette::light();
        assert!(dark.is_dark && !light.is_dark);
        let differing = dark
            .tokens()
            .iter()
            .zip(light.tokens())
            .filter(|(a, b)| a.0 != b.0 || a.1 != b.1)
            .count();
        assert!(
            differing > 10,
            "深浅两套只有 {differing} 个令牌不同，疑似配色方案被复制"
        );
    }

    /// `ThemeMode` 与 `UiConfig::dark_mode` 的双向映射必须闭合。
    ///
    /// 接线层全靠这一对转换来回传值，不闭合就会「配置写深色、界面亮色」。
    #[test]
    fn theme_mode_roundtrips_through_dark_mode() {
        for m in [
            theme::ThemeMode::Dark,
            theme::ThemeMode::Light,
            theme::ThemeMode::FollowSystem,
        ] {
            let back = theme::ThemeMode::from_dark_mode(m.to_dark_mode());
            // FollowSystem 会先被系统主题解析成 Dark/Light，
            // 因此断言的是「解析后幂等」而不是「原样返回」。
            let resolved = theme::resolve(back, theme::ThemeMode::Dark);
            assert_eq!(
                theme::resolve(m, theme::ThemeMode::Dark),
                resolved,
                "{m:?} 经 dark_mode 往返后变了"
            );
        }
        // 显式映射不能含糊。
        assert_eq!(
            theme::ThemeMode::from_dark_mode(Some(true)),
            theme::ThemeMode::Dark
        );
        assert_eq!(
            theme::ThemeMode::from_dark_mode(Some(false)),
            theme::ThemeMode::Light
        );
        assert_eq!(
            theme::ThemeMode::from_dark_mode(None),
            theme::ThemeMode::FollowSystem
        );
    }
}
