//! 列表行的类型图标（文本/图片/文件等）。
//!
//! `md_icon_geom` 给出统一几何，`paint_kind_icon` 按
//! `ClipKind` 选形状。

use egui::Color32;
use egui::Rect;
use egui::pos2;

use super::{Icon};

/// Markdown 徽标的几何参数（**纯函数**：绘制与测试共用一条算式）。
///
/// 归一化坐标（0..1，相对传入的边长 `side`）。抽成结构体是为了
/// 让测试能验证「墨迹在框内、框铺满 rect」而不必真的跑一帧——
/// 手绘图标没有字体度量可依，几何即规格。
///
/// # 2026-10-20 重绘（用户反馈「MD 图标渲染效果差」）
///
/// 1. **铺满 rect**：旧版外框只占约 70%×74%，而同屏的字体图标
///    （[`INK_FIT`] 0.98）几乎铺满 ⇒ Markdown 徽标在列表行与类型栏
///    里肉眼小一圈。现在外框取 `0.02..0.98`（96% 边长），与字体
///    图标观感一致；
/// 2. **笔画收细**：旧版 `stroke = side × 0.10`，16pt 下 1.6px，
///    M 的三根竖笔挤成一团。现在 0.085（16pt → 1.36px、32pt →
///    2.72px），小尺寸下更透气，大尺寸下不飘；
/// 3. **M 与箭头重新配比**：M 占左 55%（x 0.20~0.52），箭头占右
///    （x 0.66~0.86）；两者**底缘严格对齐**（都在 y 0.78）——旧版
///    箭头吊在 y 0.32~0.62、M 在 0.32~0.70，底缘差 8%，看着
///    「箭头短一截」。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MdIconGeom {
    /// 外框（归一化 `[x0, y0, x1, y1]`）。
    pub box_rect: [f32; 4],
    /// M 的折线（左竖 → 谷底 → 右竖）。
    pub m: [(f32, f32); 5],
    /// 下箭头的折线（竖线 + V，顶点重复一次以连通两翼）。
    pub arrow: [(f32, f32); 5],
    /// 描边宽度相对边长的比例。
    pub stroke_ratio: f32,
}

/// 见 [`MdIconGeom`]。
pub fn md_icon_geom(side: f32) -> MdIconGeom {
    let _ = side;
    MdIconGeom {
        box_rect: [0.02, 0.02, 0.98, 0.98],
        // x 0.20~0.52（左 55% 给 M），y 0.22~0.78（占框高 56%，
        // 上下各留约 22%——与字体图标的视觉重心一致）。
        m: [
            (0.20, 0.78),
            (0.20, 0.22),
            (0.36, 0.50),
            (0.52, 0.22),
            (0.52, 0.78),
        ],
        // 竖线 x=0.76：y 0.22~0.78（与 M 竖笔同底缘）；V 顶点
        // (0.76,0.78)，两翼 (0.66,0.66) 与 (0.86,0.66)（V 深 0.12，
        // 小尺寸下仍可辨）。
        arrow: [
            (0.76, 0.22),
            (0.76, 0.78),
            (0.66, 0.66),
            (0.76, 0.78),
            (0.86, 0.66),
        ],
        stroke_ratio: 0.085,
    }
}

/// 按内容类型画徽标图标（行内右下角 / 类型过滤栏共用）。
///
/// Markdown 走**几何绘制**：图标字体的 PUA 码位（U+E000..E011）已占满，
/// 加字形要重新生成 TTF；而「M + 下箭头」用几段线段就能表达清楚，
/// 与 `paint.rs::draw_topbar_icon` 里那批几何图标同一套路。
/// 几何参数见 [`md_icon_geom`]（纯函数，测试直接断言铺满与包含关系）。
pub fn paint_kind_icon(
    painter: &egui::Painter,
    kind: modular_clipboard_core::ClipKind,
    rect: Rect,
    color: Color32,
) {
    use modular_clipboard_core::ClipKind;
    if kind != ClipKind::Markdown {
        Icon::for_kind(kind).paint(painter, rect, color);
        return;
    }
    let side = rect.width().min(rect.height());
    if side <= 0.0 || color == Color32::TRANSPARENT {
        return;
    }
    let g = md_icon_geom(side);
    let w = (side * g.stroke_ratio).max(1.0);
    let stroke = egui::Stroke::new(w, color);
    let u = |x: f32, y: f32| pos2(rect.min.x + x * side, rect.min.y + y * side);
    // 外框：描边朝内（`Inside`），不侵占外圈留白。
    let [bx0, by0, bx1, by1] = g.box_rect;
    painter.rect_stroke(
        Rect::from_min_max(u(bx0, by0), u(bx1, by1)),
        w,
        stroke,
        egui::StrokeKind::Inside,
    );
    // M 与箭头：顶点重复（箭头）让折线连通，epaint 的 miter join
    // 在尖角处自然收边，不需要额外处理。
    painter.add(egui::Shape::line(
        g.m.iter().map(|&(x, y)| u(x, y)).collect(),
        stroke,
    ));
    painter.add(egui::Shape::line(
        g.arrow.iter().map(|&(x, y)| u(x, y)).collect(),
        stroke,
    ));
}
