//! MCP 覆盖层与热键录制覆盖层。
//!
//! 两者都是设置窗口内的全屏覆盖层：之前的图元模糊、之后的清晰，
//! 分割点由 `push_blur_split_marker` 打标记（见 `settings_win`）。

use egui::Rect;
use egui::Stroke;
use egui::Ui;
use egui::pos2;
use egui::vec2;
use modular_clipboard_core::i18n::t;

use super::{Frame, Op, ConfigGroup};

use super::topbar::overlay_scrim;
use super::settings_items::MCP_CAPS;
use super::settings_items::McpCap;
use super::settings_items::get_mcp_cap;
use super::settings_rows::record_recent;
use super::settings_rows::setting_row;
use super::controls::draw_switch;
use super::shadow::fade_color;

pub(super) fn draw_mcp_info_btn(f: &mut Frame<'_>, ui: &mut Ui) -> egui::Response {
    let pal = f.pal;
    // 直径随字号缩放（与开关同量级）。
    let d = pal.font_sm * 1.47;
    let (r, resp) = ui.allocate_exact_size(egui::vec2(d, d), egui::Sense::click());
    let p = ui.painter_at(r);
    // 未 hover：surface_variant 实底（与卡片 surface 拉开一层）；
    // hover：accent 实底。避免用半透明 row_selected 叠在卡片上显脏。
    let bg = if resp.hovered() {
        pal.accent
    } else {
        pal.surface_variant
    };
    p.circle_filled(r.center(), 11.0, bg);
    let ic = if resp.hovered() {
        pal.text_on_accent
    } else {
        pal.accent
    };
    // 信息图标：点 + 竖线。
    p.circle_filled(r.center() + vec2(0.0, -2.5), 1.3, ic);
    p.line_segment(
        [r.center() + vec2(0.0, 1.0), r.center() + vec2(0.0, 5.0)],
        Stroke::new(1.6, ic),
    );
    f.hit_insert("settings_mcp_info", r);
    resp
}

/// 热键录制覆盖层。
///
/// 结构（用户定稿，2026-10-13）：**整窗只画它**——设置内容作为 GPU
/// 模糊的底（`draw_settings` 在调用前放分割标记），本面板保持清晰。
/// 用户看不到设置界面，只看到居中的大圆角正方形：
///
/// ```text
/// ┌──────── 圆角正方形（第一个，外层面板）────────┐
/// │              正在录制快捷键                  │
/// │   ┌Ctrl┐ ┌Shift┐ ┌Alt┐ ┌Win┐   ← 修饰键键帽 │
/// │        ┌──────────┐                    │
/// │        │    K     │                    │  ← 主键键帽（第二个）
/// │        └──────────┘                    │
/// │     录制完成后，点击任意位置停止录制        │
/// └──────────────────────────────────────────┘
/// ```
///
/// 绘制顺序即「先圆角正方形 + 圆角正方形」：先遮罩 + 外层面板，
/// 再内层键帽（修饰键四个 + 主键一个），全部 `painter` 直绘——
/// 不注册任何子交互区，点击一律由全屏 `interact` 收走。
///
/// # 交互模型
///
/// 按下的组合**只显示不应用**；点击任意位置 = 确认并停止
/// （用户要求「点击任意位置停止录制」）。没按过主键就点击 = 取消。
/// Esc 也可取消（在 `App::capture_recording` 里处理）。
pub(super) fn draw_recording_overlay(f: &mut Frame<'_>, area: Rect) {
    let pal = f.pal;

    // 入场动效：遮罩淡入 + 面板下沉 8pt（200ms OutCubic）。
    //
    // 录制态下用户会不停按键 ⇒ 事件驱动的帧不会停，动画天然能播完，
    // 不需要额外续帧请求。id 由调用方在录制结束时清除（见 `draw_settings`
    // 的 else 分支），每次开始录制都重播一遍。
    let k = crate::anim::fade_in_once(
        f.ui.ctx(),
        egui::Id::unique("recording_overlay_in"),
        std::time::Duration::from_millis(200),
        crate::anim::Ease::OutCubic,
    );

    // 遮罩：与 MCP 覆盖层同色同 alpha（96）——压暗但不糊死，
    // 模糊本身才是「失焦」的主体。色值统一走 [`overlay_scrim`]。
    let shade = overlay_scrim(pal);
    // 遮罩随入场淡入（预乘缩放，见 `fade_color`）。
    f.ui
        .painter_at(area)
        .rect_filled(area, 0.0, fade_color(shade, k));

    // ---- 第一个圆角正方形：外层面板（居中）----
    //
    // 用**正方形**而非横幅：录制是「看一眼、按一下、点一下」的
    // 短动作，正方形把内容聚在视觉中心，横幅会让键帽散成一行、
    // 提示文字离得老远。
    let side = 340.0_f32
        .min(area.width() * 0.92)
        .min(area.height() * 0.92);
    // 面板从上方 8pt 沉入（k=1 时归位）。
    let panel = Rect::from_center_size(
        area.center() + vec2(0.0, (1.0 - k) * 8.0),
        vec2(side, side),
    );

    // 全屏交互：录制态下**任意位置**（含面板内）的点击都算确认。
    // 子内容全部 painter 直绘、不注册交互区，点击自然落到这层。
    let bg = f.ui.interact(area, egui::Id::unique("recording_overlay"), egui::Sense::click());
    if bg.clicked() {
        f.state.push(Op::FinishRecording);
    }

    let p = f.ui.painter_at(panel);
    p.rect_filled(panel, pal.radius_lg, pal.surface);

    // ---- 布局：统一垂直节奏 + 两行等宽（2026-10-22 用户要求「对称对齐」）----
    //
    // 早前标题带 `y + 10` 的基线补偿、三段间距各不相等（34 / 46+16 /
    // 72+18），肉眼看就是「一高一低」。现在每块等间距 `vgap` 排布，
    // 块内文字/键帽一律按**垂直中心**放置，标题与提示也用
    // CENTER_CENTER 而不是 CENTER_TOP——混合顶/中心对齐正是不齐的根源。
    //
    // 主键宽度取**修饰键行等宽**：两行左右边缘对齐，不是「上行一行宽、
    // 下行一个小方块」的悬空感。
    let pad = pal.space_lg * 1.4;
    let vgap = 18.0_f32;

    let cap_w = 62.0_f32;
    let cap_h = 46.0_f32;
    let gap = 10.0_f32;
    let row_w = cap_w * 4.0 + gap * 3.0;

    let mut y = panel.min.y + pad;

    // 标题（块高按字号算，随 font_scale 缩放）。
    let title_h = pal.font_md * 1.7;
    p.text(
        pos2(panel.center().x, y + title_h / 2.0),
        egui::Align2::CENTER_CENTER,
        t("Recording shortcut"),
        egui::FontId::proportional(pal.font_md),
        pal.text_bright,
    );
    y += title_h + vgap;

    // ---- 修饰键行：4 键等宽、行居中 ----
    //
    // 修饰键状态：**已确定主键就保持已录制的组合**，否则取实时按下。
    // 用户口径（2026-10-22）：「在已经输入键后，修饰键录制状态保持
    // 直至输入键再次更新后」——录完 Ctrl+V 松手，Ctrl 必须继续亮着，
    // 否则界面上看不出刚录了什么。下一次输入主键会重写
    // `recording_combo`，键帽随之更新。
    let mods = f.ui.input(|i| i.modifiers);
    let win_down = modular_clipboard_platform::win_key_down();
    let (ctrl_on, shift_on, alt_on, win_on) = match &f.state.recording_combo {
        Some(hk) => (hk.ctrl, hk.shift, hk.alt, hk.win),
        None => (mods.ctrl, mods.shift, mods.alt, win_down),
    };
    let caps: [(&str, bool); 4] = [
        ("Ctrl", ctrl_on),
        ("Shift", shift_on),
        ("Alt", alt_on),
        ("Win", win_on),
    ];
    let row_x0 = panel.center().x - row_w / 2.0;
    for (i, (name, down)) in caps.iter().copied().enumerate() {
        let r = Rect::from_min_size(
            pos2(row_x0 + i as f32 * (cap_w + gap), y),
            vec2(cap_w, cap_h),
        );
        // 修饰键键帽：小圆角方块。亮 = accent 填充；暗 = surface_variant。
        let (fill, fg) = if down {
            (pal.accent, pal.text_on_accent)
        } else {
            (pal.surface_variant, pal.text_dim)
        };
        p.rect_filled(r, pal.radius_md, fill);
        p.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            name,
            egui::FontId::proportional(pal.font_sm),
            fg,
        );
    }
    y += cap_h + vgap;

    // ---- 主键：与修饰键行**等宽**，左右边缘对齐 ----
    //
    // 早前 104pt 的方块悬在 278pt 的修饰键行下面，两边都空着，
    // 看着「下盘不稳」。等宽后两行是一个整体。
    let main_h = 64.0_f32;
    let main = Rect::from_min_size(pos2(row_x0, y), vec2(row_w, main_h));
    let has_key = f.state.recording_combo.is_some();
    let (fill, fg) = if has_key {
        (pal.accent, pal.text_on_accent)
    } else {
        (pal.surface_variant, pal.text_dim)
    };
    p.rect_filled(main, pal.radius_md, fill);
    let main_label = f
        .state
        .recording_combo
        .as_ref()
        .map(|hk| hk.key.to_uppercase())
        .unwrap_or_else(|| "？".into());
    p.text(
        main.center(),
        egui::Align2::CENTER_CENTER,
        main_label,
        // 主键名的「大显示」：比 font_lg 大一档，随 font_scale 缩放
        // （裸 24.0 在高字号下会比周围文字小，失去焦点效果）。
        egui::FontId::proportional(pal.font_lg * 1.25),
        fg,
    );
    y += main_h + vgap;

    // 底部提示（与标题同款 CENTER_CENTER，不混用顶对齐）。
    let hint_h = pal.font_sm * 1.6;
    p.text(
        pos2(panel.center().x, y + hint_h / 2.0),
        egui::Align2::CENTER_CENTER,
        t("Click anywhere to stop recording"),
        egui::FontId::proportional(pal.font_sm),
        pal.text_dim,
    );
}

/// MCP 调用设置覆盖层。
///
/// 结构：全窗遮罩（白/黑随主题）+ 圆角面板（限制在窗口
/// 客户区内，max 92% 宽 / 82% 高）+ 8 个可调用能力开关 + 取消/完成。
///
/// NOTE: 遮罩背后现在是**真正的 GPU 模糊**——`draw_settings` 在调用本函数
/// 前放了一个分割标记（见 [`push_blur_split_marker`]），渲染层把标记之前的
/// 界面内容画到离屏图、跑三趟卷积（降采样 + 横向 + 纵向），再把模糊结果
/// 作为全屏底图铺回来。遮罩只做一层淡淡的压暗，模糊本身才是「失焦」的主体，
/// 因此 alpha 从早期的 148 降到 96（否则会把模糊糊死、看不出效果）。
pub(super) fn settings_mcp_overlay(f: &mut Frame<'_>, area: Rect) {
    let pal = f.pal;

    // 遮罩色随主题：亮色界面用白、暗色界面用黑（[`overlay_scrim`]）。
    f.ui
        .painter_at(area)
        .rect_filled(area, 0.0, overlay_scrim(pal));

    // 面板：居中，限制在窗口客户区内。
    let pw = 480.0_f32.min(area.width() * 0.92);
    let ph = 560.0_f32.min(area.height() * 0.82);
    let panel = Rect::from_center_size(area.center(), vec2(pw, ph));

    // 点击面板外关闭。
    let bg = f.ui.interact(area, egui::Id::unique("mcp_overlay_bg"), egui::Sense::click());
    if bg.clicked() && bg.interact_pointer_pos().is_some_and(|p| !panel.contains(p)) {
        f.state.mcp_overlay_open = false;
    }

    let painter = f.ui.painter_at(panel);
    painter.rect_filled(panel, pal.radius_lg, pal.surface);

    let mut ui = f.ui.new_child(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique("mcp_overlay"))
            .max_rect(panel.shrink2(vec2(16.0, 14.0)))
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    ui.spacing_mut().item_spacing = vec2(0.0, 6.0);

    // 标题行：左标题，右 × 关闭。
    let total_on = MCP_CAPS.iter().filter(|c| get_mcp_cap(&f.svc.state.config, c.field)).count();
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(t("MCP tool access"))
                .strong()
                .size(pal.font_md)
                .color(pal.text_bright),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 按钮尺寸随 control_height（@scale1≈26）：固定 26 在高字号
            // 下相对文字偏小、可点区域缩水。
            let btn = pal.control_height * 0.93;
            let (r, resp) = ui.allocate_exact_size(vec2(btn, btn), egui::Sense::click());
            let p = ui.painter_at(r);
            if resp.hovered() {
                p.rect_filled(r, pal.radius_sm, pal.row_hover);
            }
            crate::icons::Icon::Close.paint(
                &p,
                Rect::from_center_size(
                    r.center(),
                    vec2(pal.icon_size * 0.81, pal.icon_size * 0.81),
                ),
                if resp.hovered() { pal.text_bright } else { pal.text_dim },
            );
            f.hit_insert("settings_mcp_overlay_close", r);
            if resp.clicked() {
                f.state.mcp_overlay_open = false;
            }
        });
    });

    // 副标题：能力总数 / 当前可调用数。
    ui.label(
        egui::RichText::new(
            t("{a} tools · {b} callable · all disabled when off")
            .replace("{a}", &MCP_CAPS.len().to_string())
            .replace(
                "{b}",
                &if f.svc.state.config.mcp.enabled { total_on } else { 0 }.to_string(),
            ),
        )
        .size(pal.font_xs)
        .color(pal.text_dim),
    );

    // 服务连通状态（**真实探测结果**，不是编译期常量）。
    //
    // 这一行是「徽标为什么显示未连接」的答案来源：未连接有四种成因
    // （总开关关 / 未配置端口 / 服务没起 / 已连上），只给一个徽标
    // 用户完全不知道该去改哪里。
    //
    // CAUTION: 必须拆成**两行**。`McpConn::detail()` 最长约 25 个
    // 全角字，与「MCP 服务：xx」挤在同一行会超出面板右缘——egui 的
    // Label 在**横向**布局（`ui.horizontal`）里默认 `TextWrapMode::
    // Extend`：不换行也不截断，直接溢出容器（用户反馈「文字渲染出
    // 界面」）。放到纵向布局下才默认 Wrap。第一行只放短状态，
    // 第二行放长说明。
    {
        let conn = f.state.mcp_conn;
        let color = if conn.is_connected() {
            pal.accent
        } else {
            pal.text_dim
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 0.0);
            // 状态圆点：比读文字更快识别。
            let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter_at(dot).circle_filled(dot.center(), 3.5, color);
            ui.label(
                egui::RichText::new(format!("{}{}", t("MCP service: "), t(conn.label())))
                    .size(pal.font_xs)
                    .color(color),
            );
        });
        ui.label(
            egui::RichText::new(t(conn.detail()))
                .size(pal.font_xs)
                .color(pal.text_dim),
        );
    }

    // 分隔线。
    let sep_y = ui.min_rect().max.y + 4.0;
    let panel_p = f.ui.painter_at(panel);
    panel_p.hline(
        panel.min.x + 16.0..=panel.max.x - 16.0,
        sep_y,
        egui::Stroke::new(1.0, pal.border_subtle),
    );

    // 能力列表（可滚动）。
    let list_top = sep_y + 8.0;
    let list_bottom = panel.max.y - 52.0;
    let list_rect = Rect::from_min_max(pos2(panel.min.x + 16.0, list_top), pos2(panel.max.x - 16.0, list_bottom));
    if list_bottom > list_top + 8.0 {
        let mut lu = f.ui.new_child(
            egui::UiBuilder::new()
                .scope_id(egui::Id::unique("mcp_overlay_list"))
                .max_rect(list_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        lu.spacing_mut().item_spacing = vec2(0.0, 2.0);
        egui::ScrollArea::vertical()
            .id_salt("mcp_overlay_scroll")
            .auto_shrink([false, false])
            .show(&mut lu, |ui| {
                for cap in MCP_CAPS {
                    settings_mcp_overlay_row(f, ui, cap);
                }
            });
    }

    // 底部：取消 / 完成。
    let mut bu = f.ui.new_child(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique("mcp_overlay_bottom"))
            .max_rect(Rect::from_min_max(
                pos2(panel.min.x + 16.0, list_bottom),
                pos2(panel.max.x - 16.0, panel.max.y - 10.0),
            ))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    bu.spacing_mut().item_spacing = vec2(8.0, 0.0);
    draw_mcp_overlay_btn(f, &mut bu, "settings_mcp_overlay_done", t("Done"), true);
    draw_mcp_overlay_btn(f, &mut bu, "settings_mcp_overlay_cancel", t("Cancel"), false);
}

/// 覆盖层底部按钮（自绘小按钮：圆角 + 文字，右对齐顺序）。
pub(super) fn draw_mcp_overlay_btn(
    f: &mut Frame<'_>,
    ui: &mut Ui,
    key: &'static str,
    label: &str,
    primary: bool,
) {
    let pal = f.pal;
    // 尺寸随字号缩放（基准 84x32 @ font_sm 15）。
    let (r, resp) = ui.allocate_exact_size(
        vec2(pal.font_sm * 5.0 + 9.0, pal.control_height + 4.0),
        egui::Sense::click(),
    );
    let p = ui.painter_at(r);
    let hovered = resp.hovered();
    let fill = if primary {
        if hovered { pal.accent_hover } else { pal.accent }
    } else if hovered {
        pal.row_hover
    } else {
        pal.surface_variant
    };
    p.rect_filled(r, pal.radius_md, fill);
    let color = if primary {
        pal.text_on_accent
    } else {
        pal.text
    };
    p.text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(pal.font_sm),
        color,
    );
    f.hit_insert(key, r);
    if resp.clicked() {
        f.state.mcp_overlay_open = false;
    }
}

/// 覆盖层里的一行能力：左 label+desc，右 徽标 + 开关。
pub(super) fn settings_mcp_overlay_row(f: &mut Frame<'_>, ui: &mut Ui, cap: &McpCap) {
    let total_enabled = f.svc.state.config.mcp.enabled;
    let mut v = get_mcp_cap(&f.svc.state.config, cap.field);
    let changed = setting_row(ui, cap.name, cap.desc, |ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
            if total_enabled {
                // 开关（最右）。id 直接用字段名（主卡片开关只用 "enabled"，
                // 与能力字段不冲突；热区 key 同理不冲突）。
                let sw = draw_switch(f, ui, cap.field, &mut v);
                // 徽标：已连接 / 未连接。
                //
                // 判据 = **能力已实现**（`ready`，编译期常量）**且** MCP 服务
                // 真的探测得到。早前只看 `ready`，于是只要代码里写了这个能力
                // 就永远显示「已连接」，与服务是否在运行毫无关系。
                let conn = cap.ready && f.state.mcp_conn.is_connected();
                draw_mcp_badge(f, ui, conn);
                sw
            } else {
                // 总开关关闭：画灰色静态开关 + 灰色徽标（不可交互）。
                draw_mcp_badge_disabled(f, ui);
                draw_static_switch_off(f, ui);
                false
            }
        })
        .inner
    });
    if changed {
        f.state.push(Op::SetBool {
            group: ConfigGroup::Mcp,
            field: cap.field,
            value: v,
        });
        record_recent(f, cap.field);
    }
}

/// 「已连接 / 未连接」徽标。
pub(super) fn draw_mcp_badge(f: &mut Frame<'_>, ui: &mut Ui, ready: bool) {
    let pal = f.pal;
    let (text, fg) = if ready {
        (t("Connected"), pal.accent)
    } else {
        (t("Not connected"), pal.text_dim)
    };
    // 宽度 = 三个全角字 + 8pt 内边距，随字号缩放：固定 50pt 时
    // 1.5× 字号（三字约 63pt）会把文字顶出徽标。
    let w = pal.font_xs * 3.0 + 8.0;
    let h = pal.font_xs * 1.43;
    let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::hover());
    let p = ui.painter_at(r);
    p.rect_filled(r, 10.0, pal.surface_variant);
    p.text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(pal.font_xs),
        fg,
    );
}

/// 总开关关闭时的灰色徽标。
pub(super) fn draw_mcp_badge_disabled(f: &mut Frame<'_>, ui: &mut Ui) {
    let pal = f.pal;
    // 同 `draw_mcp_badge`：宽度 = 三个全角字 + 8pt 内边距，随字号缩放。
    let w = pal.font_xs * 3.0 + 8.0;
    let h = pal.font_xs * 1.43;
    let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::hover());
    let p = ui.painter_at(r);
    p.rect_filled(r, 10.0, pal.surface_variant);
    p.text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        t("Disabled"),
        egui::FontId::proportional(pal.font_xs),
        pal.text_dim,
    );
}

/// 灰色静态开关（关闭态，不可交互）。
pub(super) fn draw_static_switch_off(f: &mut Frame<'_>, ui: &mut Ui) {
    let pal = f.pal;
    let h = 18.0_f32;
    let w = h * 1.8;
    let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::hover());
    let track = Rect::from_center_size(r.center(), vec2(w, h));
    let p = ui.painter_at(r);
    p.rect_filled(track, h * 0.5, pal.surface_variant);
    p.rect_stroke(track, h * 0.5, Stroke::new(1.0, pal.border), egui::StrokeKind::Inside);
    p.circle_filled(pos2(track.min.x + h * 0.5, track.center().y), h * 0.5 - 2.0, pal.text_dim);
}
