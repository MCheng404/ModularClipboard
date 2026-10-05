//! 界面层：egui 渲染与交互。
//!
//! 该层只依赖 [`tiez_app::Service`] 暴露的服务接口，不直接访问数据库或剪贴板，
//! 因此可以整体替换而不影响业务逻辑。

pub mod renderer;
pub mod theme;
pub mod view;

use tiez_app::Service;

pub use view::UiLocal;

/// 启动图形界面。`config` 为初始配置。
pub fn run(config: tiez_core::Config) -> anyhow::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([
                config.ui.window_width,
                config.ui.window_height,
            ])
            .with_min_inner_size([320.0, 240.0])
            .with_decorations(false) // 自绘标题栏
            .with_transparent(false),
        // 渲染后端由 renderer 模块集中配置。
        wgpu_options: renderer::native_options(),
        ..Default::default()
    };

    eframe::run_native(
        renderer::WINDOW_TITLE,
        native_options,
        Box::new(|cc| Ok(Box::new(App::new(cc, config)))),
    )
    .map_err(|e| anyhow::anyhow!("界面启动失败: {e}"))
}

/// eframe 应用状态。
struct App {
    svc: Service,
    local: UiLocal,
    /// 请求退出标记。
    should_quit: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, config: tiez_core::Config) -> Self {
        theme::install_cjk_font(&cc.egui_ctx, config.ui.font_path.as_deref());

        let svc = match Service::new(config) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(%e, "服务初始化失败，界面将以空状态运行");
                // 用内存库兜底，避免整个应用无法启动。
                Service::in_memory()
            }
        };

        // 载入界面偏好
        let dark_mode = svc.state.config.ui.dark_mode;
        if let Some(dark) = dark_mode {
            if dark {
                cc.egui_ctx.set_visuals(cc.egui_ctx.style_of(eframe::egui::Theme::Dark).visuals.clone());
            } else {
                cc.egui_ctx.set_visuals(light_visuals(&cc.egui_ctx));
            }
        }

        let mut svc = svc;
        svc.start_capture();

        Self {
            svc,
            local: UiLocal::default(),
            should_quit: false,
        }
    }
}

impl eframe::App for App {
    /// eframe 0.36 用 `ui` 取代了旧版的 `update`：框架已把中央面板准备好，
    /// 只需往给定Ui 里画，也因此不再需要手动管理面板与视口命令。
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        // 消费后台捕获事件；有变化时安排后续重绘。
        if self.svc.pump() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }

        if view::draw(ui, &mut self.svc, &mut self.local) {
            self.should_quit = true;
        }
        if self.should_quit {
            self.svc.stop_capture();
            let _ = self.svc.save_config();
            ui.ctx().send_viewport_cmd(eframe::egui::ViewportCommand::Close);
        }
    }

    /// wgpu 后端下 `on_exit` 不接收 GL 上下文参数（无 glow feature）。
    fn on_exit(&mut self) {
        self.svc.stop_capture();
        if let Err(e) = self.svc.save_config() {
            tracing::warn!(%e, "退出时保存配置失败");
        }
    }
}

/// 生成浅色视觉配置。
fn light_visuals(ctx: &eframe::egui::Context) -> eframe::egui::Visuals {
    let mut v = ctx.style_of(eframe::egui::Theme::Dark).visuals.clone();
    v.dark_mode = false;
    v.panel_fill = eframe::egui::Color32::from_rgb(0xF5, 0xF5, 0xF5);
    v.window_fill = eframe::egui::Color32::from_rgb(0xFA, 0xFA, 0xFA);
    v.extreme_bg_color = eframe::egui::Color32::WHITE;
    v.faint_bg_color = eframe::egui::Color32::from_gray(240);
    v.override_text_color = Some(eframe::egui::Color32::from_gray(20));
    v.widgets.noninteractive.bg_stroke =
        eframe::egui::Stroke::new(1.0, eframe::egui::Color32::from_gray(200));
    v.widgets.inactive.bg_fill = eframe::egui::Color32::from_gray(245);
    v.widgets.hovered.bg_fill = eframe::egui::Color32::from_gray(235);
    v
}
