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
/// 128 是「重复图案可感性」与成本的折中：96 实测下星空的随机分布
/// 会被人眼数出重复的「星座图案」（2026-10-18 星空主题反馈），
/// 128 把重复周期拉到 5~6 个 tile，随机性足够摊薄；256 的上传与
/// 生成成本翻四倍而边际收益很小。
pub const TILE: usize = 128;

/// 纸质感 tile 在窗口里的逻辑尺寸（pt）。
///
/// # 为什么必须与 [`TILE`] **1:1**（2026-10-18 实测修正）
///
/// 初版取 24pt：96px 纹理缩到 24pt 显示 ⇒ 4:1 降采样，线性过滤把
/// 稀疏的斑点与纤维**平均成一层脏雾**（实测整窗标准差 0.8 色阶，
/// 用户反馈「纸质渲染不正常」）。1:1 映射下每个纹素占一个物理像素
/// （ppp=1 时），4px 的噪点格与 6~20px 的纤维都清晰可辨。
///
/// 物理尺度也正好：4px 格 ≈ 4pt ≈ 1.3mm（96 DPI），与真实纸纤维
/// 的斑驳尺度同量级；一个 tile 覆盖 128pt，720pt 宽的窗口重复 5.6 次，
/// 周期性不会被人眼数出来。
pub const PAPER_TILE_PT: f32 = 128.0;

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

/// 生成纸质感 tile：局部斑点 + 少量横向纤维。
///
/// # 为什么均值必须归零（2026-10-18 实测修正）
///
/// 初版把噪声 alpha 的均值放在 128（50%）——那等于「半透明墨均匀蒙
/// 满整个窗口」，实测整窗亮度标准差只有 0.8 色阶、峰值与均值没有
/// 区别：用户看到的就是一层脏雾，不是纸。纸的质感在物理上是**局部
/// 特征**（纤维、斑点、杂质），背景大部分是干净的。因此 alpha 以 0
/// 为基、只有噪声高于均值的区域出墨，峰值约 90；纤维再叠一层。
///
/// 输出为**白色**、alpha 起伏（tint 在上色时乘进去，见
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
            // 中值 0.5 附近波动 ⇒ alpha 以 0 为轴、峰值 90。
            // 阈值 0.5 是硬切：低于它的区域全透明（干净纸面），
            // 高于它的按超出量出墨——「斑驳」而不是「均匀」。
            let n = fine * 0.65 + coarse * 0.35;
            let a = ((n - 0.5) * 200.0).clamp(0.0, 90.0) as u8;
            let i = (y * TILE + x) * 4;
            px[i] = 255;
            px[i + 1] = 255;
            px[i + 2] = 255;
            px[i + 3] = a;
        }
    }
    // 横向纤维：少量短划，方向略斜、亮度略高——纸纹里最可辨的
    // 线索其实是这些「一丝一丝」的反光，纯噪点读起来像磨砂。
    // 密度按 1:1 重标：128² tile 里 8 条 ⇒ 720×700 窗口约 350 条，
    // 初版 14 条是 24pt tile 时代的密度（缩采样 4:1 后约 90 条），
    // 放到 1:1 会连成块状横带（2026-10-18 截图标定）。
    let mut rng = Rng::new(PAPER_FIBER_SEED);
    for _ in 0..8 {
        let x0 = rng.unit() * TILE as f32;
        let y0 = rng.unit() * TILE as f32;
        let len = 6.0 + rng.unit() * 14.0;
        let slope = (rng.unit() - 0.5) * 0.25;
        let bright = 90 + (rng.unit() * 50.0) as u8;
        for t in 0..(len as usize) {
            let x = (x0 as usize + t) % TILE;
            let y = (y0 + slope * t as f32).rem_euclid(TILE as f32) as usize % TILE;
            let i = (y * TILE + x) * 4;
            px[i + 3] = px[i + 3].saturating_add(bright);
        }
    }
    px
}

/// 纸纤维的随机种子（固定值：纹理必须可复现）。
const PAPER_FIBER_SEED: u32 = 0x51ED_2701;

/// 惰性加载纸质感 tile（每个 `Context` 一份）。
///
/// 只生成一次：`load_texture` 用同名覆盖语义，但没必要每帧调——
/// 生成是 128×128 的 CPU 循环，缓存后零成本。
pub fn paper_tile(ctx: &egui::Context) -> egui::TextureHandle {
    ctx.load_texture(
        "theme_paper_fibers",
        egui::ColorImage::from_rgba_unmultiplied([TILE, TILE], &paper_pixels()),
        // CAUTION: 必须是 **REPEAT**——`paint_tiled` 的 UV 超出 [0,1]
        // 靠它环绕。renderer 按本选项选采样器（`UserTexture::wrap_repeat`），
        // 写成 ClampToEdge 的话整个窗口会被 tile 的角落像素拉伸铺满
        // （2026-10-18 纸质的实际故障）。与 `LINEAR_REPEAT` 常量同义，
        // 用常量是为了与 renderer 建采样器的那处同源。
        egui::TextureOptions::LINEAR_REPEAT,
    )
}

/// 星点精灵纹理的边长（像素）：**单颗星**的「照片」。
///
/// 2026-10-19 从「整片星空 tile」改为「精灵 + CPU 星星列表」：tile
/// 方案下，随机化（换一片星）、动态（闪烁）、大小不规则（逐颗半径）
/// 三个需求都做不到——纹理上传后就是静态的。精灵方案下每颗星是一个
/// 小 quad，位置/大小/亮度全部由 CPU 逐帧算，tint 通道（2026-10-18
/// 修好的预乘路径）承载闪烁 alpha。
///
/// 16px 足够：星星本身 1~3pt，16px 容纳核心 + 光晕还有余量。
pub const STAR_SPRITE: usize = 16;

/// 一颗星。
///
/// 坐标为**归一化** [0,1)（相对窗口左上角）：窗口 resize 时星星按
/// 比例跟随，不必重生成列表。半径是逻辑点（不归一化）——它不该随
/// 窗口变大而变大，星星的物理大小是恒定的。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Star {
    /// 归一化 x ∈ [0,1)。
    pub x: f32,
    /// 归一化 y ∈ [0,1)。
    pub y: f32,
    /// 半径（逻辑点）。
    pub radius: f32,
    /// 闪烁相位（弧度）。每颗星不同 ⇒ 不会齐刷刷地闪。
    pub phase: f32,
    /// 闪烁频率（Hz）。很慢——肉眼可辨的「呼吸」而不是频闪。
    pub freq: f32,
    /// 基础亮度 alpha（0..1，含亮度分级：少数亮星、多数暗星）。
    pub base: f32,
}

/// CPU 侧的「一片星空」。
///
/// 生成是纯函数（同种子同列表，见 [`StarField::generate`]），调用方
/// 按 [`StarField::fingerprint_of`] 判断是否需要重建——参数没变就
/// 每帧复用同一份列表，只重算闪烁。
pub struct StarField {
    /// 全部星星（数量由密度参数决定，0 密度 = 空表）。
    pub stars: Vec<Star>,
    /// 生成时的参数指纹（调用方据此判重建）。
    pub fingerprint: String,
}

impl StarField {
    /// 密度参数 → 星星数量（0..1 → 0..[`STAR_COUNT_MAX`]）。
    ///
    /// 上限 220 是 draw call 预算：每颗星一个 quad，220 个 image shape
    /// 经 tessellator 同纹理合批后仍是一批，但 CPU 侧每帧要算 220 次
    /// 三角函数——再密就是「麻点」而不是「星空」了（用户反馈过）。
    pub const COUNT_MAX: usize = 220;

    /// 生成一片星空。
    ///
    /// - `density` / `size_var` 见 [`modular_clipboard_core::config::ThemeParams`]；
    /// - `random` = 0 时**完全确定性**（固定种子，每次启动同一片星，
    ///   回归测试可断言）；> 0 时按比例混入 `entropy` 与位置抖动——
    ///   1.0 时每次重建都是一片新星空。
    /// - `entropy`：重建时的随机熵（调用方传 egui 时间等；random=0 时忽略）。
    pub fn generate(density: f32, random: f32, size_var: f32, entropy: u64) -> Self {
        let density = density.clamp(0.0, 1.0);
        let random = random.clamp(0.0, 1.0);
        let size_var = size_var.clamp(0.0, 1.0);
        let count = (density * Self::COUNT_MAX as f32).round() as usize;
        // 种子：固定基底 ^ 按 random 比例混入的熵。random=0 时熵不参与
        // （乘 0 后异或固定种子，仍是确定性星空）。
        let entropy_mix = (entropy as f64 * f64::from(random)) as u64;
        let seed = 0x5741_5ED2 ^ entropy_mix as u32;
        let mut rng = Rng::new(seed | 1);
        let mut stars = Vec::with_capacity(count);
        for _ in 0..count {
            let mut x = rng.unit();
            let mut y = rng.unit();
            // 位置抖动：random 在 [0,1] 时叠加 ±1% 窗口尺寸的偏移——
            // 破除 hash 均匀分布残留的微弱网格规律。random=0 时无抖动。
            if random > 0.0 {
                x = (x + (rng.unit() - 0.5) * 0.02 * random).rem_euclid(1.0);
                y = (y + (rng.unit() - 0.5) * 0.02 * random).rem_euclid(1.0);
            }
            // 大小：size_var=0 时全部 1.2pt（整齐的哑星），1 时 0.6~3.2pt
            // 全范围随机（读出纵深）。
            let radius = 1.2 + (rng.unit() - 0.5) * 2.6 * size_var;
            // 亮度分级（25% 亮星）：真实星空的亮度分布是指数的，均匀
            // 随机会得到一排一样亮的「珠子」。
            let base = if rng.unit() < 0.25 {
                0.82 + rng.unit() * 0.18
            } else {
                0.35 + rng.unit() * 0.35
            };
            stars.push(Star {
                x,
                y,
                radius: radius.clamp(0.4, 4.0),
                phase: rng.unit() * std::f32::consts::TAU,
                freq: 0.05 + rng.unit() * 0.12,
                base,
            });
        }
        Self {
            stars,
            fingerprint: Self::fingerprint_of(density, random, size_var),
        }
    }

    /// 参数指纹：三个影响列表的参数（密度/随机化/大小）。旋钮的其余
    /// 两个（紫度是颜色层、闪烁是逐帧量）不进指纹。
    pub fn fingerprint_of(density: f32, random: f32, size_var: f32) -> String {
        format!(
            "{:.3}:{:.3}:{:.3}",
            density.clamp(0.0, 1.0),
            random.clamp(0.0, 1.0),
            size_var.clamp(0.0, 1.0)
        )
    }
}

/// 光标视差：星点位置随光标**反向偏移**（纯函数，便于单测）。
///
/// # 语义：让开，而不是吸引
///
/// 光标靠近时星点沿「光标 → 星点」方向被推开——像手电筒照进夜空、
/// 星点轻轻让开。选「推开」而不是「吸引」有两个理由：吸引会把星点
/// 拉成一团（密集处糊成斑），而推开保持分布；且推开的直觉更接近
/// 「光照使东西显形又回避」。
///
/// # 纵深感：距离衰减
///
/// 偏移幅度 = `基准 × 强度 / (1 + 距离/影响半径)`：近的星动得多、
/// 远的几乎不动——这正是视差（parallax）的本义（近处物体位移大）。
/// 影响半径 160pt 约覆盖光标周围一小片，整窗星点不会一起晃。
///
/// # 边界
///
/// - `pointer = None`（光标不在窗口上）⇒ 零偏移，星点回原位；
/// - 光标几乎压在星点上 ⇒ 零偏移（不抖）；
/// - 偏移量钳制到距离的一半——不越过光标，否则视觉上「跳过去」。
pub fn cursor_offset(
    star: &Star,
    pointer: Option<egui::Pos2>,
    area: egui::Rect,
    strength: f32,
) -> egui::Vec2 {
    let strength = strength.clamp(0.0, 1.0);
    if strength <= 0.004 {
        return egui::Vec2::ZERO;
    }
    let Some(p) = pointer else {
        return egui::Vec2::ZERO;
    };
    let c = egui::pos2(
        area.min.x + star.x * area.width(),
        area.min.y + star.y * area.height(),
    );
    let d = c - p; // 光标 → 星点
    let dist = d.length();
    if dist < 0.5 {
        return egui::Vec2::ZERO;
    }
    // 基准幅度 18pt（满强度下近处星点的位移），影响半径 160pt。
    let falloff = 1.0 / (1.0 + dist / 160.0);
    let mag = (18.0 * strength * falloff).min(dist * 0.5);
    d / dist * mag
}

/// 闪烁 alpha（纯函数，便于单测）。
///
/// `twinkle` = 0 时恒定返回 `star.base`（静态星）；1 时在
/// `base × [0, 1]` 之间按每颗星自己的相位与频率呼吸。中间值线性
/// 缩放摆幅——用户要「闪得含蓄一点」时不会得到一个奇怪的曲线。
pub fn twinkle_alpha(star: &Star, twinkle: f32, t: f64) -> f32 {
    let twinkle = twinkle.clamp(0.0, 1.0);
    if twinkle <= 0.004 {
        return star.base;
    }
    let wave = 0.5 + 0.5 * (t * f64::from(star.freq) * std::f64::consts::TAU + f64::from(star.phase)).sin();
    (star.base * (1.0 - twinkle + twinkle * wave as f32)).clamp(0.0, 1.0)
}

/// 生成星点精灵像素：中心实点 + 径向衰减光晕，白底。
///
/// 径向核手写（不用高斯）：16px 上手写核与高斯无视觉差别，便宜一个
/// 数量级。tint 只调 alpha（白星），所以 RGB 恒 255。
fn star_sprite_pixels() -> Vec<u8> {
    let n = STAR_SPRITE;
    let mut px = vec![0u8; n * n * 4];
    let c = (n as f32 - 1.0) * 0.5;
    for y in 0..n {
        for x in 0..n {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt() / (n as f32 * 0.5);
            // 核心（d ≤ 0.25）实点，之外幂衰减到 0。
            let a = if d <= 0.25 {
                255.0
            } else {
                (1.0 - d).max(0.0).powf(2.2) * 220.0
            };
            let i = (y * n + x) * 4;
            px[i] = 255;
            px[i + 1] = 255;
            px[i + 2] = 255;
            px[i + 3] = a as u8;
        }
    }
    px
}

/// 惰性加载星点精灵（每个 `Context` 一份）。语义同 [`paper_tile`]。
///
/// 采样器用默认 LINEAR + Clamp：精灵的 UV 恒为 [0,1]（不超出），
/// 不需要 Repeat。
pub fn star_sprite(ctx: &egui::Context) -> egui::TextureHandle {
    ctx.load_texture(
        "theme_star_sprite",
        egui::ColorImage::from_rgba_unmultiplied([STAR_SPRITE, STAR_SPRITE], &star_sprite_pixels()),
        egui::TextureOptions::LINEAR,
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

    /// 纸纹 tile：确定性（两次生成逐字节相同）+ alpha 以 0 为基、
    /// 有局部峰值（「斑驳」而非「均匀蒙层」，2026-10-18 修正的判据）。
    #[test]
    fn paper_pixels_deterministic_and_varied() {
        let a = paper_pixels();
        let b = paper_pixels();
        assert_eq!(a, b, "同种子必须生成同一份纹理");
        let alphas: Vec<u8> = a.chunks_exact(4).map(|p| p[3]).collect();
        assert!(!alphas.iter().all(|&x| x == 0), "不能全透明");
        assert!(!alphas.iter().all(|&x| x == 255), "不能全不透明");
        let mean: f32 = alphas.iter().map(|&x| x as f32).sum::<f32>() / alphas.len() as f32;
        // 均值必须**接近 0**：均匀蒙层（旧实现的均值 128）实测让整个
        // 窗口像蒙了一层脏雾，纸纹的「斑驳」感来自大面积干净 + 局部出墨。
        assert!(mean < 30.0, "alpha 均值 {mean} 应接近 0（干净纸面为底）");
        // 峰值要够：没有任何超过 60 的像素就只剩底噪、读不出纤维。
        assert!(
            alphas.iter().any(|&x| x > 60),
            "应有纤维级峰值（>60）"
        );
        // RGB 恒白（tint 在上色时乘入，见 `paint_tiled` 的说明）。
        assert!(a.chunks_exact(4).all(|p| p[0] == 255 && p[1] == 255 && p[2] == 255));
    }

    /// 星点精灵：确定性 + 中心亮、边缘透明（径向核的形状判据）。
    #[test]
    fn star_sprite_deterministic_and_radial() {
        let a = star_sprite_pixels();
        assert_eq!(a, star_sprite_pixels(), "精灵是纯函数，两次生成必须相同");
        let n = STAR_SPRITE;
        assert_eq!(a.len(), n * n * 4);
        // 中心像素必须不透明（实点核心）。
        let c = (n / 2) * n + n / 2;
        assert!(a[c * 4 + 3] > 200, "精灵中心应是不透明核心");
        // 四角必须全透明（光晕衰减到 0，否则小星星带方框）。
        for &(x, y) in &[(0usize, 0usize), (n - 1, 0), (0, n - 1), (n - 1, n - 1)] {
            assert_eq!(a[(y * n + x) * 4 + 3], 0, "角点 ({x},{y}) 应全透明");
        }
        // RGB 恒白（闪烁只调 tint alpha）。
        assert!(a.chunks_exact(4).all(|p| p[0] == 255 && p[1] == 255 && p[2] == 255));
    }

    /// 精灵尺寸与常量一致（CPU 侧按半径放大它，两者必须对上）。
    #[test]
    fn tile_dimensions_match_pixel_buffers() {
        assert_eq!(paper_pixels().len(), TILE * TILE * 4);
        assert_eq!(star_sprite_pixels().len(), STAR_SPRITE * STAR_SPRITE * 4);
    }

    // ---- 星星列表（StarField）-----------------------------------------

    /// random=0 时**完全确定性**：同参数两次生成的列表逐字段相等
    /// （回归测试与「固定一片星空」的产品语义都依赖这条）。
    #[test]
    fn star_field_deterministic_at_zero_random() {
        let a = StarField::generate(0.5, 0.0, 0.5, 0);
        let b = StarField::generate(0.5, 0.0, 0.5, 999_999);
        assert_eq!(a.stars, b.stars, "random=0 时熵不参与");
        assert_eq!(a.fingerprint, b.fingerprint);
        assert!(!a.stars.is_empty(), "密度 0.5 应有星星");
    }

    /// random=1 时熵参与：不同 entropy 生成不同列表（「换一片星空」）。
    #[test]
    fn star_field_random_mixes_entropy() {
        let a = StarField::generate(0.5, 1.0, 0.5, 1);
        let b = StarField::generate(0.5, 1.0, 0.5, 2);
        assert_ne!(a.stars, b.stars, "random=1 时不同熵应生成不同星空");
    }

    /// 密度 → 数量：0 密度空表、满密度达到上限、中间单调。
    #[test]
    fn star_field_count_follows_density() {
        assert!(StarField::generate(0.0, 0.0, 0.0, 0).stars.is_empty());
        let mid = StarField::generate(0.5, 0.0, 0.0, 0).stars.len();
        let full = StarField::generate(1.0, 0.0, 0.0, 0).stars.len();
        assert_eq!(full, StarField::COUNT_MAX);
        assert!(mid > 0 && mid < full, "中间密度应介于两者之间");
    }

    /// 大小不规则：size_var=0 时半径全相等（整齐），1 时有方差。
    #[test]
    fn star_size_variance_follows_param() {
        let uniform = StarField::generate(1.0, 0.0, 0.0, 0);
        assert!(
            uniform.stars.windows(2).all(|w| (w[0].radius - w[1].radius).abs() < 1e-6),
            "size_var=0 时半径应全部相等"
        );
        let varied = StarField::generate(1.0, 0.0, 1.0, 0);
        let radii: Vec<f32> = varied.stars.iter().map(|s| s.radius).collect();
        let mean = radii.iter().sum::<f32>() / radii.len() as f32;
        let var = radii.iter().map(|r| (r - mean).powi(2)).sum::<f32>() / radii.len() as f32;
        assert!(var > 0.1, "size_var=1 时半径应有可观方差，实测 {var}");
    }

    /// 亮度分级：约 1/4 是亮星（真实星空的指数分布近似）。
    #[test]
    fn star_brightness_is_graded() {
        let f = StarField::generate(1.0, 0.0, 0.0, 0);
        let bright = f.stars.iter().filter(|s| s.base > 0.8).count();
        let ratio = bright as f32 / f.stars.len() as f32;
        assert!(
            (0.1..0.45).contains(&ratio),
            "亮星占比应接近 25%，实测 {ratio}"
        );
    }

    // ---- 光标视差（cursor_offset）--------------------------------------

    /// 视差纯函数：关 = 零偏移、无光标 = 零偏移、方向 = 让开光标、
    /// 近大远小、不越过光标。
    #[test]
    fn cursor_offset_parallax_laws() {
        let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(720.0, 700.0));
        let star = Star {
            x: 0.75,
            y: 0.5,
            radius: 1.2,
            phase: 0.0,
            freq: 0.1,
            base: 0.8,
        };
        // 关（strength=0）⇒ 零偏移。
        assert_eq!(
            cursor_offset(&star, Some(egui::pos2(100.0, 350.0)), area, 0.0),
            egui::Vec2::ZERO
        );
        // 光标不在窗口上 ⇒ 零偏移（星点回原位）。
        assert_eq!(cursor_offset(&star, None, area, 1.0), egui::Vec2::ZERO);
        // 星点在光标右侧（540 vs 100）⇒ 偏移 x > 0（被推离光标）。
        let off = cursor_offset(&star, Some(egui::pos2(100.0, 350.0)), area, 1.0);
        assert!(off.x > 0.0, "星点应被推离光标（x 正向），实测 {off:?}");
        // 同距离下满强度比半强度偏移大。
        let half = cursor_offset(&star, Some(egui::pos2(100.0, 350.0)), area, 0.5);
        assert!(off.length() > half.length(), "强度应线性缩放偏移");
        // 近大远小（视差纵深）：同一颗星，光标 400pt 外的偏移应小于
        // 100pt 外的。
        let near = cursor_offset(&star, Some(egui::pos2(440.0, 350.0)), area, 1.0);
        assert!(
            near.length() > off.length(),
            "近处偏移应更大：near={near:?} far={off:?}"
        );
        // 不越过光标：偏移量 < 星点到光线的距离。
        let c = egui::pos2(540.0, 350.0);
        let p = egui::pos2(100.0, 350.0);
        assert!(off.length() < (c - p).length());
        // 光标几乎压在星点上 ⇒ 零偏移（不抖）。
        assert_eq!(
            cursor_offset(&star, Some(egui::pos2(540.0, 350.0)), area, 1.0),
            egui::Vec2::ZERO
        );
    }

    /// 闪烁纯函数：twinkle=0 恒定、twinkle=1 在 [0, base] 间呼吸、
    /// 相同时刻同值（可断言）。
    #[test]
    fn twinkle_alpha_is_pure_and_bounded() {
        let star = StarField::generate(1.0, 0.0, 0.0, 0).stars[0];
        // 静态。
        assert_eq!(twinkle_alpha(&star, 0.0, 0.0), star.base);
        assert_eq!(twinkle_alpha(&star, 0.0, 12.34), star.base);
        // 呼吸：一个周期内应同时出现接近 base 与接近 0 的值。
        let period = 1.0 / star.freq as f64;
        let mut lo = f32::MAX;
        let mut hi = 0.0_f32;
        for i in 0..32 {
            let t = period * i as f64 / 32.0;
            let a = twinkle_alpha(&star, 1.0, t);
            assert!((0.0..=star.base + 1e-6).contains(&a), "闪烁值越界: {a}");
            lo = lo.min(a);
            hi = hi.max(a);
        }
        assert!(hi > star.base * 0.9, "应能闪到接近全亮");
        assert!(lo < star.base * 0.15, "应能暗到接近熄灭");
        // 同输入同输出（纯函数）。
        assert_eq!(
            twinkle_alpha(&star, 0.6, 3.21),
            twinkle_alpha(&star, 0.6, 3.21)
        );
    }
}
