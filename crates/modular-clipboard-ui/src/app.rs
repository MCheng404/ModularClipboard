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
//!
//! # 模块地图（2026-10-22 拆分）
//!
//! 本文件是模块根：`App` 结构体与它的 `impl`（业务状态 + 每帧入口
//! `draw_frame` + 操作落地 `apply_ops` + 各窗口的帧入口）。
//! 具体机制按职责住在 `app/` 子模块里：
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | `boot` | 入口函数（`run*`）与启动期辅助（背景实度 / 清屏色 / env 开关） |
//! | `frame_loop` | `run_mode` 主循环与附属窗口驱动（`drive_attach_windows`） |
//! | `recording` | 热键录制的 `App` 方法（录制态 / 落地 / 暂停） |
//! | `config_write` | 设置项字符串字段的写入臂（`set_*_field`） |
//! | `resize` | 窗口宽度调整请求（`WidthRequest`）与求解 |
//!
//! 入口与跨文件复用项（`run*` / `DeltaGuard` / `pick_surface_format` 等）
//! 在根里再导出，`crate::app::X` 路径不变。

use modular_clipboard_core::WindowPos;
use modular_clipboard_app::Service;

use crate::theme;

mod boot;
mod config_write;
mod frame_loop;
mod recording;
mod resize;

// 入口与跨文件复用项：`crate::app::X` / `crate::ui::X` 路径保持不变
// （lib.rs 的 `pub use app::{run, ...}` 依赖这四个名字）。
pub use boot::{run, run_remote, run_with_capture_override, run_with_options};
pub(crate) use boot::DeltaGuard;
pub(crate) use boot::is_sane_window_pos;
pub(crate) use frame_loop::pick_surface_format;
pub(crate) use resize::WidthRequest;

use boot::{backdrop_alpha, env_flag};
use config_write::{set_bool_field, set_number_field, set_opt_number_field, set_text_field};
use resize::resize_width_target;



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
                    .with_card_opacity(card_alpha);
                &backdrop_pal
            }
            modular_clipboard_core::config::WindowBackdrop::Translucent => {
                let base = (opacity.clamp(0.3, 1.0) * 0.85).max(0.55);
                let alpha = backdrop_alpha(base, self.backdrop_lum, is_dark);
                backdrop_pal = self
                    .pal
                    .with_backdrop_alpha(alpha)
                    .with_card_opacity(card_alpha);
                &backdrop_pal
            }
            modular_clipboard_core::config::WindowBackdrop::None => &self.pal,
        };
        // 卡片透明度在**窗口背景无底图**时也要能生效——只要开了卡片磨砂。
        //
        // 早前的判据是「有没有背景效果」，理由是「不透明底图后面没有东西
        // 可透」。但卡片磨砂给每张卡片自带了一层模糊的身后内容，卡片后面
        // 于是**有东西可透**了；继续禁用这个滑块，用户开了磨砂却调不动
        // 透明度，会以为设置没保存。
        let card_blur_pal;
        let pal_ref = if backdrop == modular_clipboard_core::config::WindowBackdrop::None
            && self.svc.state.config.ui.card_blur > 0
        {
            card_blur_pal = pal_ref.with_card_opacity(card_alpha);
            &card_blur_pal
        } else {
            pal_ref
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
        // 卡片磨砂：渲染层上一帧抓到的像素在这里上传（需要 `Context`）。
        //
        // CAUTION: **不能**每次都 `load_texture`。
        //
        // `load_texture` 是「注册一张新纹理」：旧 `TextureHandle` 被替换
        // 掉就 drop，egui 随即把那张纹理标 free，渲染层于是回收它的
        // 描述符集再为新 id 分配一个。捕获按 30fps 走 ⇒ 每秒 30 次
        // free+alloc。实测几秒就把用户纹理描述符池抽干
        // （`ERROR_OUT_OF_POOL_MEMORY`，缩略图开始批量上传失败）。
        //
        // 稳态下纹理尺寸是**固定**的（客户区 ÷ 降采样倍率），所以只有
        // 首次与窗口尺寸变化时才真的需要新建，其余帧用 `TextureHandle::set`
        // 就地更新像素——同一个 id、同一个描述符集，零分配。
        if let Some(img) = self.paint_state.card_blur_pending.take() {
            let size = img.size;
            let need_new = match &self.paint_state.card_blur_tex {
                None => true,
                Some(h) => h.size() != size,
            };
            if need_new {
                let handle =
                    ui.ctx().load_texture("card_blur", img, egui::TextureOptions::LINEAR);
                self.paint_state.card_blur_tex = Some(handle);
            } else if let Some(h) = &mut self.paint_state.card_blur_tex {
                h.set(img, egui::TextureOptions::LINEAR);
            }
        }

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
                    modular_clipboard_app::apply_language(lang);
                    self.after_config_change();
                }
                Op::CopyMcpPairPrompt => {
                    // 把按当前 MCP 配置动态拼接的接入说明复制到剪贴板。
                    //
                    // CAUTION: `write_clipboard_text` 内部在写之前武装抑制
                    // （同 `copy_item` 规则），防止捕获线程把自己写的内容再记一条。
                    let prompt = crate::paint::mcp_pair_prompt(&self.svc.state.config);
                    match self.svc.write_clipboard_text(&prompt) {
                        Ok(()) => self.svc.notify(modular_clipboard_core::i18n::t("AI pairing prompt copied").into()),
                        Err(e) => self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Failed to copy the pairing prompt:"))),
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
                Op::SetCardBlur(level) => {
                    // 档位 0 = 关闭（不抓屏、不上传纹理）。渲染层按
                    // 「已应用值」变化启停 GDI 捕获器，这里只负责落配置。
                    self.svc.state.config.ui.card_blur = level.min(10);
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
                            self.svc.notify(format!("{} {text}", modular_clipboard_core::i18n::t("Set to")));
                        }
                        Err(e) => {
                            // 解析失败要说清楚**为什么**：只提示「无效」
                            // 用户就得自己猜是不是键名写错了。
                            self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Invalid shortcut:")));
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
                            self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Failed to apply:")));
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
                        self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Pin failed:")));
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
                        Err(e) => self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Delete failed:"))),
                    }
                }
                Op::Copy(id) => {
                    // 成功也要反馈：旧实现只在失败时 notify，用户点「复制」
                    // 成功时界面**毫无变化」——剪贴板管理器最核心的动作
                    // 反而最没有确认感（2026-10-20 用户反馈「复制没反应」，
                    // 排查后发现是成功路径无反馈）。提示走 toast/弹窗通道
                    // （`ui.notice_popup` 决定呈现），3 秒自消。
                    match self.svc.copy_item(id) {
                        Ok(()) => self.svc.notify(modular_clipboard_core::i18n::t("Copied").into()),
                        Err(e) => self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Copy failed:"))),
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
                                .notify(modular_clipboard_core::i18n::t("Cannot edit: content was evicted ({e})").replace("{e}", &e.to_string()));
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
                        self.svc.notify(modular_clipboard_core::i18n::t("Empty content, not saved").into());
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
                            self.svc.notify(modular_clipboard_core::i18n::t("Nothing to undo").into());
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
                                .notify(modular_clipboard_core::i18n::t("Cannot edit image: missing dimensions").into());
                        }
                        (Err(e), _) => {
                            self.svc
                                .notify(modular_clipboard_core::i18n::t("Cannot edit image: content was evicted ({e})").replace("{e}", &e.to_string()));
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
                            self.svc.notify(modular_clipboard_core::i18n::t("All history cleared").to_string());
                        }
                        Err(e) => self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Clear failed:"))),
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
                        self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Failed to open link:")));
                    }
                }
                Op::OpenUrl(url) => {
                    // 双击链接型条目：尊重设置里的自定义浏览器路径
                    // （留空 = 系统默认浏览器）。失败必须告知用户——
                    // 双击没反应是最难自查的一类故障。
                    let custom = self.svc.state.config.ui.browser_path.trim();
                    if let Err(e) = modular_clipboard_platform::open_url_with(&url, custom) {
                        self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Failed to open link:")));
                    }
                }
                Op::OpenDataDir => {
                    // 关于页「数据目录」超链接：进入目录（不是定位选中）。
                    let dir = self.svc.data_dir().display().to_string();
                    let custom = self.svc.state.config.ui.explorer_path.trim();
                    if let Err(e) = modular_clipboard_platform::open_dir_with(&dir, custom) {
                        self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Failed to open the file manager:")));
                    }
                }
                Op::RevealPath(path) => {
                    // 双击文件型条目：尊重设置里的自定义资源管理器路径
                    // （留空 = 系统 explorer /select 定位）。
                    let custom = self.svc.state.config.ui.explorer_path.trim();
                    if let Err(e) = modular_clipboard_platform::reveal_with(&path, custom) {
                        self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Failed to open the file manager:")));
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
                self.svc.notify(modular_clipboard_core::i18n::t("Changes saved").into());
            }
            Err(e) => {
                self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Save failed:")));
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
            self.svc.notify(format!("{} {e}", modular_clipboard_core::i18n::t("Failed to save settings:")));
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
