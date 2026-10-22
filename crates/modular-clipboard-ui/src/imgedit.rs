//! 图片编辑窗口（2026-10-21 新增）。
//!
//! # 定位
//!
//! 主窗口卡片左下角的「编辑图片」按钮打开它：一个独立小窗口，支持
//! **简单**变换——90° 旋转（左/右）、水平/垂直翻转、亮度、对比度。
//! 「保存」经 `Op::CommitEditImage` 落回原条目（`Store::update_content`
//! 原地更新，保留置顶 / 分组 / 使用计数）。
//!
//! # 为什么是独立窗口而不是内联
//!
//! 文本编辑可以「预览带就地换成输入框」（内容还是文字），图片编辑
//! 需要画布 + 工具条 + 参数面板——这些东西塞不进列表行里。做成独立
//! 窗口还顺带得到：不挤占列表空间、可同时开着对照原图（主窗口仍在）、
//! 关闭即释放资源（与设置窗口同款「即关即毁」）。
//!
//! # 预览与保存的不同源
//!
//! 预览用**降采样**副本（最长边 ≤ [`PREVIEW_MAX_EDGE`]，
//! `thumbnail::decode_rgba`）：拖动滑块时每帧重算，全分辨率（4K =
//! 8M 像素）会把 UI 线程卡到几十毫秒一帧。保存用**原图**：同一份
//! 变换代码、不同输入，存回去的是全分辨率结果——预览只为看效果，
//! 画质以保存为准。
//!
//! # 资源纪律
//!
//! 与 `popup::Popup` / `childwin::ChildWindow` 同构：自带 Vulkan 表面、
//! 交换链、独立 `egui::Context`；`Drop` 里先抽干残留 posted 消息、再
//! 等 GPU 空闲、最后销毁表面（三处模块同一套顺序，见 `ChildWindow::drop`
//! 的注释）。

use modular_clipboard_gfx::window::{Window, WindowEvent};
use std::time::Instant;

use crate::popup::{RedrawGate, RedrawQuery};

use crate::multiwindow::{SharedGfx, WindowGfx};
use crate::thumbnail::Thumbnail;

/// 预览源的最长边（像素）。4K 图降到 ~1024 后每帧重算的开销从几十毫秒
/// 降到 1ms 量级，而滑块拖动本来也看不出全分辨率的差别。
pub const PREVIEW_MAX_EDGE: u32 = 1024;

/// 亮度调节范围（加减值，0 = 不变）。与 `image::imageops::brighten` 的
/// 语义一致（逐通道加减、钳 0..255）。
pub const BRIGHTNESS_RANGE: std::ops::RangeInclusive<i32> = -100..=100;
/// 对比度调节范围（0 = 不变，正增负减）。与 `adjust_contrast` 一致。
pub const CONTRAST_RANGE: std::ops::RangeInclusive<f32> = -50.0..=50.0;
/// 饱和度范围（-100 = 完全去色，+100 = 加倍鲜艳）。
pub const SATURATION_RANGE: std::ops::RangeInclusive<f32> = -100.0..=100.0;
/// 色温范围（正 = 暖偏红黄，负 = 冷偏蓝）。
pub const TEMPERATURE_RANGE: std::ops::RangeInclusive<f32> = -100.0..=100.0;
/// 伽马范围（1.0 = 不变）。下限取 0.2 而不是 0：`1/0` 会让 LUT 溢出。
pub const GAMMA_RANGE: std::ops::RangeInclusive<f32> = 0.2..=3.0;
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

/// 窗口的逻辑点尺寸（不含任何边距——它是普通不透明窗口，没有自绘投影）。
const WIN_W: f32 = 560.0;
const WIN_H: f32 = 680.0;

/// 标题行高度（逻辑点）。同时是拖动区的高度。
const TITLE_H: f32 = 36.0;
/// 关闭按钮边长（逻辑点）。
const CLOSE_BTN: f32 = 24.0;
/// 关闭按钮中心距窗口右缘的距离（逻辑点）。
const CLOSE_INSET: f32 = 22.0;

/// 关闭按钮矩形（逻辑点，客户区坐标）。
///
/// 提成一个函数：绘制（`draw_ui`）与**拖动区挖空**（`drag_regions`）
/// 必须用同一份几何，两处各写一次必然漂移——一旦拖动区没跟着改，
/// 关闭按钮就被包进拖动区，点击被系统当拖动吞掉，表现为「✕ 点不动」
/// （2026-10-22 用户反馈）。
fn close_button_rect(window_w: f32) -> egui::Rect {
    egui::Rect::from_center_size(
        egui::pos2(window_w - CLOSE_INSET, TITLE_H * 0.5),
        egui::vec2(CLOSE_BTN, CLOSE_BTN),
    )
}

/// 裁剪矩形，**归一化**坐标（0..1，相对它所在的图像空间）。
///
/// 用归一化而不是像素：同一份参数要给**预览**（≤1024 边）与**保存**
/// （原图 4K）两处用，写死像素必然有一处对不上（2026-10-22）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl CropRect {
    /// 全幅（不裁剪）。
    pub fn full() -> Self {
        Self { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }
    }

    /// 归一化矩形 → 像素矩形（按 `w`/`h` 尺寸展开，并钳进边界）。
    ///
    /// CAUTION: 必须保证结果**至少 1×1** 且落在图内——裁剪出 0 尺寸
    /// 会让后续所有步骤（纹理上传、保存）直接失败，而失败的的表现是
    /// 「画面空白」而不是报错，极难排查。
    fn to_pixels(self, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        if w == 0 || h == 0 {
            return None;
        }
        let x = (self.x.clamp(0.0, 1.0) * w as f32).floor() as i64;
        let y = (self.y.clamp(0.0, 1.0) * h as f32).floor() as i64;
        let x1 = (self.w.clamp(0.0, 1.0) * w as f32 + x as f32).ceil() as i64;
        let y1 = (self.h.clamp(0.0, 1.0) * h as f32 + y as f32).ceil() as i64;
        let (x0, y0) = (x.clamp(0, w as i64), y.clamp(0, h as i64));
        let (x1, y1) = (x1.clamp(0, w as i64), y1.clamp(0, h as i64));
        let (cw, ch) = (x1 - x0, y1 - y0);
        if cw < 1 || ch < 1 {
            return None;
        }
        Some((x0 as u32, y0 as u32, cw as u32, ch as u32))
    }
}

/// 一组图像变换参数（编辑器按钮/滑条的当前状态）。
///
/// 收成结构体而不是平铺参数：`apply_transforms` 的调用点有「预览」与
/// 「保存」两处，散参数必然有一处漏传或传错顺序（`flip_h` / `flip_v`
/// 同为 bool，编译器根本不拦）。
///
/// # 各步骤的执行顺序（2026-10-22）
///
/// ```text
/// 旋转/翻转 → 调色（亮度/对比度/饱和度/色温/伽马） → 裁剪 → 笔画
/// ```
///
/// 裁剪放在调色**之后**、笔画放在**最后**，都是为了**所见即所得**：
/// 用户在画布上看到的就是调色后的图，于是拖出的裁剪框与画下的笔画
/// 都直接落在「当前显示空间」，不需要在做变换时把坐标逆回去。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImgTransform {
    /// 顺时针 90° 次数（0..3）。
    pub quarter: u8,
    /// 水平翻转。
    pub flip_h: bool,
    /// 垂直翻转。
    pub flip_v: bool,
    /// 亮度加减值（0 = 不变）。
    pub brightness: i32,
    /// 对比度（0 = 不变）。
    pub contrast: f32,
    /// 饱和度（-100..100，0 = 不变；-100 = 完全去色）。
    pub saturation: f32,
    /// 色温（-100..100，0 = 不变；正 = 暖，负 = 冷）。
    pub temperature: f32,
    /// 伽马（0.1..4.0，1.0 = 不变）。
    pub gamma: f32,
    /// 裁剪矩形（`None` = 不裁剪）。定义在**调色之后**的图像空间。
    pub crop: Option<CropRect>,
}

impl Default for ImgTransform {
    /// 手写而不是 `derive`：`gamma` 的中性值是 **1.0**，派生出来的
    /// `0.0` 会把整幅图调成纯黑——这种默认值错误在 UI 上表现为「一打开
    /// 编辑器图就没了」，而且滑条显示正常，极难联想到默认值。
    fn default() -> Self {
        Self {
            quarter: 0,
            flip_h: false,
            flip_v: false,
            brightness: 0,
            contrast: 0.0,
            saturation: 0.0,
            temperature: 0.0,
            gamma: 1.0,
            crop: None,
        }
    }
}

/// 一次颜色调整是否**不需要**任何逐像素运算（可整段跳过）。
fn color_is_identity(t: &ImgTransform) -> bool {
    t.brightness == 0
        && t.contrast.abs() <= f32::EPSILON
        && t.saturation.abs() <= f32::EPSILON
        && t.temperature.abs() <= f32::EPSILON
        && (t.gamma - 1.0).abs() <= 0.001
}

/// 逐像素调色：亮度 / 对比度 / 饱和度 / 色温 / 伽马，一次遍历做完。
///
/// # 为什么合并成一次遍历
///
/// 预览源最长边 1024（约 786K 像素），六个参数各自走一遍就是 4.7M 次
/// 像素访问；拖动滑条时每帧都要重算，那会直接把 UI 线程拖到几十毫秒
/// 一帧。合并后只遍历一次，且**全部参数都是中性值时直接整段返回**。
///
/// # 为什么不用 `image` 的组合子
///
/// `image` 有 `brighten` / `adjust_contrast`，但没有饱和度与色温；混用
/// 组合子 + 手写通道运算意味着「一部分参数走库、一部分走手写」，两者
/// 的钳位与舍入口径不一致（实测亮度 -100 时边缘像素差 1~2），预览与
/// 保存虽同源却仍会有细微差别。统一手写一遍更可控。
fn apply_color(buf: &mut [u8], t: &ImgTransform) {
    if color_is_identity(t) {
        return;
    }
    let sat = 1.0 + t.saturation / 100.0;
    let temp = t.temperature / 100.0;
    // 伽马用查表：每像素一次 `powf` 是这段最贵的操作，256 项足够。
    let mut gamma_lut = [0u8; 256];
    let g = t.gamma.clamp(0.1, 4.0);
    if (g - 1.0).abs() > 0.001 {
        for (i, v) in gamma_lut.iter_mut().enumerate() {
            let x = i as f32 / 255.0;
            *v = (x.powf(1.0 / g) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    } else {
        for (i, v) in gamma_lut.iter_mut().enumerate() {
            *v = i as u8;
        }
    }
    let use_gamma = (g - 1.0).abs() > 0.001;
    let contrast_k = if t.contrast.abs() > f32::EPSILON {
        // 与 `image::adjust_contrast` 同款：先线性拉伸再钳位，中性灰
        // （128）附近变化最小，暗部/亮部变化最大。
        let c = t.contrast / 100.0;
        Some(((259.0 * (c + 1.0)) / (255.0 * (1.0 - c))).clamp(0.01, 8.0))
    } else {
        None
    };

    for px in buf.as_chunks_mut::<4>().0 {
        let (r, g2, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
        // 亮度
        let (mut r, mut g2, mut b) = (
            r + t.brightness as f32,
            g2 + t.brightness as f32,
            b + t.brightness as f32,
        );
        // 对比度
        if let Some(k) = contrast_k {
            r = (r - 128.0) * k + 128.0;
            g2 = (g2 - 128.0) * k + 128.0;
            b = (b - 128.0) * k + 128.0;
        }
        // 饱和度：向亮度插值（>1 增艳，<1 去色，0 = 灰度）
        let lum = 0.299 * r + 0.587 * g2 + 0.114 * b;
        r = lum + (r - lum) * sat;
        g2 = lum + (g2 - lum) * sat;
        b = lum + (b - lum) * sat;
        // 色温：暖 = 抬 R 压 B，冷反之
        r += temp * 30.0;
        b -= temp * 30.0;

        px[0] = (r.round() as i32).clamp(0, 255) as u8;
        px[1] = (g2.round() as i32).clamp(0, 255) as u8;
        px[2] = (b.round() as i32).clamp(0, 255) as u8;
        if use_gamma {
            px[0] = gamma_lut[px[0] as usize];
            px[1] = gamma_lut[px[1] as usize];
            px[2] = gamma_lut[px[2] as usize];
        }
    }
}

/// 把一组变换应用到 RGBA8 位图，返回 (新字节, 新宽, 新高)。
///
/// 纯函数：同一份代码同时服务**预览**（降采样源）与**保存**（原图），
/// 只是输入不同。两处各写一份变换必然漂移（预览看的和存下的不一样）。
///
/// `quarter` 只取低 2 位（0..3 = 顺时针 90° 的次数）；尺寸不吻合或
/// 长度不对时返回 `None`（调用方降级，不 panic——UI 线程的 panic 会
/// 带崩整个帧循环）。
pub fn apply_transforms(src: &[u8], w: u32, h: u32, t: &ImgTransform) -> Option<(Vec<u8>, u32, u32)> {
    let ImgTransform {
        quarter,
        flip_h,
        flip_v,
        crop,
        ..
    } = *t;
    if w == 0 || h == 0 {
        return None;
    }
    let expected = (w as usize)
        .checked_mul(h as usize)?
        .checked_mul(4)?;
    if src.len() != expected {
        return None;
    }
    let mut img = image::RgbaImage::from_raw(w, h, src.to_vec())
        .map(image::DynamicImage::ImageRgba8)?;
    // ---- 1. 几何：旋转 / 翻转 ----
    for _ in 0..(quarter % 4) {
        img = img.rotate90();
    }
    if flip_h {
        img = img.fliph();
    }
    if flip_v {
        img = img.flipv();
    }
    // ---- 2. 调色（合并一次遍历）----
    let mut out = img.to_rgba8();
    apply_color(out.as_mut(), t);
    // ---- 3. 裁剪（在调色之后 ⇒ 用户拖的框就是他看到的范围）----
    if let Some(c) = crop {
        let (nw, nh) = out.dimensions();
        let (x, y, cw, ch) = c.to_pixels(nw, nh)?;
        out = image::imageops::crop(&mut out, x, y, cw, ch).to_image();
    }
    let (nw, nh) = out.dimensions();
    if nw == 0 || nh == 0 {
        return None;
    }
    Some((out.into_raw(), nw, nh))
}

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
                    // 标准 source-over 混合（在**未预乘**空间算，egui 的
                    // ColorImage 也是未预乘）。
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

/// 图片编辑窗口。
pub struct ImageEditor<'a> {
    /// Vulkan 表面句柄。必须自己留一份：`FrameRenderer::drop` 不销毁
    /// 表面（见 `ChildWindow::surface` 的说明）。
    surface: ash::vk::SurfaceKHR,
    window: Window,
    gfx: WindowGfx<'a>,
    events: modular_clipboard_gfx::window::EventLoop,
    ctx: egui::Context,
    /// 被编辑的条目。
    entry_id: modular_clipboard_core::EntryId,
    /// 原图（全分辨率裸 RGBA8 + 宽高）。**保存的唯一数据源**。
    original: (Vec<u8>, u32, u32),
    /// 预览源（降采样副本）；`None` = 降采样失败，退化用原图。
    preview_src: Option<Thumbnail>,
    /// 当前变换：顺时针 90° 次数（0..3）。
    /// 全部变换/调色/裁剪参数（预览与保存共用这一份，见
    /// [`ImgTransform`] 的顺序说明）。
    t: ImgTransform,
    /// 显示纹理。参数变化时重算重传（`TextureHandle::set`）。
    tex: Option<egui::TextureHandle>,
    /// 参数是否变脏（需要重算 + 重传）。
    dirty: bool,
    /// 已应用的置顶状态（幂等去重）。
    topmost: bool,
    /// 上一帧是否可见（用于 `just_opened` 判定）。
    was_open: bool,
    /// 重绘闸门（与 [`crate::popup::Popup`] / 设置窗口同款）。
    ///
    /// # 为什么编辑窗口必须有闸门
    ///
    /// 早前本窗口每帧无条件 `run_ui` + `acquire/paint/present`，而它是
    /// 一块**几乎静止**的画面（用户只在拖滑块、点按钮时才需要新帧）。
    /// 主循环空转约 125Hz ⇒ 编辑窗口以同样频率持续向共享的那条
    /// Vulkan 队列提交并 present，与主窗口的提交交替进行——这是
    /// 「编辑窗口一打开，它和主窗口一起闪」的持续负载来源
    /// （2026-10-22 用户反馈）。
    gate: RedrawGate,
    /// 当前工具页。
    tool: ToolTab,
    /// 已落下的笔画（矢量，预览与保存共用同一份）。
    strokes: Vec<Stroke>,
    /// 画笔颜色（RGBA）。
    brush_color: [u8; 4],
    /// 画笔宽度（归一化，相对图像较短边）。
    brush_width: f32,
    /// 画布上正在拖拽的裁剪框（**画布坐标**，两点确定矩形）。
    crop_drag: Option<(egui::Pos2, egui::Pos2)>,
    /// 正在绘制的那一笔在 `strokes` 里的下标（按下时开一笔，拖动追加
    /// 点，松开结束）。
    drawing: Option<usize>,
}

/// 工具页。
///
/// 四个页各自只放自己的控件：全部平铺在 680pt 高的窗口里会挤成一团，
/// 而且「裁剪」与「画笔」还要抢画布的拖拽手势——两者不能同时激活。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolTab {
    /// 旋转 / 翻转 / 亮度 / 对比度。
    #[default]
    Adjust,
    /// 饱和度 / 色温 / 伽马。
    Color,
    /// 拖拽选区裁剪。
    Crop,
    /// 画笔。
    Brush,
}

impl ToolTab {
    pub const ALL: [ToolTab; 4] = [Self::Adjust, Self::Color, Self::Crop, Self::Brush];
    pub fn label(self) -> &'static str {
        match self {
            Self::Adjust => "调整",
            Self::Color => "调色",
            Self::Crop => "裁剪",
            Self::Brush => "画笔",
        }
    }
}

impl<'a> ImageEditor<'a> {
    /// 创建编辑窗口（懒创建：用户点「编辑图片」时才建）。
    ///
    /// `bytes` / `dims` 是**原图**（调用方已从 store 读出）。降采样失败
    /// 不阻断创建——预览退化用原图（慢但能看）。
    pub fn create(
        entry_id: modular_clipboard_core::EntryId,
        bytes: Vec<u8>,
        dims: (u32, u32),
        pos: egui::Vec2,
        scale_factor: f32,
        shared: &SharedGfx<'a>,
    ) -> anyhow::Result<Self> {
        let s = if scale_factor > 0.01 { scale_factor } else { 1.0 };
        let mut window = Window::new("编辑图片", (WIN_W * s).round() as u32, (WIN_H * s).round() as u32)?;
        window.set_max_texture_side(shared.gpu.properties.limits.max_image_dimension2_d);
        // 混合 DPI 多屏：按本窗口真实 scale 复核尺寸（与 ChildWindow 同款）。
        let real_s = window.scale_factor();
        if (real_s - s).abs() > 0.01 {
            window.set_size_physical(
                (WIN_W * real_s).round() as u32,
                (WIN_H * real_s).round() as u32,
            );
        }
        // 定位到调用方给的位置（屏幕逻辑点），并钳进工作区。
        let (w_px, h_px) = window.inner_size_physical();
        let (cx, cy) = crate::presence::clamp_to_work_area(
            window.hwnd(),
            (pos.x * real_s) as i32,
            (pos.y * real_s) as i32,
            w_px as i32,
            h_px as i32,
        );
        window.move_to(cx, cy);
        let ctx = shared.new_context();
        let surface = shared
            .gpu
            .create_surface_for(window.hinstance(), window.hwnd())
            .map_err(|e| anyhow::anyhow!("创建表面失败: {e}"))?;
        let gfx = WindowGfx::new_for(shared, surface)
            .map_err(|e| anyhow::anyhow!("建交换链失败: {e}"))?;
        let events = modular_clipboard_gfx::window::EventLoop::new(&window);
        let preview_src = crate::thumbnail::decode_rgba(&bytes, dims.0, dims.1, PREVIEW_MAX_EDGE).ok();
        Ok(Self {
            surface,
            window,
            gfx,
            events,
            ctx,
            entry_id,
            original: (bytes, dims.0, dims.1),
            preview_src,
            t: ImgTransform::default(),
            tex: None,
            dirty: true,
            topmost: false,
            was_open: false,
            gate: RedrawGate::default(),
            tool: ToolTab::default(),
            strokes: Vec::new(),
            brush_color: [255, 80, 80, 255],
            brush_width: 0.02,
            crop_drag: None,
            drawing: None,
        })
    }

    /// 置顶状态跟随主窗口（幂等）。同 `Popup::apply_topmost`。
    pub fn apply_topmost(&mut self, on: bool) {
        if self.topmost != on {
            self.topmost = on;
            crate::presence::set_topmost(self.window.hwnd(), on);
        }
    }

    /// 被编辑的条目 id。
    pub fn entry_id(&self) -> modular_clipboard_core::EntryId {
        self.entry_id
    }

    /// 当前变换参数（预览与保存共用一份，保证「看的」与「存的」同参）。
    fn transform(&self) -> ImgTransform {
        self.t
    }

    /// 当前预览位图（降采样源经当前变换 + 笔画）。
    ///
    /// 预览源缺失时退回原图——大图会卡，但「看得见」优先于「流畅」。
    fn preview_pixels(&self) -> Option<(Vec<u8>, u32, u32)> {
        let (src, w, h) = match &self.preview_src {
            Some(t) => (t.pixels.as_slice(), t.width, t.height),
            None => (self.original.0.as_slice(), self.original.1, self.original.2),
        };
        let (mut px, nw, nh) = apply_transforms(src, w, h, &self.transform())?;
        apply_strokes(&mut px, nw, nh, &self.strokes);
        Some((px, nw, nh))
    }

    /// 保存用的位图（**原图**经当前变换 + 笔画）。
    ///
    /// 与 [`Self::preview_pixels`] 走同一套变换和同一份笔画（矢量），
    /// 只是输入是全分辨率 ⇒ 存下来的画质以原图为准，不受预览降采样
    /// 影响。两处若各写一份，必然漂移成「预览好看、存下来不一样」。
    fn save_pixels(&self) -> Option<(Vec<u8>, u32, u32)> {
        let (mut px, nw, nh) =
            apply_transforms(&self.original.0, self.original.1, self.original.2, &self.transform())?;
        apply_strokes(&mut px, nw, nh, &self.strokes);
        Some((px, nw, nh))
    }

    /// 同步显隐，需要时绘制一帧。返回 `Err` 只表示本帧渲染失败
    /// （调用方记警告后继续，不中断主循环）。
    pub fn sync_and_render(
        &mut self,
        app: &mut crate::app::App,
        want_open: bool,
    ) -> anyhow::Result<()> {
        let just_opened = !self.was_open && want_open;
        self.was_open = want_open;
        if !want_open {
            self.events.poll_for(None);
            self.events.take_pending();
            self.window.set_visible(false);
            return Ok(());
        }
        if just_opened {
            // 抬到自己所在 band 的顶部（按主窗口置顶状态选 band，
            // 见 `presence::raise_to_top` 2026-10-21 的修复说明）。
            crate::presence::raise_to_top(self.window.hwnd(), self.topmost);
            crate::presence::focus_window(self.window.hwnd());
        }
        self.window.set_visible(true);

        let (cw, ch) = self.window.inner_size_physical();
        if cw == 0 || ch == 0 {
            self.events.poll_for(None);
            self.events.take_pending();
            return Ok(());
        }
        let frame_events = if just_opened {
            // 打开首帧丢弃积压输入（位置/焦点未稳定，残留点击会误触）。
            self.events.poll_for(None);
            let _ = self.events.take_pending();
            Vec::new()
        } else {
            self.events.poll_for(None);
            self.events.take_pending()
        };
        if frame_events
            .iter()
            .any(|e| matches!(e, WindowEvent::CloseRequested))
        {
            // ✕ = 取消（不保存）：把意图交回状态机，不绕过 Op。
            app.paint_state.image_editor = None;
            app.ctx.request_repaint();
            return Ok(());
        }

        // ---- 重绘闸门 ----
        //
        // 与 `popup::Popup` 同一套判据：只有「刚打开 / 有事件 / 指针进出 /
        // 焦点变化 / 尺寸变化 / egui 自己请求了重绘 / 心跳到期」才真的画。
        // 静止时不提交、不 present——编辑窗口与主窗口共享同一条 Vulkan
        // 队列，每帧多一次无谓的提交就是多一次与主窗口交错的机会。
        let q = RedrawQuery {
            now: Instant::now(),
            // 本窗口创建后不移动（位置在建窗时定一次），所以强制项只有
            // 刚打开这一条。
            forced: just_opened,
            has_events: !frame_events.is_empty(),
            pointer_over: modular_clipboard_gfx::chrome::pointer_is_over(self.window.hwnd()),
            focused: self.ctx.memory(|m| m.focused()),
            scale: self.window.scale_factor(),
            extent: (cw, ch),
        };
        if !self.gate.needs_redraw(q) {
            return Ok(());
        }

        let raw_input = self.events.egui_input(&self.ctx, &frame_events);
        crate::theme::apply(&self.ctx, app.palette());
        // CAUTION: 先克隆 Context 再 run_ui——闭包里要 `&mut self`
        // （draw_ui 改变换参数 / 纹理），直接 `self.ctx.run_ui(.., |ui| self.draw_ui(..))`
        // 会撞「self.ctx 已借 + 闭包再借 self」的 E0500。Context 内部是
        // Arc，克隆只是加一次引用计数。
        let ctx = self.ctx.clone();
        let mut out = ctx.run_ui(raw_input, |ui| self.draw_ui(ui, app));
        let ppp = out.pixels_per_point;

        // 上报拖动区（标题行）：必须在 run_ui 之后——区域由本帧布局算出。
        let drag = self.drag_regions();
        self.window.set_drag_region_multi(&drag);

        let mut textures_delta = crate::app::DeltaGuard(std::mem::take(&mut out.textures_delta));
        let primitives = self.ctx.tessellate(std::mem::take(&mut out.shapes), ppp);
        // 闸门记账必须在 `out` 被拆散之前（同 `popup::Popup`）。
        self.gate.note_drawn(q, crate::renderer::repaint_delay(&out));
        drop(out);

        self.gfx.rebuild_if_needed((cw, ch))?;
        let Some(acquired) = self.gfx.fr.acquire()? else {
            // IMPORTANT: 重建**必须带目标尺寸**（同 `multiwindow` 的修复）。
            // 传零尺寸会走 `FALLBACK_EXTENT`(1280×800)，与本窗口真实
            // 客户区不符 ⇒ 下一帧再判不符 ⇒ 无限重建 ⇒ 每帧一次同步。
            self.gfx.fr.rebuild_swapchain(ash::vk::Extent2D {
                width: cw,
                height: ch,
            })?;
            // 交换链刚重建、本帧又没画成 ⇒ 图像内容未定义（新分配的
            // 交换链图像不保证已清零）。闸门若按「刚画过」跳过下一帧，
            // 窗口会黑着/花着最多一个心跳间隔 ⇒ 清掉记账。
            self.gate = RedrawGate::default();
            return Ok(());
        };
        let painted = self.gfx.painter.paint(
            &mut self.gfx.fr,
            &primitives,
            ppp,
            self.window.scale_factor(),
            &mut textures_delta.0,
        );
        if let Err(e) = painted {
            self.gfx.fr.abandon_frame();
            return Err(e);
        }
        let _ = self.gfx.fr.present(acquired)?;
        Ok(())
    }

    /// 标题行的拖动区（逻辑点、客户区坐标）。
    fn drag_regions(&self) -> Vec<egui::Rect> {
        let (w, _h) = self.window.inner_size_points();
        // CAUTION: 必须挖空关闭按钮（2026-10-22 修复）。
        //
        // `chrome::set_drag_regions` 的规则是「落在矩形内的按下会被系统
        // 拿走变成拖动，egui 收不到」。早前这里上报整行 `[0, w] × 36`，
        // 关闭按钮正好落在里面 ⇒ 点击被 Win32 当成拖动 ⇒ **✕ 点不动**
        // （用户反馈）。主窗口顶栏的齿轮/搜索框能点，正是因为它上报的
        // 是「顶栏减去控件」的多段矩形。
        //
        // 关闭按钮贴着右缘，挖掉它只需把右边界收到它的左缘（再让 2pt
        // 免得边界取整后压到按钮上）。
        let close = close_button_rect(w);
        let right = (close.min.x - 2.0).max(0.0);
        if right <= 0.0 {
            return Vec::new();
        }
        vec![egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(right, TITLE_H),
        )]
    }

    /// 画一帧界面。
    fn draw_ui(&mut self, ui: &mut egui::Ui, app: &mut crate::app::App) {
        // CAUTION: 调色板必须**克隆**一份。`app.palette()` 借的是 `&Palette`，
        // 而下面的保存/取消要 `&mut app`（push Op / 改状态）——持有引用
        // 跨可变借用会直接撞借用检查（E0502）。调色板 55 个令牌，克隆
        // 成本可忽略，且只在编辑器开着时发生。
        let pal = app.palette().clone();
        let rect = ui.max_rect();
        // 不透明底（普通窗口，不走磨砂通道）。
        ui.painter().rect_filled(rect, 0.0, pal.bg);

        // ---- 标题行 ----
        //
        // CAUTION: 关闭按钮的几何必须走 `close_button_rect`——
        // `drag_regions` 也用它算挖空范围，两处共用一份才是「画在哪」
        // 与「哪里能点」一致的唯一保证。
        let title_r =
            egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.min.y + TITLE_H));
        ui.painter_at(title_r).rect_filled(title_r, 0.0, pal.surface);
        ui.painter_at(title_r).text(
            egui::pos2(title_r.min.x + 14.0, title_r.center().y),
            egui::Align2::LEFT_CENTER,
            "编辑图片",
            egui::FontId::proportional(pal.font_md),
            pal.text,
        );
        let close_r = close_button_rect(rect.width());
        let cr = ui.interact(close_r, egui::Id::unique("imgedit_close"), egui::Sense::click());
        let close_clicked = cr.clicked();
        let close_hovered = cr.hovered();
        cr.on_hover_text("关闭（不保存）");
        crate::icons::Icon::Close.paint(
            &ui.painter_at(close_r),
            close_r,
            if close_hovered { pal.text_bright } else { pal.text_dim },
        );
        if close_clicked {
            app.paint_state.image_editor = None;
            app.ctx.request_repaint();
            return;
        }
        // Esc = 关闭（不保存）：与主窗口 Esc 取消内联编辑同一套语义
        // （用户对「Esc 放弃当前编辑」的预期是跨窗口一致的）。
        let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if esc {
            app.paint_state.image_editor = None;
            app.ctx.request_repaint();
            return;
        }

        // ---- 画布（剩余空间的上部）----
        let canvas = egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 12.0, title_r.max.y + 12.0),
            egui::pos2(rect.max.x - 12.0, rect.max.y - 190.0),
        );
        if canvas.height() > 8.0 {
            ui.painter_at(canvas)
                .rect_filled(canvas, pal.radius_sm, pal.surface_variant);
            self.paint_canvas(ui, canvas, &pal);
        }

        // ---- 工具区 ----
        let tools = egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 12.0, canvas.max.y + 10.0),
            egui::pos2(rect.max.x - 12.0, rect.max.y - 10.0),
        );
        if tools.height() > 8.0 {
            self.paint_tools(ui, tools, app, &pal);
        }
    }

    /// 画布：纹理按「适应」缩放居中。
    fn paint_canvas(&mut self, ui: &egui::Ui, canvas: egui::Rect, pal: &crate::theme::Palette) {
        // 参数变脏 ⇒ 重算预览像素并重传纹理。
        if self.dirty {
            self.dirty = false;
            if let Some((px, w, h)) = self.preview_pixels() {
                let img = egui::ColorImage {
                    size: [w as usize, h as usize],
                    source_size: egui::vec2(w as f32, h as f32),
                    pixels: px
                        .as_chunks::<4>().0.iter()
                        .map(|p| egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]))
                        .collect(),
                };
                match &mut self.tex {
                    Some(h) => h.set(img, egui::TextureOptions::LINEAR),
                    None => {
                        // CAUTION: `load_texture` 直接返回 `TextureHandle`
                        // （不是 Result）——0.36 的签名，没有 `.ok()` 可调。
                        self.tex = Some(
                            self.ctx
                                .load_texture("imgedit_preview", img, egui::TextureOptions::LINEAR),
                        );
                    }
                }
            }
        }
        let Some(tex) = &self.tex else {
            ui.painter_at(canvas).text(
                canvas.center(),
                egui::Align2::CENTER_CENTER,
                "无法预览",
                egui::FontId::proportional(pal.font_sm),
                pal.text_dim,
            );
            return;
        };
        let (iw, ih) = (tex.size()[0] as f32, tex.size()[1] as f32);
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        // 适应缩放：塞进画布；上限 4× 防「小图被放大到糊成一片」。
        let scale = (canvas.width() / iw).min(canvas.height() / ih).min(4.0);
        let size = egui::vec2(iw * scale, ih * scale);
        let r = egui::Rect::from_center_size(canvas.center(), size);
        ui.painter_at(canvas).image(
            tex.id(),
            r,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );

        // ---- 画布手势 ----
        //
        // 交互区域必须是**图像矩形** `r` 而不是整个 `canvas`：归一化坐标
        // 是相对图像的（`Stroke` 与 `CropRect` 都按 0..1 定义），用 canvas
        // 换算会把四周留白算进去 ⇒ 画的线整体偏移、裁剪框对不上。
        //
        // 只有裁剪/画笔两页接管手势：调整页与调色页仍可点画布查看，
        // 不该因为悬停就误触发绘制。
        let interactive = matches!(self.tool, ToolTab::Crop | ToolTab::Brush);
        let resp = ui.interact(
            r,
            egui::Id::unique("imgedit_canvas"),
            if interactive {
                egui::Sense::click_and_drag()
            } else {
                egui::Sense::hover()
            },
        );
        if !interactive {
            return;
        }
        // 画布坐标 → 图像归一化坐标（0..1）。
        let norm = |p: egui::Pos2| -> (f32, f32) {
            (
                ((p.x - r.min.x) / r.width()).clamp(0.0, 1.0),
                ((p.y - r.min.y) / r.height()).clamp(0.0, 1.0),
            )
        };
        if self.tool == ToolTab::Crop {
            if resp.drag_started()
                && let Some(p) = resp.interact_pointer_pos()
            {
                self.crop_drag = Some((p, p));
            }
            if resp.dragged()
                && let (Some(p), Some((a, _))) = (resp.interact_pointer_pos(), self.crop_drag)
            {
                self.crop_drag = Some((a, p));
            }
            if resp.drag_stopped()
                && let Some((a, b)) = self.crop_drag.take()
            {
                let (ax, ay) = norm(a);
                let (bx, by) = norm(b);
                let x = ax.min(bx);
                let y = ay.min(by);
                let w = (ax - bx).abs().max(0.02);
                let h = (ay - by).abs().max(0.02);
                // 替换而不是叠加：拖出的框就是用户看到的范围，
                // 叠加需要把新框逆变换回旧框之前的空间，既反直觉
                // 又容易算出越界矩形。想重裁就再拖一次。
                self.t.crop = Some(CropRect { x, y, w, h });
                self.dirty = true;
            }
            // 拖拽中的选框：框外压暗，让用户看清将保留哪一块。
            if let Some((a, b)) = self.crop_drag {
                let sel = egui::Rect::from_two_pos(a, b).intersect(r);
                let dim = egui::Color32::from_black_alpha(110);
                for o in [
                    egui::Rect::from_min_max(r.min, egui::pos2(r.max.x, sel.min.y)),
                    egui::Rect::from_min_max(egui::pos2(r.min.x, sel.max.y), r.max),
                    egui::Rect::from_min_max(
                        egui::pos2(r.min.x, sel.min.y),
                        egui::pos2(sel.min.x, sel.max.y),
                    ),
                    egui::Rect::from_min_max(
                        egui::pos2(sel.max.x, sel.min.y),
                        egui::pos2(r.max.x, sel.max.y),
                    ),
                ] {
                    if o.width() > 0.5 && o.height() > 0.5 {
                        ui.painter_at(canvas).rect_filled(o, 0.0, dim);
                    }
                }
                ui.painter_at(canvas).rect_stroke(
                    sel,
                    0.0,
                    egui::Stroke::new(1.5, egui::Color32::WHITE),
                    egui::StrokeKind::Inside,
                );
            }
            return;
        }

        // ---- 画笔 ----
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.strokes.push(Stroke {
                pts: vec![norm(p)],
                color: self.brush_color,
                width: self.brush_width,
            });
            self.drawing = Some(self.strokes.len() - 1);
            self.dirty = true;
        }
        if resp.dragged()
            && let (Some(p), Some(i)) = (resp.interact_pointer_pos(), self.drawing)
            && let Some(s) = self.strokes.get_mut(i)
        {
            let n = norm(p);
            // 与上一点几乎重合时不追加：拖动会产生大量重复点，
            // 让 `strokes` 无谓膨胀（保存时要全部重画一遍）。
            let far = s
                .pts
                .last()
                .is_none_or(|l| (l.0 - n.0).abs() > 0.001 || (l.1 - n.1).abs() > 0.001);
            if far {
                s.pts.push(n);
                self.dirty = true;
            }
        }
        if resp.drag_stopped() {
            self.drawing = None;
        }
    }

    /// 一枚工具按钮（等宽等高，悬停手型）。返回是否被点。
    fn tool_button(&mut self, ui: &mut egui::Ui, label: &str, pal: &crate::theme::Palette) -> bool {
        ui.add_sized(
            egui::vec2(64.0, 26.0),
            egui::Button::new(egui::RichText::new(label).color(pal.text)),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    }

    /// 工具区：变换按钮行 + 两个滑块 + 保存/取消。
    ///
    /// CAUTION: `allocate_ui_at_rect` 在 0.36 的 `Ui` 上不存在，用
    /// `scope_builder` + `max_rect` 在指定矩形里开子 Ui。Op 不在这里
    /// 直接 push——闭包里同时借着 `&mut self`（改变换参数）与
    /// `&mut app`（push / 通知），先收进 Vec 出来后再统一落地。
    fn paint_tools(
        &mut self,
        ui: &mut egui::Ui,
        area: egui::Rect,
        app: &mut crate::app::App,
        pal: &crate::theme::Palette,
    ) {
        let mut ops: Vec<crate::paint::Op> = Vec::new();
        let mut save_failed = false;
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(area)
                .layout(egui::Layout::top_down(egui::Align::Min)),
            |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);

                // ---- 工具页切换 ----
                //
                // 分页而不是全平铺：四个功能全塞进 ~180pt 的工具区会让
                // 每行控件都被压扁，而且**裁剪与画笔要抢画布的拖拽手势**
                // ——同一时刻只能有一个接管，分页天然保证了这一点。
                ui.horizontal(|ui| {
                    for tab in ToolTab::ALL {
                        let on = self.tool == tab;
                        let b = ui.add_sized(
                            egui::vec2(58.0, 24.0),
                            egui::Button::new(
                                egui::RichText::new(tab.label())
                                    .color(if on { pal.text_on_accent } else { pal.text }),
                            )
                            .fill(if on { pal.accent } else { pal.surface_variant }),
                        );
                        if b.clicked() {
                            self.tool = tab;
                            // 切页时丢弃未完成的拖拽：否则在裁剪页拖到一半
                            // 切走，`crop_drag` 还留着，下次回到裁剪页会
                            // 凭空出现一个选框。
                            self.crop_drag = None;
                            self.drawing = None;
                        }
                    }
                });
                ui.separator();

                match self.tool {
                    ToolTab::Adjust => {
                        // ---- 旋转 / 翻转 ----
                        ui.horizontal(|ui| {
                            if self.tool_button(ui, "↺ 左转", pal) {
                                self.t.quarter = (self.t.quarter + 3) % 4;
                                self.dirty = true;
                            }
                            if self.tool_button(ui, "↻ 右转", pal) {
                                self.t.quarter = (self.t.quarter + 1) % 4;
                                self.dirty = true;
                            }
                            if self.tool_button(ui, "⇋ 水平", pal) {
                                self.t.flip_h = !self.t.flip_h;
                                self.dirty = true;
                            }
                            if self.tool_button(ui, "⇅ 垂直", pal) {
                                self.t.flip_v = !self.t.flip_v;
                                self.dirty = true;
                            }
                        });
                        // ---- 亮度 / 对比度 ----
                        ui.horizontal(|ui| {
                            ui.label("亮度");
                            let mut b = self.t.brightness;
                            if ui
                                .add(
                                    egui::Slider::new(&mut b, BRIGHTNESS_RANGE.clone())
                                        .show_value(true),
                                )
                                .changed()
                            {
                                self.t.brightness = b;
                                self.dirty = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("对比");
                            let mut c = self.t.contrast;
                            if ui
                                .add(
                                    egui::Slider::new(&mut c, CONTRAST_RANGE.clone())
                                        .show_value(true),
                                )
                                .changed()
                            {
                                self.t.contrast = c;
                                self.dirty = true;
                            }
                        });
                    }
                    ToolTab::Color => {
                        // ---- 饱和度 / 色温 / 伽马 ----
                        ui.horizontal(|ui| {
                            ui.label("饱和");
                            let mut v = self.t.saturation;
                            if ui
                                .add(
                                    egui::Slider::new(&mut v, SATURATION_RANGE.clone())
                                        .show_value(true),
                                )
                                .changed()
                            {
                                self.t.saturation = v;
                                self.dirty = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("色温");
                            let mut v = self.t.temperature;
                            if ui
                                .add(
                                    egui::Slider::new(&mut v, TEMPERATURE_RANGE.clone())
                                        .show_value(true),
                                )
                                .changed()
                            {
                                self.t.temperature = v;
                                self.dirty = true;
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("伽马");
                            let mut v = self.t.gamma;
                            if ui
                                .add(
                                    egui::Slider::new(&mut v, GAMMA_RANGE.clone())
                                        .show_value(true),
                                )
                                .changed()
                            {
                                self.t.gamma = v;
                                self.dirty = true;
                            }
                        });
                        if self.tool_button(ui, "全部复位", pal) {
                            self.t.saturation = 0.0;
                            self.t.temperature = 0.0;
                            self.t.gamma = 1.0;
                            self.dirty = true;
                        }
                    }
                    ToolTab::Crop => {
                        ui.label(
                            egui::RichText::new("在图上拖出一个框，松手即裁剪")
                                .color(pal.text_dim)
                                .size(pal.font_sm),
                        );
                        ui.horizontal(|ui| {
                            let has = self.t.crop.is_some();
                            let b = ui.add_enabled(
                                has,
                                egui::Button::new(
                                    egui::RichText::new("撤销裁剪").color(pal.text),
                                ),
                            );
                            if b.clicked() {
                                self.t.crop = None;
                                self.dirty = true;
                            }
                        });
                    }
                    ToolTab::Brush => {
                        // ---- 颜色 ----
                        ui.horizontal(|ui| {
                            ui.label("颜色");
                            for c in BRUSH_PRESETS {
                                let col = egui::Color32::from_rgba_unmultiplied(
                                    c[0], c[1], c[2], c[3],
                                );
                                let on = self.brush_color == c;
                                let (rect, r) = ui.allocate_exact_size(
                                    egui::vec2(20.0, 20.0),
                                    egui::Sense::click(),
                                );
                                ui.painter().rect_filled(rect, 3.0, col);
                                ui.painter().rect_stroke(
                                    rect,
                                    3.0,
                                    egui::Stroke::new(
                                        if on { 2.0 } else { 1.0 },
                                        if on { pal.accent } else { pal.text_dim },
                                    ),
                                    egui::StrokeKind::Inside,
                                );
                                if r.clicked() {
                                    self.brush_color = c;
                                }
                            }
                        });
                        // ---- 粗细 ----
                        ui.horizontal(|ui| {
                            ui.label("粗细");
                            let mut v = self.brush_width;
                            if ui
                                .add(
                                    egui::Slider::new(&mut v, BRUSH_WIDTH_RANGE.clone())
                                        .show_value(false),
                                )
                                .changed()
                            {
                                self.brush_width = v;
                            }
                        });
                        ui.horizontal(|ui| {
                            if self.tool_button(ui, "撤销一笔", pal) {
                                self.strokes.pop();
                                self.dirty = true;
                            }
                            if self.tool_button(ui, "清空", pal) {
                                self.strokes.clear();
                                self.dirty = true;
                            }
                        });
                    }
                }
                // ---- 保存 / 取消 ----
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let save = ui.add_sized(
                            egui::vec2(96.0, 30.0),
                            egui::Button::new(egui::RichText::new("保存").color(pal.text_on_accent))
                                .fill(pal.accent),
                        );
                        if save.clicked() {
                            match self.save_pixels() {
                                Some((bytes, w, h)) => {
                                    ops.push(crate::paint::Op::CommitEditImage {
                                        id: self.entry_id,
                                        bytes,
                                        dims: (w, h),
                                    })
                                }
                                None => save_failed = true,
                            }
                        }
                        let cancel = ui.add_sized(
                            egui::vec2(80.0, 30.0),
                            egui::Button::new(egui::RichText::new("取消").color(pal.text)),
                        );
                        if cancel.clicked() {
                            ops.push(crate::paint::Op::CloseImageEditor);
                        }
                    });
                });
            },
        );
        if save_failed {
            app.notify("保存失败：图像数据异常".into());
        }
        for op in ops {
            app.paint_state.push(op);
        }
        // 关窗意图（✕ / 取消）落地后 `image_editor` 变 None：主窗口的
        // 顶栏/列表还停在「编辑中」的旧状态，而它的 ctx 收不到本窗事件
        // ⇒ 必须显式请求一帧，否则两边状态差一拍。
        if app.paint_state.image_editor.is_none() {
            app.ctx.request_repaint();
        }
    }
}

impl<'a> Drop for ImageEditor<'a> {
    fn drop(&mut self) {
        // 与 `Popup` / `ChildWindow` 同款三件事，顺序不能换：
        // 抽干残留 posted 消息 → 等 GPU 空闲 → 销毁表面。
        self.events.drain_pending();
        self.gfx.gpu.wait_idle();
        unsafe {
            self.gfx.gpu.destroy_surface(self.surface);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2×2  RGBA8：四个角不同色，用来验证旋转/翻转的**方向**。
    fn sample() -> (Vec<u8>, u32, u32) {
        // (0,0)=红 (1,0)=绿
        // (0,1)=蓝 (1,1)=白
        let px = vec![
            255, 0, 0, 255, // (0,0) 红
            0, 255, 0, 255, // (1,0) 绿
            0, 0, 255, 255, // (0,1) 蓝
            255, 255, 255, 255, // (1,1) 白
        ];
        (px, 2, 2)
    }

    fn at(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    #[test]
    fn identity_transform_is_byte_identical() {
        let (px, w, h) = sample();
        let (out, ow, oh) =
            apply_transforms(&px, w, h, &ImgTransform::default()).expect("恒等变换不应失败");
        assert_eq!(out, px);
        assert_eq!((ow, oh), (w, h));
    }

    #[test]
    fn rotate90_swaps_dims_and_moves_pixels() {
        let (px, w, h) = sample();
        let (out, ow, oh) =
            apply_transforms(
                &px,
                w,
                h,
                &ImgTransform {
                    quarter: 1,
                    ..ImgTransform::default()
                },
            )
            .expect("旋转不应失败");
        assert_eq!((ow, oh), (2, 2), "2×2 旋转后仍是 2×2（尺寸互换）");
        // 顺时针 90°：原 (x,y) → (h-1-y, x)。
        // 原 (0,0) 红 → 新 (1,0)；原 (1,0) 绿 → 新 (1,1)。
        assert_eq!(at(&out, ow, 1, 0), [255, 0, 0, 255], "红应到右上");
        assert_eq!(at(&out, ow, 1, 1), [0, 255, 0, 255], "绿应到右下");
    }

    #[test]
    fn flips_mirror_pixels() {
        let (px, w, h) = sample();
        let (out, _, _) =
            apply_transforms(
                &px,
                w,
                h,
                &ImgTransform {
                    flip_h: true,
                    ..ImgTransform::default()
                },
            )
            .expect("翻转不应失败");
        assert_eq!(at(&out, w, 1, 0), [255, 0, 0, 255], "水平翻转后红在右上");
        let (out, _, _) =
            apply_transforms(
                &px,
                w,
                h,
                &ImgTransform {
                    flip_v: true,
                    ..ImgTransform::default()
                },
            )
            .expect("翻转不应失败");
        assert_eq!(at(&out, w, 0, 0), [0, 0, 255, 255], "垂直翻转后蓝在左上");
    }

    #[test]
    fn brightness_and_contrast_apply() {
        let (px, w, h) = sample();
        // 亮度 +50：红 (255,0,0) 的 R 通道钳在 255，G/B 从 0 到 50。
        let (out, _, _) =
            apply_transforms(
                &px,
                w,
                h,
                &ImgTransform {
                    brightness: 50,
                    ..ImgTransform::default()
                },
            )
            .expect("调亮不应失败");
        assert_eq!(at(&out, w, 0, 0), [255, 50, 50, 255]);
        // 对比度 -50：纯白 (255,255,255) 被压灰（<255）。
        let (out, _, _) =
            apply_transforms(
                &px,
                w,
                h,
                &ImgTransform {
                    contrast: -50.0,
                    ..ImgTransform::default()
                },
            )
            .expect("调对比不应失败");
        let white = at(&out, w, 1, 1);
        assert!(white[0] < 250 && white[0] > 100, "白应被压灰：{white:?}");
    }

    #[test]
    fn bad_input_returns_none_not_panic() {
        // 长度不吻合（UI 线程的 panic 会带崩帧循环，必须返回 None）。
        let (px, w, h) = sample();
        assert!(apply_transforms(&px[..8], w, h, &ImgTransform::default()).is_none());
        // 零尺寸。
        assert!(apply_transforms(&[], 0, 0, &ImgTransform::default()).is_none());
        // quarter 取模：5 次 = 1 次。
        let q1 = ImgTransform {
            quarter: 1,
            ..ImgTransform::default()
        };
        let q5 = ImgTransform {
            quarter: 5,
            ..ImgTransform::default()
        };
        let (a, _, _) = apply_transforms(&px, w, h, &q1).unwrap();
        let (b, _, _) = apply_transforms(&px, w, h, &q5).unwrap();
        assert_eq!(a, b, "quarter 只应取低 2 位");
    }

    // ------------------------------------------------------------------
    // 2026-10-22 新增：裁剪 / 调色 / 笔画
    // ------------------------------------------------------------------

    /// 8×8 **不透明**纯灰（便于观察调色与笔画的影响）。
    ///
    /// CAUTION: 不能用 `vec![128u8; 8*8*4]`——那会把 alpha 也填成 128，
    /// 得到一张半透明图。多数断言不在乎 alpha，但「调色 LUT 是否作用到
    /// alpha 通道」这类回归会被它掩盖。
    fn gray8() -> (Vec<u8>, u32, u32) {
        let mut v = Vec::with_capacity(8 * 8 * 4);
        for _ in 0..(8 * 8) {
            v.extend_from_slice(&[128, 128, 128, 255]);
        }
        (v, 8, 8)
    }

    #[test]
    fn gray8_helper_is_solid_gray() {
        // `gray8` 的自我守门：它是下面多个测试的地基，若被误改成非
        // 均匀填充，`neutral_color_does_not_touch_pixels` 之类的断言会
        // 全部失去意义（"没变"可能只是因为两边都错了）。
        let (px, w, h) = gray8();
        assert_eq!((w, h), (8, 8));
        assert!(px.as_chunks::<4>().0.iter().all(|p| *p == [128u8, 128, 128, 255]));
    }

    #[test]
    fn default_gamma_is_one_not_zero() {
        // 回归守门：`ImgTransform` 的 `Default` 一旦改回 `derive`，
        // gamma 会变成 0.0 ⇒ 整幅图被调成纯黑，而滑条仍显示正常。
        // 这种「UI 看起来没问题但图没了」的故障极难定位，必须有测试。
        assert_eq!(ImgTransform::default().gamma, 1.0);
    }

    #[test]
    fn neutral_color_does_not_touch_pixels() {
        let (px, w, h) = gray8();
        let (out, _, _) = apply_transforms(&px, w, h, &ImgTransform::default()).unwrap();
        assert_eq!(out, px, "全部调色参数为中性时应逐字节不变");
    }

    #[test]
    fn saturation_minus_100_produces_gray() {
        let (px, w, h) = (
            {
                let mut v = Vec::new();
                for _ in 0..(4 * 4) {
                    v.extend_from_slice(&[200u8, 60, 30, 255]);
                }
                v
            },
            4u32,
            4u32,
        );
        let (out, _, _) =
            apply_transforms(&px, w, h, &ImgTransform { saturation: -100.0, ..Default::default() })
                .unwrap();
        // 完全去色后 R=G=B。
        for p in out.as_chunks::<4>().0.iter() {
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            assert!(
                (r - g).abs() <= 1 && (g - b).abs() <= 1,
                "去色后三通道应相等，实得 {r}/{g}/{b}"
            );
        }
    }

    #[test]
    fn crop_shrinks_to_requested_pixels() {
        let (px, w, h) = gray8();
        let (out, ow, oh) = apply_transforms(
            &px,
            w,
            h,
            &ImgTransform {
                crop: Some(CropRect { x: 0.25, y: 0.5, w: 0.5, h: 0.25 }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((ow, oh), (4, 2), "8×8 上裁 (0.25,0.5,0.5,0.25) 应得 4×2");
        assert_eq!(out.len(), (ow * oh * 4) as usize);
    }

    #[test]
    fn out_of_range_crop_is_clamped_not_rejected() {
        // 越界裁剪必须被钳成图内的合法矩形，而不是返回 None（那会让
        // 编辑器整块空白，而 UI 上看不出原因）。
        let (px, w, h) = gray8();
        let (out, ow, oh) = apply_transforms(
            &px,
            w,
            h,
            &ImgTransform {
                crop: Some(CropRect { x: -0.5, y: -0.5, w: 2.0, h: 2.0 }),
                ..Default::default()
            },
        )
        .unwrap();
        assert!((1..=8).contains(&ow) && (1..=8).contains(&oh), "尺寸应被钳进图内");
        assert_eq!(out.len(), (ow * oh * 4) as usize);
    }

    #[test]
    fn strokes_use_normalized_coords_at_any_resolution() {
        // 这是「矢量存储」的核心不变量：同一笔在预览（小图）与保存
        // （原图）上必须落在**同一个相对位置**。若笔画按像素存，这个
        // 测试会在第二种分辨率上失败。
        let s = vec![Stroke {
            pts: vec![(0.5, 0.5)],
            color: [255, 0, 0, 255],
            width: 0.2,
        }];
        for (w, h) in [(8u32, 8u32), (64u32, 64u32), (64u32, 32u32)] {
            let mut px = vec![0u8; (w * h * 4) as usize];
            apply_strokes(&mut px, w, h, &s);
            let i = (((h / 2) * w + w / 2) * 4) as usize;
            assert_eq!(px[i], 255, "中心点应被涂红 @ {w}×{h}");
            assert_eq!(px[i + 1], 0, "绿色通道应为 0 @ {w}×{h}");
        }
    }

    #[test]
    fn single_point_stroke_still_marks_a_dot() {
        // 单击（没有拖动）也要留下一个点：pts 只有一项时不能走
        // `windows(2)` 那条路径（那会什么都不画）。
        let (w, h) = (16u32, 16u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        apply_strokes(
            &mut px,
            w,
            h,
            &[Stroke { pts: vec![(0.25, 0.75)], color: [0, 0, 255, 255], width: 0.1 }],
        );
        let i = (((12 * w) + 4) * 4) as usize;
        assert_eq!(px[i + 2], 255, "单击处应出现蓝色点");
    }

    #[test]
    fn empty_and_zero_width_strokes_are_noop() {
        let (w, h) = (8u32, 8u32);
        let base = vec![7u8; (w * h * 4) as usize];
        let mut px = base.clone();
        apply_strokes(&mut px, w, h, &[]);
        apply_strokes(
            &mut px,
            w,
            h,
            &[Stroke { pts: vec![(0.5, 0.5)], color: [1, 2, 3, 255], width: 0.0 }],
        );
        assert_eq!(px, base, "空笔画与零宽度都不应改动像素");
    }
}
