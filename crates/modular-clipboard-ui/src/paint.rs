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
const CARD_RADIUS: f32 = 8.0;

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
}

/// 画一帧。
///
/// 流程：顶栏 → 卡片（Z 序） → 状态栏。
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
    let status = Rect::from_min_max(
        pos2(area.min.x, area.max.y - crate::solver::STATUSBAR_HEIGHT),
        pos2(area.max.x, area.max.y),
    );
    let body = crate::solver::card_area(area);

    draw_topbar(f, top);
    draw_cards(f, body);
    draw_status_bar(f, status);
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
    let title_area = Rect::from_min_max(
        inner.min,
        pos2(inner.min.x + 120.0, inner.max.y),
    );
    let title_font = sized(pal.font_md, scale);
    // ⚠️ 必须 painter_at：标题比标题区宽时，根 painter 会让它
    // 压到搜索框上（这正是旧实现的老问题）。
    f.ui.painter_at(title_area).text(
        title_area.left_center(),
        Align2::LEFT_CENTER,
        "模块化剪贴板",
        title_font,
        pal.text_bright,
    );

    // 搜索框占中间剩余空间。
    let search = Rect::from_min_max(
        pos2(title_area.max.x + pal.space_md, bar.min.y + pal.space_sm),
        pos2(inner.max.x - 160.0 * scale, bar.max.y - pal.space_sm),
    );
    if search.width() > 20.0 && search.height() > 8.0 {
        draw_search(f, search);
    }

    // 右侧：置顶模式切换 + 窗口按钮。
    let right_w = 160.0 * scale;
    let right = Rect::from_min_max(
        pos2(inner.max.x - right_w, bar.min.y),
        pos2(inner.max.x - pal.space_sm, bar.max.y),
    );
    if right.width() > 40.0 {
        draw_pinned_mode_toggle(f, right);
    }

    // 顶栏整体作为拖动区（但搜索框与按钮除外）。
    f.state.drag_region = Some(bar);
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

    let font = sized(pal.font_md, scale);
    let inner = r.shrink2(vec2(pal.space_sm + pal.icon_size, 0.0));
    let resp = f.ui.interact(r, egui::Id::new("card_search"), Sense::click());
    let focused = resp.has_focus();
    if focused {
        f.ui.painter_at(r).rect_stroke(
            r,
            pal.radius_md,
            Stroke::new(1.0, pal.border_strong),
            egui::StrokeKind::Inside,
        );
    }

    // ⚠️ 文字必须 painter_at(inner)：占位文本「搜索历史…」在窄框里
    // 放不下，根 painter 会让它画出框外。
    f.ui.painter_at(inner).text(
        inner.left_center(),
        Align2::LEFT_CENTER,
        if f.state.query.is_empty() {
            "搜索历史…"
        } else {
            f.state.query.as_str()
        },
        font,
        if f.state.query.is_empty() {
            pal.text_dim
        } else {
            pal.text
        },
    );
    if resp.clicked() {
        // 聚焦交由 egui 的 TextEdit 接管；此处只请求重绘。
        f.ui.ctx().request_repaint();
    }
}

fn draw_pinned_mode_toggle(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    let cur = PinnedMode::SharedColumn;
    let font = sized(pal.font_xs, scale);

    // 两个小按钮：共用单栏 / 独立分栏。
    let w = r.width() / 2.0;
    for (i, (mode, label)) in [
        (PinnedMode::SharedColumn, "共用"),
        (PinnedMode::OwnCard, "分栏"),
    ]
    .into_iter()
    .enumerate()
    {
        let b = Rect::from_min_max(
            pos2(r.min.x + i as f32 * w, r.min.y + 2.0),
            pos2(r.min.x + (i + 1) as f32 * w, r.max.y - 2.0),
        );
        let resp = f.ui.interact(b, egui::Id::new(("pinned_mode", i)), Sense::click());
        let active = mode == cur;
        let painter = f.ui.painter_at(b);
        if active {
            painter.rect_filled(b, pal.radius_sm, pal.accent);
        } else if resp.hovered() {
            painter.rect_filled(b, pal.radius_sm, pal.row_hover);
        }
        f.ui.painter_at(b).text(
            b.center(),
            Align2::CENTER_CENTER,
            label,
            font.clone(),
            if active { pal.text_on_accent } else { pal.text_dim },
        );
        if resp.clicked() {
            f.state.push(Op::SetPinnedMode(mode));
        }
    }
}

// ---------------------------------------------------------------- 卡片

fn draw_cards(f: &mut Frame<'_>, _body: Rect) {
    // 按 Z 序遍历。这里直接读 `card.rect`——**不做任何二次判断**，
    // 也不重新计算宽度。矩形是solver 算好的，绘制只负责照着画。
    for card in f.ws.cards.iter() {
        if card.host != CardHost::Docked {
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
    let pal = f.pal;

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

    let row_h = panels_row_height();
    // 只画可见范围：项目里已有 `view::draw_virtual_list` 的虚拟化思路，
    // 这里保持同一水平——列表可能有上千条，全量绘制会拖垮帧率。
    let first = (f.state.scroll_offset / row_h).floor().max(0.0) as usize;
    let visible_rows = ((body.height() / row_h).ceil() as usize).max(1);
    let last = (first + visible_rows + 1).min(items.len());

    for i in first..last {
        let Some(item) = items.get(i) else { continue };
        let y = body.min.y + i as f32 * row_h - f.state.scroll_offset;
        let r = Rect::from_min_size(pos2(body.min.x, y), vec2(body.width(), row_h));
        // 裁到可视区：画到区外的内容会被 scissor 裁掉，但白白产生几何。
        if r.max.y < body.min.y || r.min.y > body.max.y {
            continue;
        }
        draw_item_row(f, r, item);
    }

    // 滚动条：内容超高时才显示。
    let content_h = items.len() as f32 * row_h;
    if content_h > body.height() {
        let track = Rect::from_min_size(
            pos2(body.max.x - 6.0, body.min.y),
            vec2(6.0, body.height()),
        );
        let ratio = body.height() / content_h;
        let thumb_h = (body.height() * ratio).max(24.0);
        let t = (f.state.scroll_offset / (content_h - body.height()).max(1.0)).clamp(0.0, 1.0);
        let thumb = Rect::from_min_size(
            pos2(track.min.x, track.min.y + t * (body.height() - thumb_h)),
            vec2(6.0, thumb_h),
        );
        let p = f.ui.painter_at(track);
        p.rect_filled(track, 3.0, pal.surface_variant);
        p.rect_filled(thumb, 3.0, pal.border);
    }
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

    // 图标 + 预览文本。文本区避开左侧图标，右侧留出操作按钮。
    let icon_w = pal.icon_size + pal.space_sm;
    let acts_w = 3.0 * (pal.control_height * 0.7 + pal.space_xs);
    let text_area = Rect::from_min_max(
        pos2(r.min.x + icon_w, r.min.y),
        pos2((r.max.x - acts_w).max(r.min.x + icon_w), r.max.y),
    );
    let font = sized(pal.font_sm, scale);
    let shown = crate::titlebar::elide_text(f.ui, &item.preview, &font, text_area.width());
    if !shown.is_empty() {
        // ⚠️ painter_at(text_area)：预览可能很长（长路径/长文本），
        // 根 painter 会让它横跨到相邻卡片上。
        f.ui.painter_at(text_area).text(
            text_area.left_center(),
            Align2::LEFT_CENTER,
            shown,
            font,
            if selected { pal.text_bright } else { pal.text },
        );
    }

    // 右侧三个操作：置顶 / 复制 / 删除。
    let btn = pal.control_height * 0.7;
    let acts: [(&str, Op, bool); 3] = [
        (
            "置顶",
            Op::TogglePin(item.id),
            !item.pinned,
        ),
        ("复制", Op::Copy(item.id), true),
        ("删除", Op::Delete(item.id), true),
    ];
    for (i, (_, op, enabled)) in acts.iter().enumerate() {
        let b = Rect::from_center_size(
            pos2(
                r.max.x - acts_w + (i as f32 + 0.5) * (btn + pal.space_xs),
                r.center().y,
            ),
            vec2(btn, btn),
        );
        if b.min.x < text_area.max.x {
            break; // 卡片太窄，放不下就不画操作
        }
        let rr = f.ui.interact(b, egui::Id::new(("row_act", item.id, i)), Sense::click());
        let color = if !*enabled {
            pal.text_dim
        } else if rr.hovered() {
            pal.text_bright
        } else {
            pal.text_dim
        };
        let glyph = match op {
            Op::TogglePin(_) => {
                if item.pinned {
                    "★"
                } else {
                    "☆"
                }
            }
            Op::Copy(_) => "⧉",
            _ => "✕",
        };
        let fsz = sized(pal.font_xs, scale);
        f.ui.painter_at(b).text(b.center(), Align2::CENTER_CENTER, glyph, fsz, color);
        if rr.clicked() && *enabled {
            f.state.push(op.clone());
        }
    }

    if resp.clicked() {
        f.state.push(Op::Select(item.id));
    }
    if resp.double_clicked() {
        f.state.push(Op::Copy(item.id));
    }
}

// ---------------------------------------------------------------- 状态栏

fn draw_status_bar(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let scale = f.scale;
    f.ui
        .painter()
        .hline(r.x_range(), r.min.y, Stroke::new(1.0, pal.border_subtle));

    let font = sized(pal.font_xs, scale);
    let total = f.svc.state.items.len();
    let text = if f.svc.state.capturing {
        format!("监听中 · 共 {total} 条")
    } else {
        format!("监听已停止 · 共 {total} 条")
    };

    // 右侧按钮区宽度（清空 + 设置）。
    let right_w = (pal.font_xs * 2.0 + pal.space_md * 2.0) * scale + pal.icon_size * scale;
    // ⚠️ 右侧区右端必须**内缩**一个 space_sm：否则 right_to_left 布局
    // 会把最后一个控件贴在客户区最右缘，无边框窗口外面就是桌面，
    // 按钮看上去像被切掉一截。
    let right = Rect::from_min_max(
        pos2(r.max.x - right_w - pal.space_sm, r.min.y),
        pos2(r.max.x - pal.space_sm, r.max.y),
    );

    // 左侧文本区：从 x=0 到右侧区起点，中间留一个间隙。
    let left = Rect::from_min_max(
        pos2(r.min.x, r.min.y),
        pos2((right.min.x - pal.space_md).max(r.min.x), r.max.y),
    );
    let shown = crate::titlebar::elide_text(f.ui, &text, &font, left.width());
    if !shown.is_empty() {
        f.ui.painter_at(left).text(
            left.left_center(),
            Align2::LEFT_CENTER,
            shown,
            font.clone(),
            pal.text_dim,
        );
    }

    // 「清空」与「设置」。
    let bw = (pal.font_xs * 2.0 + pal.space_md * 2.0) * scale;
    let clear = Rect::from_min_max(
        pos2(right.min.x, right.min.y),
        pos2(right.min.x + bw, right.max.y),
    );
    let r1 = f.ui.interact(clear, egui::Id::new("status_clear"), Sense::click());
    f.ui.painter_at(clear).text(
        clear.center(),
        Align2::CENTER_CENTER,
        "清空",
        font.clone(),
        if r1.hovered() { pal.danger } else { pal.text_dim },
    );
    r1.on_hover_text("清空全部历史");

    let settings = Rect::from_min_max(
        pos2(clear.max.x, clear.min.y),
        pos2((clear.max.x + bw).min(right.max.x), clear.max.y),
    );
    let r2 = f
        .ui
        .interact(settings, egui::Id::new("status_settings"), Sense::click());
    f.ui.painter_at(settings).text(
        settings.center(),
        Align2::CENTER_CENTER,
        "设置",
        font,
        if r2.hovered() { pal.text_bright } else { pal.text_dim },
    );
}

// ---------------------------------------------------------------- 工具

fn panels_row_height() -> f32 {
    40.0
}

fn format_meta(ts_ms: i64) -> String {
    modular_clipboard_app::format_time(ts_ms, modular_clipboard_core::now_ms())
}
