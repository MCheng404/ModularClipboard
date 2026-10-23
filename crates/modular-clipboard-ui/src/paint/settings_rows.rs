//! 设置行渲染：一行设置项、搜索结果、最近查看、排版助手。
//!
//! 「先量行高、再按显式矩形摆放」是这里的核心约定——
//! egui 横向布局不重排先放置的控件，顺序放必然错位。

use egui::Rect;
use egui::Sense;
use egui::Ui;
use egui::pos2;
use egui::vec2;
use modular_clipboard_core::config::WindowBackdrop;
use modular_clipboard_core::i18n::Bi;
use modular_clipboard_core::i18n::t;
use crate::card::PinnedMode;
use crate::theme::Palette;

use super::{Frame, Op, ConfigGroup};

use super::settings_items::FONT_SCALE_PRESETS;
use super::settings_items::ItemKind;
use super::settings_items::SETTINGS_CARDS;
use super::settings_items::SettingsItem;
use super::settings_items::draw_row_meta_editor;
use super::settings_items::font_scale_is_preset;
use super::settings_items::font_scale_percent;
use super::settings_items::get_bool;
use super::settings_items::get_number;
use super::settings_items::get_opt_number;
use super::settings_items::get_text;
use super::settings_items::shadow_param_row;
use super::controls::draw_segmented;
use super::controls::draw_slider;
use super::controls::draw_step_slider;
use super::controls::draw_switch;
use super::controls::draw_text_field;
use super::theme_editor::draw_accent_editor;
use super::theme_editor::draw_token_entry;
use super::mcp_overlay::draw_mcp_info_btn;

/// 一行设置项：按类型分派到开关 / 数值 / 单选 / 只读。
pub(super) fn settings_item_row(f: &mut Frame<'_>, ui: &mut Ui, it: &SettingsItem) {
    let pal = f.pal;
    match &it.kind {
        ItemKind::Bool(g, field) => {
            let mut v = get_bool(&f.svc.state.config, *g, field);
            if setting_row(ui, it.label, it.desc, |ui| draw_switch(f, ui, field, &mut v)) {
                f.state.push(Op::SetBool { group: *g, field, value: v });
                record_recent(f, it.key);
            }
        }
        ItemKind::Number(g, field, spec) => {
            let cur = (spec.display)(get_number(&f.svc.state.config, *g, field));
            let mut val = cur;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                ui.add(
                    egui::DragValue::new(&mut val)
                        .range(spec.min..=spec.max)
                        .speed(spec.step)
                        .suffix(t(spec.unit)),
                )
                .changed()
            });
            if changed {
                f.state.push(Op::SetNumber {
                    group: *g,
                    field,
                    value: (spec.store)(val),
                });
                record_recent(f, it.key);
            }
        }
        ItemKind::OptNumber(g, field, spec) => {
            let cur = get_opt_number(&f.svc.state.config, *g, field);
            let mut val = cur.unwrap_or(spec.default);
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                ui.add(
                    egui::DragValue::new(&mut val)
                        .range(spec.default..=1_000_000.0)
                        .speed(spec.step)
                        .suffix(t(spec.unit)),
                )
                .changed()
            });
            if changed {
                let next = if val <= spec.default { None } else { Some(val) };
                f.state.push(Op::SetOptNumber { group: *g, field, value: next });
                record_recent(f, it.key);
            }
        }
        ItemKind::PinnedMode => {
            setting_row(ui, it.label, it.desc, |ui| {
                let picked = draw_segmented(
                    f,
                    ui,
                    "pinned_mode",
                    &[
                        (PinnedMode::SharedColumn, ("共用单栏", "Shared column")),
                        (PinnedMode::OwnCard, ("独立分栏", "Own column")),
                    ],
                    f.ws.pinned_mode(),
                );
                if let Some(m) = picked {
                    f.state.push(Op::SetPinnedMode(m));
                    record_recent(f, it.key);
                }
                picked.is_some()
            });
        }
        ItemKind::FontScale => {
            let cur = f.svc.state.config.ui.font_scale;
            setting_row(ui, it.label, it.desc, |ui| {
                // 百分比读数（right_to_left：先加的靠右 ⇒ 显示在分段控件右侧）。
                // 不在预设上时标「自定义」——比假装选中最近档位诚实。
                let pct = (crate::theme::clamp_font_scale(cur) * 100.0).round() as i32;
                let readout = if font_scale_is_preset(cur) {
                    format!("{pct}%")
                } else {
                    format!("{pct}%{}", t(("（自定义）", " (custom)")))
                };
                ui.add(egui::Label::new(
                    egui::RichText::new(readout).color(pal.text_dim),
                ));
                // 分段控件：值与配置同源，点哪段设哪个倍率。
                let picked = draw_segmented(f, ui, "font_scale", FONT_SCALE_PRESETS, font_scale_percent(cur));
                if let Some(p) = picked {
                    f.state.push(Op::SetNumber {
                        group: ConfigGroup::Ui,
                        field: "font_scale",
                        value: f64::from(p) / 100.0,
                    });
                    record_recent(f, it.key);
                }
                picked.is_some()
            });
        }
        ItemKind::Language => {
            setting_row(ui, it.label, it.desc, |ui| {
                let cur = f.svc.state.config.ui.language.as_str();
                let sel = if cur.eq_ignore_ascii_case("zh-CN") || cur.eq_ignore_ascii_case("zh") {
                    1u8
                } else if cur.eq_ignore_ascii_case("en-US") || cur.eq_ignore_ascii_case("en") {
                    2u8
                } else {
                    0u8 // system / 未知写法都显示「跟随系统」
                };
                let picked = draw_segmented(
                    f,
                    ui,
                    "ui_language",
                    &[
                        (0u8, ("跟随系统", "Follow system")),
                        (1u8, ("中文", "Chinese")),
                        (2u8, ("English", "English")),
                    ],
                    sel,
                );
                if let Some(v) = picked {
                    f.state.push(Op::SetLanguage(match v {
                        1 => "zh-CN",
                        2 => "en-US",
                        _ => modular_clipboard_core::i18n::LANG_SYSTEM,
                    }));
                    true
                } else {
                    false
                }
            });
        }
        ItemKind::Theme => {
            setting_row(ui, it.label, it.desc, |ui| {
                let picked = draw_segmented(
                    f,
                    ui,
                    "theme_mode",
                    &[
                        (crate::theme::ThemeMode::FollowSystem, ("跟随系统", "Follow system")),
                        (crate::theme::ThemeMode::Dark, ("深色", "Dark")),
                        (crate::theme::ThemeMode::Light, ("浅色", "Light")),
                    ],
                    crate::theme::ThemeMode::from_dark_mode(f.svc.state.config.ui.dark_mode),
                );
                if let Some(m) = picked {
                    f.state.push(Op::SetThemeMode(m));
                    record_recent(f, it.key);
                }
                picked.is_some()
            });
        }
        ItemKind::Accent => {
            if draw_accent_editor(f, ui) {
                record_recent(f, it.key);
            }
        }
        ItemKind::SecondaryTheme => {
            use modular_clipboard_core::config::SecondaryTheme;
            setting_row(ui, it.label, it.desc, |ui| {
                let picked = draw_segmented(
                    f,
                    ui,
                    "secondary_theme",
                    &[
                        (SecondaryTheme::None, ("无", "None")),
                        (SecondaryTheme::Paper, ("纸纹", "Paper")),
                        (SecondaryTheme::Green, ("护眼绿", "Eye-care green")),
                        (SecondaryTheme::Purple, ("星空紫", "Starfield purple")),
                        (SecondaryTheme::Oled, ("OLED", "OLED")),
                    ],
                    f.svc.state.config.ui.secondary_theme,
                );
                if let Some(t) = picked {
                    f.state.push(Op::SetSecondaryTheme(t));
                    record_recent(f, it.key);
                }
                picked.is_some()
            });
        }
        ItemKind::ThemeParams => {
            let theme = f.svc.state.config.ui.secondary_theme;
            let mut params = f.svc.state.config.ui.theme_params;
            let mut changed = false;
            if theme.knobs().is_empty() {
                // 没有二级主题：整项禁用并压暗（与磨砂强度在非磨砂模式下
                // 禁用的处理一致——「关着还能调」会让用户以为参数没保存）。
                let _ = setting_row(ui, it.label, it.desc, |ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new(t(("先选择一个风格主题", "Select a style theme first"))).color(f.pal.text_dim),
                    ));
                    false
                });
            } else {
                // 摘要行：右侧显示当前主题名（参数归属一眼可见）。
                let _ = setting_row(ui, it.label, it.desc, |ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new(t(theme.label())).color(f.pal.text_dim),
                    ));
                    false
                });
                // 旋钮按主题的 `knobs()` 动态生成：新增主题参数只用在
                // core 侧登记，设置界面自动跟上（键名寻址测试守着）。
                for knob in theme.knobs() {
                    let mut v = params.get(knob.key).unwrap_or(0.5);
                    ui.add_space(2.0);
                    let c = shadow_param_row(
                        f,
                        ui,
                        knob.key,
                        t(knob.label),
                        format!("{}%", (v * 100.0).round() as i32),
                        &mut v,
                        0.0..=1.0,
                    );
                    if c {
                        params.set(knob.key, v);
                        changed = true;
                    }
                }
            }
            if changed {
                // 整包替换：两个旋钮可能同帧连改（见 Op::SetThemeParams）。
                f.state.push(Op::SetThemeParams(params));
                record_recent(f, it.key);
            }
        }
        ItemKind::TokenOverrides => {
            // 2026-10-19：入口行只负责打开二级面板（draw_token_panel），
            // 近 60 行令牌不再内联在主题卡里。
            draw_token_entry(f, ui);
        }
        ItemKind::Backdrop => {
            setting_row(ui, it.label, it.desc, |ui| {
                let picked = draw_segmented(
                    f,
                    ui,
                    "window_backdrop",
                    &[
                        (WindowBackdrop::None, ("无", "None")),
                        (WindowBackdrop::Acrylic, ("磨砂", "Frosted")),
                        (WindowBackdrop::Translucent, ("半透明", "Translucent")),
                    ],
                    f.svc.state.config.ui.window_backdrop,
                );
                if let Some(m) = picked {
                    f.state.push(Op::SetBackdrop(m));
                    record_recent(f, it.key);
                }
                picked.is_some()
            });
        }
        ItemKind::Opacity => {
            let mut op = f.svc.state.config.ui.window_opacity;
            let enabled = f.svc.state.config.ui.window_backdrop != WindowBackdrop::None;
            let pal = f.pal;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // right_to_left 布局：先加的靠右 ⇒ 百分比显示在滑条右侧。
                ui.add(egui::Label::new(
                    egui::RichText::new(format!("{}%", (op * 100.0).round() as i32))
                        .color(pal.text_dim),
                ));
                draw_slider(f, ui, "window_opacity", "", &mut op, 0.3..=1.0, enabled, 0.0)
            });
            if changed && enabled {
                f.state.push(Op::SetOpacity(op));
                record_recent(f, it.key);
            }
        }
        ItemKind::CardOpacity => {
            let mut op = f.svc.state.config.ui.card_opacity;
            // 与「背景不透明度」同一条依赖：背景不透明时没有底图可透，
            // 这个滑块禁用并压暗（「关着还能调」会让用户以为没保存）。
            let enabled = f.svc.state.config.ui.window_backdrop != WindowBackdrop::None;
            let pal = f.pal;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                ui.add(egui::Label::new(
                    egui::RichText::new(format!("{}%", (op * 100.0).round() as i32))
                        .color(pal.text_dim),
                ));
                draw_slider(
                    f,
                    ui,
                    "card_opacity",
                    "",
                    &mut op,
                    0.3..=1.0,
                    enabled,
                    0.0,
                )
            });
            if changed && enabled {
                f.state.push(Op::SetCardOpacity(op));
                record_recent(f, it.key);
            }
        }
        ItemKind::BlurStrength => {
            // 只有磨砂模式需要模糊强度；半透明 / 无背景时禁用并压暗。
            let mut level = f.svc.state.config.ui.window_blur_strength;
            let enabled =
                f.svc.state.config.ui.window_backdrop == WindowBackdrop::Acrylic;
            let pal = f.pal;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // right_to_left：先加的靠右 ⇒ 档位数字在滑条右侧。
                ui.add(egui::Label::new(
                    egui::RichText::new(format!("{level}/10")).color(pal.text_dim),
                ));
                draw_step_slider(
                    f,
                    ui,
                    "window_blur_strength",
                    &mut level,
                    1..=10,
                    enabled,
                )
            });
            if changed && enabled {
                f.state.push(Op::SetBlurStrength(level));
                record_recent(f, it.key);
            }
        }
        ItemKind::CardShadow => {
            let mut cfg = f.svc.state.config.ui.card_shadow;
            let mut changed = false;

            // 开关行：整项的总闸。
            changed |= setting_row(ui, it.label, it.desc, |ui| {
                draw_switch(f, ui, "card_shadow_enabled", &mut cfg.enabled)
            });

            // 参数子块：仅在开启时显示——「关着还能调」的控件只会
            // 让用户以为参数没保存。
            if cfg.enabled {
                ui.add_space(2.0);
                changed |= shadow_param_row(
                    f,
                    ui,
                    "card_shadow_blur",
                    t(("模糊", "Blur")),
                    format!("{}", cfg.blur.round() as i32),
                    &mut cfg.blur,
                    0.0..=24.0,
                );
                changed |= shadow_param_row(
                    f,
                    ui,
                    "card_shadow_opacity",
                    t(("浓度", "Opacity")),
                    format!("{}%", (cfg.opacity * 100.0).round() as i32),
                    &mut cfg.opacity,
                    0.0..=0.6,
                );
                changed |= shadow_param_row(
                    f,
                    ui,
                    "card_shadow_offset_y",
                    t(("偏移", "Offset")),
                    format!("{}", cfg.offset_y.round() as i32),
                    &mut cfg.offset_y,
                    -8.0..=16.0,
                );
            }

            if changed {
                // 整包替换：三个滑块 + 开关可能在同一帧连续改，一个 Op
                // 就够（见 `Op::SetCardShadow` 的说明）。
                f.state.push(Op::SetCardShadow(cfg));
                record_recent(f, it.key);
            }
        }
        ItemKind::RowMeta => {
            // 编辑器内部自己 push Op（单击 / 拖拽可能一帧内多次改）。
            if draw_row_meta_editor(f, ui) {
                record_recent(f, it.key);
            }
        }
        ItemKind::TypeRail => {
            // 类型过滤栏：总开关 + （开启时）始终显示 / 多选 / 标签 /
            // 位置 / 大小。整包替换（六个参数可能同帧连改，见 Op::SetTypeRail）。
            let mut cfg = f.svc.state.config.ui.type_rail;
            let mut changed = false;
            changed |= setting_row(ui, it.label, it.desc, |ui| {
                draw_switch(f, ui, "type_rail_visible", &mut cfg.visible)
            });
            if cfg.visible {
                ui.add_space(2.0);
                changed |= setting_row(ui, ("始终显示全部类型", "Show all types"), ("关闭时仅显示已有记录的类型", "When off, only types present in history are shown"), |ui| {
                    draw_switch(f, ui, "type_rail_always", &mut cfg.always)
                });
                ui.add_space(2.0);
                // 多选：按钮变开关，可同时点亮多个类型（2026-10-20 新增）。
                changed |= setting_row(ui, ("多选组合过滤", "Multi-select"), ("开启后可同时选择多个类型，列表显示任一所选类型的条目", "Select several types at once; the list shows entries matching any of them"), |ui| {
                    draw_switch(f, ui, "type_rail_multi", &mut cfg.multi)
                });
                ui.add_space(2.0);
                // 文字标签：图标旁显示类型名称（2026-10-20 新增）。
                changed |= setting_row(ui, ("显示类型名称", "Show type names"), ("在图标旁显示文字标签；竖直停靠时在图标下方，水平停靠时在右侧", "Text labels next to the icons: below them when vertical, to the right when horizontal"), |ui| {
                    draw_switch(f, ui, "type_rail_labels", &mut cfg.labels)
                });
                ui.add_space(2.0);
                // 位置：四向分段控件（与「背景效果」同一控件）。
                changed |= setting_row(ui, ("停靠位置", "Dock position"), ("类型栏贴在主窗口的哪一侧", "Which edge of the main window the bar sticks to"), |ui| {
                    use modular_clipboard_core::config::TypeRailPos;
                    let picked = draw_segmented(
                        f,
                        ui,
                        "type_rail_pos",
                        &[
                            (TypeRailPos::Right, ("右", "Right")),
                            (TypeRailPos::Left, ("左", "Left")),
                            (TypeRailPos::Top, ("上", "Top")),
                            (TypeRailPos::Bottom, ("下", "Bottom")),
                        ],
                        cfg.pos,
                    );
                    if let Some(p) = picked {
                        cfg.pos = p;
                    }
                    picked.is_some()
                });
                ui.add_space(2.0);
                // 大小：24~48pt 滑块（步进由滑块天然连续，绘制侧钳制）。
                let mut size = modular_clipboard_core::config::clamp_type_rail_size(cfg.size);
                let pal = f.pal;
                changed |= setting_row(ui, ("图标大小", "Icon size"), ("类型按钮的边长（24~48pt）", "Side length of each type button (24-48 pt)"), |ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new(format!("{}", size.round() as i32)).color(pal.text_dim),
                    ));
                    draw_slider(
                        f,
                        ui,
                        "type_rail_size",
                        "",
                        &mut size,
                        modular_clipboard_core::config::TYPE_RAIL_SIZE_MIN
                            ..=modular_clipboard_core::config::TYPE_RAIL_SIZE_MAX,
                        true,
                        0.0,
                    )
                });
                cfg.size = modular_clipboard_core::config::clamp_type_rail_size(size);
            }
            if changed {
                f.state.push(Op::SetTypeRail(cfg));
                record_recent(f, it.key);
            }
        }
        ItemKind::SearchPanel => {
            // 悬浮搜索面板：停靠位置（窗口上/下）+ 高度。整包替换。
            let mut cfg = f.svc.state.config.ui.search_panel;
            let mut changed = false;
            changed |= setting_row(ui, ("停靠位置", "Dock position"), ("搜索面板贴在主窗口的上方还是下方", "Whether the panel docks above or below the main window"), |ui| {
                use modular_clipboard_core::config::SearchPanelPos;
                let picked = draw_segmented(
                    f,
                    ui,
                    "search_panel_pos",
                    &[
                        (SearchPanelPos::Top, ("上方", "Top")),
                        (SearchPanelPos::Bottom, ("下方", "Bottom")),
                    ],
                    cfg.pos,
                );
                if let Some(p) = picked {
                    cfg.pos = p;
                }
                picked.is_some()
            });
            ui.add_space(2.0);
            let mut height = modular_clipboard_core::config::clamp_search_panel_height(cfg.height);
            let pal = f.pal;
            changed |= setting_row(ui, ("面板高度", "Panel height"), ("搜索面板的高度（30~56pt）", "Height of the search panel (30-56 pt)"), |ui| {
                ui.add(egui::Label::new(
                    egui::RichText::new(format!("{}", height.round() as i32)).color(pal.text_dim),
                ));
                draw_slider(
                    f,
                    ui,
                    "search_panel_height",
                    "",
                    &mut height,
                    modular_clipboard_core::config::SEARCH_PANEL_H_MIN
                        ..=modular_clipboard_core::config::SEARCH_PANEL_H_MAX,
                    true,
                    0.0,
                )
            });
            cfg.height = modular_clipboard_core::config::clamp_search_panel_height(height);
            if changed {
                f.state.push(Op::SetSearchPanel(cfg));
                record_recent(f, it.key);
            }
        }
        ItemKind::NoticeMode => {
            // 通知呈现：窗内 toast / 独立弹窗（分段控件，落点布尔配置）。
            let mut popup = f.svc.state.config.ui.notice_popup;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                let picked = draw_segmented(
                    f,
                    ui,
                    "notice_popup",
                    &[(false, ("窗口内提示", "In-window toast")), (true, ("独立弹窗", "Separate popup"))],
                    popup,
                );
                if let Some(p) = picked {
                    popup = p;
                }
                picked.is_some()
            });
            if changed {
                f.state.push(Op::SetBool {
                    group: ConfigGroup::Ui,
                    field: "notice_popup",
                    value: popup,
                });
                record_recent(f, it.key);
            }
        }
        ItemKind::Hotkey(slot) => {
            //
            // 两条输入路径都要有：
            // - **文本输入**：能精确表达 Win+V 这类组合，也能复制粘贴；
            // - **录制**：用户不用记键名，按一下就好。
            // 只做其中一个都会有人卡住（记不住键名 / 不知道 Win 怎么写）。
            //
            // 录制态下本行不画特殊 UI：录制覆盖层（`draw_recording_overlay`）
            // 会盖住整窗，这里保持朴素即可。
            let cur = slot.current(&f.svc.state.config);
            let mut changed = false;
            let _ = setting_row(ui, it.label, it.desc, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(slot.draft_mut(f.state))
                        // 宽度随字号缩放（基准 112 @ font_sm 15）。
                        .desired_width(pal.font_sm * 7.5)
                        .hint_text(cur.display())
                        .font(egui::TextStyle::Monospace),
                );
                // 回车即应用：设置项里再放一个「确定」按钮太重了。
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    // 先取文本再 push：两个字段同属 `f.state`，
                    // 一起借会撞借用检查。
                    let text = slot.draft_mut(f.state).trim().to_string();
                    f.state.push(Op::SetHotkeyText { slot: *slot, text });
                    changed = true;
                }
                // 输入框与按钮之间留 8pt：设置卡片把
                // `item_spacing.x` 归了 0（行与行才够呼吸），
                // 同行控件零间距会挤成一团（用户实测反馈）。
                ui.add_space(8.0);
                if ui.small_button(t(("录制", "Record"))).clicked() {
                    f.state.push(Op::StartRecording(*slot));
                }
                changed
            });
            // 注册失败要在**配置它的地方**可见：状态栏提示只闪 3 秒，
            // 用户回到设置页时早就没了。失败信息里含该行当前的组合文本
            // （格式由 `hotkey::failed_message` 固定），据此匹配行列。
            if let Some(err) = f.svc.state.hotkey_error.as_ref()
                && err.contains(cur.display().as_str())
            {
                ui.horizontal(|ui| {
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(format!("⚠ {err}"))
                            .color(pal.danger)
                            // 小一号提示字：随字号缩放（11.0 裸值在
                            // font_scale=2 时不放大，用户看到「半截文字」）。
                            .size(pal.font_xs),
                    );
                });
            }
            let _ = changed;
        }
        ItemKind::Autostart => {
            let mut v = f.svc.state.config.ui.autostart;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                draw_switch(f, ui, "autostart", &mut v)
            });
            if changed {
                f.state.push(Op::SetAutostart(v));
            }
        }
        ItemKind::McpToggle => {
            let mut v = get_bool(&f.svc.state.config, ConfigGroup::Mcp, "enabled");
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // control 位于 right_to_left 容器：先 add 的控件靠右。
                // 先画开关（最右），再画 ⓘ 详细按钮（紧贴开关**左边**，
                // 即「MCP 详细在总开关按钮的左边」）。
                let sw = draw_switch(f, ui, "mcp_enabled", &mut v);
                // 与 ⓘ 按钮之间留 8pt：设置卡片把 `item_spacing.x`
                // 归了 0（行与行才够呼吸），同行控件零间距会**直接
                // 贴在一起**——用户实测反馈「MCP 设置和开关挨得
                // 太挤」。热键行的输入框与「录制」按钮同款修法。
                ui.add_space(8.0);
                let info = draw_mcp_info_btn(f, ui);
                if info.clicked() {
                    f.state.mcp_overlay_open = !f.state.mcp_overlay_open;
                }
                sw
            });
            if changed {
                f.state.push(Op::SetBool {
                    group: ConfigGroup::Mcp,
                    field: "enabled",
                    value: v,
                });
                record_recent(f, it.key);
            }
        }
        ItemKind::Text {
            group,
            field,
            ph,
            secret,
        } => {
            let cur = get_text(&f.svc.state.config, *group, field);
            let mut buf = cur;
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // 宽度随字号缩放（基准 170 / 230 @ font_sm 15），
                // 否则 2× 字号下框里只显示得下 4~5 个字。
                let w = pal.font_sm * if *secret { 11.3 } else { 15.3 };
                draw_text_field(f, ui, field, &mut buf, t(*ph), *secret, w)
            });
            if changed {
                f.state.push(Op::SetText {
                    group: *group,
                    field,
                    value: buf,
                });
                record_recent(f, it.key);
            }
        }
        ItemKind::McpPort => {
            let port = f.svc.state.config.mcp.port;
            let mut buf = if port == 0 {
                String::new()
            } else {
                port.to_string()
            };
            let changed = setting_row(ui, it.label, it.desc, |ui| {
                // 宽度随字号缩放（基准 110 @ font_sm 15）。
                draw_text_field(f, ui, "mcp_port", &mut buf, "9123", false, pal.font_sm * 7.3)
            });
            if changed {
                let v: u16 = buf.trim().parse().unwrap_or(0);
                f.state.push(Op::SetNumber {
                    group: ConfigGroup::Mcp,
                    field: "port",
                    value: v as f64,
                });
                record_recent(f, it.key);
            }
        }
        ItemKind::Group => {
            // 分组小标题：给「关于」这种元信息页切出节奏，与搜索结果
            // 里的 `section()` 同一视觉语言（更轻一号）。
            ui.add_space(8.0);
            ui.label(egui::RichText::new(t(it.label)).size(pal.font_sm).color(pal.text_dim));
        }
        ItemKind::OpenDataDir => {
            // 数据目录**超链接**：路径是运行期事实（可能被 --data-dir
            // 改写），不在目录表里存；展示与打开都问 `Service`。
            let shown = f.svc.data_dir().display().to_string();
            setting_row(ui, it.label, it.desc, |ui| {
                let resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(shown.as_str()).size(pal.font_sm).color(pal.accent),
                    )
                    // 不截断会向左溢出到导航列上（见 Info 时期的教训）。
                    .truncate()
                    .sense(egui::Sense::click()),
                );
                f.hit_insert("settings_data_dir", resp.rect);
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    let r = resp.rect;
                    ui.painter_at(r).line_segment(
                        [pos2(r.left(), r.bottom()), pos2(r.right(), r.bottom())],
                        egui::Stroke::new(1.0, pal.accent),
                    );
                }
                if resp.clicked() {
                    f.state.push(Op::OpenDataDir);
                }
                false
            });
        }
        ItemKind::Link(url) => {
            setting_row(ui, it.label, it.desc, |ui| {
                // 强调色 URL，hover 手型 + 下划线；点击用默认浏览器打开。
                let resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new(*url).size(pal.font_sm).color(pal.accent),
                    )
                    // 同 `ItemKind::Info`：不截断会向左溢出到导航列上。
                    .truncate()
                    .sense(egui::Sense::click()),
                );
                // 登记热区（key 即 URL），供自动化与测试定位。
                f.hit_insert(url, resp.rect);
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    let r = resp.rect;
                    ui.painter_at(r).line_segment(
                        [pos2(r.left(), r.bottom()), pos2(r.right(), r.bottom())],
                        egui::Stroke::new(1.0, pal.accent),
                    );
                }
                if resp.clicked() {
                    f.state.push(Op::OpenLink(url));
                }
                false
            });
        }
        ItemKind::PastePreviousMode => {
            use modular_clipboard_core::config::PastePreviousMode as PM;
            let cur = f.svc.state.config.paste_previous_mode;
            setting_row(ui, it.label, it.desc, |ui| {
                let picked = draw_segmented(
                    f,
                    ui,
                    "paste_previous_mode",
                    &[
                        (PM::NotifyOnly, ("仅粘贴并提示", "Copy & notify")),
                        (PM::PasteAndType, ("直接粘贴并输入", "Copy & paste")),
                    ],
                    cur,
                );
                if let Some(m) = picked {
                    f.state.push(Op::SetPastePreviousMode(m));
                    record_recent(f, it.key);
                }
                picked.is_some()
            });
        }
        ItemKind::Quit => {
            ui.add_space(8.0);
            let (r, resp) = ui.allocate_exact_size(
                vec2(ui.available_width(), 32.0),
                egui::Sense::click(),
            );
            let p = ui.painter_at(r);
            if resp.hovered() {
                p.rect_filled(r, pal.radius_md, pal.surface_variant);
            }
            p.text(
                pos2(r.min.x + 4.0, r.center().y),
                egui::Align2::LEFT_CENTER,
                t(("退出程序", "Quit")),
                egui::FontId::proportional(pal.font_md),
                pal.danger,
            );
            f.hit_insert("settings_quit", r);
            if resp.clicked() {
                f.state.push(Op::Quit);
            }
        }
        ItemKind::Version => {
            // 版本号**运行时**算：cl 版带 commit 短 hash（排障时用户
            // 截图里的号能直接查到那次提交），稳定版显示 `0.2`。
            // 与静态只读行的差别就是「文本不是静态的」。
            let v = crate::about::version_label();
            setting_row(ui, it.label, it.desc, |ui| {
                let r = ui.add(
                    egui::Label::new(egui::RichText::new(v.as_str()).color(pal.text_dim))
                        .truncate(),
                );
                r.on_hover_text(v.as_str());
                false
            });
        }
        ItemKind::Stack => {
            // 技术栈：一段**可换行**的链接串，每段点开对应仓库。
            //
            // 不用 `setting_row`：右侧控件列是右对齐且固定宽的，6 个
            // 词条塞进去会被 truncate 到只剩第一个。整行铺开 + 换行，
            // 名称与 `·` 分隔符按阅读顺序排。
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 2.0);
                for (i, (name, url)) in crate::about::STACK_LINKS.iter().enumerate() {
                    if i > 0 {
                        ui.add(egui::Label::new(
                            egui::RichText::new("·").size(pal.font_sm).color(pal.text_dim),
                        ));
                    }
                    let resp = ui.add(
                        egui::Label::new(
                            egui::RichText::new(*name).size(pal.font_sm).color(pal.accent),
                        )
                        .sense(egui::Sense::click()),
                    );
                    f.hit_insert(url, resp.rect);
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        let r = resp.rect;
                        ui.painter_at(r).line_segment(
                            [pos2(r.left(), r.bottom()), pos2(r.right(), r.bottom())],
                            egui::Stroke::new(1.0, pal.accent),
                        );
                    }
                    if resp.clicked() {
                        f.state.push(Op::OpenLink(url));
                    }
                }
            });
        }
        ItemKind::LicenseView => {
            setting_row(ui, it.label, it.desc, |ui| {
                let resp = ui.add(
                    egui::Label::new(
                        egui::RichText::new("GNU GPL v3.0 or later")
                            .size(pal.font_sm)
                            .color(pal.accent),
                    )
                    .truncate()
                    .sense(egui::Sense::click()),
                );
                f.hit_insert("settings_license", resp.rect);
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    let r = resp.rect;
                    ui.painter_at(r).line_segment(
                        [pos2(r.left(), r.bottom()), pos2(r.right(), r.bottom())],
                        egui::Stroke::new(1.0, pal.accent),
                    );
                }
                if resp.clicked() {
                    // 打开协议悬浮面板（在设置窗口最上层画，见
                    // `draw_license_popup`）。不是开浏览器——用户点这里
                    // 就是想**看**协议，跳到网页反而多一跳。
                    f.state.license_popup = true;
                }
                false
            });
        }
        ItemKind::UpdateCheck => {
            // 检查在**后台线程**跑（`about::start_check`），点击这一帧
            // 立刻返回；结果由 `current_state()` 在后续帧里读到并显示。
            // 同步跑 `git ls-remote` 会把 UI 卡住几百毫秒到几秒（网络慢
            // 时更久），而主循环空转 ~125Hz，那一停顿非常显眼。
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
                let resp = ui.add_sized(
                    egui::vec2(88.0, 28.0),
                    egui::Button::new(
                        egui::RichText::new(t(("检查更新", "Check"))).color(pal.text),
                    )
                    .fill(pal.surface_variant),
                );
                f.hit_insert("settings_check_update", resp.rect);
                if resp.clicked() {
                    crate::about::start_check();
                    // 重新检查 ⇒ 允许结果再次弹窗（否则用户点掉一次后
                    // 永远不再弹，「检查更新」就成了只在行里显示一行字）。
                    f.state.update_popup_shown = false;
                }
                use crate::about::UpdateState;
                match crate::about::current_state() {
                    UpdateState::Idle => {}
                    UpdateState::Checking => {
                        ui.add(egui::Label::new(
                            egui::RichText::new(t(("检查中…", "Checking…"))).color(pal.text_dim),
                        ));
                    }
                    UpdateState::UpToDate { remote } => {
                        let s = if remote.is_empty() {
                            t(("已是最新", "Up to date")).to_string()
                        } else {
                            format!("{}（v{remote}）", t(("已是最新", "Up to date")))
                        };
                        ui.add(egui::Label::new(egui::RichText::new(s).color(pal.accent)));
                    }
                    UpdateState::Outdated {
                        local,
                        remote,
                        release,
                    } => {
                        // 有更新：除了那行文字，还要能点开详情弹窗。
                        // 文字本身就是入口（与「开源协议」行同款交互）。
                        let s = format!(
                            "{} v{remote}（{} {local}）",
                            t(("发现新版本", "Update available")),
                            t(("本地", "local"))
                        );
                        let r = ui.add(
                            egui::Label::new(egui::RichText::new(s).color(pal.accent))
                                .sense(egui::Sense::click()),
                        );
                        if r.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            let rr = r.rect;
                            ui.painter_at(rr).line_segment(
                                [pos2(rr.left(), rr.bottom()), pos2(rr.right(), rr.bottom())],
                                egui::Stroke::new(1.0, pal.accent),
                            );
                        }
                        if r.clicked() {
                            f.state.update_popup = true;
                        }
                        // 详情弹窗要能拿到下载页：没有 release 时按钮降级
                        // 成「打开仓库」，见 `draw_update_popup`。
                        let _ = release;
                    }
                    UpdateState::Failed(msg) => {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(msg).color(pal.danger).size(pal.font_sm),
                            )
                            .truncate(),
                        )
                        .on_hover_text(t(("远程不可达（网络/防火墙/git）", "Unreachable")));
                    }
                    UpdateState::Downloading => {
                        // 后台下载中：只给状态，按钮不再可点（`start_self_update`
                        // 里已做幂等，重复点也不会再起一个下载线程）。
                        ui.add(egui::Label::new(
                            egui::RichText::new(t(("下载中…", "Downloading…")))
                                .color(pal.text_dim),
                        ));
                    }
                    UpdateState::Applying => {
                        ui.add(egui::Label::new(
                            egui::RichText::new(t(("即将重启…", "Restarting…")))
                                .color(pal.accent),
                        ));
                    }
                }
            });
        }
    }
}

/// 搜索非空：跨全部分类过滤，命中项只读展示（点击跳转分类）。
pub(super) fn settings_search_results(f: &mut Frame<'_>, ui: &mut Ui, q: &str) {
    let pal = f.pal;
    let mut total = 0;
    for card in SETTINGS_CARDS {
        let hits: Vec<&SettingsItem> = card
            .items
            .iter()
            .filter(|it| {
                // 搜索对**两种语言**的名称与介绍、以及稳定 key 全匹配：
                // 界面切到英文时用中文词也能搜到，反之亦然。
                it.label.0.contains(q)
                    || it.label.1.contains(q)
                    || it.desc.0.contains(q)
                    || it.desc.1.contains(q)
                    || it.key.contains(q)
            })
            .collect();
        if hits.is_empty() {
            continue;
        }
        total += hits.len();
        section(ui, t(card.title), f.pal);
        for it in hits {
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), egui::Sense::click());
            if resp.hovered() {
                ui.painter_at(rect).rect_filled(rect, pal.radius_md, pal.row_hover);
            }
            ui.painter_at(rect).text(
                rect.min + vec2(8.0, 8.0),
                egui::Align2::LEFT_TOP,
                t(it.label),
                egui::FontId::proportional(pal.font_sm),
                pal.text,
            );
            ui.painter_at(rect).text(
                rect.max - vec2(8.0, 8.0),
                egui::Align2::RIGHT_BOTTOM,
                t(card.title),
                egui::FontId::proportional(pal.font_xs),
                pal.text_dim,
            );
            if resp.clicked() {
                f.state.settings_cat = card.cat;
                f.state.settings_query.clear();
                f.state.settings_show_recent = false;
            }
        }
    }
    if total == 0 {
        settings_recent_view(f, ui);
    }
}

/// 最近修改的设置项（倒序，最多 8 条）。
pub(super) fn settings_recent_view(f: &mut Frame<'_>, ui: &mut Ui) {
    let pal = f.pal;
    if f.state.settings_recent.is_empty() {
        ui.label(egui::RichText::new(t(("暂无最近修改的设置", "No recently changed settings"))).color(pal.text_dim));
        return;
    }
    section(ui, t(("最近修改", "Recently changed")), f.pal);
    for (key, _ts) in f.state.settings_recent.iter().rev() {
        // 稳定 key → 当前语言的名称；找不到就显示 key（只可能是
        // 版本升级删掉了该设置项，留个可辨认的痕迹比空白好）。
        let item = SETTINGS_CARDS
            .iter()
            .flat_map(|c| c.items.iter().map(move |it| (c, it)))
            .find(|(_, it)| it.key == *key);
        let (label, cat_title) = match item {
            Some((c, it)) => (t(it.label), t(c.title)),
            None => (key.as_str(), ""),
        };
        let (rect, _resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), egui::Sense::hover());
        ui.painter_at(rect).text(
            rect.min + vec2(8.0, 8.0),
            egui::Align2::LEFT_TOP,
            label,
            egui::FontId::proportional(pal.font_sm),
            pal.text,
        );
        ui.painter_at(rect).text(
            rect.max - vec2(8.0, 8.0),
            egui::Align2::RIGHT_BOTTOM,
            cat_title,
            egui::FontId::proportional(pal.font_xs),
            pal.text_dim,
        );
    }
}

/// 登记一条最近修改（去重 + 上限 8 条）。
pub(super) fn record_recent(f: &mut Frame<'_>, label: &str) {
    f.state.settings_recent.retain(|(l, _)| l != label);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    f.state.settings_recent.push((label.to_string(), now));
    if f.state.settings_recent.len() > 8 {
        f.state.settings_recent.remove(0);
    }
}

/// 分组标题。
pub(super) fn section(ui: &mut Ui, title: &str, pal: &Palette) {
    ui.add_space(6.0);
    ui.label(
        egui::RichText::new(title)
            .strong()
            // 15.0 裸值 == font_sm @scale1，但不随 font_scale 放大
            // （高字号下分组标题反而比正文小，倒挂）。
            .size(pal.font_sm)
            .color(ui.visuals().weak_text_color()),
    );
}

/// 一行「标签 + 说明 + 右侧控件」。
///
/// `control` 闭包在右侧区域里放控件，返回 `true` 表示值被改动。
/// 宽度分配交给 `ui.horizontal` + `with_layout(right_to_left)`，
/// 不再手算 `ctrl_w`（旧实现那处 `min(r.width() * 0.4)` 的钳制
/// 会在极窄面板里把标签挤没）。
///
/// # 顺序：**先放控件，再放标签列**
///
/// 这是「两者永不重叠」的关键。早前是「先标签列、后控件」，而控件在
/// `right_to_left` 里是**右对齐**的：一旦控件比剩下的空间宽（字体放大
/// 后必然发生），它就向左溢出、压到标签列身上——实测 1.5× 时字体缩放
/// 分段控件左缘 436.5，而标签列右缘 456.8，重叠 20pt。
///
/// 反过来先放控件：控件先占住自己需要的宽度，标签列随后拿到的
/// `available_width` 已是**净宽**，截断按净宽发生，两者不可能重叠。
/// 字体缩放到这个倍率以上时，设置行改为**上下堆叠**（标签在上、控件在下）。
///
/// # 为什么需要堆叠（用户反馈「字体调整太大导致布局错乱」）
///
/// 并排时标签列与控件列争同一条行宽（544pt）。5 段的分段控件在 1.3×
/// 时已占 300pt+，加上读数与间距后留给标签列的不足 160pt——描述只能
/// 显示 6 个字（虽然可悬停看全文，但一眼看不到重点）。堆叠后控件独占
/// 整行宽度，描述也不再被截断，代价只是卡片变高（内容区本来就可滚动）。
///
/// 阈值 1.25：1.15× 及以下并排仍然宽松（实测标签列 ≥ 240pt），
/// 从 1.3× 开始明显拥挤。
pub(super) const ROW_STACK_SCALE: f32 = 1.25;

/// 从 egui 样式反推当前字体缩放倍率。
///
/// `theme::apply` 把 `pal.font_md` 写进 `TextStyle::Body`，因此
/// 「Body 字号 / 基准字号」就是缩放倍率。取不到样式时按 1.0 处理
/// （退回并排布局，最坏只是大字号下拥挤一点，不会崩）。
pub(super) fn row_font_scale(ui: &Ui) -> f32 {
    let base = crate::theme::Palette::light().font_md;
    let cur = ui
        .style()
        .text_styles
        .get(&egui::TextStyle::Body)
        .map_or(base, |f| f.size);
    if base > 0.0 && cur.is_finite() { (cur / base).max(0.1) } else { 1.0 }
}

/// 标题与说明之间的行距（逻辑点）。
///
/// 与「行与行之间」的间距（`settings_card` 的 `item_spacing.y`）分开：
/// 标题和说明是一组，贴紧才读得成一句；行与行之间要松。
pub(super) const LABEL_DESC_GAP: f32 = 5.0;

/// 说明文字最多占几行。超出部分由 egui 省略（末行加「…」）。
///
/// 2 行是「读得完一句说明」与「行高不失控」的折中：设置项的描述通常
/// 一到两句，3 行以上多半是文案该精简了。悬停仍可看到完整描述。
pub(super) const DESC_MAX_ROWS: usize = 2;

/// 标题行高（逻辑点）。**与宽度无关**（标题单行 `truncate`），可精确预量。
pub(super) fn head_line_height(ui: &Ui, label: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(
            label.to_owned(),
            egui::TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
        )
        .rect
        .height()
}

/// 说明文字排版：按 `width` **折行**，最多 [`DESC_MAX_ROWS`] 行，超出加省略号。
///
/// # 为什么不用 `Label::truncate()` / `.wrap()`
///
/// - `truncate()` 只出**一行**，长说明被砍成半句（用户反馈「描述显示不全」）；
/// - `wrap()` 不限行数，超长说明能把一行设置撑到五六行高，整张卡片失去节奏。
///
/// 用 `LayoutJob` 的 `wrap.max_rows` 才是「折行 + 到上限加省略号」——
/// 与列表预览（`draw_text_preview`）同一套做法。
pub(super) fn desc_galley(ui: &Ui, desc: &str, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        desc,
        0.0,
        egui::TextFormat {
            font_id: egui::TextStyle::Small.resolve(ui.style()),
            color: ui.visuals().weak_text_color(),
            ..Default::default()
        },
    );
    job.wrap.max_width = width.max(1.0);
    job.wrap.max_rows = DESC_MAX_ROWS;
    job.wrap.break_anywhere = true;
    ui.painter().layout_job(job)
}

/// 一行设置项的布局常量：控件列的最小高度。
///
/// 标题行本身只有 ~20pt，而按钮类控件（快捷键录制、迷你按钮）有 24~28pt。
/// 容器按 24pt 起步，控件居中后不会向上/向下出界。
pub(super) const CTRL_MIN_H: f32 = 24.0;

/// 一行「标签 + 说明 + 右侧控件」。
///
/// # 布局（三块，全部用**显式矩形**定位）
///
/// ```text
/// ┌───────────────────────────────┬──────────────┐
/// │ 标题                          │   控件（右对齐、垂直居中）│
/// │ 说明（自动折行，最多 2 行…）   │              │
/// └───────────────────────────────┴──────────────┘
///   ← 标签列宽 = 可用宽 − 控件宽 − 间距 →
/// ```
///
/// # 为什么必须「先量行高、再按矩形摆放」
///
/// 早前是「控件先放、标签列后放」，而 egui 的横向布局**不会重排先放的
/// 控件**：控件放下时行高还只有它自己那么高，随后标签列把行撑高，控件
/// 仍停在行首 ⇒ 开关贴着标题行、输入框贴着说明行、分段控件又高半行，
/// 同一张卡片里三种对齐——观感就是「内容和按钮错位」。
///
/// 现在整行先 `allocate_exact_size` 占住，再把控件与标签列放进**显式
/// 矩形**（`new_child` 不推进游标），三者的几何完全由本函数决定：
/// - 控件：行右上角，在 `CTRL_MIN_H` 高的容器里垂直居中（与标题行同中心）；
/// - 标签列：行左上角，宽度 = 可用宽 − 控件实际宽 − 间距；
/// - 行高：标题行高 + 说明实际折行高度（说明折行后行会变高）。
///
/// # 为什么标签列宽度要减去控件宽
///
/// 说明按这个宽度折行——**量出来的行数就是画出来的行数**。若按整行宽
/// 量、按窄列画，说明会被挤成更多行而撑出卡片（用户反馈「描述跑到
/// 卡片外」的成因）。控件宽从 `min_rect` 取实际值，不靠猜。
pub(super) fn setting_row(
    ui: &mut Ui,
    label: Bi,
    desc: Bi,
    control: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    let label = t(label);
    let desc = t(desc);
    let mut changed = false;
    if row_font_scale(ui) > ROW_STACK_SCALE {
        // ---- 大字号：上下堆叠 ----
        ui.vertical(|ui| {
            ui.add(egui::Label::new(label));
            if !desc.is_empty() {
                let g = desc_galley(ui, desc, ui.available_width());
                let r = ui.add(egui::Label::new(g));
                r.on_hover_text(desc);
            }
            // CAUTION: 必须用 `ui.horizontal` 包一层。直接在纵向布局里
            // `with_layout(right_to_left)` 会让子 ui 拿到**剩余全部高度**
            // （滚动区里是上千点），`Align::Center` 随即把控件垂直居中到
            // 几百点之外——实测第一行控件被推到 y=632（行首在 y=54）。
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    changed = control(ui);
                });
            });
        });
        return changed;
    }

    let avail = ui.available_width().max(1.0);
    let head_h = head_line_height(ui, label);
    let ctrl_h = head_h.max(CTRL_MIN_H);
    // 1) 先占住整行（高度先按「标题行高」，说明折行后再补）。
    let (row, _) = ui.allocate_exact_size(vec2(avail, ctrl_h), Sense::empty());

    // 2) 控件：行右上角，从右往左排（右对齐）。
    //
    // 容器宽度 = 可用宽 − **标题实际宽度** − 间距：控件最多吃到这里，
    // 标题永远拿得下自己的字（长标题也不会被控件挤没）。早前用固定
    // 72% 上限，Info 行（右侧是长文本）被白白砍掉三成宽度。
    let gap = ui.spacing().item_spacing.x;
    let label_nat = ui
        .painter()
        .layout_no_wrap(
            label.to_owned(),
            egui::TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
        )
        .rect
        .width()
        .min(avail * 0.5);
    let ctrl_max = (avail - label_nat - gap).max(avail * 0.3);
    let mut ctrl_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(
                pos2(row.max.x - ctrl_max, row.min.y),
                pos2(row.max.x, row.min.y + ctrl_h),
            ))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    changed = control(&mut ctrl_ui);
    // 实际占用的宽度（开关 ~38、分段 ~250、输入框 ~230）。
    let ctrl_w = ctrl_ui.min_rect().width().min(ctrl_max);

    // 3) 标签列：行左上角。宽度先定，说明按这个宽度折行 ⇒ 量=画。
    let label_w = (avail - ctrl_w - gap).max(80.0);
    let desc_g = (!desc.is_empty()).then(|| desc_galley(ui, desc, label_w));
    let desc_h = desc_g.as_ref().map_or(0.0, |g| g.rect.height());
    let label_h = head_h + if desc_g.is_some() { LABEL_DESC_GAP + desc_h } else { 0.0 };
    // 说明折行后行更高：补足游标，下一行才不会压上来。
    if label_h > ctrl_h {
        ui.add_space(label_h - ctrl_h);
    }
    let mut lab = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_size(row.min, vec2(label_w, label_h)))
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    lab.spacing_mut().item_spacing.y = LABEL_DESC_GAP;
    // 标题单行：`truncate()` 让它在窄列里省略而不是换行（换行会把
    // 标题和说明挤成一片，读不出层次）。
    lab.add(egui::Label::new(label).truncate());
    if let Some(g) = desc_g {
        // 说明已经按列宽排好版（折行 + 省略），直接画 galley。
        let r = lab.add(egui::Label::new(g));
        // 折行到 2 行仍可能看不全，悬停给出完整描述。
        r.on_hover_text(desc);
    }
    changed
}
