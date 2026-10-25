//! 画笔标注：归一化点列与光栅化。
//!
//! 存归一化点列（而不是烧进预览像素），保存时按目标尺寸
//! 重新光栅化，画质以保存为准。

/// 画笔宽度（归一化，相对图像**较短边**）。
pub const BRUSH_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 0.003..=0.08;
/// 画笔预设色（RGBA）。红/橙/黄/绿/蓝/白 + 黑，覆盖标注的常见用途。
pub const BRUSH_PRESETS: [[u8; 4]; 7] = [
    [239, 68, 68, 255],
    [249, 115, 22, 255],
    [234, 179, 8, 255],
    [34, 197, 94, 255],
    [59, 130, 246, 255],
    [255, 255, 255, 255],
    [17, 17, 17, 255],
];

/// 一笔画。
///
/// # 为什么存**矢量**而不是直接画进像素
///
/// 预览（≤1024 边）与保存（原图 4K）的分辨率差好几倍。若把笔画直接
/// 烧进预览像素，保存时就只剩这张低分辨率的图——「画的时候很清楚，
/// 存下来全是马赛克」。
///
/// 存归一化点列则两边各自按目标尺寸光栅化，画质以保存为准，与变换
/// 参数（也是归一化/参数化）同一套思路。
#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    /// 归一化点列（0..1，相对**裁剪之后**的图像空间）。
    pub pts: Vec<(f32, f32)>,
    /// 颜色 RGBA。
    pub color: [u8; 4],
    /// 笔宽（归一化，相对图像**较短边**）。
    pub width: f32,
}

/// 在 RGBA8 位图上光栅化所有笔画（原地修改）。
///
/// 画法是「沿线段步进落一个实心圆」：圆比线段端点拼接更匀，起笔与
/// 收笔也不会缺角。步进取半径的一半，保证相邻两圆重叠、不出现串珠。
///
/// `width` 相对**较短边**换算，这样横幅与竖幅的笔感一致（若相对长边，
/// 细长图上画笔会明显变粗）。
pub fn apply_strokes(pixels: &mut [u8], w: u32, h: u32, strokes: &[Stroke]) {
    if w == 0 || h == 0 {
        return;
    }
    let short = w.min(h) as f32;
    for s in strokes {
        if s.pts.is_empty() || s.width <= 0.0 {
            continue;
        }
        let radius = (s.width * short * 0.5).max(0.6);
        let (cr, cg, cb, ca) = (
            s.color[0] as f32,
            s.color[1] as f32,
            s.color[2] as f32,
            s.color[3] as f32 / 255.0,
        );
        // 单个点也要画（单击应留下一个点，而不是什么都不做）。
        let mut stamp = |cx: f32, cy: f32| {
            let x0 = (cx - radius).floor().max(0.0) as u32;
            let y0 = (cy - radius).floor().max(0.0) as u32;
            let x1 = (cx + radius).ceil().min(w as f32) as u32;
            let y1 = (cy + radius).ceil().min(h as f32) as u32;
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let dx = xx as f32 + 0.5 - cx;
                    let dy = yy as f32 + 0.5 - cy;
                    if dx * dx + dy * dy > radius * radius {
                        continue;
                    }
                    let i = ((yy as usize) * (w as usize) + xx as usize) * 4;
                    if i + 3 >= pixels.len() {
                        continue;
                    }
                    // 标准 source-over 混合。这里操作的是 `preview_pixels`
                    // 产出的**原始 RGBA 字节**（未预乘），所以按未预乘
                    // 公式算；只有在把结果交给 egui 时才由
                    // `from_rgba_unmultiplied` 转成 `Color32` 的预乘存储。
                    //
                    // CAUTION: 别把「ColorImage 是未预乘的」当真——
                    // `ColorImage::pixels` 是 `Color32`，内部**按预乘存**
                    // （`r()` 返回已乘分量）。混淆这两者就是本仓踩过的
                    // 「二次预乘、颜色整体发暗」（见 `paint::shadow`）。
                    let inv = 1.0 - ca;
                    pixels[i] = (cr * ca + pixels[i] as f32 * inv).clamp(0.0, 255.0) as u8;
                    pixels[i + 1] = (cg * ca + pixels[i + 1] as f32 * inv).clamp(0.0, 255.0) as u8;
                    pixels[i + 2] = (cb * ca + pixels[i + 2] as f32 * inv).clamp(0.0, 255.0) as u8;
                    pixels[i + 3] = ((ca * 255.0) + pixels[i + 3] as f32 * inv)
                        .clamp(0.0, 255.0) as u8;
                }
            }
        };
        if s.pts.len() == 1 {
            let (x, y) = s.pts[0];
            stamp(x * w as f32, y * h as f32);
            continue;
        }
        for pair in s.pts.windows(2) {
            let (ax, ay) = (pair[0].0 * w as f32, pair[0].1 * h as f32);
            let (bx, by) = (pair[1].0 * w as f32, pair[1].1 * h as f32);
            let dist = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
            let steps = (dist / (radius * 0.5)).ceil().max(1.0) as u32;
            for i in 0..=steps {
                let t = i as f32 / steps as f32;
                stamp(ax + (bx - ax) * t, ay + (by - ay) * t);
            }
        }
    }
}
