//! 矢量图标。
//!
//! # 为什么不用字符或emoji
//!
//! 用 `✕` / `★` 这类字符当功能图标有三个问题：
//!
//! 1. **字形依赖**——不同字体下形状与粗细不一致，有的字体里根本没有。
//! 2. **无法着色**——字符颜色跟随前景色，做不出「hover 变色」这类状态。
//! 3. **不可控**——`★` 在部分环境会被渲染成彩色 emoji（VS16 变体选择器）。
//!
//! 所以这里用 [`egui::Painter`] 直接画矢量路径：统一 1.5px 描边、
//! 16px 画布、随主题着色，行为完全可预测。
//!
//! 全部图标遵循同一套规格：
//! - 画布 16×16（`ICON_SIZE`）
//! - 描边 1.5px（`STROKE_WIDTH`），圆头圆角
//! - 视觉重心居中，与 Material Symbols 的轮廓风格对齐

use egui::{Color32, Pos2, Stroke, Ui, Vec2};

/// 图标画布边长（逻辑像素）。
pub const ICON_SIZE: f32 = 16.0;

/// 描边宽度。1.5px 在 100% 缩放下清晰，放大后也不会显得过细。
const STROKE_WIDTH: f32 = 1.5;

/// 在按钮内绘制图标所需的响应尺寸。
const HIT_SIZE: f32 = 20.0;

/// 图标语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// 关闭 / 清除。
    Close,
    /// 已置顶。
    Pinned,
    /// 未置顶（置顶按钮的默认态）。
    Unpinned,
    /// 复制到剪贴板。
    Copy,
    /// 删除。
    Delete,
    /// 编辑。
    Edit,
    /// 搜索。
    Search,
    /// 粘贴。
    Paste,
    /// 设置。
    Settings,
    /// 图片类型条目。
    Image,
    /// 文本类型条目。
    Text,
    /// 文件类型条目。
    File,
    /// HTML / 富文本条目。
    Code,
}

impl Icon {
    /// 按剪贴板类型选图标。
    ///
    /// 用于列表左侧的条目类型标识。`ClipKind` 属于业务层，
    /// 这里用穷举匹配避免依赖它。
    pub fn for_kind(kind: modular_clipboard_core::ClipKind) -> Self {
        use modular_clipboard_core::ClipKind;
        match kind {
            ClipKind::Text => Icon::Text,
            ClipKind::Html => Icon::Code,
            ClipKind::Image => Icon::Image,
            ClipKind::Files => Icon::File,
        }
    }

    /// 无障碍标签。屏幕阅读器会读出这个。
    pub fn label(self) -> &'static str {
        match self {
            Icon::Close => "关闭",
            Icon::Pinned => "已置顶",
            Icon::Unpinned => "置顶",
            Icon::Copy => "复制",
            Icon::Delete => "删除",
            Icon::Edit => "编辑",
            Icon::Search => "搜索",
            Icon::Paste => "粘贴",
            Icon::Settings => "设置",
            Icon::Image => "图片",
            Icon::Text => "文本",
            Icon::File => "文件",
            Icon::Code => "HTML",
        }
    }

    /// 绘制到指定的矩形区域。
    pub fn paint(self, painter: &egui::Painter, rect: egui::Rect, color: Color32) {
        let stroke = Stroke::new(STROKE_WIDTH, color);
        // 缩放到 ICON_SIZE 的坐标系再画，路径数据就能硬编码 16×16 的数值
        let scale = (rect.width() / ICON_SIZE).min(rect.height() / ICON_SIZE);
        let center = rect.center();
        let p = |x: f32, y: f32| {
            Pos2::new(
                center.x + (x - ICON_SIZE / 2.0) * scale,
                center.y + (y - ICON_SIZE / 2.0) * scale,
            )
        };

        match self {
            Icon::Close => {
                // X：两条对角线，从 (4,4) 到 (12,12)
                let d = 4.0 * scale;
                let m = 0.5 * STROKE_WIDTH;
                painter.line_segment([p(4.0 - m, 4.0 - m), p(11.0 + m, 11.0 + m)], stroke);
                painter.line_segment([p(11.0 + m, 4.0 - m), p(4.0 - m, 11.0 + m)], stroke);
                let _ = d;
            }
            Icon::Pinned | Icon::Unpinned => {
                // 五角星。置顶态实心，未置顶态仅描边。
                let cx = 8.0;
                let cy = 8.6;
                let outer = 6.2;
                let inner = 2.7;
                let mut pts = Vec::with_capacity(10);
                for i in 0..10 {
                    let r = if i % 2 == 0 { outer } else { inner };
                    // 顶点在正上方，顺时针每 36°
                    let ang = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
                    pts.push(p(
                        cx + r * ang.cos(),
                        cy + r * ang.sin(),
                    ));
                }
                if self == Icon::Pinned {
                    // 实心：已置顶状态，需要视觉上「更重」
                    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
                } else {
                    // 描边：未置顶状态，暗示「可点击以置顶」
                    painter.add(egui::Shape::closed_line(pts, stroke));
                }
            }
            Icon::Copy => {
                // 两张重叠的卡片：后一张描边，前一张实心
                let back = [p(3.5, 3.5), p(10.5, 3.5), p(10.5, 8.5)];
                painter.add(egui::Shape::closed_line(back.to_vec(), stroke));
                let front = [
                    p(5.5, 5.5),
                    p(12.5, 5.5),
                    p(12.5, 12.5),
                    p(5.5, 12.5),
                ];
                painter.add(egui::Shape::closed_line(front.to_vec(), stroke));
                // 用背景色填住重叠区，让「复制」语义成立
                let _ = front;
            }
            Icon::Delete => {
                // 垃圾桶：盖子 + 桶身 + 两条竖线
                painter.line_segment([p(3.0, 4.5), p(13.0, 4.5)], stroke);
                painter.line_segment([p(6.5, 4.5), p(6.5, 3.0)], stroke);
                painter.line_segment([p(9.5, 4.5), p(9.5, 3.0)], stroke);
                painter.line_segment([p(6.5, 3.0), p(9.5, 3.0)], stroke);
                painter.add(egui::Shape::closed_line(
                    vec![p(4.5, 4.5), p(5.0, 13.0), p(11.0, 13.0), p(11.5, 4.5)],
                    stroke,
                ));
                painter.line_segment([p(7.0, 6.5), p(7.2, 11.0)], stroke);
                painter.line_segment([p(9.0, 6.5), p(8.8, 11.0)], stroke);
            }
            Icon::Edit => {
                // 铅笔：斜杆 + 笔尖 + 底边
                painter.add(egui::Shape::closed_line(
                    vec![p(3.0, 13.0), p(4.2, 10.0), p(11.0, 3.2), p(12.8, 5.0)],
                    stroke,
                ));
                painter.line_segment([p(3.0, 13.0), p(4.8, 13.0)], stroke);
                painter.line_segment([p(10.4, 3.8), p(12.2, 5.6)], stroke);
            }
            Icon::Search => {
                // 放大镜：圆 + 柄
                painter.add(egui::Shape::circle_stroke(p(7.0, 7.0), 4.2, stroke));
                painter.line_segment([p(10.2, 10.2), p(13.2, 13.2)], stroke);
            }
            Icon::Paste => {
                // 剪贴板：板身 + 顶部夹子
                painter.add(egui::Shape::closed_line(
                    vec![p(4.0, 3.5), p(12.0, 3.5), p(12.0, 13.0), p(4.0, 13.0)],
                    stroke,
                ));
                painter.add(egui::Shape::closed_line(
                    vec![p(6.0, 2.0), p(10.0, 2.0), p(10.0, 4.5), p(6.0, 4.5)],
                    stroke,
                ));
                painter.line_segment([p(6.2, 8.0), p(9.8, 8.0)], stroke);
                painter.line_segment([p(6.2, 10.5), p(9.8, 10.5)], stroke);
            }
            Icon::Settings => {
                // 齿轮：外圈 + 中心孔+ 四根辐条（简化）
                painter.add(egui::Shape::circle_stroke(p(8.0, 8.0), 5.0, stroke));
                painter.add(egui::Shape::circle_filled(p(8.0, 8.0), 1.8, color));
                for (dx, dy) in [(0.0, -6.6), (0.0, 6.6), (-6.6, 0.0), (6.6, 0.0)] {
                    painter.line_segment(
                        [p(8.0 + dx * 0.55, 8.0 + dy * 0.55), p(8.0 + dx, 8.0 + dy)],
                        stroke,
                    );
                }
            }
            Icon::Image => {
                // 图片：外框 + 山峰 + 太阳
                painter.add(egui::Shape::closed_line(
                    vec![p(2.5, 3.5), p(13.5, 3.5), p(13.5, 12.5), p(2.5, 12.5)],
                    stroke,
                ));
                painter.add(egui::Shape::circle_filled(p(6.0, 6.6), 1.1, color));
                painter.add(egui::Shape::line(
                    vec![p(4.0, 11.0), p(7.0, 7.5), p(9.0, 9.5), p(10.5, 8.0), p(12.5, 11.0)],
                    stroke,
                ));
            }
            Icon::Text => {
                // 文本：三条横线，长度递减
                painter.line_segment([p(3.0, 4.5), p(13.0, 4.5)], stroke);
                painter.line_segment([p(3.0, 8.0), p(13.0, 8.0)], stroke);
                painter.line_segment([p(3.0, 11.5), p(9.5, 11.5)], stroke);
            }
            Icon::File => {
                // 文件：带折角的矩形
                painter.add(egui::Shape::line(
                    vec![
                        p(3.5, 2.5),
                        p(9.5, 2.5),
                        p(12.5, 5.5),
                        p(12.5, 13.5),
                        p(3.5, 13.5),
                        p(3.5, 2.5),
                    ],
                    stroke,
                ));
                painter.add(egui::Shape::line(vec![p(9.5, 2.5), p(9.5, 5.5), p(12.5, 5.5)], stroke));
            }
            Icon::Code => {
                // 代码：尖括号 + 斜杠
                // 左尖括号
                painter.add(egui::Shape::line(
                    vec![p(6.0, 4.0), p(2.5, 8.0), p(6.0, 12.0)],
                    stroke,
                ));
                // 右尖括号
                painter.add(egui::Shape::line(
                    vec![p(10.0, 4.0), p(13.5, 8.0), p(10.0, 12.0)],
                    stroke,
                ));
                // 中间斜杠
                painter.line_segment([p(9.2, 3.4), p(6.8, 12.6)], stroke);
            }
        }
    }

}

/// 图标按钮。
///
/// 尺寸取 `HIT_SIZE`（20px），符合「按钮内图标」的规范；
/// 图形本身按 `ICON_SIZE`（16px）绘制，四周留 2px 呼吸空间。
pub fn icon_button(ui: &mut Ui, icon: Icon) -> egui::Response {
    // 颜色取自主题，保证随明暗模式切换。
    // `weak_text_color` 是 egui 0.36 提供的弱化前景色，语义上正好是
    // 「非强调内容」——图标按钮正属于这类。
    let base = ui.visuals().weak_text_color();
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(HIT_SIZE), egui::Sense::click());
    let painter = ui.painter_at(rect);

    if response.hovered() {
        painter.rect_filled(rect, 4.0, ui.visuals().selection.bg_fill);
    }
    let color = if response.hovered() || response.is_pointer_button_down_on() {
        ui.visuals().text_color()
    } else {
        base
    };
    let inner = egui::Rect::from_center_size(rect.center(), Vec2::splat(ICON_SIZE));
    icon.paint(&painter, inner, color);

    response.on_hover_text(icon.label())
}

/// 绘制一个非交互的图标（不占响应空间）。
///
/// 用于「状态标记」这类场合——比如已置顶的星标：
/// 它只是提示当前状态，不可点击，因此不应该吃掉点击。
/// 仍然走矢量绘制，与按钮图标视觉一致。
pub fn paint_inline(ui: &mut Ui, icon: Icon, color: Color32) {
    let (rect, _resp) = ui.allocate_exact_size(Vec2::splat(ICON_SIZE), egui::Sense::hover());
    icon.paint(&ui.painter_at(rect), rect, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_a_label() {
        // 空标签会让屏幕阅读器静默，这是可访问性缺陷
        let all = [
            Icon::Close,
            Icon::Pinned,
            Icon::Unpinned,
            Icon::Copy,
            Icon::Delete,
            Icon::Edit,
            Icon::Search,
            Icon::Paste,
            Icon::Settings,
            Icon::Image,
            Icon::Text,
            Icon::File,
            Icon::Code,
        ];
        for i in all {
            assert!(!i.label().is_empty(), "{i:?} 缺少无障碍标签");
        }
    }

    #[test]
    fn pinned_and_unpinned_are_distinct() {
        // 置顶按钮的两种状态不能混淆
        assert_ne!(Icon::Pinned, Icon::Unpinned);
        assert_ne!(Icon::Pinned.label(), Icon::Unpinned.label());
    }

    #[test]
    fn icon_size_is_consistent() {
        // 规格：画布 16px，按钮20px。改了这里要同步改文档
        assert_eq!(ICON_SIZE, 16.0);
        assert_eq!(HIT_SIZE, 20.0);
    }

    #[test]
    fn stroke_is_thinner_than_icon() {
        // 描边过粗会糊成一团
        assert!(STROKE_WIDTH < ICON_SIZE / 4.0);
    }
}
