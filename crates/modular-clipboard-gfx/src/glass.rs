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
/// # `power` 必须与 [`bevel_weight`] 相乘后使用
///
/// 本函数返回的是**轮廓处的最大位移**（那里斜边最陡、法线最长）。
/// 玻璃的设计是「中心几乎不折、全靠轮廓那一带」，所以调用方必须乘上
/// `bevel_weight(de, radius)`：轮廓处 `power * weight` 可达 0.1~0.35 UV
/// （这是设计意图——没有这么大的位移就看不到折射），带内端
/// `weight → 0`，位移归零。
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

/// 斜角（bevel）带的折射权重：轮廓处 1、带内端 0。
///
/// # 为什么不是 `exp(-de · 0.4)`
///
/// 指数衰减有两个毛病：
///
/// 1. **长尾 + 硬跳变**：`de = 10` 处还剩 1.8%，而那里 [`glass_sdf`] 的法线
///    **早已是零向量**（平坦区），折射被法线突然截断 ⇒ 平滑表面上留一条
///    隐约的棱；
/// 2. **带宽与形状无关**：`exp` 的尺度是写死的像素数，与卡片圆角半径各写
///    一份，改圆角时两者漂移。
///
/// Apple 与各家的移植都把玻璃看成**带斜边的板**：斜边宽度就是圆角半径，
/// 高度剖面在轮廓处最陡、在带内端水平（Apple 用的是凸超椭圆
/// `h(x) = (1-(1-x)⁴)^¼`）。位移 ∝ 剖面斜率，于是：
///
/// - 带宽 = `radius`（与形状同源，改圆角自动跟着变）；
/// - 权重取 `(1 - x)²`（`x = de / radius`）：轮廓处 1、带内端 0，且**内端
///   斜率为 0** ⇒ 与平坦区严丝合缝，没有跳变。
///
/// # CAUTION: 入参是**距边距离**，不是 SDF 值
///
/// 圆角矩形的 SDF 在内部恒等于 `-radius`（平坦区，见 [`glass_sdf`] 的
/// 公式），拿它当距离会让**整片内部的折射强度一模一样**。必须先用
/// [`dist_to_edge`] 换成真实距离。
pub fn bevel_weight(dist_to_edge: f32, radius: f32) -> f32 {
    let band = radius.max(1.0);
    let x = (dist_to_edge.max(0.0) / band).min(1.0);
    let t = 1.0 - x;
    t * t
}

/// 镜面高光强度（Blinn-Phong 的 `dot(N, L)` 形式，归一到 [0, 1]）。
///
/// Apple 的液态玻璃「用一圈**镜面高光取代描边**：朝向光源的一侧最亮，背光
/// 一侧淡下去」（HIG Materials 的口径）。各向同性的 `pow(|slope|, k)` 做
/// 不到这一点——它四边一样亮，看着像描了一圈边而不像玻璃。
///
/// `normal` 是斜角带上**朝外**的法线（单位长），`light` 是**指向光源**的
/// 单位向量（屏幕空间 y 向下 ⇒ 左上打光是 `(-0.6, -0.8)`）。背光侧
/// `dot < 0` 一律取 0——不该出现负高光。
pub fn specular_rim(normal: [f32; 2], light: [f32; 2], shininess: f32) -> f32 {
    let d = normal[0] * light[0] + normal[1] * light[1];
    d.max(0.0).powf(shininess.max(0.5))
}

/// 卡片色在玻璃层里的**最大**混合强度。
///
/// 玻璃层自己合成「底图 + 卡片色」（`fs_glass` 的 `tint`），所以这个值
/// 决定卡片看起来有多像一张有色卡片、有多像一块纯玻璃。
///
/// # 为什么是 0.45 而不是 1.0
///
/// 1.0 就是「不透明卡片」，等于回到玻璃 pass 存在之前的样子——折射与
/// 色散全被这层色盖住。0.45 让底图的明暗与色相仍然透得出来，同时把
/// 卡片压到足够暗（深色主题）/足够亮（浅色主题），前景文字才有对比度：
/// 亮壁纸上如果 tint 太低，浅色主题的深字与玻璃底图会撞在一起。
///
/// 与 `card_alpha`（覆盖度）**无关**：那个决定「透过多少真实桌面」，
/// 这个决定「玻璃自身有多浓的色」。两者相乘得到最终观感。
pub const CARD_TINT_MAX: f32 = 0.45;

/// 由卡片不透明度推出 tint 强度：`card_opacity × CARD_TINT_MAX`。
///
/// 复用同一个滑块（「卡片不透明度」）做两件事是刻意的：用户调低它就是
/// 想要「更通透」——覆盖度变低的同时卡片色也该变淡，否则会出现
/// 「玻璃透了、但卡片色还糊在上面」的割裂。
pub fn card_tint_strength(card_alpha: f32) -> f32 {
    (card_alpha.clamp(0.0, 1.0)) * CARD_TINT_MAX
}

/// 相对亮度（Rec.709 权重）。
pub fn luminance(rgb: [f32; 3]) -> f32 {
    0.212_6 * rgb[0] + 0.715_2 * rgb[1] + 0.072_2 * rgb[2]
}

/// 亮度自适应的目标中灰，见 [`luminosity_adapt`]。
pub const LUMA_TARGET: f32 = 0.5;

/// 亮底压暗的**上限**（相对亮度 1.0 处压暗多少）。
///
/// 取自 HIG Materials 对 clear 变体的规定：背景明亮时叠一层
/// **35% 不透明度的暗色调光层**。这一档比「对称压缩」更狠，但它是
/// 玻璃变透之后唯一的可读性保险——中心磨砂从 0.85 降到 0.5 之后，
/// 白底壁纸上的深色文字全靠这一层撑住对比度。
pub const LUMA_DIM_MAX: f32 = 0.35;

/// 暗底提亮的**上限**（相对亮度 0.0 处提亮多少）。
///
/// 对应 Apple 玻璃的 exposure boost：暗底上玻璃要「透光」而不是发灰。
/// 取 0.20 而不是与压暗对称的 0.35：提亮过多会把纯黑壁纸洗成中灰，
/// 玻璃反而失去「透」的观感（暗色反而更该保持深）。
pub const LUMA_LIFT_MAX: f32 = 0.20;

/// 磨砂/清晰混合的距离尺度（像素）：离边超过这么多像素即算「内部」。
///
/// 2026-10-27 从 36 提到 56：列表行卡片只有 56~70pt 高，`36` 的尺度
/// 意味着几乎整张卡片都落在「内部」档（满磨砂）——玻璃读起来是一块
/// 半透明色块，而不是一块透镜。放大尺度让**边缘清晰带**占满卡片高度
/// 的三分之一左右，才看得出「折射 + 通透」。
pub const FROST_SCALE: f32 = 56.0;

/// 内部（远离边缘）保留的磨砂比例上限。
///
/// 0 = 中心完全清晰（纯透镜），1 = 中心完全磨砂（退回 Acrylic）。
/// 取 0.5：HIG 把「高度透明」的 clear 变体留给媒体背景，而 regular
/// 变体的可读性由亮度自适应 + 卡片色承担。0.85 那档（2026-10-25 的
/// 取值）在深色壁纸上会把玻璃读成一块发灰的色块——这正是「不够通透」
/// 的来源。
pub const FROST_MAX: f32 = 0.5;

/// 亮度自适应：**非对称**地压缩底图亮度。
///
/// HIG 写得很直接：regular 变体「blurs and **adjusts the luminosity of
/// background content** to maintain legibility」；clear 变体在内容偏亮时
/// 建议叠一层 35% 的暗色；visionOS 的 glass「limits the range of background
/// color information so a window can continue to provide contrast」。
///
/// # 为什么不是对称的 `1 + gain·(target − luma)`
///
/// 对称式在 `gain = 0.18` 时只把纯白压 9%、纯黑提 9%——对「白底上的
/// 深色文字」这种最常见也最糟的组合不够。而玻璃变透之后，可读性全靠
/// 这一层。所以按 HIG 的两条指引分开定档：
///
/// - 亮底：线性压暗，`luma = 1` 时压到 `1 - dim_max`（默认 0.35）；
/// - 暗底：线性提亮，`luma = 0` 时提到 `1 + lift_max`（默认 0.20）。
///
/// 三通道**同比例**缩放：只动亮度，不动色相与饱和（逐通道各自拉向
/// 中灰会把颜色一起洗掉）。
pub fn luminosity_adapt(rgb: [f32; 3], dim_max: f32, lift_max: f32) -> [f32; 3] {
    let l = luminance(rgb);
    let k = if l > LUMA_TARGET {
        // 亮底：l 从 target → 1 时，系数从 1 线性降到 1 - dim_max。
        1.0 - dim_max * (l - LUMA_TARGET) / (1.0 - LUMA_TARGET).max(1e-3)
    } else {
        // 暗底：l 从 target → 0 时，系数从 1 线性升到 1 + lift_max。
        1.0 + lift_max * (LUMA_TARGET - l) / LUMA_TARGET.max(1e-3)
    }
    .max(0.0);
    [rgb[0] * k, rgb[1] * k, rgb[2] * k]
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
        // 合成斜角权重之后，中心必须 ≈ 0。
        //
        // CAUTION: 这里必须用 `dist_to_edge` 而不是 SDF——SDF 在内部恒为
        // `-radius`，用它算权重会让中心的折射强度和边缘一模一样。
        let d_center = glass_sdf([0.0, 0.0], HALF, 8.0);
        let de = dist_to_edge([0.0, 0.0], HALF, d_center);
        let eff = edge * bevel_weight(de, 8.0);
        // 距边 30px、圆角半径 8 ⇒ 带外，权重严格为 0（不再是 exp 的长尾）。
        assert_eq!(eff, 0.0, "中心合成位移应为 0，实际 {eff}（距边 {de}）");
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
    fn bevel_weight_is_band_limited_and_smooth() {
        // 轮廓处满权重、带内端严格归零——严格为零（不是 exp 的 1.8% 长尾）
        // 才不会在平坦区留一条硬棱。
        assert!(bevel_weight(0.0, 8.0) > 0.99, "轮廓处权重应接近 1");
        assert_eq!(bevel_weight(8.0, 8.0), 0.0, "带内端必须严格为 0");
        assert_eq!(bevel_weight(50.0, 8.0), 0.0, "带外恒为 0");
        // 单调递减。
        let mut prev = 1.0;
        for i in 1..=8 {
            let w = bevel_weight(i as f32, 8.0);
            assert!(w < prev, "权重应单调递减：{i}px 处 {w} ≥ {prev}");
            prev = w;
        }
        // 内端斜率为 0（与平坦区严丝合缝）：最后一格的差分必须远小于第一格。
        let first = bevel_weight(0.0, 8.0) - bevel_weight(1.0, 8.0);
        let last = bevel_weight(7.0, 8.0) - bevel_weight(8.0, 8.0);
        assert!(
            last < first * 0.25,
            "带内端应平滑归零：首格 {first}、末格 {last}"
        );
        // 带宽随圆角半径走（与形状同源）。
        assert!(
            bevel_weight(12.0, 16.0) > bevel_weight(12.0, 8.0),
            "圆角大的卡片斜角带更宽"
        );
        // 退化：半径为 0 时不得除零 / NaN。
        assert!(bevel_weight(3.0, 0.0).is_finite());
    }

    #[test]
    fn specular_is_directional() {
        // Apple 的要点：高光**取代描边**且朝向光源的一侧最亮。各向同性的
        // 写法过不了这条——它四边一样亮。
        let light = [-0.6, -0.8]; // 左上打光（屏幕空间 y 向下）
        // 最亮处是**外法线与光源同向**的那一小段轮廓（本例即左上方向的
        // 斜边），不是「整个上边缘」——这正是它取代描边后仍读作玻璃的
        // 原因：亮带沿轮廓渐变，而不是一圈等宽的线。
        let lit = specular_rim(light, light, 8.0);
        let back = specular_rim([0.6, 0.8], light, 8.0);
        assert!(lit > 0.99, "正对光源处应几乎全亮，实际 {lit}");
        assert_eq!(back, 0.0, "背光侧不得有高光，实际 {back}");
        // 上边缘比左边缘亮（光源更偏上），右/下边缘完全不亮。
        let up = specular_rim([0.0, -1.0], light, 8.0);
        let down = specular_rim([0.0, 1.0], light, 8.0);
        let left = specular_rim([-1.0, 0.0], light, 8.0);
        let right = specular_rim([1.0, 0.0], light, 8.0);
        assert!(up > left && left > 0.0, "上 > 左 > 0：{up} / {left}");
        assert_eq!(down, 0.0, "下边缘背光，实际 {down}");
        assert_eq!(right, 0.0, "右边缘背光，实际 {right}");
        // 平坦区（法线为零向量）没有高光。
        assert_eq!(specular_rim([0.0, 0.0], light, 8.0), 0.0);
        // 锐度越大高光越集中（斜射处衰减更快）。
        assert!(specular_rim([-1.0, 0.0], light, 32.0) < left);
    }

    #[test]
    fn card_tint_strength_is_proportional_and_clamped() {
        // 覆盖度与 tint 强度是**两个**概念，但共用同一个滑块：卡片越不
        // 透明，卡片色越浓。两者都不得越界（配置写坏成 2.0 时不能出现
        // 大于 1 的混合系数——mix 的 t 超过 1 会外插出负色）。
        assert_eq!(card_tint_strength(0.0), 0.0);
        assert!((card_tint_strength(1.0) - CARD_TINT_MAX).abs() < 1e-6);
        assert!(card_tint_strength(2.0) <= CARD_TINT_MAX, "上界必须钳住");
        assert_eq!(card_tint_strength(-1.0), 0.0, "负值钳到 0");
        // 单调：卡片越不透明，色越浓。
        let mut prev = -1.0;
        for a in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let k = card_tint_strength(a);
            assert!(k > prev, "应单调递增：a={a} 得 {k}");
            prev = k;
        }
        // 上限本身必须留出「底图透得出来」的余量：超过 0.6 就等于把玻璃
        // 盖成一张实色卡（那正是这轮要修的 bug）。
        assert!(card_tint_strength(1.0) <= 0.6, "tint 上限过高会把玻璃盖死");
    }

    #[test]
    fn luminosity_adapt_compresses_toward_mid_grey() {
        // 非对称压缩：亮底最多压 dim_max（HIG clear 变体的 35% 调光层），
        // 暗底最多提 lift_max（Apple 的 exposure boost）。
        let bright = luminosity_adapt([0.9, 0.9, 0.9], LUMA_DIM_MAX, LUMA_LIFT_MAX);
        let dark = luminosity_adapt([0.1, 0.1, 0.1], LUMA_DIM_MAX, LUMA_LIFT_MAX);
        assert!(bright[0] < 0.9, "亮底应被压暗，实际 {bright:?}");
        assert!(dark[0] > 0.1, "暗底应被提亮，实际 {dark:?}");
        // 纯白压到 1 - dim_max、纯黑提到 1 + lift_max（两端的定档）。
        let white = luminosity_adapt([1.0, 1.0, 1.0], LUMA_DIM_MAX, LUMA_LIFT_MAX);
        assert!(
            (white[0] - (1.0 - LUMA_DIM_MAX)).abs() < 1e-5,
            "纯白应压到 {}，实际 {}",
            1.0 - LUMA_DIM_MAX,
            white[0]
        );
        let black = luminosity_adapt([0.0, 0.0, 0.0], LUMA_DIM_MAX, LUMA_LIFT_MAX);
        assert!(black[0] == 0.0, "纯黑乘任何系数仍是 0：{black:?}");
        // 关闭时恒等。
        assert_eq!(luminosity_adapt([0.7, 0.2, 0.4], 0.0, 0.0), [0.7, 0.2, 0.4]);
        // 只动亮度、不动色相：三通道同比例 ⇒ 比值不变。
        let src = [0.8, 0.4, 0.2];
        let out = luminosity_adapt(src, LUMA_DIM_MAX, LUMA_LIFT_MAX);
        assert!(
            ((out[0] / out[1]) - (src[0] / src[1])).abs() < 1e-5,
            "色相必须保持：{src:?} -> {out:?}"
        );
        // 已经落在中灰上的输入几乎不动。
        let mid = luminosity_adapt([0.5, 0.5, 0.5], LUMA_DIM_MAX, LUMA_LIFT_MAX);
        assert!((mid[0] - 0.5).abs() < 1e-6, "中灰不该被推动：{mid:?}");
        // 极端输入不得产生负比例（dim_max > 1 是配置外的输入，也不能出负数）。
        assert!(
            luminosity_adapt([1.0, 1.0, 1.0], 1.0, 1.0)
                .iter()
                .all(|c| *c >= 0.0)
        );
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
            ("LUMA_TARGET", LUMA_TARGET),
            ("LUMA_DIM_MAX", LUMA_DIM_MAX),
            ("LUMA_LIFT_MAX", LUMA_LIFT_MAX),
            ("FROST_SCALE", FROST_SCALE),
            ("FROST_MAX", FROST_MAX),
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
