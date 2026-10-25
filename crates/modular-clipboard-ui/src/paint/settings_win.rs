//! 设置窗口外壳：布局常量、导航、搜索、许可证 / 更新弹窗。
//!
//! 只负责「窗口级」元素；具体设置项在 `settings_items` 一族。

use egui::Rect;
use egui::pos2;
use egui::vec2;
use modular_clipboard_core::i18n::t;

use super::{Frame, Op, SettingsCat};

use super::topbar::draw_popup_shadow;
use super::topbar::overlay_scrim;
use super::settings_items::settings_content;
use super::mcp_overlay::draw_recording_overlay;
use super::mcp_overlay::settings_mcp_overlay;
use super::list::SCROLLBAR_GAP;
use super::list::SCROLLBAR_RESERVE;
use super::shadow::fade_color;

/// 设置界面的整帧内容（画在**独立窗口**里）。
///
/// # 与旧「覆盖层」版的差别
///
/// 旧版是在主窗口上盖一块居中面板 + 一层遮罩。这里改成独立窗口后：
///
/// - **没有遮罩**——独立窗口天然就是模态的，不需要额外拦点击；
/// - **不居中**——窗口本身就是面板，按客户区铺满即可；
/// - **没有外框**——它已经是独立窗口，再画一层卡片框会显得
///   「窗口里还有一块面板」。
///
/// `area` 是**本窗口自己的客户区**，与主窗口无关。
/// 设置界面布局常量：左侧导航列宽（逻辑点）。
///
/// `settingswin` 的拖动区用这两个值挖掉左列，二者必须保持一致。
pub const SETTINGS_NAV_W: f32 = 180.0;
/// 设置界面外缘留白（逻辑点）。
pub const SETTINGS_MARGIN: f32 = 18.0;
/// 左右列间距（逻辑点）。
///
/// 2026-10-25 改版：14 → 22。悬浮卡带 22pt 主题色光晕（深色）/ 三层
/// 投影（浅色），间距小于光晕半径时两列的光晕连成一片，左侧「涨」
/// 进内容区（用户反馈「内容显示区域被左导航栏吃掉一部分」）。
pub const SETTINGS_COL_GAP: f32 = 22.0;
/// 侧栏卡内边距（搜索框 / 导航项 / 底脚共用的内缩）。
const NAV_PAD: f32 = 8.0;
/// 侧栏底脚高度（发丝线 + 应用名）。
const SETTINGS_NAV_FOOTER_H: f32 = 30.0;

/// 设置界面：左侧搜索框 + 圆角导航块，右侧卡片分组。
///
/// `area` 是**本窗口自己的客户区**，与主窗口无关。
pub fn draw_settings(f: &mut Frame<'_>) {
    let pal = f.pal;
    let area = f.area;

    // IMPORTANT: 清空必须**早于**下面所有提前 return。
    //
    // 若只在「画成了」的路径里清，那么某一帧尺寸异常跳过绘制时，
    // 上一帧的热区就留在表里——自动化拿着它去点，点的是不存在的位置。
    f.hit_clear();

    // NOTE: **首帧的 `area` 可能荒谬**（实测 6666x6666）。
    //
    // `ui.max_rect()` 取的是子 `Ui` 的可用矩形，而首帧 `screen_rect`
    // 尚未由 `egui_input` 正确设置（`RawInput.screen_rect` 是默认值）。
    // 此时按 `area` 排布的面板会被算到屏幕外，**看起来像设置界面没打开**。
    //
    // 显式拒绝明显不合理的尺寸：不画任何东西，等下一帧。
    let reasonable = area.width() <= 4000.0 && area.height() <= 4000.0;
    if !reasonable {
        tracing::debug!(area = ?area, "客户区尺寸异常，本帧跳过设置界面");
        return;
    }

    // 铺满客户区：外框 + 背景。独立窗口不再画卡片框，
    // 但要有底色，否则透明区域会透出桌面。
    f.ui.painter_at(area).rect_filled(area, 0.0, pal.bg);

    let m = SETTINGS_MARGIN;
    let left_w = SETTINGS_NAV_W;

    // ---- 左列：一整块悬浮侧栏卡（搜索 + 导航 + 应用名锚点）----
    //
    // 2026-10-25 改版（HTML 原型定稿 A 方案）：左导航不再是「裸胶囊列
    // 直接落在窗口底上 + 一块圆角底」，而是与右侧内容卡**同一视觉
    // 语言**的悬浮卡——surface 底 + `radius_xl` + 分主题投影/围绕
    // （浅色三层黑影 / 深色主题色环+光晕，见
    // [`crate::paint::shadow::draw_settings_card_shadow`]）。矩形静态
    // 已知（铺满左列全高）：先铺底，再画内部，晕带最后补（环带整体
    // 位于卡片矩形之外，盖不到导航项）。
    let nav_card = Rect::from_min_max(
        pos2(area.min.x + m, area.min.y + m),
        pos2(area.min.x + m + left_w, area.max.y - m),
    );
    // 必须用**视口级** painter 画底与晕带：`painter_at(nav_card)` 会把
    // 裁剪区缩到卡片矩形，外扩的晕带整个被裁掉。
    //
    // CAUTION: `Painter` 持有 `&Ui` 的借用——铺底、画项、补晕带三段必须
    // **各自重新取**（`f.ui.painter()` 只是克隆内部 Arc，成本可忽略），
    // 否则第一个 `vp` 会把 `f`  immutable 借住，后面的
    // `draw_settings_search(f, …)` 全部撞 E0502。
    {
        let vp = f.ui.painter();
        // 侧栏卡矩形静态已知（铺满左列全高）⇒ 晕环可以直接先画：
        // 外扩矩形盖住卡片内部的部分随后被不透明填充遮住。
        vp.add(super::shadow::settings_halo_shape(
            nav_card,
            pal.radius_xl,
            pal,
        ));
        vp.rect_filled(nav_card, pal.radius_xl, pal.surface);
        // 浅色：发丝边；深色：边由主题色环表达（`draw_settings_card_edge`
        // 里画，内容之后），这里不描边。
        if !pal.is_dark {
            vp.rect_stroke(
                nav_card,
                pal.radius_xl,
                egui::Stroke::new(1.0, pal.border_subtle),
                egui::StrokeKind::Inside,
            );
        }
    }

    // 搜索框（卡内顶部，通宽减内边距）。
    let nav_inner = nav_card.shrink(NAV_PAD);
    // 高度随字号缩放（`control_height` 已含字体缩放）。
    let search_h = pal.control_height * 1.21;
    let search_rect = Rect::from_min_size(nav_inner.min, vec2(nav_inner.width(), search_h));
    draw_settings_search(f, search_rect);

    // 导航列表：搜索框与底脚之间。
    let nav_top = search_rect.max.y + 10.0;
    let nav_bottom = nav_inner.max.y - SETTINGS_NAV_FOOTER_H;
    if nav_bottom > nav_top + 8.0 {
        let nav_rect = Rect::from_min_max(
            pos2(nav_inner.min.x, nav_top),
            pos2(nav_inner.max.x, nav_bottom),
        );
        draw_settings_nav(f, nav_rect);
    }

    // 底脚：发丝线 + 应用名，给侧栏一个视觉锚点（否则卡片下方一大片
    // 空白读作「没画完」）。
    draw_settings_nav_footer(f, nav_card);

    // 边缘装饰（深色主题色环 + 受光高光）：画在填充之上、内容之后。
    //
    // CAUTION: `Ui::painter()` 返回的**已经是** `&Painter`（egui 的签名如
    // 此），再写 `&vp` 是「引用的引用」，会被 clippy 的 needless_borrow
    // 命中。绑到局部变量只是为了不让借用与下面的 `f.ui` 可变借用打架。
    let vp = f.ui.painter();
    super::shadow::draw_settings_card_edge(vp, nav_card, pal.radius_xl, pal);

    // ---- 右列页头行：主标签 + 关闭按钮，都与搜索框同线 ----
    let right_x0 = nav_card.max.x + SETTINGS_COL_GAP;
    let right = Rect::from_min_max(
        pos2(right_x0, area.min.y + m),
        area.max - vec2(m, m),
    );
    if right.width() <= 8.0 || right.height() <= 8.0 {
        return;
    }

    // 2026-10-25 用户反馈：「关闭窗口按钮与搜索框平齐，主标签也和
    // 搜索框平齐在同一条线上，去除主标签描述」。三者共用搜索框的
    // 垂直中心线：
    // - 主标签 = 当前分类名（与导航选中项同一份标题源，加粗），在左；
    // - 关闭按钮在右，垂直中心对齐；
    // - 描述行取消——分类名自解释，副标题白占一行还把内容区往下推。
    //
    // CAUTION: 主标签落在顶部拖动带（44pt）内是无害的——它只是文字，
    // 不注册交互；关闭按钮有 `close_zone_w`(40) 挖洞，点得动。
    let head_cy = search_rect.center().y;
    f.ui.painter().text(
        pos2(right.min.x, head_cy),
        egui::Align2::LEFT_CENTER,
        t(f.state.settings_cat.title()),
        crate::theme::emphasis(pal.font_md, f.scale),
        pal.text_bright,
    );

    // 右上角关闭按钮（独立于内容区，不随滚动）：垂直中心对齐搜索框。
    let close_r = Rect::from_center_size(
        pos2(right.max.x - 15.0, head_cy),
        vec2(30.0, 30.0),
    );
    draw_settings_close(f, close_r);

    // 内容区：页头行下方，可滚动。
    //
    // 视口用**右列全宽**——滚动条让位由**卡片**内缩完成（见下面
    // `show` 闭包里的 `set_width`），不是缩视口：egui 的浮动滚动条
    // 画在视口右缘**之上**，缩视口只会把滚动条一起左移，照样压住
    // 卡片右缘（2026-10-25 用户反馈「滚动条和卡片边上粘在一起」，
    // 旧代码的让出量因此落在了滚动条与窗口边之间——看不见的地方）。
    // 与列表行 `row_w` 同一处理，见 `draw_history_body`。
    //
    // 顶缘 = 搜索框底 / 关闭按钮底 的较大者 + 呼吸位：字号放大时任一
    // 元素变高都不会被内容区盖上。
    let head_bottom = search_rect.max.y.max(close_r.max.y);
    let content = Rect::from_min_max(
        pos2(right.min.x, head_bottom + 10.0),
        pos2(right.max.x, right.max.y),
    );
    if content.height() <= 8.0 {
        return;
    }
    let mut ui = f.ui.new_child(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique("settings"))
            .max_rect(content)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    ui.spacing_mut().item_spacing = vec2(0.0, 6.0);
    egui::ScrollArea::vertical()
        .id_salt("settings_scroll")
        .auto_shrink([false, false])
        .show(&mut ui, |ui| {
            // 卡片右缘让出滚动条热区 + 呼吸空间（逻辑点）。
            //
            // CAUTION: 让的是**内容宽度**（`set_width` 把内层 Ui 的
            // min/max 宽一起钉住），不是视口宽度。egui 0.36+ 默认
            // `ScrollStyle::floating`：滚动条不占布局宽度，浮在
            // `[视口.max.x - 10, 视口.max.x]` 上。缩视口 ⇒ 滚动条
            // 跟着左移，与卡片右缘零距相贴甚至压住开关/分段最后一段
            // （用户反馈「滑条展开后容易挡到按钮」）。
            //
            // 无条件让出（不像列表行那样仅可滚动时让）：卡片宽度不随
            // 「本分类能否滚动」跳变，切分类时控件不会横向抖动。
            let card_w = (content.width() - SCROLLBAR_RESERVE - SCROLLBAR_GAP).max(160.0);
            ui.set_width(card_w);
            settings_content(f, ui);
        });

    // 热键录制覆盖层（限制在本窗口客户区内，不遮桌面）。
    //
    // 与 MCP 覆盖层同一套路：设置内容照常画（它是 GPU 模糊的底），
    // 分割标记之后的本面板保持清晰。**整窗只认它**——录制中再弹
    // MCP 覆盖层只会叠罗汉，故录制态直接跳过 MCP。
    if f.state.hotkey_recording.is_some() {
        push_blur_split_marker(f, area);
        draw_recording_overlay(f, area);
        return;
    }
    // 录制结束 ⇒ 清掉入场补间条目：`fade_in_once` 判「首次出现」，
    // 不清的话只有程序生命周期内**第一次**录制有淡入动效，第二次起
    // 硬现（与弹窗 `just_opened` 清 `("popup_in", kind)` 同一个道理）。
    crate::anim::clear(f.ui.ctx(), egui::Id::unique("recording_overlay_in"));

    // MCP 调用设置覆盖层（限制在本窗口客户区内，不遮桌面）。
    //
    // 画在内容区**之后**：视觉上盖住卡片，交互区也后注册 → 命中优先。
    if f.state.mcp_overlay_open {
        // 自研模糊的分割标记：渲染层据此把界面拆成「覆盖层之前的内容
        // （画到离屏图并卷积成模糊底图）」与「覆盖层本身（保持清晰）」。
        push_blur_split_marker(f, area);
        settings_mcp_overlay(f, area);
    }

    // 开源协议悬浮面板（关于页「开源协议」行点击后）。
    //
    // 画在**最外层**、在 MCP 覆盖层之后：协议是独立的一层，要盖住
    // 一切内容（用户点开它就是想看正文，不是看背后的设置项）。
    // 不走 `push_blur_split_marker`——正文必须清晰，不需要模糊底图。
    if f.state.license_popup {
        draw_license_popup(f, area);
    }

    // 「发现新版本」悬浮面板。
    //
    // 检查到更新且本次还没弹过 ⇒ 自动弹一次。`update_popup_shown`
    // 防止状态停在 `Outdated` 时每帧重开（用户点掉立刻又弹回来）；
    // 点「检查更新」按钮时清掉它，让下次检查结果重新触发。
    if matches!(
        crate::about::current_state(),
        crate::about::UpdateState::Outdated { .. }
    ) && !f.state.update_popup_shown
    {
        f.state.update_popup_shown = true;
        f.state.update_popup = true;
    }
    if f.state.update_popup {
        draw_update_popup(f, area);
    }

    // 自更新已下载完毕、替换脚本已启动 ⇒ 退出本进程让它接手。
    //
    // 这是接力的最后一棒：脚本在等本进程退出才能动 exe
    // （Windows 对运行中的映像有文件锁），不退就会卡在等待超时。
    if matches!(
        crate::about::current_state(),
        crate::about::UpdateState::Applying
    ) {
        f.state.push(Op::Quit);
    }
}

/// 开源协议悬浮面板：半透明遮罩 + 居中卡片 + 可滚动正文。
///
/// # 为什么要「悬浮界面」而不是开浏览器
///
/// 用户点「查看协议」的意图是**看**正文，不是被跳走。开浏览器会
/// 丢掉当前上下文（设置页），离线时根本打不开。内嵌正文永远可用
/// （`about::LICENSE_TEXT` 是编译期纳入的，见该常量的说明）。
///
/// # 正文用虚拟滚动
///
/// GPL 全文 ~35KB / 上千行。一次画完会让布局与绘制各卡一帧（egui
/// 要排版所有字形）；`show_rows` 只画可视区那几十行，滚多久都是
/// 常数开销。
pub(super) fn draw_license_popup(f: &mut Frame<'_>, area: Rect) {
    let pal = f.pal;
    let mut painter = f.ui.painter_at(area);

    // 遮罩：压暗背后内容，注意力落到面板上。
    painter.rect_filled(area, 0.0, overlay_scrim(pal));

    // 面板：居中，**占窗口约 2/3**——协议是长时间阅读的文本，
    // 满屏会压迫背后的设置页（用户 2026-10-22 要求「缩小一点」）。
    // 下限 280×220：小窗口下也要保证标题栏 + 一两行正文看得见。
    let panel = egui::Rect::from_center_size(
        area.center(),
        egui::vec2(
            (area.width() * 0.68).max(280.0),
            (area.height() * 0.76).max(220.0),
        ),
    );
    // 投影与项目弹窗同一套（`draw_popup_shadow`）：投影是实心矩形外扩，
    // 所以下面的面板**必须有不透明底**（`pal.surface` 已是），否则会
    // 透出黑边——弹窗侧踩过的坑（见 `POPUP_SHADOW_STRENGTH` 的说明）。
    draw_popup_shadow(&painter, panel, pal);

    // 点击**面板外任意空白**即可退出（2026-10-22 用户要求）。
    //
    // 面板外的区域拆成上/下/左/右四块矩形注册点击：不能只对整个
    // `area` 注册——面板内的滚动区、关闭按钮都要命中，而 egui 的
    // hit-test 是「后注册的在上层」，整块 area 后注册会把面板内的
    // 交互盖掉（点关闭按钮变成关面板，点正文变成关面板）。
    //
    // 四块矩形互不重叠、也不与 `panel` 重叠，注册顺序无关紧要。
    let mask = [
        // 上
        egui::Rect::from_min_max(area.min, egui::pos2(area.max.x, panel.min.y)),
        // 下
        egui::Rect::from_min_max(egui::pos2(area.min.x, panel.max.y), area.max),
        // 左
        egui::Rect::from_min_max(
            egui::pos2(area.min.x, panel.min.y),
            egui::pos2(panel.min.x, panel.max.y),
        ),
        // 右
        egui::Rect::from_min_max(
            egui::pos2(panel.max.x, panel.min.y),
            egui::pos2(area.max.x, panel.max.y),
        ),
    ];
    for (i, r) in mask.iter().enumerate() {
        if r.width() <= 0.5 || r.height() <= 0.5 {
            continue;
        }
        let resp = f
            .ui
            .interact(*r, egui::Id::unique(("license_mask", i)), egui::Sense::click());
        if resp.clicked() {
            f.state.license_popup = false;
        }
    }

    painter.rect_filled(panel, pal.radius_lg, pal.surface);
    painter.rect_stroke(
        panel,
        pal.radius_lg,
        egui::Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );

    // 标题栏：用 `surface_raised` 与正文区分层。
    //
    // # 圆角怎么处理
    //
    // 标题栏是直角矩形，直接填会把面板**顶部的圆角盖成方角**。egui 没有
    // 「只圆上两角」的矩形，所以用 clip：把裁剪区限到标题栏内、用面板的
    // 圆角半径画一次，结果就是「圆角矩形的上半部分」——顶部两角圆、
    // 下沿是直的，正好是标题栏想要的形状。画完必须把 clip 复原，
    // 否则后面正文会被裁掉。
    let title_h = 38.0;
    let title_r =
        egui::Rect::from_min_max(panel.min, egui::pos2(panel.max.x, panel.min.y + title_h));
    let old_clip = painter.clip_rect();
    painter.set_clip_rect(title_r.intersect(old_clip));
    painter.rect_filled(panel, pal.radius_lg, pal.surface_raised);
    painter.set_clip_rect(old_clip);
    // 标题栏下沿一条细分隔线，把「头」与「正文」切开。
    painter.line_segment(
        [
            egui::pos2(title_r.min.x, title_r.max.y),
            egui::pos2(title_r.max.x, title_r.max.y),
        ],
        egui::Stroke::new(1.0, pal.border),
    );
    painter.text(
        egui::pos2(title_r.min.x + pal.space_lg, title_r.center().y),
        egui::Align2::LEFT_CENTER,
        t("License"),
        egui::FontId::proportional(pal.font_md),
        pal.text,
    );
    // 副标题：协议名（小一号、弱化），让用户知道看的是哪一份。
    painter.text(
        egui::pos2(title_r.min.x + 88.0, title_r.center().y),
        egui::Align2::LEFT_CENTER,
        "GNU GPL v3.0",
        egui::FontId::proportional(pal.font_sm),
        pal.text_dim,
    );

    // 关闭按钮（右上角）。几何与图片编辑窗同一思路：绘制与交互共用
    // 同一个矩形，别各算一份（两处漂移过一次，见 imgedit 的说明）。
    let close_r = egui::Rect::from_center_size(
        egui::pos2(title_r.max.x - 22.0, title_r.center().y),
        egui::vec2(26.0, 26.0),
    );
    let close_resp =
        f.ui.interact(close_r, egui::Id::unique("license_close"), egui::Sense::click());
    if close_resp.hovered() {
        // 悬停底：小图标按钮只有变色反馈太弱，加一层有可点的实体感
        // （与列表行操作按钮同一处理）。
        painter.rect_filled(close_r, pal.radius_sm, pal.row_hover);
    }
    crate::icons::Icon::Close.paint(
        &f.ui.painter_at(close_r),
        egui::Rect::from_center_size(close_r.center(), close_r.size() * 0.72),
        if close_resp.hovered() {
            pal.text_bright
        } else {
            pal.text_dim
        },
    );
    // CAUTION: `clicked()` 必须在 `on_hover_text` 之前取——后者按值
    // 消耗 `Response`（与列表行操作按钮同一个坑）。
    let close_clicked = close_resp.clicked();
    close_resp.on_hover_text(t("Close"));
    if close_clicked {
        f.state.license_popup = false;
    }

    // Esc 也关：与所有弹层（搜索面板、图片编辑窗）一致，「Esc = 退出
    // 当前层」是用户跨窗口的通用预期。
    if f.ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        f.state.license_popup = false;
    }

    // 正文：虚拟滚动（只排版可视行）。用 `surface_variant` 做底，
    // 与标题栏的 `surface_raised` 形成三层（面板 → 头 → 正文）层次。
    //
    // 内边距压到最小（8/6pt）：用户要「内容渲染满显示区域」——早前
    // 12+10 的双层缩进把正文挤到中间一窄条，法律条文的行长本身就长，
    // 再被外边距吃掉宽度就更难读了。
    let pad = pal.space_sm;
    let body = egui::Rect::from_min_max(
        egui::pos2(panel.min.x + pad, title_r.max.y + pad),
        egui::pos2(panel.max.x - pad, panel.max.y - pad),
    );
    if body.width() <= 8.0 || body.height() <= 8.0 {
        return;
    }
    painter.rect_filled(body, pal.radius_md, pal.surface_variant);
    let inner = body.shrink(pal.space_xs);
    let mut ui = f.ui.new_child(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique("license_body"))
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    let lines: Vec<&str> = crate::about::LICENSE_TEXT.lines().collect();
    // 等宽字体：协议是**法律文本**，缩进与对齐有含义（条款编号、
    // 示例代码都靠缩进），比例字体会让它们对不齐。
    // 字号取 `font_sm` 而不是 `font_xs`：法律条文要长时间阅读，太小
    // 看着累（`font_xs` 适合 meta 小字，不适合正文）。
    let font = egui::FontId::monospace(pal.font_sm);
    let line_h = font.size * 1.45;
    egui::ScrollArea::vertical()
        .id_salt("license_scroll")
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
        .show_rows(&mut ui, line_h, lines.len(), |ui, range| {
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            for i in range {
                // 空行给一行的空白（协议里的段落分隔），否则挤成一坨。
                if lines[i].trim().is_empty() {
                    ui.add_space(line_h * 0.5);
                    continue;
                }
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(lines[i]).font(font.clone()).color(pal.text),
                    )
                    .wrap(),
                );
            }
        });
}

/// 「发现新版本」悬浮面板：版本号 + 更新内容预览 + 取消/更新按钮。
///
/// # 为什么是「打开下载页」而不是应用内自更新
///
/// 应用内替换自己的 exe 要处理：下载到临时目录、校验、备份旧文件、
/// 提权（`Program Files` 下写不进去）、重启换文件——每一步都能把
/// 用户的安装搞坏，而收益只是省一次「下载-双击安装」。桌面工具的
/// 通行做法是把用户带到发布页（内容、校验和、变更说明都在那儿）。
/// 所以「更新」按钮的语义是**去下载**，不是「后台换掉自己」。
///
/// # 没有 release 详情时的降级
///
/// `release` 为 `None`（网络不通 / curl 不可用 / 仓库还没有 release）
/// 时正文显示「暂无更新说明」，按钮降级成「打开仓库」——功能不假死，
/// 也不假装有更新说明。
pub(super) fn draw_update_popup(f: &mut Frame<'_>, area: Rect) {
    let pal = f.pal;
    // 状态被重新检查清空（或从未有过更新）时整层不画。
    let (local, remote, notes, url, download_url) = match crate::about::current_state() {
        crate::about::UpdateState::Outdated {
            local,
            remote,
            release,
        } => match release {
            Some(r) => (local, remote, r.notes, r.url, r.download_url),
            None => (
                local,
                remote,
                String::new(),
                crate::about::REPO_URL.to_string(),
                None,
            ),
        },
        _ => return,
    };

    let mut painter = f.ui.painter_at(area);
    painter.rect_filled(area, 0.0, overlay_scrim(pal));

    // 面板：比协议面板小一号——它是「一个决定」而不是「一段长文」，
    // 太大会让背后设置页的上下文丢失（用户 2026-10-22 要求「缩小」）。
    let panel = egui::Rect::from_center_size(
        area.center(),
        egui::vec2(
            (area.width() * 0.46).clamp(320.0, 520.0),
            (area.height() * 0.56).clamp(260.0, 420.0),
        ),
    );
    draw_popup_shadow(&painter, panel, pal);
    painter.rect_filled(panel, pal.radius_lg, pal.surface);
    painter.rect_stroke(
        panel,
        pal.radius_lg,
        egui::Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );

    // 点击面板外任意空白退出（与协议面板同一交互）。
    let mask = [
        egui::Rect::from_min_max(area.min, egui::pos2(area.max.x, panel.min.y)),
        egui::Rect::from_min_max(egui::pos2(area.min.x, panel.max.y), area.max),
        egui::Rect::from_min_max(
            egui::pos2(area.min.x, panel.min.y),
            egui::pos2(panel.min.x, panel.max.y),
        ),
        egui::Rect::from_min_max(
            egui::pos2(panel.max.x, panel.min.y),
            egui::pos2(area.max.x, panel.max.y),
        ),
    ];
    for (i, r) in mask.iter().enumerate() {
        if r.width() <= 0.5 || r.height() <= 0.5 {
            continue;
        }
        let resp = f.ui.interact(
            *r,
            egui::Id::unique(("update_mask", i)),
            egui::Sense::click(),
        );
        if resp.clicked() {
            f.state.update_popup = false;
        }
    }

    // 标题栏（clip 画圆角顶部，同协议面板）。
    let title_h = 38.0;
    let title_r =
        egui::Rect::from_min_max(panel.min, egui::pos2(panel.max.x, panel.min.y + title_h));
    let old_clip = painter.clip_rect();
    painter.set_clip_rect(title_r.intersect(old_clip));
    painter.rect_filled(panel, pal.radius_lg, pal.surface_raised);
    painter.set_clip_rect(old_clip);
    painter.line_segment(
        [
            egui::pos2(title_r.min.x, title_r.max.y),
            egui::pos2(title_r.max.x, title_r.max.y),
        ],
        egui::Stroke::new(1.0, pal.border),
    );
    painter.text(
        egui::pos2(title_r.min.x + 16.0, title_r.center().y),
        egui::Align2::LEFT_CENTER,
        t("Update available"),
        egui::FontId::proportional(pal.font_md),
        pal.text,
    );

    // 关闭按钮（右上角）。
    let close_r = egui::Rect::from_center_size(
        egui::pos2(title_r.max.x - 22.0, title_r.center().y),
        egui::vec2(26.0, 26.0),
    );
    let close_resp = f
        .ui
        .interact(close_r, egui::Id::unique("update_close"), egui::Sense::click());
    if close_resp.hovered() {
        painter.rect_filled(close_r, pal.radius_sm, pal.row_hover);
    }
    crate::icons::Icon::Close.paint(
        &f.ui.painter_at(close_r),
        egui::Rect::from_center_size(close_r.center(), close_r.size() * 0.72),
        if close_resp.hovered() {
            pal.text_bright
        } else {
            pal.text_dim
        },
    );
    let close_clicked = close_resp.clicked();
    close_resp.on_hover_text(t("Close"));
    if close_clicked || f.ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        f.state.update_popup = false;
    }

    // ---- 版本号 ----
    let mut y = title_r.max.y + 16.0;
    let x0 = panel.min.x + pal.space_lg;
    // 「本地 → 远程」是用户判断「跳多远」的唯一信息，用主字号 + 强调色。
    painter.text(
        egui::pos2(x0, y),
        egui::Align2::LEFT_TOP,
        format!("{local}  →  v{remote}"),
        egui::FontId::proportional(pal.font_md),
        pal.accent,
    );
    y += pal.font_md + 14.0;

    // ---- 更新内容预览 ----
    painter.text(
        egui::pos2(x0, y),
        egui::Align2::LEFT_TOP,
        t("What's new"),
        egui::FontId::proportional(pal.font_sm),
        pal.text_dim,
    );
    y += pal.font_sm + 8.0;

    // 按钮区固定在底部，预览区拿剩下的高度。
    let btn_h = 34.0;
    let btn_gap = 10.0;
    let btn_y = panel.max.y - btn_h - 18.0;
    let notes_r = egui::Rect::from_min_max(
        egui::pos2(x0, y),
        egui::pos2(panel.max.x - 16.0, btn_y - 12.0),
    );
    if notes_r.height() > 16.0 {
        painter.rect_filled(notes_r, pal.radius_md, pal.surface_variant);
        if notes.trim().is_empty() {
            // 没有 release 详情：明说，而不是留一块空白让用户猜。
            painter.text(
                notes_r.center(),
                egui::Align2::CENTER_CENTER,
                t("No release notes available"),
                egui::FontId::proportional(pal.font_sm),
                pal.text_dim,
            );
        } else {
            let inner = notes_r.shrink(pal.space_sm);
            let mut ui = f.ui.new_child(
                egui::UiBuilder::new()
                    .scope_id(egui::Id::unique("update_notes"))
                    .max_rect(inner)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            // release body 是 Markdown **源文**：原样按等宽显示，
            // 不做渲染（渲染整套 Markdown 要一套解析器，而这些符号
            // 用户本来也看得懂；真正要渲染的地方是发布页）。
            let font = egui::FontId::monospace(pal.font_sm);
            let line_h = font.size * 1.45;
            let lines: Vec<&str> = notes.lines().collect();
            egui::ScrollArea::vertical()
                .id_salt("update_notes_scroll")
                .auto_shrink([false, false])
                .show_rows(&mut ui, line_h, lines.len(), |ui, range| {
                    ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                    for i in range {
                        if lines[i].trim().is_empty() {
                            ui.add_space(line_h * 0.5);
                            continue;
                        }
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(lines[i]).font(font.clone()).color(pal.text),
                            )
                            .wrap(),
                        );
                    }
                });
        }
    }

    // ---- 底部按钮：取消 / 更新（圆角，右对齐）----
    //
    // 「更新」是主按钮（强调色填充），「取消」是次按钮（描边）。
    // 两个都右对齐、更新在最右——这是用户预期的「下一步」位置。
    let (m_left, m_right) = (panel.min.x + pal.space_lg, panel.max.x - pal.space_lg);
    let cancel_w = 88.0;
    let update_w = 108.0;
    let cancel_r = egui::Rect::from_min_size(
        egui::pos2(m_right - update_w - btn_gap - cancel_w, btn_y),
        egui::vec2(cancel_w, btn_h),
    );
    let update_r = egui::Rect::from_min_size(
        egui::pos2(m_right - update_w, btn_y),
        egui::vec2(update_w, btn_h),
    );

    let cancel_resp = f
        .ui
        .interact(cancel_r, egui::Id::unique("update_cancel"), egui::Sense::click());
    painter.rect_filled(
        cancel_r,
        pal.radius_md,
        if cancel_resp.hovered() {
            pal.row_hover
        } else {
            pal.surface_variant
        },
    );
    painter.rect_stroke(
        cancel_r,
        pal.radius_md,
        egui::Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );
    painter.text(
        cancel_r.center(),
        egui::Align2::CENTER_CENTER,
        t("Cancel"),
        egui::FontId::proportional(pal.font_sm),
        pal.text,
    );
    if cancel_resp.clicked() {
        f.state.update_popup = false;
    }

    let update_resp = f
        .ui
        .interact(update_r, egui::Id::unique("update_confirm"), egui::Sense::click());
    painter.rect_filled(
        update_r,
        pal.radius_md,
        if update_resp.hovered() {
            pal.accent_hover
        } else {
            pal.accent
        },
    );
    // 无 release 详情时按钮语义是「打开仓库」（那里同样能下载）。
    let label = if notes.trim().is_empty() {
        t("Open repo")
    } else {
        t("Update")
    };
    painter.text(
        update_r.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(pal.font_sm),
        pal.text_on_accent,
    );
    if update_resp.clicked() {
        // 去下载页：用户在那儿能看到完整变更说明、校验和、各平台包。
        //
        // CAUTION: 直接 `open_external` 而不是 `Op::OpenLink`——后者
        // 只收 `&'static str`（静态表里的链接），而下载页 URL 是**运行时**
        // 从 GitHub API 拿到的 `String`。开浏览器不改任何共享状态
        // （就是 `platform::open_external` 那一下），不必绕道状态机
        // 去改 `Op` 的签名影响所有既有调用点。
        if download_url.is_some() {
            // 应用内自更新：下载 + 起替换脚本，随后本进程退出
            // （脚本等它退出后才动 exe，见 about::build_apply_script）。
            crate::about::start_self_update();
        } else {
            // 该 release 没挂 exe 资产：只能打开下载页，让用户自己取。
            if let Err(e) = modular_clipboard_platform::open_external(&url) {
                tracing::warn!(%e, %url, "打开下载页失败");
            }
        }
        f.state.update_popup = false;
    }
    let _ = m_left;
}

/// 放置一个「模糊分割」标记。
///
/// 回调体是**空操作**——渲染层不执行它，只取它的**位置**作为
/// 「此点之后的内容保持清晰」的分界。见 `renderer::tessellate_into` 的
/// 返回值说明。
///
/// # 为什么用 `PaintCallback`
///
/// egui 的 `ClippedPrimitive` 不携带「第几段」信息，而 `PaintCallback`
/// 是 egui 自带的「此处插入自定义绘制」机制：它在 tessellation 里必然
/// 保留为一个独立 primitive（不会与相邻网格合批），天然适合当分段信号。
/// 自造一套「魔法颜色矩形」之类协议反而更脆弱。
pub(super) fn push_blur_split_marker(f: &mut Frame<'_>, area: Rect) {
    f.ui.painter_at(area).add(egui::Shape::Callback(
        egui::epaint::PaintCallback {
            rect: area,
            callback: std::sync::Arc::new(|_: &egui::epaint::ClippedPrimitive| {}),
        },
    ));
}

/// 右上角关闭按钮（绝对定位，不随滚动）。
///
/// 2026-10-25 全圆角改版：悬停底从 `radius_sm` 改**圆形**
/// （半径 = 按钮边一半）——图标按钮统一成圆形是「全圆角」的收口。
pub(super) fn draw_settings_close(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    let btn = f.ui.scope_builder(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique("settings_close"))
            .max_rect(r)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |u| u.add_sized(r.size(), egui::Button::new("").frame(false)),
    );
    let btn = btn.inner;
    if btn.hovered() {
        f.ui
            .painter_at(r)
            .rect_filled(r, r.height() * 0.5, pal.row_hover);
    }
    crate::icons::Icon::Close.paint(
        &f.ui.painter_at(r),
        Rect::from_center_size(r.center(), vec2(14.0, 14.0)),
        if btn.hovered() { pal.text_bright } else { pal.text_dim },
    );
    f.hit_insert("settings_close", r);
    if btn.clicked() {
        f.state.show_settings = false;
    }
}

/// 左侧搜索框：胶囊输入框（表面变体底 + 发丝边），右侧放大镜。
///
/// 2026-10-25 全圆角改版：圆角从 `radius_md`(6) 改为**胶囊**
/// （半径 = 框高一半），并补一层 `border_subtle` 发丝边——悬浮侧栏
/// 卡里没有可见边界的输入框会读作「一块灰疤」。
///
/// 定稿交互：内容为空且聚焦时显示「最近修改」；输入内容后转为
/// 搜索结果视图；失焦且内容为空时恢复当前分类卡片。
pub(super) fn draw_settings_search(f: &mut Frame<'_>, search_rect: Rect) {
    let pal = f.pal;
    let mut q = f.state.settings_query.clone();
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique("settings_search"))
            .max_rect(search_rect),
        |u| {
            let pill = search_rect.height() * 0.5;
            {
                let p = u.painter();
                p.rect_filled(search_rect, pill, pal.surface_variant);
                p.rect_stroke(
                    search_rect,
                    pill,
                    egui::Stroke::new(1.0, pal.border_subtle),
                    egui::StrokeKind::Inside,
                );
            }
            // 文字输入区（左侧留白，右侧留图标位）。
            let text_rect = Rect::from_min_max(
                search_rect.min + vec2(10.0, 0.0),
                search_rect.max - vec2(30.0, 0.0),
            );
            let mut child = u.new_child(
                egui::UiBuilder::new()
                    .max_rect(text_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            let r = child.add(
                egui::TextEdit::singleline(&mut q)
                    .hint_text(t("Search settings"))
                    .frame(egui::Frame::NONE)
                    .desired_width(text_rect.width()),
            );
            // 聚焦描边转 accent（130ms 补间，与 `draw_text_field` 同一套：
            // 颜色过渡、宽度两档硬切——1px 量级的宽度补间只会让抗锯齿
            // 边缘抖一下）。
            //
            // CAUTION: painter 在 `child` 建好之后重新取——`u.painter()`
            // 借住 `u`，与随后的 `u.new_child`（要 `&mut u`）相撞。
            let focus_k = crate::anim::tween_f32(
                u.ctx(),
                egui::Id::unique("settings_search_focus"),
                if r.has_focus() { 1.0 } else { 0.0 },
                std::time::Duration::from_millis(130),
                crate::anim::Ease::OutCubic,
            );
            if focus_k > 0.004 {
                u.painter().rect_stroke(
                    search_rect,
                    pill,
                    egui::Stroke::new(1.0 + 0.5 * focus_k, pal.accent),
                    egui::StrokeKind::Inside,
                );
            }
            // 右侧放大镜。
            let icon_r = Rect::from_center_size(
                pos2(search_rect.max.x - 15.0, search_rect.center().y),
                vec2(16.0, 16.0),
            );
            crate::icons::Icon::Search.paint(u.painter(), icon_r, pal.text_dim);
            r
        },
    );
    let inner = resp.inner;
    if q != f.state.settings_query {
        f.state.settings_query = q;
    }
    // 空态聚焦 → 最近修改；失焦（空）→ 回分类视图。
    if f.state.settings_query.is_empty() {
        f.state.settings_show_recent = inner.has_focus();
    }
    f.hit_insert("settings_search", search_rect);
}

/// 左侧导航块：圆角背景 + 分类按钮（图标 + 标题）。
///
/// 导航背景是**一整块圆角矩形**，顶边与搜索框下缘对齐、底边伸到
/// 窗口下缘留白处——「导航背景覆盖设置与搜索框」的问题由搜索框
/// 画在导航块**上方**解决。
/// 画「通信」分类的插头图标（自绘，不依赖图标字体）。
///
/// 参照 HTML 原型的插头 SVG（24 viewBox）：两个脚（插头插脚）+
/// 圆角矩形（插头头）+ 一条线（线尾）。映射到 16×16 的 `r`。
pub(super) fn draw_plug_icon(painter: &egui::Painter, r: egui::Rect, color: egui::Color32, stroke_w: f32) {
    let stroke = egui::Stroke::new(stroke_w, color);
    let u = |x: f32, y: f32| -> egui::Pos2 { pos2(r.min.x + x, r.min.y + y) };
    // 两个脚
    painter.line_segment([u(5.5, 1.0), u(5.5, 5.5)], stroke);
    painter.line_segment([u(10.5, 1.0), u(10.5, 5.5)], stroke);
    // 头
    let head = egui::Rect::from_min_max(u(4.0, 5.0), u(12.0, 10.5));
    painter.rect_stroke(head, 2.0, stroke, egui::StrokeKind::Inside);
    // 线
    painter.line_segment([u(8.0, 11.0), u(8.0, 14.5)], stroke);
}

pub(super) fn draw_settings_nav(f: &mut Frame<'_>, nav_rect: Rect) {
    let pal = f.pal;
    let painter = f.ui.painter_at(nav_rect);

    let pad = 4.0;
    let gap = 2.0;
    // 项高随字号缩放，否则大字号下导航文字会顶到相邻项。
    let item_h = pal.control_height * 1.43;
    let row_step = item_h + gap;
    let item_rect = |i: usize| {
        let y0 = nav_rect.min.y + pad + i as f32 * row_step;
        Rect::from_min_max(
            pos2(nav_rect.min.x + pad, y0),
            pos2(nav_rect.max.x - pad, y0 + item_h),
        )
    };

    // ---- 选中底：**一整块**，在两项之间平移 -------------------------
    //
    // 早前是「每项各自淡入淡出」：切换分类时旧块淡出、新块淡入，两个
    // 半透明块在中间同时存在——观感是「闪一下」而不是「移过去」。
    // 用户要求「选中导航后背景平移到选择导航」，所以改成**单块**：
    // 位置（下标）走补间，切换时从旧项滑到新项；颜色/圆角不变。
    //
    // 补间键与分类无关（`nav_pill`）——有键就只有一个块，天然不会
    // 出现两块并存。从当前显示值续接：连续快速切换时它在中途改方向，
    // 不会跳回起点（与分段控件的 `seg_pill` 同一套）。
    let sel_idx = SettingsCat::ALL
        .iter()
        .position(|c| *c == f.state.settings_cat)
        .unwrap_or(0) as f32;
    let pill_idx = crate::anim::tween_f32(
        f.ui.ctx(),
        egui::Id::unique("nav_pill"),
        sel_idx,
        std::time::Duration::from_millis(220),
        crate::anim::Ease::OutCubic,
    );
    let pill_y = nav_rect.min.y + pad + pill_idx * row_step;
    let pill = Rect::from_min_max(
        pos2(nav_rect.min.x + pad, pill_y),
        pos2(nav_rect.max.x - pad, pill_y + item_h),
    );
    painter.rect_filled(pill, item_h * 0.5, pal.row_selected);

    for (i, cat) in SettingsCat::ALL.iter().enumerate() {
        let r = item_rect(i);
        let selected = f.state.settings_cat == *cat;
        let resp = f
            .ui
            .interact(r, egui::Id::unique(("settings_cat", *cat)), egui::Sense::click());
        // ---- 悬停底（110ms）：只在未选中时叠 -----------------------------
        //
        // 选中态由上面那块**平移的**选中底表达，这里不再各画一份——
        // 两块半透明底叠在一起会加深，滑过时颜色不匀。
        if !selected {
            let hov_t = crate::anim::tween_f32(
                f.ui.ctx(),
                egui::Id::unique(("nav_hover", *cat)),
                if resp.hovered() { 1.0 } else { 0.0 },
                std::time::Duration::from_millis(110),
                crate::anim::Ease::OutCubic,
            );
            if hov_t > 0.004 {
                painter.rect_filled(r, item_h * 0.5, fade_color(pal.row_hover, hov_t));
            }
        }
        // 图标（左）+ 标题（紧挨图标，不留大空隙）。
        //
        // 早期实现文字以 `r.center() + vec2(14, 0)` 左对齐起点——图标
        // 贴左缘（x≈17）、文字却从导航项中心偏右起，中间空出约 70pt，
        // 观感「图标和字体离得太远」（2026-10-11 用户反馈）。改为文字
        // 起点 = 图标右缘 + 8pt，图标与标题紧凑成组。
        let icon_r = Rect::from_center_size(pos2(r.min.x + 17.0, r.center().y), vec2(16.0, 16.0));
        // 图标与文字颜色补间（130ms）：切分类时旧项褪色、新项点亮，
        // 与选中底同步。首帧落目标（`tween_color` 惰性插入）。
        let icon_color = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::unique(("nav_icon", *cat)),
            if selected { pal.accent } else { pal.text_dim },
            std::time::Duration::from_millis(130),
        );
        let text_color = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::unique(("nav_text", *cat)),
            if selected { pal.text_bright } else { pal.text_dim },
            std::time::Duration::from_millis(130),
        );
        // 「通信」用自绘插头（图标字体的 PUA 码位已占满，新增字形要
        // 重新生成 TTF，成本高）：头 + 两个脚 + 线。其余分类用字体图标。
        //
        // 2026-10-25：原「主题」分类的自绘调色板分支随 Theme 并入
        // Appearance 一并移除——外观分类直接用字体图标
        // （`Icon::Image`），调色板自绘函数保留（关于页等处仍可能
        // 复用；确认无引用后再删）。
        if *cat == SettingsCat::Comm {
            draw_plug_icon(&painter, icon_r, icon_color, pal.stroke_normal);
        } else {
            cat.icon().paint(&painter, icon_r, icon_color);
        }
        // 选中项用**粗体**（`emphasis`），未选中用常规。
        //
        // 2026-10-23 用户要求「优化字体字重」。粗体来自独立的粗体伴随
        // 字体（`msyhbd.ttc` 等，见 `theme::fonts::install_bold_companion`），
        // 机器上没有该文件时 `emphasis` 自动回落常规字重，不会崩也不会
        // 伪造加粗。
        //
        // CAUTION: 这里安全是因为**图标是独立 `painter` 绘制**的
        // （上面 `cat.icon().paint(...)`），不走文字通路——粗体族没有挂
        // 图标字体回退，若把图标字形写进这段文字就会缺字形。同类用法
        // 请沿用这个前提。
        let font = if selected {
            crate::theme::emphasis(pal.font_sm, f.scale)
        } else {
            egui::FontId::proportional(pal.font_sm)
        };
        painter.text(
            pos2(icon_r.max.x + 8.0, r.center().y),
            egui::Align2::LEFT_CENTER,
            t(cat.title()),
            font,
            text_color,
        );
        f.hit_insert(cat.hot_key(), r);
        if resp.clicked() {
            f.state.settings_cat = *cat;
            f.state.settings_query.clear();
            f.state.settings_show_recent = false;
        }
    }
}

/// 侧栏底脚：发丝线 + 应用名（accent 圆点 + 名称）。
///
/// 给悬浮侧栏卡一个视觉锚点——否则卡片下方一大片空白读作「没画完」
/// （HTML 原型 A 方案的同款处理）。名称是产品自称词（同 `OLED` /
/// `日本語` 的待遇）：**不进 i18n 表**，任何语言下都显示自身。
///
/// `card` 是侧栏卡的**外矩形**；本函数在其底部内边距之内绘制。
pub(super) fn draw_settings_nav_footer(f: &mut Frame<'_>, card: Rect) {
    let pal = f.pal;
    let painter = f.ui.painter_at(card);
    let line_y = card.max.y - SETTINGS_NAV_FOOTER_H;
    painter.line_segment(
        [
            pos2(card.min.x + NAV_PAD, line_y),
            pos2(card.max.x - NAV_PAD, line_y),
        ],
        egui::Stroke::new(1.0, pal.border_subtle),
    );
    // 圆点 + 名称垂直中心 = 发丝线到卡片内底边的中点。
    let cy = line_y + (SETTINGS_NAV_FOOTER_H - NAV_PAD) * 0.5;
    let dot_r = 3.0;
    painter.circle_filled(
        pos2(card.min.x + NAV_PAD + dot_r, cy),
        dot_r,
        pal.accent,
    );
    painter.text(
        pos2(card.min.x + NAV_PAD + dot_r * 2.0 + 6.0, cy),
        egui::Align2::LEFT_CENTER,
        "ModularClipboard",
        egui::FontId::proportional(pal.font_xs),
        pal.text_dim,
    );
}
