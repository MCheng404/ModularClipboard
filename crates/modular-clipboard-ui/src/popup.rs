//! 悬浮面板弹窗：搜索面板与类型过滤栏。
//!
//! # 为什么是弹窗而不是「画在窗口内」
//!
//! 原型（`main-proto/index.html`）里这两个面板**漂在主窗口之外**：
//! 搜索面板悬在窗口上方 12pt，类型过滤栏贴在窗口某一侧 12pt，间距
//! 透出桌面。真实 Win32 窗口画不出自己的客户区（egui 的绘制被裁剪在
//! HWND 矩形内），于是两条路：
//!
//! 1. **画进窗口内**——观感与原型不符（面板贴在内容区边缘，不是浮在外面）；
//! 2. **独立弹窗**（本模块）——与原型一致。
//!
//! 代价是每个弹窗一套 `FrameRenderer` + `Painter`（含独立字体图集），
//! 这正是项目历史上删掉卡片子窗口时权衡过的那笔显存。取舍：两个弹窗
//! 的**面板本体**很小（搜索条 720×45、类型栏 ≤48×~250），交换链开销
//! 可忽略，真正占内存的是字体图集——而弹窗只在需要时创建、隐藏时保留
//! （见 [`Popups`]），实际同时存在最多两个。
//!
//! # 圆角与投影：**为什么不靠 alpha 合成**
//!
//! 最初的实现把「面板之外的区域」留成 alpha=0，指望 DWM 按交换链的
//! 逐像素 alpha 合成出圆角与投影。实机是**黑屏 + 黑色直角**：本项目的
//! 窗口并不是靠 DWM 的 alpha 合成来做半透明的——主窗口的磨砂/半透明
//! 是**自研方案**（`gfx::backdrop` 抓窗口身后的桌面、GPU 处理后当底图，
//! 窗口实际是**不透明**的，见 `presence::apply_backdrop` 的说明）。
//! 交换链 alpha 在这条管线上不参与合成，alpha=0 的像素就落成黑色。
//!
//! 于是弹窗照同一套做：== 假透明 ==
//!
//! 1. 节流捕获**自己身后**的桌面（`DesktopCapture::capture_behind`，
//!    配合 `WDA_EXCLUDEFROMCAPTURE` 自排除，否则会抓到自己的上一帧，
//!    形成反馈回路）；
//! 2. 上传成 egui 纹理，**铺满整个客户区**（不透明）；
//! 3. 投影画在底图之上（alpha 混合在渲染层完成），面板再画在最上层，
//!    面板圆角处露出的就是底图 ⇒ 视觉上就是圆角 + 浮在桌面上。
//!
//! 窗口矩形仍比面板大一圈 [`SHADOW_MARGIN`]，而**命中判定不看像素**，
//! 所以还要登记「面板形状」让 `WM_NCHITTEST` 对外圈返回
//! `HTTRANSPARENT`（见 `set_panel_shape`），否则点投影会点不动底下的窗口。
//!
//! # 焦点
//!
//! 每个弹窗有自己的 `egui::Context`：**键盘焦点在弹窗自己的 Context
//! 里**，主窗口的 `memory()` 看不到它。于是「焦点钉住展开」这条规则
//! 改由弹窗每帧把 `focused / pointer_over` 回报给主窗口的状态机
//! （[`crate::paint::UiState::search_popup_focus`] 等）。
//!
//! 搜索面板**要能打字**⇒ 建成普通可激活窗口（不加 `WS_EX_NOACTIVATE`）；
//! 类型过滤栏只有按钮 ⇒ `no_activate`，点它不把焦点从用户正在用的
//! 程序抢走。

use modular_clipboard_gfx::window::{Window, WindowEvent};

use crate::multiwindow::{SharedGfx, WindowGfx};

/// 面板投影的安全边距（逻辑点）。
///
/// 弹窗客户区 = 面板 + 四周各一圈本值，用来容纳高斯投影（模糊半径
/// 8pt ≈ 12px，浓度 20%）。**窗口矩形比面板大**这一圈，是上面
/// 「点击穿透」那条存在的原因。
pub const SHADOW_MARGIN: f32 = 12.0;

/// 弹窗身份。既是内容分派的键，也是热区命名空间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PopupKind {
    /// 悬浮搜索面板（主窗口上方，可激活）。
    Search,
    /// 类型过滤栏（主窗口某一侧，不抢焦点）。
    Rail,
}

impl PopupKind {
    /// 窗口标题（不进任务栏，但调试时看得出是哪个）。
    pub fn title(self) -> &'static str {
        match self {
            PopupKind::Search => "ModularClipboard 搜索",
            PopupKind::Rail => "ModularClipboard 类型栏",
        }
    }

    /// 是否允许点击时抢占键盘焦点。
    ///
    /// 搜索面板要接收键盘（用户要打字）⇒ 可激活；类型栏只有按钮 ⇒
    /// 用 `WS_EX_NOACTIVATE`，点它时前台程序不被抢走。
    fn activatable(self) -> bool {
        matches!(self, PopupKind::Search)
    }
}

/// 一个弹窗窗口。
///
/// 结构与 [`crate::childwin::ChildWindow`] 同构（Vulkan 表面 + 窗口 +
/// 交换链 + 独立 Context），差别是不与卡片绑定、不拖动、面板在窗口外。
pub struct Popup<'a> {
    /// Vulkan 表面句柄。
    ///
    /// 必须自己留一份：`FrameRenderer::drop` **不销毁表面**
    /// （见 `ChildWindow` 同名字段的说明），`Drop` 里要手动收。
    surface: ash::vk::SurfaceKHR,
    window: Window,
    gfx: WindowGfx<'a>,
    events: modular_clipboard_gfx::window::EventLoop,
    ctx: egui::Context,
    /// 上一帧是否可见（用于「刚打开」判定）。
    was_open: bool,
    /// 已应用的置顶状态（幂等去重）。
    topmost: bool,
    /// 桌面捕获器（身后背景）。懒创建：GDI 句柄，失败即降级为纯色底。
    capture: Option<modular_clipboard_gfx::backdrop::DesktopCapture>,
    /// 上次成功捕获的时刻（节流）。
    last_capture: Option<std::time::Instant>,
    /// 底图纹理（捕获成功即更新；首帧还没抓到时为 `None`）。
    bg_tex: Option<egui::TextureHandle>,
    /// 上次定位到的屏幕位置。窗口一动身后内容就变了，必须强制重抓。
    last_pos: Option<(i32, i32)>,
}

impl<'a> Popup<'a> {
    /// 按**面板**的逻辑点尺寸创建弹窗（客户区自动加一圈 [`SHADOW_MARGIN`]）。
    pub fn create(
        kind: PopupKind,
        panel_size: egui::Vec2,
        scale_factor: f32,
        shared: &SharedGfx<'a>,
    ) -> anyhow::Result<Self> {
        let s = if scale_factor > 0.01 { scale_factor } else { 1.0 };
        let client = egui::vec2(
            panel_size.x + SHADOW_MARGIN * 2.0,
            panel_size.y + SHADOW_MARGIN * 2.0,
        );
        let mut window = Window::new_popup(
            kind.title(),
            (client.x * s) as u32,
            (client.y * s) as u32,
            !kind.activatable(),
        )?;
        window.set_max_texture_side(shared.gpu.properties.limits.max_image_dimension2_d);
        // 面板形状 = 客户区中央的面板矩形；之外（投影区）点击穿透。
        // 用**整数像素**登记：Win32 命中判定按整数像素，亚像素矩形会漏边。
        // （egui 0.36 的 `Rect` 没有 `round()`，自己取整。）
        let panel = egui::Rect::from_min_max(
            egui::pos2(SHADOW_MARGIN, SHADOW_MARGIN),
            egui::pos2(SHADOW_MARGIN + panel_size.x, SHADOW_MARGIN + panel_size.y),
        );
        let panel = egui::Rect::from_min_max(
            egui::pos2(panel.min.x.round(), panel.min.y.round()),
            egui::pos2(panel.max.x.round(), panel.max.y.round()),
        );
        modular_clipboard_gfx::chrome::set_panel_shape(window.hwnd(), Some(panel));
        // 捕获自排除：桌面捕获要看到弹窗**身后**而不是弹窗自己
        // （否则抓到的是自己的上一帧，形成反馈回路，表现为拖影/自叠加）。
        // 副作用与主窗口磨砂模式一致：弹窗不出现在录屏/截图里。
        crate::presence::apply_backdrop(
            window.hwnd(),
            modular_clipboard_core::config::WindowBackdrop::Acrylic,
        );

        let surface = shared.gpu.create_surface_for(window.hinstance(), window.hwnd())?;
        let gfx = WindowGfx::new_for(shared, surface)?;
        let ctx = shared.new_context();
        // EventLoop 借用 window，必须在 struct 字面量**搬走** window 之前建好。
        let events = modular_clipboard_gfx::window::EventLoop::new(&window);
        Ok(Self {
            surface,
            window,
            gfx,
            events,
            ctx,
            was_open: false,
            topmost: false,
            capture: None,
            last_capture: None,
            bg_tex: None,
            last_pos: None,
        })
    }

    /// 置顶状态跟随主窗口（幂等）。
    pub fn apply_topmost(&mut self, on: bool) {
        if self.topmost != on {
            self.topmost = on;
            crate::presence::set_topmost(self.window.hwnd(), on);
        }
    }

    /// 指针是否在本弹窗上（用于「悬停钉住」判定）。
    pub fn pointer_over(&self) -> bool {
        modular_clipboard_gfx::chrome::pointer_is_over(self.window.hwnd())
    }

    /// 输入框是否持有焦点。
    ///
    /// 判定用「本 Context 有焦点且焦点落在编辑区 id 上」——弹窗里
    /// 可能还有清除按钮等别的可聚焦控件，只判「有无焦点」会把点过
    /// 清除按钮的状态也当成正在打字。
    pub fn input_focused(&self, edit_id: egui::Id) -> bool {
        self.ctx.memory(|m| m.focused() == Some(edit_id))
    }

    /// 把弹窗移动到**屏幕**坐标（物理像素）。
    fn move_to_screen(&mut self, x_px: i32, y_px: i32) {
        if self.last_pos != Some((x_px, y_px)) {
            self.window.move_to(x_px, y_px);
            self.last_pos = Some((x_px, y_px));
        }
    }

    /// 刷新身后的桌面底图（节流 30fps；窗口移动时立即刷新）。
    ///
    /// # 为什么必须自己抓
    ///
    /// 弹窗的「圆角 + 投影」靠的是**不透明窗口 + 画上去的桌面底图**
    /// （见模块文档）。底图不更新时，窗口移动后圆角缝里露出的还是
    /// 上一处的桌面——移动时强制重抓就是为了这个。
    ///
    /// 捕获失败（GDI 异常 / 最小化）只是**本帧不更新**，`bg_tex`
    /// 里上一帧的图仍在，不会闪黑。
    fn update_backdrop(&mut self) {
        use modular_clipboard_gfx::backdrop::{capture_due, DesktopCapture, CAPTURE_INTERVAL};
        let now = std::time::Instant::now();
        if !capture_due(self.last_capture, now, CAPTURE_INTERVAL) {
            return;
        }
        // CAUTION: `swap_rb` 传 **true**：DIB 恒 BGRA，而 egui 的
        // `ColorImage` 要 RGBA。主窗口那条路径是直接上传给 GPU 图像
        // （格式与交换链一致，无需交换），这里走 egui 纹理，必须交换。
        let cap = match self.capture.as_mut() {
            Some(c) => c,
            None => match DesktopCapture::new(true) {
                Ok(c) => self.capture.insert(c),
                Err(e) => {
                    tracing::warn!(%e, "弹窗桌面捕获器初始化失败，底图停更");
                    return;
                }
            },
        };
        match cap.capture_behind(self.window.hwnd().0 as isize) {
            Ok(Some((bits, w, h))) => {
                // 捕获是客户区的 1/2 分辨率（`CAPTURE_DOWNSCALE`）：
                // 底图只需要「身后大概是什么」，省一半上传量。
                let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], bits);
                self.bg_tex = Some(self.ctx.load_texture(
                    "popup_backdrop",
                    img,
                    egui::TextureOptions::LINEAR,
                ));
                self.last_capture = Some(now);
            }
            // 最小化 ⇒ 跳过，保留上一帧底图。
            Ok(None) => {}
            Err(e) => tracing::debug!(%e, "弹窗桌面捕获失败，本帧沿用上一张底图"),
        }
    }

    /// 同步显隐并在需要时绘制一帧。
    ///
    /// - 隐藏态：泵消息并**排空**事件，不渲染（同设置窗口的纪律：
    ///   积压事件会在下次打开时一次性回放）；
    /// - 显示态：事件 → `run_ui`（内容由 `App::draw_popup` 提供）→
    ///   tessellate → 绘制。
    pub fn sync_and_render(
        &mut self,
        app: &mut crate::app::App,
        kind: PopupKind,
        want_open: bool,
        panel_pos_screen: egui::Vec2,
    ) -> anyhow::Result<()> {
        let just_opened = !self.was_open && want_open;
        self.was_open = want_open;

        if !want_open {
            self.events.poll_for(None);
            self.events.take_pending();
            self.window.set_visible(false);
            return Ok(());
        }

        // 定位：面板左上角的屏幕坐标（逻辑点）→ 物理像素。
        // 客户区比面板大一圈投影边距，弹窗左上角要减掉它。
        let s = self.window.scale_factor();
        let margin = SHADOW_MARGIN;
        let x_px = ((panel_pos_screen.x - margin) * s) as i32;
        let y_px = ((panel_pos_screen.y - margin) * s) as i32;
        self.move_to_screen(x_px, y_px);
        // 位子定好**之后**再抓身后桌面：捕获区间是本窗口的客户区，
        // 顺序反了就会抓到移动前那块区域。
        self.update_backdrop();
        let bg = self.bg_tex.as_ref().map(|t| t.id());
        // 首次显示：抬到 Z 序顶部，避免被主窗口盖住；搜索面板还要
        // 抢焦点（用户要立刻打字），类型栏保持不抢（no_activate）。
        if just_opened {
            crate::presence::raise_to_top(self.window.hwnd());
            if kind.activatable() {
                crate::presence::focus_window(self.window.hwnd());
            }
        }
        self.window.set_visible(true);

        // 客户区 0×0（不可见/最小化）时不能 acquire。
        let (cw, ch) = self.window.inner_size_physical();
        if cw == 0 || ch == 0 {
            self.events.poll_for(None);
            self.events.take_pending();
            return Ok(());
        }

        let frame_events = if just_opened {
            // 打开首帧丢弃积压输入：窗口刚创建、位置与焦点尚未稳定时，
            // 系统队列里可能混着落在这块区域上的残留点击/移动
            // （与设置窗口同一条纪律）。
            self.events.poll_for(None);
            let _ = self.events.take_pending();
            Vec::new()
        } else {
            self.events.poll_for(None);
            self.events.take_pending()
        };
        if frame_events
            .iter()
            .any(|e| matches!(e, WindowEvent::CloseRequested))
        {
            // 关窗请求等价于「收起」：交回状态机的唯一入口——
            // 直接改 `search_expanded` / `type_filter`，不绕过 Op。
            app.paint_state.search_expanded = false;
        }

        let raw_input = self.events.egui_input(&self.ctx, &frame_events);
        // 调色板与主窗口同步（各自 Context，`Visuals` 不共享）。
        crate::theme::apply(&self.ctx, app.palette());
        let mut out = self.ctx.run_ui(raw_input, |ui| app.draw_popup(ui, kind, bg));
        let ppp = out.pixels_per_point;
        let mut textures_delta = std::mem::take(&mut out.textures_delta);
        let primitives = self.ctx.tessellate(std::mem::take(&mut out.shapes), ppp);
        drop(out);

        self.gfx.rebuild_if_needed((cw, ch))?;
        let Some(acquired) = self.gfx.fr.acquire()? else {
            self.gfx.fr.rebuild_swapchain(Default::default())?;
            return Ok(());
        };
        let dpi_scale = self.window.scale_factor();
        let painted = self.gfx.painter.paint(
            &mut self.gfx.fr,
            &primitives,
            ppp,
            dpi_scale,
            &mut textures_delta,
        );
        if let Err(e) = painted {
            // 出错必须放弃本帧，否则该窗口永久死掉（见设置窗口同款注释）。
            self.gfx.fr.abandon_frame();
            return Err(e);
        }
        let _ = self.gfx.fr.present(acquired)?;
        // IMPORTANT: 必须 clear：`TexturesDelta` 的 `Drop` 有 debug 断言
        // 「未被应用的 delta」。
        textures_delta.clear();
        Ok(())
    }
}

impl Drop for Popup<'_> {
    fn drop(&mut self) {
        // 表面由 Gpu 创建、FrameRenderer 不销毁——必须自己收，
        // 否则每次重建弹窗泄漏一个 vk::SurfaceKHR。
        unsafe {
            self.gfx.gpu.destroy_surface(self.surface);
        }
        // chrome 里登记的面板形状随 HWND 失效即可（`Window::drop`
        // 会调 chrome::forget）。
    }
}

/// 两个悬浮弹窗的持有者：懒创建、隐藏时保留。
///
/// # 为什么隐藏而不是销毁
///
/// 设置窗口是「关掉即销毁」（实测过：不开设置省 65MB）。但那两个
/// 面板不同：
///
/// - 类型过滤栏**常驻可见**（它是常驻功能，不是临时面板）；
/// - 搜索面板展开/收起频繁，销毁重建要付「建交换链 + 灌字体图集」
///   的百毫秒级代价，收起就销毁会让「点放大镜」有可感延迟。
///
/// 两个弹窗合计最多占一份多余字体图集（~10~30MB 量级），换来交互
/// 无延迟。窗口矩形与交换链在隐藏期间不变化，显存也不涨。
#[derive(Default)]
pub struct Popups<'a> {
    search: Option<Popup<'a>>,
    rail: Option<Popup<'a>>,
}

impl<'a> Popups<'a> {
    /// 同步搜索面板弹窗（`want_open` = 搜索框应展开）。
    ///
    /// `panel_rect` 是**主窗口客户区坐标系**下的面板矩形（逻辑点，
    /// y 为负 = 在窗口上方），这里加上主窗口屏幕位置得到屏幕坐标。
    #[allow(clippy::too_many_arguments)]
    pub fn sync_search(
        &mut self,
        app: &mut crate::app::App,
        want_open: bool,
        panel_rect: Option<egui::Rect>,
        anchor_screen: egui::Vec2,
        scale_factor: f32,
        shared: &SharedGfx<'a>,
        topmost: bool,
    ) {
        let want_open = want_open && panel_rect.is_some();
        if want_open && self.search.is_none() {
            let size = panel_rect
                .map(|r| r.size())
                .unwrap_or_else(|| egui::vec2(720.0, 45.0));
            match Popup::create(PopupKind::Search, size, scale_factor, shared) {
                Ok(p) => {
                    tracing::info!("悬浮弹窗已创建：搜索面板");
                    self.search = Some(p);
                }
                Err(e) => {
                    tracing::warn!(%e, "创建搜索弹窗失败");
                    return;
                }
            }
        }
        let Some(p) = self.search.as_mut() else { return };
        p.apply_topmost(topmost);
        let rect = panel_rect.unwrap_or(egui::Rect::ZERO);
        let pos = anchor_screen + rect.min.to_vec2();
        if let Err(e) = p.sync_and_render(app, PopupKind::Search, want_open, pos) {
            tracing::warn!(%e, "搜索弹窗渲染失败");
        }
    }

    /// 同步类型过滤栏弹窗（常驻可见）。
    #[allow(clippy::too_many_arguments)]
    pub fn sync_rail(
        &mut self,
        app: &mut crate::app::App,
        want_open: bool,
        panel_rect: Option<egui::Rect>,
        anchor_screen: egui::Vec2,
        scale_factor: f32,
        shared: &SharedGfx<'a>,
        topmost: bool,
    ) {
        let want_open = want_open && panel_rect.is_some();
        if want_open && self.rail.is_none() {
            let size = panel_rect
                .map(|r| r.size())
                .unwrap_or_else(|| egui::vec2(32.0, 200.0));
            match Popup::create(PopupKind::Rail, size, scale_factor, shared) {
                Ok(p) => {
                    tracing::info!("悬浮弹窗已创建：类型栏");
                    self.rail = Some(p);
                }
                Err(e) => {
                    tracing::warn!(%e, "创建类型栏弹窗失败");
                    return;
                }
            }
        }
        let Some(p) = self.rail.as_mut() else { return };
        p.apply_topmost(topmost);
        let rect = panel_rect.unwrap_or(egui::Rect::ZERO);
        let pos = anchor_screen + rect.min.to_vec2();
        if let Err(e) = p.sync_and_render(app, PopupKind::Rail, want_open, pos) {
            tracing::warn!(%e, "类型栏弹窗渲染失败");
        }
    }

    /// 搜索弹窗当前的悬停 / 焦点状态（喂给主窗口的搜索状态机）。
    ///
    /// `(pointer_over, input_focused)`；弹窗不存在时都是 `false`。
    pub fn search_hover_focus(&self, edit_id: egui::Id) -> (bool, bool) {
        match &self.search {
            Some(p) => (p.pointer_over(), p.input_focused(edit_id)),
            None => (false, false),
        }
    }
}