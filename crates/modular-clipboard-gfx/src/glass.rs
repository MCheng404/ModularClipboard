//! 液态玻璃（Liquid Glass）的**数学内核**。
//!
//! # 为什么先有这一层
//!
//! 玻璃效果是一条链：`SDF 形状 → 法线 → 折射位移 → 色散 → Fresnel 边缘光`。
//! 这条链全在 GPU 片元着色器里跑，而着色器**没法单测**——一个符号写反只会
//! 表现为「看起来有点怪」，跟真实故障（描述符绑错、布局错）长得一样。
//!
//! 所以把整条链的数学抽成纯函数放在这里，`blur.wgsl` 的 `fs_glass` 逐条
//! 镜像它。单测在这里守住：
//! - SDF 的形状语义（内负外正、边缘≈0、圆角处梯度连续）；
//! - 法线方向（必须朝外，否则折射会把内容「吸」进去）；
//! - 折射位移的**幅度上限**（位移超过一个纹素就是「内容被撕走」，观感是
//!   卡片里的背景和卡片外的对不上）。
//!
//! # 参数来源
//!
//! - 折射：`refract(incident, normal, eta)`，`eta = 1/1.5`（玻璃 IOR 1.5）。
//! - 色散：R/G/B 各用一个 IOR（1.50 / 1.52 / 1.54），三次采样重合成。
//! - Fresnel：`pow(1 - |dot(eye, n)|, power)`，这里 `eye = (0,0,1)`，
//!   于 2D 情形退化成「离边缘越近越亮」。
//!
//! 参考：Maxime Heckel《Refraction, dispersion, and other shader light
//! effects》与 bergice/liquidglass 的单文件实现（两级折射：
//! 中心径向穹顶 + 边缘轮廓，按距边距离混合）。

/// 圆角矩形的有符号距离场（SDF）。
///
/// `p` 是相对**中心**的坐标，`half` 是半尺寸，`r` 是圆角半径。
/// 约定与 GLSL 的 `roundedBoxSDF` 一致：内部为负、外部为正、边缘 ≈ 0。
///
/// # 为什么不用 `Rect::contains`
///
/// 折射与边缘光都需要**连续的梯度**（法线），而布尔包含判断在边缘不连续，
/// 求不出法线。SDF 的梯度天然连续（圆角处也连续，这是选圆角矩形而不是
/// 普通矩形的另一个理由）。
pub fn glass_sdf(p: [f32; 2], half: [f32; 2], radius: f32) -> f32 {
    let r = radius.min(half[0]).min(half[1]).max(0.0);
    let qx = (p[0].abs() - half[0] + r).max(0.0);
    let qy = (p[1].abs() - half[1] + r).max(0.0);
    (qx * qx + qy * qy).sqrt() - r
}

/// SDF 的梯度（= 外法线方向，未归一化时的模即距离梯度）。
///
/// 用**中心差分**而不是解析梯度：解析式在圆角与直边的接缝处要分支，
/// 而中心差分处处同一套代码，且与着色器里的写法逐字对应（着色器没有
/// 解析梯度可用）。
pub fn glass_normal(p: [f32; 2], half: [f32; 2], radius: f32, eps: f32) -> [f32; 2] {
    let e = eps.max(1e-5);
    let dx = glass_sdf([p[0] + e, p[1]], half, radius)
        - glass_sdf([p[0] - e, p[1]], half, radius);
    let dy = glass_sdf([p[0], p[1] + e], half, radius)
        - glass_sdf([p[0], p[1] - e], half, radius);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-6 {
        // 中心处梯度接近 0（平坦区），归一化会放大噪声。返回零向量，
        // 调用方按「无折射」处理。
        [0.0, 0.0]
    } else {
        [dx / len, dy / len]
    }
}

/// SDF 的**解析**梯度（= 外法线）。
///
/// # 为什么在 [`glass_normal`] 之外再要一个
///
/// `glass_normal` 用中心差分：4 次 SDF 调用，每次含一个 `sqrt`。而圆角
/// 矩形 SDF 的内部量 `q = max(|p| - half + r, 0)` **本身就是未归一化的
/// 梯度**——`q / |q|` 即外法线（`q = 0` 的平坦区梯度为 0，与中心差分的
/// 零向量一致）。所以解析版一次 SDF 就够，省下 3 次 `sqrt` 与一堆
/// `max`/`abs`。
///
/// 玻璃 pass 是**每像素**跑这条链的（2026-10-24 用户实测滚动列表卡顿，
/// 剖析后确认这是单像素成本里最大的一项），所以这个等价替换值得做。
/// `glass_normal`（中心差分）保留：测试用它做黄金模型，验证解析版没错。
///
/// # 两者何时会不一致
///
/// 中心差分在**圆角与直边的接缝**附近会有半步的采样偏差（差分点跨过了
/// 接缝），解析版没有。差异量级是 `eps`（1 像素）的一小部分，而折射位移
/// 的最小可辨单位远大于此——观感无差。
///
/// # 唯一的坑：象限符号
///
/// `q` 用 `|p|` 算 ⇒ 负半轴要乘回符号，见实现里的 CAUTION。
pub fn glass_normal_analytic(p: [f32; 2], half: [f32; 2], radius: f32) -> [f32; 2] {
    let r = radius.min(half[0]).min(half[1]).max(0.0);
    let qx = (p[0].abs() - half[0] + r).max(0.0);
    let qy = (p[1].abs() - half[1] + r).max(0.0);
    let len = (qx * qx + qy * qy).sqrt();
    if len < 1e-6 {
        return [0.0, 0.0];
    }
    // CAUTION: `q` 是用 **|p|** 算的，上下/左右半平面被折到了一起。
    // 梯度必须按 p 所在的象限还原符号，否则负半轴的法线整体反向——
    // 折射会把内容朝反方向推（观感是「玻璃在吸气而不是推气」），
    // 而这类偏差**不报任何错**。
    //
    // 这条正是 `analytic_normal_matches_central_difference` 抓到的：
    // (-90,-25) 处中心差分给 (0,-1)，未修正的解析版给 (0,+1)。
    let sx = if p[0] < 0.0 { -1.0 } else { 1.0 };
    let sy = if p[1] < 0.0 { -1.0 } else { 1.0 };
    [sx * qx / len, sy * qy / len]
}

/// 2D 斜率场抬起成 3D 表面法线时，xy 分量的放大倍数。
///
/// 玻璃是一片**弯曲的薄片**：2D 的 SDF 法线/穹顶斜率给出薄片在 xy 上
/// 的倾斜方向，抬起成 3D 法线 `N = normalize(vec3(n * TILT, 1))` 才有
/// 可折射的表面。TILT 越大薄片越「鼓」（同样距边处越接近掠射）。
///
/// 2.0：单位长 2D 法线抬起后 `|N.xy| ≈ 0.894`、`N.z ≈ 0.447`，即边缘
/// 处薄片倾约 63°——介于「平板」与「半球」之间，是液态玻璃边缘那种
/// 明显但不成哈哈镜的透镜感。
pub const SURFACE_TILT: f32 = 2.0;

/// 单位长 2D 法线抬起后的 `|N.xy|`（`TILT / sqrt(TILT² + 1)`）。
pub const EDGE_NXY: f32 = 0.894_427_2;

/// 参考 eta（[`DEFAULT_ETA`]）在**边缘**（`|n| = 1`）处的折射因子
/// `sqrt(1 - eta²(1 - N.z²)) - eta·N.z`。
///
/// 用它把 [`refract_offset`] 归一化：于是 `power` 的语义仍是「边缘处的
/// 最大位移」（与文档契约一致），而 eta 的差别**只**体现在因子上——
/// 这正是色散可见度所在。改 `SURFACE_TILT` 必须同步改这三个常量，
/// 见 `tilt_constants_are_self_consistent` 与
/// `shader_constants_match_rust`（后者扫 `glass.wgsl` 的同一批字面量）。
pub const EDGE_FACTOR: f32 = 0.504_630_6;

/// 折射造成的 UV 位移（单位：UV，即 [0,1] 纹理空间）。
///
/// `normal` 是薄片表面的 2D 斜率（SDF 法线为单位长，穹顶斜率 ≤ 1），
/// `eta = 1 / ior`。
///
/// # 为什么入射不能取 `-normal`
///
/// 早期版本把 `incident = -normal`（2D 向量）喂给 `refract()`。那是个
/// **退化模型**：`dot(N, I) = -|N|²`，对单位法线恒为 -1，于是
/// `k = 1 - eta²(1 - 1) = 1`、`s = 1`，整个式子塌成
///
/// ```text
/// (eta·(-n) - (eta·(-1) + 1)·n)·power = -n·power
/// ```
///
/// ——**eta 被完全约掉**。后果有三个，且都不报错：
///
/// 1. `IOR_R/G/B` 三个折射率给出**完全相同**的位移 ⇒ 色散（边缘彩边）
///    恒为 0，`glass_dispersion` 滑块是死的；
/// 2. 边缘那 1~3 次分通道采样白付（两个通道采样坐标逐位相同）；
/// 3. `eta = 1`（ior = 1，即「没有玻璃」）时位移仍是 `-n·power` 而非 0
///    ——物理上就不该有折射。
///
/// 正确模型：入射是**正对玻璃的视线** `I = (0, 0, -1)`，法线是抬起后的
/// 3D 法线 `N = normalize(vec3(n·TILT, 1))`。此时
///
/// ```text
/// T.xy = -(s - eta·N.z) · N.xy,   s = sqrt(1 - eta²(1 - N.z²))
/// ```
///
/// eta 安安稳稳地留在式子里：ior 越大 ⇒ eta 越小 ⇒ `(s - eta·N.z)` 越大
/// ⇒ 蓝光弯得比红光多，色散才成立。
///
/// # `power` 的语义（归一化）
///
/// 上式再乘 `EDGE_NXY / EDGE_FACTOR` 的倒数归一：于是 **`|n| = 1` 且
/// `eta = DEFAULT_ETA` 时位移模长恰好等于 `power`**，与旧模型的幅值逐点
/// 对齐（实测 `|n| = 0.5` 处旧 0.632·power、新 0.643·power，差 1.8%）。
/// 换句话说这次修的是「eta 有没有用」，不是「折射有多强」——观感不变，
/// 色散才开始生效。
///
/// # `power` 必须与 [`edge_falloff`] 相乘后使用
///
/// 本函数返回的是**边缘处的最大位移**（那里曲面最陡、法线最长）。
/// 玻璃的设计是「中心清晰、边缘折射」，所以调用方必须乘上
/// `edge_falloff(d, ..)`：边缘处 `power * falloff` 可达 0.1~0.35 UV
/// （这是设计意图——没有这么大的位移就看不到折射），中心处
/// `falloff → 0`，位移归零。
///
/// **不要**在别处再乘一次衰减，也不要拿这个值当「全局位移上限」
/// ——它不是。
pub fn refract_offset(normal: [f32; 2], eta: f32, power: f32) -> [f32; 2] {
    // 抬起成 3D 法线。|normal| 已知量级（0~1），无需先归一化：
    // `t = |n|·TILT`，归一化系数 `inv = 1/sqrt(t² + 1)` 一次算出
    // `N.xy` 与 `N.z` 两者。
    let t = (normal[0] * normal[0] + normal[1] * normal[1]).sqrt() * SURFACE_TILT;
    let inv = 1.0 / (t * t + 1.0).sqrt();
    let nx = normal[0] * SURFACE_TILT * inv;
    let ny = normal[1] * SURFACE_TILT * inv;
    let nz = inv;
    // refract(I, N, eta) 的标准实现（WGSL 内建同一套），I = (0,0,-1)：
    //   d = dot(N, I) = -N.z
    //   k = 1 - eta^2 * (1 - d^2)
    //   T = eta*I - (eta*d + sqrt(k)) * N
    // 取 xy 分量（I.xy = 0）：
    //   T.xy = -(eta*d + s) * N.xy = -(s - eta*N.z) * N.xy
    let d = -nz;
    let k = (1.0 - eta * eta * (1.0 - d * d)).max(0.0);
    let s = k.sqrt();
    let f = s - eta * nz;
    // 归一到「边缘 + 参考 eta ⇒ 模长 = power」。
    let scale = (f / EDGE_FACTOR) / EDGE_NXY;
    [-nx * scale * power, -ny * scale * power]
}

/// 边缘轮廓折射的衰减权重。
///
/// 距边缘越近权重越大（`dist → 0` 时 → 1，深入内部 → 0）。这是 bergice
/// 实现里 `exp(-|dist| * 0.4)` 的那一项——**边缘处折射最强**，因为那里
/// 曲面最陡。
///
/// # CAUTION: 入参是**距边距离**，不是 SDF 值
///
/// 圆角矩形的 SDF 在内部恒等于 `-radius`（平坦区，见 [`glass_sdf`] 的
/// 公式），拿它当距离会让**整片内部的折射强度一模一样**——中心本该清晰
/// 的地方也糊着一层折射。必须先用 [`dist_to_edge`] 换成真实距离。
pub fn edge_falloff(dist_to_edge: f32, sharpness: f32) -> f32 {
    (-dist_to_edge.max(0.0) * sharpness).exp()
}

/// 点到卡片边缘的**真实距离**（像素）。
///
/// 为什么不能直接用 [`glass_sdf`]：圆角矩形 SDF 在内部是平坦的
/// （恒为 `-radius`，梯度为 0），它回答的是「在形状内还是外」，不是
/// 「离边多远」。而折射衰减要的是后者。
///
/// - 外部：SDF 本身就是距离（正值）；
/// - 内部：取到两条直边的较小者（圆角区用它略有偏差，但圆角区面积小，
///   且那里本来就该是最强的折射，偏差方向安全）。
pub fn dist_to_edge(p: [f32; 2], half: [f32; 2], sdf: f32) -> f32 {
    if sdf > 0.0 {
        sdf
    } else {
        (half[0] - p[0].abs()).min(half[1] - p[1].abs()).max(0.0)
    }
}

/// 径向穹顶折射的 UV 位移（bergice 的第一级折射）。
///
/// # 为什么需要它
///
/// [`glass_normal`] 在卡片内部平坦区返回零向量（SDF 梯度为 0），只靠
/// 边缘轮廓折射的话，**中心一大片完全没有折射**——玻璃看起来像「只有
/// 一圈会动的贴图」。bergice 的做法是叠一级「穹顶」：把卡片当成半球透镜，
/// 法线方向取 `normalize(局部坐标) * 曲率`，于是中心也有（弱的）折射，
/// 越往边缘越强。
///
/// `local` 是归一化到 `[-1, 1]` 的局部坐标（相对中心、按半尺寸归一），
/// `power` 是强度。返回的是 UV 位移，同样要调用方按需要缩放。
pub fn dome_refract_offset(local: [f32; 2], eta: f32, power: f32) -> [f32; 2] {
    let len = (local[0] * local[0] + local[1] * local[1]).sqrt();
    if len < 1e-6 {
        return [0.0, 0.0]; // 正中心：穹顶顶点，法线垂直入射，无位移
    }
    // 曲率：线性（bergice 的默认 `pow(r, 1.0)`）。换成
    // `sin(r * π/2)` 会更「鼓」，`r * (2 - r)` 的最大斜率在 0.5 处。
    let curvature = len.min(1.0);
    let dome = [local[0] / len * curvature, local[1] / len * curvature];
    refract_offset(dome, eta, power)
}

/// Fresnel 边缘光强度（0 = 全透射，1 = 全反射）。
///
/// # `normal` 是**斜率场**，不是 SDF 梯度
///
/// 这是 2026-10-24 修正过的语义。此前直接吃 [`glass_normal`]（SDF 梯度），
/// 公式写成 `1 - |n|`，**物理映射是反的**，后果是整片平坦内部全亮：
///
/// - 圆角矩形 SDF 的梯度在内部恒为 0、在贴边处为 1；
/// - 于是 `1 - |n|` 在内部 = 1（最亮）、在边缘 = 0（不亮）——恰好颠倒。
/// - 而「直边中央不透亮」那条旧测试把这个错误钉住了：它拿 SDF 梯度
///   （贴边 = 1）算，得到 0，就断言「边缘不该亮」。
///
/// 正确的模型：玻璃是一片**弯曲的薄片**。表面法线的 xy 分量就是薄片的
/// **斜率**——中心平（对着观众，法线 (0,0,1)）⇒ 无反射；越靠边缘越倾斜
/// ⇒ 越接近掠射 ⇒ Fresnel 越强。所以：
///
/// ```text
///   fresnel = pow(clamp(|slope|, 0, 1), power)
/// ```
///
/// 单调递增，中心 0、边缘 1，且**处处平滑**（SDF 梯度在「平坦区 / 贴边」
/// 之间有跳变，用它会出现一条硬边亮带）。
///
/// 现成的平滑斜率场是 [`dome_refract_offset`] 用的那个穹顶法线：
/// `normalize(local) * min(|local|, 1)`。调用方传它。
pub fn fresnel_edge(slope: [f32; 2], power: f32) -> f32 {
    let m = (slope[0] * slope[0] + slope[1] * slope[1]).sqrt().min(1.0);
    m.powf(power.max(0.0))
}

/// 穹顶斜率场（[`fresnel_edge`] 的配套输入）。
///
/// 与 [`dome_refract_offset`] 用同一个场，保证「折射强的地方恰好也亮」——
/// 两者分开算会出现「折射很明显但边缘不亮」的割裂感。
///
/// `local` 是归一化到 `[-1, 1]` 的局部坐标（相对中心、按半尺寸归一）。
pub fn dome_slope(local: [f32; 2]) -> [f32; 2] {
    let len = (local[0] * local[0] + local[1] * local[1]).sqrt();
    if len < 1e-6 {
        return [0.0, 0.0]; // 穹顶顶点：薄片水平，无斜率
    }
    let curvature = len.min(1.0);
    [local[0] / len * curvature, local[1] / len * curvature]
}

/// 色散的三个 IOR（R 最小、B 最大 ⇒ 蓝光弯得更多，边缘出现彩色描边）。
///
/// CAUTION: 它们只有在 [`refract_offset`]**真的让 eta 参与运算**时才有
/// 意义。早前那版把入射取成 `-normal`，eta 被约掉，三个 IOR 算出完全
/// 相同的位移——滑块能拖、画面没反应。`refraction_varies_with_eta`
/// 与 `dispersion_offsets_actually_differ` 盯住这条。
pub const IOR_R: f32 = 1.50;
pub const IOR_G: f32 = 1.52;
pub const IOR_B: f32 = 1.54;

/// 默认 IOR 比值（`eta = 1 / ior`）。
pub const DEFAULT_ETA: f32 = 1.0 / 1.5;

#[cfg(test)]
mod tests {
    use super::*;

    const HALF: [f32; 2] = [100.0, 30.0];

    #[test]
    fn sdf_sign_matches_position() {
        // 中心必须在内部（负），远处必须在外部（正）。
        assert!(glass_sdf([0.0, 0.0], HALF, 8.0) < 0.0, "中心应在卡片内");
        assert!(
            glass_sdf([200.0, 0.0], HALF, 8.0) > 0.0,
            "右侧 200 应在卡片外"
        );
        assert!(glass_sdf([0.0, 200.0], HALF, 8.0) > 0.0, "下方应在卡片外");
        // 圆角内切点：到边缘的距离应恰好等于圆角半径。
        let d = glass_sdf([HALF[0], 0.0], HALF, 8.0);
        assert!(d.abs() < 1e-6, "直边中点应在边缘上，实际 {d}");
    }

    #[test]
    fn sdf_gradient_is_continuous_across_corner() {
        // 圆角的意义：**梯度**连续。沿一条穿过圆角的内部直线扫，SDF 的
        // 一阶差分（梯度）不应有跳变——矩形直角处会有折点，圆角处没有。
        //
        // CAUTION: 必须扫**内部**（x = HALF[0] - 4 在卡片内）。从卡片外
        // 起扫会穿过边缘，SDF 由正转负是必然的，那测不出连续性。
        // CAUTION: 扫描线必须**远离圆角**（距边缘 > 圆角半径）。贴着
        // 圆角扫会切过圆弧，那里梯度本来就平滑地 1.0 -> 0 -> 1.0，
        // 变化可达 0.5——那不是折点。要验证「直边无折点」就得在直边上扫。
        for x in [HALF[0] - 20.0, HALF[0] - 16.0] {
            // 扫描范围也要避开圆角：y ∈ [-(half.y - r - 2), +(half.y - r - 2)]，
            // 即只覆盖两条**直边**之间的区域。起点贴着下边缘会切进圆角，
            // 那里的梯度平滑变化，会把断言打红。
            let steps = 40;
            let span = HALF[1] - 10.0 - 2.0;
            let mut prev_d = glass_sdf([x, -span], HALF, 10.0);
            let mut prev_grad = f32::MAX;
            for i in 1..=steps {
                let y = -span + (2.0 * span) * (i as f32) / steps as f32;
                let d = glass_sdf([x, y], HALF, 10.0);
                let grad = (d - prev_d).abs();
                if prev_grad != f32::MAX {
                    assert!(
                        (grad - prev_grad).abs() < 0.05,
                        "x={x} 处梯度跳变 {prev_grad} -> {grad}（y={y}）"
                    );
                }
                prev_grad = grad;
                prev_d = d;
            }
        }
    }

    #[test]
    fn normal_points_outward_everywhere() {
        // 法线必须朝外：在卡片内取样，法线与「中心 → 取样点」同向。
        //
        // CAUTION: **正中心允许零向量**。那里 SDF 是平坦的（梯度 ≈ 0），
        // 而「中心清晰」正是玻璃要的——没有法线就没有折射，中心区域原样
        // 透出。所以零向量是特性不是缺陷，测试不能要求它非零。
        // 只取**靠近边缘**的点：平坦区（如 (50, 0)）的法线数学上就是零，
        // 见下面单独断言。
        // 必须取**圆角区**的点（判据：|p| - half + r > 0）。平坦区
        //（如 (0,20)、(50,0)）的法线数学上恒为零，见下面的单独断言。
        for &(x, y) in &[(90.0, 25.0), (99.0, 29.0), (95.0, 5.0), (99.0, 0.0), (0.0, 29.5)] {
            let n = glass_normal([x, y], HALF, 8.0, 1.0);
            assert!(n != [0.0, 0.0], "({x},{y}) 处法线退化为零向量");
            let dot = n[0] * x + n[1] * y;
            assert!(dot > 0.0, "({x},{y}) 法线朝内：n={n:?}");
        }
        assert_eq!(
            glass_normal([0.0, 0.0], HALF, 8.0, 1.0),
            [0.0, 0.0],
            "中心处应无折射（平坦区）"
        );
    }

    #[test]
    fn analytic_normal_matches_central_difference() {
        // 解析梯度必须与中心差分（黄金模型）一致——否则 shader 里的
        // 等价替换会悄悄改变观感，而那种偏差正是「看起来有点怪」、
        // 与真实故障无法区分的那一类。
        let samples: [(f32, f32); 12] = [
            (90.0, 25.0),
            (99.0, 29.0),
            (95.0, 5.0),
            (99.0, 0.0),
            (0.0, 29.5),
            (99.5, 29.5),
            (50.0, 29.0),
            (50.0, 0.0),   // 平坦区
            (0.0, 0.0),    // 正中心
            (-90.0, -25.0),
            (99.0, -29.0),
            (70.0, 12.0),
        ];
        for &(x, y) in &samples {
            let cd = glass_normal([x, y], HALF, 8.0, 1.0);
            let an = glass_normal_analytic([x, y], HALF, 8.0);
            let diff = ((cd[0] - an[0]).powi(2) + (cd[1] - an[1]).powi(2)).sqrt();
            assert!(
                diff < 0.02,
                "({x},{y}) 解析法线与中心差分不一致：cd={cd:?} an={an:?} 差 {diff}"
            );
        }
        // 平坦区：两者都必须是零向量（中心清晰的前提）。
        assert_eq!(glass_normal_analytic([50.0, 0.0], HALF, 8.0), [0.0, 0.0]);
    }

    #[test]
    fn refraction_is_centered_clear_and_edge_strong() {
        // 玻璃的位移分布必须是「中心 ≈ 0、边缘最大」。
        //
        // CAUTION: 不要给边缘位移设一个小上限。边缘处 0.1~0.35 UV 的位移
        // 是**设计意图**——没有它就看不到折射；会被「撕走」的不是位移大，
        // 而是**中心**也有大位移（那里本该清晰）。要守的是这个分布。
        let mag_at = |x: f32, y: f32| {
            let n = glass_normal([x, y], HALF, 8.0, 1.0);
            let off = refract_offset(n, DEFAULT_ETA, 0.35);
            (off[0] * off[0] + off[1] * off[1]).sqrt()
        };
        let center = mag_at(0.0, 0.0);
        let mid = mag_at(50.0, 0.0);
        let edge = mag_at(99.0, 29.0);
        assert!(center < 1e-6, "中心位移应为 0，实际 {center}");
        assert!(mid < edge, "位移应随靠近边缘增大：{mid} < {edge}");
        // 合成 falloff 之后，中心必须 ≈ 0。
        //
        // CAUTION: 这里必须用 `dist_to_edge` 而不是 SDF——SDF 在内部恒为
        // `-radius`，用它算 falloff 会让中心的折射强度和边缘一模一样。
        let d_center = glass_sdf([0.0, 0.0], HALF, 8.0);
        let de = dist_to_edge([0.0, 0.0], HALF, d_center);
        let eff = edge * edge_falloff(de, 0.4);
        // 1e-5 而不是 1e-6：这里残留的是 f32 指数/乘法的噪声量级
        // （2e-6），不是真实折射。卡 1e-6 会在不同优化等级间抖动。
        assert!(eff < 1e-5, "中心合成位移应为 0，实际 {eff}（距边 {de}）");
        // 而边缘处应当实打实地有折射。
        let d_edge = glass_sdf([99.0, 29.0], HALF, 8.0);
        let de_edge = dist_to_edge([99.0, 29.0], HALF, d_edge);
        assert!(de_edge < de, "边缘距边应更近：{de_edge} < {de}");
    }

    #[test]
    fn dome_refraction_fills_the_flat_center() {
        // 穹顶折射的意义：**平坦区也有折射**（边缘轮廓折射在那里是零）。
        let local = |x: f32, y: f32| [x / HALF[0], y / HALF[1]];
        let mag = |l: [f32; 2]| {
            let o = dome_refract_offset(l, DEFAULT_ETA, 0.03);
            (o[0] * o[0] + o[1] * o[1]).sqrt()
        };
        let center = mag(local(0.0, 0.0));
        let mid = mag(local(50.0, 0.0));
        let edge = mag(local(99.0, 29.0));
        assert!(center < 1e-6, "穹顶顶点应无位移，实际 {center}");
        assert!(
            mid > 1e-4,
            "中部应有折射（这是穹顶的存在意义），实际 {mid}"
        );
        assert!(edge > mid, "越靠边缘折射越强：{edge} > {mid}");
    }

    #[test]
    fn dist_to_edge_is_real_distance_not_sdf() {
        // 同一处的 SDF 与真实距边：内部 SDF 恒为 -radius，距边则是 30px。
        let sdf = glass_sdf([0.0, 0.0], HALF, 8.0);
        assert!((sdf + 8.0).abs() < 1e-6, "内部 SDF 应恒为 -radius，实际 {sdf}");
        let de = dist_to_edge([0.0, 0.0], HALF, sdf);
        assert!(
            (de - 30.0).abs() < 1e-6,
            "中心距边应为半高 30，实际 {de}"
        );
        // 外部：SDF 即距离。
        let sdf_out = glass_sdf([120.0, 0.0], HALF, 8.0);
        let de_out = dist_to_edge([120.0, 0.0], HALF, sdf_out);
        assert!(
            (de_out - sdf_out).abs() < 1e-6,
            "外部应直接用 SDF，实际 {de_out} vs {sdf_out}"
        );
    }

    #[test]
    fn edge_falloff_peaks_at_edge() {
        assert!(edge_falloff(0.0, 0.4) > 0.99, "边缘处权重应接近 1");
        assert!(
            edge_falloff(50.0, 0.4) < 1e-6,
            "深入内部 50px 权重应接近 0"
        );
        // 单调递减
        let a = edge_falloff(1.0, 0.4);
        let b = edge_falloff(2.0, 0.4);
        assert!(a > b, "衰减应单调：{a} > {b}");
    }

    #[test]
    fn fresnel_brightens_only_near_edge() {
        // 新语义（2026-10-24 修正）：入参是**斜率场**，中心 0、边缘 1。
        //
        // CAUTION: 这里必须用 `dome_slope` 而不是 `glass_normal`。
        // 旧版吃 SDF 梯度，公式还是 `1 - |n|` ⇒ 物理映射整个反了：
        // SDF 梯度在内部恒 0、贴边为 1，于是 `1-|n|` 让**整片内部最亮**、
        // 边缘不亮。旧测试「直边中央应接近 0」正好把这个错误钉死了。
        let local = |x: f32, y: f32| [x / HALF[0], y / HALF[1]];
        let center = fresnel_edge(dome_slope(local(0.0, 0.0)), 5.0);
        let mid = fresnel_edge(dome_slope(local(50.0, 0.0)), 5.0);
        let edge = fresnel_edge(dome_slope(local(99.0, 29.0)), 5.0);
        assert!(center < 1e-6, "中心（薄片水平）应无 Fresnel，实际 {center}");
        assert!(mid > center, "越靠边缘越亮：{mid} > {center}");
        assert!(edge > mid, "边缘最亮：{edge} > {mid}");
        assert!(edge > 0.9, "边缘应接近全亮，实际 {edge}");
        // 单调性：斜率场模长单调增 ⇒ Fresnel 单调增。
        let mut prev = 0.0;
        for i in 0..=10 {
            let t = i as f32 / 10.0;
            let f = fresnel_edge(dome_slope(local(99.0 * t, 29.0 * t)), 5.0);
            assert!(f >= prev - 1e-6, "Fresnel 应单调不减：{prev} -> {f}");
            prev = f;
        }
    }

    #[test]
    fn tilt_constants_are_self_consistent() {
        // `EDGE_NZ / EDGE_NXY / EDGE_FACTOR` 都是从 `SURFACE_TILT` 推出来的
        // 常量（f32 常量不能调 `sqrt`，只能写字面量）。改 TILT 忘改它们，
        // 折射幅值会整体偏掉而不报错——这条把它钉住。
        let denom = (SURFACE_TILT * SURFACE_TILT + 1.0).sqrt();
        assert!(
            (EDGE_NXY - SURFACE_TILT / denom).abs() < 1e-6,
            "EDGE_NXY={EDGE_NXY} 应为 TILT/sqrt(TILT²+1)={}",
            SURFACE_TILT / denom
        );
        // 抬起后的法线必须是单位向量：`N.z² + |N.xy|² == 1`。
        let nz = 1.0 / denom;
        assert!(
            (nz * nz + EDGE_NXY * EDGE_NXY - 1.0).abs() < 1e-6,
            "抬起后的法线必须是单位向量"
        );
        let eta = DEFAULT_ETA;
        let f0 = (1.0 - eta * eta * (1.0 - nz * nz)).sqrt() - eta * nz;
        assert!(
            (EDGE_FACTOR - f0).abs() < 1e-6,
            "EDGE_FACTOR={EDGE_FACTOR} 应为 {f0}"
        );
    }

    #[test]
    fn refraction_magnitude_matches_documented_power_at_edge() {
        // 归一化的契约：`|n| = 1` 且 eta = DEFAULT_ETA 时位移模长 == power。
        // 这条是「修物理模型不能顺手改掉折射强度」的守门——观感参数
        // （默认 0.35）的含义必须原样保留。
        let n = glass_normal([99.0, 29.0], HALF, 8.0, 1.0);
        let len = (n[0] * n[0] + n[1] * n[1]).sqrt();
        assert!((len - 1.0).abs() < 1e-3, "取样点应给出单位法线，实际 |n|={len}");
        for power in [0.1_f32, 0.35, 1.0] {
            let o = refract_offset(n, DEFAULT_ETA, power);
            let mag = (o[0] * o[0] + o[1] * o[1]).sqrt();
            assert!(
                (mag - power).abs() < 1e-3,
                "power={power} 时边缘位移应为 {power}，实际 {mag}"
            );
        }
        // 方向仍然是 -n（朝卡片内推，与旧模型同向）。
        let o = refract_offset(n, DEFAULT_ETA, 0.35);
        assert!(o[0] * n[0] + o[1] * n[1] < 0.0, "位移应朝法线反方向：{o:?}");
    }

    #[test]
    fn refraction_varies_with_eta() {
        // 退化模型的判据（eta 被约掉 ⇒ 位移与 eta 无关）。这条是保证
        // `IOR_*` 还有意义的**根本**守门：它红 = 色散一定失效。
        let n = glass_normal([99.0, 29.0], HALF, 8.0, 1.0);
        let mag = |eta: f32| {
            let o = refract_offset(n, eta, 0.35);
            (o[0] * o[0] + o[1] * o[1]).sqrt()
        };
        let m1 = mag(1.0 / 1.2);
        let m2 = mag(1.0 / 1.9);
        assert!(
            (m1 - m2).abs() > 1e-3,
            "eta 必须影响位移大小，否则色散恒为 0：ior1.2 => {m1}, ior1.9 => {m2}"
        );
        // ior 越大 ⇒ eta 越小 ⇒ 弯得越多。
        assert!(m2 > m1, "大 ior 应弯得更多：{m2} > {m1}");
    }

    #[test]
    fn refraction_vanishes_when_ior_is_one() {
        // ior = 1（eta = 1）意味着「介质与空气相同」⇒ 不该有任何折射。
        // 旧模型（入射 = -normal）在这里给 `-n·power`，是个恒不为零的
        // 假位移——这条正是它过不了的测试。
        let n = glass_normal([99.0, 29.0], HALF, 8.0, 1.0);
        let o = refract_offset(n, 1.0, 0.35);
        assert!(
            (o[0] * o[0] + o[1] * o[1]).sqrt() < 1e-6,
            "ior=1 应无折射，实际 {o:?}"
        );
        // 穹顶那一路（法线非单位长）同样必须归零。
        let d = dome_refract_offset([0.6, 0.3], 1.0, 0.35);
        assert!(
            (d[0] * d[0] + d[1] * d[1]).sqrt() < 1e-6,
            "ior=1 时穹顶也应无折射，实际 {d:?}"
        );
    }

    #[test]
    fn dispersion_offsets_actually_differ() {
        // 色散的**可观测**判据：三个通道的位移必须真的不同，且差值大到
        // 能在边缘拉开可见的彩边宽度。
        //
        // 旧模型三通道逐位相同（差 0），而 `dispersion_orders_blue_strongest`
        // 用的是 >=，因此**照常通过**——正是它漏掉了这个缺陷。所以要一条
        // 严格不等 + 量级下限的测试。
        let n = glass_normal([95.0, 25.0], HALF, 8.0, 1.0);
        let mag = |ior: f32| {
            let o = refract_offset(n, 1.0 / ior, 0.35);
            (o[0] * o[0] + o[1] * o[1]).sqrt()
        };
        let (r, g, b) = (mag(IOR_R), mag(IOR_G), mag(IOR_B));
        assert!(b > g && g > r, "色散顺序错且必须严格递增：R={r} G={g} B={b}");
        // 边缘位移量级 ≈ 0.35 UV ⇒ 蓝红差应有约 1% 以上（720px 宽窗口上
        // 就是 3~10px 的彩边宽度）。
        let spread = (b - r) / r;
        assert!(
            spread > 0.01,
            "蓝红位移差 {spread:.4} 太小，肉眼看不出彩边（R={r} B={b}）"
        );
    }

    #[test]
    fn shader_constants_match_rust() {
        // 着色器无法单测，但**常量**可以：把 `glass.wgsl` 里的同名 const
        // 抠出来逐一对数值。手抄常量时漏改一边是这类项目的经典故障，
        // 且症状是「折射整体偏一点」，没人会怀疑常量。
        let src = include_str!("../shaders/glass.wgsl");
        for (name, want) in [
            ("SURFACE_TILT", SURFACE_TILT),
            ("EDGE_NXY", EDGE_NXY),
            ("EDGE_FACTOR", EDGE_FACTOR),
        ] {
            let decl = format!("const {name}: f32 = ");
            let at = src
                .find(&decl)
                .unwrap_or_else(|| panic!("glass.wgsl 里找不到 `{name}` 的常量声明"));
            let rest = &src[at + decl.len()..];
            let end = rest.find(';').expect("常量声明应以分号结束");
            let got: f32 = rest[..end]
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("`{name}` 的字面量无法解析：{}", &rest[..end]));
            assert!(
                (got - want).abs() < 1e-6,
                "glass.wgsl 的 {name}={got} 与 Rust 侧的 {want} 不一致"
            );
        }
    }

    #[test]
    fn dispersion_orders_blue_strongest() {
        // B 的 eta 最小 ⇒ 折射向量越长 ⇒ 位移最大（蓝光在边缘外侧）。
        let n = glass_normal([95.0, 25.0], HALF, 8.0, 1.0);
        let mag = |ior: f32| {
            let o = refract_offset(n, 1.0 / ior, 0.35);
            (o[0] * o[0] + o[1] * o[1]).sqrt()
        };
        let (r, g, b) = (mag(IOR_R), mag(IOR_G), mag(IOR_B));
        assert!(
            b >= g && g >= r,
            "色散顺序错：R={r} G={g} B={b}（应 B ≥ G ≥ R）"
        );
    }
}
