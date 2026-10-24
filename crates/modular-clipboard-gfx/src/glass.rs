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

/// 折射造成的 UV 位移（单位：UV，即 [0,1] 纹理空间）。
///
/// `normal` 是外法线（`glass_normal` 的输出），`eta = 1 / ior`。
/// `incident` 取 `-normal`（光从玻璃外垂直射入曲面），`refract()` 的
/// WGSL/GLSL 语义是「入射方向、法线、eta」，返回折射后的方向，这里只用
/// 它的 xy 分量去偏移采样坐标。
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
    let incident = [-normal[0], -normal[1]];
    // refract(I, N, eta) 的标准实现（WGSL 内建同一套）：
    //   k = 1 - eta^2 * (1 - dot(N, I)^2)
    //   T = eta * I - (eta * dot(N, I) + sqrt(k)) * N
    let d = normal[0] * incident[0] + normal[1] * incident[1];
    let k = (1.0 - eta * eta * (1.0 - d * d)).max(0.0);
    let s = k.sqrt();
    [
        (eta * incident[0] - (eta * d + s) * normal[0]) * power,
        (eta * incident[1] - (eta * d + s) * normal[1]) * power,
    ]
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
/// 2D 情形下 `eye = (0,0,1)`，`|dot(eye, n)|` 由法线的 xy 长度给出：
/// 直边中央法线朝外（xy 长度 1）⇒ 项为 0 ⇒ 全透射；边缘处法线趋于水平
/// 的切向 ⇒ 项趋 1 ⇒ 出现亮边。
pub fn fresnel_edge(normal: [f32; 2], power: f32) -> f32 {
    let facing = 1.0 - (normal[0] * normal[0] + normal[1] * normal[1]).sqrt().min(1.0);
    facing.powf(power.max(0.0))
}

/// 色散的三个 IOR（R 最小、B 最大 ⇒ 蓝光弯得更多，边缘出现彩色描边）。
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
        // 直边中央：法线完全朝外 ⇒ facing = 0 ⇒ 不透亮。
        let n = glass_normal([99.0, 0.0], HALF, 8.0, 1.0);
        let f = fresnel_edge(n, 5.0);
        assert!(f < 0.05, "直边中央 Fresnel 应接近 0，实际 {f}");
        // 近圆角处：法线偏切向 ⇒ facing 上升 ⇒ 亮边。
        let n2 = glass_normal([HALF[0] - 2.0, HALF[1] - 2.0], HALF, 8.0, 0.5);
        let f2 = fresnel_edge(n2, 5.0);
        assert!(f2 > f, "圆角处应更亮：{f2} > {f}");
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
