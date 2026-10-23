//! 主题微调：强调色编辑器与二级主题令牌面板。
//!
//! 强调色三条路径（预设色板 / HSV 滑条 / 十六进制输入）；
//! 令牌面板以二级菜单呈现，占设置窗口主要区域。

use egui::Align2;
use egui::Rect;
use egui::Sense;
use egui::Ui;
use egui::pos2;
use egui::vec2;
use modular_clipboard_core::i18n::t;
use crate::theme::Palette;
use crate::theme::TokenValue;

use super::{Frame, Op};

use super::settings_items::shadow_param_row;
use super::controls::draw_mini_button;
use super::controls::draw_slider;

/// 主题强调色编辑器（「主题色」设置项）。
///
/// 三种调节方式（对应用户要求的「多中自定义调节颜色方式」）：
/// 1. **预设色板**：10 个色块，单击即用——最常见的路径；
/// 2. **HSV 滑条**：色相 / 饱和度 / 明度各一条——精细调节；
/// 3. **十六进制输入**：`#RRGGBB`——从设计稿抄色。
///
/// 返回 `true` 表示本帧有改动（调用方 `record_recent`）。
pub(super) fn draw_accent_editor(f: &mut Frame<'_>, ui: &mut Ui) -> bool {
    use modular_clipboard_core::config::AccentColor;
    let pal = f.pal;
    let cur = f.svc.state.config.ui.accent;
    let shown = cur.unwrap_or_else(AccentColor::default_blue);
    let mut next = cur;
    let mut changed = false;

    // ---- 预览行：当前色块 + 来源 + hex ----
    let rgb = shown.to_rgb();
    ui.horizontal(|ui| {
        // 高度随字号（font_sm×1.7 @scale1≈26）：本行是 horizontal，
        // 色块先放置、来源标注后放置——egui 不重排先放的控件，固定高
        // 在 font_scale>1 时被文字 galley 撑行后色块会顶格偏高（与令牌
        // 行同款病，见 token_row_h 的 CAUTION）。
        let swatch_h = (pal.font_sm * 1.7).max(22.0);
        let (r, _) = ui.allocate_exact_size(vec2(48.0, swatch_h), Sense::empty());
        ui.painter_at(r)
            .rect_filled(r, pal.radius_sm, egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
        ui.add_space(8.0);
        // 来源标注：预设 / 自定义 / 默认。比只显示颜色多一分确定性
        // （用户看得知道自己用的是哪一个预设）。
        let source = if cur.is_none() {
            t("Default")
        } else if crate::theme::ACCENT_PRESETS
            .iter()
            .any(|(_, p)| AccentColor::from_rgb(*p).to_rgb() == rgb)
        {
            t("Preset")
        } else {
            t("Custom")
        };
        ui.add(egui::Label::new(
            egui::RichText::new(format!("{source}  {}", shown.to_hex())).color(pal.text_dim),
        ));
    });

    // ---- 预设色板 ----
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(1.0);
        for (i, (name, prgb)) in crate::theme::ACCENT_PRESETS.iter().enumerate() {
        let name = t(name);
            let s = 20.0;
            let (r, _) = ui.allocate_exact_size(vec2(s, s), Sense::empty());
            let resp = ui.interact(r, egui::Id::unique(("accent_preset", i)), Sense::click());
            let selected = cur.is_some_and(|c| c.to_rgb() == *prgb);
            let p = ui.painter_at(r);
            p.rect_filled(
                r,
                pal.radius_sm,
                egui::Color32::from_rgb(prgb[0], prgb[1], prgb[2]),
            );
            if selected {
                // 选中圈：text_bright 描边（任何底色上都看得见）。
                p.rect_stroke(
                    r,
                    pal.radius_sm,
                    egui::Stroke::new(2.0, pal.text_bright),
                    egui::StrokeKind::Inside,
                );
            } else if resp.hovered() {
                p.rect_stroke(
                    r,
                    pal.radius_sm,
                    egui::Stroke::new(1.0, pal.text_dim),
                    egui::StrokeKind::Inside,
                );
            }
            if resp.clicked() {
                next = Some(AccentColor::from_rgb(*prgb));
                changed = true;
            }
            // 热区键 = 预设名（全局唯一，同分段控件的约定）。
            f.hit_insert(name, r);
            ui.add_space(5.0);
        }
    });

    // ---- HSV 滑条 ----
    ui.add_space(4.0);
    let mut hsv = shown;
    let mut h = hsv.h;
    if shadow_param_row(
        f,
        ui,
        "accent_hue",
        t("Hue"),
        format!("{:.0}°", h),
        &mut h,
        0.0..=360.0,
    ) {
        hsv.h = h;
        next = Some(hsv);
        changed = true;
    }
    let mut s = hsv.s;
    if shadow_param_row(
        f,
        ui,
        "accent_sat",
        t("HSV saturation"),
        format!("{}%", (s * 100.0).round() as i32),
        &mut s,
        0.0..=1.0,
    ) {
        hsv.s = s;
        next = Some(hsv);
        changed = true;
    }
    let mut v = hsv.v;
    if shadow_param_row(
        f,
        ui,
        "accent_val",
        t("Lightness"),
        format!("{}%", (v * 100.0).round() as i32),
        &mut v,
        0.0..=1.0,
    ) {
        hsv.v = v;
        next = Some(hsv);
        changed = true;
    }

    // ---- 十六进制输入 + 恢复默认 ----
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(t("Hex")));
        ui.add_space(8.0);
        // 草稿同步的焦点感知：无焦点时每帧覆写为当前色（点预设 / 拖滑条
        // 后输入框跟着变），有焦点时保留用户输入（ rationale 见
        // `UiState::accent_hex`）。非法半截输入不清除已选颜色。
        let editing = ui.ctx().memory(|m| m.has_focus(egui::Id::unique("accent_hex_edit")));
        if !editing {
            f.state.accent_hex = shown.to_hex();
        }
        let w = pal.font_sm * 7.3;
        let (r, _) = ui.allocate_exact_size(
            vec2(w, pal.control_height + 2.0),
            egui::Sense::empty(),
        );
        let painter = ui.painter_at(r);
        painter.rect_filled(r, pal.radius_md, pal.surface_variant);
        painter.rect_stroke(
            r,
            pal.radius_md,
            egui::Stroke::new(1.0, pal.border),
            egui::StrokeKind::Inside,
        );
        let text_rect = r.shrink2(vec2(8.0, 4.0));
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(text_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let resp = child.add(
            egui::TextEdit::singleline(&mut f.state.accent_hex)
                .id(egui::Id::unique("accent_hex_edit"))
                .hint_text("#RRGGBB")
                .frame(egui::Frame::NONE)
                .text_color(pal.text)
                .desired_width(text_rect.width()),
        );
        if resp.has_focus() {
            // 聚焦描边补间（130ms）：与设置页其它输入框同一套。
            let focus_k = crate::anim::tween_f32(
                ui.ctx(),
                egui::Id::unique("accent_hex_focus"),
                1.0,
                std::time::Duration::from_millis(130),
                crate::anim::Ease::OutCubic,
            );
            let ring = crate::anim::tween_color(
                ui.ctx(),
                egui::Id::unique("accent_hex_focus_color"),
                pal.accent,
                std::time::Duration::from_millis(130),
            );
            painter.rect_stroke(
                r,
                pal.radius_md,
                egui::Stroke::new(1.0 + 0.5 * focus_k, ring),
                egui::StrokeKind::Inside,
            );
        }
        if resp.changed()
            && let Some(c) = AccentColor::from_hex(&f.state.accent_hex)
        {
            next = Some(c);
            changed = true;
        }
        // 恢复默认：仅在用户改过时出现（默认态放个按钮是噪声）。
        if cur.is_some() {
            ui.add_space(8.0);
            if draw_mini_button(f, ui, "accent_reset_default", t("Restore default"), Some("accent_reset_default")) {
                next = None;
                changed = true;
            }
        }
    });

    if changed {
        f.state.push(Op::SetAccent(next));
    }
    changed
}

/// 数值令牌的滑条量程（按令牌名推断）。
///
/// 推断而不是另维护一张表：新增令牌时忘记登记量程的后果只是「量程
/// 不合理」（滑起来别别扭扭），而忘记登记**令牌本身**的后果是
/// 「这个令牌在编辑器里根本不出现」——前者良性得多。
pub(super) fn token_num_range(name: &str) -> std::ops::RangeInclusive<f32> {
    if name.starts_with("font_") {
        8.0..=48.0
    } else if name.starts_with("radius_") || name.starts_with("space_") {
        0.0..=40.0
    } else if name.starts_with("stroke_") {
        0.0..=6.0
    } else if name == "control_height" {
        16.0..=64.0
    } else if name == "icon_size" {
        8.0..=48.0
    } else {
        0.0..=64.0
    }
}

/// 「高级微调」入口行（主题卡片内）。
///
/// 2026-10-19 起令牌编辑器从「卡片内联长列表」升级为**二级面板**：
/// 点击本行把设置窗口右侧内容区整体切换为令牌面板（占一大半区域），
/// 近 60 行令牌终于有地方完整铺开。 rationale：
/// - 内联在主题卡里时，55 行把主题卡撑到五屏，滚半天才能滚完，
///   与「外观/常规」等卡片的信息密度完全失衡；
/// - 折叠态只有本行一个摘要，主题卡回到一屏以内。
///
/// 返回 true 表示本帧产生了 Op（调用方记「最近修改」）。
pub(super) fn draw_token_entry(f: &mut Frame<'_>, ui: &mut Ui) -> bool {
    let pal = f.pal;
    let n_over = f.svc.state.config.ui.theme_overrides.colors.len()
        + f.svc.state.config.ui.theme_overrides.nums.len();

    // 入口行本身做成一个可点条目：标签 + 摘要 + 「调整」。
    let label = if n_over > 0 {
        t("Token overrides · {n} overridden")
            .replace("{n}", &n_over.to_string())
    } else {
        t("Token overrides").to_string()
    };
    let (r, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), pal.control_height),
        egui::Sense::click(),
    );
    let p = ui.painter_at(r);
    if resp.hovered() {
        p.rect_filled(r, pal.radius_sm, pal.row_hover);
    }
    p.text(
        egui::pos2(r.min.x + 4.0, r.center().y),
        Align2::LEFT_CENTER,
        &label,
        egui::FontId::proportional(pal.font_sm),
        pal.text,
    );
    p.text(
        egui::pos2(r.max.x - 4.0, r.center().y),
        Align2::RIGHT_CENTER,
        "调整 ›",
        egui::FontId::proportional(pal.font_sm),
        pal.accent,
    );
    f.hit_insert("token_expand", r);
    if resp.clicked() {
        f.state.theme_tokens_expanded = true;
    }
    false
}

/// 令牌行的统一行高（逻辑点）。
///
/// # 为什么不能写死 18pt（2026-10-19 用户实机反馈「文字向上且渲染
/// 不完整」的根因）
///
/// 令牌行是 horizontal 三列：左列（118pt 文字格）、滑条、数值。
/// egui 的 horizontal **不重排先放置的控件**——行高被后放的高控件
/// 撑大时，先放的左列仍顶部对齐 ⇒ 文字相对滑条偏高。
///
/// 150% DPI（ppp=1.5）下两件事同时发生：
/// 1. 滑条 cell 高 = font_sm×1.07 = 22.5×1.07 ≈ 24pt > 18 ⇒ 行高被
///    撑到 24，左列（18pt）顶格 ⇒ 文字比滑条 knob 中心高约 3pt；
/// 2. 令牌名字号 font_xs = 14×1.5 = 21pt，galley 行高 ≈ 21×1.28
///    ≈ 27pt > 18pt 的格子 ⇒ 字形上下各被裁约 4.5pt（视觉上
///    「上半还在、下半不见了」）。
///
/// 测试 harness 的 ppp=1.0 恰好复现不了这两个数（21→14、24→16），
/// 所以单测全绿而实机糊——**依赖字号的布局必须按字号算，不能拍
/// 常数**。本函数取三者的最大值：左列字形（font_xs×1.45，含 galley
/// 行距）、滑条 cell（font_sm×1.07）、保底 18pt。
pub(super) fn token_row_h(pal: &Palette) -> f32 {
    (pal.font_xs * 1.45).max(pal.font_sm * 1.07).max(18.0)
}

/// 令牌级覆盖**二级面板**（主题分类的二级视图）。
///
/// 列出**全部**主题令牌（[`Palette::tokens`]），逐项给出当前生效值
/// 与调节入口——「界面上任何元素、任何参数都允许自由调整」的最终
/// 出口（二级主题的旋钮覆盖常见需求，这里覆盖其余一切）。
///
/// - 颜色令牌：色块 + 色相滑条；**半透明**令牌追加透明度滑条
///   （不透明令牌的 alpha 滑条没有意义——拖了没有任何视觉效果）；
/// - 数值令牌：滑条（量程按名字推断，见 [`token_num_range`]）；
/// - 时长令牌：只读（动效时长不适合滑条连续拖，且极少有人调）。
///
/// 已覆盖的令牌行尾有「重置」按钮，恢复解析链派生值。
///
/// # 分组标题
///
/// 55 个令牌平铺是一面墙。按类型分三组（颜色 / 数值 / 时长）并给组
/// 标题，用户能靠分组定位而不是逐行扫描。组标题不注册热区。
pub(super) fn draw_token_panel(f: &mut Frame<'_>, ui: &mut Ui) {
    let pal = f.pal;

    // ---- 顶部：返回 + 标题 ----
    ui.horizontal(|ui| {
        // 返回：二级视图的唯一出口。做成图标按钮 + 文字。
        let (r, resp) = ui.allocate_exact_size(
            egui::vec2(pal.control_height, pal.control_height),
            egui::Sense::click(),
        );
        let p = ui.painter_at(r);
        if resp.hovered() {
            p.rect_filled(r, pal.radius_sm, pal.row_hover);
        }
        p.text(
            r.center(),
            Align2::CENTER_CENTER,
            "‹",
            egui::FontId::proportional(pal.font_lg),
            if resp.hovered() { pal.text_bright } else { pal.text_dim },
        );
        f.hit_insert("token_back", r);
        if resp.clicked() {
            f.state.theme_tokens_expanded = false;
        }
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                egui::RichText::new(t("Token overrides")).color(pal.text).size(pal.font_lg),
            ),
        );
    });
    ui.add_space(2.0);
    ui.add(
        egui::Label::new(
            egui::RichText::new(
                t("Effective values of all theme tokens. Dragging writes an override, which wins over values derived from the style theme and accent color."),
            )
            .color(pal.text_dim),
        )
        .wrap(),
    );
    ui.add_space(6.0);

    let n_over = f.svc.state.config.ui.theme_overrides.colors.len()
        + f.svc.state.config.ui.theme_overrides.nums.len();
    // 摘要 + 全部重置（仅在有覆盖时出现——默认态放个按钮是噪声）。
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(
            egui::RichText::new(t("{n} overridden")
                .replace("{n}", &n_over.to_string())).color(pal.text_dim),
        ));
        if n_over > 0 {
            ui.add_space(8.0);
            let reset_all =
                draw_mini_button(f, ui, "token_reset_all", t("Reset all tokens"), Some("token_reset_all"));
            if reset_all {
                f.state.push(Op::ClearAllTokenOverrides);
            }
        }
    });
    ui.add_space(6.0);

    // ---- 逐令牌（按类型分组）----
    //
    // CAUTION: ops 先收集后统一 push——`f.state.push` 要 `&mut Frame`，
    // 而循环里还要读 `f.svc.state.config`（是否已覆盖）。边读边写在
    // Rust 里是借用冲突，收集式则天然顺序执行。
    let row_h = token_row_h(pal);
    let mut ops: Vec<Op> = Vec::new();
    let mut group = "";
    for (name, value) in pal.tokens() {
        // 组标题：只在分组变化时画一次。
        let g = match value {
            TokenValue::Color(_) => t("Colors"),
            TokenValue::Num(_) => t("Numbers"),
            TokenValue::Dur(_) => t("Durations (read-only)"),
        };
        if g != group {
            group = g;
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(egui::RichText::new(g).color(pal.text_dim).size(pal.font_xs)),
            );
        }
        match value {
            TokenValue::Color(c) => {
                let overridden = f.svc.state.config.ui.theme_overrides.colors.contains_key(name);
                let [sr, sg, sb, sa] = c.to_srgba_unmultiplied();
                let hsv = modular_clipboard_core::config::AccentColor::from_rgb([sr, sg, sb]);
                let mut h = hsv.h;
                ui.horizontal(|ui| {
                    // 左列：色块 + 令牌名（等宽小字，令牌名是给「知道自己在
                    // 找什么」的用户看的，不需要好看）。
                    let (lr, _) = ui.allocate_exact_size(vec2(118.0, row_h), Sense::empty());
                    let p = ui.painter_at(lr);
                    let sw = Rect::from_min_size(lr.min, vec2(14.0, 14.0));
                    p.rect_filled(sw, pal.radius_sm, c);
                    p.text(
                        pos2(sw.max.x + 6.0, lr.center().y),
                        Align2::LEFT_CENTER,
                        name,
                        egui::FontId::proportional(pal.font_xs),
                        if overridden { pal.text } else { pal.text_dim },
                    );
                    // 右列：色相滑条（+ 半透明令牌的透明度滑条）。
                    if draw_slider(
                        f,
                        ui,
                        name,
                        "hue",
                        &mut h,
                        0.0..=360.0,
                        true,
                        row_h,
                    ) {
                        let nc = modular_clipboard_core::config::AccentColor::new(h, hsv.s, hsv.v);
                        let rgb2 = nc.to_rgb();
                        ops.push(Op::SetTokenOverride {
                            name: name.to_string(),
                            value: TokenValue::Color(egui::Color32::from_rgba_unmultiplied(
                                rgb2[0], rgb2[1], rgb2[2], sa,
                            )),
                        });
                    }
                    if sa < 255 {
                        // 半透明令牌：第二行放透明度滑条（与色相滑条共用
                        // 令牌名，靠 variant 区分 Id，见 draw_slider 的
                        // CAUTION）。
                        let mut a = f32::from(sa);
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            let (lr2, _) =
                                ui.allocate_exact_size(vec2(118.0, row_h), Sense::empty());
                            ui.painter_at(lr2).text(
                                pos2(lr2.min.x + 20.0, lr2.center().y),
                                Align2::LEFT_CENTER,
                                t("└ Opacity"),
                                egui::FontId::proportional(pal.font_xs),
                                pal.text_dim,
                            );
                            if draw_slider(f, ui, name, "alpha", &mut a, 0.0..=255.0, true, row_h) {
                                let rgb2 = modular_clipboard_core::config::AccentColor::new(
                                    hsv.h, hsv.s, hsv.v,
                                )
                                .to_rgb();
                                ops.push(Op::SetTokenOverride {
                                    name: name.to_string(),
                                    value: TokenValue::Color(
                                        egui::Color32::from_rgba_unmultiplied(
                                            rgb2[0], rgb2[1], rgb2[2],
                                            a.round() as u8,
                                        ),
                                    ),
                                });
                            }
                        });
                    }
                    // 重置按钮（仅已覆盖的令牌）。
                    if overridden {
                        ui.add_space(6.0);
                        // hit=None：重置按钮与色相滑条共用令牌名，登记会
                        // 覆盖滑条的热区（见 draw_mini_button 的 `hit`）。
                        if draw_mini_button(f, ui, name, "重置", None) {
                            ops.push(Op::ClearTokenOverride(name.to_string()));
                        }
                    }
                    // CAUTION: 这里**不**再登记左列——`draw_slider` 已用
                    // 同一个 key 登记了滑条矩形，重复登记会把它覆盖掉，
                    // 自动化就拖不到色相滑条了。
                });
            }
            TokenValue::Num(n) => {
                let overridden = f.svc.state.config.ui.theme_overrides.nums.contains_key(name);
                let mut v = n;
                let range = token_num_range(name);
                ui.horizontal(|ui| {
                    let (lr, _) = ui.allocate_exact_size(vec2(118.0, row_h), Sense::empty());
                    ui.painter_at(lr).text(
                        lr.min,
                        Align2::LEFT_CENTER,
                        name,
                        egui::FontId::proportional(pal.font_xs),
                        if overridden { pal.text } else { pal.text_dim },
                    );
                    let c = draw_slider(f, ui, name, "num", &mut v, range.clone(), true, row_h);
                    ui.add_space(4.0);
                    // 显式给行高：Label 默认字号（font_md）在高 DPI 下可能
                    // 超过 row_h，把 horizontal 行高撑开后又把先放置的左列
                    // 顶上去（同一个「不重排」病）。
                    ui.add_sized(
                        egui::vec2(30.0, row_h),
                        egui::Label::new(
                            egui::RichText::new(format!("{:.0}", v)).color(pal.text_dim),
                        ),
                    );
                    if c {
                        ops.push(Op::SetTokenOverride {
                            name: name.to_string(),
                            value: TokenValue::Num(v),
                        });
                    }
                    if overridden {
                        ui.add_space(6.0);
                        if draw_mini_button(f, ui, name, "重置", None) {
                            ops.push(Op::ClearTokenOverride(name.to_string()));
                        }
                    }
                    // 同颜色行：左列不登记，滑条的热区（draw_slider 内）
                    // 保留给自动化。
                });
            }
            // 时长令牌只读：动效时长不适合滑条连续拖（90ms 与 91ms 没有
            // 任何可感差别），且调整它的用户极少——列出来只是噪声。
            TokenValue::Dur(_) => {}
        }
    }
    for op in ops {
        f.state.push(op);
    }
}
