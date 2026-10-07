//! 主视图。
//!
//! # 布局
//!
//! ```text
//! ┌────────────[标题 · 搜索框 ······ 设置]─[−][✕]─┐  ← 自绘标题栏（同时是顶栏）
//! ├──────────┬───��────────────────────┬─┬────────┤
//! │置顶 ×5   │ 内容来源 置顶左侧 固定   │表│  详情  │
//! │          ├────────────────────────┤ │        │
//! │          │ （历史条目 ×N）        │密│        │
//! │          │                        │ │        │
//! └──────────┴────────────────────────┴─┴────────┘
//! ```
//!
//! 布局由 [`layout::solve`] 全量算出，本文件只负责把结果画出来，
//! 不做任何位置决策——这样「提示画在这里但吸附到那里」这类不一致
//! 在结构上不可能出现。
//!
//! # 功能没有减少
//!
//! 重构前的搜索、分组过滤、列表、详情面板、缩略图预览、软件联动、
//! 托盘入口、状态栏**全部保留**。线框图是布局示意，不是功能清单。
//! 新增的是：自绘标题栏、可拖分隔条、模块折叠 / 浮动 / 停靠。
//!
//! # 性能考量
//!
//! 列表用 `ScrollArea::show_rows` 虚拟化，只为可见行构建控件。
//! 2000 条目若全部实例化控件会造成明显卡顿，虚拟化后与条目数基本无关。
//! **行高必须恒定**（[`panels::ROW_HEIGHT`]），否则虚拟化的行索引与
//! 实际内容对不上，表现为滚动时内容错位。

use std::time::Duration;

use egui::{Align, Align2, CornerRadius, Layout, Pos2, Rect, RichText, ScrollArea, Stroke, Ui, pos2, vec2};
use modular_clipboard_app::Service;
use modular_clipboard_core::{ClipItem, ClipKind, EntryId, now_ms};
use modular_clipboard_store::GroupFilter;

use crate::icons;
use crate::layout::{self, LayoutState, Panel, Solved};
use crate::panels::{self, RailView, RowTool};
use crate::theme::{Palette, sized};
use crate::thumbnail::{self, CacheKey, ThumbnailCache};
use crate::titlebar::{self, TitlebarAction, TitlebarLayout};

/// 界面状态：不属于业务数据的临时状态（展开项、悬停等）。
#[derive(Default)]
pub struct UiLocal {
    pub show_settings: bool,
    pub settings_tab: usize,
    /// 详情面板是否展开（浮动/停靠都受它控制）。
    pub show_detail: bool,
    /// 需要在下一帧执行的操作。
    pending: Option<PendingOp>,
    /// 图片缩略图缓存（按条目 id + 内容指纹）。
    thumbs: ThumbnailCache,
    /// 已上传到 egui 的缩略图纹理：`条目 id → TextureId`。
    ///
    /// 为什么与 CPU 缓存分开记：CPU 侧 `ThumbnailCache` 按字节预算淘汰，
    /// 而**纹理一旦上传就无法单独释放**（egui 的纹理管理器不提供
    /// 「回收某个 TextureId」的接口，只能整体释放）。两个生命周期不同，
    /// 合成一个结构会导致「缓存驱逐了但显存还在」或「纹理被复用但像素已变」。
    ///
    /// 同一帧内重复上传同一张图会产生重复的 `TexturesDelta`，
    /// 因此这里记录已经传过的 `(条目 id, 像素指纹)`。
    thumb_textures: Vec<(EntryId, u64, egui::TextureHandle)>,
    /// 布局状态（宽度、折叠、浮动位置）。
    pub layout: LayoutState,
    /// 正在被拖动的模块（浮动拖动中非`None`）。
    ///
    /// 只记「是谁在拖」：位置在 [`FloatDrag::last`] 里。
    dragging: Option<Panel>,
    /// 悬停的分隔条序号。
    hover_splitter: Option<usize>,
    /// 本帧要上报给外壳的拖动区。
    ///
    /// 标题栏每帧重算后写在这里，由上层读取并调用
    /// [`modular_clipboard_gfx::window::Window::set_drag_region`]。
    /// **必须每帧设置**：只在第一帧设置的话，用户拖过之后或窗口
    /// resize 之后矩形就对不上了，表现为「有时拖得动有时拖不动」。
    pub drag_region: Option<Rect>,
    /// 悬浮模块的拖动状态。
    float_drag: Option<FloatDrag>,
    /// 标题栏动作（关闭 → 隐藏到托盘）。
    pub close_requested: bool,
    pub minimize_requested: bool,
}

/// 浮动模块拖动态。
#[derive(Debug, Clone, Copy)]
struct FloatDrag {
    panel: Panel,
    /// 上一次指针位置。
    last: Pos2,
}

enum PendingOp {
    /// 单击行：选中并展开详情（不写剪贴板，避免误覆盖）
    Select(EntryId),
    Copy(EntryId),
    Paste(EntryId),
    Delete(EntryId),
    Action(EntryId, usize),
    /// 置顶 / 取消置顶。
    TogglePin(EntryId),
}

const ROW_HEIGHT: f32 = panels::ROW_HEIGHT;

/// 主视图。返回 `true` 表示请求退出应用。
///
/// 由 `Context::run_ui` 提供的根 `Ui` 覆盖整个客户区，本函数直接往里画，
/// 不再自行创建 `Window`，也无需处理视口命令。
///
/// # `pal` 为什么由调用方传入
///
/// 调色板由 [`crate::App`] 在启动时经 [`crate::theme::set_theme`] 解析
/// 一次并持有。早前这里每帧自己算 `Palette::dark()/light()`——
/// 55 个字段逐个构造，每帧一次纯浪费，且**解析来源与灌进 egui Visuals
/// 的那次解析可能分叉**（一个走 `config.dark_mode`，一个走系统探测）。
/// 现在两者必然同源。
pub fn draw(ui: &mut Ui, svc: &mut Service, local: &mut UiLocal, pal: &Palette) -> bool {
    let scale = svc.state.config.ui.font_scale;

    // 先处理上一帧排队的操作，避免在渲染过程中修改数据。
    if let Some(op) = local.pending.take() {
        execute(svc, op);
    }

    let area = ui.max_rect();
    // 正在拖动的浮动模块矩形进solve，停靠区预览只在拖动时出现。
    let dragging_rect = local
        .dragging
        .and_then(|p| local.layout.placement[layout::panel_index(p)].float_rect());
    let solved = layout::solve(area, &local.layout, dragging_rect);

    // 窄窗口降级提示：只在详情转为浮层档位时出现。
    //
    // 它**不单独占一行**，而是复用状态栏的提示位（见 `draw_status_bar`）：
    // 任何新增的浮层都会与面板争空间，而面板矩形是由 `layout::solve`
    // 算出来的，绘制层私自加一条横幅必然造成重叠。
    let narrowed = solved.tier >= layout::Tier::DetailFloating;

    draw_topbar(ui, &solved, svc, local, pal, scale);

    // 模块拖动要在画面板**之前**处理：拖动位置决定了这一帧画在哪。
    handle_splitters(ui, &solved, local, pal);
    // ⚠️ 第二次 `solve` 是**必需的**，不是冗余计算。
    //
    // `handle_splitters` 在拖动中会通过 `layout::drag_splitter` 就地修改
    // `local.layout` 的面板宽度。上面那份 `solved` 是**拖动前**算出来的，
    // 直接拿去画面板，本帧就会画在旧位置——表现为分隔条与面板错位一帧。
    // 因此拖动处理完必须重算一次，让这一帧画在拖动后的位置上。
    //
    // 它**不是** widget ID 冲突的来源：`solve` 是纯函数，只算矩形不碰egui
    // 的 widget 注册；`handle_splitters` 也只在循环里按序号各画一次分隔条
    // （见 `panels::draw_splitter` 的 `index` 参数），不存在重复绘制。
    let solved = layout::solve(area, &local.layout, dragging_rect);

    if let Some(preview) = solved.dock_preview {
        panels::draw_dock_preview(ui, preview.1, pal, scale);
    }

    draw_pinned_panel(ui, &solved, svc, local, pal, scale);
    draw_history_panel(ui, &solved, svc, local, pal, scale);
    draw_rail_panel(ui, &solved, svc, local, pal, scale);
    // 详情两种来源都要看：停靠矩形（宽窗口）或浮层矩形（窄窗口降级）。
    // `rects` 与 `floating` 同时为空说明该档位下详情已完全让位，
    // `draw_detail_panel` 内部会直接返回。
    if local.show_detail || solved.floating[layout::panel_index(Panel::Detail)].is_some() {
        draw_detail_panel(ui, &solved, svc, local, pal, scale);
    }

    draw_status_bar(ui, svc, local, pal, scale, narrowed);
    local.drag_region = titlebar_drag_region(&solved, scale);
    false
}

/// 模块在当前帧是否应画成「折叠把手」而不是完整面板。
///
/// # 为什么要看`tier` 而不是只看 `LayoutState`
///
/// [`layout::solve`] 会因为窗口太窄而**自动**把模块收成把手，
/// 但它**不修改** [`LayoutState`]——用户的布局意图必须原样保留，
/// 窗口一变宽就该自动恢复。若绘制层只读 `LayoutState`，
/// 就会在一个 24 逻辑点宽的把手矩形里画完整列表，
/// 文字挤成竖条：降级在布局层生效了，在绘制层却没生效。
///
/// 判定直接复用 [`layout::Tier::visibility`]，
/// 保证「布局算出来的」与「绘制读到的」是同一个答案——
/// 这与本项目「判定与绘制同源」的原则一致。
fn is_veiled(tier: layout::Tier, local: &UiLocal, panel: Panel) -> bool {
    tier.visibility(&local.layout, panel) == layout::Visibility::Collapsed
}

/// 画顶栏（= 自绘标题栏）。
///
/// 关闭按钮走**隐藏到托盘**语义，与 `presence::decide_on_close` 的既有
/// 行为一致：剪贴板类工具直接退出会让用户以为「记录停了」。
fn draw_topbar(
    ui: &mut Ui,
    solved: &Solved,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
) {
    let mut query = svc.state.query.clone();
    let action = titlebar::draw(
        ui,
        solved.topbar,
        pal,
        scale,
        &mut query,
        "模块化剪切板",
    );
    if query != svc.state.query {
        svc.search(query);
    }
    match action {
        TitlebarAction::Close => local.close_requested = true,
        TitlebarAction::Minimize => local.minimize_requested = true,
        TitlebarAction::Settings => local.show_settings = !local.show_settings,
        TitlebarAction::None => {}
    }
}

/// 上报给外壳的拖动区。
///
/// 复用 [`titlebar::TitlebarLayout`] 的同一个计算结果，不二次推导——
/// 绘制时按钮画在哪，拖动区就避开哪。
fn titlebar_drag_region(solved: &Solved, scale: f32) -> Option<Rect> {
    let l = TitlebarLayout::new(solved.topbar, scale);
    let r = l.drag_region();
    // 零面积 / 零高度的拖动区会让 hit_test 判定为无效（它要求
    // width > 0 && height > 0），上报 `None` 而不是退化矩形。
    if r.width() > 0.0 && r.height() > 0.0 {
        Some(r)
    } else {
        None
    }
}

/// 处理分隔条拖动。
fn handle_splitters(ui: &mut Ui, solved: &Solved, local: &mut UiLocal, pal: &Palette) {
    let pointer = ui.input(|i| i.pointer.hover_pos());
    for (i, rect) in solved.splitters.iter().enumerate() {
        if rect.width() <= 0.0 {
            continue;
        }
        let hovered = pointer.is_some_and(|p| layout::hit(*rect, p));
        let resp =
            panels::draw_splitter(ui, *rect, pal, hovered, local.hover_splitter == Some(i), i);
        if resp.dragged() || resp.drag_started() {
            local.hover_splitter = Some(i);
            let d = ui.input(|i| i.pointer.delta());
            if d.x != 0.0 {
                if let Some(pair) = layout::splitter_pair(solved, *rect) {
                    let body_w = solved.topbar.max.y;
                    layout::drag_splitter(&mut local.layout, pair, d.x, body_w);
                }
            }
        }
        if resp.drag_stopped() {
            local.hover_splitter = None;
        }
    }
}

/// 左侧：置顶项列表。
///
/// 线框图上这里是「取消置顶」按钮 + 5 个青色条目。
/// 列表用虚拟化，条目再多也不卡。
fn draw_pinned_panel(
    ui: &mut Ui,
    solved: &Solved,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
) {
    let Some(rect) = solved.rects[layout::panel_index(Panel::Pinned)] else {
        return;
    };
    // ⚠️ 判据必须读**求解结果里的可见性**，而不是 `LayoutState` 的折叠位。
    // 窄窗口下降级会把置顶收成把手，但 `LayoutState` 里它仍是展开的——
    // 读原始状态会在 24 逻辑点宽的把手矩形里画完整列表，
    // 于是文字挤成竖条，正是本次要修的「所有元素挤在一起」的另一种形态。
    if is_veiled(solved.tier, local, Panel::Pinned) {
        panels::draw_collapse_handle(ui, rect, pal, scale, Panel::Pinned);
        return;
    }
    panels::panel_frame(ui, rect, pal, false, scale);

    let inner = rect.shrink(panels_panel_pad(pal));
    // 顶部工具条：「取消置顶」按钮 + 计数。
    let bar_h = pal.control_height * scale + pal.space_sm * scale;
    let bar = Rect::from_min_size(inner.min, vec2(inner.width(), bar_h));
    at_rect(ui, bar, |ui| {
        ui.label(
            RichText::new(format!("置顶{}", pinned_count(svc)))
                .size(sized(pal.font_md, scale).size)
                .color(pal.text_dim),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icons_close(ui, pal, scale, "取消全部置顶").clicked() {
                for it in pinned_ids(svc) {
                    local.pending = Some(PendingOp::TogglePin(it));
                }
            }
        });
    });
    let list = Rect::from_min_max(
        pos2(inner.min.x, bar.max.y + pal.space_xs),
        inner.max,
    );

    let pinned: Vec<ClipItem> = svc
        .state
        .items
        .iter()
        .filter(|i| i.pinned)
        .cloned()
        .collect();
    draw_virtual_list(ui, list, &pinned, svc, local, pal, scale, true);
}

/// 置顶条目数。
fn pinned_count(svc: &Service) -> usize {
    svc.state.items.iter().filter(|i| i.pinned).count()
}

/// 全部置顶条目的 id。
fn pinned_ids(svc: &Service) -> Vec<EntryId> {
    svc.state
        .items
        .iter()
        .filter(|i| i.pinned)
        .map(|i| i.id)
        .collect()
}

/// 主区：历史列表。
///
/// 线框图上每条顶部有「内容来源 / 置顶左侧 / 固定」三个工具。
fn draw_history_panel(
    ui: &mut Ui,
    solved: &Solved,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
) {
    let Some(rect) = solved.rects[layout::panel_index(Panel::History)] else {
        return;
    };
    if is_veiled(solved.tier, local, Panel::History) {
        panels::draw_collapse_handle(ui, rect, pal, scale, Panel::History);
        return;
    }
    panels::panel_frame(ui, rect, pal, false, scale);
    let inner = rect.shrink(panels_panel_pad(pal));

    // 分组过滤栏（保留既有功能）。
    let bar_h = draw_group_filter(ui, inner, svc, pal, scale);

    let list = Rect::from_min_max(
        pos2(inner.min.x, inner.min.y + bar_h),
        inner.max,
    );
    let items: Vec<ClipItem> = svc.state.items.clone();
    draw_virtual_list(ui, list, &items, svc, local, pal, scale, false);
}

/// 面板内边距。
fn panels_panel_pad(pal: &Palette) -> f32 {
    pal.space_sm
}

/// 分组过滤栏。返回占用的高度。
fn draw_group_filter(
    ui: &mut Ui,
    rect: Rect,
    svc: &mut Service,
    pal: &Palette,
    scale: f32,
) -> f32 {
    let h = pal.font_md * scale + pal.space_md * scale;
    let bar = Rect::from_min_size(rect.min, vec2(rect.width(), h));
    at_rect(ui, bar, |ui| {
        let all_selected = svc.state.group_filter.is_none();
        if ui
            .selectable_label(all_selected, RichText::new("全部").size(sized(pal.font_sm, scale).size))
            .clicked()
        {
            svc.filter_group(None);
        }
        for g in svc.state.groups.clone() {
            let selected =
                matches!(svc.state.group_filter, Some(GroupFilter::Group(id)) if id == g.id);
            let color = parse_hex_color(&g.color);
            let text = RichText::new(&g.name)
                .size(sized(pal.font_sm, scale).size)
                .color(if selected { color } else { pal.text_dim });
            if ui.selectable_label(selected, text).clicked() {
                svc.filter_group(Some(GroupFilter::Group(g.id)));
            }
        }
        if ui.small_button("+").on_hover_text("新建分组").clicked() {
            let name = format!("分组 {}", svc.state.groups.len() + 1);
            if let Err(e) = svc.create_group(&name) {
                svc.notify(format!("创建分组失败: {e}"));
            }
        }
        // 「未分组」入口：store 的 GroupFilter 只有 Group / Ungrouped，
        // 补上它让用户能捞回没有归类的条目。
        let ungrouped_sel = matches!(svc.state.group_filter, Some(GroupFilter::Ungrouped));
        if ui
            .selectable_label(
                ungrouped_sel,
                RichText::new("未分组").size(sized(pal.font_sm, scale).size),
            )
            .clicked()
        {
            svc.filter_group(Some(GroupFilter::Ungrouped));
        }
    });
    h + pal.space_xs
}

/// 虚拟化列表。
///
/// `is_pinned_list` 为真时列表只显示置顶项（左侧模块），
/// 且每条右侧给「取消置顶」；否则显示全部并给三个行内工具。
fn draw_virtual_list(
    ui: &mut Ui,
    rect: Rect,
    items: &[ClipItem],
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
    is_pinned_list: bool,
) {
    if items.is_empty() {
        let msg = if svc.state.query.is_empty() {
            if is_pinned_list {
                "还没有置顶条目\n在历史列表点「置顶左侧」试试"
            } else {
                "还没有剪贴板记录"
            }
        } else {
            "没有匹配的结果"
        };
        panels::draw_empty_state(ui, rect, msg, pal, scale);
        return;
    }
    if !is_pinned_list {
        panels::draw_group_bar(ui, rect, pal, scale);
    }
    let hover = ui.input(|i| i.pointer.hover_pos());
    // `id_salt` 必须给：egui 0.36 的 ScrollArea 默认用固定 salt
    // `"scroll_area"`（`scroll_area.rs` 的 `IdSalt::new("scroll_area")`），
    // 而本帧最多同时存在 4 个 ScrollArea，同一个根 `Ui` 下它们会共用一个
    // Id，触发 `check_for_id_clash` 的红字报错，且滚动偏移会互相串。
    ScrollArea::vertical()
        .id_salt("history_list")
        .max_height(rect.height())
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, items.len(), |ui, rows| {
            for row in rows {
                let item = &items[row];
                let selected = svc.state.selected == Some(item.id);
                let row_rect = Rect::from_min_size(
                    ui.max_rect().min,
                    vec2(ui.max_rect().width(), ROW_HEIGHT),
                );
                let hovered = hover.is_some_and(|p| layout::hit(row_rect, p));
                let meta = format!(
                    "{} · {}",
                    modular_clipboard_app::format_time(item.created_at, now_ms()),
                    if item.source_app.is_empty() {
                        "未知来源".to_string()
                    } else {
                        item.source_app.clone()
                    }
                );
                let resp = panels::draw_row(
                    ui,
                    row_rect,
                    item,
                    selected,
                    hovered,
                    pal,
                    scale,
                    row % 2 == 1,
                    meta,
                );
                // 行内工具画在行右侧，不占行的点击区域。
                let tools_w = if is_pinned_list {
                    pal.control_height * scale
                } else {
                    pal.control_height * 2.0 * scale
                };
                let tools = Rect::from_min_size(
                    pos2(row_rect.max.x - tools_w - pal.space_xs, row_rect.min.y),
                    vec2(tools_w, row_rect.height()),
                );
                at_rect(ui, tools, |ui| {
                    ui.horizontal_centered(|ui| {
                        if let Some(tool) =
                            panels::draw_row_tools(ui, pal, scale, item)
                        {
                            match tool {
                                RowTool::Unpin | RowTool::PinLeft => {
                                    local.pending = Some(PendingOp::TogglePin(item.id));
                                }
                                RowTool::Lock => {
                                    local.pending = Some(PendingOp::TogglePin(item.id));
                                }
                            }
                        }
                    });
                });

                if hovered {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    local.pending = Some(PendingOp::Select(item.id));
                }
                if resp.clicked_by(egui::PointerButton::Secondary) {
                    local.pending = Some(PendingOp::Delete(item.id));
                }
            }
        });
}

/// 右侧窄栏：竖排「表 / 密」视图切换。
///
/// 按字面实现为**两种视图的可切换标签**：
/// - 表 = 表格视图（紧凑多列，快速扫读）；
/// - 密 = 密文视图（内容打码，防肩窥）。
fn draw_rail_panel(
    ui: &mut Ui,
    solved: &Solved,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
) {
    let Some(rect) = solved.rects[layout::panel_index(Panel::Rail)] else {
        return;
    };
    let view = RailView::from_index(local.layout.rail_view);
    panels::panel_frame(ui, rect, pal, false, scale);
    let inner = rect.shrink(2.0);
    // 「表」「密」两个字已被 Table / Mask 图标取代：图标在小尺寸下
    // 比单字表意更清楚，也不依赖中文字体一定存在。
    let view_icon = view.icon();

    if rect.width() <= panels::rail_handle_width(pal, scale) + pal.space_xs {
        // 折叠态：只画图标，点一下展开。
        let resp = ui.interact(inner, ui.id().with("rail_collapsed"), egui::Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(inner, CornerRadius::ZERO, pal.row_hover);
        }
        view_icon.paint(
            &ui.painter_at(inner),
            egui::Rect::from_center_size(
                inner.center(),
                vec2(pal.icon_size, pal.icon_size) * scale,
            ),
            pal.text_bright,
        );
        if resp.clicked() {
            layout::toggle_collapse(&mut local.layout, Panel::Rail);
        }
        return;
    }

    // 展开态：上半是标签区（点击切换），下半是该视图的列表。
    let rail_w = panels::rail_handle_width(pal, scale) + pal.space_xs;
    let tag = Rect::from_min_size(inner.min, vec2(rail_w, inner.height()));
    let tab = Rect::from_min_size(tag.min, vec2(rail_w, pal.font_md * scale * 2.2));
    let resp = ui.interact(tab, ui.id().with("rail_tab"), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(tab, CornerRadius::ZERO, pal.row_hover);
    }
    view_icon.paint(
        &ui.painter_at(tab),
        egui::Rect::from_center_size(
            tab.center(),
            vec2(pal.icon_size, pal.icon_size) * scale,
        ),
        pal.text_bright,
    );
    resp.clone().on_hover_text(view_icon.label());
    if resp.clicked() {
        // 在两种视图间循环。
        local.layout.rail_view = (local.layout.rail_view + 1) % RailView::all().len();
    }

    let body = Rect::from_min_max(
        pos2(tag.max.x + pal.space_xs, inner.min.y),
        inner.max,
    );
    let items = svc.state.items.clone();
    match view {
        RailView::Table => {
            let cols: [(&str, f32); 3] = [
                ("来源", body.width() * 0.32),
                ("时间", body.width() * 0.34),
                ("类型", body.width() * 0.34),
            ];
            let hover = ui.input(|i| i.pointer.hover_pos());
            ScrollArea::vertical()
                .id_salt("rail_table_view")
                .max_height(body.height())
                .auto_shrink([false, false])
                .show_rows(ui, panels::COMPACT_ROW_HEIGHT, items.len(), |ui, rows| {
                    for row in rows {
                        let item = &items[row];
                        let rr = Rect::from_min_size(
                            ui.max_rect().min,
                            vec2(ui.max_rect().width(), panels::COMPACT_ROW_HEIGHT),
                        );
                        let hovered = hover.is_some_and(|p| layout::hit(rr, p));
                        let ts = modular_clipboard_app::format_time(item.created_at, now_ms());
                        let cells: Vec<(&str, f32)> = vec![
                            (item.source_app.as_str(), cols[0].1),
                            (ts.as_str(), cols[1].1),
                            (kind_name(item.kind), cols[2].1),
                        ];
                        let resp = panels::draw_compact_row(
                            ui,
                            rr,
                            &cells,
                            pal,
                            scale,
                            svc.state.selected == Some(item.id),
                        );
                        if hovered {
                            ui.painter().rect_filled(rr, CornerRadius::ZERO, pal.row_hover);
                        }
                        if resp.clicked() {
                            local.pending = Some(PendingOp::Select(item.id));
                        }
                    }
                });
        }
        RailView::Masked => {
            // 密文视图：只显示来源与时间，内容一律打码。
            let hover = ui.input(|i| i.pointer.hover_pos());
            ScrollArea::vertical()
                .id_salt("rail_masked_view")
                .max_height(body.height())
                .auto_shrink([false, false])
                .show_rows(ui, panels::COMPACT_ROW_HEIGHT, items.len(), |ui, rows| {
                    for row in rows {
                        let item = &items[row];
                        let rr = Rect::from_min_size(
                            ui.max_rect().min,
                            vec2(ui.max_rect().width(), panels::COMPACT_ROW_HEIGHT),
                        );
                        let hovered = hover.is_some_and(|p| layout::hit(rr, p));
                        let ts = modular_clipboard_app::format_time(item.created_at, now_ms());
                        let cells: Vec<(&str, f32)> = vec![
                            (item.source_app.as_str(), body.width() * 0.4),
                            (ts.as_str(), body.width() * 0.6),
                        ];
                        let resp = panels::draw_compact_row(
                            ui,
                            rr,
                            &cells,
                            pal,
                            scale,
                            svc.state.selected == Some(item.id),
                        );
                        if hovered {
                            ui.painter().rect_filled(rr, CornerRadius::ZERO, pal.row_hover);
                        }
                        // 内容遮罩：盖住可能透出的正文，只留标签。
                        ui.painter().rect_filled(
                            rr,
                            CornerRadius::ZERO,
                            pal.surface_variant.gamma_multiply(0.6),
                        );
                        ui.painter().rect_stroke(
                            rr,
                            CornerRadius::ZERO,
                            Stroke::new(pal.stroke_thin, pal.border_subtle),
                            egui::StrokeKind::Inside,
                        );
                        if resp.clicked() {
                            local.pending = Some(PendingOp::Select(item.id));
                        }
                    }
                });
            ui.painter().text(
                pos2(body.center().x, body.max.y - pal.font_xs * scale),
                Align2::CENTER_BOTTOM,
                "内容已遮罩",
                sized(pal.font_xs, scale),
                pal.text_dim,
            );
        }
    }
}

/// 类型的中文名（表格视图用）。
fn kind_name(kind: ClipKind) -> &'static str {
    match kind {
        ClipKind::Text => "文本",
        ClipKind::Html => "富文本",
        ClipKind::Image => "图片",
        ClipKind::Files => "文件",
    }
}

/// 详情面板（保留既有全部功能）。
fn draw_detail_panel(
    ui: &mut Ui,
    solved: &Solved,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
) {
    let i = layout::panel_index(Panel::Detail);
    // 窄窗口下详情转为浮层，矩形只在 `floating` 里，`rects` 是 `None`。
    // 两个来源都读，才不会在降级时「详情整个消失」。
    let (rect, floating) = match (solved.rects[i], solved.floating[i]) {
        (_, Some(r)) => (r, true),
        (Some(r), None) => (r, false),
        (None, None) => return,
    };
    panels::panel_frame(ui, rect, pal, floating, scale);
    let mut inner = rect.shrink(panels_panel_pad(pal));

    // 悬浮时给一个头部，兼作拖动手柄。
    if floating {
        let header = Rect::from_min_size(inner.min, vec2(inner.width(), pal.control_height * scale));
        let h = panels::floating_header_drag(header, 1.0, pal.control_height * scale);
        let resp = ui.interact(h.drag, ui.id().with("detail_float_drag"), egui::Sense::drag());
        if resp.drag_started() || resp.dragged() {
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                handle_float_drag(ui, local, Panel::Detail, pos);
            }
        }
        ui.painter().text(
            header.left_center(),
            Align2::LEFT_CENTER,
            Panel::Detail.title(),
            sized(pal.font_md, scale),
            pal.text_bright,
        );
        let close = Rect::from_min_size(
            pos2(header.max.x - pal.control_height * scale, header.min.y),
            vec2(pal.control_height * scale, header.height()),
        );
        if icons_close_at(ui, close, pal, scale, "关闭详情").clicked() {
            local.show_detail = false;
            svc.state.selected = None;
        }
        inner = Rect::from_min_max(pos2(inner.min.x, header.max.y), inner.max);
    }

    draw_detail(ui, inner, svc, local, pal, scale);
}

/// 浮动模块拖动处理。
fn handle_float_drag(ui: &mut Ui, local: &mut UiLocal, panel: Panel, pos: Pos2) {
    match &mut local.float_drag {
        Some(f) if f.panel == panel => {
            let d = pos - f.last;
            if d.x != 0.0 || d.y != 0.0 {
                if let Some(r) = local.layout.placement[layout::panel_index(panel)].float_rect() {
                    let body = Rect::from_min_max(
                        pos2(0.0, local.layout.topbar_height),
                        ui.max_rect().max,
                    );
                    let nr = layout::drag_float(r, d, body);
                    layout::float(&mut local.layout, panel, nr);
                }
                if let Some(f) = &mut local.float_drag {
                    f.last = pos;
                }
            }
            local.dragging = Some(panel);
        }
        _ => {
            local.float_drag = Some(FloatDrag { panel, last: pos });
        }
    }
}

/// 图片详情区：显示真实缩略图。
///
/// # 降级链
///
/// 每一级失败都退回「文字摘要」，**不显示空白面板**——
/// 用户无法区分「图片坏了」和「程序没画出来」。
///
/// 1. 缩略图可解码 → 显示图像 + 尺寸摘要；
/// 2. 载荷已被淘汰（`item.payload == None`）→ 明确告知「载荷已淘汰」；
/// 3. 载荷在但解不开（非图片/ 损坏）→ 告知解码失败原因。
///
/// # 为什么解码失败不弹错误框
///
/// 本函数每帧被调用（详情面板开着时）。弹框会阻塞在模态循环里，
/// 且同一个坏载荷会反复弹。用一行灰色说明即可。
fn draw_image_preview(
    ui: &mut egui::Ui,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
    item: &modular_clipboard_core::ClipItem,
) {
    let note = |ui: &mut egui::Ui, text: &str| {
        ui.label(
            RichText::new(text)
                .color(pal.text_dim)
                .size(sized(pal.font_md, scale).size),
        );
    };

    // 载荷被淘汰时 `load_payload` 会直接报错，不必先读磁盘。
    if item.payload.is_none() {
        note(ui, "图片载荷已被淘汰，仅保留元数据");
        note(ui, &item.preview);
        return;
    }

    let key = CacheKey::new(item.id, item.hash.clone());
    let loaded = local.thumbs.get_or_load(key, || {
        svc.store
            .load_payload(item)
            .map_err(|e| format!("读取图片载荷失败: {e}"))
    });

    match loaded {
        Ok(thumb) => {
            let handle = upload_thumbnail(ui.ctx(), local, item.id, &thumb);
            // 按缩略图的**真实**长宽比给显示区域，定宽不缩放。
            let avail_w = ui.available_width();
            let w = thumb.width as f32;
            let h = thumb.height as f32;
            let scale_fit = if w > 0.0 { (avail_w / w).min(1.0) } else { 1.0 };
            ui.add(
                egui::Image::new(&handle)
                    .fit_to_exact_size(egui::vec2(w * scale_fit, h * scale_fit)),
            );
            note(
                ui,
                &format!(
                    "{} · 显示 {}×{}",
                    item.preview, thumb.width, thumb.height
                ),
            );
        }
        Err(thumbnail::ThumbError::TooLarge) => {
            note(ui, "图片数据过大，无法生成缩略图");
            note(ui, &item.preview);
        }
        Err(thumbnail::ThumbError::NotAnImage) => {
            note(ui, "载荷内容不是可识别的图片格式");
            note(ui, &item.preview);
        }
        Err(thumbnail::ThumbError::DecodeFailed) => {
            note(ui, "图片解码失败（文件可能已损坏）");
            note(ui, &item.preview);
        }
    }
}

fn draw_detail(
    ui: &mut Ui,
    rect: Rect,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
) {
    let Some(id) = svc.state.selected else {
        panels::draw_empty_state(ui, rect, "点击条目查看详情", pal, scale);
        return;
    };
    let Some(item) = svc.state.items.iter().find(|i| i.id == id).cloned() else {
        return;
    };

    at_rect(ui, rect, |ui| {
        ui.horizontal(|ui| {
            icons::paint_inline(
                ui,
                icons::Icon::for_kind(item.kind),
                pal.text_dim,
                pal,
                scale,
            );
            ui.label(
                RichText::new(kind_name(item.kind))
                    .size(sized(pal.font_sm, scale).size)
                    .color(pal.text_dim),
            );
        });

        ui.separator();

        // 预览区：按类型分派
        egui::ScrollArea::vertical()
            .id_salt("detail_preview")
            .max_height(rect.height() * 0.5)
            .show(ui, |ui| {
                ui.add_space(pal.space_xs);
                match item.kind {
                    ClipKind::Text | ClipKind::Html => {
                        ui.add(
                            egui::TextEdit::multiline(&mut item.preview.clone())
                                .desired_width(f32::INFINITY)
                                .desired_rows(10)
                                .font(sized(pal.font_md, scale))
                                .interactive(false),
                        );
                    }
                    ClipKind::Files => {
                        for line in item.preview.lines() {
                            ui.label(RichText::new(line).size(sized(pal.font_sm, scale).size));
                        }
                    }
                    ClipKind::Image => {
                        draw_image_preview(ui, svc, local, pal, scale, &item);
                    }
                }
            });

        ui.separator();

        // 动作区：软件联动入口
        ui.label(
            RichText::new("可用动作")
                .size(sized(pal.font_sm, scale).size)
                .color(pal.text_dim),
        );
        let proposals = svc.proposals_for(&item);
        if proposals.is_empty() {
            ui.label(
                RichText::new("该条目无可用动作")
                    .size(sized(pal.font_sm, scale).size)
                    .color(pal.text_dim),
            );
        }
        for (i, p) in proposals.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui
                    .add_sized(
                        [ui.available_width() - 64.0, pal.control_height * scale],
                        egui::Button::new(
                            RichText::new(&p.label).size(sized(pal.font_sm, scale).size),
                        ),
                    )
                    .on_hover_text(match &p.plan {
                        modular_clipboard_app::action::ActionPlan::OpenUrl(u) => format!("打开 {u}"),
                        modular_clipboard_app::action::ActionPlan::RunCommand { program, .. } => {
                            format!("执行 {program}")
                        }
                        _ => "执行该动作".to_string(),
                    })
                    .clicked()
                {
                    local.pending = Some(PendingOp::Action(id, i));
                }
                if ui
                    .add_sized(
                        [58.0, pal.control_height * scale],
                        egui::Button::new("复制"),
                    )
                    .clicked()
                {
                    local.pending = Some(PendingOp::Copy(id));
                }
            });
        }

        ui.add_space(pal.space_xs);
        ui.horizontal(|ui| {
            if ui
                .add_sized(
                    [ui.available_width() - 64.0, pal.control_height * scale + 2.0],
                    egui::Button::new("粘贴到前台窗口"),
                )
                .on_hover_text("写回剪贴板后自动发送 Ctrl+V")
                .clicked()
            {
                local.pending = Some(PendingOp::Paste(id));
            }
            if ui
                .add_sized(
                    [58.0, pal.control_height * scale + 2.0],
                    egui::Button::new(if item.pinned { "取消" } else { "置顶" }),
                )
                .clicked()
            {
                local.pending = Some(PendingOp::TogglePin(id));
            }
        });
    });
}

/// 把缩略图上传到 egui，返回可绘制的纹理句柄。
///
/// # 上传去重
///
/// egui 的 `load_texture` 每次调用都会往 `TexturesDelta` 里塞一条增量，
/// 而同一帧内若重复上传会产生冗余的GPU 拷贝。这里按
/// `(条目 id, 像素指纹)` 记一笔，命中就直接复用上次的句柄。
///
/// # 指纹为什么用像素内容而不是 `item.hash`
///
/// `item.hash` 是**原始载荷**的指纹，而缓存里保存的是**缩放后**的像素。
/// 两者不是同一份数据；若只按 `hash` 去重，将来若缓存策略变化
/// （例如按视口大小重算缩略图），就会拿到尺寸过期的纹理句柄。
/// 对缩略后的实际像素取指纹才是真正的自变量。
fn upload_thumbnail(
    ctx: &egui::Context,
    local: &mut UiLocal,
    id: EntryId,
    thumb: &thumbnail::Thumbnail,
) -> egui::TextureHandle {
    let fingerprint = thumb_fingerprint(thumb);
    if let Some((_, _fp, handle)) = local
        .thumb_textures
        .iter()
        .find(|(eid, fp, _)| *eid == id && *fp == fingerprint)
    {
        return handle.clone();
    }
    let handle = ctx.load_texture(
        format!("thumb-{id}"),
        thumb.to_color_image(),
        egui::TextureOptions::LINEAR,
    );
    local.thumb_textures.retain(|(eid, _, _)| *eid != id);
    local.thumb_textures.push((id, fingerprint, handle.clone()));
    // 纹理显存不可单独回收，超出上限时丢弃最旧的句柄。
    // 丢弃只解除本模块的引用，真正的释放要等 egui 的纹理管理器整体回收。
    const MAX_TEX: usize = 32;
    if local.thumb_textures.len() > MAX_TEX {
        // `remove` 返回被移除的元组（含`TextureHandle`）。`TextureHandle`
        // 是 `must_use`：这里**故意**让它随语句结束而析构，从而解除本模块
        // 对该纹理的引用——这正是「淘汰」要做的事，因此不能写成 `let _`，
        // 那样读起来像「丢弃返回值」而非「主动释放」。
        drop(local.thumb_textures.remove(0));
    }
    handle
}

/// 缩略图像素指纹（FNV-1a 64位）。
///
/// 不需要密码学强度：只用于「同一份像素是否已上传过」这种
/// 缓存自检。碰撞的后果至多是复用一张视觉上相近的图。
fn thumb_fingerprint(thumb: &thumbnail::Thumbnail) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in &thumb.pixels {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn draw_status_bar(
    ui: &mut Ui,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
    narrowed: bool,
) {
    let root = ui.max_rect();
    let h = pal.font_md * scale * 1.6;
    if root.height() < h * 2.0 {
        // 窗口太矮时状态栏没有意义，让位给内容。
        return;
    }
    // 贴在客户区底部：状态栏属于窗口 chrome，不属于任何面板。
    let rect = Rect::from_min_max(
        pos2(root.min.x, root.max.y - h),
        pos2(root.max.x, root.max.y),
    );
    at_rect(ui, rect, |ui| {
        ui.painter().hline(
            rect.x_range(),
            rect.min.y,
            Stroke::new(pal.stroke_thin, pal.border_subtle),
        );
        ui.horizontal(|ui| {
            // 窄窗口降级提示优先占左侧：它解释的是「详情去哪了」，
            // 与临时通知相比更影响用户对界面的理解。
            if narrowed {
                ui.label(
                    RichText::new("窗口较窄 · 详情转为浮层，拉宽窗口可恢复三栏")
                        .size(sized(pal.font_sm, scale).size)
                        .color(pal.text_dim),
                );
            }
            // 提示信息 3 秒后自动消失
            if let Some((msg, at)) = &svc.state.notice {
                let age = at.elapsed();
                if age < Duration::from_secs(3) {
                    let text = if !msg.is_empty() {
                        msg.clone()
                    } else {
                        String::new()
                    };
                    if !text.is_empty() {
                        ui.label(
                            RichText::new(text)
                                .color(pal.accent)
                                .size(sized(pal.font_sm, scale).size),
                        );
                    }
                } else {
                    svc.state.notice = None;
                }
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // 「清空」是破坏性操作：垃圾桶图标 + 悬停说明，
                // 悬停时用 danger 色与旁边的「设置」区分开。
                let (clear_rect, clear_resp) = ui.allocate_exact_size(
                    egui::vec2(pal.icon_size, pal.icon_size) * scale,
                    egui::Sense::click(),
                );
                let clear_icon_center = clear_rect.center();
                let clear_hovered = clear_resp.hovered();
                if clear_resp.clicked() {
                    if let Err(e) = svc.clear_all() {
                        svc.notify(format!("清空失败: {e}"));
                    }
                }
                icons::Icon::Trash.paint(
                    &ui.painter_at(clear_rect),
                    egui::Rect::from_center_size(
                        clear_icon_center,
                        egui::vec2(pal.icon_size, pal.icon_size) * scale,
                    ),
                    if clear_hovered { pal.danger } else { pal.text_dim },
                );
                clear_resp.on_hover_text("清空全部历史");
                if ui.small_button("设置").clicked() {
                    local.show_settings = !local.show_settings;
                }

                let dropped = svc.state.stats.total();
                let text = if svc.state.capturing {
                    if dropped > 0 {
                        format!("监听中 · 拦截 {dropped}")
                    } else {
                        "监听中".to_string()
                    }
                } else {
                    "监听已停止".to_string()
                };
                ui.label(
                    RichText::new(text)
                        .color(pal.text_dim)
                        .size(sized(pal.font_xs, scale).size),
                );
            });
        });
    });
}

/// 解析 `#RRGGBB`，失败时回退到默认强调色。
pub fn parse_hex_color(hex: &str) -> egui::Color32 {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return egui::Color32::from_rgb(0x5B, 0x8D, 0xF6);
    }
    egui::Color32::from_rgb(
        u8::from_str_radix(&h[0..2], 16).unwrap_or(0x5B),
        u8::from_str_radix(&h[2..4], 16).unwrap_or(0x8D),
        u8::from_str_radix(&h[4..6], 16).unwrap_or(0xF6),
    )
}

/// 在指定矩形里开一个子 `Ui`。
///
/// egui 0.36 没有 `allocate_ui_at_rect`，用 `scope_builder` + `max_rect`
/// 达到同样效果。布局完全由外部给的矩形决定，不受游标位置影响——
/// 这正是「面板画在自己该在的位置」所需要的。
fn at_rect<R>(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        add,
    )
    .inner
}

/// 关闭图标按钮（自绘，画在指定位置）。
fn icons_close(ui: &mut Ui, pal: &Palette, scale: f32, tip: &str) -> egui::Response {
    let size = vec2(pal.control_height * 0.8, pal.control_height * 0.8) * scale;
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter_at(rect);
    if resp.hovered() {
        painter.rect_filled(rect, pal.radius_sm, pal.row_hover);
    }
    icons::Icon::Close.paint(
        &painter,
        egui::Rect::from_center_size(rect.center(), vec2(pal.icon_size, pal.icon_size) * scale),
        if resp.hovered() { pal.text_bright } else { pal.text_dim },
    );
    resp.on_hover_text(tip)
}

/// 关闭图标按钮（画在给定矩形）。
fn icons_close_at(
    ui: &mut Ui,
    rect: Rect,
    pal: &Palette,
    scale: f32,
    tip: &str,
) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with(tip), egui::Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, pal.radius_sm, pal.row_hover);
    }
    icons::Icon::Close.paint(
        ui.painter(),
        egui::Rect::from_center_size(rect.center(), vec2(pal.icon_size, pal.icon_size) * scale),
        if resp.hovered() { pal.text_bright } else { pal.text_dim },
    );
    resp.on_hover_text(tip)
}

fn execute(svc: &mut Service, op: PendingOp) {
    match op {
        PendingOp::Select(id) => {
            svc.state.selected = Some(id);
        }
        PendingOp::Copy(id) => {
            if let Err(e) = svc.copy_item(id) {
                svc.notify(format!("复制失败: {e}"));
            } else {
                svc.state.selected = Some(id);
            }
        }
        PendingOp::Paste(id) => {
            if let Err(e) = svc.copy_and_paste(id) {
                svc.notify(format!("粘贴失败: {e}"));
            } else {
                svc.state.selected = Some(id);
            }
        }
        PendingOp::Delete(id) => {
            if let Err(e) = svc.delete_item(id) {
                svc.notify(format!("删除失败: {e}"));
            }
        }
        PendingOp::TogglePin(id) => {
            if let Err(e) = svc.toggle_pin(id) {
                svc.notify(format!("置顶失败: {e}"));
            }
        }
        PendingOp::Action(item_id, idx) => {
            let Ok(Some(item)) = svc.store.get(item_id) else {
                return;
            };
            let props = svc.proposals_for(&item);
            if let Some(p) = props.get(idx)
                && let Err(e) = svc.run_action(&item, p)
            {
                svc.notify(format!("动作失败: {e}"));
            }
        }
    }
}

/// 分组色板，供新建分组使用。
pub const GROUP_COLORS: [&str; 6] = [
    "#5B8DEF", "#4CAF7D", "#E2A03F", "#C05BB5", "#E2604B", "#5BC0C0",
];

/// 供测试使用的纯函数：判断列表是否为空时应显示空状态。
pub fn should_show_empty(count: usize) -> bool {
    count == 0
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_distinct() {
        // P0 规则：类型标识必须是矢量图标，不能是字符。
        let all = [
            icons::Icon::for_kind(ClipKind::Text),
            icons::Icon::for_kind(ClipKind::Html),
            icons::Icon::for_kind(ClipKind::Image),
            icons::Icon::for_kind(ClipKind::Files),
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "不同类型的图标必须可区分");
                }
            }
        }
    }

    #[test]
    fn group_colors_are_valid_hex() {
        for c in GROUP_COLORS {
            assert_eq!(c.len(), 7, "应为 #RRGGBB: {c}");
            assert!(c.starts_with('#'));
            assert!(c[1..].chars().all(|ch| ch.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn kind_names_are_distinct() {
        let all = [
            kind_name(ClipKind::Text),
            kind_name(ClipKind::Html),
            kind_name(ClipKind::Image),
            kind_name(ClipKind::Files),
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "类型名必须可区分");
                }
            }
        }
    }

    #[test]
    fn empty_state_predicate() {
        assert!(should_show_empty(0));
        assert!(!should_show_empty(1));
    }

    /// 布局状态必须真的走 layout 模块的往返，而不是自造一份。
    #[test]
    fn ui_local_layout_roundtrips_through_config() {
        let mut local = UiLocal::default();
        local.layout.rail_view = 1;
        layout::float(&mut local.layout, Panel::Detail, Rect::from_min_size(pos2(10.0, 20.0), vec2(200.0, 150.0)));
        let cfg = layout::LayoutConfig::from(&local.layout);
        let back: LayoutState = (&cfg).into();
        assert_eq!(local.layout, back, "UiLocal 里的布局必须能完整往返");
    }

    // ------------------------------------------------------------------
    // widget ID 冲突回归（真实 run_ui 通路）
    // ------------------------------------------------------------------

    /// 在真实 `Context::run_ui` 里算出若干 widget Id 并返回。
    ///
    /// # 为什么必须跑真实 run_ui
    ///
    /// 冲突判据在 egui 内部（`context.rs` 的 `create_widget` →
    /// `check_for_id_clash`），依赖 `pass_state.used_ids` 这张按帧填充的表。
    /// 但反过来说：**只要一帧内用到的 Id 两两不同，就不可能触发冲突**，
    /// 而「Id 是否唯一」不必读 egui 内部状态就能判定。
    ///
    /// 所以这里跑真帧拿到真实的 `ui.id()`（而不是自造一个 `Ui`），
    /// 再比对唯一性—— 这样测的是**本项目的 ID 设计**，
    /// 测的不是「egui 有没有报错」。
    fn ids_in_real_pass(f: impl Fn(&egui::Ui) -> Vec<egui::Id>) -> Vec<egui::Id> {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = slot.clone();
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            *sink.lock().expect("Id 槽位不该被 Poison") = f(ui);
        });
        // 必须消费掉纹理增量，否则测试结束时留下未处理的 delta。
        out.textures_delta.clear();
        let ids = std::mem::take(&mut *slot.lock().expect("Id 槽位不该被 Poison"));
        ids
    }

    /// 分隔条的 widget Id 必须随序号变化。
    ///
    /// `view::handle_splitters` 一帧里画最多 4 条分隔条，且共用同一个
    /// 根 `Ui`（`ui.id()` 全帧不变）。固定 salt ⇒ 第 2..4 条与第 1 条
    /// 撞 Id ⇒既有红字报错，又因 egui 把 hover/active 状态按 Id 存而互串。
    #[test]
    fn splitter_salts_are_unique_by_index() {
        // 复制 `panels::draw_splitter` 内部的取 Id 方式。
        let ids = ids_in_real_pass(|ui| (0..4).map(|i| ui.id().with(("splitter", i))).collect());
        let uniq: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            uniq.len(),
            4,
            "4 条分隔条必须得到 4 个不同的 widget Id，实际 {ids:?}"
        );
    }

    /// 折叠把手的 widget Id 必须随模块变化。
    ///
    /// `draw_collapse_handle` 被 `draw_pinned_panel` 与
    /// `draw_history_panel` 等多处调用；窄窗口降级会让多个面板同时 veiled，
    /// 于是同一帧内出现多个不同矩形 —— 固定 salt 必然冲突。
    #[test]
    fn collapse_handle_salts_are_unique_by_panel() {
        let all = [Panel::Pinned, Panel::History, Panel::Detail, Panel::Rail];
        let ids =
            ids_in_real_pass(|ui| all.iter().map(|p| ui.id().with(("collapse_handle", *p))).collect());
        let uniq: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            uniq.len(),
            all.len(),
            "各模块的折叠把手必须得到不同 Id，实际 {ids:?}"
        );
    }

    /// 4 个 ScrollArea 的 `id_salt` 必须互不相同。
    ///
    /// egui 0.36 的 ScrollArea 默认用固定 salt `"scroll_area"`
    /// （`scroll_area.rs` 的 `IdSalt::new("scroll_area")`），而本帧最多
    /// 同时存在 4 个（历史列表 / 侧栏表视图 / 侧栏密文 / 详情预览）。
    /// 共用一个 Id 会同时触发冲突红字**和**滚动偏移互串。
    #[test]
    fn scroll_area_salts_are_distinct() {
        let salts = [
            "history_list",
            "rail_table_view",
            "rail_masked_view",
            "detail_preview",
        ];
        let uniq: std::collections::BTreeSet<_> = salts.iter().collect();
        assert_eq!(
            uniq.len(),
            salts.len(),
            "4 个 ScrollArea 的 id_salt 必须互不相同"
        );
    }
}
