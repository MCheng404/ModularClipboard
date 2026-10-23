//! 帧循环：`run_mode` 与附属窗口驱动。
//!
//! `run_mode` 组装窗口 + GPU + 管线 + 托盘后进入主循环；
//! 循环内的编号小节（0 事件 / 1 状态 / 2 业务 UI / 3 尺寸 /
//! 4 渲染）是既有的分段注释，改动前先读它们。

use modular_clipboard_app::Service;
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::frame::FrameRenderer;
use modular_clipboard_gfx::frame::PipelineBundle;
use modular_clipboard_gfx::frame::PresentResult;
use modular_clipboard_gfx::window::EventLoop;
use modular_clipboard_gfx::window::Window;
use modular_clipboard_gfx::window::WindowEvent;
use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
use crate::presence::CloseDecision;
use crate::presence::Resident;
use crate::presence;
use crate::renderer::Painter;
use crate::renderer::WINDOW_TITLE;

use super::{App};

use super::boot::DEFAULT_CLEAR_COLOR;
use super::boot::DeltaGuard;
use super::boot::TRANSPARENT_CLEAR_COLOR;
use super::boot::TRAY_TOOLTIP;
use super::boot::env_flag;

/// 两种模式共用的帧循环。
///
/// `remote` 为真时界面不建托盘、关闭窗口即退出进程，
/// 其余（渲染、绘制、事件）完全一致。
pub(super) fn run_mode(
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
                    app.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Clear failed:")));
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
                        app.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Paste failed:")));
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
                    app.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Copy failed:")));
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
pub(super) fn drive_attach_windows<'a>(
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
                        app.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Cannot open the image editor:")));
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
pub(super) fn rebuild<'a>(
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
