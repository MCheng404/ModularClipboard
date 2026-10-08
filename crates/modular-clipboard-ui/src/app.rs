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

use crate::presence::{self, CloseDecision, Resident};
use crate::renderer::{Painter, WINDOW_TITLE};
use crate::theme;

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
struct DeltaGuard(egui::TexturesDelta);

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
const TRAY_TOOLTIP: &str = "模块化剪切板 — 正在后台记录剪贴板";

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
    // ⚠️ 先取 `show_tray` 快照：`config` 后面会被移动进 `App::new`，
    // 到那时就访问不到了。
    let want_tray = config.ui.show_tray;

    // `Window::new` 的宽高是**物理像素**，而配置里存的是逻辑点。
    // DPI 缩放在窗口创建后才可查，因此先按 1.0 换算——创建后第一帧的
    // `Resized` 事件（或帧循环里的尺寸兜底检查）会把交换链纠正到真实尺寸。
    let window = Window::new(
        WINDOW_TITLE,
        config.ui.window_width.max(1.0) as u32,
        config.ui.window_height.max(1.0) as u32,
    )?;
    // 恢复上次退出时的窗口位置。
    //
    // ⚠️ 早前位置只存在内存里，每次启动都回屏幕左上角——
    // 用户拖动的位置在重启后丢失。位置存`UiConfig::window_pos`。
    //
    // `Option` 是必要的：`0.0` 是合法坐标（屏幕左上角），
    // 不能用它当「未设置」的哨兵值。
    if let Some(p) = config.ui.window_pos {
        let s = window.scale_factor();
        window.move_to((p.x * s) as i32, (p.y * s) as i32);
    }
    // ⚠️ `CreateWindowExW` 创建的窗口**默认不可见**，
    // 必须显式 `ShowWindow` 才会出现在桌面上。
    //
    // 实测踩坑：早前这里从不调用它，窗口**从不显示**——
    // 实机启动 `modular-clipboard.exe` 后 `MainWindowTitle` 为空，
    // 用户只看到托盘图标，会以为程序没启动。
    //
    // `start_minimized` 配置项此前**从未被读取**（字段存在于
    // `UiConfig` 但代码里grep 不到），现已接上。
    // 必须显式显示窗口：`CreateWindowExW` 创建的窗口**默认不可见**。
    //
    // 实测踩坑：早前这里从不显示，窗口**从不出现**——
    // 启动 `modular-clipboard.exe` 后 `MainWindowTitle` 为空，
    // 用户只看到托盘图标，会以为程序没启动。
    //
    // 用 `presence::focus_window` 而非裸 ShowWindow：它已处理
    // Windows 的前台窗口限制（`SetForegroundWindow` 失败时
    // 走 `AttachThreadInput`），托盘点击唤起也需要它。
    //
    presence::focus_window(window.hwnd());

    let mut events = EventLoop::new(&window);
    tracing::info!(
        width = window.inner_size_physical().0,
        height = window.inner_size_physical().1,
        scale = window.scale_factor(),
        "窗口已创建"
    );

    // ---- Vulkan 设备 ----------------------------------------------------
    let gpu = Gpu::new(WINDOW_TITLE, window.hinstance(), window.hwnd())?;

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
    let shared = crate::multiwindow::SharedGfx::create(&gpu)?;

    // ---- egui -----------------------------------------------------------
    let mut app = App::new(config, capture_is_override, data_dir);
    let ctx = app.ctx.clone();

    // 恢复上次退出时的置顶窗口位置。
    //
    // ⚠️ 必须在第一次 `sync_with` 之前：那个函数按卡片上的
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
    // ⚠️ 必须在托盘启动**之前**登记——否则用户在这段时间内右键
    // 托盘图标，`TrackPopupMenu` 会挂在隐藏的托盘辅助窗口上，
    // 菜单闪一下就消失。
    modular_clipboard_platform::tray::set_main_hwnd(window.hwnd().0 as isize);

    // ⚠️ 必须尊重 `ui.show_tray` —— 设置界面里那个开关。
    //
    // 早前这里无条件 `Resident::start`，于是 `show_tray` 只被**写入**、
    // 从不被读取：用户在设置里关掉「显示托盘图标」，托盘照样在；
    // 开着也无所谓。配置项形同虚设。
    let resident = if want_tray {
        match Resident::start(TRAY_TOOLTIP) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::error!(%e, "托盘启动失败，关闭按钮将直接退出程序");
                None
            }
        }
    } else {
        tracing::info!("配置已关闭托盘图标，不启动托盘");
        None
    };

    // ---- 帧循环 --------------------------------------------------------
    let mut frame_no: u64 = 0;
    let mut quit = false;
    // 卡片子窗口集合（详情/视图等被拖出主窗的卡片）。
    let mut children = crate::childwin::ChildWindows::new();
    // 窗口隐藏到托盘后仍在跑帧循环（要继续接收托盘事件），
    // 但不再渲染。`GetClientRect` 对隐藏窗口仍返回原尺寸，
    // 靠尺寸判断不出隐藏态，必须显式记这个标志。
    let mut hidden = false;
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

    while !quit {
        // ---- 1. 事件 ----
        //
        // `saw_close` 与 `wm_quit` 必须分开记：`gfx::window` 把
        // `WM_CLOSE` 与 `WM_QUIT` 都翻译成同一个 `CloseRequested`，
        // 而两者处置完全相反——前者隐藏到托盘，后者退出。
        // 区分依据是 `quit_requested()`，它**只在** `WM_QUIT` 时置位。
        let mut saw_close = false;
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
                WindowEvent::Resized { .. } => {
                    // 客户区尺寸变了 → 交换链必须重建。`rebuild_swapchain`
                    // 内部会 `device_wait_idle` 并重查表面能力。
                    rebuild(&mut fr, &mut painter)?;
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
                        rebuild(&mut fr, &mut painter)?;
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

        // ---- 1.5 托盘与全局快捷键 ----
        //
        // 放在窗口事件之后、业务 UI 之前：唤起动作要在本帧绘制出内容，
        // 否则用户会看到「窗口弹出来了但还是空的，下一帧才有东西」。
        if let Some(r) = resident.as_ref() {
            let f = r.poll();
            if f.clear_history {
                if let Err(e) = app.clear_history() {
                    tracing::warn!(%e, "托盘清空历史失败");
                }
            }
            if f.show {
                presence::focus_window(window.hwnd());
                hidden = false;
                // 窗口刚恢复，尺寸可能已变（最大化/还原切换）。
                // 立刻同步一次，避免用旧尺寸 present 一帧。
                rebuild(&mut fr, &mut painter)?;
                ctx.request_repaint();
            }
            if f.quit {
                tracing::info!("收到托盘退出请求");
                quit = true;
            }
        }

        // 关闭请求：托盘可用时隐藏到托盘，否则退出。
        //
        // 这里**不能**无条件 break——`WM_CLOSE` 不销毁窗口，
        // 隐藏后循环要继续跑，否则托盘事件再也没人收。
        if saw_close {
            match presence::decide_on_close(resident.is_some(), events.quit_requested()) {
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

        // ---- 2. 业务 UI ----
        //
        // **隐藏时也要跑**：业务 UI 里的 `svc.pump()` 才是把后台捕获
        // 的剪贴板内容写进数据库的那一步。隐藏后跳过它等于
        // 「窗口看不见 = 停止记录」，与后台常驻的初衷正好相反。
        //
        // `frame_events` 是上面 `take_pending()` 取出的本帧事件，
        // 事件**只有这一份**：既喂应用层（第 1 步的关闭/resize 判断），
        // 也喂 egui。漏掉后者会让 UI 收不到任何鼠标/键盘输入。
        let raw_input = events.egui_input(&ctx, &frame_events);
        // 诊断用：`run_ui` 按值消费 `raw_input`，之后拿不到 `screen_rect`。
        let diag_screen_rect = if std::env::var_os("MC_DIAG").is_some() {
            raw_input.screen_rect
        } else {
            None
        };
        let mut output = ctx.run_ui(raw_input, |ui| {
            app.draw_frame(ui);
        });
        let ppp = output.pixels_per_point;
        // 在移动 output.shapes 之前先把重绘延时取出来。
        let delay = crate::renderer::repaint_delay(&output);

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
            match presence::decide_on_close(resident.is_some(), false) {
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
            continue;
        }

        // ---- 3. 尺寸兜底检查 ----
        // 客户区可能被最小化到 0×0，此时交换链拿不到可呈现的图像。
        let (cw, ch) = window.inner_size_physical();
        if cw == 0 || ch == 0 {
            // ⚠️ 这里必须用 `events.poll_for` 而不是 `thread::sleep`。
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
            continue;
        }
        if (cw, ch) != (fr.extent().width, fr.extent().height) {
            // Resized 事件已处理过；这里兜住「尺寸查询与事件不一致」的情况
            // （例如 SetWindowPos 引起的多消息合并）。
            rebuild(&mut fr, &mut painter)?;
        }

        // ---- 4. 帧渲染 ----
        //
        // 把这一帧要用的东西全部从 `output` 里取出来，之后让它整体 drop。
        // `tessellate` 按值消耗 shapes，因此要先 `mem::take` 才能整体 drop。
        let mut textures_delta = DeltaGuard(std::mem::take(&mut output.textures_delta));
        let primitives = ctx.tessellate(std::mem::take(&mut output.shapes), ppp);
        drop(output);

        let Some(acquired) = fr.acquire()? else {
            rebuild(&mut fr, &mut painter)?;
            tracing::debug!("acquire 返回过期，已重建交换链");
            continue;
        };

        painter.paint(&mut fr, &primitives, ppp, &mut textures_delta.0)?;

        if fr.present(acquired)? == PresentResult::Outdated {
            // 呈现时才发现过期：下一轮 acquire 会返回 None 再重建。
            // 这里不立刻重建，避免连续两次 device_wait_idle。
            tracing::debug!("present 报告过期，下一轮重建");
        }

        if app.should_quit {
            quit = true;
        }

        // ---- 4.5 卡片子窗口渲染 ----
        //
        // 每个子窗口有**独立的 `egui::Context`、独立的交换链**，
        // 所以要各自走一遍「事件 → run_ui → tessellate → 绘制」。
        //
        // ⚠️ 渲染失败**不中断主循环**：一个附属窗口出问题
        // （客户区变 0、交换链过期、驱动抽风）不该让主窗黑屏。
        // 这里记警告后继续，下一帧重试。
        for child in children.iter_mut() {
            if let Err(e) = child.render(&mut app) {
                tracing::warn!(card = ?child.card_id, %e, "子窗口渲染失败，跳过本帧");
            }
        }

        frame_no += 1;
        if frame_no % 300 == 0 {
            tracing::debug!(
                frame = frame_no,
                vertices = painter.stats.vertices,
                draw_calls = painter.stats.draw_calls,
                rebuilds = painter.stats.rebuilds,
                "帧统计"
            );
        }
        if std::env::var_os("MC_DIAG").is_some() && frame_no % 30 == 0 {
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

        // ---- 5. 节流 ----
        // 无事件时不要空转烧 CPU。egui 请求了延时重绘就按它等待。
        //
        // ⚠️ `Some(ZERO)` 是「立即重绘」，直接传给 `poll_for` 会走
        // 纯自旋分支（`Some(d) if !d.is_zero()` 不成立），CPU 占用 100%。
        // 但也不能当成「无限等待」——那样聚焦的输入框光标不会闪、
        // 刚输入的字符不会立刻上屏。这里给 1ms 下限：
        // 既避免忙等，又保证输入延迟感知不到。
        let wait = match delay {
            Some(d) if d.is_zero() => Some(std::time::Duration::from_millis(1)),
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
    // ⚠️ 必须在 `shutdown()` 之前取：它会 `save_config()` 把配置落盘，
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
    app.persist_window_pos(main_pos);
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
) -> anyhow::Result<()> {
    fr.rebuild_swapchain(Default::default())?;
    painter.on_swapchain_rebuilt(fr)
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// 应用状态。
///
/// 只持有业务状态与 [`egui::Context`]；渲染资源在 [`run`] 的栈上。
/// 两者分离让 [`App`] 可以在没有 GPU 的环境下被单测。
pub struct App {
    /// egui 上下文。内部是 `Arc`，克隆代价极低。
    pub ctx: egui::Context,
    svc: Service,
    /// 已解析生效的调色板。
    ///
    /// 由 [`theme::set_theme`] 在构造时产出：它同时把令牌灌进 egui
    /// `Visuals` 并返回自绘要用的那一份，因此「控件观感」与「自绘图形」
    /// 必然同源。每帧把它交给 [`crate::paint::draw`]。
    pal: theme::Palette,
    /// 新卡片架构的工作区（卡片的唯一权威来源）。
    ///
    /// `pub` 是因为帧循环要用它同步子窗口：
    /// [`crate::childwin::ChildWindows::sync_with`] 要读每张卡片的
    /// [`crate::card::CardHost`] 并写回窗口位置。
    pub ws: crate::workspace::Workspace,
    /// 新卡片架构的界面状态。
    paint_state: crate::paint::UiState,
    /// 诊断用帧计数（仅 `MC_DIAG` 打开时用）。
    diag_frames: u64,
    /// 界面退出请求。
    should_quit: bool,
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
        let ctx = egui::Context::default();
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

        let mut svc = match Service::with_data_dir(config, data_dir) {
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

        // 载入界面偏好。未指定时跟随系统（`set_theme` 内部会探测）。
        //
        // 解析与灌入一次做完，返回的 `Palette` 存进 `App.pal` 给每帧自绘用——
        // 两处必须同源，早前`view::draw` 每帧自己算一遍，既浪费
        // （55 个字段逐个构造）又可能与灌进 Visuals 的那次解析分叉。
        let pal = theme::set_theme(
            &ctx,
            theme::ThemeMode::from_dark_mode(svc.state.config.ui.dark_mode),
        );
        tracing::info!(
            dark = pal.is_dark,
            configured = ?svc.state.config.ui.dark_mode,
            "主题已解析"
        );

        let mut ws = crate::workspace::Workspace::default();
        // 恢复上次保存的卡片窗口位置。
        //
        // ⚠️ 这里**不恢复** `config.ui.layout`（旧 `LayoutState` 布局）：
        // 那套四面板停靠/浮动体系连同 `layout.rs` 已随`view.rs` 一并删除，
        // 新架构的几何由 `solver` 每帧求解、由 `card.window_pos` 持久化。
        // 继续做那个往返只会把一份没人读的配置原样写回去。
        ws.apply_saved_positions(&svc.state.config.ui);

        svc.start_capture();

        Self {
            ctx,
            svc,
            pal,
            ws,
            paint_state: crate::paint::UiState::default(),
            diag_frames: 0,
            should_quit: false,
        }
    }

    /// 画一帧业务 UI。
    pub fn draw_frame(&mut self, ui: &mut egui::Ui) {
        // `MC_SETTINGS=1` 时默认打开设置界面。
        //
        // 用途是**截图验证**：设置面板是覆盖层，不点齿轮按钮就拍不到。
        // 做成环境变量而不是命令行参数，是为了让截图脚本不用改传参逻辑。
        // `MC_SETTINGS=1` 时默认打开设置界面。
        //
        // 用途是**截图验证**：设置面板是覆盖层，不点齿轮按钮就拍不到。
        // 做成环境变量而不是命令行参数，是为了让截图脚本不用改传参逻辑。
        if std::env::var_os("MC_SETTINGS").is_some() {
            self.paint_state.show_settings = true;
        }

        // 消费后台捕获事件；有变化时安排后续重绘。
        if self.svc.pump() {
            self.ctx
                .request_repaint_after(std::time::Duration::from_millis(250));
        }

        // ---- 新卡片架构 ------------------------------------------------
        //
        // 顺序是硬性的：**先求解、再应用、后绘制**。
        // `solver::solve` 是纯函数（不改工作区），`apply` 才写入矩形。
        // 绘制层只读 `card.rect`，不做任何二次判断——这正是新架构
        // 消除「布局算一遍、绘制再判一遍」的关键。
        let area = ui.max_rect();
        let ppp = ui.ctx().pixels_per_point();
        let sol = crate::solver::solve(&self.ws, area);
        crate::solver::apply(&mut self.ws, &sol);

        if std::env::var_os("MC_DIAG").is_some() {
            self.diag_frames += 1;
            if self.diag_frames % 60 == 0 {
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

        {
            let mut frame = crate::paint::Frame {
                ui,
                state: &mut self.paint_state,
                svc: &mut self.svc,
                ws: &self.ws,
                pal: &self.pal,
                scale: ppp,
                area,
                // `None` = 画主窗口（顶栏 + 全部停靠卡片）。
                card_id: None,
            };
            crate::paint::draw(&mut frame);
        }

        // 绘制层只识别操作、不改布局：统一在这里落到工作区与服务上。
        self.apply_ops();

        if self.should_quit {
            self.svc.stop_capture();
            let _ = self.svc.save_config();
        }
    }

    /// 把绘制层收集到的操作落到工作区与服务上。
    ///
    /// 绘制层**只识别、不改状态**——所有变更集中在这里，
    /// 于是「点了折叠按钮」这件事不会散落在几十个绘制函数里。
    fn apply_ops(&mut self) {
        use crate::paint::Op;
        for op in self.paint_state.take_ops() {
            match op {
                Op::SetBool { group, field, value } => {
                    set_bool_field(&mut self.svc.state.config, group, field, value);
                    self.after_config_change();
                }
                Op::SetNumber { group, field, value } => {
                    set_number_field(&mut self.svc.state.config, group, field, value);
                    self.after_config_change();
                }
                Op::SetOptNumber { group, field, value } => {
                    set_opt_number_field(&mut self.svc.state.config, group, field, value);
                    self.after_config_change();
                }
                Op::Select(id) => {
                    self.svc.state.selected = Some(id);
                }
                Op::TogglePin(id) => {
                    if let Err(e) = self.svc.toggle_pin(id) {
                        self.svc.notify(format!("置顶失败: {e}"));
                    }
                }
                Op::Delete(id) => {
                    if let Err(e) = self.svc.delete_item(id) {
                        self.svc.notify(format!("删除失败: {e}"));
                    }
                }
                Op::Copy(id) => {
                    if let Err(e) = self.svc.copy_item(id) {
                        self.svc.notify(format!("复制失败: {e}"));
                    }
                }
                Op::SetPinnedMode(mode) => {
                    self.ws.set_pinned_mode(mode);
                }
                Op::ToggleSettings => {
                    self.paint_state.show_settings = !self.paint_state.show_settings;
                }
                Op::ClearAll => {
                    if let Err(e) = self.svc.clear_all() {
                        self.svc.notify(format!("清空失败: {e}"));
                    }
                }
                // 窗口级按钮只**置标志**，由帧循环消费。
                //
                // ⚠️ 不能在这里直接关窗口：`apply_ops`  borrows `self`，
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
        // ⚠️ 必须比较**内容**而不是「非空」：早前版本写成
        // `if !q.is_empty() || !svc.state.query.is_empty()`，
        // 结果用户删掉全部文本（query 变空）时条件不成立，
        // 过滤永远解不掉——列表卡在「无结果」状态。
        //
        // 只在内容真的变化时调 `search()`：它内部会 `reload_list()`
        // 重新查库，每帧调一次是无谓的 IO。
        if self.paint_state.query != self.svc.state.query {
            let q = self.paint_state.query.clone();
            self.svc.search(q);
        }
    }

    /// 画一个卡片子窗口的内容。
    ///
    /// 与 [`Self::draw_frame`] 分开而不是加参数：`draw_frame` 还要跑
    /// 主窗口专属的逻辑（`svc.pump()`、拖动区上报、操作落地），
    /// 那些对子窗口没有意义——子窗口只是一张卡片的容器。
    pub fn draw_child_frame(
        &mut self,
        ui: &mut egui::Ui,
        card_id: crate::card::CardId,
    ) {
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
        };
        crate::paint::draw_child(&mut frame);
        // 子窗口里的操作（点叉收回、点拖动条收回）也要落地。
        self.apply_ops();
    }

    /// 本帧的上报拖动区。由外壳每帧读一次并转给窗口层。
    pub fn drag_regions(&self) -> &[egui::Rect] {
        // 新架构的拖动区由 `paint::draw_topbar` 每帧写入。
        // 旧的 `local.drag_region` 已不再更新——保留读旧字段会拿到
        // 永远为 None 的值，表现为「窗口拖不动」。
        //
        // ⚠️ 是**多段**：顶栏里的齿轮、垃圾桶、搜索框必须从拖动区里
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

    /// 清空全部历史（托盘菜单「清空历史」入口）。
    ///
    /// 与界面上的「清空」按钮走同一个 [`Service::clear_all`]，
    /// 保证两条入口的行为完全一致——包括失败时的提示方式。
    pub fn clear_history(&mut self) -> anyhow::Result<()> {
        self.svc.clear_all()
    }

    /// 退出前保存状态。
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
    pub fn persist_window_pos(&mut self, p: egui::epaint::emath::Vec2) {
        self.svc.state.config.ui.window_pos = Some(WindowPos::new(p.x, p.y));
    }

    /// 配置改动后的收尾：立刻落盘。
    ///
    /// 设置界面里点一下就写文件——用户不该担心「关掉程序才生效」。
    /// 写失败只记日志：设置面板不该因为磁盘问题卡住。
    fn after_config_change(&mut self) {
        if let Err(e) = self.svc.save_config() {
            tracing::warn!(%e, "保存配置失败");
            self.svc.notify(format!("保存设置失败: {e}"));
        }
    }

    /// 记下置顶窗口位置（供下次启动恢复）。
    pub fn set_pinned_window_pos(&mut self, p: egui::epaint::emath::Vec2) {
        self.svc.state.config.ui.pinned_window_pos = Some(WindowPos::new(p.x, p.y));
    }

    pub fn shutdown(&mut self) {
        self.svc.stop_capture();
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
        (G::Storage, "cleanup_on_start") => cfg.storage.cleanup_on_start = v,
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
        (G::Storage, "max_bytes") => cfg.storage.max_bytes = v.max(1.0) as u64,
        (G::Storage, "max_payload_bytes") => cfg.storage.max_payload_bytes = v.max(1.0) as u64,
        _ => tracing::warn!(?g, field, "未知的数值配置字段，已忽略"),
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

#[cfg(test)]
mod tests {
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
