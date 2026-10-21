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

use std::time::{Duration, Instant};

use modular_clipboard_gfx::window::{Window, WindowEvent};

use crate::multiwindow::{SharedGfx, WindowGfx};

/// 弹窗窗口相对内容的边距（逻辑点）。
///
/// 用来容纳**自研**高斯投影：窗口比内容大一圈，那圈是透明的（清屏
/// alpha=0），投影画在里面、向外渐隐到全透明。
///
/// 不能为 0：投影只能画在面板矩形之外，没有余量就只能压在面板边缘上、
/// 再被窗口边界裁掉 ⇒ 四边出现三角形暗块（踩过）。
///
/// # 由投影参数反推，别随手改小
///
/// 必须 ≥「投影最外层可见外扩 + 垂直偏移」：
/// `crate::paint::POPUP_SHADOW_BLUR`（10，σ=5）的最大可见 grow 是 11，
/// 加 `POPUP_SHADOW_OFFSET_Y`（2）= 13，取 14 留 1pt 余量。
///
/// 小于这个值 ⇒ 外层的层被**窗口边界硬切**，窗口最外圈像素留下一条
/// 等浓度的黑框——实机就是「弹窗有一圈黑色描边」。有测试守着
/// （`paint::tests::popup_shadow_decays_before_window_edge`），
/// 改任一侧都要同步另一侧。
pub const SHADOW_MARGIN: f32 = 14.0;

/// 弹窗**空闲**时的心跳重绘间隔。
///
/// 它是 [重绘闸门](RedrawGate) 的安全网，不是主要节流手段——主要手段
/// 是「没动静就不画」。存在的理由：弹窗自己的 `egui::Context` 看不见
/// **外部**状态变化（主窗口改了类型过滤、用户切了主题、列表项增减），
/// egui 不会为这些请求重绘，闸门自然也判不出来。
///
/// 100ms ⇒ 这类变化最坏 100ms 内可见（典型 50ms），而空闲时的渲染量
/// 从「主循环转速」降到 10Hz。
pub const IDLE_HEARTBEAT: Duration = Duration::from_millis(100);

/// 面板在弹窗客户区里的矩形（纯函数，便于测试）。
///
/// 窗口比面板大一圈 [`SHADOW_MARGIN`]，面板居中。用**整数像素**取整：
/// Win32 命中判定按整数像素，亚像素矩形会漏边（`WM_NCHITTEST` 据此
/// 决定投影区是否点击穿透）。
fn panel_rect_in(panel_size: egui::Vec2) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(SHADOW_MARGIN.round(), SHADOW_MARGIN.round()),
        egui::pos2(
            (SHADOW_MARGIN + panel_size.x).round(),
            (SHADOW_MARGIN + panel_size.y).round(),
        ),
    )
}

/// 由面板逻辑点尺寸换算窗口**客户区**物理像素尺寸。
///
/// `WS_POPUP` 的客户区 == 窗口区（`Window::create` 里已说明不加边框），
/// 所以这里算出来的就是 `SetWindowPos` 要的宽高。
///
/// 下限 1px：`set_size_physical` 会拒绝 0（客户区塌成 0×0 会让交换链
/// 重建失败、窗口看着像消失）。面板尺寸被算成 0（例如类型栏刚被关掉）
/// 时宁可留 1px，也不要把窗口弄没。
fn client_size_px(panel_size: egui::Vec2, scale: f32) -> (u32, u32) {
    let s = if scale > 0.01 { scale } else { 1.0 };
    let w = ((panel_size.x + SHADOW_MARGIN * 2.0) * s).round().max(1.0);
    let h = ((panel_size.y + SHADOW_MARGIN * 2.0) * s).round().max(1.0);
    (w as u32, h as u32)
}

/// 弹窗身份。既是内容分派的键，也是热区命名空间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PopupKind {
    /// 悬浮搜索面板（主窗口上方/下方，可激活）。
    Search,
    /// 类型过滤栏（主窗口某一侧，不抢焦点）。
    Rail,
    /// 通知弹窗（主窗口上方，不抢焦点；`ui.notice_popup` 开启时使用）。
    ///
    /// 与窗内 toast 是同一则通知的两种呈现（见
    /// `paint::draw_notice_toast` 的说明）。尺寸随文字长度每帧变化
    /// （`Popup::resize_to` 已支持动态尺寸）。
    Notice,
}

impl PopupKind {
    /// 窗口标题（不进任务栏，但调试时看得出是哪个）。
    pub fn title(self) -> &'static str {
        match self {
            PopupKind::Search => "ModularClipboard 搜索",
            PopupKind::Rail => "ModularClipboard 类型栏",
            PopupKind::Notice => "ModularClipboard 通知",
        }
    }

    /// 是否允许点击时抢占键盘焦点。
    ///
    /// 搜索面板要接收键盘（用户要打字）⇒ 可激活；类型栏与通知都只有
    /// 展示 ⇒ 用 `WS_EX_NOACTIVATE`，点它们时前台程序不被抢走。
    fn activatable(self) -> bool {
        matches!(self, PopupKind::Search)
    }
}

/// 重绘闸门：决定这一帧要不要真的跑一遍 egui + GPU。
///
/// # 为什么需要
///
/// 弹窗由**主循环**驱动，而主循环空转时也在跑（8ms 节流 ⇒ ~125Hz）。
/// 类型过滤栏是**常驻可见**的静态面板，照旧每帧走一遍
/// `run_ui` + `tessellate` + `acquire/paint/present`，等于以 125fps
/// 反复渲染两块不会动的像素——两份额外的 tessellation 与两次 GPU 提交，
/// 而主窗口本来就要付的那份一点没省。
///
/// # 判据（满足任一即重绘）
///
/// 1. **强制**（刚打开 / 本帧移动过 / 尺寸变过）；
/// 2. 本帧收到**窗口事件**（指针移动、点击、按键）——悬停与输入都靠它；
/// 3. **指针进出窗口**。
///    CAUTION: 本项目**没有** `WM_MOUSELEAVE` 事件（见
///    `gfx::window::WindowEvent`：只有 `MouseMoved`，没有离开）。
///    所以指针移出后**不会**再来事件，只能靠
///    `chrome::pointer_is_over` 轮询——少了这条，鼠标移出后按钮会
///    一直停在悬停态（最典型的「闸门一加就出 bug」）。
/// 4. **egui 焦点**变化；
/// 5. **尺寸 / DPI** 变化；
/// 6. egui 自己**请求了重绘**（`repaint_delay`，例如输入框光标闪烁）；
/// 7. 心跳 [`IDLE_HEARTBEAT`] 到期——安全网。
///
/// # 只在真正绘制时记账
///
/// 跳过帧时**不**更新任何字段：否则「指针离开又回来」这种在一次心跳
/// 内往返的变化会被记成「没变化」而丢掉那一帧。
/// （实践中进出都会产生 `MouseMoved`，但闸门不该依赖这个前提。）
#[derive(Debug, Default)]
struct RedrawGate {
    /// 上次真正绘制的时刻。
    last_draw: Option<Instant>,
    /// egui 请求的下次重绘时刻（由 `repaint_delay` 换算）。
    next_repaint: Option<Instant>,
    last_pointer_over: bool,
    last_focused: Option<egui::Id>,
    last_scale_bits: u32,
    last_extent: (u32, u32),
}

impl RedrawGate {
    /// 本帧是否需要重绘。`now` 由调用方传入（测试可注入）。
    fn needs_redraw(&self, q: RedrawQuery) -> bool {
        if q.forced || q.has_events {
            return true;
        }
        if q.pointer_over != self.last_pointer_over || q.focused != self.last_focused {
            return true;
        }
        if q.scale.to_bits() != self.last_scale_bits || q.extent != self.last_extent {
            return true;
        }
        if self.next_repaint.is_some_and(|t| q.now >= t) {
            return true;
        }
        // 从没画过 → 必须画。
        self.last_draw.is_none_or(|t| q.now.duration_since(t) >= IDLE_HEARTBEAT)
    }

    /// 画完一帧后记账（只在真正绘制后调用）。
    ///
    /// `repaint_delay` 取自本帧的 `egui::FullOutput`（由调用方用
    /// `renderer::repaint_delay` 换算）：`None` = egui 表示「不需要重绘」，
    /// `Some(d)` = 最迟 `d` 之后要再画一次（例如输入框光标闪烁）。
    ///
    /// 收 `Option<Duration>` 而不是 `&FullOutput`：闸门因此是**纯逻辑**，
    /// 测试不必真的跑一遍 egui（那要建 `Context`、代价不小且不稳）。
    fn note_drawn(&mut self, q: RedrawQuery, repaint_delay: Option<Duration>) {
        self.last_draw = Some(q.now);
        self.last_pointer_over = q.pointer_over;
        self.last_focused = q.focused;
        self.last_scale_bits = q.scale.to_bits();
        self.last_extent = q.extent;
        self.next_repaint = repaint_delay.map(|d| q.now + d);
    }
}

/// 一次闸门判定的输入（打包成结构体，避免 7 个位置参数）。
#[derive(Debug, Clone, Copy)]
struct RedrawQuery {
    now: Instant,
    /// 强制重绘（刚打开 / 移动过 / 尺寸变过）。
    forced: bool,
    has_events: bool,
    pointer_over: bool,
    /// egui 当前持有焦点的控件 id。
    focused: Option<egui::Id>,
    scale: f32,
    /// 物理客户区尺寸。
    extent: (u32, u32),
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
    /// 当前**面板**的逻辑点尺寸（窗口比它大一圈 [`SHADOW_MARGIN`]）。
    ///
    /// 尺寸是会变的（见 [`Popup::resize_to`]），窗口必须跟着改——
    /// 所以这里要记住「现在是多大」，才能判断要不要 resize。
    panel_size: egui::Vec2,
    /// 重绘闸门（[`RedrawGate`]）。
    gate: RedrawGate,
    /// 上一帧是否可见（用于「刚打开」判定）。
    was_open: bool,
    /// 已应用的置顶状态（幂等去重）。
    topmost: bool,
    /// 上次定位到的屏幕位置（避免每帧重复 `SetWindowPos`）。
    last_pos: Option<(i32, i32)>,
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
        let (cw, ch) = client_size_px(panel_size, scale_factor);
        let mut window =
            Window::new_popup(kind.title(), cw, ch, !kind.activatable(), owner)?;
        window.set_max_texture_side(shared.gpu.properties.limits.max_image_dimension2_d);
        // 圆角**自己画**，所以必须显式关掉 DWM 圆角：微软文档明确写了
        // 「每像素 alpha 分层的窗口无法被 DWM 圆角化」，而 DWM 若还在尝试
        // 圆角，我们自绘的投影就会被那一刀切掉（四个角的三角形）。
        modular_clipboard_gfx::chrome::disable_rounded_corners(window.hwnd());
        // 面板形状 = 窗口中央的内容矩形；之外那圈（投影区）点击穿透。
        // ——命中判定看窗口矩形而非像素，不登记就会吃掉底下窗口的点击。
        modular_clipboard_gfx::chrome::set_panel_shape(
            window.hwnd(),
            Some(panel_rect_in(panel_size)),
        );
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
            panel_size,
            gate: RedrawGate::default(),
            was_open: false,
            topmost: false,
            last_pos: None,
        })
    }

    /// 面板尺寸变了就把窗口跟着改大 / 改小。
    ///
    /// # 为什么不能只在创建时定一次
    ///
    /// `FloatLayout::compute` 每帧算出的面板尺寸是**会变的**：
    ///
    /// - 类型栏的按钮数 = 「历史上出现过哪几类」（`FloatLayout::kinds`），
    ///   复制了第一张图片就多一枚按钮；
    /// - 用户在设置里拖「类型栏尺寸」滑块，每个按钮都变大；
    /// - 搜索面板宽度 = 主窗口宽度，用户拖边缘 resize 时它跟着变。
    ///
    /// 窗口尺寸却只在 `create` 时定过一次 ⇒ 新增的按钮被画在**客户区
    /// 之外**：既看不见（被窗口边界裁掉），也点不到（命中判定按窗口
    /// 矩形，窗口外根本没有本窗口的 HWND）。
    ///
    /// # 副作用
    ///
    /// 命中穿透的面板形状要按新尺寸重登记；位置也要强制重算——
    /// 面板几何是「居中 / 贴边」算出来的，尺寸变了锚点就变了。
    /// 交换链由 `sync_and_render` 里的 `rebuild_if_needed` 跟进。
    fn resize_to(&mut self, panel_size: egui::Vec2) {
        // 亚像素抖动不必 resize（resize 会牵动交换链重建，代价不小）。
        if (self.panel_size.x - panel_size.x).abs() < 0.5
            && (self.panel_size.y - panel_size.y).abs() < 0.5
        {
            return;
        }
        let (w, h) = client_size_px(panel_size, self.window.scale_factor());
        self.window.set_size_physical(w, h);
        self.panel_size = panel_size;
        modular_clipboard_gfx::chrome::set_panel_shape(
            self.window.hwnd(),
            Some(panel_rect_in(panel_size)),
        );
        // 尺寸变了 ⇒ 几何锚点变了 ⇒ 下一帧必须重新定位。
        self.last_pos = None;
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

    /// 把弹窗移动到**屏幕**坐标（物理像素）。返回是否真的移动了。
    fn move_to_screen(&mut self, x_px: i32, y_px: i32) -> bool {
        if self.last_pos == Some((x_px, y_px)) {
            return false;
        }
        self.window.move_to(x_px, y_px);
        self.last_pos = Some((x_px, y_px));
        true
    }

    /// 同步显隐，并在**需要时**绘制一帧。
    ///
    /// - 隐藏态：泵消息并**排空**事件，不渲染（同设置窗口的纪律：
    ///   积压事件会在下次打开时一次性回放）；
    /// - 显示态：**先过一遍[重绘闸门](RedrawGate)**——没动静就直接返回，
    ///   窗口保持上一帧的画面；有动静才走
    ///   事件 → `run_ui`（内容由 `App::draw_popup` 提供）→ tessellate → 绘制。
    ///
    /// CAUTION: 消息是**每一帧都泵**的（跳过绘制也泵）。不泵的窗口会被
    /// Win32 判成无响应；而绘制只是「把状态画出来」，可以省。
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
        let s = self.window.scale_factor();
        let margin = SHADOW_MARGIN;
        let x_px = ((panel_pos_screen.x - margin) * s) as i32;
        let y_px = ((panel_pos_screen.y - margin) * s) as i32;
        let moved = self.move_to_screen(x_px, y_px);
        // 首次显示：抬到 Z 序顶部，避免被主窗口盖住；搜索面板还要
        // 抢焦点（用户要立刻打字），类型栏保持不抢（no_activate）。
        if just_opened {
            // 入场动画重播：弹窗 ctx **跨展开/收起存活**（本函数只切
            // `set_visible`，窗口与 Context 都不重建），`("popup_in", kind)`
            // 的补间条目在第一次展开后就一直躺在 data store 里 ⇒
            // `anim::fade_in_once` 判「首次出现」不再成立，之后每次展开
            // 都没有滑入动效。这里显式清掉，让每次展开都重播一遍。
            // （绘制侧的同名 id 见 `App::draw_popup`。）
            crate::anim::clear(&self.ctx, egui::Id::new(("popup_in", kind)));
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
            //
            // IMPORTANT: 必须对**主窗口**请求重绘。顶栏的搜索框是主窗口
            // 画的，而这里的写入发生在弹窗自己的 `Context` 里 ⇒ 主窗口
            // 既收不到事件也不请求重绘，渲染闸门会整帧跳过，画面停在
            // 「搜索框还展开着」而弹窗已经没了（两块 UI 对不上）。
            app.paint_state.search_expanded = false;
            app.ctx.request_repaint();
        }

        // ---- 重绘闸门 ----
        //
        // 指针进出用 `pointer_over()` 轮询：本项目**没有**鼠标离开事件，
        // 少了这条，指针移出后按钮会一直停在悬停态。
        let q = RedrawQuery {
            now: Instant::now(),
            forced: just_opened || moved,
            has_events: !frame_events.is_empty(),
            pointer_over: self.pointer_over(),
            focused: self.ctx.memory(|m| m.focused()),
            scale: s,
            extent: (cw, ch),
        };
        if !self.gate.needs_redraw(q) {
            return Ok(());
        }

        let raw_input = self.events.egui_input(&self.ctx, &frame_events);
        // 调色板与主窗口同步（各自 Context，`Visuals` 不共享）。
        crate::theme::apply(&self.ctx, app.palette());
        let mut out = self.ctx.run_ui(raw_input, |ui| app.draw_popup(ui, kind));
        let ppp = out.pixels_per_point;
        // CAUTION: 从这里开始的**每一个**离开路径都必须清空 delta
        // （守卫兜住 `acquire` 返回 None、`paint` 报错这两处早退）。
        // release 下漏清不会 panic，而是那一帧的字体图集增量被静默丢弃
        // ⇒ 弹窗空白/缺字形且不自愈（egui 认为已传过）。
        let mut textures_delta = crate::app::DeltaGuard(std::mem::take(&mut out.textures_delta));
        let primitives = self.ctx.tessellate(std::mem::take(&mut out.shapes), ppp);
        // 记账必须在 `out` 被拆散之前：`repaint_delay` 只看
        // `viewport_output`，而拆走 `shapes`/`textures_delta` 后字段还在，
        // 顺序其实无所谓——放在这里是为了让「闸门记账」紧挨着「跑完 UI」。
        self.gate.note_drawn(q, crate::renderer::repaint_delay(&out));
        drop(out);

        self.gfx.rebuild_if_needed((cw, ch))?;
        let Some(acquired) = self.gfx.fr.acquire()? else {
            self.gfx.fr.rebuild_swapchain(Default::default())?;
            // 交换链刚重建、本帧又没画成 ⇒ 图像内容是**未定义的**
            // （新分配的交换链图像不保证已清零）。闸门若按「刚画过」
            // 跳过下一帧，窗口会黑着/花着最多一个心跳间隔。
            // 清掉记账 ⇒ 下一帧必定重绘。
            self.gate = RedrawGate::default();
            return Ok(());
        };
        let dpi_scale = self.window.scale_factor();
        // 弹窗**不走** GPU 磨砂通道（`paint_backdrop`）——不透明面板。
        //
        // 那条链路是按主窗口调的，套在弹窗上有三处不对：
        // ① 底图是**铺满窗口矩形**的全屏 blit，没有形状参数 ⇒ 无法与
        //    面板的圆角轮廓对齐（方底图 + 圆角面板 = 四角露三角形）；
        // ② UV 位移（`backdrop_uv_offset`）用**屏幕点**算 dx 却除以
        //    **物理像素**的窗口尺寸，单位错配；
        // ③ 弹窗创建于 (0,0) 再移到目标位置，运动预测会给出很大的位移，
        //    底图因此整体偏出去（实机「偏移还挺大的」）。
        // 弹窗是几十像素的小面板，磨砂收益远小于这些代价。
        //
        // NOTE: 别再拿「`Painter` 只有一个用户纹理槽」当理由——那条已经
        // 不成立：用户纹理现在是「一张表 + 每张一个专属描述符集合」
        // （`USER_TEXTURE_SET_CAP`，见 `renderer.rs`）。不磨砂的理由是
        // 上面三条几何问题，与纹理槽无关。
        let painted = self.gfx.painter.paint(
            &mut self.gfx.fr,
            &primitives,
            ppp,
            dpi_scale,
            &mut textures_delta.0,
        );
        if let Err(e) = painted {
            // 出错必须放弃本帧，否则该窗口永久死掉（见设置窗口同款注释）。
            // `textures_delta` 由守卫在返回时清空，不必在此手工收尾。
            self.gfx.fr.abandon_frame();
            return Err(e);
        }
        let _ = self.gfx.fr.present(acquired)?;
        Ok(())
    }
}

impl Drop for Popup<'_> {
    fn drop(&mut self) {
        // 表面由 Gpu 创建、FrameRenderer 不销毁——必须自己收，
        // 否则每次重建弹窗泄漏一个 vk::SurfaceKHR。
        //
        // 2026-10-19 修（与设置窗口/卡片子窗同款，三处一起）：
        // 先抽干残留 posted 消息（hwnd 还有效，窗口销毁后它们永远摘不走，
        // 线程队列会随弹窗开关累积到系统上限 ⇒ 系统判「未响应」），
        // 再等 GPU 空闲（in-flight 命令可能仍在读 surface），
        // 最后销毁表面。
        self.events.drain_pending();
        self.gfx.gpu.wait_idle();
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
    notice: Option<Popup<'a>>,
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
        owner: Option<windows::Win32::Foundation::HWND>,
    ) {
        Self::sync_one(
            &mut self.search,
            app,
            PopupKind::Search,
            want_open,
            panel_rect,
            anchor_screen,
            scale_factor,
            shared,
            topmost,
            owner,
            // 只在「拿到真实布局之前」用一次的兜底尺寸。
            egui::vec2(720.0, 45.0),
        );
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
        owner: Option<windows::Win32::Foundation::HWND>,
    ) {
        Self::sync_one(
            &mut self.rail,
            app,
            PopupKind::Rail,
            want_open,
            panel_rect,
            anchor_screen,
            scale_factor,
            shared,
            topmost,
            owner,
            egui::vec2(32.0, 200.0),
        );
    }

    /// 两个弹窗的同步逻辑**完全相同**，只有槽位、种类与兜底尺寸不同。
    ///
    /// 收敛成一个实现而不是复制两份：早前 `childwin` / `settingswin` /
    /// 测试夹具各存一份「装字体」的代码，其中两份传了 `None`，同一界面
    /// 里出现两种字体——典型的复制粘贴漂移（见 `SharedGfx::new_context`
    /// 的说明）。这里不重蹈覆辙。
    #[allow(clippy::too_many_arguments)]
    fn sync_one(
        slot: &mut Option<Popup<'a>>,
        app: &mut crate::app::App,
        kind: PopupKind,
        want_open: bool,
        panel_rect: Option<egui::Rect>,
        anchor_screen: egui::Vec2,
        scale_factor: f32,
        shared: &SharedGfx<'a>,
        topmost: bool,
        owner: Option<windows::Win32::Foundation::HWND>,
        fallback_size: egui::Vec2,
    ) {
        let want_open = want_open && panel_rect.is_some();
        if want_open && slot.is_none() {
            let size = panel_rect.map_or(fallback_size, |r| r.size());
            match Popup::create(kind, size, scale_factor, shared, owner) {
                Ok(p) => {
                    tracing::info!("悬浮弹窗已创建：{}", kind.title());
                    *slot = Some(p);
                }
                Err(e) => {
                    tracing::warn!(%e, "创建悬浮弹窗失败：{}", kind.title());
                    return;
                }
            }
        }
        let Some(p) = slot.as_mut() else { return };
        p.apply_topmost(topmost);
        let rect = panel_rect.unwrap_or(egui::Rect::ZERO);
        // 尺寸跟着本帧的布局走（按钮数增减、主窗口变宽都会改尺寸，
        // 见 `Popup::resize_to`）。只在显示态 resize：收起时
        // `panel_rect` 可能是零矩形，拿它改窗口会把窗口压没。
        if want_open {
            p.resize_to(rect.size());
        }
        let pos = anchor_screen + rect.min.to_vec2();
        if let Err(e) = p.sync_and_render(app, kind, want_open, pos) {
            tracing::warn!(%e, "悬浮弹窗渲染失败：{}", kind.title());
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

    /// 同步通知弹窗（`ui.notice_popup` 开启时的通知呈现）。
    ///
    /// 与另两个弹窗共用 `sync_one`：懒创建、隐藏保留、尺寸随布局。
    /// `panel_rect` 来自 `UiState::notice_panel`（每帧由主窗口绘制层
    /// 按文字长度算好，见 `paint::notice_panel_rect`）。
    #[allow(clippy::too_many_arguments)]
    pub fn sync_notice(
        &mut self,
        app: &mut crate::app::App,
        want_open: bool,
        panel_rect: Option<egui::Rect>,
        anchor_screen: egui::Vec2,
        scale_factor: f32,
        shared: &SharedGfx<'a>,
        topmost: bool,
        owner: Option<windows::Win32::Foundation::HWND>,
    ) {
        Self::sync_one(
            &mut self.notice,
            app,
            PopupKind::Notice,
            want_open,
            panel_rect,
            anchor_screen,
            scale_factor,
            shared,
            topmost,
            owner,
            // 首帧兜底尺寸（拿到真实布局前的一帧）。
            egui::vec2(200.0, 40.0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个「什么都没发生」的查询（各项都等于静止时的取值）。
    fn idle_query(now: Instant) -> RedrawQuery {
        RedrawQuery {
            now,
            forced: false,
            has_events: false,
            pointer_over: false,
            focused: None,
            scale: 1.0,
            extent: (100, 50),
        }
    }

    /// 空闲帧的 `repaint_delay`：`None` = egui 表示「这一帧不需要重绘」。
    const IDLE: Option<Duration> = None;

    // ---- 几何 ----

    #[test]
    fn panel_rect_leaves_a_shadow_margin_on_each_side() {
        let r = panel_rect_in(egui::vec2(720.0, 45.0));
        assert_eq!(r.min, egui::pos2(SHADOW_MARGIN, SHADOW_MARGIN));
        assert_eq!(r.size(), egui::vec2(720.0, 45.0));
    }

    #[test]
    fn panel_rect_rounds_to_whole_pixels() {
        // Win32 命中判定按整数像素，亚像素矩形会漏边。
        let r = panel_rect_in(egui::vec2(10.4, 10.6));
        assert_eq!(
            r.max.x,
            (SHADOW_MARGIN + 10.4).round(),
            "{SHADOW_MARGIN} + 10.4 = {} 应取整到整像素",
            SHADOW_MARGIN + 10.4
        );
        assert_eq!(
            r.max.y,
            (SHADOW_MARGIN + 10.6).round(),
            "{SHADOW_MARGIN} + 10.6 = {} 应取整到整像素",
            SHADOW_MARGIN + 10.6
        );
    }

    #[test]
    fn client_size_is_panel_plus_two_margins_in_physical_pixels() {
        // 用常量算期望值：`SHADOW_MARGIN` 由投影参数反推，会跟着调。
        let m = SHADOW_MARGIN * 2.0;
        assert_eq!(
            client_size_px(egui::vec2(100.0, 50.0), 1.0),
            ((100.0 + m) as u32, (50.0 + m) as u32)
        );
        assert_eq!(
            client_size_px(egui::vec2(100.0, 50.0), 1.5),
            (((100.0 + m) * 1.5) as u32, ((50.0 + m) * 1.5) as u32)
        );
    }

    #[test]
    fn client_size_never_collapses_to_zero() {
        // 0 会被 `set_size_physical` 拒绝（客户区塌成 0×0 ⇒ 交换链重建失败、
        // 窗口看着像消失），所以只留投影边距也算一个合法尺寸。
        let m = (SHADOW_MARGIN * 2.0) as u32;
        assert_eq!(client_size_px(egui::vec2(0.0, 0.0), 1.0), (m, m));
        // 退化缩放按 1.0 处理，不能算出 0。
        assert_eq!(
            client_size_px(egui::vec2(10.0, 10.0), 0.0),
            (10 + m, 10 + m)
        );
    }

    // ---- 重绘闸门 ----

    #[test]
    fn gate_always_draws_the_first_frame() {
        let g = RedrawGate::default();
        assert!(g.needs_redraw(idle_query(Instant::now())));
    }

    #[test]
    fn gate_skips_when_nothing_happened() {
        let t = Instant::now();
        let mut g = RedrawGate::default();
        g.note_drawn(idle_query(t), IDLE);
        assert!(!g.needs_redraw(idle_query(t + Duration::from_millis(10))));
    }

    #[test]
    fn gate_draws_after_the_idle_heartbeat() {
        let t = Instant::now();
        let mut g = RedrawGate::default();
        g.note_drawn(idle_query(t), IDLE);
        assert!(!g.needs_redraw(idle_query(t + IDLE_HEARTBEAT / 2)));
        assert!(g.needs_redraw(idle_query(t + IDLE_HEARTBEAT)));
    }

    #[test]
    fn gate_draws_on_window_events() {
        let t = Instant::now();
        let mut g = RedrawGate::default();
        g.note_drawn(idle_query(t), IDLE);
        let mut q = idle_query(t + Duration::from_millis(1));
        q.has_events = true;
        assert!(g.needs_redraw(q));
    }

    #[test]
    fn gate_draws_when_pointer_enters_or_leaves() {
        // 本项目**没有**鼠标离开事件（见 `gfx::window::WindowEvent`）：
        // 指针移出后不会再有事件进来，只能靠 `pointer_over` 翻转判定。
        // 这条回归测试守的就是它——少了这条，按钮会一直停在悬停态。
        let t = Instant::now();
        let mut g = RedrawGate::default();
        g.note_drawn(idle_query(t), IDLE);

        let mut q = idle_query(t + Duration::from_millis(1));
        q.pointer_over = true;
        assert!(g.needs_redraw(q), "指针进入必须重绘");

        g.note_drawn(q, IDLE);
        q.pointer_over = false;
        assert!(g.needs_redraw(q), "指针离开必须重绘");
    }

    #[test]
    fn gate_draws_on_focus_change() {
        let t = Instant::now();
        let mut g = RedrawGate::default();
        g.note_drawn(idle_query(t), IDLE);

        let mut q = idle_query(t + Duration::from_millis(1));
        q.focused = Some(egui::Id::new("search_edit"));
        assert!(g.needs_redraw(q), "获得焦点必须重绘");

        g.note_drawn(q, IDLE);
        q.focused = None;
        assert!(g.needs_redraw(q), "失去焦点必须重绘");
    }

    #[test]
    fn gate_draws_on_size_or_dpi_change() {
        let t = Instant::now();
        let mut g = RedrawGate::default();
        g.note_drawn(idle_query(t), IDLE);

        let mut q = idle_query(t + Duration::from_millis(1));
        q.extent = (200, 50);
        assert!(g.needs_redraw(q), "客户区尺寸变了必须重绘");

        g.note_drawn(q, IDLE);
        q.scale = 1.5;
        assert!(g.needs_redraw(q), "DPI 变了必须重绘");
    }

    #[test]
    fn gate_draws_when_egui_requests_repaint() {
        let t = Instant::now();
        let mut g = RedrawGate::default();
        // 输入框光标闪烁这类「自己请求、带延时」的重绘。
        g.note_drawn(idle_query(t), Some(Duration::from_millis(500)));
        assert!(
            !g.needs_redraw(idle_query(t + Duration::from_millis(50))),
            "延时未到不该重绘——否则省不掉任何帧"
        );
        assert!(g.needs_redraw(idle_query(t + Duration::from_millis(500))));
    }

    #[test]
    fn gate_draws_immediately_when_egui_asks_for_it() {
        let t = Instant::now();
        let mut g = RedrawGate::default();
        g.note_drawn(idle_query(t), Some(Duration::ZERO));
        assert!(
            g.needs_redraw(idle_query(t)),
            "egui 请求立即重绘 ⇒ 下一帧就画（动画/打字不能被闸门挡住）"
        );
    }
}