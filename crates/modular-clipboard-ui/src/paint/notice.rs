//! 通知：窗内 toast 与通知弹窗。
//!
//! 两条呈现路径共用同一份出入场补间（`notice_anim`）与 TTL——
//! 各写一遍必然漂移。toast 画在主窗口内，弹窗是独立窗口。

use egui::CornerRadius;
use egui::Rect;
use egui::Stroke;
use egui::pos2;
use egui::vec2;
use crate::theme::sized;

use super::{Frame};

use super::float::RAIL_GAP;
use super::shadow::fade_color;

/// 通知（操作反馈）的存活时间。3 秒是桌面端通知的惯例时长：
/// 足够读完一句话，又不至于挡太久。
///
/// 单一数据源：窗内 toast（[`draw_notice_toast`]）、通知弹窗
/// （[`draw_notice_panel`]）与主循环的过期消费（`app.rs` 的
/// `drive_attach_windows`）共用——三处各写一份必然漂移。
pub const NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(3);
/// 入场时长（秒）：下滑 + 淡入。
pub(super) const NOTICE_ENTER: f32 = 0.14;
/// 出场淡化时长（秒）：TTL 结束前这么久开始淡出。
pub(super) const NOTICE_EXIT: f32 = 0.22;
/// 入场位移幅度（逻辑点）：从上方 8pt 滑入。
pub(super) const NOTICE_SLIDE: f32 = 8.0;
/// 面板左右内边距（逻辑点）。
pub(super) const NOTICE_PAD_H: f32 = 8.0;
/// 面板上下内边距（逻辑点）。
pub(super) const NOTICE_PAD_V: f32 = 7.0;
/// 窗内 toast 的宽度上下限（逻辑点）。
pub(super) const NOTICE_MIN_W: f32 = 120.0;
pub(super) const NOTICE_MAX_W: f32 = 280.0;
/// 通知弹窗的宽度上下限（逻辑点）。弹窗不受窗口宽度约束，但也不能
/// 宽到夸张——320pt 约等于 21 个汉字，足够任何一条通知。
pub(super) const NOTICE_PANEL_MIN_W: f32 = 120.0;
pub(super) const NOTICE_PANEL_MAX_W: f32 = 320.0;

/// 通知的出入场进度：`(enter, k)`。
///
/// - `enter` 0→1（OutCubic）：配下滑位移用；
/// - `k = enter * exit`：整体不透明度，出场在 TTL 末尾线性 1→0。
///
/// 抽成函数是因为 toast 与弹窗**必须逐帧一致**：两条路径各写一遍
/// 补间，改一处忘一处就会出现「弹窗淡入了、toast 没有」。
pub(super) fn notice_anim(age: f32) -> (f32, f32) {
    let enter = crate::anim::Ease::OutCubic.apply((age / NOTICE_ENTER).clamp(0.0, 1.0));
    let exit = ((NOTICE_TTL.as_secs_f32() - age) / NOTICE_EXIT).clamp(0.0, 1.0);
    (enter, enter * exit)
}

/// 通知动画的**重绘预约**。
///
/// # 为什么必须有它
///
/// 主循环与弹窗闸门都有「没动静就不画」的优化，而本动画的进度是
/// `at.elapsed()`（不走 `anim` 注册表，因此不被 `anim::busy` 覆盖）——
/// 不主动预约帧的话：
/// 1. 入场可能只播了一半就被闸门掐住，面板僵在中间；
/// 2. 更糟的是**出场**：2.78s 时那一帧永远等不到，通知会满亮度挂在
///    屏幕上直到用户动一下鼠标，然后瞬间消失。
///
/// 策略：动画中（k<1）逐帧续请求；稳态（k==1）一次性预约到
/// 「出场开始前一点」的那一帧。稳态期间不占渲染预算（2.6s 空闲）。
pub(super) fn notice_schedule_repaint(ctx: &egui::Context, age: f32, k: f32) {
    if k < 1.0 - 1e-4 {
        ctx.request_repaint();
    } else {
        let wake = (NOTICE_TTL.as_secs_f32() - age - NOTICE_EXIT).max(0.001);
        ctx.request_repaint_after(std::time::Duration::from_secs_f32(wake));
    }
}

/// 通知 toast：窗口上方居中短暂显示 `Service::notify` 的消息（F-16 / UI-P0-2）。
///
/// # 为什么必须有它
///
/// `AppState::notice` 有 8 处写入（复制失败、删除失败、清空失败、
/// 保存失败……），但在本函数存在之前**全仓没有一个读取方**——通知
/// 写进去即蒸发，用户点了「复制」却没反应时得不到任何解释。
/// 原状态栏（`STATUSBAR_HEIGHT`）已移除，toast 是它的替代通道。
///
/// 行为：在窗口**上方居中**显示（宽度按内容自适应并钳在窗口内，
/// 窄窗口也不会渲染到窗外），[`NOTICE_TTL`] 后自动消失；过期的那一帧
/// 顺带把 `notice` 清掉（读取即消费，不需要额外的定时器）。
///
/// # 弹窗模式（2026-10-20）
///
/// `ui.notice_popup` 开启时本函数**不画窗内 toast**，只负责算好
/// [`UiState::notice_panel`]（弹窗定位要用的面板矩形）——通知改由
/// 独立弹窗显示（[`draw_notice_panel`]），不占窗口内的列表空间。
///
/// # 出入场动效（2026-10-17）
///
/// 早前 toast 是「硬现、硬消失」——满不透明度瞬间出现在最终位置，
/// 3 秒后瞬间蒸发，与其余界面元素（悬停/选中全部走补间）的观感
/// 断裂。现在入场 140ms OutCubic 下滑 8pt + 淡入，出场在最后 220ms
/// 线性淡出。alpha 走 [`fade_color`]（预乘缩放），三层（底/描边/文字）
/// 同步淡，不会出现「底没了字还在」。
///
/// # 换行修复（2026-10-20，用户实机反馈）
///
/// 旧实现量宽度用 `layout(.., MAX_W - 16.0)`（换行到 264pt），**绘制**
/// 却用 `painter.text(..)`——后者是 no-wrap 重排。于是长消息量着是
/// 两行、画出来是一行：文字以整行宽居中，**渲染到通知框之外**。
/// 窄窗口更糟：框宽被 `min(avail_w)` 压窄，而文字仍按 264pt 折行 ⇒
/// 溢出量与窗口窄度成正比。现在测量与绘制共用**同一份 galley**
/// （`galley_with_override_text_color` 只换色、不重排），且换行宽度
/// 按实际可用宽度取；高度也改为按 galley 行数自适应（旧值写死 30pt，
/// 多行文字会纵向溢出）。
pub(super) fn draw_notice_toast(f: &mut Frame<'_>) {
    /// 右边距与顶栏间距。
    const MARGIN: f32 = 12.0;

    let Some((msg, at)) = f.svc.state.notice.clone() else {
        f.state.notice_panel = None;
        return;
    };
    let age = at.elapsed().as_secs_f32();
    let popup_mode = f.svc.state.config.ui.notice_popup;
    // 过期即消费：读取的这一帧负责清掉，避免状态里堆着旧通知。
    // CAUTION: 弹窗模式**不在这里消费**——那时没人画窗内 toast，
    // 消费方移到主循环（`drive_attach_windows`，每帧必到的路径）。
    if age >= NOTICE_TTL.as_secs_f32() {
        if !popup_mode {
            f.svc.state.notice = None;
        }
        f.state.notice_panel = None;
        return;
    }

    // ---- 弹窗面板矩形（两种模式都算，见字段说明）----
    f.state.notice_panel = notice_panel_rect(f, &msg);
    if popup_mode {
        return; // 弹窗模式：窗内不画，交给独立弹窗
    }

    let pal = f.pal;
    let font = sized(pal.font_sm, f.scale);
    let area = f.area;
    let avail_w = area.width() - 2.0 * MARGIN;
    if avail_w <= 0.0 {
        // 退化情况：窗口窄到连左右边距都放不下，画了必溢出——干脆不画。
        return;
    }
    // 换行宽度按**实际可用宽度**取：galley 与最终绘制必须同一宽度，
    // 否则「量着换行、画着不换」——文字会渲染到通知框外（见函数头）。
    let wrap_w = (NOTICE_MAX_W - NOTICE_PAD_H * 2.0)
        .min(avail_w - NOTICE_PAD_H * 2.0)
        .max(8.0);
    let galley = f
        .ui
        .painter()
        .layout(msg.clone(), font.clone(), pal.text, wrap_w);
    let w = (galley.rect.width() + NOTICE_PAD_H * 2.0)
        .clamp(NOTICE_MIN_W, NOTICE_MAX_W)
        .min(avail_w);
    let h = (galley.rect.height() + NOTICE_PAD_V * 2.0).max(30.0);
    // 上方居中 + 边界钳制：
    // 1) 可用宽度 = 窗口宽 - 左右各一个 MARGIN；
    // 2) toast 宽度压到可用宽度以内（哪怕因此比 MIN_W 还窄）；
    // 3) 水平中心对齐窗口中线，再把 x 钳进 [min+MARGIN, max-MARGIN-w]。
    //    只要 w <= 可用宽度，这个区间恒非空（下界 <= 上界）。
    let cx = (area.min.x + area.max.x) * 0.5;
    let x = (cx - w * 0.5).clamp(area.min.x + MARGIN, area.max.x - MARGIN - w);
    // 垂直仍贴顶栏下缘；多行文字时向下长，底缘不得越过窗口。
    let y = (crate::solver::TOPBAR_HEIGHT + MARGIN * 0.5).min(area.max.y - 4.0 - h);
    let r = Rect::from_min_size(pos2(x, y), vec2(w, h));
    // 自报热区：与其它命名区域同一套路，测试与自动化读坐标而不重算公式。
    // 报的是**稳态**矩形（不含入场位移）——MCP 自动化要的是可复用的
    // 落点，动画中途的 8pt 偏移不该让它每周换一个坐标。
    f.hit_insert("notice_toast", r);
    // 出入场进度：enter 0→1（OutCubic）配下滑；exit 在 TTL 末尾 1→0。
    let (enter, k) = notice_anim(age);
    notice_schedule_repaint(f.ui.ctx(), age, k);
    if k <= 0.004 {
        // 完全透明（入场第一帧 / 淡出完毕那一帧）：不画。
        return;
    }
    // 入场位移：从最终位置上方 8pt 滑入（enter 补完后归位）。
    let r = r.translate(vec2(0.0, (1.0 - enter) * NOTICE_SLIDE));
    let painter = f.ui.painter_at(r);
    // 圆角取调色板令牌而非硬编码 6：toast 是浮层控件，规格与按钮/输入框
    // 同级（`radius_md`）。写死数字的话，改主题圆角时 toast 会独自
    // 留在旧值上——这类「一半跟随主题一半不跟随」最难被肉眼发现。
    let radius = CornerRadius::same(pal.radius_md as u8);
    painter.rect_filled(r, radius, fade_color(pal.surface_raised, k));
    painter.rect_stroke(
        r,
        radius,
        Stroke::new(1.0, fade_color(pal.border_subtle, k)),
        egui::StrokeKind::Inside,
    );
    // 文字用**同一份已换行的 galley**：`galley_with_override_text_color`
    // 只替换颜色、不重排（`painter.text` 是 no-wrap 重排，正是旧 bug）。
    let text_pos = r.center() - galley.rect.size() * 0.5;
    painter.galley_with_override_text_color(text_pos, galley, fade_color(pal.text, k));
}

/// 通知弹窗的面板矩形（主窗口客户区坐标，y 可为负 = 窗口上方）。
///
/// 文字宽度要用**主窗口 ctx** 的字体度量（弹窗定位发生在
/// `drive_attach_windows`，那里没有 painter），所以在每帧绘制里算。
/// 与搜索面板同向叠放：搜索面板展开时，通知弹到它上面——
/// 两块面板重叠会互相盖住对方的投影区。
pub(super) fn notice_panel_rect(f: &mut Frame<'_>, msg: &str) -> Option<Rect> {
    let pal = f.pal;
    let font = sized(pal.font_sm, f.scale);
    let galley = f.ui.painter().layout(
        msg.to_string(),
        font,
        pal.text,
        NOTICE_PANEL_MAX_W - NOTICE_PAD_H * 2.0,
    );
    let w = (galley.rect.width() + NOTICE_PAD_H * 2.0)
        .clamp(NOTICE_PANEL_MIN_W, NOTICE_PANEL_MAX_W);
    let h = (galley.rect.height() + NOTICE_PAD_V * 2.0).max(30.0);
    let area = f.area;
    // 搜索面板展开时占着窗口上方那条带，通知往上叠。
    let search_cfg = f.svc.state.config.ui.search_panel;
    let base_top = if f.state.search_expanded {
        area.min.y
            - RAIL_GAP
            - modular_clipboard_core::config::clamp_search_panel_height(search_cfg.height)
    } else {
        area.min.y
    };
    let top = base_top - RAIL_GAP - h;
    let cx = area.center().x;
    Some(Rect::from_min_size(pos2(cx - w * 0.5, top), vec2(w, h)))
}

/// 通知**弹窗**的内容（`ui.notice_popup` 开启时的呈现）。
///
/// 与 [`draw_notice_toast`] 共用全部节奏常量与 [`notice_anim`] /
/// [`notice_schedule_repaint`]，差别只在容器：这里是弹窗客户区中央
/// 的面板（自带投影，由 `draw_popup` 统一画），toast 画在窗口内。
pub fn draw_notice_panel(f: &mut Frame<'_>, panel: Rect) {
    let Some((msg, at)) = f.svc.state.notice.clone() else {
        return;
    };
    let age = at.elapsed().as_secs_f32();
    if age >= NOTICE_TTL.as_secs_f32() {
        // 过期由主循环消费；这里直接不画（别把过期通知再显示出来）。
        return;
    }
    let (enter, k) = notice_anim(age);
    // 续帧必须**先于** k 早退：透明帧也是动画的一帧（见
    // `notice_schedule_repaint` 的说明）。
    notice_schedule_repaint(f.ui.ctx(), age, k);
    if k <= 0.004 {
        return;
    }
    let pal = f.pal;
    let r = panel.translate(vec2(0.0, (1.0 - enter) * NOTICE_SLIDE));
    let painter = f.ui.painter_at(r);
    let radius = CornerRadius::same(pal.radius_md as u8);
    painter.rect_filled(r, radius, fade_color(pal.surface_raised, k));
    painter.rect_stroke(
        r,
        radius,
        Stroke::new(1.0, fade_color(pal.border_subtle, k)),
        egui::StrokeKind::Inside,
    );
    let font = sized(pal.font_sm, f.scale);
    // 弹窗面板尺寸由 notice_panel_rect 按**同样的换行宽度**算出，
    // 这里用面板内宽重排一次，保证文字恰好铺满（换行宽度一致 ⇒
    // 字形度量一致 ⇒ 面板与文字不会互相错位）。
    let galley = f
        .ui
        .painter()
        .layout(msg, font, pal.text, (r.width() - NOTICE_PAD_H * 2.0).max(8.0));
    let text_pos = r.center() - galley.rect.size() * 0.5;
    painter.galley_with_override_text_color(text_pos, galley, fade_color(pal.text, k));
}
