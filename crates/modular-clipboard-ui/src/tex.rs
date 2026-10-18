//! 程序化纹理：二级主题的「质感层」。
//!
//! 二级主题分两层落地：
//! - **颜色层**（`theme::apply_secondary`）：整套令牌混向主题目标色；
//! - **质感层**（本模块）：纸质的纤维噪点、星空紫的星点。
//!
//! # 为什么用程序化纹理而不是贴图
//!
//! 两个主题的质感都是「细密、无方向、低对比」的底纹，用图片资产会
//! 带来三个问题：体积（哪怕 128×128 的 PNG 也要几 KB，而这里是零
//! 字节运行时生成）、授权（外部贴图的许可证要维护）、以及最关键的
//! **不可参数化**——「纹理强度 0.3」没法对一张固定图片表达，只能
//! 整体调 alpha，一调就连对比度一起变了。
//!
//! 程序化生成则天然支持参数：强度就是 tint 的 alpha，密度就是星点
//! 个数，且每帧可从同一份 tile 重复采样（GPU 侧 `Repeat` 环绕）。
//!
//! # 确定性
//!
//! 生成器是纯函数（同样的种子必然同样的像素）：纹理只加载**一次**并
//! 缓存（`UiState`），重启后观感一致；测试可以对像素做断言。
//!
//! # 坐标约定
//!
//! 纹理是**正方形 tile**（[`TILE`]），像素坐标与 UV 都是左上原点。
//! 所有噪声用取模坐标查表 ⇒ 四边无缝衔接，`Repeat` 采样看不到接缝。

/// tile 边长（像素）。同时是 UV 的重复基准（见 [`paint_tiled`]）。
///
/// 96 是折中：再小（32）噪声的周期性会被看出来（同一块 32px 图案
/// 在 800px 高的窗口里重复 25 次，眼睛能数出来）；再大（256）上传
/// 与生成成本翻四倍，而纹理本身是低分辨率的——它是**质感**不是图像，
/// 放大镜下不需要细节。
pub const TILE: usize = 96;

/// 纸质感 tile 在窗口里的逻辑尺寸（pt）。
///
/// 比 [`TILE`] 像素大 4 倍：纸纤维的物理尺度约 0.5~2mm，在 96 DPI
/// 下折合 2~8 逻辑点；取 24pt 意味着一个 tile 覆盖约 4 个「纤维团」，
/// 重复 30+ 次也不显单调。
pub const PAPER_TILE_PT: f32 = 24.0;

/// 星空 tile 的逻辑尺寸（pt）。
///
/// 星点比纤维稀疏得多，tile 放大到 48pt：96px 里撒约 14 颗星，
/// 一块 48pt 的「天区」里有 14 颗，密度接近真实夜空（当然亮度和
/// 分布是风格化的）。
pub const STAR_TILE_PT: f32 = 48.0;

/// 确定性伪随机数（xorshift32）。
///
/// 不自带 crate：只需要一个能复现的序列，xorshift 的状态转移足够。
/// 种子相同的序列必然相同——这是「纹理只生成一次、重启后观感一致」
/// 与「测试可断言」的基础。
struct Rng(u32);

impl Rng {
    fn new(seed: u32) -> Self {
        // 0 是 xorshift 的不动点（永远输出 0），避开。
        Self(seed | 1)
    }

    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// `[0, 1)` 浮点。
    fn unit(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }
}

/// 周期 value noise：`[0,1)`，在 `TILE × TILE` 上**四边无缝**。
///
/// 实现：整数格点上撒 hash 随机值，双线性插值；查表坐标对 [`TILE`]
/// 取模 ⇒ 格点首尾相接。两层（细/粗）叠加出纸纹的层次。
fn value_noise(x: usize, y: usize, seed: u32) -> f32 {
    // 细层：4px 一格。
    let cell = 4usize;
    let gx = x / cell;
    let gy = y / cell;
    let fx = (x % cell) as f32 / cell as f32;
    let fy = (y % cell) as f32 / cell as f32;
    let a = hash2(gx % (TILE / cell), gy % (TILE / cell), seed);
    let b = hash2((gx + 1) % (TILE / cell), gy % (TILE / cell), seed);
    let c = hash2(gx % (TILE / cell), (gy + 1) % (TILE / cell), seed);
    let d = hash2((gx + 1) % (TILE / cell), (gy + 1) % (TILE / cell), seed);
    let top = a + (b - a) * smooth(fx);
    let bot = c + (d - c) * smooth(fx);
    top + (bot - top) * smooth(fy)
}

/// 平滑插值核（smoothstep 的 3t²-2t³ 变体，一阶连续即可）。
fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// 二维整数 hash → `[0, 1)`。
fn hash2(x: usize, y: usize, seed: u32) -> f32 {
    let mut h = seed
        ^ (x as u32).wrapping_mul(0x27D4_EB2D)
        ^ (y as u32).wrapping_mul(0x1656_67B1);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// 生成纸质感 tile：细密噪点 + 少量横向纤维。
///
/// 输出为**白色**、alpha 随噪点起伏（tint 在上色时乘进去，见
/// [`paint_tiled`]）：这样同一份 tile 在深浅两套下只需换 tint 颜色，
/// 不必生成两份纹理。
fn paper_pixels() -> Vec<u8> {
    let mut px = vec![0u8; TILE * TILE * 4];
    // 细层（4px 格）+ 粗层（16px 格）叠加：单层 noise 太均匀，
    // 像磨砂玻璃；两层才读出「纸」的颗粒感。
    for y in 0..TILE {
        for x in 0..TILE {
            let fine = value_noise(x, y, 0x9E37_79B9);
            let coarse = value_noise(x / 4, y / 4, 0x7F4A_7C15);
            // 中值 0.5 附近波动 ⇒ alpha 以 128 为轴、幅度 ±64。
            let n = fine * 0.65 + coarse * 0.35;
            let a = (128.0 + (n - 0.5) * 128.0).clamp(0.0, 255.0) as u8;
            let i = (y * TILE + x) * 4;
            px[i] = 255;
            px[i + 1] = 255;
            px[i + 2] = 255;
            px[i + 3] = a;
        }
    }
    // 横向纤维：十几条短划，方向略斜、亮度略高——纸纹里最可辨的
    // 线索其实是这些「一丝一丝」的反光，纯噪点读起来像磨砂。
    let mut rng = Rng::new(PAPER_FIBER_SEED);
    for _ in 0..14 {
        let x0 = rng.unit() * TILE as f32;
        let y0 = rng.unit() * TILE as f32;
        let len = 6.0 + rng.unit() * 14.0;
        let slope = (rng.unit() - 0.5) * 0.25;
        let bright = 180 + (rng.unit() * 60.0) as u8;
        for t in 0..(len as usize) {
            let x = (x0 as usize + t) % TILE;
            let y = (y0 + slope * t as f32).rem_euclid(TILE as f32) as usize % TILE;
            let i = (y * TILE + x) * 4;
            px[i + 3] = px[i + 3].saturating_add(bright / 3);
        }
    }
    px
}

/// 纸纤维的随机种子（固定值：纹理必须可复现）。
const PAPER_FIBER_SEED: u32 = 0x51ED_2701;

/// 生成星空 tile：稀疏星点，亮度/半径随机。
///
/// 每颗星 = 中心实点 + 一圈衰减光晕（手写径向衰减，不用高斯——
/// 3px 半径上手写核与高斯无视觉差别，但便宜一个数量级）。
fn star_pixels() -> Vec<u8> {
    let mut px = vec![0u8; TILE * TILE * 4];
    let mut rng = Rng::new(STAR_BASE_SEED);
    for _ in 0..14 {
        let cx = rng.unit() * TILE as f32;
        let cy = rng.unit() * TILE as f32;
        let r = 1.0 + rng.unit() * 1.6;
        let core = 200 + (rng.unit() * 55.0) as u8;
        let halo = (core as f32 * 0.35) as u8;
        let x0 = (cx - r - 1.0).max(0.0) as usize;
        let x1 = ((cx + r + 1.0).min(TILE as f32 - 1.0)) as usize;
        let y0 = (cy - r - 1.0).max(0.0) as usize;
        let y1 = ((cy + r + 1.0).min(TILE as f32 - 1.0)) as usize;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let d = (dx * dx + dy * dy).sqrt();
                let a = if d <= r {
                    core
                } else if d <= r + 1.0 {
                    halo
                } else {
                    continue;
                };
                let i = (y * TILE + x) * 4;
                px[i] = 255;
                px[i + 1] = 255;
                px[i + 2] = 255;
                // 星点可能重叠：取更亮者（叠加会糊成团）。
                px[i + 3] = px[i + 3].max(a);
            }
        }
    }
    px
}

/// 星空种子（固定值）。
const STAR_BASE_SEED: u32 = 0x5741_5ED2;

/// 惰性加载纸质感 tile（每个 `Context` 一份）。
///
/// 只生成一次：`load_texture` 用同名覆盖语义，但没必要每帧调——
/// 生成是 96×96 的 CPU 循环，缓存后零成本。
pub fn paper_tile(ctx: &egui::Context) -> egui::TextureHandle {
    ctx.load_texture(
        "theme_paper_fibers",
        egui::ColorImage::from_rgba_unmultiplied([TILE, TILE], &paper_pixels()),
        egui::TextureOptions {
            magnification: egui::TextureFilter::Linear,
            minification: egui::TextureFilter::Linear,
            wrap_mode: egui::TextureWrapMode::Repeat,
            mipmap_mode: None,
        },
    )
}

/// 惰性加载星空 tile。语义同 [`paper_tile`]。
pub fn star_tile(ctx: &egui::Context) -> egui::TextureHandle {
    ctx.load_texture(
        "theme_starfield",
        egui::ColorImage::from_rgba_unmultiplied([TILE, TILE], &star_pixels()),
        egui::TextureOptions {
            magnification: egui::TextureFilter::Linear,
            minification: egui::TextureFilter::Linear,
            wrap_mode: egui::TextureWrapMode::Repeat,
            mipmap_mode: None,
        },
    )
}

/// 把 tile 纹理平铺画进 `rect`，整体乘上 `tint`。
///
/// # tint 语义
///
/// egui 的 `image` tint 对纹理的 **rgb 与 alpha 逐通道相乘**：
/// - 纸质感 tile 是白底、alpha 起伏 ⇒ tint = 纸墨色 + 强度 alpha；
///   深浅两套共用一份纹理，只换 tint（见 [`paper_pixels`]）。
/// - 星空 tile 同理 ⇒ tint = 白色 + 密度 alpha。
///
/// # UV 与重复
///
/// UV 取 `[0, w/TILE_PT] × [0, h/TILE_PT]`（**超出 [0,1]**），靠
/// `TextureWrapMode::Repeat` 环绕。`tile_pt` 是 tile 的逻辑尺寸：
/// 窗口 720pt 宽 ⇒ 横向重复 30 次。传 0 或非有限值按 1 处理
/// （至少不画飞）。
pub fn paint_tiled(
    painter: &egui::Painter,
    rect: egui::Rect,
    tex: &egui::TextureHandle,
    tint: egui::Color32,
    tile_pt: f32,
) {
    let tile_pt = if tile_pt.is_finite() && tile_pt > 1.0 {
        tile_pt
    } else {
        1.0
    };
    if rect.width() <= 0.0 || rect.height() <= 0.0 || tint.a() == 0 {
        return;
    }
    let uv = egui::Rect::from_min_max(
        egui::pos2(0.0, 0.0),
        egui::pos2(rect.width() / tile_pt, rect.height() / tile_pt),
    );
    painter.image(tex.id(), rect, uv, tint);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 噪声在 tile 边界必须无缝：`x=0` 与 `x=TILE` 取到同一个格点
    /// ⇒ 值相等。这是 `Repeat` 采样看不到接缝的判据（几何上的保证）。
    #[test]
    fn value_noise_wraps_at_tile_edges() {
        for y in [0usize, 1, 17, 48, 95] {
            let a = value_noise(0, y, 0x9E37_79B9);
            let b = value_noise(TILE, y, 0x9E37_79B9);
            assert!((a - b).abs() < 1e-9, "y={y} 左右边界噪声应相等");
            let c = value_noise(y, 0, 0x9E37_79B9);
            let d = value_noise(y, TILE, 0x9E37_79B9);
            assert!((c - d).abs() < 1e-9, "x={y} 上下边界噪声应相等");
        }
    }

    /// 纸纹 tile：确定性（两次生成逐字节相同）+ alpha 有起伏但不全饱和。
    #[test]
    fn paper_pixels_deterministic_and_varied() {
        let a = paper_pixels();
        let b = paper_pixels();
        assert_eq!(a, b, "同种子必须生成同一份纹理");
        let alphas: Vec<u8> = a.chunks_exact(4).map(|p| p[3]).collect();
        assert!(!alphas.iter().all(|&x| x == 0), "不能全透明");
        assert!(!alphas.iter().all(|&x| x == 255), "不能全不透明");
        let mean: f32 = alphas.iter().map(|&x| x as f32).sum::<f32>() / alphas.len() as f32;
        // 均值应在 128 附近（中轴）——偏离太远说明强度需要重新标定。
        assert!((mean - 128.0).abs() < 40.0, "alpha 均值 {mean} 偏离中轴");
        // RGB 恒白（tint 在上色时乘入，见 `paint_tiled` 的说明）。
        assert!(a.chunks_exact(4).all(|p| p[0] == 255 && p[1] == 255 && p[2] == 255));
    }

    /// 星空 tile：确定性 + 确实有亮点（星点）+ 大部分区域是暗的
    /// （夜空不能是麻子脸）。
    #[test]
    fn star_pixels_deterministic_and_sparse() {
        let a = star_pixels();
        assert_eq!(a, star_pixels(), "同种子必须生成同一份纹理");
        let alphas: Vec<u8> = a.chunks_exact(4).map(|p| p[3]).collect();
        let lit = alphas.iter().filter(|&&x| x > 64).count();
        assert!(lit > 20, "星点太少：{lit}");
        assert!(
            lit < alphas.len() / 8,
            "星点太密：{lit}/{}",
            alphas.len()
        );
    }

    /// tile 尺寸与常量一致（`paint_tiled` 的 UV 依赖它）。
    #[test]
    fn tile_dimensions_match_pixel_buffers() {
        assert_eq!(paper_pixels().len(), TILE * TILE * 4);
        assert_eq!(star_pixels().len(), TILE * TILE * 4);
    }
}
