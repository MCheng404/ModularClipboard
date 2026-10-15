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
//! # 圆角与投影：**真·逐像素透明**
//!
//! 交换链是 `PRE_MULTIPLIED`、渲染管线的混合也是预乘 alpha
//! （`src=ONE, dst=ONE_MINUS_SRC_ALPHA`），所以窗口**本来就能**按像素
//! 透明——前提是**清屏色必须是透明的**。
//!
//! `FrameRenderer` 的默认清屏色是 `[0.10, 0.10, 0.12, 1.0]`（不透明深色，
//! 主窗口需要它兜底）。早前的弹窗沿用这个默认值，于是「面板之外什么
//! 都不画」的区域被清成不透明深色 ⇒ 实机看到的是黑块与黑色直角。
//! 这个现象被我误判成「DWM 不支持 alpha」，于是去抓桌面当底图——
//! 方向错了。
//!
//! 现在的做法（用户拍板去掉底图）：
//! 1. 弹窗把清屏色设为 `[0,0,0,0]`（`set_clear_color`），没画到的地方
//!    **真的透明**；
//! 2. 窗口 = 内容 + 一圈 [`SHADOW_MARGIN`]，圆角与高斯投影都自己画，
//!    画在透明画布上、alpha 渐变到 0 ⇒ 天然的圆角与柔和投影；
//! 3. 不再抓桌面、不再需要 `WDA_EXCLUDEFROMCAPTURE`（弹窗因此能正常
//!    出现在截图/录屏里）；
//! 4. 命中判定仍按**窗口矩形**而非像素，所以投影区要登记
//!    `set_panel_shape` 让 `WM_NCHITTEST` 返回 `HTTRANSPARENT`。

use modular_clipboard_gfx::window::{Window, WindowEvent};

use crate::multiwindow::{SharedGfx, WindowGfx};

/// 弹窗窗口相对内容的边距（逻辑点）。
///
/// **恒为 0**：窗口矩形就等于内容矩形，圆角由 **DWM 系统圆角**裁出，
/// 磨砂底图由渲染器的 GPU 通道铺满整个窗口——两者都以「窗口即面板」
/// 为前提。留边距的话那圈既没有内容也没有底图，只会是黑的。
pub const SHADOW_MARGIN: f32 = 0.0;

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
    /// 上次定位到的屏幕位置（避免每帧重复 `SetWindowPos`）。
    last_pos: Option<(i32, i32)>,
    /// 已应用的捕获自排除状态（`None` = 未设过）。
    affinity_set: Option<modular_clipboard_core::config::WindowBackdrop>,
}

impl<'a> Popup<'a> {
    /// 按**面板**的逻辑点尺寸创建弹窗（客户区自动加一圈 [`SHADOW_MARGIN`]）。
    pub fn create(
        kind: PopupKind,
        panel_size: egui::Vec2,
        scale_factor: f32,
        shared: &SharedGfx<'a>,
        owner: Option<windows::Win32::Foundation::HWND>,
    ) -> anyhow::Result<Self> {
        let s = if scale_factor > 0.01 { scale_factor } else { 1.0 };
        let client = egui::vec2(panel_size.x + SHADOW_MARGIN * 2.0, panel_size.y + SHADOW_MARGIN * 2.0);
        let mut window = Window::new_popup(
            kind.title(),
            (client.x * s) as u32,
            (client.y * s) as u32,
            !kind.activatable(),
            owner,
        )?;
        window.set_max_texture_side(shared.gpu.properties.limits.max_image_dimension2_d);
        // 圆角交给 DWM（与主窗口同一套）：窗口本身就是那块面板。
        modular_clipboard_gfx::chrome::apply_rounded_corners(window.hwnd());
        // 面板形状 = 整个客户区（窗口即面板）。
        // ——命中判定看窗口矩形而非像素，不登记就会吃掉底下窗口的点击。
        // 用**整数像素**登记：Win32 命中按整数像素，亚像素矩形会漏边。
        // （egui 0.36 的 `Rect` 没有 `round()`，自己取整。）
        let panel = egui::Rect::from_min_max(
            egui::pos2(
                (SHADOW_MARGIN).round(),
                (SHADOW_MARGIN).round(),
            ),
            egui::pos2(
                (SHADOW_MARGIN + panel_size.x).round(),
                (SHADOW_MARGIN + panel_size.y).round(),
            ),
        );
        modular_clipboard_gfx::chrome::set_panel_shape(window.hwnd(), Some(panel));
        // NOTE: 不再设 `WDA_EXCLUDEFROMCAPTURE`：那是给「抓身后桌面」用的
        // （不加会抓到自己的上一帧）。现在靠真透明，没有捕获，也就不该
        // 让弹窗从截图/录屏里消失。

        let surface = shared.gpu.create_surface_for(window.hinstance(), window.hwnd())?;
        let mut gfx = WindowGfx::new_for(shared, surface)?;
        // 真透明的关键一步：清屏色 alpha = 0。默认是 `[0.10,0.10,0.12,1.0]`
        // （主窗口要它兜底），弹窗沿用就会把没画到的地方清成不透明深色。
        gfx.fr.set_clear_color([0.0, 0.0, 0.0, 0.0]);
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
            last_pos: None,
            affinity_set: None,
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
        acrylic: bool,
        blur_strength: u8,
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
        let s = self.window.scale_factor();
        let margin = SHADOW_MARGIN;
        let x_px = ((panel_pos_screen.x - margin) * s) as i32;
        let y_px = ((panel_pos_screen.y - margin) * s) as i32;
        self.move_to_screen(x_px, y_px);
        // 磨砂需要**捕获自排除**：GPU 磨砂通道会抓本窗口身后的桌面，
        // 不排除就会抓到自己的上一帧（反馈回路，表现为拖影/自叠加）。
        // 状态变化才调（`affinity_set` 去重）。
        use modular_clipboard_core::config::WindowBackdrop;
        let want = if acrylic { WindowBackdrop::Acrylic } else { WindowBackdrop::None };
        if self.affinity_set.is_none_or(|m| m != want) {
            crate::presence::apply_backdrop(self.window.hwnd(), want);
            self.affinity_set = Some(want);
        }
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
        let mut out = self.ctx.run_ui(raw_input, |ui| app.draw_popup(ui, kind));
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
        // 磨砂走渲染器自带的 **GPU 磨砂通道**（与主窗口同一套：实时捕获
        // 身后桌面 + 三趟高斯卷积，画成底图后再叠界面）。
        //
        // CAUTION: 不能自己抓桌面当 egui 纹理用——`Painter` 只有**一个
        // 用户纹理槽**（`user_tex`），缩略图也占这个槽，两者同时出现会
        // 互相顶替：采样到的是另一张纹理，实机表现为「磨砂是白的」。
        let painted = if acrylic {
            self.gfx.painter.paint_backdrop(
                &mut self.gfx.fr,
                self.window.hwnd().0 as isize,
                modular_clipboard_gfx::blur::radius_for_strength(blur_strength),
                &primitives,
                ppp,
                dpi_scale,
                &mut textures_delta,
            )
        } else {
            self.gfx.painter.paint(
                &mut self.gfx.fr,
                &primitives,
                ppp,
                dpi_scale,
                &mut textures_delta,
            )
        };
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
        acrylic: bool,
        blur_strength: u8,
        owner: Option<windows::Win32::Foundation::HWND>,
    ) {
        let want_open = want_open && panel_rect.is_some();
        if want_open && self.search.is_none() {
            let size = panel_rect
                .map(|r| r.size())
                .unwrap_or_else(|| egui::vec2(720.0, 45.0));
            match Popup::create(PopupKind::Search, size, scale_factor, shared, owner) {
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
        if let Err(e) =
            p.sync_and_render(app, PopupKind::Search, want_open, pos, acrylic, blur_strength)
        {
            tracing::warn!(%e, "搜索弹窗渲染失败");
        }
    }

    /// 同步类型过滤栏弹窗（常驻可见）。
    #[allow(clippy::too_many_arguments)]
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
        acrylic: bool,
        blur_strength: u8,
        owner: Option<windows::Win32::Foundation::HWND>,
    ) {
        let want_open = want_open && panel_rect.is_some();
        if want_open && self.rail.is_none() {
            let size = panel_rect
                .map(|r| r.size())
                .unwrap_or_else(|| egui::vec2(32.0, 200.0));
            match Popup::create(PopupKind::Rail, size, scale_factor, shared, owner) {
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
        if let Err(e) =
            p.sync_and_render(app, PopupKind::Rail, want_open, pos, acrylic, blur_strength)
        {
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