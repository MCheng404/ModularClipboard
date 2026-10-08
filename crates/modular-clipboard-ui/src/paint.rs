//! 绘制层：遍历工作区，按 `card.rect` 把卡片画出来。
//!
//! # 本模块的第一原则：**不做二次判断**
//!
//! 旧架构的历史症状几乎都源于「绘制层自己又判了一遍」：
//!
//! - 详情卡片折叠后，`draw` 里的 `if local.show_detail` 让整个绘制函数
//!   被跳过，可布局层照样分配了矩形 ⇒ **那块矩形无人绘制**，与相邻栏
//!   同色，看起来像两块糊在一起；
//! - 状态栏左侧提示按「预算」算宽度，实际字体偏大就压到右侧按钮上。
//!
//! 所以这里的规则是硬的：
//!
//! 1. **矩形只从 [`Card::rect`] 读**，不在这里算宽度、不做降级判断。
//! 2. **每张停靠卡片都必须被画**——折叠的画成把手，展开的画成卡片，
//!    没有「要不要画」这种分支。
//! 3. **任何文字都必须 `painter_at(所属矩形)`**。`ui.painter()` 是根
//!    painter、完全没有裁剪，用它画文字就是允许溢出到别的卡片上。
//!
//! # 为什么第3 条要单独强调
//!
//! 渲染层已经实现逐批次 scissor（见 `renderer::Batch::to_draw`），
//! 但那只保证「不超出**批次**的裁剪矩形」。而 `ui.painter()` 画出的
//! 文字，其批次裁剪矩形就是**整屏**——裁剪等于没裁。要真正限制在
//! 卡片内，必须显式传对应矩形给 `painter_at`。

use egui::{Align2, CornerRadius, FontId, Rect, Sense, Stroke, Ui, pos2, vec2};
use modular_clipboard_app::Service;
use modular_clipboard_core::ClipItem;

use crate::card::{Card, CardHost, CardKind, PinnedMode};
use crate::theme::{Palette, sized};
use crate::workspace::Workspace;

/// 卡片边框宽度（逻辑点）。
const CARD_STROKE: f32 = 1.0;

/// 卡片圆角（逻辑点）。
/// 卡片圆角（逻辑点）。
///
/// ⚠️ **取 0（直角）**，不是原先的 8。
///
/// 圆角会在相邻卡片之间与窗口边缘留下 4 个「缺口」：卡片明明
/// 紧贴（实测底部间隙 0px），但视觉上像有一条缝——用户报的
/// 「窗口与窗口之间存在间隙」有一部分就来自这里。
/// 直角 + 1px 描边才能得到连续的、真正无缝的面板。
///
/// 这也符合本项目「**纯平毛玻璃**」的视觉语言：禁止一切装饰性
/// 圆角带来的拟物暗示。
const CARD_RADIUS: f32 = 0.0;

/// 一帧内要执行的操作。
///
/// 绘制层**只识别操作、不改布局**：`Workspace` 的改动由上层在
/// `apply_ops` 里做。这样「点了折叠按钮」这件事不会分散在绘制代码里。
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// 切换某张卡片的折叠态。
    ToggleCollapse(crate::card::CardId),
    /// 把卡片分离成子窗口。
    Detach(crate::card::CardId),
    /// 把卡片收回工作区。
    Dock(crate::card::CardId),
    /// 选中某个条目。
    Select(modular_clipboard_core::EntryId),
    /// 切换条目置顶。
    TogglePin(modular_clipboard_core::EntryId),
    /// 删除条目。
    Delete(modular_clipboard_core::EntryId),
    /// 复制条目到剪贴板。
    Copy(modular_clipboard_core::EntryId),
    /// 置顶模式切换。
    SetPinnedMode(PinnedMode),

    // ---- 设置界面 ----
    //
    // ⚠️ 配置项**不能**在 paint 里直接改：`Frame` 借的是 `&UiConfig`，
    // 且改完还要落盘（`svc.save_config()`）。统一走 Op 由 `App` 落地。

    /// 改一个布尔配置项。
    SetBool {
        /// 配置分组。
        group: ConfigGroup,
        /// 组内字段名（见 `set_bool_field`）。
        field: &'static str,
        /// 目标值。
        value: bool,
    },
    /// 改一个数值配置项。
    SetNumber {
        group: ConfigGroup,
        field: &'static str,
        value: f64,
    },
    /// 改一个可选数值（`None` = 不限）。
    SetOptNumber {
        group: ConfigGroup,
        field: &'static str,
        /// `None` 表示「不限」。
        value: Option<f64>,
    },
    /// 打开/关闭设置界面。
    ToggleSettings,
    /// 清空全部历史。
    ClearAll,
}

/// 配置分组。
///
/// 设置界面的每个可调项都归到某一组；`App` 侧用
/// [`ConfigGroup`] + 字段名定位到具体字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigGroup {
    Ui,
    Capture,
    Storage,
}

impl ConfigGroup {
    /// 分组标题（设置界面里显示）。
    pub fn title(self) -> &'static str {
        match self {
            ConfigGroup::Ui => "界面",
            ConfigGroup::Capture => "捕获",
            ConfigGroup::Storage => "存储",
        }
    }
}

/// 绘制上下文：跨帧保留的界面状态。
#[derive(Default)]
pub struct UiState {
    /// 本帧收集到的操作。
    pub ops: Vec<Op>,
    /// 搜索框文本。
    pub query: String,
    /// 上一帧提交给窗口的拖动区（标题栏）。
    pub drag_region: Option<Rect>,
    /// 当前悬停的卡片。
    pub hover_card: Option<crate::card::CardId>,
    /// 列表滚动偏移（逻辑点）。
    pub scroll_offset: f32,
    /// 设置界面是否打开。
    pub show_settings: bool,
    /// 设置面板的滚动偏移（逻辑点）。
    pub settings_scroll: f32,
    /// 关闭 / 最小化请求。
    pub close_requested: bool,
    pub minimize_requested: bool,
}

impl UiState {
    /// 取出并清空本帧操作。
    pub fn take_ops(&mut self) -> Vec<Op> {
        std::mem::take(&mut self.ops)
    }

    fn push(&mut self, op: Op) {
        self.ops.push(op);
    }
}

/// 一帧的绘制上下文。
pub struct Frame<'a> {
    /// 根 `Ui`。
    pub ui: &'a mut Ui,
    /// 界面状态。
    pub state: &'a mut UiState,
    /// 服务。
    pub svc: &'a mut Service,
    /// 工作区：卡片的唯一权威来源。
    pub ws: &'a Workspace,
    /// 调色板。
    pub pal: &'a Palette,
    /// DPI 缩放。
    pub scale: f32,
    /// 整个客户区。
    pub area: Rect,
    /// 本帧要绘制的卡片（子窗口用）。
    ///
    /// `None` 表示画主窗口（顶栏 + 全部停靠卡片）。
    ///
    /// ⚠️ 不能在 `draw_child` 里"找第一张 host==Window 的卡片"：
    /// 详情与视图都是 Window 型，两个子窗口会各自找到同一张卡片。
    pub card_id: Option<crate::card::CardId>,
}

/// 画一个**独立子窗口**的整帧内容。
///
/// 与 [`draw`] 的区别：那函数画「主窗口的顶栏 + 卡片区」，
/// 这函数画「一张卡片铺满整个窗口」。
///
/// # 为什么子窗口不画顶栏
///
/// 子窗口是纯内容视图，没有「搜索/设置」这类全局控件——
/// 那些只在主窗口有一份。子窗口只承载一张卡片的内容区。
pub fn draw_child(f: &mut Frame<'_>) {
    let area = f.area;
    f.ui.painter().rect_filled(area, 0.0, f.pal.bg);

    // 找到本窗口对应的那张卡片。
    //
    // ⚠️ 不 `expect`：`draw_child` 是生产路径，而「窗口存在但卡片
    // 不在列表里」是**可能发生**的短暂不一致（卡片刚被删、窗口
    // 还没销毁）。这里降级为空画而不是 panic——一个附属面板
    // 的状态问题不该让整个程序崩掉。
    let Some(id) = f.card_id else {
        draw_empty(f, area, "未指定卡片");
        return;
    };
    let Some(card) = f.ws.get(id) else {
        draw_empty(f, area, "窗口与卡片状态不同步");
        return;
    };

    // 卡片铺满整个客户区，四周留一点内边角。
    let r = area.shrink2(vec2(f.pal.space_sm, f.pal.space_sm));
    if r.width() <= 0.0 || r.height() <= 0.0 {
        return;
    }
    draw_card_frame(f, r);

    let pad = f.pal.space_sm;
    let inner = r.shrink2(vec2(pad, pad));
    let head_h = (f.pal.font_md * 1.8).max(20.0);
    let head = Rect::from_min_size(inner.min, vec2(inner.width(), head_h));
    draw_card_header(f, card, head);

    let body = Rect::from_min_max(
        pos2(inner.min.x, head.max.y + f.pal.space_xs),
        inner.max,
    );
    if body.width() <= 0.0 || body.height() <= 0.0 {
        return;
    }
    match card.kind {
        // 置顶卡片作为独立窗口时，只画列表内容（无标题头）——
        // 窗口标题栏已经说明了它是什么。
        CardKind::Pinned => draw_history_body(f, body, Some(true)),
        CardKind::Detail => draw_detail_body(f, body),
        CardKind::Rail => draw_rail_body(f, body),
        // 主窗口专属的种类不该出现在子窗口；画空状态而不是崩溃。
        _ => draw_empty(f, body, "该卡片不支持独立窗口"),
    }
}

/// 画一帧。
///
/// 流程：顶栏 → 设置界面（覆盖层）→ 卡片（Z 序）。
/// 与旧实现最大的差别是**卡片一定会被画**——折叠的画把手，
/// 展开的画内容，不存在「跳过」。
pub fn draw(f: &mut Frame<'_>) {
    let area = f.area;
    // 背景必须铺满整个客户区，否则窗口边缘会露出 clear color。
    f.ui.painter().rect_filled(area, 0.0, f.pal.bg);

    let top = Rect::from_min_max(
        pos2(area.min.x, area.min.y),
        pos2(area.max.x, area.min.y + crate::solver::TOPBAR_HEIGHT),
    );
    let body = crate::solver::card_area(area);

    draw_topbar(f, top);
    draw_cards(f, body);

    // 设置界面作为**覆盖层**画在最后：它要盖住卡片，
    // 所以顺序上必须在卡片之后。
    if f.state.show_settings {
        draw_settings(f, area);
    }
}

// ---------------------------------------------------------------- 顶栏

fn draw_topbar(f: &mut Frame<'_>, bar: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    let painter = f.ui.painter_at(bar);
    painter.rect_filled(bar, CornerRadius::ZERO, pal.surface_variant);
    painter.hline(
        bar.x_range(),
        bar.max.y,
        Stroke::new(1.0, pal.border_subtle),
    );

    let inner = bar.shrink2(vec2(pal.space_md, 0.0));

    // ---- 右侧：清空 + 设置（图标按钮）--------------------------------
    //
    // 从底部状态栏搬上来的。按钮用**图标**而非文字：顶栏高度只有
    // 44pt，中文「清空」两个字排在那里会又高又挤；图标在小尺寸下
    // 更清楚，也不随系统字号缩放变形。
    //
    // 尺寸取顶栏高度的 60%，留出上下呼吸空间。
    let btn = bar.height() * 0.62;
    let btn_area = Rect::from_min_size(
        pos2(
            inner.max.x - btn * 2.0 - pal.space_sm * 2.0,
            bar.center().y - btn * 0.5,
        ),
        vec2(btn * 2.0 + pal.space_sm, btn),
    );
    draw_topbar_icons(f, btn_area);

    // ---- 搜索框：占满剩余全部空间 --------------------------------------
    //
    // ⚠️ 顺序硬性：**先算右侧按钮区，再算搜索框**。反过来的话搜索框会
    // 一路铺到顶栏右缘，把两个图标按钮压在下面（按钮不可点）。
    let search = Rect::from_min_max(
        pos2(inner.min.x, bar.min.y + pal.space_sm),
        pos2(btn_area.min.x - pal.space_md, bar.max.y - pal.space_sm),
    );
    if search.width() > 40.0 && search.height() > 8.0 {
        draw_search(f, search);
    }

    let _ = scale;

    // 顶栏整体作为拖动区（搜索框与按钮自行 `interact` 会排除命中）。
    f.state.drag_region = Some(bar);
}

/// 顶栏右侧的两个图标按钮：清空（垃圾桶）、设置（齿轮）。
///
/// 从底部状态栏搬上来的。**必须用图标字体**（PUA 码位
/// `U+E000..=U+E011`）而不是 Unicode 字形：后者在当前字体链里
/// 缺字形，会渲染成方框（本项目已踩过一次，见行内按钮）。
fn draw_topbar_icons(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    let half = r.width() / 2.0;

    // 清空
    let clear = Rect::from_min_size(r.min, vec2(half, r.height()));
    let rc = f
        .ui
        .interact(clear, egui::Id::new("topbar_clear"), Sense::click());
    if rc.hovered() {
        f.ui.painter_at(clear).rect_filled(clear, pal.radius_sm, pal.row_hover);
    }
    crate::icons::Icon::Trash.paint(
        &f.ui.painter_at(clear),
        Rect::from_center_size(
            clear.center(),
            vec2(pal.icon_size, pal.icon_size) * scale,
        ),
        // 破坏性操作用 danger 色，与「设置」在悬停时区分开。
        if rc.hovered() { pal.danger } else { pal.text_dim },
    );
    // ⚠️ `on_hover_text` 按值消耗 `Response`，所以必须放在最后一次
    // 使用 `rc` 之后；顺序反了会编译失败（E0382）。
    if rc.clicked() {
        f.state.push(Op::ClearAll);
    }
    rc.on_hover_text("清空全部历史");

    // 设置
    let set = Rect::from_min_size(
        pos2(clear.max.x + pal.space_sm * 2.0, clear.min.y),
        vec2(half, clear.height()),
    );
    let rs = f
        .ui
        .interact(set, egui::Id::new("topbar_settings"), Sense::click());
    if rs.hovered() {
        f.ui.painter_at(set).rect_filled(set, pal.radius_sm, pal.row_hover);
    }
    crate::icons::Icon::Settings.paint(
        &f.ui.painter_at(set),
        Rect::from_center_size(
            set.center(),
            vec2(pal.icon_size, pal.icon_size) * scale,
        ),
        if rs.hovered() { pal.text_bright } else { pal.text_dim },
    );
    if rs.clicked() {
        f.state.push(Op::ToggleSettings);
    }
    // 同样：`on_hover_text` 消耗 `Response`，放最后。
    rs.on_hover_text("设置");
}

fn draw_search(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    let painter = f.ui.painter_at(r);
    painter.rect_filled(r, pal.radius_md, pal.surface);
    painter.rect_stroke(
        r,
        pal.radius_md,
        Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );

    // 放大镜图标坐在左内边距里，与文字左边界对齐。
    let pad = pal.space_sm;
    let icon_w = pal.icon_size + pad;
    crate::icons::Icon::Search.paint(
        &f.ui.painter_at(r),
        Rect::from_center_size(
            pos2(r.min.x + pad + pal.icon_size * 0.5, r.center().y),
            vec2(pal.icon_size, pal.icon_size) * scale,
        ),
        pal.text_dim,
    );

    // ⚠️ 这里必须是**真正的输入控件**，不能只画文字。
    //
    // 早前版本用 `painter_at().text()` 把占位符「搜索历史…」画上去、
    // 点击只 `request_repaint()`——看着像搜索框，实际**打不了字**。
    // 搜索框一旦占据顶栏主位置（原先那里是产品标题），静态假框就等于
    // 占着最好的位置却不可用，比没有更糟。
    //
    // `TextEdit` 必须放在一个**限定矩形**的子 `Ui` 里，否则它会按
    // egui 默认布局占满整行，把右侧的模式切换按钮挤出顶栏。
    let edit_rect = Rect::from_min_max(
        pos2(r.min.x + icon_w, r.min.y + 1.0),
        pos2(r.max.x - pad, r.max.y - 1.0),
    );
    if edit_rect.width() <= 4.0 || edit_rect.height() <= 4.0 {
        return;
    }
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(edit_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut f.state.query)
                    .hint_text("搜索历史…")
                    .desired_width(f32::INFINITY)
                    // ⚠️ 字号必须给**不带scale 的逻辑字号**。
                    //
                    // 早前传的是 `sized(pal.font_md, scale)`——它已经把
                    // DPI 系数乘进去了，而 egui 在计算可用高度时会
                    // **再乘一次** `pixels_per_point`。结果是行高被放大到
                    // 远超框高，文字被裁掉上半/下半截，表现为
                    //「能输入但看不见、字很小」。
                    //
                    // 正确做法：`FontId` 用逻辑字号，由 egui 负责缩放。
                    .font(egui::FontId::proportional(pal.font_md))
                    // 内边距收窄：顶栏只有 44pt，egui 默认的
                    // `symmetric(4,2)` 在此会让单行控件偏高而裁切。
                    .margin(egui::Margin::symmetric(2, 0))
                    .text_color(pal.text)
                    // 去掉 TextEdit 自带的外框：外框已由上面的
                    // `rect_stroke` 画好，两层框叠在一起会显得脏。
                    //
                    // ⚠️ 这里必须给 `egui::Frame` 枚举而不是 `false`：
                    // 0.36 的 `frame()` 收的是容器框类型，不是 bool。
                    .frame(egui::Frame::NONE),
            )
        },
    );
    // `scope_builder` 返回 `InnerResponse<Response>`：内层是 TextEdit
    // 自己的响应，外层是这次作用域分配整体的响应。
    // 焦点与 id 都要用**内层**的——外层 id 属于作用域，不是输入框。
    let edit_resp = resp.inner;

    if edit_resp.has_focus() {
        // 聚焦时描边加强，给出明确的视觉反馈。
        f.ui.painter_at(r).rect_stroke(
            r,
            pal.radius_md,
            Stroke::new(1.5, pal.border_strong),
            egui::StrokeKind::Inside,
        );
    }

    // 有文本时给一个清除按钮。
    if !f.state.query.is_empty() {
        let btn = r.height() * 0.6;
        let clear = Rect::from_center_size(
            pos2(r.max.x - pad - btn * 0.5, r.center().y),
            vec2(btn, btn),
        );
        let cr = f
            .ui
            .interact(clear, egui::Id::new("card_search_clear"), Sense::click());
        crate::icons::Icon::Close.paint(
            &f.ui.painter_at(clear),
            Rect::from_center_size(clear.center(), vec2(btn, btn) * 0.7),
            if cr.hovered() { pal.text_bright } else { pal.text_dim },
        );
        if cr.clicked() {
            f.state.query.clear();
            // 清空后主动交还焦点：否则用户还得再点一次才能继续打字。
            f.ui.memory_mut(|m| m.request_focus(edit_resp.id));
        }
        cr.on_hover_text("清空搜索");
    }
}

/// 画一个「标签 + 描述 + 开关」的行。返回是否被切换。
///
/// # 为什么要自己画而不用 `egui::Checkbox`
///
/// 项目视觉语言是「**纯平毛玻璃**」：禁止一切装饰性拟物效果
/// （渐变、光泽、内发光）。`egui::Checkbox` 自带的勾选框是
/// 带立体感的控件，与整体观感不符。这里用 egui 的
/// `Rect::show` 语义手画：纯色填充 + 描边，无渐变。
///
/// ⚠️ `id_suffix` 必须由调用方给**稳定且唯一**的字符串（通常是配置
/// 字段名）：`Pos2` 不能作 `egui::Id`（不满足 `Hash`），而用矩形坐标
/// 当 id 会因窗口 resize 变化，导致同一开关在不同帧拿到不同 id、
/// 交互状态（hover/active）被反复丢弃。
fn draw_switch(f: &mut Frame<'_>, r: Rect, on: bool, id_suffix: &str) -> bool {
    let pal = f.pal;
    let rr = f
        .ui
        .interact(r, egui::Id::new(("switch", id_suffix)), Sense::click());

    // 开关本体：宽=高的两倍。
    let h = (r.height() * 0.62).min(18.0);
    let w = h * 1.8;
    let knob_r = h * 0.5 - 2.0;
    let track = Rect::from_center_size(
        pos2(r.max.x - w * 0.5, r.center().y),
        vec2(w, h),
    );
    let p = f.ui.painter_at(r);
    if on {
        p.rect_filled(track, h * 0.5, pal.accent);
    } else {
        p.rect_filled(track, h * 0.5, pal.surface_variant);
        p.rect_stroke(
            track,
            h * 0.5,
            Stroke::new(1.0, pal.border),
            egui::StrokeKind::Inside,
        );
    }
    // 滑块：开时靠右、关时靠左。
    let knob_x = if on {
        track.max.x - h * 0.5
    } else {
        track.min.x + h * 0.5
    };
    let knob = Rect::from_center_size(pos2(knob_x, track.center().y), vec2(knob_r * 2.0, knob_r * 2.0));
    let knob_color = if on { pal.text_bright } else { pal.text_dim };
    p.circle_filled(knob.center(), knob_r, knob_color);

    rr.clicked()
}

/// 画一行「标签 + 说明」，右侧留出控件区。
///
/// 返回控件应占的矩形，供调用方放开关/输入框。
fn draw_setting_row(f: &mut Frame<'_>, r: Rect, label: &str, desc: &str) -> Rect {
    let pal = f.pal;
    let font = sized(pal.font_sm, f.scale);
    let dfont = sized(pal.font_xs, f.scale);

    // 右侧预留控件宽度，标签只在左侧区域排布。
    let ctrl_w = ((56.0 * f.scale).max(38.0)).min(r.width() * 0.4);
    let text_area = Rect::from_min_max(r.min, pos2(r.max.x - ctrl_w - pal.space_sm, r.max.y));

    // 标签与说明上下排布（说明可能很长，单行会溢出）。
    let two_line = !desc.is_empty();
    let lh = if two_line { r.height() * 0.5 } else { r.height() };
    let lab = Rect::from_min_size(text_area.min, vec2(text_area.width(), lh));
    f.ui.painter_at(lab).text(
        lab.left_center(),
        Align2::LEFT_CENTER,
        label,
        font,
        pal.text,
    );
    if two_line {
        let d = Rect::from_min_size(pos2(text_area.min.x, lab.max.y), vec2(text_area.width(), r.height() - lh));
        let shown = crate::titlebar::elide_text(f.ui, desc, &dfont, d.width());
        if !shown.is_empty() {
            f.ui.painter_at(d).text(d.left_center(), Align2::LEFT_CENTER, shown, dfont, pal.text_dim);
        }
    }
    Rect::from_center_size(
        pos2(r.max.x - ctrl_w * 0.5, r.center().y),
        vec2(ctrl_w, r.height()),
    )
}

/// 数值项的调节粒度。
enum CaptureSlider {
    /// 监听间隔，步长 50ms。
    Ms,
    /// 条数上限，步长 1000。
    Items,
    /// 去重窗口，步长 5 秒。
    Secs,
}

/// 设置界面（覆盖层）。
///
/// 「置顶模式」的切换入口现在住在这里，不在顶栏——
/// 顶栏只保留搜索与两个图标按钮，视觉噪声更低。
fn draw_settings(f: &mut Frame<'_>, area: Rect) {
    let pal = f.pal;
    let scale = f.scale;

    // ⚠️ **首帧的 `area` 可能荒谬**（实测 6666x6666）。
    //
    // `ui.max_rect()` 取的是子`Ui` 的可用矩形，而首帧 `screen_rect`
    // 尚未由 `egui_input` 正确设置（`RawInput.screen_rect` 是默认值）。
    // 此时按 `area` 居中的面板会被算到屏幕外，**看起来像设置界面没打开**。
    //
    // 这里显式拒绝明显不合理的尺寸：不画任何东西，等下一帧。
    // 判断用「超过客户区常见上限」而非绝对值——不同 DPI 下客户区大小差异很大。
    let reasonable = area.width() <= 4000.0 && area.height() <= 4000.0;
    if !reasonable {
        tracing::debug!(area = ?area, "客户区尺寸异常，本帧跳过设置界面");
        return;
    }

    // 遮罩：盖住下面的卡片，点击遮罩关闭。
    let resp = f.ui.interact(area, egui::Id::new("settings_scrim"), Sense::click());
    f.ui
        .painter()
        .rect_filled(area, 0.0, pal.overlay_scrim);
    if resp.clicked() {
        f.state.show_settings = false;
        return;
    }

    // 面板：居中，宽度按逻辑点定，不随窗口无限拉伸。
    // 面板尺寸：宽度留足（标签 + 说明 + 开关要同排），
    // 高度取可用区的 85%——设置项会随版本增加，
    // 给固定高度不如「尽量高 + 内容滚动」。
    //
    // ⚠️ 两处 `max(200.0, ...)` 是为了在**极小窗口**下不出现负宽度：
    // 客户区可能只有 60pt（窗口被拉到极窄），`area.width() - 40` 会为负。
    // ⚠️ 这里**不能乘 `scale`**。
    //
    // `area` 来自 `ui.max_rect()`，是**逻辑点**（DPI 无关）：
    // 实测 1.5x 屏上客户区 420x560 物理像素，`area` 报 280x373。
    //
    // 早前写 `420.0 * scale` 得到 630「逻辑点」——比整个客户区还宽，
    // 虽然被 `.min(area.width()-40)` 兜住不至于溢出，但那只是
    // 恰好被夹住，语义是错的：一旦窗口够宽，面板就会宽到不合理。
    //
    // 同理高度直接用 `area.height()` 的比例。
    let w = 420.0_f32.min(area.width() - 40.0).max(120.0);
    let h = (area.height() * 0.85).min(560.0).max(120.0);
    let panel = Rect::from_center_size(area.center(), vec2(w, h));
    draw_card_frame(f, panel);

    let pad = pal.space_md;
    let mut y = panel.min.y + pad;

    // 标题行 + 关闭按钮。
    let title_font = sized(pal.font_md, scale);
    let head = Rect::from_min_size(pos2(panel.min.x + pad, y), vec2(panel.width() - pad * 2.0, 24.0 * scale));
    f.ui.painter_at(head).text(
        head.left_center(),
        Align2::LEFT_CENTER,
        "设置",
        title_font,
        pal.text_bright,
    );
    let close = Rect::from_center_size(
        pos2(head.max.x - 8.0 * scale, head.center().y),
        vec2(16.0 * scale, 16.0 * scale),
    );
    let rc = f
        .ui
        .interact(close, egui::Id::new("settings_close"), Sense::click());
    crate::icons::Icon::Close.paint(
        &f.ui.painter_at(close),
        Rect::from_center_size(close.center(), vec2(12.0, 12.0) * scale),
        if rc.hovered() { pal.text_bright } else { pal.text_dim },
    );
    if rc.clicked() {
        f.state.show_settings = false;
    }
    y = head.max.y + pal.space_sm;

    // ---- 内容区（可滚动）----------------------------------------------
    //
    // ⚠️ 设置项会随版本增加，固定高度的面板迟早装不下。
    // 这里用「内容高度 vs 可视高度」决定要不要滚动条，
    // 并把滚动偏移夹在合法区间（不夹的话滚到底内容会整体上移）。
    let view = Rect::from_min_max(pos2(panel.min.x + pad, y), pos2(panel.max.x - pad, panel.max.y - pad));
    if view.height() <= 8.0 || view.width() <= 8.0 {
        return;
    }
    // 先把内容画到一个**虚拟的**高矩形里，再按偏移取可见部分。
    let mut content_y = view.min.y - f.state.settings_scroll;
    let row_h = 34.0 * scale;

    // ⚠️ `FontId` 在本版本**不是 `Copy`**，所以宏里每次现建而不能
    // 闭包捕获外部的 `sec_font`（否则第二次展开就move 走了）。
    macro_rules! section {
        ($t:expr) => {{
            let r = Rect::from_min_size(pos2(view.min.x, content_y), vec2(view.width(), 18.0 * scale));
            if r.max.y >= view.min.y && r.min.y <= view.max.y {
                f.ui.painter_at(r).text(
                    r.left_center(),
                    Align2::LEFT_CENTER,
                    $t,
                    sized(pal.font_xs, scale),
                    pal.text_dim,
                );
            }
            content_y = r.max.y + pal.space_xs;
        }};
    }

    // ⚠️ 不能写 `let cfg = &f.svc.state.config`：那是**不可变借用**整个 `f`，
    // 之后调用 `draw_setting_row(f, ...)` 需要可变借用 → E0502。
    //
    // 所以把设置面板要读的字段**逐个复制成局部值**。
    // 面板是一次性快照：用户点了开关后值下一帧才变，
    // 这正是期望行为（不会出现「开关还没动、显示已翻转」）。
    let ui_cfg = f.svc.state.config.ui.clone();
    let cap_cfg = f.svc.state.config.capture.clone();
    let sto_cfg = f.svc.state.config.storage.clone();

    // ---- 界面 ---------------------------------------------------------
    section!("置顶");
    {
        let r = Rect::from_min_size(pos2(view.min.x, content_y), vec2(view.width(), row_h));
        content_y = r.max.y + pal.space_xs;
        let cur = f.ws.pinned_mode();
        let two: Vec<(PinnedMode, &str, &str)> = vec![
            (PinnedMode::SharedColumn, "共用单栏", "与历史列表在同一栏内"),
            (PinnedMode::OwnCard, "独立分栏", "单独一栏，可拖出成窗口"),
        ];
        // 横向排列两个选项。
        let w = (r.width() - pal.space_xs) * 0.5;
        for (i, (mode, label, desc)) in two.into_iter().enumerate() {
            let rr = Rect::from_min_size(pos2(r.min.x + i as f32 * (w + pal.space_xs), r.min.y), vec2(w, r.height()));
            let active = mode == cur;
            let resp = f.ui.interact(rr, egui::Id::new(("pinned_mode", format!("{mode:?}"))), Sense::click());
            let painter = f.ui.painter_at(rr);
            if active {
                painter.rect_filled(rr, pal.radius_sm, pal.accent.gamma_multiply(0.22));
                painter.rect_stroke(rr, pal.radius_sm, Stroke::new(1.0, pal.accent), egui::StrokeKind::Inside);
            } else if resp.hovered() {
                painter.rect_filled(rr, pal.radius_sm, pal.row_hover);
            }
            let dot_r = 4.0 * scale;
            let dot = Rect::from_center_size(pos2(rr.min.x + dot_r + pal.space_sm, rr.center().y), vec2(dot_r * 2.0, dot_r * 2.0));
            if active {
                painter.circle_filled(dot.center(), dot_r, pal.accent);
            } else {
                painter.circle_stroke(dot.center(), dot_r, Stroke::new(1.0, pal.border));
            }
            let la = Rect::from_min_max(pos2(dot.max.x + pal.space_sm, rr.min.y), pos2(rr.max.x - pal.space_xs, rr.max.y));
            let txt = format!("{label}    {desc}");
            let shown = crate::titlebar::elide_text(f.ui, &txt, &sized(pal.font_sm, scale), la.width());
            painter.text(la.left_center(), Align2::LEFT_CENTER, shown, sized(pal.font_sm, scale), if active { pal.text_bright } else { pal.text });
            if resp.clicked() {
                f.state.push(Op::SetPinnedMode(mode));
            }
        }
    }

    // ---- 开关组 --------------------------------------------------------
    //每项：(分组, 字段, 标签, 说明, 当前值)
    let toggles: [(ConfigGroup, &'static str, &str, &str, bool); 7] = [
        (ConfigGroup::Ui, "always_on_top", "窗口置顶", "始终显示在其它窗口之上", ui_cfg.always_on_top),
        (ConfigGroup::Ui, "hide_on_focus_lost", "失焦时隐藏", "切换到别的程序后隐藏窗口", ui_cfg.hide_on_focus_lost),
        (ConfigGroup::Ui, "show_tray", "显示托盘图标", "后台常驻，保留右键菜单入口", ui_cfg.show_tray),
        (ConfigGroup::Ui, "start_minimized", "启动时最小化", "启动后隐藏到托盘", ui_cfg.start_minimized),
        (ConfigGroup::Capture, "enabled", "记录剪贴板", "关闭后不再新增历史", cap_cfg.enabled),
        (ConfigGroup::Capture, "skip_password_fields", "跳过密码字段", "疑似输入密码时不记录", cap_cfg.skip_password_fields),
        (ConfigGroup::Capture, "dedup", "自动去重", "短时间内相同内容只留一条", cap_cfg.dedup),
    ];

    let mut last_group = None;
    for (g, field, label, desc, on) in toggles {
        if last_group != Some(g) {
            section!(g.title());
            last_group = Some(g);
        }
        let r = Rect::from_min_size(pos2(view.min.x, content_y), vec2(view.width(), row_h));
        content_y = r.max.y + pal.space_xs * 0.5;
        if r.max.y < view.min.y || r.min.y > view.max.y {
            continue;
        }
        let ctrl = draw_setting_row(f, r, label, desc);
        if draw_switch(f, ctrl, on, field) {
            f.state.push(Op::SetBool { group: g, field, value: !on });
        }
    }

    // ---- 数值组 --------------------------------------------------------
    section!("监听与存储");
    {
        let items: [(&str, &str, &str, String, CaptureSlider); 3] = [
            (
                "poll_interval_ms",
                "监听间隔",
                "越小越灵敏、越耗电",
                format!("{} ms", cap_cfg.poll_interval_ms),
                CaptureSlider::Ms,
            ),
            (
                "max_items",
                "历史条数上限",
                "留空表示只按容量淘汰",
                sto_cfg
                    .max_items
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "不限".into()),
                CaptureSlider::Items,
            ),
            (
                "dedup_window_secs",
                "去重窗口",
                "该时长内的相同内容视为重复",
                format!("{} 秒", cap_cfg.dedup_window_secs),
                CaptureSlider::Secs,
            ),
        ];
        for (field, label, desc, val, kind) in items {
            let r = Rect::from_min_size(pos2(view.min.x, content_y), vec2(view.width(), row_h));
            content_y = r.max.y + pal.space_xs * 0.5;
            if r.max.y < view.min.y || r.min.y > view.max.y {
                continue;
            }
            let ctrl = draw_setting_row(f, r, label, desc);
            // 左半画当前值、右半画调节按钮。
            let vw = ctrl.width() * 0.46;
            let vb = Rect::from_min_max(ctrl.min, pos2(ctrl.min.x + vw, ctrl.max.y));
            f.ui.painter_at(vb).text(
                vb.center(),
                Align2::CENTER_CENTER,
                val,
                sized(pal.font_xs, scale),
                pal.text_dim,
            );
            // 「−」「+」两个按钮
            let bw = (ctrl.height() * 0.9).min(20.0);
            let plus = Rect::from_center_size(pos2(ctrl.max.x - bw * 0.5, ctrl.center().y), vec2(bw, bw));
            let minus = Rect::from_center_size(pos2(plus.min.x - bw * 1.1, ctrl.center().y), vec2(bw, bw));
            let delta = match kind {
                CaptureSlider::Ms => 50.0,
                CaptureSlider::Items => 1000.0,
                CaptureSlider::Secs => 5.0,
            };
            for (rr, sign, cur) in [
                (minus, -1.0f64, cap_cfg.dedup_window_secs as f64),
                (plus, 1.0f64, cap_cfg.dedup_window_secs as f64),
            ] {
                let resp = f.ui.interact(rr, egui::Id::new(("num", field, sign > 0.0)), Sense::click());
                let p = f.ui.painter_at(rr);
                if resp.hovered() {
                    p.rect_filled(rr, pal.radius_sm, pal.row_hover);
                }
                p.rect_stroke(rr, pal.radius_sm, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
                p.text(rr.center(), Align2::CENTER_CENTER, if sign > 0.0 { "+" } else { "-" }, sized(pal.font_md, scale), pal.text);
                if resp.clicked() {
                    match kind {
                        CaptureSlider::Ms => {
                            let v = (cap_cfg.poll_interval_ms as f64 + sign * delta).clamp(50.0, 2000.0);
                            f.state.push(Op::SetNumber { group: ConfigGroup::Capture, field, value: v });
                        }
                        CaptureSlider::Items => {
                            // 0 = 不限（None）；递增到 0 时变回 Some
                            let cur = sto_cfg.max_items;
                            let next = match cur {
                                None => Some(1000usize),
                                Some(n) if n as f64 + sign * delta <= 0.0 => None,
                                Some(n) => Some(((n as f64 + sign * delta).max(100.0)) as usize),
                            };
                            f.state.push(Op::SetOptNumber { group: ConfigGroup::Storage, field, value: next.map(|x| x as f64) });
                        }
                        CaptureSlider::Secs => {
                            let v = (cur + sign * delta).max(0.0);
                            f.state.push(Op::SetNumber { group: ConfigGroup::Capture, field, value: v });
                        }
                    }
                }
            }
        }
    }

    // ---- 存储分组开关（单独一个分组标题）------------------------------
    section!("存储");
    {
        let r = Rect::from_min_size(pos2(view.min.x, content_y), vec2(view.width(), row_h));
        let ctrl = draw_setting_row(f, r, "启动时清理临时文件", "删除上次退出遗留的临时文件");
        let on = sto_cfg.cleanup_on_start;
        if draw_switch(f, ctrl, on, "cleanup_on_start") {
            f.state.push(Op::SetBool { group: ConfigGroup::Storage, field: "cleanup_on_start", value: !on });
        }
        content_y = r.max.y;
    }

    // ---- 滚动 --------------------------------------------------------
    let content_h = content_y - (view.min.y - f.state.settings_scroll);
    let max_scroll = (content_h - view.height()).max(0.0);
    f.state.settings_scroll = f.state.settings_scroll.clamp(0.0, max_scroll);
    if f.ui.rect_contains_pointer(view) {
        let d = f.ui.input(|i| i.smooth_scroll_delta.y);
        if d != 0.0 {
            f.state.settings_scroll =
                (f.state.settings_scroll - d * view.height() / 3.0).clamp(0.0, max_scroll);
            f.ui.ctx().request_repaint();
        }
    }
    // 滚动条
    if max_scroll > 0.5 {
        let track_w = 4.0;
        let track = Rect::from_min_max(
            pos2(view.max.x + 2.0, view.min.y),
            pos2(view.max.x + 2.0 + track_w, view.max.y),
        );
        let p = f.ui.painter_at(track);
        p.rect_filled(track, track_w * 0.5, pal.surface_variant);
        let t = (f.state.settings_scroll / max_scroll).clamp(0.0, 1.0);
        let h = (track.height() * (view.height() / content_h)).max(24.0).min(track.height());
        let knob = Rect::from_min_size(
            pos2(track.min.x, track.min.y + (track.height() - h) * t),
            vec2(track_w, h),
        );
        p.rect_filled(knob, track_w * 0.5, pal.border);
    }

}

/// 置顶卡片当前是否独立成窗。
///
/// 有独立置顶窗时主窗口**不再显示置顶栏**——置顶内容已经在那个
/// 窗口里了，再画一份就是重复。
fn pinned_is_separate(ws: &Workspace) -> bool {
    ws.cards
        .iter()
        .any(|c| c.kind == CardKind::Pinned && c.host == CardHost::Window)
}

fn draw_cards(f: &mut Frame<'_>, _body: Rect) {
    // 置顶**不占一栏**：要么独立成窗、要么内嵌在历史栏顶部。
    // 两种情况主窗口都不画置顶卡片。
    //
    // ⚠️ 判据必须与 solver 用的一致（`pinned_is_embedded`），
    // 否则会出现「solver 按占一栏分了宽度、paint 又在历史栏内画
    // 一份」—— 实测界面就是这样：置顶与历史并排，
    // 而用户要的是不分栏。
    let skip_pinned = f.ws.pinned_is_embedded() || pinned_is_separate(f.ws);

    // 按 Z 序遍历。这里直接读 `card.rect`——**不做任何二次判断**，
    // 也不重新计算宽度。矩形是solver 算好的，绘制只负责照着画。
    for card in f.ws.cards.iter() {
        if card.host != CardHost::Docked {
            continue;
        }
        if skip_pinned && card.kind == CardKind::Pinned {
            continue;
        }
        let r = card.rect;
        if r.width() <= 0.0 || r.height() <= 0.0 {
            // 求解器保证停靠卡片必有非零矩形；走到这里说明它还没被
            // 求解（例如最小化后的第一帧）。**跳过而不是画零尺寸卡**——
            // 画零尺寸卡会产出退化矩形，某些路径下会算出非法 scissor。
            continue;
        }
        if card.collapsed {
            draw_collapsed_card(f, card, r);
        } else {
            draw_expanded_card(f, card, r);
        }
    }

    // 置顶**没有**独立成窗 ⇒ 在历史栏顶部内嵌一个置顶区，
    // 免得为了一两条置顶再开一个窗口。
    if !pinned_is_separate(f.ws) {
        draw_embedded_pinned(f, _body);
    }
}

/// 在历史栏内嵌置顶区（无独立置顶窗时）。
///
/// 画在历史栏**上方**，两栏之间用一条细分隔线隔开——
/// 这不是独立卡片，所以不画自己的边框，避免与历史栏的边框叠成双线。
fn draw_embedded_pinned(f: &mut Frame<'_>, _body: Rect) {
    let pal = f.pal;
    let Some(history) = f.ws.by_kind(CardKind::History) else {
        return;
    };
    let hr = history.rect;
    if hr.width() <= 0.0 || hr.height() <= 40.0 {
        return;
    }

    // 置顶条目为空时不占空间——否则为了 0 条记录占掉一条横幅。
    let has_pinned = f.svc.state.items.iter().any(|it| it.pinned);
    if !has_pinned {
        return;
    }

    let pad = pal.space_sm;
    let font: FontId = sized(pal.font_xs, f.scale);
    let label_h = (pal.font_xs * 1.6).max(14.0);
    let inner = hr.shrink2(vec2(pad, pad));

    // 标题行「置顶」
    let label = Rect::from_min_size(inner.min, vec2(inner.width(), label_h));
    f.ui.painter_at(label).text(
        label.left_center(),
        Align2::LEFT_CENTER,
        "置顶",
        font,
        pal.text_dim,
    );

    // 分隔线 + 列表区
    let sep_y = label.max.y;
    let list = Rect::from_min_max(
        pos2(inner.min.x, sep_y + pal.space_xs),
        pos2(inner.max.x, inner.max.y),
    );
    if list.height() <= 8.0 {
        return;
    }
    // `hline(x范围, y, 描边)`：横线要传 x 的**区间**，不是 y 区间。
    f.ui
        .painter_at(hr)
        .hline(hr.x_range(), sep_y, Stroke::new(1.0, pal.border_subtle));

    draw_history_body(f, list, Some(true));
}

fn draw_card_frame(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    // 纯平毛玻璃：纯色底 + 1px 描边 + 主题色光晕。
    // **禁止渐变、光泽、折射、内发光**——那类效果在浅色卡片上
    // 看起来廉价，且与「平」的视觉语言冲突。
    let painter = f.ui.painter_at(r);
    painter.rect_filled(r, CARD_RADIUS, pal.surface);
    painter.rect_stroke(
        r,
        CARD_RADIUS,
        Stroke::new(CARD_STROKE * scale, pal.border_subtle),
        egui::StrokeKind::Inside,
    );
}

/// 折叠态：只画一条把手，可点回展开。
fn draw_collapsed_card(f: &mut Frame<'_>, card: &Card, r: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    // 把手实际只占 handle_width，其余是背景——与 solver 的分配一致。
    let handle = Rect::from_min_size(
        r.min,
        vec2(r.width().min(card.kind.handle_width()), r.height()),
    );
    if handle.width() <= 0.0 {
        return;
    }
    let resp = f.ui.interact(
        handle,
        egui::Id::new(("card_handle", card.id)),
        Sense::click(),
    );
    let painter = f.ui.painter_at(handle);
    if resp.hovered() {
        painter.rect_filled(handle, CARD_RADIUS, pal.row_hover);
    } else {
        painter.rect_filled(handle, CARD_RADIUS, pal.surface_variant);
    }
    // ⚠️ 必须描边：相邻两张折叠把手底色接近，不描边会糊成一片。
    // 这条曾导致「详情把手与视图把手看不出边界」。
    painter.rect_stroke(
        handle,
        CARD_RADIUS,
        Stroke::new(CARD_STROKE * scale, pal.border_subtle),
        egui::StrokeKind::Inside,
    );

    // 标题竖排在把手中央。
    let font = sized(pal.font_xs, scale);
    let label = card.kind.title();
    let y0 = handle.center().y - (label.chars().count() as f32) * font.size * 0.6;
    for (i, ch) in label.chars().enumerate() {
        let y = y0 + i as f32 * font.size * 1.15;
        if y > handle.max.y - font.size * 0.5 {
            break;
        }
        f.ui.painter_at(handle).text(
            pos2(handle.center().x, y),
            Align2::CENTER_CENTER,
            ch.to_string(),
            font.clone(),
            pal.text_dim,
        );
    }
    if resp.clicked() {
        f.state.push(Op::ToggleCollapse(card.id));
    }
    resp.on_hover_text(format!("展开{}", card.kind.title()));
}

/// 展开态：卡片外框 + 头部 + 内容。
fn draw_expanded_card(f: &mut Frame<'_>, card: &Card, r: Rect) {
    draw_card_frame(f, r);
    let pal = f.pal;

    let pad = pal.space_sm;
    let inner = r.shrink2(vec2(pad, pad));

    // 头部：标题 + 折叠/分离按钮。
    let head_h = (pal.font_md * 1.8).max(20.0);
    let head = Rect::from_min_size(inner.min, vec2(inner.width(), head_h));
    draw_card_header(f, card, head);

    // 内容区。
    let body = Rect::from_min_max(
        pos2(inner.min.x, head.max.y + pal.space_xs),
        inner.max,
    );
    if body.width() <= 0.0 || body.height() <= 0.0 {
        return;
    }
    match card.kind {
        CardKind::History => draw_history_body(f, body, None),
        CardKind::Pinned => draw_history_body(f, body, Some(true)),
        CardKind::Detail => draw_detail_body(f, body),
        CardKind::Rail => draw_rail_body(f, body),
    }
}

fn draw_card_header(f: &mut Frame<'_>, card: &Card, head: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    let font: FontId = sized(pal.font_md, scale);

    // 右侧预留两个按钮的宽度，标题只在其左侧绘制。
    let btn = (pal.control_height * 0.8).max(16.0);
    let title_area = Rect::from_min_max(
        head.min,
        pos2(head.max.x - btn * 2.0 - pal.space_xs * 2.0, head.max.y),
    );
    f.ui.painter_at(title_area).text(
        title_area.left_center(),
        Align2::LEFT_CENTER,
        card.kind.title(),
        font,
        pal.text_bright,
    );

    // 「分离」按钮：把卡片变成独立子窗口。
    let detach = Rect::from_center_size(
        pos2(head.max.x - btn * 1.5 - pal.space_xs, head.center().y),
        vec2(btn, btn),
    );
    let r1 = f
        .ui
        .interact(detach, egui::Id::new(("card_detach", card.id)), Sense::click());
    crate::icons::Icon::Drag.paint(
        &f.ui.painter_at(detach),
        Rect::from_center_size(detach.center(), vec2(pal.icon_size, pal.icon_size) * scale),
        if r1.hovered() { pal.text_bright } else { pal.text_dim },
    );
    if r1.clicked() {
        f.state.push(Op::Detach(card.id));
    }
    r1.on_hover_text("分离为独立窗口");

    // 「折叠」按钮。
    let collapse = Rect::from_center_size(
        pos2(head.max.x - btn * 0.5, head.center().y),
        vec2(btn, btn),
    );
    let r2 = f.ui.interact(
        collapse,
        egui::Id::new(("card_collapse", card.id)),
        Sense::click(),
    );
    crate::icons::Icon::Collapse.paint(
        &f.ui.painter_at(collapse),
        Rect::from_center_size(collapse.center(), vec2(pal.icon_size, pal.icon_size) * scale),
        if r2.hovered() { pal.text_bright } else { pal.text_dim },
    );
    if r2.clicked() {
        f.state.push(Op::ToggleCollapse(card.id));
    }
    r2.on_hover_text("折叠卡片");
}

// ---------------------------------------------------------------- 内容

fn draw_history_body(f: &mut Frame<'_>, body: Rect, only_pinned: Option<bool>) {
    // 过滤：全部 / 仅置顶。
    let items: Vec<ClipItem> = f
        .svc
        .state
        .items
        .iter()
        .filter(|it| only_pinned.map_or(true, |p| it.pinned == p))
        .cloned()
        .collect();

    if items.is_empty() {
        let msg = match only_pinned {
            Some(true) => "还没有置顶条目",
            _ if f.state.query.is_empty() => "还没有剪贴板记录",
            _ => "没有匹配的结果",
        };
        draw_empty(f, body, msg);
        return;
    }

    // 行高：足够放两行文字（预览 + 来源/时间），让内容**完整可读**。
    //
    // 早前版本是 40pt 单行 + `elide_text` 截断，长路径/长文本只能看到
    // 一小段（截图里 `miniz_oxide-36a1…` 就是这样）。用户要求「完整
    // 显示内容」，因此改成两行：上行给预览（最多两行），下行给来源时间。
    // ---- 动态行高 -------------------------------------------------
    //
    // 每条内容按折行数算自己的高度（见 [`row_height_for`]），
    // 这样长内容能被完整展开、短内容不占多余空间。
    //
    // 代价是要**每帧重算全部行高**才能知道滚动位置。
    // 这一步只做排版测量、不产生绘制指令，n=1000 时约几毫秒，
    // 对剪贴板这类「几十到几百条」的场景可接受。
    let pal = f.pal;
    let scale = f.scale;
    let acts_w = 3.0 * (pal.control_height * 0.7 + pal.space_xs);
    let text_w = (body.width() - (pal.icon_size + pal.space_sm) - acts_w - 8.0).max(20.0);
    let heights: Vec<f32> = items
        .iter()
        .map(|it| row_height_for(f.ui, it, pal, scale, text_w))
        .collect();
    // 前缀和：用来 O(log n) 由滚动偏移定位到行。
    let mut offsets: Vec<f32> = Vec::with_capacity(heights.len() + 1);
    offsets.push(0.0);
    for h in &heights {
        offsets.push(offsets.last().copied().unwrap_or(0.0) + h);
    }
    let content_h = offsets.last().copied().unwrap_or(0.0);

    // ---- 滚动 ----
    //
    // 滚动偏移夹在 `[0, content_h - 可视高]`：不夹的话快速滚到底后
    // 列表会整体上移、最后几项「跳」出可视区。
    let max_scroll = (content_h - body.height()).max(0.0);
    f.state.scroll_offset = f.state.scroll_offset.clamp(0.0, max_scroll);

    // 滚轮：只有在指针悬停于本列表时才消费，否则会抢走整个窗口的滚动。
    // 步长取可视高的 1/3，与原生滚动条的手感一致。
    if f.ui.rect_contains_pointer(body) {
        let delta = f.ui.input(|i| i.smooth_scroll_delta.y);
        if delta != 0.0 {
            f.state.scroll_offset =
                (f.state.scroll_offset - delta * body.height() / 3.0).clamp(0.0, max_scroll);
            f.ui.ctx().request_repaint();
        }
    }

    // 虚拟化：定位到第一条可视行，只画到超出可视区为止。
    let first = offsets.partition_point(|&o| o + ROW_HEIGHT_MIN <= f.state.scroll_offset);
    for i in first..items.len() {
        let y = body.min.y + offsets[i] - f.state.scroll_offset;
        if y > body.max.y {
            break; // 已越过可视区底部
        }
        let r = Rect::from_min_size(pos2(body.min.x, y), vec2(body.width(), heights[i]));
        // 裁到可视区：画到区外的内容会被 scissor 裁掉，但白白产生几何。
        if r.max.y < body.min.y {
            continue;
        }
        draw_item_row(f, r, &items[i]);
    }

    // 滚动条：内容超高时才显示。
    if content_h > body.height() {
        draw_scrollbar(f, body, f.state.scroll_offset / max_scroll.max(1.0), content_h);
    }
}

/// 列表行的**最小**高度（逻辑点）。
///
/// 两行布局：预览（14pt，可折行）+ 来源/时间（11pt）+ 上下内边距。
///
/// ⚠️ 这是**下限**不是固定值：早前用固定 40pt 时预览只够放一行半，
/// 长内容被省略号截断（用户反馈「需要完整显示内容」）。
/// 实际行高由 [`row_height_for`] 按每条内容的折行数动态算出。
const ROW_HEIGHT_MIN: f32 = 52.0;

/// 每行文字的高度倍率。
const LINE_H: f32 = 1.35;

/// 单条列表项的**实际**行高：按预览文本折行后的行数计算。
///
/// # 为什么必须逐条算
///
/// 预览长度差异极大（`1` 和一段几百字的粘贴内容）。固定行高会
/// 出现两种坏结果：
/// - 行高 < 需要 → 长内容被省略号截断（看不到完整内容）；
/// - 行高 > 需要 → 短内容下方留一大块空白。
///
/// 折行数由 egui 自己算（[`egui::TextWrapping::max_rows`]），
/// 这里只取「实际用了几行」——比按字符数估算准得多，
/// 中英文混排、全角半角混排都能正确处理。
fn row_height_for(ui: &egui::Ui, item: &ClipItem, pal: &Palette, scale: f32, text_w: f32) -> f32 {
    let preview_font = sized(pal.font_sm, scale);
    let meta_font = sized(pal.font_xs, scale);
    let line_h = pal.font_sm * scale * LINE_H;
    let meta_h = meta_font.size.max(10.0);

    // 折行一次，拿到实际行数。
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &item.preview,
        0.0,
        egui::TextFormat {
            font_id: preview_font,
            color: pal.text,
            ..Default::default()
        },
    );
    job.wrap.max_width = text_w;
    // 不限行数，先量出「到底需要几行」。
    //
    // ⚠️ 必须用 `usize::MAX` 而不是 `0`：`TextWrapping::max_rows` 的
    // 默认值是 `usize::MAX`（不限），而它的文档明确写着
    // 「If set to `0`, no text will be outputted」——
    // **0 表示一行都不输出**，不是「不限」。
    //
    // 误设成 0 的后果：galley.rows 恒为空 → `n_lines` 恒为 1 →
    // 行高恒等于最小值 52pt → 预览带只有 24pt 高 → 长内容被省略号
    // 截断。实测诊断数据完全对上：
    //   row_h=52.8  band_h=24.3  galley_rows=1  elided=true
    job.wrap.max_rows = usize::MAX;
    job.wrap.break_anywhere = true;
    let galley = ui.painter().layout_job(job);
    // `rows` 是字段不是方法（`Arc<Galley>` 直接解引用）。
    let n_lines = galley.rows.len().max(1);
    let text_h = n_lines as f32 * line_h;

    // 上下内边距 + 预览 + 行间2pt + 来源/时间
    (text_h + meta_h + 5.0 * 2.0 + 2.0).max(ROW_HEIGHT_MIN)
}

/// 画细滚动条。`t` 是滚动比例 `[0,1]`，`content_h` 是内容总高。
fn draw_scrollbar(f: &mut Frame<'_>, body: Rect, t: f32, content_h: f32) {
    let pal = f.pal;
    let w = 6.0;
    let track = Rect::from_min_size(
        pos2(body.max.x - w - 2.0, body.min.y),
        vec2(w, body.height()),
    );
    // 滑块长度 = 可视高 / 内容高 × 轨道长。
    //
    // ⚠️ 必须用**真实内容高**算。早前版本用 `可视高 / ROW_HEIGHT`
    // 反推行数，那算的是「屏幕上放得下几行」而不是「一共有几行」，
    // 结果滑块永远是满格（比例恒为 1），内容超出时看不出能滚。
    let ratio = (body.height() / content_h.max(1.0)).clamp(0.06, 1.0);
    let thumb_h = (body.height() * ratio).max(28.0);
    let y = body.min.y + t.clamp(0.0, 1.0) * (body.height() - thumb_h);
    let thumb = Rect::from_min_size(pos2(track.min.x, y), vec2(w, thumb_h));
    let p = f.ui.painter_at(track);
    p.rect_filled(track, w / 2.0, pal.surface_variant);
    p.rect_filled(thumb, w / 2.0, pal.border);
}

fn draw_detail_body(f: &mut Frame<'_>, body: Rect) {
    let Some(id) = f.svc.state.selected else {
        draw_empty(f, body, "选中一条记录查看详情");
        return;
    };
    let Some(item) = f.svc.state.items.iter().find(|it| it.id == id) else {
        draw_empty(f, body, "该记录已不存在");
        return;
    };
    let pal = f.pal;
    let scale = f.scale;

    // 详情文字按可用矩形裁剪：长内容不得溢出卡片。
    let text_area = body.shrink2(vec2(pal.space_xs, 0.0));
    let font = sized(pal.font_sm, scale);
    let preview = if item.preview.is_empty() {
        "（无预览）"
    } else {
        item.preview.as_str()
    };
    let shown = crate::titlebar::elide_text(f.ui, preview, &font, text_area.width());
    f.ui.painter_at(text_area).text(
        pos2(text_area.min.x, text_area.min.y + font.size),
        Align2::LEFT_TOP,
        shown,
        font.clone(),
        pal.text,
    );

    // 来源与时间。
    let meta_y = text_area.min.y + font.size * 2.4;
    if meta_y < text_area.max.y {
        let meta_area = Rect::from_min_max(
            pos2(text_area.min.x, meta_y),
            pos2(text_area.max.x, text_area.min.y + meta_y + font.size * 1.2),
        );
        let meta_font = sized(pal.font_xs, scale);
        let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
        let meta_shown =
            crate::titlebar::elide_text(f.ui, &meta, &meta_font, meta_area.width());
        f.ui.painter_at(meta_area).text(
            meta_area.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font,
            pal.text_dim,
        );
    }
}

fn draw_rail_body(f: &mut Frame<'_>, body: Rect) {
    let pal = f.pal;
    let items: Vec<ClipItem> = f.svc.state.items.iter().cloned().collect();
    if items.is_empty() {
        draw_empty(f, body, "暂无数据");
        return;
    }
    let font = sized(pal.font_xs, f.scale);
    let row_h = 22.0;
    let n = ((body.height() / row_h).floor() as usize).min(items.len());
    for i in 0..n {
        let y = body.min.y + i as f32 * row_h;
        let r = Rect::from_min_size(pos2(body.min.x, y), vec2(body.width(), row_h));
        if r.max.y > body.max.y {
            break;
        }
        let txt = format!("{} · {}", items[i].source_app, items[i].preview);
        let shown = crate::titlebar::elide_text(f.ui, &txt, &font, r.width());
        f.ui.painter_at(r).text(
            r.left_center(),
            Align2::LEFT_CENTER,
            shown,
            font.clone(),
            pal.text_dim,
        );
    }
}

fn draw_empty(f: &mut Frame<'_>, body: Rect, msg: &str) {
    let pal = f.pal;
    let font = sized(pal.font_sm, f.scale);
    // ⚠️ painter_at(body)：空状态文案是居中的，而窄卡片（折叠把手
    // 旁的详情栏）装不下整句，根 painter 会让它溢出到相邻卡片上。
    f.ui.painter_at(body).text(
        body.center(),
        Align2::CENTER_CENTER,
        msg,
        font,
        pal.text_dim,
    );
}

fn draw_item_row(f: &mut Frame<'_>, r: Rect, item: &ClipItem) {
    let pal = f.pal;
    let scale = f.scale;
    let selected = f.svc.state.selected == Some(item.id);

    let resp = f
        .ui
        .interact(r, egui::Id::new(("row", item.id)), Sense::click());
    let painter = f.ui.painter_at(r);
    if selected {
        painter.rect_filled(r, CornerRadius::ZERO, pal.row_selected);
        painter.rect_filled(
            Rect::from_min_size(r.min, vec2(2.0 * scale, r.height())),
            CornerRadius::ZERO,
            pal.accent,
        );
    } else if resp.hovered() {
        painter.rect_filled(r, CornerRadius::ZERO, pal.row_hover);
    }

    // ---- 两行布局 ----------------------------------------------------
    //
    // 上行：预览（可换行，最多占 2 行）+ 左侧类型图标
    // 下行：来源程序 · 时间（小字弱化）
    //
    // ⚠️ 目标是「**完整显示内容**」：预览**不再截断**，而是按宽度
    // 折行。折行数由 `wrap` 直接给出，不用手工估算——
    // 早前版本按 `preview.chars().take(30)` 硬砍，中文一个字算一个
    // char、英文一个词算一个 char，两者的实际显示宽度差了好几倍，
    // 于是「看起来还有空间」但已经被砍掉了。
    let icon_w = pal.icon_size + pal.space_sm;
    let acts_w = 3.0 * (pal.control_height * 0.7 + pal.space_xs);
    let text_x0 = r.min.x + icon_w;
    let text_x1 = (r.max.x - acts_w).max(text_x0);
    if text_x1 - text_x0 < 20.0 {
        return; // 太窄，画不下任何文字
    }

    // 左侧类型图标，垂直居中。
    crate::icons::Icon::for_kind(item.kind).paint(
        &f.ui.painter_at(r),
        Rect::from_center_size(
            pos2(
                r.min.x + icon_w * 0.5,
                r.center().y,
            ),
            vec2(pal.icon_size, pal.icon_size) * scale,
        ),
        if selected { pal.accent } else { pal.text_dim },
    );

    let pad_v = 5.0;
    let meta_font = sized(pal.font_xs, scale);
    let meta_h = meta_font.size.max(10.0);
    // 下行固定给来源/时间一行。
    let meta_band = Rect::from_min_max(
        pos2(text_x0, r.max.y - pad_v - meta_h),
        pos2(text_x1, r.max.y - pad_v),
    );
    // 上行占剩下的全部高度。
    let preview_band = Rect::from_min_max(
        pos2(text_x0, r.min.y + pad_v),
        pos2(text_x1, meta_band.min.y - 2.0),
    );

    if preview_band.height() > 4.0 {
        // 用 `Painter::layout_job` 按宽度折行，返回**完整** Galley——
        // 这才是「完整显示」：不丢任何字符，只是折行。
        //
        // ⚠️ 不能用 `Ui::layout`/`Context::layout`：那是给「让出矩形」
        // 的控件做换行用的，会受子 `Ui` 的布局约束影响，而我们只要
        // 一个纯文本排版结果。`Painter::layout_job` 才是纯排版入口。
        let mut job = egui::text::LayoutJob::default();
        job.append(
            &item.preview,
            0.0,
            egui::TextFormat {
                font_id: sized(pal.font_sm, scale),
                color: if selected {
                    pal.text_bright
                } else {
                    pal.text
                },
                ..Default::default()
            },
        );
        job.wrap.max_width = preview_band.width();
        // 高度用 `max_rows` 表达（0.36 的 `TextWrapping` **没有**
        // `max_height` 字段，只有 `max_width` / `max_rows` /
        // `break_anywhere` / `overflow_character`）。
        //
        // 行数按可用高度与字号算出，至少 1 行。超出部分由 egui 省略并
        // 在末行加省略号——**完整内容在详情子窗口里能看到**。
        // 行数上限：`row_height_for` 已按同一份预览文本算出行数并据此
        // 定好行高，这里给同样的上限只是兜底（防止两次排版结果不一致）。
        //
        // ⚠️ **不能设 0**：egui 文档写明「If set to 0, no text will be
        // outputted」——0 表示一行都不输出。默认值 `usize::MAX` 才是「不限」。
        job.wrap.max_rows = ((preview_band.height() / (pal.font_sm * scale * LINE_H))
            .floor()
            .max(1.0)) as usize;
        // 长路径/无空格的长串要在任意字符间断行，否则会横向溢出。
        job.wrap.break_anywhere = true;
        let galley = f.ui.painter().layout_job(job);
        f.ui.painter_at(preview_band)
            .galley(pos2(preview_band.min.x, preview_band.min.y), galley, pal.text);
    }

    // 来源 · 时间
    let meta = format!("{} · {}", item.source_app, format_meta(item.created_at));
    let meta_shown = crate::titlebar::elide_text(f.ui, &meta, &meta_font, meta_band.width());
    if !meta_shown.is_empty() {
        // ⚠️ painter_at(meta_band)：来源名可能很长（长 exe 路径），
        // 根 painter 会让它横跨到相邻卡片上。
        f.ui.painter_at(meta_band).text(
            meta_band.left_center(),
            Align2::LEFT_CENTER,
            meta_shown,
            meta_font,
            pal.text_dim,
        );
    }

    // 右侧三个操作：置顶 / 复制 / 删除。
    //
    // ⚠️ **必须用图标字体**（[`crate::icons::Icon`]，PUA 码位
    // `U+E000..=U+E011`），不能用 Unicode 字形。
    //
    // 实测：早前这里写的是 `"⧉"` / `"✕"` / `"★"`，其中 `⧉`(U+29C9)
    // 与 `✕`(U+2715) 在当前字体链（微软雅黑 + 图标字体）里**没有
    // 字形**，于是渲染成**空心方框**（tofu）。`★`/`☆` 恰好在雅黑里
    // 有字形，所以截图里只有「复制/删除」变方框、置顶正常——
    // 这种「一部分符号缺字形」的现象最容易误判成字体没加载。
    //
    // 代价：图标字体只有 18 个码位，「复制」没有现成图标，
    // 只能用 `File`（文档）近似表达「复制到剪贴板」。
    let btn = pal.control_height * 0.7;
    let acts: [(crate::icons::Icon, Op, bool); 3] = [
        (
            if item.pinned {
                crate::icons::Icon::Pinned
            } else {
                crate::icons::Icon::Unpinned
            },
            Op::TogglePin(item.id),
            !item.pinned,
        ),
        (crate::icons::Icon::File, Op::Copy(item.id), true),
        (crate::icons::Icon::Close, Op::Delete(item.id), true),
    ];

    for (i, (icon, op, enabled)) in acts.iter().enumerate() {
        let b = Rect::from_center_size(
            pos2(
                r.max.x - acts_w + (i as f32 + 0.5) * (btn + pal.space_xs),
                r.center().y,
            ),
            vec2(btn, btn),
        );
        if b.min.x < text_x1 {
            break; // 卡片太窄，放不下就不画操作
        }
        let rr = f.ui.interact(b, egui::Id::new(("row_act", item.id, i)), Sense::click());
        let color = if !*enabled {
            pal.text_dim
        } else if i == 2 && rr.hovered() {
            // 删除是破坏性操作，悬停用 danger 色与前两个区分。
            pal.danger
        } else if rr.hovered() {
            pal.text_bright
        } else {
            pal.text_dim
        };
        icon.paint(
            &f.ui.painter_at(b),
            Rect::from_center_size(b.center(), vec2(btn, btn) * 0.78),
            color,
        );
        // 悬停说明：图标本身太小，必须给文字提示。
        let tip = match i {
            0 => {
                if item.pinned {
                    "取消置顶"
                } else {
                    "置顶"
                }
            }
            1 => "复制",
            _ => "删除",
        };
        if rr.clicked() && *enabled {
            f.state.push(op.clone());
        }
        // `on_hover_text` 按值消耗 `Response`，必须是最后一次使用。
        rr.on_hover_text(tip);
    }

    if resp.clicked() {
        f.state.push(Op::Select(item.id));
    }
    if resp.double_clicked() {
        f.state.push(Op::Copy(item.id));
    }
}

// ---------------------------------------------------------------- 状态栏

fn format_meta(ts_ms: i64) -> String {
    modular_clipboard_app::format_time(ts_ms, modular_clipboard_core::now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个跑完字体注册的 `egui::Context`。
    ///
    /// 排版类断言必须走真实字体链：字形是否存在只有 egui 知道，
    /// 纯逻辑测试测不出「这一串字符会不会被排成两行」。
    fn settled_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::theme::install_cjk_font(&ctx, None);
        crate::theme::install_icon_font(&ctx);
        // 注册在第一帧之后生效，必须再跑一帧。
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        ctx
    }

    /// 在 `ctx` 上排版一段文字，返回它占了几行。
    ///
    /// `max_rows` 传 `0` 与传 `usize::MAX` 会得到截然不同的结果，
    /// 所以这个辅助函数把它作为参数供测试对比。
    fn rows_with_max_rows(ctx: &egui::Context, text: &str, max_rows: usize) -> usize {
        ctx.fonts_mut(|f| {
            let mut job = egui::text::LayoutJob::default();
            job.append(
                text,
                0.0,
                egui::TextFormat {
                    font_id: egui::FontId::proportional(14.0),
                    color: egui::Color32::WHITE,
                    ..Default::default()
                },
            );
            job.wrap.max_width = 100.0;
            job.wrap.max_rows = max_rows;
            job.wrap.break_anywhere = true;
            f.layout_job(job).rows.len()
        })
    }

    /// `TextWrapping::max_rows = 0` 表示「一行都不输出」，不是「不限」。
    ///
    /// 实测踩坑：`row_height_for` 里写了 `max_rows = 0`，本意是
    /// 「不限行数以便量出到底需要几行」，实际得到空 galley →
    /// `rows.len()` 恒为 0 → 行高恒等于下限 52pt → 预览带只有 24pt →
    /// 长内容全被省略号截断。诊断数据完全对上：
    ///   row_h=52.8  band_h=24.3  galley_rows=1  elided=true
    ///
    /// 这条测试把「0 会吃掉所有行」钉死，防止有人再写回 0。
    #[test]
    fn max_rows_zero_produces_no_rows() {
        let ctx = settled_ctx();
        let text = "一段比较长的中文内容用来测试折行行数是否被正确计算";
        assert_eq!(
            rows_with_max_rows(&ctx, text, 0),
            0,
            "max_rows=0 时 galley 不应有任何行（它不是「不限」）"
        );
        assert!(
            rows_with_max_rows(&ctx, text, usize::MAX) > 0,
            "max_rows=usize::MAX（不限）时应正常排出版面"
        );
    }

    /// 行高必须随内容长度增长，否则长内容会被固定高度挤掉。
    #[test]
    fn row_height_grows_with_content_length() {
        let ctx = settled_ctx();
        let pal = crate::theme::Palette::dark();
        // ⚠️ `run_ui` 返回 `FullOutput`（不是闭包的返回值），
        // 所以要通过外部变量把结果取出来。
        let mut h = 0.0_f32;
        let mut mk = |text: &str, h: &mut f32| {
            let item = modular_clipboard_core::ClipItem::new(
                modular_clipboard_core::ClipKind::Text,
                "h".into(),
                text.into(),
                "test".into(),
            );
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                *h = row_height_for(ui, &item, &pal, 1.0, 200.0);
            });
            // ⚠️ 必须 `clear()`：排版会往纹理图集里加新字形，
            // `FullOutput` 被 drop 时若还带着未应用的 deltas，egui 会 panic
            // （「Dropped TexturesDelta with N unapplied deltas」）。
            // 生产代码走 `FrameRenderer` 消费 delta，测试里没人消费。
            out.textures_delta.clear();
        };
        mk("短", &mut h);
        let short = h;
        // 200pt 宽约能放 13 个汉字，30 个"很长的内容"必然折多行。
        mk(&"很长的内容".repeat(30), &mut h);
        let long = h;

        assert!(
            long > short,
            "长内容的行高应大于短内容：短={short} 长={long}"
        );
        assert!(
            short >= ROW_HEIGHT_MIN - 0.01,
            "行高不应低于下限 {ROW_HEIGHT_MIN}，实际 {short}"
        );
    }

    /// 行高下限必须容得下「预览一行 + 来源一行 + 内边距」。
    ///
    /// 下限太小会让第二行（来源/时间）被挤出行外。
    #[test]
    fn row_height_min_fits_two_text_lines() {
        assert!(
            ROW_HEIGHT_MIN >= 40.0,
            "两行布局至少需要 40pt：预览 ~19 + 来源 ~13 + 内边距 ~12"
        );
    }
}
