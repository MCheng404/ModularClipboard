//! 应用入口：窗口 + 事件循环 + 帧循环。
//!
//! # 一帧的完整时序
//!
//! ```text
//! EventLoop::poll            取 Win32 消息，处理 Resized / CloseRequested
//! EventLoop::egui_input      翻译成 RawInput（必须在 poll 之后）
//! Context::run_ui            跑业务 UI，产出 FullOutput
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

use modular_clipboard_app::Service;
use modular_clipboard_gfx::frame::{FrameRenderer, PipelineBundle, PresentResult};
use modular_clipboard_gfx::window::{EventLoop, Window, WindowEvent};
use modular_clipboard_gfx::Gpu;

use crate::presence::{self, CloseDecision, Resident};
use crate::renderer::{Painter, WINDOW_TITLE};
use crate::view::UiLocal;
use crate::{theme, view};

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
    run_with_capture_override(config, false)
}

/// 同[`run`]，但显式声明 `config.capture.enabled` 是否为命令行临时覆盖。
///
/// `--no-capture` 会把它置false 以便调试时不动真实剪贴板；
/// 若这个临时值被 [``Service::save_config``] 写进config.json，
/// 用户的剪贴板监听会**永久关闭**且无任何提示。
/// 传 `true` 让它跳过落盘。
pub fn run_with_capture_override(
    config: modular_clipboard_core::Config,
    capture_is_override: bool,
) -> anyhow::Result<()> {
    // `Window::new` 的宽高是**物理像素**，而配置里存的是逻辑点。
    // DPI 缩放在窗口创建后才可查，因此先按 1.0 换算——创建后第一帧的
    // `Resized` 事件（或帧循环里的尺寸兜底检查）会把交换链纠正到真实尺寸。
    let window = Window::new(
        WINDOW_TITLE,
        config.ui.window_width.max(1.0) as u32,
        config.ui.window_height.max(1.0) as u32,
    )?;
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

    // ---- egui -----------------------------------------------------------
    let mut app = App::new(config, capture_is_override);
    let ctx = app.ctx.clone();

    // ---- 托盘常驻--------------------------------------------------------
    //
    // 托盘自带独立线程与消息循环（见 `presence` 模块文档），
    // 这里只是每帧收一次事件。启动失败**不阻断应用**：
    // 托盘只是入口方式，没有它界面照样能用，只是关掉就没了。
    let resident = match Resident::start(TRAY_TOOLTIP) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::error!(%e, "托盘启动失败，关闭按钮将直接退出程序");
            None
        }
    };

    // ---- 帧循环 --------------------------------------------------------
    let mut frame_no: u64 = 0;
    let mut quit = false;
    // 窗口隐藏到托盘后仍在跑帧循环（要继续接收托盘事件），
    // 但不再渲染。`GetClientRect` 对隐藏窗口仍返回原尺寸，
    // 靠尺寸判断不出隐藏态，必须显式记这个标志。
    let mut hidden = false;

    while !quit {
        // ---- 1. 事件 ----
        //
        // `saw_close` 与 `wm_quit` 必须分开记：`gfx::window` 把
        // `WM_CLOSE` 与 `WM_QUIT` 都翻译成同一个 `CloseRequested`，
        // 而两者处置完全相反——前者隐藏到托盘，后者退出。
        // 区分依据是 `quit_requested()`，它**只在** `WM_QUIT` 时置位。
        let mut saw_close = false;
        for ev in events.poll() {
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

        // ---- 2. 业务 UI ----
        //
        // **隐藏时也要跑**：业务 UI 里的 `svc.pump()` 才是把后台捕获
        // 的剪贴板内容写进数据库的那一步。隐藏后跳过它等于
        // 「窗口看不见 = 停止记录」，与后台常驻的初衷正好相反。
        //
        // `poll` 已在上面填好缓冲，`egui_input` 读的就是本帧事件。
        let raw_input = events.egui_input(&ctx);
        let mut output = ctx.run_ui(raw_input, |ui| {
            app.draw_frame(ui);
        });
        let ppp = output.pixels_per_point;
        // 在移动 output.shapes 之前先把重绘延时取出来。
        let delay = crate::renderer::repaint_delay(&output);

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
            // 不渲染。增量由守卫在离开作用域时清空。
            std::thread::sleep(std::time::Duration::from_millis(50));
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

        // ---- 5. 节流 ----
        // 无事件时不要空转烧 CPU。egui 请求了延时重绘就按它等待。
        events.poll_for(delay);
    }

    // ---- 清理 ------------------------------------------------------------
    //
    // 顺序要紧：先撤托盘图标（用户能立刻看到它消失），
    // 再停捕获并保存配置，最后才销毁渲染资源。
    if let Some(mut r) = resident {
        r.shutdown();
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
/// 单格式时按规范原样采用。真实的一致性由 `FrameRenderer::new` 校验。
fn pick_surface_format(gpu: &Gpu) -> anyhow::Result<ash::vk::Format> {
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
    local: UiLocal,
    /// 界面退出请求。
    should_quit: bool,
}

impl App {
    /// 按配置初始化业务状态与视觉风格。
    pub fn new(
        config: modular_clipboard_core::Config,
        capture_is_override: bool,
    ) -> Self {
        let ctx = egui::Context::default();
        theme::install_cjk_font(&ctx, config.ui.font_path.as_deref());

        let mut svc = match Service::new(config) {
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

        // 载入界面偏好。未指定时跟随系统（Windows 上取深色，
        // 与旧 eframe 默认一致）。
        let visuals = match svc.state.config.ui.dark_mode {
            Some(false) => light_visuals(&ctx),
            _ => ctx.style_of(egui::Theme::Dark).visuals.clone(),
        };
        ctx.set_visuals(visuals);

        svc.start_capture();

        Self {
            ctx,
            svc,
            local: UiLocal::default(),
            should_quit: false,
        }
    }

    /// 画一帧业务 UI。
    pub fn draw_frame(&mut self, ui: &mut egui::Ui) {
        // 消费后台捕获事件；有变化时安排后续重绘。
        if self.svc.pump() {
            self.ctx
                .request_repaint_after(std::time::Duration::from_millis(250));
        }

        if view::draw(ui, &mut self.svc, &mut self.local) {
            self.should_quit = true;
        }
        if self.should_quit {
            self.svc.stop_capture();
            let _ = self.svc.save_config();
        }
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
    pub fn shutdown(&mut self) {
        self.svc.stop_capture();
        if let Err(e) = self.svc.save_config() {
            tracing::warn!(%e, "退出时保存配置失败");
        }
    }
}

/// 生成浅色视觉配置。
///
/// 从深色派生再逐项改写，比从零构造稳——新增 egui 视觉项时不会漏。
pub fn light_visuals(ctx: &egui::Context) -> egui::Visuals {
    let mut v = ctx.style_of(egui::Theme::Dark).visuals.clone();
    v.dark_mode = false;
    v.panel_fill = egui::Color32::from_rgb(0xF5, 0xF5, 0xF5);
    v.window_fill = egui::Color32::from_rgb(0xFA, 0xFA, 0xFA);
    v.extreme_bg_color = egui::Color32::WHITE;
    v.faint_bg_color = egui::Color32::from_gray(240);
    v.override_text_color = Some(egui::Color32::from_gray(20));
    // 从深色主题克隆来的fg_stroke 是**浅色**文字（灰度约 20），
    // 直接用在浅色背景上等于「白底白字」——界面能跑但什么都看不见。
    // 每一个会被用到的前景色都要翻转为深色。
    let fg = egui::Color32::from_gray(20);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, fg);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, egui::Color32::from_gray(200));
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, fg);
    v.widgets.inactive.bg_fill = egui::Color32::from_gray(245);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, fg);
    v.widgets.hovered.bg_fill = egui::Color32::from_gray(235);
    v.widgets.open.fg_stroke = egui::Stroke::new(1.0, fg);
    v.error_fg_color = egui::Color32::from_rgb(0xB0, 0x00, 0x20);
    v.warn_fg_color = egui::Color32::from_rgb(0x8A, 0x60, 0x00);
    v
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

    #[test]
    fn light_visuals_actually_flips_dark_mode() {
        // `light_visuals` 从深色派生，若忘记改 `dark_mode` 就会得到
        // 「深色背景 + 深色文字」= 什么都看不见。
        let ctx = egui::Context::default();
        let v = light_visuals(&ctx);
        assert!(!v.dark_mode, "浅色视觉必须把 dark_mode 置否");

        // 浅色模式下必须**深字浅底**才可读——即文字亮度**低于**背景。
        //
        // （曾把断言写成 `lum(text) > lum(panel_fill)`，那是深色模式的
        //  要求，方向反了。浅色模式下若文字比背景亮，那就是白底白字。）
        //
        // 用「亮度」而不是单通道比较：单通道会被「文字偏黄/偏蓝」之类的
        // 合法配色误判为失败。
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
}
