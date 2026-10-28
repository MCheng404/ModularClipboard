//! 卡片：停靠卡片的框、标题头与展开态。
//!
//! 矩形只从 `Card::rect` 读（布局求解在 `solver`），绘制层不做
//! 二次判断；置顶要么独立成窗、要么内嵌在历史栏顶部。

use egui::Align2;
use egui::FontId;
use egui::Rect;
use egui::Stroke;
use egui::pos2;
use egui::vec2;
use modular_clipboard_core::i18n::t;
use crate::card::Card;
use crate::card::CardHost;
use crate::card::CardKind;
use crate::theme::sized;
use crate::workspace::Workspace;

use super::{Frame, CARD_STROKE, CARD_RADIUS};

use super::list::draw_history_body;
use super::detail::draw_detail_body;
use super::detail::draw_rail_body;

/// 置顶卡片当前是否独立成窗。
///
/// 有独立置顶窗时主窗口**不再显示置顶栏**——置顶内容已经在那个
/// 窗口里了，再画一份就是重复。
pub(super) fn pinned_is_separate(ws: &Workspace) -> bool {
    ws.cards
        .iter()
        .any(|c| c.kind == CardKind::Pinned && c.host == CardHost::Window)
}

pub(super) fn draw_cards(f: &mut Frame<'_>, _body: Rect) {
    // 置顶**不占一栏**：要么独立成窗、要么内嵌在历史栏顶部。
    // 两种情况主窗口都不画置顶卡片。
    //
    // IMPORTANT: 判据必须与 solver 用的一致（`pinned_is_embedded`），
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
        // 卡片不再有折叠态——原先这里按 `card.collapsed` 二选一
        // （折叠画把手 / 展开画内容），折叠功能移除后只剩一条路径。
        draw_expanded_card(f, card, r);
    }
    // 置顶内嵌的绘制**不在这里**——它在
    // `draw_expanded_card` 里随历史卡片一起处理：那里会先把历史
    // 卡片的 body 切成两段（置顶在上、历史在下），两段各画各的。
    //
    // NOTE: 早前是「画完历史再往上叠一层」，于是标题叠字 + 列表重叠。
    // 而且这里也拿不到「置顶该占多高」——那是切分时才确定的量。
}

/// 历史卡片是否走「一体性」样式（整窗一块底 + 行卡片浮起）。
///
/// # 为什么有这个东西
///
/// 早前界面是四种灰叠出层次：
///
/// ```text
/// 窗口底  bg / 顶栏 surface_variant / 卡片区 list_bg / 行卡片 surface_raised
/// ```
///
/// 三个相邻区域用了三种灰，再各加一道 1px 描边把它们切开 ——
/// 读起来是「三块拼的」。用户反馈要求「强化整体界面的一体性」。
///
/// 一体性版本的思路：**取消色块分割，层次改由「浮起」表达**。
/// 顶栏与卡片区同色（`bg`），只靠一条分隔线区分顶栏；
/// 行卡片靠阴影 / 更亮的底浮起来。
///
/// 关键取舍：层次仍然分得清（圆角 / 留白 / 明暗），但不再靠
/// 「这块比那块浅一点」来划分 —— 那是最容易显得廉价的手法，
/// 色差一点点不明显、一点点就变脏，而且每加一层都要重新调。
///
/// 代价要认：`list_bg` 这一层**消失**了，「卡片」与「窗口」的边界
/// 不再可见。将来若要加第二个卡片（详情 / 侧栏），两边都是透明底
/// 就没法区分 —— 那种场景得把 `list_bg` 加回来，所以留了这个开关。
pub(super) const HISTORY_UNITY: bool = true;

pub(super) fn draw_card_frame(f: &mut Frame<'_>, r: Rect) {
    let pal = f.pal;
    // NOTE: 不取 scale——描边宽度按逻辑点声明（F-35），本函数没有需要乘 ppp 的量。
    // 纯平毛玻璃：纯色底 + 1px 描边。
    // **禁止渐变、光泽、折射、内发光**——那类效果在浅色卡片上
    // 看起来廉价，且与「平」的视觉语言冲突。
    let painter = f.ui.painter_at(r);
    if HISTORY_UNITY {
        // 一体性：卡片**不画自己的底与描边**，让整窗连成一片。
        // 卡片内缩 `CARD_AREA_INSET` 已经给出了边界暗示。
        return;
    }
    painter.rect_filled(r, CARD_RADIUS, pal.surface);
    painter.rect_stroke(
        r,
        CARD_RADIUS,
        // CAUTION: 不乘 scale（F-35 / UI-P1-4）：egui 的 Stroke 宽度单位
        // 是**逻辑点**，乘 ppp 会让 150% DPI 下的 2pt 描边实际画成 3pt，
        // 与其它 1.0 逻辑点的描边不成体系。物理像素对齐用
        // `round_to_pixel`，不是线性缩放。
        //
        // NOTE: 本分支当前不可达（HISTORY_UNITY 恒 true，函数开头已早退）——
        // 但留着它就必須是正确的，否则将来关掉 UNITY 时这个潜在 bug 直接复活。
        Stroke::new(CARD_STROKE, pal.border_subtle),
        egui::StrokeKind::Inside,
    );
}

/// 展开态：卡片外框 + 头部 + 内容。
pub(super) fn draw_expanded_card(f: &mut Frame<'_>, card: &Card, r: Rect) {
    // 液态玻璃覆盖：**详情卡与停靠的类型栏整卡**参与折射。
    //
    // 为什么历史卡不在这里登记：它内部每张行卡片各有自己的玻璃矩形
    // （`row.rs` 的 `push_glass`，圆角 = 行圆角）。整卡再登记一次会
    // 盖住所有行——着色器逐矩形遍历、**命中即停**，整卡在前 ⇒ 行卡片
    // 的独立折射带全部失效，列表会变成「一整块大透镜」。
    //
    // 详情卡/侧栏卡内部没有更小的玻璃单元，整卡登记是唯一选择。
    // `push_glass` 自带「仅主窗口」闸门，附属窗口不会污染主窗口的收集表。
    if matches!(card.kind, CardKind::Detail | CardKind::Rail) {
        // 底色取 `bg`：`HISTORY_UNITY` 下卡片**不画自己的底**
        // （`draw_card_frame` 早退），看到的就是整窗底色。
        // 传 `surface` 会让详情卡的玻璃比周围深一档。
        f.push_glass(r, CARD_RADIUS, f.pal.bg);
    }
    draw_card_frame(f, r);
    let pal = f.pal;

    let pad = pal.space_sm;
    let inner = r.shrink2(vec2(pad, pad));

    // 头部：标题 + 折叠/分离按钮。
    //
    // NOTE: 历史栏的标题「历史」已按用户要求去掉——头部高度压为 0，
    // 列表从卡片顶部直接开始（不占位置）。其它卡片保留头部标题。
    let head_h = if card.kind == CardKind::History {
        0.0
    } else {
        (pal.font_md * 1.8).max(20.0)
    };
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
        CardKind::History => {
            if HISTORY_UNITY {
                // 一体性：卡片容器透明，整窗一块 `bg`。
                // 层次全部交给行卡片（`surface_raised` 浮起）。
                draw_history_body(f, body, None);
                return;
            }
            // NOTE: 非一体性时历史栏整卡是「暗容器」：frame 底色也用
            // `list_bg`，否则列表四周仍露出 frame 的 `surface` 底色
            // ——浅色主题下就是一圈白色（用户反馈「列表边上还是白色」）。
            f.ui.painter_at(r).rect_filled(r, CARD_RADIUS, pal.list_bg);
            // 置顶条目**就在这个列表里**（排在最前 + 底色微亮标记），
            // 不再有独立的「置顶区」，也没有第二段 body。
            draw_history_body(f, body, None);
        }
        CardKind::Pinned => draw_history_body(f, body, Some(true)),
        CardKind::Detail => draw_detail_body(f, body),
        CardKind::Rail => draw_rail_body(f, body),
    }
}

/// 卡片头部：只有标题。
///
/// # 为什么不再有「分离」「折叠」两枚按钮
///
/// 历史栏**固定在下方**，是主界面的主区，不需要用户手动折叠或拖走——
/// 这两个操作对它是多余的。而它们在界面上表现为两个 PUA 码位的
/// 小图标（⠿ / ⇤），在 150% 缩放下辨识度很差。
///
/// 折叠与分离的**完整逻辑**（`Card::collapsed`、`handle_width`、
/// `collapse_priority`、`Solution::auto_collapsed`、`draw_collapsed_card`、
/// `Op::ToggleCollapse` / `Detach` / `Dock`）都已移除——不是只删按钮。
pub(super) fn draw_card_header(f: &mut Frame<'_>, card: &Card, head: Rect) {
    // 历史栏标题按用户要求去掉：主窗口历史卡片不再显示「历史」两字，
    // 其头部高度也在 `draw_expanded_card` 压为 0，列表从卡片顶部直接
    // 开始（不占位置）。其它卡片（详情等）保留自己的标题。
    if card.kind == CardKind::History {
        return;
    }
    let pal = f.pal;
    let scale = f.scale;
    let font: FontId = sized(pal.font_md, scale);
    f.ui.painter_at(head).text(
        head.left_center(),
        Align2::LEFT_CENTER,
        t(card.kind.title()),
        font,
        pal.text_bright,
    );
}
