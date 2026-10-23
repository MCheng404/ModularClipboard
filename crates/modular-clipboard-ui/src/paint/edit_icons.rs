//! 三个几何编辑图标（铅笔 / 撤销 / 对勾）。
//!
//! 为什么不用图标字体：PUA 码位已分配完毕，加字形要重新生成 TTF；
//! 这三个形状用几段线段就能表达，与 Markdown 徽标同款处理。
//! 全部按归一化坐标（0..1 盒）定义，绘制时映射到目标矩形。

use egui::Rect;
use egui::Stroke;
use egui::pos2;

// ---------------------------------------------------------------- 编辑图标
//
// 三个几何图标（铅笔 / 撤销 / 对勾）。为什么不用图标字体：PUA 码位
// U+E000..U+E011 已分配完毕（见 `icons::Icon` 的 ALL），加字形要重新
// 生成 TTF；而这三个形状用几段线段/三角形就能表达，与 Markdown 徽标
// 同款处理（见 `icons::paint_kind_icon` 的说明）。
//
// 全部按**归一化**坐标（0..1 盒）定义，绘制时映射到目标矩形——
// 与 `icons.rs` 的自绘徽标同一套口径，改尺寸只改传入矩形。

/// 「编辑」图标：斜置铅笔（三角笔尖 + 杆 + 笔尾平口）。
///
/// # 形状为什么是三段（2026-10-22 重画）
///
/// 早前只有「杆 + 笔尖」两段，斜杆末端直接断在半空，16pt 下读起来像
/// 一支**箭**而不是笔——杆与笔尖同向、末端无收口，缺少「这是一支有
/// 尾端的书写工具」的视觉锚点。
///
/// 补上**笔尾平口**（垂直于轴的一小段）后，形状变成
/// 「尖 → 杆 → 尾」三段，这是铅笔最容易辨识的剪影；平口同时把杆的
/// 末端收住，小尺寸下不再显得毛糙。
///
/// 几何全部由**轴向/法向单位向量**推导（而不是写死四组坐标）：改倾角
/// 只改 `ax/ay/bx/by` 两个端点，笔尖与尾口自动跟着走，不会各歪一边。
pub fn paint_pencil_icon(painter: &egui::Painter, r: Rect, color: egui::Color32) {
    let side = r.width().min(r.height());
    if side <= 0.0 || color.a() == 0 {
        return;
    }
    let u = |x: f32, y: f32| pos2(r.min.x + x * side, r.min.y + y * side);
    let w = (side * 0.17).max(1.2);
    let stroke = Stroke::new(w, color);

    // 主轴：笔尖侧 (0.26,0.74) → 笔尾侧 (0.72,0.28)（笔尖在左下）。
    let (ax, ay) = (0.26f32, 0.74f32);
    let (bx, by) = (0.72f32, 0.28f32);
    let (dx, dy) = (bx - ax, by - ay);
    let len = (dx * dx + dy * dy).sqrt();
    if len <= f32::EPSILON {
        return;
    }
    let (ux, uy) = (dx / len, dy / len); // 轴向
    let (nx, ny) = (-uy, ux); // 法向

    // 笔杆。
    painter.add(egui::Shape::line(vec![u(ax, ay), u(bx, by)], stroke));

    // 笔尖（实心三角）：尖端沿轴向前伸出，底边跨在杆的笔尖端上。
    let tip_l = 0.19;
    let shoulder = 0.10;
    painter.add(egui::Shape::convex_polygon(
        vec![
            u(ax + nx * shoulder, ay + ny * shoulder),
            u(ax - ux * tip_l, ay - uy * tip_l),
            u(ax - nx * shoulder, ay - ny * shoulder),
        ],
        color,
        Stroke::NONE,
    ));

    // 笔尾平口：垂直于轴、跨过杆末端的一小段。
    let tail = 0.12;
    painter.add(egui::Shape::line(
        vec![
            u(bx + nx * tail, by + ny * tail),
            u(bx - nx * tail, by - ny * tail),
        ],
        Stroke::new((w * 0.92).max(1.2), color),
    ));
}

/// 「撤销」图标：回旋箭头（弧线 + 箭头）。
///
/// 弧从右侧起、越过顶部、收到左上；箭头在弧的**左上端**、沿切线指向
/// 左下——「转回去」的读法。数学标准角（y 向上）参数化，映射时翻转 y。
pub fn paint_undo_icon(painter: &egui::Painter, r: Rect, color: egui::Color32) {
    let side = r.width().min(r.height());
    if side <= 0.0 || color.a() == 0 {
        return;
    }
    let cx = r.min.x + 0.5 * side;
    let cy = r.min.y + 0.52 * side;
    let rad = 0.34 * side;
    // 弧：θ ∈ [-32°, 196°]（2026-10-22 加长）。
    //
    // 早前只跨 180°，两端都收在圆的**上半部**，剪影接近一个「C」；
    // 拉到约 228° 后末端明显绕到左下，读起来才是「转回去」的回旋箭头，
    // 与铅笔的斜置笔杆也能一眼区分（此前两者在 16pt 下都像斜线）。
    let a0 = -32.0f32;
    let a1 = 196.0f32;
    let steps = 30usize;
    let mut pts = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        let th = a0.to_radians() + t * (a1 - a0).to_radians();
        pts.push(pos2(cx + th.cos() * rad, cy - th.sin() * rad));
    }
    painter.add(egui::Shape::line(
        pts,
        Stroke::new((side * 0.15).max(1.2), color),
    ));
    // 箭头：尖 = 弧终点；两翼 = 尖 - 切线×l ± 法线×h。
    let th = a1.to_radians();
    let tip = pos2(cx + th.cos() * rad, cy - th.sin() * rad);
    let tan = pos2(-th.sin(), -th.cos()); // d/dθ（已含 y 翻转）
    let nrm = pos2(-tan.y, tan.x);
    // 箭头略微加大：小尺寸下三角太小会与弧线糊成一团。
    let l = 0.23 * side;
    let h = 0.13 * side;
    let base = pos2(tip.x - tan.x * l, tip.y - tan.y * l);
    painter.add(egui::Shape::convex_polygon(
        vec![
            tip,
            pos2(base.x + nrm.x * h, base.y + nrm.y * h),
            pos2(base.x - nrm.x * h, base.y - nrm.y * h),
        ],
        color,
        Stroke::NONE,
    ));
}

/// 「确认」图标：对勾（两段线）。
pub fn paint_check_icon(painter: &egui::Painter, r: Rect, color: egui::Color32) {
    let side = r.width().min(r.height());
    if side <= 0.0 || color.a() == 0 {
        return;
    }
    let u = |x: f32, y: f32| pos2(r.min.x + x * side, r.min.y + y * side);
    painter.add(egui::Shape::line(
        vec![u(0.18, 0.55), u(0.42, 0.78), u(0.84, 0.24)],
        Stroke::new((side * 0.16).max(1.2), color),
    ));
}
