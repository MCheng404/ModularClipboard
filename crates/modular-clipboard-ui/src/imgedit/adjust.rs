//! 影调与色彩调整：亮度 / 对比度 / 饱和度 / 色温 / 伽马。
//!
//! `apply_color` 是逐像素核心，`apply_transforms` 把旋转翻转、
//! 调色、裁剪串成一条管线；预览与保存走同一份代码、不同输入。

use super::{CropRect};

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
