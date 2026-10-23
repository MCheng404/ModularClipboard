//! 图标的矢量绘制：线段/圆角笔触与内联排布。
//!
//! `vector_icon` 是各 `Icon` 变体的实际画法，
//! `paint_inline` 负责在文本行里对齐。

use egui::Color32;
use egui::Pos2;
use egui::Rect;
use egui::Ui;
use egui::Vec2;
use egui::pos2;

use super::{Icon};

/// 圆头线段：线两端补一个圆，消除方头的尖角感。
///
/// 「圆角、简约」是图标风格的硬要求，而 egui 的 `line_segment` 只有
/// 方头。两头补圆是成本最低的圆角做法（比整条胶囊网格轻得多）。
fn round_line(painter: &egui::Painter, a: Pos2, b: Pos2, w: f32, color: Color32) {
    painter.line_segment([a, b], egui::Stroke::new(w, color));
    let r = w * 0.5;
    painter.circle_filled(a, r, color);
    painter.circle_filled(b, r, color);
}

/// 四枚自绘图标的实际绘制（圆角、简约风）。
///
/// 归一化坐标 0~1，`u()` 把它映射进 `rect` 的内接方框（`pad` 给描边
/// 留余量，避免线宽的一半被包围盒裁掉）。线宽与顶栏图标同一口径
/// （约边长 9.5%，下限 1.4 保证 1x 屏不出现断续）。
pub(crate) fn vector_icon(painter: &egui::Painter, icon: Icon, rect: Rect, color: Color32) {
    let side = rect.width().min(rect.height());
    if side <= 0.0 {
        return;
    }
    let w = (side * 0.095).max(1.4);
    let pad = side * 0.09;
    let inner = side - pad * 2.0;
    let u = |x: f32, y: f32| Pos2::new(rect.min.x + pad + x * inner, rect.min.y + pad + y * inner);

    match icon {
        // 设置 = **左一条竖线 + 右三条横线**（用户指定的形状）。
        //
        // 三横线右端长短不一（长/短/中）形成节奏，比三根等长的更像
        // 「调节器」而不是「列表」；都等长反而像汉堡菜单、与「设置」
        // 的语义对不上。
        Icon::Settings => {
            round_line(painter, u(0.18, 0.14), u(0.18, 0.86), w, color);
            round_line(painter, u(0.38, 0.22), u(0.92, 0.22), w, color);
            round_line(painter, u(0.38, 0.50), u(0.74, 0.50), w, color);
            round_line(painter, u(0.38, 0.78), u(0.86, 0.78), w, color);
        }

        // 置顶 = **圆头 + 直针**（`Pinned` 实心圆、`Unpinned` 描边圆）。
        //
        // 两态**形状完全相同**、只差填充，切换时不会有「换了个图标」
        // 的跳变——用户分不清是「换图标了」还是「状态变了」。
        // 比早前的「梯形身 + 矩形头」简约得多，16pt 下也糊不了。
        Icon::Pinned | Icon::Unpinned => {
            let filled = icon == Icon::Pinned;
            let head_c = Pos2::new((u(0.0, 0.0).x + u(1.0, 0.0).x) * 0.5, u(0.0, 0.34).y);
            let head_r = inner * 0.21;
            if filled {
                painter.circle_filled(head_c, head_r, color);
            } else {
                painter.circle_stroke(head_c, head_r, egui::Stroke::new(w, color));
            }
            // 针：从圆底伸到底部，圆头收尾。
            round_line(
                painter,
                Pos2::new(head_c.x, u(0.0, 0.56).y),
                Pos2::new(head_c.x, u(0.0, 0.94).y),
                w,
                color,
            );
        }

        // 清空 = **圆角垃圾桶**（盖 + 小提手 + 梯形桶身），去掉早前的
        // 两条内竖线——那是写实画法，16pt 下挤成一团，简约风不需要。
        Icon::Trash => {
            // 盖
            round_line(painter, u(0.12, 0.28), u(0.88, 0.28), w, color);
            // 提手：倒 U，坐在盖线上。
            round_line(painter, u(0.38, 0.28), u(0.38, 0.12), w, color);
            round_line(painter, u(0.38, 0.12), u(0.62, 0.12), w, color);
            round_line(painter, u(0.62, 0.12), u(0.62, 0.28), w, color);
            // 桶身：上宽下窄（开口朝上，由盖线闭合）。
            round_line(painter, u(0.22, 0.40), u(0.30, 0.92), w, color);
            round_line(painter, u(0.78, 0.40), u(0.70, 0.92), w, color);
            round_line(painter, u(0.30, 0.92), u(0.70, 0.92), w, color);
        }

        // 复制 = **两枚重叠圆角矩形**（后层描边、前层实心）。
        //
        // 经典的「双文档」形状一眼可辨；前层实心让它在 16pt 下也有
        // 明确的前后关系（两层都描边会糊成一个回）。
        Icon::Copy => {
            let radius = w * 1.1;
            let back = Rect::from_min_max(u(0.36, 0.10), u(0.90, 0.64));
            painter.rect_stroke(back, radius, egui::Stroke::new(w, color), egui::StrokeKind::Inside);
            let front = Rect::from_min_max(u(0.10, 0.36), u(0.64, 0.90));
            painter.rect_filled(front, radius, color);
        }

        // 其余图标不走这里（`Icon::paint` 已经拦截）。
        _ => {}
    }
}

/// 绘制一个非交互的图标（不占响应空间），尺寸取 `icon_size` 令牌。
///
/// 用于「状态标记」这类场合——比如已置顶的图钉：
/// 它只是提示当前状态，不可点击，因此不应该吃掉点击。
pub fn paint_inline(ui: &mut Ui, icon: Icon, color: Color32, pal: &crate::theme::Palette, scale: f32) {
    let side = pal.icon_size * scale;
    let (rect, _resp) = ui.allocate_exact_size(Vec2::splat(side), egui::Sense::hover());
    icon.paint(&ui.painter_at(rect), rect, color);
}

/// 图标在 `rect` 内的绘制中心。
///
/// 抽出来是为了让测试能验证「实际绘制位置」而不必真的跑一帧。
pub fn icon_center(rect: Rect) -> egui::Pos2 {
    pos2(rect.center().x, rect.center().y)
}
