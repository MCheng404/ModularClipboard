//! 顶栏：搜索开关、清除按钮、设置入口，以及两个悬浮面板。
//!
//! 顶栏画在卡片之后（不透明顶条遮住越界内容）；搜索面板与类型
//! 过滤栏虽然是弹窗，其绘制函数住在主窗口的 `Frame` 上，由
//! `app.rs` 在弹窗自己的 Context 里调用。

use egui::CornerRadius;
use egui::Rect;
use egui::Sense;
use egui::Stroke;
use egui::pos2;
use egui::vec2;
use modular_clipboard_core::i18n::{t, tp};
use crate::icons::Icon;
use crate::theme::Palette;
use crate::theme::sized;

use super::{Frame, Op, SearchOpenCause, ConfigGroup};

use super::float::RAIL_GAP;
use super::float::RAIL_PAD;
use super::cards::HISTORY_UNITY;
use super::list::SEARCH_COLLAPSE_DELAY;
use super::list::SEARCH_HOVER_DELAY;
use super::list::SEARCH_OPEN_GRACE;
use super::list::SEARCH_TYPING_GRACE;
use super::shadow::card_shadow_layers;
use super::shadow::fade_color;
use super::shadow::lerp_color;
use modular_clipboard_core::i18n::msg;

// ---------------------------------------------------------------- 顶栏

/// 画顶栏：背景 + 分隔线 + 右侧四枚窗口按钮 + 搜索开关图标。
///
/// 返回搜索图标的 [`egui::Response`]——调用方（`draw`）要拿它喂
/// [`run_search_state_machine`]（悬停展开 / 点击展开的判据）。
/// 输入框本身**不在这里画**：第三轮起它是悬浮面板（`draw_search_panel`）。
pub(super) fn draw_topbar(f: &mut Frame<'_>, bar: Rect) -> egui::Response {
    let pal = f.pal;
    let scale = f.scale;
    // 液态玻璃：顶栏也参与（圆角 0 = 直角矩形，SDF 退化为普通矩形）。
    f.push_glass(bar, 0.0);
    let painter = f.ui.painter_at(bar);
    // 一体性：顶栏与卡片区**同色**（`bg`），整窗连成一片。
    // 早前这里用 `surface_variant`（第二种灰），与下面的卡片区
    // （第三种灰 `list_bg`）相邻 —— 两种灰 + 一条描边，界面就读成
    // 「三块拼的」。现在顶栏只靠底部那条 1px 分隔线区分。
    let bar_bg = if HISTORY_UNITY { pal.bg } else { pal.surface_variant };
    painter.rect_filled(bar, CornerRadius::ZERO, bar_bg);
    painter.hline(
        bar.x_range(),
        bar.max.y,
        Stroke::new(1.0, pal.border_subtle),
    );

    let inner = bar.shrink2(vec2(pal.space_md, 0.0));

    // ---- 右侧：清空 + 置顶 + 设置（图标按钮）--------------------------
    //
    // 从底部状态栏搬上来的。按钮用**图标**而非文字：顶栏高度只有
    // 44pt，中文「清空」两个字排在那里会又高又挤；图标在小尺寸下
    // 更清楚，也不随系统字号缩放变形。
    //
    // 尺寸取顶栏高度的 60%，留出上下呼吸空间。
    let btn = bar.height() * 0.62;
    let gap = pal.space_sm * 2.0;
    // **三枚**按钮：清空 / 窗口置顶 / 设置。隐藏按钮独立在最左。
    //
    // IMPORTANT: 这个数字同时出现在 `draw_topbar_icons` 的
    // `step = r.width() / TOPBAR_BTN_COUNT`。两处各写一份的话，
    // 改一处忘另一处 ⇒ 按钮区与实际按钮错位，表现为
    // 「最右那枚被搜索框压住」或「点齿轮变成拖窗口」。
    // 所以这里用**同一个常量**，不是再抄一遍数字。
    let btn_count = TOPBAR_BTN_COUNT;
    let btn_area = Rect::from_min_size(
        pos2(
            inner.max.x - btn * btn_count - gap * (btn_count - 1.0),
            bar.center().y - btn * 0.5,
        ),
        vec2(btn * btn_count + gap * (btn_count - 1.0), btn),
    );
    draw_topbar_icons(f, btn_area, bar.center().y);

    // ---- 搜索：折叠图标（输入框是悬浮面板，见 draw_search_panel） -------
    //
    // 布局（自左向右）：
    //   [ 大片拖动区 ][ 🔍 ][ ✕ ][ 🗑 ][ ⚙ ]
    //
    // 搜索框默认**折叠成一枚放大镜图标**、贴在窗口按钮左侧：顶栏左侧
    // 整片（除按钮与图标）都是拖动区，用户落手即拖。第三轮起输入框
    // 移出顶栏成为悬浮面板（贴卡片区顶部），顶栏结构因此简化——
    // 左侧拖动区不再需要随展开收缩。
    let sq = btn * 0.62;

    // ---- 最左：隐藏窗口（2026-10-22 用户要求「移到最左边」）----------
    //
    // 独立于右侧功能组，放在顶栏最左缘（类似 macOS 红绿灯的位置直觉：
    // 「把窗口收起来」是与内容无关的窗口操作，不该混进功能按钮里）。
    // 语义仍是「收起到托盘」（不是退出），所以图标保持 `Minimize`
    // 的横线——叉号在中文界面里更常被读成「删除/退出」。
    let hide_r = Rect::from_center_size(
        pos2(inner.min.x + sq * 0.5, bar.center().y),
        vec2(sq, sq),
    );
    f.hit_insert("topbar_close", hide_r);
    if icon_button(f, "topbar_close", hide_r, Icon::Minimize, t("Hide to tray"), false, false) {
        f.state.push(Op::CloseWindow);
    }

    let search_icon_r = Rect::from_center_size(
        pos2(btn_area.min.x - pal.space_md - sq * 0.5, bar.center().y),
        vec2(sq, sq),
    );
    f.hit_insert("search_icon", search_icon_r);
    let icon_resp = search_toggle_button(f, search_icon_r);

    // 顶栏拖动区 = 顶栏**减去交互控件**。
    //
    // IMPORTANT: 这里必须是「挖空」而不是「整块上报」。`WM_NCHITTEST` 对拖动区
    // 返回 `HTCAPTION`，系统随后把按下转成 `WM_NCLBUTTONDOWN`
    // ——**不会产生 `WM_LBUTTONDOWN`**，egui 于是收不到点击。
    //
    // 症状：整个顶栏（含齿轮、垃圾桶、搜索框）全都点不动，
    // 表现为「设置打不开 / 搜索框不能点」。
    let mut drag = Vec::new();
    // 左段：**隐藏按钮右缘** → 搜索图标左缘。
    //
    // 早前是「窗口左缘 → 搜索图标」，隐藏按钮移到最左后它整块压在
    // 这段拖动区上——不改成从它的右缘起算，点隐藏按钮就变成拖窗口。
    //
    // **必须用右缘 + 2pt 起算**：多出来的 2pt 与控件之间留缝，
    // 避免亚像素重叠把控件边缘判成 Caption。
    let left_start = hide_r.max.x;
    let left_end = search_icon_r.min.x;
    if left_end - left_start > 8.0 {
        drag.push(Rect::from_min_max(
            pos2(left_start + 2.0, bar.min.y),
            pos2(left_end - 2.0, bar.max.y),
        ));
    }
    // NOTE: 这里**不设**「两按钮之间」那一段。
    //
    // `btn_area` 宽 `btn*2 + space_sm`、两按钮各占一半——它们是
    // **紧邻**的（`space_sm` 落在 `btn_area` 的内部，两侧各一半）。
    // 早前按「中点 ± 3」加一段，实测该段与垃圾桶重叠
    // （`[[645.7,0]-[651.7,44]]` vs 按钮 `[[617.4,8.4]-[648.7,35.6]]`），
    // 于是那段又变成「点垃圾桶 = 拖窗口」。
    //
    // 两按钮之间本来也没有可拖的空白，略掉即可。
    //
    // 右段：从**设置按钮的真实右缘**到窗口右缘。
    //
    // CAUTION: 不能用 `btn_area.max.x + pal.space_sm` 估——
    // `space_sm` 已经算进 `btn_area` 的宽度里了（见 `btn_area` 的
    // 构造：宽 `btn*2 + space_sm`），再加一次会**侵入按钮**。
    // 实测该段`[[688,0]-[700,44]]` 与齿轮 `[[664.7,8.4]-[696,35.6]]`
    // 重叠，于是点齿轮变成拖窗口。
    //
    // 用热区表里按钮的真实矩形——它就是按钮实际交互的区域。
    let settings_r = f.hit_get("topbar_settings");
    let right_start = settings_r.map(|r| r.max.x).unwrap_or(btn_area.max.x);
    if bar.max.x - right_start > 8.0 {
        drag.push(Rect::from_min_max(
            pos2(right_start + 2.0, bar.min.y),
            bar.max,
        ));
    }
    // 拖动区是**累加**而不是赋值：列表（`draw_history_body`）先于顶栏
    // 画，已经 push 了自己的矩形；赋值会把它们全丢掉（左右留白、行间
    // 缝隙都拖不动）。清零在 `draw()` 开头统一做。
    f.state.drag_regions.extend(drag);

    let _ = scale;
    icon_resp
}

/// 搜索状态机：悬停展开 / 点击开合 / 宽限折叠 / 打字活跃钉住。
///
/// 交互契约（2026-10-21 修订，与旧版的差别见各分支注释）：
/// - 光标**停在**图标上 >= [`SEARCH_HOVER_DELAY`] => 展开（来源记为
///   [`SearchOpenCause::Hover`]）；
/// - 点击图标：折叠态 => 展开 + 聚焦输入框；展开态且**是点击打开的**
///   => 收起；展开态但由悬停打开 => 本次点击只「确认」（聚焦 + 把
///   来源改为 Click），**下一次**点击才收起——避免「悬停已开、点击
///   反而闪关」（2026-10-19 反馈）又保留明确的开合切换；
/// - 离开图标与**面板** => **宽限** [`SEARCH_COLLAPSE_DELAY`] 后折叠；
///   打字活跃（[`SEARCH_TYPING_GRACE`]）与刚展开（[`SEARCH_OPEN_GRACE`]）
///   期间不判折叠；
/// - Esc：弹窗持有键盘焦点时由 `draw_search_panel` 消费，主窗口持有
///   键盘焦点时由本函数消费——两个窗口各管各的焦点场景，互为兜底。
///
/// # 2026-10-21 用户反馈「搜索框打开后无法自动关闭和手动关闭」的根因
///
/// 1. 旧 hold_open 把「egui 焦点在输入框」当**无条件**钉住：egui 的
///    焦点不因用户点到列表/切到别的应用而释放（generic interact 不
///    抢焦点），面板一旦获焦就永远收不回去；
/// 2. 「查询词非空」同样无条件钉住——搜完就走的用户收不掉面板；
/// 3. 折叠倒计时住在本函数（主窗口的 `draw()` 里），指针离开后主窗口
///    **收不到任何事件**，渲染闸门（`skip_render`）把到期帧饿死——
///    计时器永远走不到下一拍。现在倒计时起点处 `request_repaint_after`
///    预约到期帧（与通知动画同一手法）。
/// 4. 手动关闭只剩弹窗 Esc 一条路：键盘焦点被主窗口拿走后 Esc 打到
///    主窗口（没人消费）。现在图标点击是开合切换、主窗口也收 Esc。
pub(super) fn run_search_state_machine(f: &mut Frame<'_>, icon_resp: &egui::Response) {
    let hover_icon = icon_resp.hovered();
    let hover_panel = f.state.search_popup_hover;
    let now = f.ui.ctx().input(|i| i.time);

    if hover_icon {
        f.state.search_hover_since.get_or_insert(now);
    } else {
        // 离开图标即重新计时：再次移入要重新停够时间。
        f.state.search_hover_since = None;
    }
    if !f.state.search_hover_suppressed
        && !f.state.search_expanded
        && f
            .state
            .search_hover_since
            .is_some_and(|t| now - t >= SEARCH_HOVER_DELAY)
    {
        set_search_expanded(f, true);
        f.state.search_opened_by = SearchOpenCause::Hover;
    }

    // ---- 点击图标 = 显式开合（手动关闭的主路径）----
    //
    // 三分支见函数头。悬停展开后的首次点击只「确认」——用户可能还没
    // 察觉面板已开（悬停 0.18s 就展开），点击即关会重现 2026-10-19 的
    // 「点一下、闪一下、关了」。
    if icon_resp.clicked() {
        match (f.state.search_expanded, f.state.search_opened_by) {
            (false, _) => {
                set_search_expanded(f, true);
                f.state.search_opened_by = SearchOpenCause::Click;
                f.state.search_focus_pending = true;
            }
            (true, SearchOpenCause::Hover) => {
                f.state.search_opened_by = SearchOpenCause::Click;
                f.state.search_focus_pending = true;
            }
            (true, SearchOpenCause::Click) => {
                set_search_expanded(f, false);
                f.state.search_hover_suppressed = true;
            }
        }
    }

    // 光标离开图标 ⇒ 解除抑制（下次移入可以正常悬停展开）。
    if !hover_icon {
        f.state.search_hover_suppressed = false;
    }

    // ---- Esc：主窗口持有键盘焦点时的手动关闭 ----
    //
    // 弹窗持有键盘焦点时 Esc 由 `draw_search_panel` 消费（按键落在
    // 弹窗自己的 Context）；用户点过列表后键盘焦点回到主窗口，Esc 就
    // 打到这边——两条路径互补，缺一条就有一种场景关不掉。
    //
    // CAUTION: 录制热键时不能吃 Esc（`capture_recording` 用它取消录制）。
    if f.state.search_expanded
        && f.state.hotkey_recording.is_none()
        && f.ui.input(|i| i.key_pressed(egui::Key::Escape))
    {
        set_search_expanded(f, false);
        f.state.search_hover_suppressed = true;
    }

    // ---- hold_open：只保留两个真正「正在用」的窗口 ----
    //
    // - 刚展开（[`SEARCH_OPEN_GRACE`]）：焦点从请求到回报要跨一两帧，
    //   指针可能已经离开图标，宽限把「鼠标从图标走向输入框」的行程兜住；
    // - 打字活跃（[`SEARCH_TYPING_GRACE`]）：键入/删除的间隙不收，
    //   「打字打到一半弹窗自己没了」不会发生。
    //
    // CAUTION: **不再**用「egui 焦点」或「查询词非空」钉住——两者都
    // 不随用户走开而释放（见函数头根因 1、2）。查询词保留在状态里
    // （列表过滤不丢），面板收起只是把输入框藏起来。
    let typing = f
        .state
        .search_last_input_at
        .is_some_and(|t| now - t < SEARCH_TYPING_GRACE);
    let just_opened = f
        .state
        .search_open_at
        .is_some_and(|t| now - t < SEARCH_OPEN_GRACE);
    let hold_open = typing || just_opened;

    // CAUTION: 顺序是「先判折叠 → 弹窗再画」。反过来（先画再判）
    // 会让**折叠的那一帧**仍然把面板显示出来，外部（测试、自动化脚本）
    // 读到的状态与实际差一帧——表现是「已经收起了，但面板还在」。
    if f.state.search_expanded && !hover_icon && !hover_panel && !hold_open {
        match f.state.search_leave_since {
            None => {
                f.state.search_leave_since = Some(now);
                // 预约宽限到期的那一帧。
                //
                // IMPORTANT: 没有这一行，「自动关闭」在实机上就是坏的——
                // 折叠计时器活在主窗口的 draw() 里，而指针离开后主窗口
                // 收不到任何事件 ⇒ 渲染闸门（skip_render）把到期帧饿死，
                // 面板停在「已计时、未折叠」直到用户随手碰一下窗口。
                // 与通知动画的重绘预约同一手法（见 notice_schedule_repaint）。
                f.ui.ctx().request_repaint_after(std::time::Duration::from_secs_f32(
                    SEARCH_COLLAPSE_DELAY as f32 + 0.05,
                ));
            }
            Some(t) if now - t >= SEARCH_COLLAPSE_DELAY => {
                set_search_expanded(f, false);
            }
            _ => {}
        }
    } else {
        // 仍在区域内（或正在打字 / 刚展开）：取消在途的折叠倒计时。
        f.state.search_leave_since = None;
    }
}

/// 展开 / 收起搜索面板。
///
/// 单一入口：图标点击、悬停计时、弹窗内的 Esc 都走这里，保证
/// 「状态翻转」只有一处。弹窗的显隐由帧循环按 `search_expanded` 同步。
///
/// NOTE: 弹窗的**关窗请求**（标题栏 ✕）不走这里——它由
/// `crate::popup` 直接改 `search_expanded`（那时没有 `Frame` 可借，
/// 且那条路径不来自绘制层）。两处语义一致，计时器的复位由那一侧自理。
pub(super) fn set_search_expanded(f: &mut Frame<'_>, v: bool) {
    if f.state.search_expanded == v {
        return;
    }
    f.state.search_expanded = v;
    f.state.search_hover_since = None;
    f.state.search_leave_since = None;
    // 记录「本次展开时刻」：折叠判定用它做免折叠宽限（见
    // [`SEARCH_OPEN_GRACE`]）。收起时清掉，避免下次展开沿用旧时刻
    // 而让宽限失效。
    f.state.search_open_at = v.then(|| f.ui.ctx().input(|i| i.time));
    if v {
        // 预约「宽限到期」帧。
        //
        // IMPORTANT: 展开宽限结束后状态机要把折叠倒计时起上——那也
        // 是一次 draw()。而那时指针可能早已离开主窗口（用户点开就
        // 走），渲染闸门不会再开帧 ⇒ 倒计时永远起不上。一次预约
        // 覆盖两段宽限（`SEARCH_OPEN_GRACE` 到期帧起倒计时，倒计时
        // 自己再预约下一拍，见 `run_search_state_machine`）。
        f.ui.ctx().request_repaint_after(std::time::Duration::from_secs_f32(
            SEARCH_OPEN_GRACE as f32 + 0.05,
        ));
    }
}

/// 画悬浮搜索面板（第三轮起输入框的新形态）。
///
/// 与旧「顶栏内弹出输入框」的差别只在**位置与容器**：矩形由
/// [`FloatLayout`] 给（卡片区通栏、贴顶部），控件结构与交互
/// （hint / 字体 / 边距 / 无外框 / 聚焦加强 / 清除按钮 / 焦点自报）
/// 与旧 `draw_search` 完全一致，行为可无缝对拍。
pub fn draw_search_panel(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    // 液态玻璃：搜索面板参与（圆角与它自己的底色一致）。
    f.push_glass(r, pal.radius_md);
    let painter = f.ui.painter_at(r);
    // 纯平毛玻璃禁令同样适用于悬浮层：纯色底 + 1px 描边，禁渐变光泽。
    let radius = pal.radius_md;
    painter.rect_filled(r, radius, pal.surface);
    painter.rect_stroke(
        r,
        radius,
        Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );

    // 文字直接从左边距起排（放大镜在顶栏，不重复画）。
    let pad = pal.space_sm;
    let edit_rect = Rect::from_min_max(
        pos2(r.min.x + pad, r.min.y + 1.0),
        pos2(r.max.x - pad, r.max.y - 1.0),
    );
    if edit_rect.width() <= 4.0 || edit_rect.height() <= 4.0 {
        return;
    }
    // 自报热区给测试用（见 `UiState::hit` 的说明）。
    //
    // NOTE: 登记**两个**：`search_box` 是外框，`search` 是输入区。
    f.hit_insert("search_box", r);
    f.hit_insert("search", edit_rect);
    // 面板**整块**可点：点到左右 padding（输入区之外）也应当聚焦输入框。
    //
    // CAUTION: 不注册这一条的后果不是「点不动」而是更糟的「变成拖窗口」——
    // 顶栏的拖动区上报给系统后变成 HTCAPTION，面板 padding 处没有 egui
    // 交互，点击穿透到 WM_NCHITTEST，用户点输入框左侧空白就把窗口拖走了。
    // 后注册的 interact 在重叠区胜出，面板整体盖过拖动区。
    let panel_resp = f
        .ui
        .interact(r, egui::Id::unique("card_search_box"), Sense::click());
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            // IMPORTANT: **必须给显式 id**。不给的话 egui 按调用位置自动生成，
            // 于是：焦点断言没法稳定地指向它（只能退化成
            // 「有没有任何控件获得焦点」这种恒真判断），且任何调整
            // 布局的改动都可能悄悄换掉 id。
            .scope_id(egui::Id::unique("search_edit"))
            .max_rect(edit_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut f.state.query)
                    .hint_text(t("Search history…"))
                    .desired_width(f32::INFINITY)
                    // IMPORTANT: 字号必须给**不带scale 的逻辑字号**。
                    //
                    // 早前传的是 `sized(pal.font_md, scale)`——它已经把
                    // DPI 系数乘进去了，而 egui 在计算可用高度时会
                    // **再乘一次** `pixels_per_point`。结果是行高被放大到
                    // 远超框高，文字被裁掉上半/下半截，表现为
                    //「能输入但看不见、字很小」。
                    //
                    // 正确做法：`FontId` 用逻辑字号，由 egui 负责缩放。
                    .font(egui::FontId::proportional(pal.font_md))
                    // 内边距收窄：egui 默认的 `symmetric(4,2)` 在此会让
                    // 单行控件偏高而裁切。
                    .margin(egui::Margin::symmetric(2, 0))
                    .text_color(pal.text)
                    // 去掉 TextEdit 自带的外框：外框已由上面的
                    // `rect_stroke` 画好，两层框叠在一起会显得脏。
                    //
                    // IMPORTANT: 这里必须给 `egui::Frame` 枚举而不是 `false`：
                    // 0.36 的 `frame()` 收的是容器框类型，不是 bool。
                    .frame(egui::Frame::NONE),
            )
        },
    );
    // `scope_builder` 返回 `InnerResponse<Response>`：内层是 TextEdit
    // 自己的响应，外层是这次作用域分配整体的响应。
    // 焦点与 id 都要用**内层**的——外层 id 属于作用域，不是输入框。
    let edit_resp = resp.inner;

    // 自报真实 Id，供测试精确判断焦点落在哪个控件上（见字段说明）。
    f.state.search_edit_id = Some(edit_resp.id);

    // 「正在打字」时间戳：内容每变一次就刷新（见
    // [`UiState::search_last_input_at`]）。状态机的打字活跃钉住靠它——
    // egui 焦点不能当「正在打字」的判据（点别处也不释放，2026-10-21
    // 用户反馈「面板收不回去」的根因之一）。
    if edit_resp.changed() {
        f.state.search_last_input_at = Some(f.ui.ctx().input(|i| i.time));
    }

    // 聚焦描边补间（130ms）：点进输入框时 border → accent 淡入、失焦
    // 淡回，与设置页输入框同一套（`draw_text_field`）。判据用**本帧
    // 实时焦点**（`edit_resp.has_focus()`），不用滞后一帧的
    // `search_popup_focus`——那是回报给主窗口的状态机用的。
    // 弹窗 ctx 的补间由 `draw_popup` 末尾的 busy 检查续帧。
    let focus_k = crate::anim::tween_f32(
        f.ui.ctx(),
        egui::Id::unique("search_panel_focus"),
        if edit_resp.has_focus() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(130),
        crate::anim::Ease::OutCubic,
    );
    if focus_k > 0.004 {
        let ring = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::unique("search_panel_focus_color"),
            pal.accent,
            std::time::Duration::from_millis(130),
        );
        painter.rect_stroke(
            r,
            radius,
            Stroke::new(1.0 + 0.5 * focus_k, ring),
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
            .interact(clear, egui::Id::unique("card_search_clear"), Sense::click());
        // 悬停圆角底 + 图标颜色补间（110ms）：与行内操作按钮同一套。
        // 弹窗 ctx 里的补间要靠末尾的 busy 续帧（见本函数结尾）。
        let chov = crate::anim::tween_f32(
            f.ui.ctx(),
            egui::Id::unique("search_clear_hover"),
            if cr.hovered() { 1.0 } else { 0.0 },
            std::time::Duration::from_millis(110),
            crate::anim::Ease::OutCubic,
        );
        if chov > 0.004 {
            f.ui
                .painter_at(clear)
                .rect_filled(clear, pal.radius_sm, fade_color(pal.row_hover, chov));
        }
        let clear_icon_color = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::unique("search_clear_icon"),
            if cr.hovered() { pal.text_bright } else { pal.text_dim },
            std::time::Duration::from_millis(110),
        );
        crate::icons::Icon::Close.paint(
            &f.ui.painter_at(clear),
            Rect::from_center_size(clear.center(), vec2(btn, btn) * 0.7),
            clear_icon_color,
        );
        if cr.clicked() {
            f.state.query.clear();
            // 清空也是「正在用」：刷新打字活跃时间戳，别让面板在用户
            // 刚点完清除按钮的下一拍就开始折叠倒计时。
            f.state.search_last_input_at = Some(f.ui.ctx().input(|i| i.time));
            // 清空后主动交还焦点：否则用户还得再点一次才能继续打字。
            f.ui.memory_mut(|m| m.request_focus(edit_resp.id));
        }
        cr.on_hover_text(t("Clear search"));
    }

    // 焦点请求在**画完之后**发：id 刚自报出来，此刻才有效。
    if f.state.search_focus_pending
        && let Some(id) = f.state.search_edit_id
    {
        // CAUTION: 必须是自报的**真实 id**。拿上层猜的
        // `Id::unique("search_edit")` 请求焦点会打水漂——那个 id 与
        // TextEdit 实际用的 id 不相等（`UiBuilder::id` 只是容器 id）。
        f.ui.memory_mut(|m| m.request_focus(id));
        f.state.search_focus_pending = false;
    }
    // 点面板空白处（输入区左右的 padding）也聚焦输入框。
    if panel_resp.clicked()
        && let Some(id) = f.state.search_edit_id
    {
        f.ui.memory_mut(|m| m.request_focus(id));
    }

    // ---- Esc 收起面板 ----
    //
    // 为什么必须在这里而不是主窗口：按键落在**弹窗自己的
    // `egui::Context`** 里，主窗口的 `run_search_state_machine` 读不到
    // （`search_popup_focus` 需要弹窗每帧回报，就是同一个原因）。
    //
    // 为什么不能只 `set_search_expanded(f, false)`：输入框此刻持有焦点，
    // 而状态机的 `hold_open` 里「焦点」这一项是无条件的 ⇒ 下一帧又把它
    // 展开回来，Esc 看起来「按了没用」。所以必须同时把焦点交出去。
    //
    // 收起时置 `search_hover_suppressed` 与点击路径同理：光标多半还停在
    // 图标上，否则 0.18s 后悬停计时会立刻把它重新展开。
    if f.ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        set_search_expanded(f, false);
        f.state.search_hover_suppressed = true;
        // 交还焦点：焦点留在已隐藏的输入框上会让 `TextEdit` 继续吃按键，
        // 且 `hold_open` 继续为真（见上）。用**自报的真实 id**
        // 而不是 `m.focused()`：后者此刻可能已被别的控件抢走，
        // 那会把别人的焦点也一起收掉。
        if let Some(id) = f.state.search_edit_id {
            f.ui.memory_mut(|m| m.surrender_focus(id));
        }
        f.state.search_popup_focus = false;
    }
}

/// 弹窗投影的**贴边浓度**（紧贴面板外第一环带的 alpha，0~1）。
///
/// # 为什么不能沿用卡片投影的浓度
///
/// `Palette::shadow` 是 `rgba_unmultiplied(0,0,0,255)`——**alpha=255 的
/// 纯黑**。卡片投影再乘用户浓度倍率，反混合后贴边强度被顶到 0.95。
/// 这对主窗口卡片是对的：它压在**不透明底图**上，0.95 的黑看着就是
/// 阴影。
///
/// 弹窗画布是**透明**的（清屏 alpha=0，见 `crate::popup`），同一强度
/// 直接裸在桌面上 ⇒ 紧贴面板外一圈 alpha≈0.42 的纯黑，实机就是一条
/// **黑色描边**；当时类型栏还没有背景板，整块投影更是被看成
/// 「黑色背景」（那一半已由 `draw_type_rail` 的面板底修掉，但浓度
/// 不改的话描边仍在）。
///
/// 弹窗可能压在任意桌面颜色上（浅色壁纸下最明显），浓度必须按「没有
/// 底图托底」重新定，取卡片量级的五分之一左右。
pub const POPUP_SHADOW_STRENGTH: f32 = 0.18;

/// 弹窗投影的模糊半径（逻辑点）。
///
/// 必须让投影在 [`crate::popup::SHADOW_MARGIN`] **之内衰减完**：外扩超出
/// 边距的层会被窗口边界**硬切**，最外圈像素留下一条等浓度的黑框——
/// 比完全没有投影更丑（这正是「描边」的第二重来源）。
///
/// 取 10（σ=5）：`card_shadow_layers` 的可见判据（单层 alpha ≥ 0.003）
/// 会在 k ≥ 12 处全部跳过，最外层可见 grow 为 11 ⇒ 窗口边界处强度
/// < 1/255，看不出截断。有测试守着这个关系，别随手调大。
pub const POPUP_SHADOW_BLUR: f32 = 10.0;

/// 弹窗投影的垂直偏移（逻辑点，正值 = 向下）。
///
/// 向下偏移会吃掉**底部**余量（顶部反而更宽裕），所以它同样参与
/// 「不越界」的约束。
pub const POPUP_SHADOW_OFFSET_Y: f32 = 2.0;

/// 弹层遮罩色：**跟随主题**（深色界面黑遮罩、浅色界面白遮罩）。
///
/// # 为什么浅色界面是白遮罩
///
/// 「压暗」不是唯一目的，**统一色相**才是：浅色界面整体偏亮，叠一层
/// 黑色会把底图压成灰绿；白遮罩是「蒙一层光」，与浅色界面的观感一致。
/// 这是 [`draw_recording_overlay`] 与 MCP 覆盖层既有的规范，新弹层
/// 必须走同一套——四处遮罩各写各的，用户在弹层间切换就会看到压暗程度
/// 或色相突变（2026-10-22 用户反馈「自绘面板要跟随主题」）。
///
/// alpha 取 96 而不是更重的 128：96 是「压暗但不糊死」，面板才是
/// 视觉主体；糊太重会让背后的内容彻底消失，失去「这是个弹层」的感觉。
///
/// # 判据为什么是 `surface.r() < 128`
///
/// 取调色板背景色的亮度当深浅判据，不读 `is_dark`——`is_dark` 是
/// `lerp` 中点才翻转的元信息，主题切换的 220ms 渐变里它会比颜色**晚**变，
/// 遮罩跟着它走会出现「底色已变浅、遮罩还是黑的」的中间态。
pub fn overlay_scrim(pal: &Palette) -> egui::Color32 {
    if pal.surface.r() < 128 {
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 96)
    } else {
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 96)
    }
}

/// 画悬浮弹窗的投影（面板外围那一圈 alpha）。
///
/// 必须用**弹窗客户区级** painter（`ui.painter()`），不能用
/// `painter_at(panel)`——后者按面板矩形裁剪，投影会被切掉一圈，
/// 表现为「面板贴着桌面、没有浮起来」。
///
/// # 与卡片投影的关系
///
/// 只复用 `card_shadow_layers` 这层**高斯轮廓**，不复用
/// [`draw_card_shadow`] 的浓度逻辑——后者是按「有不透明底图托底」标定
/// 的，套到透明画布上就是黑描边（见 [`POPUP_SHADOW_STRENGTH`]）。
/// 同理也不画 `draw_card_shadow` 里的「近投影」：那一组浓度 ×1.15、
/// 紧贴边缘，正是描边感的主要来源。
pub fn draw_popup_shadow(painter: &egui::Painter, panel: Rect, pal: &Palette) {
    // 圆角**跟随面板**（`radius_md`），投影矩形外扩 grow 时半径同步增大
    // ⇒ 矩形与圆角始终同心。早前硬编码 9.0，与面板圆角脱钩，圆角处会
    // 露出一条投影的直角边。
    let radius = pal.radius_md;
    // 阴影**颜色**仍取主题令牌（可配成冷/暖调），只有浓度用弹窗常量。
    let [sr, sg, sb, _] = pal.shadow.to_array();
    let base = panel.translate(vec2(0.0, POPUP_SHADOW_OFFSET_Y));
    for (grow, a) in card_shadow_layers(POPUP_SHADOW_BLUR, POPUP_SHADOW_STRENGTH) {
        // 环带 alpha 走 `from_rgba_unmultiplied`：与 `draw_card_shadow`
        // 同一条路径，别在这里改成 `from_rgba_premultiplied`（管线的混合
        // 是预乘的，egui 送进来的顶点色本来就是非预乘）。
        painter.rect_filled(
            base.expand(grow),
            radius + grow,
            egui::Color32::from_rgba_unmultiplied(
                sr,
                sg,
                sb,
                (a * 255.0).round().clamp(0.0, 255.0) as u8,
            ),
        );
    }
}

/// 画悬浮类型过滤栏的内容（2026-10-15 第三轮新增，待移植设置联动）。
///
/// 由**弹窗**调用（`crate::popup`），`rail` 是弹窗客户区中央的**面板**
/// 矩形（= 按钮组 + 四周 [`RAIL_PAD`]）——[`FloatLayout::compute`] 已把
/// 面板尺寸算好，这里负责画面板底并按停靠方向依次摆按钮。
///
/// 按钮外观：圆角方块，`surface_raised` 底 + 1px 弱描边；激活 =
/// `accent` 底 + `text_on_accent` 图标。点击过滤、再点一次取消
/// （`Op::SetTypeFilter`）。
///
/// 2026-10-20 两个新选项（`TypeRailConfig`）：
/// - `multi`：多选组合过滤，按钮变成开关，可同时点亮多个类型；
/// - `labels`：图标旁/下显示类型名称——竖直停靠时文字在图标下方，
///   水平停靠时在右侧。文字宽度要字体度量，而 [`FloatLayout::compute`]
///   是**无 ctx 的纯函数**，所以实测尺寸由本函数写进
///   [`UiState::rail_panel_size`]，布局侧下一帧按它修正（首帧用估算值）。
pub fn draw_type_rail(f: &mut Frame<'_>, rail: Rect, kinds: &[modular_clipboard_core::ClipKind]) {
    use modular_clipboard_core::config::TypeRailPos;
    let pal = f.pal;
    let rail_cfg = &f.svc.state.config.ui.type_rail;
    let size = modular_clipboard_core::config::clamp_type_rail_size(rail_cfg.size);
    let gap = RAIL_GAP.min(8.0); // 按钮间距比面板间距紧一档
    let labels = rail_cfg.labels;
    let multi = rail_cfg.multi;
    let n = kinds.len();
    if n == 0 || rail.width() <= 0.0 || rail.height() <= 0.0 {
        return;
    }
    let vertical = matches!(rail_cfg.pos, TypeRailPos::Left | TypeRailPos::Right);
    // 液态玻璃：类型栏参与。圆角取面板档（与搜索面板同源）。
    f.push_glass(rail, pal.radius_md);
    // 文字标签的排版（labels 开启时）：竖直排下方、水平排右侧。
    let label_font = sized(pal.font_xs, f.scale);
    let label_h = label_font.size.max(10.0) * 1.25;
    let label_gap = 4.0;
    // 单个按钮沿排列方向的总长（含标签）。
    let cell_len = |label_w: f32| {
        if !labels {
            size
        } else if vertical {
            size + label_gap + label_h
        } else {
            size + label_gap + label_w
        }
    };
    // 实测的面板尺寸回报给布局侧（见 [`UiState::rail_panel_size`]）。
    let mut max_label_w = 0.0_f32;
    if labels {
        for kind in kinds {
            let w = crate::text::measure_text(f.ui, t(kind.label()), &label_font);
            max_label_w = max_label_w.max(w);
        }
    }
    let panel_w;
    let panel_h;
    if vertical {
        let total: f32 = kinds
            .iter()
            .map(|k| cell_len(if labels { crate::text::measure_text(f.ui, t(k.label()), &label_font) } else { 0.0 }))
            .sum::<f32>()
            + gap * (n as f32 - 1.0)
            + RAIL_PAD * 2.0;
        panel_h = total;
        panel_w = size + RAIL_PAD * 2.0;
    } else {
        let total: f32 = kinds
            .iter()
            .map(|k| cell_len(if labels { crate::text::measure_text(f.ui, t(k.label()), &label_font) } else { 0.0 }))
            .sum::<f32>()
            + gap * (n as f32 - 1.0)
            + RAIL_PAD * 2.0;
        panel_w = total;
        panel_h = size + RAIL_PAD * 2.0;
    }
    f.state.rail_panel_size = Some(vec2(panel_w, panel_h));
    // ---- 面板底（不透明）----
    //
    // 必须在按钮**之前**画，且必须不透明：投影是「面板外扩」的实心矩形，
    // 天然覆盖面板内部，只有不透明底能把它盖住。少了这一层，按钮之间的
    // 缝隙会透出投影的纯黑（实机「类型栏是一块黑色背景」）。
    let radius = pal.radius_md;
    let painter = f.ui.painter_at(rail);
    painter.rect_filled(rail, radius, pal.surface);
    painter.rect_stroke(
        rail,
        radius,
        Stroke::new(1.0, pal.border),
        egui::StrokeKind::Inside,
    );
    // 按钮组从面板内侧起排（跳过 padding）——横竖两个方向都只需把
    // 起点各挪一个 `RAIL_PAD`。
    let (mut x, mut y) = (rail.min.x + RAIL_PAD, rail.min.y + RAIL_PAD);

    for &kind in kinds {
        let label_w = if labels {
            crate::text::measure_text(f.ui, t(kind.label()), &label_font)
        } else {
            0.0
        };
        // 命中矩形：图标区 + （labels 时）标签区，视觉上是一整个按钮。
        let (slot, icon_rect) = if labels && vertical {
            let slot = Rect::from_min_size(pos2(x, y), vec2(size, cell_len(label_w)));
            let icon_rect = Rect::from_min_size(pos2(x, y), vec2(size, size));
            (slot, icon_rect)
        } else if labels {
            let slot = Rect::from_min_size(pos2(x, y), vec2(cell_len(label_w), size));
            let icon_rect = Rect::from_min_size(pos2(x, y), vec2(size, size));
            (slot, icon_rect)
        } else {
            let slot = Rect::from_min_size(pos2(x, y), vec2(size, size));
            (slot, slot)
        };
        // 激活判据只走 `active_type_kinds`（单选/多选唯一汇合点）。
        let active = f.state.active_type_kinds().contains(&kind);
        // 热区按类型分key：测试要逐类断言「某类型的按钮在不在」。
        // CAUTION: key 必须是 `&'static str`（热区表的键类型），
        // 不能 `format!`——临时 String 活不过这条语句。
        let hit_key = match kind {
            modular_clipboard_core::ClipKind::Text => "rail_text",
            modular_clipboard_core::ClipKind::Html => "rail_html",
            modular_clipboard_core::ClipKind::Image => "rail_image",
            modular_clipboard_core::ClipKind::Files => "rail_files",
            modular_clipboard_core::ClipKind::Markdown => "rail_markdown",
        };
        f.hit_insert(hit_key, slot);
        // CAUTION: 用 `ui.interact` 而不是 `scope_builder` + `Button`——
        // 后者在**弹窗上下文**里拿不到指针（实测 `hovered` 恒假、
        // 点击不响应；主窗口上下文却正常）。行内按钮（`draw_row_acts`）
        // 一直是 `ui.interact`，本函数与它保持一致。
        let resp = f
            .ui
            .interact(slot, egui::Id::unique(("rail", kind)), Sense::click());
        // 底色：激活 accent / 常态 surface_raised（磨砂模式降 alpha）。
        //
        // CAUTION: `row_hover` 是**半透明**令牌（`from_white_alpha(30)`），
        // 在主窗口里它下面有整块不透明背景托着；而弹窗的画布是**透明**的
        // （清屏 alpha=0），直接拿它当填充会把按钮画成半透明——透出背后
        // 的深色桌面，实机就是「鼠标一放上去图标变黑」。
        // 所以半透明态一律**先铺不透明底、再叠色调**。
        //
        // ---- 动效（2026-10-17）：激活交叉淡入 + 悬停补间 -----------------
        //
        // 底色目标按「是否激活」取，`tween_color` 从当前显示值向目标
        // 续接 ⇒ 点下过滤时方块从 surface_raised 平滑染到 accent，
        // 取消时反向；描边随激活淡出，图标颜色同步补间。
        let act = crate::anim::tween_f32(
            f.ui.ctx(),
            egui::Id::unique(("rail_active", kind)),
            if active { 1.0 } else { 0.0 },
            std::time::Duration::from_millis(160),
            crate::anim::Ease::OutCubic,
        );
        // 磨砂模式：面板底图要透出来，方块底色就得跟着降 alpha。
        //
        // 判据取「`surface` 令牌是否被降过透明度」——`with_backdrop_alpha`
        // 只处理背景层（bg/surface/surface_variant/list_bg），
        // `surface_raised` 不在名单里（它是卡片色，主窗口里必须保持不透明）。
        // 弹窗的方块是**面板本身**，所以按同一比例手降一次。
        let frost_k = pal.surface.a() as f32 / 255.0;
        let mut base_target = if active {
            pal.accent
        } else {
            pal.surface_raised
        };
        if frost_k < 1.0 && !active {
            base_target = base_target.gamma_multiply(frost_k);
        }
        let base = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::unique(("rail_base", kind)),
            base_target,
            std::time::Duration::from_millis(160),
        );
        let sq_radius = size * 0.25;
        let slot_painter = f.ui.painter_at(slot);
        // 有标签时方块底铺满**整个按钮**（图标 + 文字一个整体），
        // 没有标签时与旧行为一致（就是图标那块）。
        let box_rect = if labels { slot } else { icon_rect };
        slot_painter.rect_filled(box_rect, sq_radius, base);
        if !active {
            // 悬停底（110ms）：先铺不透明底（上面的 base），再叠半透明
            // 提亮；alpha 走预乘缩放（`fade_color`），不用 raw row_hover。
            let hov = crate::anim::tween_f32(
                f.ui.ctx(),
                egui::Id::unique(("rail_hover", kind)),
                if resp.hovered() { 1.0 } else { 0.0 },
                std::time::Duration::from_millis(110),
                crate::anim::Ease::OutCubic,
            );
            if hov > 0.004 {
                slot_painter.rect_filled(box_rect, sq_radius, fade_color(pal.row_hover, hov));
            }
            // 常态描边随激活淡出（act→1 时完全收进去，与 accent 底交替）。
            if act < 0.996 {
                slot_painter.rect_stroke(
                    box_rect,
                    sq_radius,
                    Stroke::new(1.0, fade_color(pal.border_subtle, 1.0 - act)),
                    egui::StrokeKind::Inside,
                );
            }
        }
        let icon_target = if active {
            pal.text_on_accent
        } else if resp.hovered() {
            pal.text_bright
        } else {
            pal.text_dim
        };
        let icon_color = crate::anim::tween_color(
            f.ui.ctx(),
            egui::Id::unique(("rail_icon", kind)),
            icon_target,
            std::time::Duration::from_millis(130),
        );
        let icon_px = size * 0.5625; // 32pt → 18pt（同行内按钮比例）
        crate::icons::paint_kind_icon(
            &f.ui.painter_at(slot),
            kind,
            Rect::from_center_size(icon_rect.center(), vec2(icon_px, icon_px)),
            icon_color,
        );
        // 文字标签（labels 开启时）：与图标同色系，激活时用 text_on_accent。
        if labels {
            let text_color = if active {
                pal.text_on_accent
            } else {
                pal.text_dim
            };
            let anchor = if vertical {
                pos2(slot.center().x, icon_rect.max.y + label_gap)
            } else {
                pos2(icon_rect.max.x + label_gap, slot.center().y)
            };
            let align = if vertical {
                egui::Align2::CENTER_TOP
            } else {
                egui::Align2::LEFT_CENTER
            };
            f.ui
                .painter_at(slot)
                .text(anchor, align, t(kind.label()), label_font.clone(), text_color);
        }
        // 提示带计数：用户能看出点下去会过滤出多少条。
        let count = f.svc.state.items.iter().filter(|it| it.kind == kind).count();
        // NOTE: `clicked()` 必须在 `on_hover_text` 之前取——后者按值消耗
        // `Response`（`icon_button` 里踩过同一个坑，E0382）。
        let clicked = resp.clicked();
        let tip = if multi {
            // 计数短语走 `tp`：`1 entry (multi-select)` vs `5 entries (...)`。
            format!(
                "{} · {}{}",
                t(kind.label()),
                count,
                tp(
                    count as i64,
                    " entry (multi-select)",
                    msg(" entries (multi-select)")
                )
            )
        } else {
            format!(
                "{} · {}{}",
                t(kind.label()),
                count,
                tp(count as i64, " entry", msg(" entries"))
            )
        };
        resp.on_hover_text(tip);
        if clicked {
            // 切换逻辑收在 UiState（单选/多选分支在那里），Op 只带
            // 「哪个类型 + 当前是不是多选模式」。
            f.state
                .push(Op::SetTypeFilterKind { kind, multi });
        }
        if vertical {
            y += cell_len(label_w) + gap;
        } else {
            x += cell_len(label_w) + gap;
        }
    }
}

/// 顶栏右侧的四枚图标按钮：关闭 / 最小化 / 清空 / 设置。
///
/// # 为什么改用 egui 的 `Button` 而不再手写 `interact`
///
/// 手写版只调 `ui.interact` + `painter`，控件在 egui 的命中体系里
/// 是「外人」：焦点环、键盘响应、命中排序都要自己照顾。历史上
/// 「按钮画了但点不动」的根因正是命中判定被拖动区抢走——
///
/// ```text
/// WM_NCHITTEST 命中拖动区 ⇒ 返回 HTCAPTION
///                ⇒ 系统发 WM_NCLBUTTONDOWN（非客户区消息）
///                ⇒ **不产生 WM_LBUTTONDOWN**
///                ⇒ egui 收不到点击
/// ```
///
/// 改用 `Button` 后控件走 egui 自己的登记与排序，配合 `draw_topbar`
/// 里的「拖动区挖空」把控件矩形从 `HTCAPTION` 范围里剔除，
/// 两者共同保证可点。
///
/// # 图标仍然自绘
///
/// `Button` 的**文本**走普通字体链，而图标在 PUA 码位
/// （`U+E000..=U+E011`），那条字体链里没有字形，会渲染成方框（tofu）。
/// 所以 `Button` 只负责命中与悬停底色，图标用 [`Icon::paint`] 叠在
/// 它自己的矩形上。
pub(super) fn draw_topbar_icons(f: &mut Frame<'_>, r: Rect, center_y: f32) {
    let step = r.width() / TOPBAR_BTN_COUNT;
    let sq = step * 0.62;

    // 自左向右：清空 / 窗口置顶 / 设置。
    //
    // **隐藏按钮不在这里**（2026-10-22 移到顶栏最左，见 `draw_topbar`）。
    let slot = |i: f32| Rect::from_center_size(pos2(r.min.x + step * (i + 0.5), center_y), vec2(sq, sq));
    let r_clear = slot(0.0);
    let r_pin = slot(1.0);
    let r_set = slot(2.0);

    // 登记热区给测试用（见 `UiState::hit` 的说明）。**必须在点击判定
    // 之前登记**：`draw_topbar` 随后要读这张表来挖拖动区。
    f.hit_insert("topbar_clear", r_clear);
    f.hit_insert("topbar_pin", r_pin);
    f.hit_insert("topbar_settings", r_set);
    // 破坏性操作（清空）：**长按 5 秒**才触发（2026-10-22 用户要求），
    // 长按期间图标摇晃并渐红。误点一下不会再清空历史。
    draw_clear_button(f, r_clear);
    // 窗口置顶：切换「始终显示在最上层、不被其它应用遮住」。
    //
    // 写配置即可 —— `App` 帧循环每帧比对 `always_on_top` 并调
    // `SetWindowPos`，所以这里不需要（也不该）直接碰 Win32。
    // 设置面板里那个同名开关读同一个字段，两处自动同步。
    let pinned = f.svc.state.config.ui.always_on_top;
    if icon_button(
        f,
        "topbar_pin",
        r_pin,
        if pinned { Icon::Pinned } else { Icon::Unpinned },
        if pinned { t("Unpin window") } else { t("Pin window") },
        false,
        pinned,
    ) {
        f.state.push(Op::SetBool {
            group: ConfigGroup::Ui,
            field: "always_on_top",
            value: !pinned,
        });
    }
    if icon_button(f, "topbar_settings", r_set, Icon::Settings, t("Settings"), false, false) {
        f.state.push(Op::ToggleSettings);
    }
}

/// 顶栏右侧的图标按钮数量。
///
/// 清空 / 窗口置顶 / 设置。**隐藏按钮不在这里**——它独立放在顶栏
/// 最左（2026-10-22 用户要求），所以不计入右侧组的宽度分母。
///
/// # 为什么是常量而不是两处各写一个数字
///
/// `draw_topbar` 用它算 `btn_area` 的宽度，`draw_topbar_icons` 用它算
/// `step`（槽位步距）。两处不一致 ⇒ 按钮区与实际按钮错位，表现为
/// 「最右那枚被搜索框压住」或「点齿轮变成拖窗口」——都不是一眼能看出
/// 原因的症状，所以直接共用一个常量，从根上杜绝。
pub(super) const TOPBAR_BTN_COUNT: f32 = 3.0;

/// 一枚方形图标按钮；返回是否被点击。
///
/// 走 egui `Button` 拿命中与悬停，图标自己叠画（理由见
/// [`draw_topbar_icons`]）。
///
/// # 为什么必须开子 `Ui` 而不能直接 `add_sized`
///
/// `add_sized` 只给**尺寸**，位置由父 `Ui` 的布局游标决定——四枚按钮
/// 会并排排在顶栏左缘，而不是我们算好的槽位。于是 `Response.rect`
/// 与登记进 `hit` 表的矩形不一致：测试按热区点击打不中，
/// 拖动区挖空也挖错位置（表现为「点按钮 = 拖窗口」）。
///
/// `UiBuilder::max_rect` 才是绝对定位：子 `Ui` 的原点就是该矩形，
/// 里面的 `Button` 落在矩形左上角，尺寸相同 ⇒ 两者完全重合。
/// 清空按钮：长按 5 秒触发，长按期间图标有渐进动画（摇晃 + 渐红 + 脉冲）。
///
/// # 为什么破坏性操作要长按
///
/// 「清空全部历史」不可撤销，而它就贴在「设置」「置顶」旁边——
/// 单击一误触就全没了。5 秒长按是这类操作的通行门槛（macOS 关机、
/// 手机删除应用同款），配一个「越来越紧张」的动画告诉用户
/// 「正在蓄力、松手就取消」。
///
/// # 动画三段（见文件头的表）
///
/// 进度 `p = elapsed / 5s` 驱动：摇晃幅度随 `p²` 上升（先礼后兵）、
/// 颜色线性走向 `danger`、`p > 0.7` 加心跳式缩放脉冲。三段节奏
/// 让「还剩多久」不用读秒也能感觉到。
///
/// # 松手/移开即取消
///
/// 破坏性操作宁可让用户多按一次，也不做「半途触发」——按到 4.9 秒
/// 手一抖就清空，比多按 5 秒糟糕得多。
pub(super) fn draw_clear_button(f: &mut Frame<'_>, r: Rect) {
    use std::time::Instant;
    const HOLD: f32 = 5.0;

    let pal = f.pal;
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique("topbar_clear"))
            .max_rect(r)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| ui.add_sized(r.size(), egui::Button::new("").frame(false)),
    );
    let resp = resp.inner;

    // 悬停底（与 icon_button 同档补间）。
    let hov = crate::anim::tween_f32(
        f.ui.ctx(),
        egui::Id::unique(("icon_btn_hover", "topbar_clear")),
        if resp.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    if hov > 0.004 {
        f.ui
            .painter_at(r)
            .rect_filled(r, pal.radius_sm, fade_color(pal.row_hover, hov));
    }

    // ---- 长按状态机 ----
    //
    // 按住：起表并推进；松开/移开：**立即归零**（不做半途保留）。
    // 到 5 秒：触发 + 复位。
    //
    // CAUTION: `Op` 要等 `hold` 的借用**释放后**再 push——两者都在
    // `f.state` 上，同时借会撞 E0499（`state` 不可拆字段同时可变）。
    // 用 `trigger` 标志把触发挪到借用区之外。
    // 长按判定：**悬停 + 主键按下**，不走 `Response::is_pointer_button_down_on`。
    //
    // CAUTION: 后者受 Button 的 sense 影响，松手那一帧容易变 false，
    // 而触发条件 `pressed && p >= 1.0` 正好卡在那儿——症状是
    // 「动画跑满、松手却不触发」（2026-10-22 用户反馈）。
    // `hovered() && primary_down()` 语义直白，与控件种类无关。
    // 长按状态机。**语义对齐手机长按**：
    //
    // - **开始**：指针在按钮上 + 主键**刚按下**（`primary_pressed`）；
    // - **继续**：主键仍按住（**不要求 hover**）——按住后手指/指针
    //   轻微移出按钮就归零是错的，那样 5 秒内一点抖动就前功尽弃，
    //   表现为「动画一直在跑、却永远触发不了」（2026-10-22 反馈）；
    // - **结束**：主键松开，此时若已蓄满仍要触发（按够了就该生效）。
    //
    // CAUTION: `Op` 要等 `hold` 的借用**释放后**再 push——两者都在
    // `f.state` 上，同时借会撞 E0499。
    let primary_down = f.ui.input(|i| i.pointer.primary_down());
    let primary_pressed = f.ui.input(|i| i.pointer.primary_pressed());
    let mut trigger = false;
    let elapsed = {
        let hold = &mut f.state.clear_hold_start;
        // 开始：在按钮上按下。
        if primary_pressed && resp.hovered() {
            *hold = Some(Instant::now());
        }
        // 结束：松开。蓄满了就触发，然后清表。
        if !primary_down {
            if let Some(start) = hold
                && start.elapsed().as_secs_f32() >= HOLD
            {
                trigger = true;
            }
            *hold = None;
        }
        // 按住期间的推进：**只看 hold 有没有**，不看 hover。
        let elapsed = match hold {
            Some(start) => start.elapsed().as_secs_f32(),
            None => 0.0,
        };
        // 按住期间到点也触发一次（不用等松手）。
        if hold.is_some() && primary_down && elapsed >= HOLD {
            trigger = true;
            *hold = None;
        }
        elapsed
    };
    if trigger {
        f.state.push(Op::ClearAll);
    }
    let p = (elapsed / HOLD).clamp(0.0, 1.0);

    // ---- 动画参数 ----
    //
    // 只看 `hold` 有没有（不要求 hover）——与状态机同一口径：
    // 按住后指针移出按钮，动画要继续跑，否则用户一移开就以为「断了」。
    let active = f.state.clear_hold_start.is_some();
    let (shake_x, color, scale, tint) = if active {
        // 摇晃：幅度随 p² 上升（先礼后兵），频率也随段位加。
        let (amp, freq) = if p < 0.4 {
            (1.5, 6.0)
        } else if p < 0.7 {
            (3.0, 10.0)
        } else {
            (4.5, 12.0)
        };
        let shake_x = (elapsed * freq * std::f32::consts::TAU).sin() * amp * (0.4 + p * 0.6);
        // 颜色：文字色 → danger，线性。
        let color = lerp_color(pal.text_dim, pal.danger, p);
        // 心跳脉冲（p > 0.7）：让最后一段「有感觉」。
        let scale = if p > 0.7 {
            let k = (p - 0.7) / 0.3;
            1.0 + 0.12 * k * (elapsed * 18.0).sin().abs()
        } else {
            1.0
        };
        // 底色渗 danger tint：与颜色同步，越接近触发越「红」。
        let tint = p * 0.35;
        (shake_x, color, scale, tint)
    } else {
        (0.0, pal.text_dim, 1.0, 0.0)
    };

    // 长按时底色渗 danger（叠在悬停底之上）。
    if tint > 0.004 {
        f.ui
            .painter_at(r)
            .rect_filled(r, pal.radius_sm, fade_color(pal.danger, tint));
    }

    // 图标：抖动位移 + 缩放脉冲。
    let icon_rect = egui::Rect::from_center_size(
        r.center() + egui::vec2(shake_x, 0.0),
        r.size() * 0.78 * scale,
    );
    draw_topbar_icon(&f.ui.painter_at(r), Icon::Trash, icon_rect, color);

    // 热区登记（拖动区挖空要用）。
    f.hit_insert("topbar_clear", r);
    resp.on_hover_text(t("Clear all history (hold 5s)"));
}

pub(super) fn icon_button(
    f: &mut Frame<'_>,
    id: &'static str,
    r: Rect,
    icon: Icon,
    tip: &'static str,
    danger: bool,
    active: bool,
) -> bool {
    let pal = f.pal;
    // `Button` 的文本留空：真正的内容是下面自绘的图标。
    // `frame(false)` 去掉它自带的边框，只保留命中与悬停。
    //
    // NOTE: `Button::frame` 收的是 **bool**（是否画框），而
    // `TextEdit::frame` 收的是 `egui::Frame` 枚举。两者同名不同签名，
    // 写反了会报 E0308。
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique(id))
            .max_rect(r)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| ui.add_sized(r.size(), egui::Button::new("").frame(false)),
    );
    let resp = resp.inner;
    // 悬停底色**补间**（110ms OutCubic，与行悬停同档）：早前是
    // `hovered()` 当帧瞬间 `rect_filled`，鼠标划过时底「硬现」、移开
    // 「硬没」，与列表行的平滑悬停观感不一致。id 用按钮名（每帧稳定）。
    let hov = crate::anim::tween_f32(
        f.ui.ctx(),
        egui::Id::unique(("icon_btn_hover", id)),
        if resp.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    if hov > 0.004 {
        // 底色叠在热区矩形上（`painter_at` 把裁剪限制在顶栏内）。
        f.ui
            .painter_at(r)
            .rect_filled(r, pal.radius_sm, fade_color(pal.row_hover, hov));
    }
    draw_topbar_icon(
        &f.ui.painter_at(r),
        icon,
        // IMPORTANT: 尺寸是**逻辑点**，绝不能乘 `scale`。
        //
        // `scale` 就是 `pixels_per_point`（超采样打开后 = 2.25），乘上去
        // 等于让 egui 按 2.25 倍栅格化字形，而绘制又被 `painter_at(r)`
        // 裁在 24pt 的按钮框内 ⇒ **只露出字形中间一块**：
        // 垃圾桶丢了盖、齿轮丢了齿、图钉丢了针。
        //
        // 症状极具迷惑性——看起来像「字体子集里字形映射错了」，
        // 实际是「字号被 DPI 乘了两次 + 裁剪」。放大镜当初漏乘了 `scale`，
        // 所以它是唯一正常的一枚，也因此比另外四枚小一大截。
        Rect::from_center_size(r.center(), vec2(pal.icon_size, pal.icon_size)),
        // 优先级：破坏性悬停 > 激活态 > 悬停提亮 > 常态弱化。
        //
        // `active` 用 `accent` 配色，而不只是换实心图标：16pt 下
        // 实心与描边的差异很小，不配色用户分不出「已置顶」和「没置顶」。
        // `danger` 与 `active` 分开判断，将来加「既破坏性又有开关态」
        // 的按钮不用改签名。
        match (danger, resp.hovered(), active) {
            (true, true, _) => pal.danger,
            (_, true, true) => pal.accent_hover,
            (_, _, true) => pal.accent,
            (_, true, false) => pal.text,
            _ => pal.text_dim,
        },
    );
    // NOTE: 顺序硬性：`on_hover_text` 按**值**消耗 `Response`，所以
    // `clicked()` 必须在它之前取。把两者写反会 E0382（值已移动）。
    let clicked = resp.clicked();
    resp.on_hover_text(tip);
    clicked
}

/// 顶栏搜索图标按钮（折叠式搜索的开关）。
///
/// 与 [`icon_button`] 同构：egui `Button` 负责命中与悬停，图标自绘。
/// 区别是放大镜**不用字体字形**（图标字体子集只有 18 个码位且
/// `U+E003` 字形偏粗，见 [`draw_search_icon`]），改用几何绘制。
///
/// 返回 `Response` 而不是 `bool`：调用方既要 `hovered`（悬停展开）
/// 也要 `clicked`（点击聚焦），只回一个布尔就丢了另一半信息。
pub(super) fn search_toggle_button(f: &mut Frame<'_>, r: Rect) -> egui::Response {
    let pal = f.pal;
    let resp = f.ui.scope_builder(
        egui::UiBuilder::new()
            .scope_id(egui::Id::unique("search_icon"))
            .max_rect(r)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| ui.add_sized(r.size(), egui::Button::new("").frame(false)),
    );
    let resp = resp.inner;
    // 悬停底色补间：与 [`icon_button`] 同一套（110ms OutCubic）。
    let hov = crate::anim::tween_f32(
        f.ui.ctx(),
        egui::Id::unique("search_icon_hover"),
        if resp.hovered() { 1.0 } else { 0.0 },
        std::time::Duration::from_millis(110),
        crate::anim::Ease::OutCubic,
    );
    if hov > 0.004 {
        f.ui
            .painter_at(r)
            .rect_filled(r, pal.radius_sm, fade_color(pal.row_hover, hov));
    }
    draw_search_icon(
        &f.ui.painter_at(r),
        Rect::from_center_size(r.center(), vec2(pal.icon_size, pal.icon_size)),
        // 过滤生效（查询词非空）时图标走强调色：面板收起后查询仍然
        // 过滤着列表，图标是唯一的「当前在过滤」指示（2026-10-21 起
        // 查询词不再钉住面板，收起后要能看出过滤还开着）。
        if !f.state.query.is_empty() {
            pal.accent
        } else if resp.hovered() {
            pal.text_bright
        } else {
            pal.text_dim
        },
    );
    resp.on_hover_text(if f.state.search_expanded {
        t("Search (expanded)")
    } else if !f.state.query.is_empty() {
        t("Search (filter active, click to view)")
    } else {
        "搜索"
    })
}


/// 画放大镜图标（圆环 + 手柄），几何绘制而非字体字形。
///
/// # 为什么不用 `Icon::Search` 字形
///
/// 图标字体子集只有 18 个码位、源字体不在仓库（只有 pyftsubset 的
/// 4KB 子集），**无法新增**码位。而现有的 `U+E003` 字形笔画偏粗、
/// 圆环与手柄比例失衡，在搜索框这种小尺寸下显得笨重（用户反馈"太丑"）。
///
/// 放大镜本来就只有两条线——圆 + 斜杠，用几何画反而更可控：
/// - **线宽可调**，不受字体设计限制；
/// - 圆环与手柄的**比例由我们决定**，不必迁就字形；
/// - 无字体依赖，不会因缺字形退化成方框。
///
/// # 尺寸约定
///
/// `r` 是图标包围盒。圆环占其中 62%，手柄从圆环边缘斜向外延伸，
/// 整体不超出 `r`——保证与文字基线视觉对齐。
/// 用**几何图形**绘制顶栏的四枚图标（关闭 / 清空 / 置顶 / 设置）。
///
/// # 为什么不用图标字体的字形
///
/// 1. **字形被裁**：`icon_button` 早前把字号写成 `icon_size * scale`，
///    而 `scale` 是 `pixels_per_point`（超采样下 = 2.25）⇒ 字形按 36pt
///    栅格化，却被 `painter_at(r)` **裁到 24pt 的按钮框**里，只露出中间
///    一块：垃圾桶丢了盖、齿轮丢了齿、图钉丢了针，看起来像「字形映射错
///    了」，实际是**裁剪**。放大镜当初没乘 `scale`，所以它是唯一正常的
///    一枚——也正因此它比另外四枚小一大截。
/// 2. **光学重量不齐**：`pin-angle-fill` 是实心、几乎铺满 em 盒，而
///    垃圾桶 / 齿轮是细描边；同一字号下前者重得多，一排按钮看着发飘。
///
/// 几何绘制把线宽、留白、圆角全部握在手里，一排按钮的视觉重量天然一致。
/// 这也与本文件既有的 [`draw_search_icon`]、`draw_plug_icon` 同一条路线
/// ——它们当初就是因为「字形偏粗」改成几何绘制的。
///
/// 未列出的图标仍回退到字形（[`Icon::paint`]），行为不变。
pub(super) fn draw_topbar_icon(painter: &egui::Painter, icon: Icon, r: Rect, color: egui::Color32) {
    let side = r.width().min(r.height());
    if side <= 0.0 || color == egui::Color32::TRANSPARENT {
        return;
    }
    // 自绘图标（设置/置顶/清空/复制）走 `Icon::paint` 的矢量覆盖——
    // 那是唯一一份设计稿，行内与顶栏共用。**这里不能各画一份**：
    // 早前顶栏在本函数里画旧版 Trash/Pinned/Settings，而 `Icon::paint`
    // 里是新版，于是「行内换了新图标、顶栏还是旧的」（2026-10-22 反馈）。
    if icon.is_vector() {
        icon.paint(painter, r, color);
        return;
    }
    // **统一线宽**是「一排按钮协调」的关键：所有图标共用同一个值。
    // 9.5% 与放大镜接近；下限 1.4 保证 1x 屏上不出现断续。
    let w = (side * 0.095).max(1.4);
    let stroke = Stroke::new(w, color);

    // 归一化坐标 → 屏幕坐标。内缩 `pad` 给描边留出余量，
    // 否则最外圈的线宽会有一半落在包围盒外被裁掉。
    let pad = side * 0.09;
    let inner = side - pad * 2.0;
    let u = |x: f32, y: f32| pos2(r.min.x + pad + x * inner, r.min.y + pad + y * inner);
    let c = r.center();

    match icon {
        // 最小化 / 收起：一条横线。
        Icon::Minimize => {
            painter.line_segment([u(0.0, 0.5), u(1.0, 0.5)], stroke);
        }
        // 清空：垃圾桶（盖 + 提手 + 桶身 + 两条内竖线）。
        Icon::Trash => {
            painter.line_segment([u(0.0, 0.20), u(1.0, 0.20)], stroke);
            // 提手：倒 U，坐在盖线上。
            painter.add(egui::Shape::line(
                vec![u(0.34, 0.20), u(0.34, 0.02), u(0.66, 0.02), u(0.66, 0.20)],
                stroke,
            ));
            // 桶身：上宽下窄的梯形（开口朝上，由盖线闭合）。
            painter.add(egui::Shape::line(
                vec![u(0.06, 0.32), u(0.15, 1.0), u(0.85, 1.0), u(0.94, 0.32)],
                stroke,
            ));
            for x in [0.36_f32, 0.64] {
                painter.line_segment(
                    [u(x, 0.46), u(x, 0.86)],
                    Stroke::new(w * 0.8, color),
                );
            }
        }
        // 置顶：图钉。`Pinned` 实心、`Unpinned` 描边——两者形状完全相同，
        // 只差填充，因此切换时不会有「换了个图标」的跳变。
        Icon::Pinned | Icon::Unpinned => {
            let filled = matches!(icon, Icon::Pinned);
            let head = Rect::from_min_max(u(0.10, 0.00), u(0.90, 0.30));
            let tip = u(0.5, 0.60);
            let body = vec![u(0.10, 0.30), u(0.90, 0.30), tip];
            if filled {
                painter.rect_filled(head, w, color);
                painter.add(egui::Shape::convex_polygon(body, color, Stroke::NONE));
            } else {
                painter.rect_stroke(head, w, stroke, egui::StrokeKind::Inside);
                painter.add(egui::Shape::closed_line(body, stroke));
            }
            // 针尖：从锥底伸到底部。
            painter.line_segment([tip, u(0.5, 1.0)], stroke);
        }
        // 设置：齿轮 = 两个同心圆 + 一圈齿。
        //
        // 齿画在**外圈之外**（径向短线）而不是画成带缺口的轮廓：
        // 16pt 下挖缺口会糊成一团，短线更清晰且线宽与其它图标一致。
        Icon::Settings => {
            let r_out = side * 0.30;
            let r_in = side * 0.115;
            painter.circle_stroke(c, r_out, stroke);
            painter.circle_stroke(c, r_in, stroke);
            const TEETH: usize = 8;
            let r_tip = r_out + w * 1.7;
            for k in 0..TEETH {
                let a = k as f32 * std::f32::consts::TAU / TEETH as f32;
                let (sin, cos) = a.sin_cos();
                painter.line_segment(
                    [
                        pos2(c.x + cos * (r_out - w * 0.5), c.y + sin * (r_out - w * 0.5)),
                        pos2(c.x + cos * r_tip, c.y + sin * r_tip),
                    ],
                    stroke,
                );
            }
        }
        // 其余图标（行内 pin/复制/删除、类型标签等）仍走字形。
        _ => icon.paint(painter, r, color),
    }
}

pub(super) fn draw_search_icon(painter: &egui::Painter, r: Rect, color: egui::Color32) {
    let side = r.width().min(r.height());
    // 线宽取边长的 9%：约 1.6px（side=16），比字体字形明显纤细。
    // NOTE: 下限 1.5——太细在1x 屏上会断续，虚化。
    let stroke_w = (side * 0.09).max(1.5);
    let c = r.center();

    // 圆环：直径 52%，圆心偏左上。
    //
    // NOTE: 早前取 62% + 手柄起点在环**内**：放大截图看到手柄被圆环
    // 盖住，整个图标只剩一个空心圆，完全不像放大镜。
    // 现在环缩小、手柄起点移到环**外**（环边缘 + 线宽一半），
    // 两段在视觉上连成一体。
    let ring_r = side * 0.26;
    let off = side * 0.13; // 圆心偏移，留出右下角放手柄
    let ring_c = pos2(c.x - off, c.y - off);
    painter.circle_stroke(ring_c, ring_r, Stroke::new(stroke_w, color));

    // 手柄：从圆环**外缘**（45° 方向）伸向包围盒右下角。
    //
    // 起点 = 环心 + (ring_r + stroke_w*0.5) * √2/2——正好贴在环上，
    // 终点取包围盒的 0.47 处（留一点边距，避免描边被裁）。
    let diag = std::f32::consts::FRAC_1_SQRT_2;
    let start_off = (ring_r + stroke_w * 0.5) * diag;
    let start = pos2(ring_c.x + start_off, ring_c.y + start_off);
    // IMPORTANT: `reach` 必须留够「线宽的一半」的余量：`end` 还要描边，
    // 取 0.47 时终点 8.45 + 0.81 = 9.26 会**溢出**包围盒 9.0
    // （被裁掉一小截）。0.43 留 0.7pt 余量。
    let reach = side * 0.43;
    let end = pos2(c.x + reach, c.y + reach);
    painter.line_segment([start, end], Stroke::new(stroke_w, color));
}
