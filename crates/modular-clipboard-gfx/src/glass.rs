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

/// 磨砂尺度的**相对系数**：以卡片短半轴为基准的倍数。
///
/// # 为什么不再是绝对像素（2026-10-28 修）
///
/// 早前是常量 56px（更早 36px），注释两次都在「卡片太糊 / 卡片太透」
/// 之间来回调。根因是**量纲错了**：磨砂梯度是按「距边距离」展开的，
/// 而这个距离的**上限是卡片短半轴**——一个随窗口高度、字号、DPI 一起
/// 变的量。用绝对像素定尺度，等于把「边缘清晰带占卡片高度的比例」
/// 交给运气：
///
/// - 行卡片 58pt 高、DPI 100% ⇒ 短半轴 29px，56 的尺度让中心只到
///   `0.5 × 29/56 = 0.26` ——整张卡片近乎全清晰，背景条纹直接穿透
///   文字区，观感是「一块带扭曲的半透明板」而不是玻璃；
/// - 同一张卡片在 150% DPI 下短半轴 43.5px ⇒ 中心到 `0.39`，糊度
///   凭空多一半。**同一份配置在两个 DPI 上不是同一个效果**。
///
/// 改成比例后，任何尺寸、任何 DPI 的卡片都得到同一条归一化曲线：
/// 边缘清晰、中心满磨砂，梯度铺满整个半高。
///
/// # 为什么是 1.0 而不是更小（2026-10-28 实测校准）
///
/// 0.667 那版让「清晰带」只占卡片高度的三分之一，其余三分之二直接
/// 满档磨砂——实测行卡片读成一块**糊卡**，折射带只剩贴边十几个像素，
/// 玻璃的辨识度（能看见身后内容被弯折）反而丢了。1.0 让 `de` 在整个
/// 半高上爬升：边缘 0、四分之一处约 0.22、正中心才 0.5。
pub const FROST_RATIO: f32 = 1.0;

/// 自适应磨砂尺度的下限（像素）。
///
/// 极小的玻璃矩形（类型栏的 96px 见方弹窗、行内小卡）若按比例算出
/// 个位数尺度，`de / scale` 会在零点几个像素内冲到 1，磨砂梯度退化成
/// 硬边。给一个下限，让这类小面板保留一条可见的清晰带。
pub const FROST_SCALE_MIN: f32 = 8.0;

/// 自适应磨砂尺度的上限（像素）。
///
/// 详情卡、设置卡这类大面板的短半轴可达数百像素，按比例会算出比
/// 屏幕还大的尺度 ⇒ 中心永远到不了满磨砂 ⇒ 大面板反而最「生」（背景
/// 全清晰、文字最难读）。封顶后大面板在离边 120px 处即达满档，与
/// 行卡片的观感一致。
pub const FROST_SCALE_MAX: f32 = 120.0;

/// 内部（远离边缘）保留的磨砂比例上限。
///
/// 0 = 中心完全清晰（纯透镜），1 = 中心完全磨砂（退回 Acrylic）。
///
/// # 为什么从 0.5 降到 0.32（2026-10-28「背景玻璃要通透」）
///
/// 0.85（2026-10-25）在深色壁纸上把玻璃读成发灰色块；降到 0.5 之后
/// 「通透」够了，但卡片中心仍有**一半**像素来自磨砂链——那张图是
/// 1/4 分辨率三趟高斯的产物，混进来就把身后的内容糊掉一半，玻璃与
/// Acrylic 的差别只剩边缘那一圈折射。
///
/// 0.32 的含义是：卡片最深处仍有 **68% 的清晰底图**（带折射位移）
/// 透出来，磨砂只负责压平高频噪点（壁纸纹理、桌面上的小字），保证
/// 文字可读。玻璃的辨识特征——「能看清身后是什么，同时看见它被弯折」
/// ——正是这个比例撑住的；再往下（≤ 0.2）文字会直接坐在桌面内容上。
pub const FROST_MAX: f32 = 0.32;

/// **背景层不透明度**的默认档（再乘 `window_opacity`）。
///
/// # 为什么不是 1.0（2026-10-28 修「无法渲染透明效果」）
///
/// 玻璃 pass 早前让背景层沿用捕获图的 alpha（恒为 1）⇒ 整窗**不透明**，
/// 真实桌面一点都透不出来。玻璃模式于是在窗口大部分面积上与「不透明
/// 窗口」没有区别，用户拖任何透明度滑块都看不到变化——报障的原话是
/// 「alpha 通道异常、渲染不正确、无法渲染透明效果」。
///
/// 0.35 的含义：捕获底图只占三分之一权重（`src.rgb + (1-a)·桌面`），
/// 合成结果 ≈ 真桌面 + 一档亮度自适应。**看得见桌面**，而可读性由
/// 亮度自适应与 egui 那层半透明 `bg` 兜住——比把窗口糊死好，也比
/// 完全不铺（等同 Translucent）多一点「玻璃压在桌面上」的分量。
pub const BG_ALPHA_DEFAULT: f32 = 0.35;

/// **窗口背景层**（不属于任何卡片的那些像素）的磨砂比例。
///
/// # 为什么背景层不能沿用磨砂的结果（2026-10-28）
///
/// 早前 `fs_glass` 的初值直接是 `blurred`——也就是 Acrylic 那张模糊
/// 底图。于是玻璃模式下整窗背景与磨砂模式**逐字节相同**，只有卡片
/// 里才有玻璃。用户看到的是「背景玻璃没渲染」：玻璃模式开不开，窗口
/// 大部分面积毫无变化（而且糊）。
///
/// 玻璃的背景层应当**清晰**：它要表达的是「一块架在桌面上的透明
/// 玻璃」，不是「一层毛玻璃滤镜」。取 0.18 而不是 0：完全不柔化的话，
/// 桌面上的高频（文字、图标边缘、壁纸纹理）会与前景文字抢注意力，
/// 而这一层之上只有 egui 的半透明 `bg`，压不住。0.18 相当于把底图
/// 的高频削掉约两成，细节与通透都在。
///
/// 卡片的磨砂仍走 [`frost_mix`]（边缘清晰、中心 [`FROST_MAX`]）——
/// 那是**卡片**的语义，与背景层无关。
pub const BG_FROST: f32 = 0.18;

/// 窗口背景层的合成：清晰桌面为主 + [`BG_FROST`] 比例的柔化，再过
/// 亮度自适应。
///
/// 与 `glass.wgsl` 的 `fs_glass` 里 `out` 初值的计算逐行对应。
/// 抽成函数是为了能单测「背景层到底有多透」——着色器里那几行没法测，
/// 而「玻璃模式看起来就是磨砂」这种故障恰恰只能靠数值判据发现。
pub fn background_glass(sharp: [f32; 3], blur: [f32; 3]) -> [f32; 3] {
    let mut mixed = [0.0; 3];
    for i in 0..3 {
        mixed[i] = sharp[i] + (blur[i] - sharp[i]) * BG_FROST;
    }
    luminosity_adapt(mixed, LUMA_DIM_MAX, LUMA_LIFT_MAX)
}

/// 磨砂爬升的曲线指数。
///
/// 取 0.9（略小于 1，即中段稍早饱和）：卡片正中间是文字最密、最需要
/// 「背景被压平」的位置，线性曲线在 `x = 0.5` 时只给到 0.5 的磨砂量
/// （乘 `FROST_MAX` 后 0.25），中段仍偏清晰。0.9 把它提到 0.27，
/// 而边缘那条清晰的折射带几乎不受影响（`x` 小时两条曲线差几个百分点）。
///
/// NOTE: 不要再往 0.75 以下走——0.75 实测把中段压成糊块，折射带消失。
pub const FROST_GAMMA: f32 = 0.9;

/// 按卡片短半轴算出磨砂尺度（像素）。
///
/// 与 `glass.wgsl` 的 `frost_scale` 逐行对应。`half` 是**物理像素**
/// 的半尺寸（uniform 里的 `rects[i].zw`）。
///
/// 取 `min(half.x, half.y)`：磨砂沿「最薄的那个方向」展开，因为
/// [`dist_to_edge`] 本身就在两个方向取最小值——用长边会让长条卡片
/// 的清晰带宽度远超卡片自身高度。
pub fn frost_scale(half: [f32; 2]) -> f32 {
    let m = half[0].abs().min(half[1].abs());
    (m * FROST_RATIO).clamp(FROST_SCALE_MIN, FROST_SCALE_MAX)
}

/// 某一点应有的磨砂比例（0 = 全清晰，`FROST_MAX` = 满档）。
///
/// `de` 是 [`dist_to_edge`] 的结果；`half` 是物理像素半尺寸。
/// 与 `glass.wgsl` 的 `frost_mix` 逐行对应。
pub fn frost_mix(de: f32, half: [f32; 2]) -> f32 {
    let x = (de.max(0.0) / frost_scale(half)).clamp(0.0, 1.0);
    x.powf(FROST_GAMMA) * FROST_MAX
}

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

// ---- 卡片底色混色（tint）：吸收 + 散射 ------------------------------

/// 吸收强度：`tint` 作为**透过率**被拉向自身颜色的程度。
///
/// 0 = 完全不吸收（玻璃全透），1 = 透过率就是 `tint` 本身。
pub const TINT_ABSORB: f32 = 0.45;

/// 色度放大：把 `tint` 除以自身亮度得到「均值 1 的色度」，再按这个比例
/// 放大通道间的差距。
///
/// 深色主题的 `card_bg` 是冷蓝灰（R < G < B），透过率带上这个色偏后卡片
/// 会明显**泛冷调**而不是单纯变暗——「混色」的可见度主要来自它。浅色
/// 主题的 `card_bg` 是纯白（色度恒为 1）⇒ 这一项自动失效。
pub const TINT_CHROMA: f32 = 0.5;

/// 散射（乳光）的基准增益。
pub const TINT_SCATTER: f32 = 0.18;

/// 浅色 tint 的散射衰减（乘在 [`TINT_SCATTER`] 上）。
///
/// 散射是加性的，浅色 tint 接近纯白 ⇒ 加的是白光，会把亮底推到饱和、
/// 把折射带与彩边冲平。按 tint 亮度线性衰减：深色 1.0、浅色 0.55。
pub const TINT_SCATTER_LIGHT_DAMP: f32 = 0.55;

/// 深色 tint 的**透过率地板**。
///
/// 深色玻璃一路按 `mix(1, tint, ABSORB)` 算下去，暗底图会连同高光一起
/// 被吃掉 ⇒ 卡片变成「黑玻璃」，折射与边缘光全消失。给透过率一个下限，
/// 保证身后内容始终可见。按 `1 - t_lum` 缩放（浅色主题地板为 0）。
pub const TINT_FLOOR_DARK: f32 = 0.28;

/// 两段式模型相对「替换式」的占比。
pub const TINT_MIX: f32 = 0.9;

/// 有色玻璃混色。与 `glass.wgsl` 的 `glass_tint` 逐行对应。
///
/// `rgb` 是折射 + 磨砂 + 亮度自适应之后的底图，`tint` 是卡片底色，
/// `a` 是覆盖度（[`card_tint_strength`] 的结果，≤ [`CARD_TINT_MAX`]）。
///
/// # 为什么不是一句 `mix`
///
/// 替换式覆盖把底图的明暗层次与高频一起压平，且观感随主题分裂：
/// 浅色主题 `card_bg = #FFFFFF` ⇒ **洗白**（折射与彩边被冲掉）；深色主题
/// `card_bg = #2F3540`（luma 0.20）⇒ **压暗**（深色壁纸上糊成灰板）。
///
/// 真实有色玻璃是两件事同时发生：**吸收**（乘法，`rgb × transmit`，保留
/// 底图结构）与**散射**（加性，`+ scatter`，乳白雾度的质感来源）。
///
/// # 端点语义不变
///
/// `a = 0` → 原样返回底图；`a = 1` → 精确返回 `tint`。两段式只在中间区间
/// 生效（权重 `a(1-a)×4`，峰值在 `a = 0.5`），所以「卡片不透明度」滑块的
/// 语义与旧实现连续。
pub fn glass_tint(rgb: [f32; 3], tint: [f32; 3], a: f32) -> [f32; 3] {
    let lerp3 = |x: [f32; 3], y: [f32; 3], t: f32| -> [f32; 3] {
        [
            x[0] + (y[0] - x[0]) * t,
            x[1] + (y[1] - x[1]) * t,
            x[2] + (y[2] - x[2]) * t,
        ]
    };
    let base = lerp3(rgb, tint, a);
    let t_lum = luminance(tint).clamp(0.0, 1.0);
    // 色度：`tint / luma` 的加权均值为 1 ⇒ `mix(1, chroma, k)` 是围绕
    // 中性色的色偏放大。纯白 / 纯灰色度恒为 1 ⇒ 不染色。
    let denom = t_lum.max(1e-3);
    let chroma = [
        1.0 + (tint[0] / denom - 1.0) * TINT_CHROMA,
        1.0 + (tint[1] / denom - 1.0) * TINT_CHROMA,
        1.0 + (tint[2] / denom - 1.0) * TINT_CHROMA,
    ];
    let floor = TINT_FLOOR_DARK * (1.0 - t_lum);
    let absorb = lerp3([1.0, 1.0, 1.0], tint, TINT_ABSORB);
    let transmit = [
        (absorb[0] * chroma[0]).max(floor),
        (absorb[1] * chroma[1]).max(floor),
        (absorb[2] * chroma[2]).max(floor),
    ];
    let scatter_gain = TINT_SCATTER * (1.0 + (TINT_SCATTER_LIGHT_DAMP - 1.0) * t_lum);
    let tinted = [
        rgb[0] * transmit[0] + tint[0] * scatter_gain,
        rgb[1] * transmit[1] + tint[1] * scatter_gain,
        rgb[2] * transmit[2] + tint[2] * scatter_gain,
    ];
    let k = (TINT_MIX * (a * (1.0 - a) * 4.0)).clamp(0.0, 1.0);
    lerp3(base, tinted, k)
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

/// 边缘覆盖率过渡带的**半宽**（像素）。
///
/// # 为什么玻璃层必须自己做抗锯齿
///
/// 卡片形状由 SDF 描述，而 SDF 只回答「在里还是在外」——`fs_glass` 早前
/// 就是硬判 `d > 0 → 丢弃`。于是**圆角与斜边是像素级阶梯**：整窗 2× 超
/// 采样（SSAA）能缓解，但玻璃 pass 是**全屏三角形 + 逐像素 SDF**，它的
/// 边界频率不受几何超采样保护——超采样只放大目标分辨率，SDF 判据仍然
/// 在放大后的像素格上二值化，阶梯只是变细，不会消失。
///
/// 而 egui 那层（文字、圆角填充）有自己的 feathering，两者边缘一旦错开
/// 半个像素，玻璃的硬边就会从卡片底下露出来一圈毛刺。
///
/// 0.5px ⇒ 过渡带总宽 1px（物理像素）。与 egui 的 1px feathering 同量级。
pub const EDGE_AA_HALF_WIDTH: f32 = 0.5;

/// 点到边缘的**内部深度**（内部为正、外部为负）。
///
/// # 为什么不能直接用 SDF
///
/// 圆角矩形 SDF 在**平坦区恒为 `-radius`**——它是「形状内还是外」的
/// 判定式，不是真实距离。两个后果：
///
/// - 直角面板（`r = 0`）内部 `d == 0`，拿它算覆盖率会得到 0.5 ⇒
///   **整块顶栏变半透明**（正是 2026-10-28 那次「整窗玻璃消失」的同一个
///   陷阱换了个入口）；
/// - 圆角面板内部 `d` 被截断在 `-r`，深一点的像素与贴边一像素的像素
///   得到相同的覆盖率。
///
/// # CAUTION: 「到直边的距离」**只能用在内部**
///
/// 早前内外都用 `max(-sdf, 到直边距离)`，结果**圆角被填成直角**
/// （2026-10-28 用户报障，且只有玻璃模式有——其他背景模式的圆角由
/// egui 自己画，不走这条路径）。
///
/// 病根：圆角切掉的那一小块角落，点虽然落在**两条直边的内侧**（于是
/// 「到直边距离」为正），却已经被圆角弧切到形状外了。取 `max` 就把它
/// 判成内部 ⇒ 覆盖率 1 ⇒ 角落被填实。
///
/// 例：`half = [300, 29]`、`r = 12`，取角落 `p = (-299, -28)`：
/// `sdf = +3.56`（外部），而 `min(300-299, 29-28) = 1.0`（两条直边的
/// 内侧）⇒ `max(-3.56, 1.0) = 1.0` ⇒ 覆盖率 1。圆角越大填得越多。
///
/// 只有 SDF 知道弧在哪里，所以**外部一律用 `-sdf`**。
pub fn edge_depth(p: [f32; 2], half: [f32; 2], sdf: f32) -> f32 {
    if sdf > 0.0 {
        // 外部：含圆角切掉的角落，只有 SDF 是对的。
        -sdf
    } else {
        // 内部：SDF 被截断在 `-r`，深处与贴边同值，改用真实距离兜底。
        (-sdf).max((half[0] - p[0].abs()).min(half[1] - p[1].abs()))
    }
}

/// 由内部深度算出覆盖率（0 = 完全在外，1 = 完全在内）。
///
/// 与 `glass.wgsl` 的 `edge_coverage` 逐行对应。映射：
/// `depth = +0.5` → 1；`depth = 0`（正好在边上）→ 0.5；`depth = -0.5` → 0。
/// 线性即够——真实覆盖面积在 1px 带内与深度近似线性，再上 smoothstep
/// 只会让边缘发虚。
pub fn edge_coverage(depth: f32) -> f32 {
    (EDGE_AA_HALF_WIDTH + depth).clamp(0.0, 1.0)
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

/// 色散位移的**独立基准幅度**。
///
/// # 为什么色散不能乘在折射强度上（2026-10-28 修）
///
/// 早前三个通道各自按自己的 IOR 算 `refract_offset(n, 1/ior, refract)`，
/// 通道间的位移差 ≈ `refract × (ior 差带来的相对差)`。于是：
///
/// - 用户把「折射强度」调到 0.15（默认 0.35，实测确实有人这么调，
///   因为默认值的边缘会把背景撕成条）时，**色散也一起缩到 43%**，
///   色散滑块从 0 拖到 1 几乎看不出变化；
/// - 反过来把折射拉到 1.0，色散强到像显卡坏了。
///
/// 两个滑块表达的是两件事：折射是「玻璃有多弯」，色散是「不同波长
/// 弯得差多少」。后者应该有自己的基准，而不是寄生前者的幅值。
///
/// 取 0.35 = 旧默认折射强度：在默认配置下色散幅度与改造前**逐点一致**，
/// 只是不再随折射滑块漂移。
pub const DISPERSION_POWER: f32 = 0.35;

/// 某个 IOR 相对默认 IOR 的**附加位移**（色散分量）。
///
/// 与 `glass.wgsl` 的 `dispersion_delta` 逐行对应。返回值在
/// `dispersion = 1` 时直接加到基础折射位移上；`dispersion = 0` 时
/// 整个式子归零，三通道共用基础位移（老行为）。
pub fn dispersion_delta(normal: [f32; 2], eta: f32) -> [f32; 2] {
    let a = refract_offset(normal, eta, DISPERSION_POWER);
    let b = refract_offset(normal, DEFAULT_ETA, DISPERSION_POWER);
    [a[0] - b[0], a[1] - b[1]]
}

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
            ("FROST_RATIO", FROST_RATIO),
            ("FROST_SCALE_MIN", FROST_SCALE_MIN),
            ("FROST_SCALE_MAX", FROST_SCALE_MAX),
            ("FROST_MAX", FROST_MAX),
            ("FROST_GAMMA", FROST_GAMMA),
            ("BG_FROST", BG_FROST),
            ("BG_ALPHA_DEFAULT", BG_ALPHA_DEFAULT),
            ("DISPERSION_POWER", DISPERSION_POWER),
            ("EDGE_AA_HALF_WIDTH", EDGE_AA_HALF_WIDTH),
            ("TINT_ABSORB", TINT_ABSORB),
            ("TINT_CHROMA", TINT_CHROMA),
            ("TINT_SCATTER", TINT_SCATTER),
            ("TINT_SCATTER_LIGHT_DAMP", TINT_SCATTER_LIGHT_DAMP),
            ("TINT_FLOOR_DARK", TINT_FLOOR_DARK),
            ("TINT_MIX", TINT_MIX),
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

    /// 磨砂分布必须**与卡片尺寸无关**地保持「边缘清晰、中心满档」。
    ///
    /// # 这条守的是什么
    ///
    /// 早前尺度是绝对像素 56，而行卡片的短半轴只有 29px（100% DPI）
    /// ——中心只到 `0.5 × 29/56 ≈ 0.26`，整张卡近乎全清晰，背景条纹
    /// 直接穿透文字区。同一份配置在 150% DPI 下又变成 0.39。用户看到
    /// 的是「玻璃时好时坏」，而代码里没有一个常量是错的。
    #[test]
    fn frost_profile_is_scale_invariant() {
        let cards = [[300.0_f32, 29.0], [450.0, 43.5], [400.0, 200.0]];
        for half in cards {
            let centre = frost_mix(half[1], half);
            assert!(
                (centre - FROST_MAX).abs() < 1e-3,
                "中心必须满档：half={half:?} 得到 {centre}，期望 {FROST_MAX}"
            );
            assert_eq!(frost_mix(0.0, half), 0.0, "贴边处必须全清晰");
            let mid = frost_mix(half[1] * 0.5, half);
            assert!(
                mid > FROST_MAX * 0.3 && mid < FROST_MAX,
                "中段应处于清晰与满档之间：half={half:?} 得到 {mid}"
            );
            let mut prev = -1.0;
            for i in 0..=20 {
                let de = half[1] * (i as f32) / 20.0;
                let v = frost_mix(de, half);
                assert!(v >= prev - 1e-6, "磨砂量应随距边距离单调不减");
                prev = v;
            }
        }
    }

    /// 背景层必须是「清晰为主」，不是整片磨砂。
    ///
    /// # 这条守的是什么
    ///
    /// `fs_glass` 的 `out` 初值早前直接是模糊底图（`blurred`），于是玻璃
    /// 模式的整窗背景与 Acrylic 逐字节相同——用户报的是「背景玻璃没
    /// 渲染」，而日志里一次错误都没有（2026-10-28）。
    ///
    /// 判据用「清晰占比 ≥ 0.75」而不是具体数字：BG_FROST 是审美常量，
    /// 会微调；但一旦有人把它推到 0.5 以上，玻璃背景就退回磨砂了。
    #[test]
    fn background_is_clear_not_frosted() {
        // 清晰占比：`BG_FROST` 一旦被推到 0.5 以上，玻璃背景就退回磨砂。
        // 判据挂在**函数结果**上而不是常量本身——常量断言会被编译器
        // 折叠，clippy 也会直接拒绝（`assertions_on_constants`）。
        let sharp = [1.0_f32, 0.0, 0.0];
        let blur = [0.0_f32, 0.0, 0.0];
        let got = background_glass(sharp, blur);
        let pure_sharp = luminosity_adapt(sharp, LUMA_DIM_MAX, LUMA_LIFT_MAX);
        assert!(
            got[0] > pure_sharp[0] * 0.75,
            "背景层清晰分量只剩 {:.0}%，玻璃背景会退回磨砂",
            got[0] / pure_sharp[0] * 100.0
        );
        // 两端一致：清晰图与模糊图相同时，柔化不该改变任何东西。
        let same = [0.4_f32, 0.45, 0.5];
        assert_eq!(
            background_glass(same, same),
            luminosity_adapt(same, LUMA_DIM_MAX, LUMA_LIFT_MAX)
        );
        // 清晰图与模糊图不同时，结果必须**不等于**纯磨砂那一侧——
        // 否则就是又退回「整窗铺模糊底图」。
        let sharp = [0.90_f32, 0.10, 0.20];
        let blur = [0.10_f32, 0.90, 0.20];
        let got = background_glass(sharp, blur);
        let pure_blur = luminosity_adapt(blur, LUMA_DIM_MAX, LUMA_LIFT_MAX);
        assert!(
            got.iter()
                .zip(pure_blur.iter())
                .any(|(a, b)| (a - b).abs() > 1e-3),
            "背景层与纯磨砂结果相同，说明清晰分量没进来: {got:?}"
        );
        // 且应当明显偏向清晰那一侧。
        let pure_sharp = luminosity_adapt(sharp, LUMA_DIM_MAX, LUMA_LIFT_MAX);
        let d_sharp: f32 = got.iter().zip(pure_sharp.iter()).map(|(a, b)| (a - b).abs()).sum();
        let d_blur: f32 = got.iter().zip(pure_blur.iter()).map(|(a, b)| (a - b).abs()).sum();
        assert!(
            d_sharp < d_blur,
            "背景层应更接近清晰图：d_sharp={d_sharp} d_blur={d_blur}"
        );
    }

    /// 卡片中心的磨砂占比必须留出足够的清晰分量（「通透」的下限）。
    #[test]
    fn card_centre_keeps_most_of_the_sharp_layer() {
        let half = [300.0_f32, 29.0];
        let centre = frost_mix(half[1], half);
        assert!((centre - FROST_MAX).abs() < 1e-3);
        assert!(
            1.0 - centre >= 0.65,
            "卡片中心应保留 ≥65% 清晰分量，实际只有 {:.0}%（磨砂 {centre}）",
            (1.0 - centre) * 100.0
        );
    }

    /// 极小的玻璃矩形不能把尺度压成零（会让梯度变硬边）。
    #[test]
    fn frost_scale_has_floor_and_ceiling() {
        assert_eq!(frost_scale([2.0, 1.0]), FROST_SCALE_MIN);
        assert_eq!(frost_scale([5000.0, 5000.0]), FROST_SCALE_MAX);
        let s = frost_scale([300.0, 43.5]);
        assert!((s - 43.5 * FROST_RATIO).abs() < 1e-4, "应取短半轴，实际 {s}");
    }

    /// 色散幅度**不能**随折射强度漂移。
    ///
    /// 早前色散位移 = `refract_offset(1/ior, refract)` 之差，与 `refract`
    /// 成正比 ⇒ 用户把折射调到 0.15 后色散滑块几乎失效。现在色散有自己的
    /// 基准 `DISPERSION_POWER`，签名里根本不接收折射强度。
    #[test]
    fn dispersion_is_independent_of_refract_strength() {
        let n = glass_normal([95.0, 25.0], HALF, 8.0, 1.0);
        let d = dispersion_delta(n, 1.0 / IOR_B);
        let mag = (d[0] * d[0] + d[1] * d[1]).sqrt();
        assert!(mag > 1e-5, "色散分量不应为零（否则彩边恒不可见）");
        let zero = dispersion_delta(n, DEFAULT_ETA);
        assert!(
            zero[0].abs() < 1e-9 && zero[1].abs() < 1e-9,
            "默认 IOR 的色散分量必须为零，实际 {zero:?}"
        );
    }

    /// 直角面板内部必须**满覆盖**。
    ///
    /// # 这条守的是什么
    ///
    /// 圆角矩形 SDF 在平坦区恒为 `-r`。直角面板 `r = 0` ⇒ 内部 `d == 0`。
    /// 若直接拿 `d` 算覆盖率（`0.5 - d`），**整块顶栏会变成 50% 透明的
    /// 玻璃**——它不重叠任何卡片，用户看到的是「顶栏比卡片淡一截」，
    /// 而日志与验证层都没有任何提示。
    ///
    /// [`edge_depth`] 用「到直边的距离」兜底正是为这条。
    #[test]
    fn right_angle_panel_is_fully_opaque_inside() {
        let half = [300.0_f32, 22.0];
        for p in [[0.0, 0.0], [120.0, 8.0], [-299.0, 0.0]] {
            let sdf = glass_sdf(p, half, 0.0);
            assert_eq!(sdf, 0.0, "前置条件：直角矩形内部 SDF 恒为 0");
            let cov = edge_coverage(edge_depth(p, half, sdf));
            assert!(
                (cov - 1.0).abs() < 1e-6,
                "直角面板内部必须满覆盖：p={p:?} 得到 {cov}"
            );
        }
    }

    /// 圆角**切掉的角落**必须完全透明。
    ///
    /// # 这条守的是什么
    ///
    /// [`edge_depth`] 里的「到直边距离」在角落里为正（那一点确实落在两条
    /// 直边的内侧），却已被圆角弧切到形状外。若让它参与 `max`，角落会被
    /// 判成内部、覆盖率 1 ⇒ **圆角被填成直角**（2026-10-28 用户报障）。
    ///
    /// 四个角都要扫：只守左上角的话，另外三个角会静默复发。
    #[test]
    fn corner_cut_by_rounding_is_transparent() {
        let half = [300.0_f32, 29.0];
        let r = 12.0;
        // 四角的符号组合；每角扫 40×40 个 0.5px 步进的点（20×20 像素）。
        let mut lying_checked = 0;
        for (sx, sy) in [(1.0, 1.0), (-1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)] {
            for ix in 0..40 {
                for iy in 0..40 {
                    let p = [
                        sx * (half[0] - ix as f32 * 0.5),
                        sy * (half[1] - iy as f32 * 0.5),
                    ];
                    let sdf = glass_sdf(p, half, r);
                    if sdf <= EDGE_AA_HALF_WIDTH {
                        continue; // 形状内或过渡带内，不属本条守的范围
                    }
                    let cov = edge_coverage(edge_depth(p, half, sdf));
                    assert!(
                        cov == 0.0,
                        "圆角切掉的角落必须完全透明：p={p:?} sdf={sdf} \
                         却得到覆盖率 {cov}"
                    );
                    // 只统计「撒谎点」：到直边距离为正（点在两条直边内侧）
                    // 却已被弧切到外面——这才是 `max` 会误判的那一类。
                    let flat = (half[0] - p[0].abs()).min(half[1] - p[1].abs());
                    if flat > 0.0 {
                        lying_checked += 1;
                    }
                }
            }
        }
        assert!(
            lying_checked > 100,
            "应扫到足够多会让直边距离撒谎的点（否则这条测试守不到东西），\
             实际只有 {lying_checked}"
        );
    }

    /// 混色的**端点语义**必须与旧的替换式一致。
    ///
    /// `a = 0`（没有玻璃）→ 原样返回底图；`a = 1`（实心卡片）→ 精确返回
    /// 卡片色。否则「卡片不透明度」滑块的语义会变：用户从旧的半透明
    /// 配置升级上来，卡片观感会跳变。
    #[test]
    fn glass_tint_keeps_endpoints() {
        let tints = [
            ("深色", [47.0 / 255.0, 53.0 / 255.0, 64.0 / 255.0]),
            ("浅色", [1.0, 1.0, 1.0]),
        ];
        for (name, tint) in tints {
            let src = [0.7, 0.5, 0.3];
            let none = glass_tint(src, tint, 0.0);
            for (i, (got, want)) in none.iter().zip(src).enumerate() {
                assert!(
                    (got - want).abs() < 1e-6,
                    "{name} a=0 应原样返回底图：通道{i} 得到 {got} 期望 {want}"
                );
            }
            let solid = glass_tint(src, tint, 1.0);
            for (i, (got, want)) in solid.iter().zip(tint).enumerate() {
                assert!(
                    (got - want).abs() < 1e-6,
                    "{name} a=1 应精确返回卡片色：通道{i} 得到 {got} 期望 {want}"
                );
            }
        }
    }

    /// 深色 tint 必须**保留底图的明暗结构**，而不是把它压平。
    ///
    /// 替换式 `mix` 会把亮底与暗底一起拉向卡片色 ⇒ 对比被压掉。乘法吸收
    /// 按同一比例缩放两个亮度 ⇒ 对比大体保留。这条守的是「深色主题下
    /// 卡片不再糊成一块灰板」。
    #[test]
    fn dark_tint_keeps_backdrop_structure() {
        let tint = [47.0 / 255.0, 53.0 / 255.0, 64.0 / 255.0];
        let a = 0.45;
        let lerp = |x: f32, y: f32| x + (y - x) * a;
        let hi = glass_tint([0.9, 0.9, 0.9], tint, a)[1];
        let lo = glass_tint([0.2, 0.2, 0.2], tint, a)[1];
        let old_hi = lerp(0.9, tint[1]);
        let old_lo = lerp(0.2, tint[1]);
        assert!(
            hi - lo > old_hi - old_lo,
            "混色后底图对比必须高于替换式：新 {:.3} 旧 {:.3}",
            hi - lo,
            old_hi - old_lo
        );
    }

    /// 深色 tint 必须把**冷调色偏**带上玻璃（混色的可见度来源）。
    ///
    /// `card_bg = #2F3540` 是冷蓝灰（R < G < B）。替换式把这点色偏几乎
    /// 抹平（0.03），两段式放大到约 0.12——玻璃「泛冷」而不是单纯变暗。
    #[test]
    fn dark_tint_carries_cool_cast() {
        let tint = [47.0 / 255.0, 53.0 / 255.0, 64.0 / 255.0];
        let a = 0.45;
        let out = glass_tint([0.9, 0.9, 0.9], tint, a);
        let cast = out[2] - out[0];
        let old_cast = {
            let lerp = |x: f32, y: f32| x + (y - x) * a;
            lerp(0.9, tint[2]) - lerp(0.9, tint[0])
        };
        assert!(
            cast > old_cast * 2.0,
            "冷调色偏应被明显放大：新 {cast:.3} 旧 {old_cast:.3}"
        );
        assert!(cast > 0.0, "深色卡片色是冷调，B 必须高于 R，实际 {cast}");
    }

    /// 浅色 tint（纯白卡片色）**不得把暗底洗白**。
    ///
    /// 浅色主题的 `card_bg = #FFFFFF`，替换式 `mix` 会把暗底一路顶到
    /// `0.56`（中灰）——那就是「浅色主题下玻璃发灰」的病根。散射是限幅
    /// 的，暗底应保持暗。
    #[test]
    fn light_tint_does_not_wash_out_dark_backdrop() {
        let white = [1.0, 1.0, 1.0];
        let a = 0.45;
        let out = glass_tint([0.2, 0.2, 0.2], white, a)[1];
        let old = 0.2 + (1.0 - 0.2) * a;
        assert!(
            out < old - 0.1,
            "暗底不应被洗白：新 {out:.3} 旧(替换) {old:.3}"
        );
        // 亮底那端也不能被推过 1.0（否则整片饱和，彩边与折射全被冲掉）。
        let bright = glass_tint([0.9, 0.9, 0.9], white, a)[1];
        assert!(bright <= 1.0, "亮底不得溢出：{bright}");
    }

    /// 浅色 tint 是**中性色**，不得凭空给底图染色。
    #[test]
    fn neutral_tint_does_not_shift_hue() {
        let gray_tint = [0.9, 0.9, 0.9];
        let out = glass_tint([0.7, 0.5, 0.3], gray_tint, 0.45);
        // 底图的 R > G > B 顺序必须保持，且中性 tint 不引入新的色偏。
        assert!(out[0] > out[1] && out[1] > out[2], "通道顺序应保持：{out:?}");
        let in_cast = 0.7f32 - 0.3;
        assert!(
            (out[0] - out[2]) > in_cast * 0.5,
            "中性 tint 不应大幅改变底图色偏：{out:?}"
        );
    }

    /// 深色玻璃**不能变成黑玻璃**：透过率有地板，身后内容始终可见。
    ///
    /// 极端情形：卡片色取纯黑。若没有地板，`transmit → 0` ⇒ 底图与高光
    /// 一起被吃掉，卡片变成一块死黑（2026-10-25「整卡平板化」的观感）。
    #[test]
    fn dark_tint_never_becomes_black_glass() {
        let black = [0.0, 0.0, 0.0];
        let out = glass_tint([0.8, 0.8, 0.8], black, 0.45);
        assert!(
            out[0] > 0.1,
            "纯黑卡片色下底图仍须可见（透过率地板），实际 {out:?}"
        );
    }

    /// 边缘覆盖率必须在 **1px 内**完成 1 → 0，且以边缘为 0.5 对称。
    #[test]
    fn rounded_edge_fades_within_one_pixel() {
        let half = [300.0_f32, 60.0];
        let r = 12.0;
        let cov_at = |dx: f32| {
            // 右直边：x 方向偏移 dx（正 = 外侧）。
            let p = [half[0] + dx, 0.0];
            edge_coverage(edge_depth(p, half, glass_sdf(p, half, r)))
        };
        assert!((cov_at(-2.0) - 1.0).abs() < 1e-6, "内侧 2px 应满覆盖");
        assert!((cov_at(0.0) - 0.5).abs() < 1e-6, "边缘处应为 0.5");
        assert!((cov_at(2.0) - 0.0).abs() < 1e-6, "外侧 2px 应完全透明");
        // 单调递减，且中间不跳变（线性）。
        let mut prev = 2.0;
        for i in -8..=8 {
            let c = cov_at(i as f32 * 0.125);
            assert!(c <= prev + 1e-6, "覆盖率应随外移单调不增");
            prev = c;
        }
    }
}
