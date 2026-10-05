//! 主界面。
//!
//! 布局：顶部搜索栏 + 左侧分组栏+ 右侧虚拟化列表 + 底部状态栏。
//! 选中条目时在右侧显示预览与可执行动作（软件联动入口）。
//!
//! 性能考量：列表用 `ScrollArea::show_rows` 虚拟化，只为可见行构建控件。
//! 2000 条目若全部实例化控件会造成明显卡顿，虚拟化后与条目数基本无关。

use crate::icons;
use std::time::{Duration, Instant};

use egui::{Align, CornerRadius, Layout, RichText, ScrollArea, TextEdit, Ui, Vec2};
use modular_clipboard_app::Service;
use modular_clipboard_core::{ClipKind, EntryId, now_ms};
use modular_clipboard_store::GroupFilter;

use crate::theme::{Palette, sized};

/// 界面状态：不属于业务数据的临时状态（展开项、悬停等）。
#[derive(Default)]
pub struct UiLocal {
    pub show_settings: bool,
    pub settings_tab: usize,
    /// 详情面板是否展开。
    pub show_detail: bool,
    /// 需要在下一帧执行的操作。
    pending: Option<PendingOp>,
}

enum PendingOp {
    /// 单击行：选中并展开详情（不写剪贴板，避免误覆盖）
    Select(EntryId),
    Copy(EntryId),
    Paste(EntryId),
    Delete(EntryId),
    Action(EntryId, usize),
}

const ROW_HEIGHT: f32 = 46.0;

/// 主视图。返回 `true` 表示请求退出应用。
///
/// 由 `Context::run_ui` 提供的根 `Ui` 覆盖整个客户区，本函数直接往里画，
/// 不再自行创建 `Window`，也无需处理视口命令。
pub fn draw(ui: &mut Ui, svc: &mut Service, local: &mut UiLocal) -> bool {
    let dark = match svc.state.config.ui.dark_mode {
        Some(v) => v,
        None => ui.ctx().style_of(egui::Theme::Dark).visuals.dark_mode,
    };
    let pal = if dark { Palette::dark() } else { Palette::light() };
    let scale = svc.state.config.ui.font_scale;

    // 先处理上一帧排队的操作，避免在渲染过程中修改数据。
    if let Some(op) = local.pending.take() {
        execute(svc, op);
    }

    let mut quit = false;
    draw_body(ui, svc, local, &pal, scale, &mut quit);
    quit
}

fn draw_body(
    ui: &mut Ui,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
    quit: &mut bool,
) {
    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);

    draw_search_bar(ui, svc, pal, scale);

    ui.add_space(4.0);
    let avail = ui.available_size();

    // 详情面板展开时，主列表占左侧 62%。
    let detail_w = if local.show_detail { avail.x * 0.38 } else { 0.0 };
    let list_w = avail.x - detail_w;

    ui.horizontal(|ui| {
        // 左侧：分组栏 + 列表
        let list_area = ui.vertical(|ui| {
            ui.set_width(list_w);
            draw_groups(ui, svc, pal, scale);
            draw_list(ui, svc, local, pal, scale);
        });

        // 右侧：详情
        if detail_w > 0.0 {
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.set_width(detail_w);
                draw_detail(ui, svc, local, pal, scale);
            });
        }
        let _ = list_area;
    });

    ui.add_space(4.0);
    draw_status_bar(ui, svc, pal, scale, quit);
}

fn draw_search_bar(ui: &mut Ui, svc: &mut Service, pal: &Palette, scale: f32) {
    ui.horizontal(|ui| {
        let mut q = svc.state.query.clone();
        let resp = ui.add(
            TextEdit::singleline(&mut q)
                .hint_text("搜索历史…")
                .desired_width(f32::INFINITY)
                .font(sized(14.0, scale)),
        );
        if resp.changed() {
            svc.search(q);
        }
        // Ctrl+F 聚焦搜索框
        if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::F)) {
            resp.request_focus();
        }

        if !svc.state.query.is_empty()
            && icons::icon_button(ui, icons::Icon::Close).clicked()
        {
            svc.search(String::new());
        }
    });
    let _ = pal;
}

fn draw_groups(ui: &mut Ui, svc: &mut Service, pal: &Palette, scale: f32) {
    ui.horizontal_wrapped(|ui| {
        // 全部
        let all_selected = svc.state.group_filter.is_none();
        if ui
            .selectable_label(all_selected, "全部")
            .clicked()
        {
            svc.filter_group(None);
        }

        let groups = svc.state.groups.clone();
        for g in &groups {
            let selected = matches!(svc.state.group_filter, Some(GroupFilter::Group(id)) if id == g.id);
            let color = parse_hex_color(&g.color);
            let normal = ui.visuals().weak_text_color();
            let text = RichText::new(&g.name).color(if selected { color } else { normal });
            if ui.selectable_label(selected, text).clicked() {
                let id = g.id;
                svc.filter_group(Some(GroupFilter::Group(id)));
            }
        }

        if ui.small_button("+").on_hover_text("新建分组").clicked() {
            local_new_group(svc);
        }
    });
    let _ = (pal, scale);
}

fn local_new_group(svc: &mut Service) {
    let name = format!("分组 {}", svc.state.groups.len() + 1);
    if let Err(e) = svc.create_group(&name) {
        svc.notify(format!("创建分组失败: {e}"));
    }
}

fn draw_list(
    ui: &mut Ui,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
) {
    let count = svc.state.items.len();

    if count == 0 {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(if svc.state.query.is_empty() {
                    "还没有剪贴板记录"
                } else {
                    "没有匹配的结果"
                })
                .color(pal.text_dim)
                .size(sized(13.0, scale).size),
            );
        });
        return;
    }

    ScrollArea::vertical()
        .max_height(ui.available_height() - 24.0)
        .auto_shrink([false, false])
        // show_rows 只为可见行回调 row_range，这是列表能做到万级不卡的关键。
        .show_rows(ui, ROW_HEIGHT, count, |ui, rows| {
            for row in rows {
                let item = &svc.state.items[row];
                let selected = svc.state.selected == Some(item.id);
                let hovered = ui.rect_contains_pointer(ui.max_rect());

                // 交替底色提升长列表可读性
                if row % 2 == 1 {
                    ui.painter().rect_filled(
                        ui.max_rect(),
                        CornerRadius::ZERO,
                        pal.row_alt,
                    );
                }
                if selected {
                    ui.painter().rect_filled(
                        ui.max_rect(),
                        CornerRadius::ZERO,
                        pal.accent.gamma_multiply(0.25),
                    );
                } else if hovered {
                    ui.painter().rect_filled(
                        ui.max_rect(),
                        CornerRadius::ZERO,
                        pal.row_hover,
                    );
                }

                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.label(kind_icon(item.kind));
                    ui.vertical(|ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(item.one_line_preview())
                                    .size(sized(13.0, scale).size),
                            )
                            .truncate(),
                        );
                        let meta = format!(
                            "{} · {}",
                            modular_clipboard_app::format_time(item.created_at, now_ms()),
                            if item.source_app.is_empty() {
                                "未知来源".to_string()
                            } else {
                                item.source_app.clone()
                            }
                        );
                        ui.label(
                            RichText::new(meta)
                                .size(sized(11.0, scale).size)
                                .color(pal.text_dim),
                        );
                    });
                    if item.pinned {
                        icons::paint_inline(ui, icons::Icon::Pinned, pal.accent);
                    }
                });

                // 整行作为点击区域：allocate_response 而非 allocate_response，
                // 因为行内已放置子控件，需用独立 id 避免抢占子控件的交互。
                let row_resp = ui.allocate_response(
                    ui.max_rect().size(),
                    egui::Sense::click(),
                );
                if hovered {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if row_resp.clicked() {
                    local.pending = Some(PendingOp::Select(item.id));
                }
                if row_resp.clicked_by(egui::PointerButton::Secondary) {
                    local.pending = Some(PendingOp::Delete(item.id));
                }
            }
        });
}

fn draw_detail(
    ui: &mut Ui,
    svc: &mut Service,
    local: &mut UiLocal,
    pal: &Palette,
    scale: f32,
) {
    let Some(id) = svc.state.selected else {
        ui.vertical_centered(|ui| {
            ui.add_space(30.0);
            ui.label(
                RichText::new("点击条目查看详情")
                    .color(pal.text_dim)
                    .size(sized(12.0, scale).size),
            );
        });
        return;
    };
    let Some(item) = svc.state.items.iter().find(|i| i.id == id).cloned() else {
        return;
    };

    ui.horizontal(|ui| {
        ui.label(kind_icon(item.kind));
        ui.label(
            RichText::new(format!("{:?}", item.kind))
                .size(sized(12.0, scale).size)
                .color(pal.text_dim),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icons::icon_button(ui, icons::Icon::Close).clicked() {
                local.show_detail = false;
                svc.state.selected = None;
            }
        });
    });

    ui.separator();

    // 预览区：按类型分派
    egui::ScrollArea::vertical()
        .max_height(ui.available_height() - 140.0)
        .show(ui, |ui| {
            ui.add_space(4.0);
            match item.kind {
                ClipKind::Text | ClipKind::Html => {
                    ui.add(
                        TextEdit::multiline(&mut item.preview.clone())
                            .desired_width(f32::INFINITY)
                            .desired_rows(10)
                            .font(sized(13.0, scale))
                            .interactive(false),
                    );
                }
                ClipKind::Files => {
                    for line in item.preview.lines() {
                        ui.label(RichText::new(line).size(sized(12.0, scale).size));
                    }
                }
                ClipKind::Image => {
                    ui.label(
                        RichText::new(&item.preview)
                            .color(pal.text_dim)
                            .size(sized(12.0, scale).size),
                    );
                }
            }
        });

    ui.separator();

    // 动作区：软件联动入口
    ui.label(
        RichText::new("可用动作")
            .size(sized(12.0, scale).size)
            .color(pal.text_dim),
    );
    let proposals = svc.proposals_for(&item);
    if proposals.is_empty() {
        ui.label(
            RichText::new("该条目无可用动作")
                .size(sized(12.0, scale).size)
                .color(pal.text_dim),
        );
    }
    for (i, p) in proposals.iter().enumerate() {
        ui.horizontal(|ui| {
            if ui
                .add_sized(
                    [ui.available_width() - 64.0, 26.0],
                    egui::Button::new(
                        RichText::new(&p.label).size(sized(12.0, scale).size),
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
                .add_sized([58.0, 26.0], egui::Button::new("复制"))
                .clicked()
            {
                local.pending = Some(PendingOp::Copy(id));
            }
        });
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui
            .add_sized([ui.available_width() - 64.0, 30.0], egui::Button::new("粘贴到前台窗口"))
            .on_hover_text("写回剪贴板后自动发送 Ctrl+V")
            .clicked()
        {
            local.pending = Some(PendingOp::Paste(id));
        }
        if ui
            .add_sized([58.0, 30.0], egui::Button::new(if item.pinned { "取消" } else { "置顶" }))
            .clicked()
        {
            if let Err(e) = svc.toggle_pin(id) {
                svc.notify(format!("置顶失败: {e}"));
            }
        }
    });
}

fn draw_status_bar(
    ui: &mut Ui,
    svc: &mut Service,
    pal: &Palette,
    scale: f32,
    quit: &mut bool,
) {
    ui.separator();
    ui.horizontal(|ui| {
        // 提示信息 3 秒后自动消失
        if let Some((msg, at)) = &svc.state.notice {
            let age = at.elapsed();
            let text = if age < Duration::from_secs(3) {
                msg.clone()
            } else {
                svc.state.notice = None;
                String::new()
            };
            if !text.is_empty() {
                ui.label(RichText::new(text).color(pal.accent).size(sized(12.0, scale).size));
            }
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("清空").on_hover_text("清空全部历史").clicked() {
                if let Err(e) = svc.clear_all() {
                    svc.notify(format!("清空失败: {e}"));
                }
            }
            if ui.small_button("设置").clicked() {
                svc.state.notice = Some(("设置面板".into(), Instant::now()));
            }
            if ui.small_button("退出").clicked() {
                *quit = true;
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
            ui.label(RichText::new(text).color(pal.text_dim).size(sized(11.0, scale).size));
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

fn kind_icon(kind: ClipKind) -> &'static str {
    match kind {
        ClipKind::Text => "T",
        ClipKind::Html => "H",
        ClipKind::Image => "I",
        ClipKind::Files => "F",
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_distinct() {
        let all = [kind_icon(ClipKind::Text), kind_icon(ClipKind::Html), kind_icon(ClipKind::Image), kind_icon(ClipKind::Files)];
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), 4, "各类型应有可区分的标识");
    }

    #[test]
    fn group_colors_are_valid_hex() {
        for c in GROUP_COLORS {
            assert_eq!(c.len(), 7, "应为 #RRGGBB: {c}");
            assert!(c.starts_with('#'));
            assert!(c[1..].chars().all(|ch| ch.is_ascii_hexdigit()));
        }
    }
}
