// ModularClipboard 液态玻璃合成着色器。
//
// # 它取代了什么
//
// 背景磨砂（Acrylic）的最后一步是把模糊结果整屏拷进目标（`blur.wgsl` 的
// `fs_copy`）。玻璃模式把那一趟**升级**：同样读背景，但按卡片形状做
// 折射 + 色散 + Fresnel，再把「清晰（折射后）」与「模糊」按距边距离混合。
//
// # 为什么能就地替换
//
// 两者的输入输出契约完全一致——都是一次全屏三角形绘制、写满整个目标、
// 只有一个 `vec2<f32>` 的 push constant（UV 位移）。所以玻璃管线可以直接
// 复用 [`crate::blur::Blit`] 结构：渲染层的 `prelude` 机制不用改一行。
//
// # 数据流
//
// ```text
// capture_img(清晰桌面) ─┐
//                        ├─► fs_glass ─► 目标（交换链 / 超采样场景图）
// blur_a(Dual Filtering) ─┘         ↑
//                             卡片矩形 uniform（≤16 张）
// ```
//
// 清晰图与模糊图都是**整窗客户区**的底图，因此同一个 uv 对两者都成立；
// 卡片矩形用来决定「这个像素属于哪张卡片、该折多少」。
//
// # 数学与 Rust 侧一一对应
//
// 本文件每个函数都镜像 `crate::glass` 的同名纯函数（那里有守门单测）。
// **改一边必须改另一边**——着色器没法单测，符号写反只表现为「看起来有点
// 怪」，与真实故障（描述符绑错、布局错）症状相同。对应关系：
//
// | WGSL | Rust |
// |---|---|
// | `sdf_rr` | `glass::glass_sdf` |
// | `glass_normal` | `glass::glass_normal` |
// | `refract_uv` | `glass::refract_offset` |
// | `dome_offset` | `glass::dome_refract_offset` |
// | `dist_edge` | `glass::dist_to_edge` |
// | `dome_slope` | `glass::dome_slope` |
// | `fresnel` | `glass::fresnel_edge` |
// | `bevel_weight` | `glass::bevel_weight` |
// | `specular`（内联） | `glass::specular_rim` |
// | `luma` / `adapt_luma` | `glass::luminance` / `glass::luminosity_adapt` |
//
// # 坐标约定
//
// `in.uv` 与 egui 的 y-down 屏幕坐标同向（`v=0` 是 framebuffer 顶行），
// 所以 `pixel = uv * size` 直接就是 egui 的物理像素坐标，卡片矩形可以
// 原样比较。这条约定由 `card_blur_uv` 的用法反推并在此固定。

/// 同时存在的玻璃面板上限。
///
/// 列表行卡片约 10~20 张（随窗口高度与字号变） + 顶栏 + 搜索面板 +
/// 类型栏 + 详情/侧栏卡 ≈ 24。超出部分**静默丢弃**（不报错）：玻璃是
/// 观感层，多两张少两张不该让界面崩——所以 CPU 侧会把**顶栏/搜索/类型栏
/// 排在最前**（它们最显眼），行卡片排在后面。
const MAX_GLASS_CARDS: u32 = 24u;

/// 色散的三个折射率（R 最小、B 最大 ⇒ 蓝光弯得最多，边缘出现彩色描边）。
/// 与 `glass::IOR_R/G/B` 一致。
const IOR_R: f32 = 1.50;
const IOR_G: f32 = 1.52;
const IOR_B: f32 = 1.54;

/// 默认折射率比值 `eta = 1 / ior`（ior = 1.5）。
const DEFAULT_ETA: f32 = 1.0 / 1.5;

/// 2D 斜率场抬起成 3D 表面法线时 xy 分量的放大倍数。与 `glass::SURFACE_TILT` 一致。
const SURFACE_TILT: f32 = 2.0;
/// 单位长 2D 法线抬起后的 `|N.xy|`（`TILT / sqrt(TILT² + 1)`）。
const EDGE_NXY: f32 = 0.8944272;
/// 参考 eta 在边缘（`|n| = 1`）处的折射因子
/// `sqrt(1 - eta²(1 - N.z²)) - eta·N.z`，用来把 `refract_uv` 归一到
/// 「`power` = 边缘最大位移」。
const EDGE_FACTOR: f32 = 0.5046306;

/// 穹顶折射相对边缘轮廓折射的强度比。
///
/// Rust 侧的测试用 `dome power = 0.03`、`edge power = 0.35`，比值 0.086。
/// 取 0.08：穹顶只负责「中心也有折射」，主角是边缘轮廓。
const DOME_RATIO: f32 = 0.08;

/// 磨砂/清晰混合的距离尺度（像素）：**离边超过**这么多像素即算「内部」。
///
/// 液态玻璃的辨识度全在这个分布上：
/// **边缘清晰**（能看见折射后的桌面，配合最强的折射与 Fresnel 亮边），
/// **中心磨砂**（`de ≥ FROST_SCALE` 时 `FROST_MAX` 磨砂 + 其余清晰）。
///
/// CAUTION: 方向与早期注释相反——`frost` 随**距边距离**单调**递增**
/// （`de = 0` 在边缘），所以是「越往里越糊」。整片清晰就只是「透明卡片」，
/// 整片磨砂就退回 Acrylic，两者都不是玻璃。
///
/// 2026-10-27：36 → 56。行卡片只有 56~70pt 高，36 的尺度让整张卡几乎
/// 都落在「内部」档，玻璃读成一块半透明色块；放大后边缘清晰带占卡片
/// 高度的三分之一左右，才看得出透镜感。
const FROST_SCALE: f32 = 56.0;

/// 内部（远离边缘）保留的磨砂比例上限。
///
/// 0 = 中心完全清晰（纯透镜），1 = 中心完全磨砂（退回 Acrylic）。
/// 取 0.5：HIG 把「高度透明」的 clear 变体留给媒体背景，regular 变体的
/// 可读性由亮度自适应 + 卡片色承担；0.85 那档在深色壁纸上会把玻璃读成
/// 一块发灰的色块（2026-10-27「不够通透」）。
const FROST_MAX: f32 = 0.5;

/// 色散生效的**距边带宽**（像素）。
///
/// 三个通道的位移差与到边缘的距离成正比：卡片内部它们几乎重合，
/// 分开采样得到的结果与单次采样差不到一个纹素，却要多付两次纹理
/// 读取。模糊/玻璃类 pass 是**带宽瓶颈**，能省的每次采样都值。
///
/// 24px：视觉上彩边只出现在边缘一两毫米内，再往里看不出来。
const DISPERSION_BAND: f32 = 24.0;

/// Fresnel 亮边的 RGB 增益（冷白色，略偏蓝）。宽而各向同性，是「环境反射」
/// 那一层；方向性的镜面高光另算（见 [`SPEC_TINT`]）。
const RIM_TINT: vec3<f32> = vec3<f32>(0.20, 0.23, 0.28);

/// 镜面高光的颜色（冷白）与强度。
///
/// Apple：「一圈**镜面高光取代描边**，朝向光源的一侧最亮，背光一侧淡下去」。
/// 这一层是**方向性**的（Blinn-Phong 的 `dot(N, L)`），与上面各向同性的
/// Fresnel 相加：前者是光源的锐利反光，后者是环境的宽反射。
const SPEC_TINT: vec3<f32> = vec3<f32>(0.85, 0.92, 1.0);
const SPEC_GAIN: f32 = 0.45;
/// 光源方向（屏幕空间，y 向下 ⇒ 左上打光）。必须是**单位**向量。
const LIGHT_DIR: vec2<f32> = vec2<f32>(-0.6, -0.8);
/// `glass_edge` 滑块到 Blinn-Phong 锐度的换算（滑块 0.5~12 ⇒ 锐度 4~96）。
const SPEC_SHININESS: f32 = 8.0;

/// 模糊底图跟随折射的比例（0 = 不跟，1 = 与清晰图同幅）。
///
/// Apple 的实现是把**位移图**作用在模糊之后的背景上（先把界面缩小、
/// 再把边缘向外拉伸），各家移植 likewise 都是 `sample(blurredScene, uv + d)`。
/// 只让清晰图位移、模糊图原地不动 ⇒ 中心（那里 `frost` 高、几乎全用模糊
/// 图）完全没有透镜感，玻璃会退化成「糊底 + 一圈会动的贴图」。
///
/// 取 0.55 而不是 1.0：模糊图已经丢掉高频，位移太大只会让画面整体漂移。
const BLUR_REFRACT: f32 = 0.55;

/// 亮底压暗的**上限**（相对亮度 1.0 处压暗多少）。
///
/// HIG Materials 对 clear 变体的规定：背景明亮时叠一层 **35% 不透明度
/// 的暗色调光层**。玻璃变透之后，这是可读性唯一的保险。
const LUMA_DIM_MAX: f32 = 0.35;
/// 暗底提亮的**上限**（相对亮度 0.0 处提亮多少）。
///
/// 对应 Apple 玻璃的 exposure boost：暗底上玻璃要「透光」而不是发灰。
/// 取 0.20 而不是与压暗对称的 0.35：提亮过多会把纯黑壁纸洗成中灰。
const LUMA_LIFT_MAX: f32 = 0.20;
/// 亮度自适应的目标中灰。
const LUMA_TARGET: f32 = 0.5;

struct GlassUniforms {
    /// 卡片矩形，`xy = 中心`、`zw = 半尺寸`，单位是**目标像素**。
    ///
    /// `vec4<f32>` 数组：WGSL uniform 空间要求数组元素步长为 16 的倍数，
    /// `vec4` 正好满足（用 `vec2` 会因 8 字节步长被 naga 判非法布局）。
    rects: array<vec4<f32>, MAX_GLASS_CARDS>,
    /// 目标图的像素尺寸 `(w, h)`。
    size: vec2<f32>,
    /// 折射强度（0 = 不折射，只剩磨砂）。
    refract: f32,
    /// 色散宽度（0 = 关掉彩色描边，1 = 全强度）。
    dispersion: f32,
    /// Fresnel 幂次（越小光晕越铺开，越大越集中在边缘一两像素）。
    edge: f32,
    /// 卡片圆角半径（像素）。
    radius: f32,
    /// 有效卡片数（`rects` 前多少项有意义）。
    count: u32,
    /// 玻璃层的**覆盖度**（0 = 完全露出真实桌面，1 = 玻璃层不透明）。
    ///
    /// 与 `tint_strength` 分工：这个决定「透过多少真实桌面」，那个决定
    /// 「看起来像不像一张有色卡片」。两者独立——覆盖度 1 + tint 0 就是
    /// 一块完全不透明、但完全通透的纯折射玻璃。
    ///
    /// CAUTION: 它参与的是 `rgb * a` 的**预乘**，不是只写进 alpha 通道。
    /// 交换链按 PRE_MULTIPLIED 合成，只改 alpha 不改 RGB 会让半透明
    /// 卡片被额外叠一层桌面（见 `fs_glass` 末尾的 CAUTION）。
    card_alpha: f32,
    /// 卡片底色（sRGB 分量，0~1，**未预乘**）。
    ///
    /// 玻璃层自己把「底图 + 卡片色」合成完。早前这层色是 egui 在玻璃
    /// pass **之后**再画的一个矩形：两层 alpha 叠加成 `a + a(1-a)`，
    /// 卡片色不透明时玻璃整层被盖死（折射/色散全看不见），半透明时
    /// 又多叠一份桌面（发白）。
    tint: vec3<f32>,
    /// 卡片色的混合强度（0 = 纯玻璃，1 = 完全是卡片色）。
    tint_strength: f32,
};

// 合成时的 UV 位移（push constant，8 字节）。
//
// 与 `blur.wgsl` 的 `CopyOffset` **逐字节一致**：玻璃管线复用同一个
// `Blit` 结构，push constant 的 layout 也必须一致，否则
// `cmd_push_constants` 推入的字节会被着色器按另一种结构解读。
// 语义见 `blur.wgsl` 模块头「合成时的 UV 位移」——磨砂移动预测。
struct CopyOffset {
    offset: vec2<f32>,
};

var<immediate> copy_pc: CopyOffset;

@group(0) @binding(0) var<uniform> u: GlassUniforms;
@group(0) @binding(1) var glass_sampler: sampler;
@group(0) @binding(2) var sharp_tex: texture_2d<f32>;
@group(0) @binding(3) var blur_tex: texture_2d<f32>;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_fullscreen(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // 全屏三角形：与 blur.wgsl 同一套（理由见那里的模块头）。
    var pts = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let xy = pts[vertex_index];

    var out: VertexOutput;
    out.clip_position = vec4<f32>(xy, 0.0, 1.0);
    // NDC → UV，y 轴翻转：Vulkan 的 NDC y 向下，而这里要的是
    // **与 egui y-down 同向**的 uv（v=0 = 顶行）。写反会让卡片矩形
    // 整体上下镜像，而画面看起来「只是有点怪」——很难察觉。
    out.uv = vec2<f32>(xy.x * 0.5 + 0.5, 0.5 - xy.y * 0.5);
    return out;
}

// ---------------------------------------------------------------------------
// 以下逐个镜像 crate::glass 的纯函数
// ---------------------------------------------------------------------------

/// 圆角矩形 SDF（内部负、外部正、边缘 ≈ 0）。
fn sdf_rr(p: vec2<f32>, half: vec2<f32>, r: f32) -> f32 {
    let rr = min(r, min(half.x, half.y));
    let q = max(abs(p) - half + vec2<f32>(rr, rr), vec2<f32>(0.0, 0.0));
    return length(q) - rr;
}

/// SDF **与**解析梯度一次算出。
///
/// # 为什么不是 `sdf_rr` + `glass_normal`
///
/// 中心差分的 `glass_normal` 要 4 次 SDF，每次一个 `sqrt`；而 `q` 本身
/// 就是未归一化的梯度，`q/|q|` 即外法线。一次调用拿两个结果，省 3 次
/// `sqrt` 加一堆 `max`/`abs`。
///
/// 玻璃 pass 是**每像素**跑这条链的，2026-10-24 实测滚动列表卡顿，
/// 剖析后确认这是单像素成本里最大的一项。
///
/// # CAUTION: 象限符号
///
/// `q` 用 `abs(p)` 算，负半轴被折到了一起，梯度必须乘回 `sign(p)`。
/// 少了这一步，负半轴的法线整体反向——折射朝反方向推内容，观感是
/// 「玻璃在吸气而不是推气」，且**不报任何错**。
/// Rust 侧的 `glass::glass_normal_analytic` 有守门测试盯着这条。
struct SdfN {
    d: f32,
    n: vec2<f32>,
};

fn sdf_rr_n(p: vec2<f32>, half: vec2<f32>, r: f32) -> SdfN {
    let rr = min(r, min(half.x, half.y));
    let q = max(abs(p) - half + vec2<f32>(rr, rr), vec2<f32>(0.0, 0.0));
    let len = length(q);
    var out: SdfN;
    out.d = len - rr;
    if (len < 1e-6) {
        // 平坦区：梯度 ≈ 0 ⇒ 归一化会放大噪声。零向量 = 无折射。
        out.n = vec2<f32>(0.0, 0.0);
    } else {
        out.n = vec2<f32>(sign(p.x) * q.x, sign(p.y) * q.y) / len;
    }
    return out;
}

/// 点到边缘的**真实距离**（不是 SDF）。
///
/// CAUTION: 圆角矩形 SDF 在内部恒为 `-radius`（平坦区），拿它当距离会让
/// 整片内部的衰减强度一模一样。必须换成这个函数（见 `glass::dist_to_edge`
/// 的 CAUTION）。
fn dist_edge(p: vec2<f32>, half: vec2<f32>, d: f32) -> f32 {
    if (d > 0.0) {
        return d;
    }
    return max(min(half.x - abs(p.x), half.y - abs(p.y)), 0.0);
}

/// 中心差分的 SDF 梯度。
///
/// CAUTION: 主路径已改用 [`sdf_rr_n]  ］的解析梯度（一次调用拿 d 与法线，
/// 省 3 次 `sqrt`）。这个函数**故意保留**——`glass::glass_normal` 是 Rust
/// 侧的黄金模型，`analytic_normal_matches_central_difference` 拿它验证
/// 解析版；shader 与 Rust 共用一份中心差分实现，两侧的对应关系不断。
///
/// NOTE: WGSL 没有 `#[allow(dead_code)]`，未被调用的函数就是不被编译进
/// SPIR-V（naga 会做死代码消除），所以留着它零成本。
fn glass_normal(p: vec2<f32>, half: vec2<f32>, r: f32, eps: f32) -> vec2<f32> {
    let dx = sdf_rr(p + vec2<f32>(eps, 0.0), half, r) - sdf_rr(p - vec2<f32>(eps, 0.0), half, r);
    let dy = sdf_rr(p + vec2<f32>(0.0, eps), half, r) - sdf_rr(p - vec2<f32>(0.0, eps), half, r);
    let g = vec2<f32>(dx, dy);
    let l = length(g);
    if (l < 1e-6) {
        // 平坦区：梯度 ≈ 0，归一化会放大噪声 ⇒ 零向量（调用方按无折射处理）。
        return vec2<f32>(0.0, 0.0);
    }
    return g / l;
}

/// `refract(I, N, eta)` 的 2D 投影，返回 UV 位移。
///
/// 与 `glass::refract_offset` 逐行对应。**入射是正对玻璃的视线**
/// `I = (0, 0, -1)`，法线是 2D 斜率场抬起成的 3D 法线
/// `N = normalize(vec3(n * SURFACE_TILT, 1))`。
///
/// # CAUTION: 入射**不能**取 `-n`
///
/// 那是个退化模型：`dot(N, I) = -|N|²`，对单位法线恒为 -1 ⇒
/// `k = 1 - eta²(1 - 1) = 1` ⇒ `s = 1` ⇒ 整个式子塌成 `-n * power`，
/// **eta 被完全约掉**。后果（三个都不报错）：
///
/// 1. `IOR_R/G/B` 算出逐位相同的位移 ⇒ 色散恒为 0，滑块是死的；
/// 2. 边缘那两次分通道采样坐标完全相同，纯浪费带宽；
/// 3. `eta = 1`（ior = 1，等于没有玻璃）时位移仍是 `-n·power` 而非 0。
///
/// 抬起成 3D 后 `T.xy = -(s - eta·N.z)·N.xy`，eta 安稳留在式子里，
/// ior 越大弯得越多 ⇒ 蓝光在边缘外侧，色散成立。
///
/// # 归一化
///
/// 上式再乘 `(f / EDGE_FACTOR) / EDGE_NXY`（`f = s - eta·N.z`）归一：
/// 于是 `|n| = 1` 且 `eta = DEFAULT_ETA` 时位移模长恰好等于 `power`，
/// 与旧模型的幅值逐点对齐（实测 `|n| = 0.5` 处旧 0.632·power、
/// 新 0.643·power）。这次改的是「eta 有没有用」，不是「折射多强」。
fn refract_uv(n: vec2<f32>, eta: f32, power: f32) -> vec2<f32> {
    // 抬起：|n| 已知量级（0~1），`inv = 1/sqrt(t² + 1)` 一次给出
    // `N.xy`（乘 TILT·inv）与 `N.z`（= inv），省一次 normalize。
    let t = length(n) * SURFACE_TILT;
    let inv = 1.0 / sqrt(t * t + 1.0);
    let nxy = n * (SURFACE_TILT * inv);
    let nz = inv;
    // d = dot(N, I) = -N.z（I = (0,0,-1)）
    let k = max(1.0 - eta * eta * (1.0 - nz * nz), 0.0);
    let s = sqrt(k);
    let f = s - eta * nz;
    let scale = (f / EDGE_FACTOR) / EDGE_NXY;
    return -nxy * scale * power;
}

/// 穹顶（半球透镜）法线的斜率场：`normalize(local) * min(|local|, 1)`。
///
/// 与 `glass::dome_slope` 一致。折射与 Fresnel **共用**这一个场——分开算
/// 会出现「折射很明显但边缘不亮」的割裂感。
fn dome_slope(local: vec2<f32>) -> vec2<f32> {
    let len = length(local);
    if (len < 1e-6) {
        return vec2<f32>(0.0, 0.0);
    }
    let curvature = min(len, 1.0);
    return local / len * curvature;
}

/// 穹顶折射的 UV 位移（bergice 的第一级折射）。
///
/// 为什么需要它：`glass_normal` 在卡片内部平坦区返回零向量，只靠边缘
/// 轮廓折射的话**中心一大片完全没有折射**——玻璃像「只有一圈会动的
/// 贴图」。穹顶让中心也有（弱的）折射，越靠边缘越强。
fn dome_offset(local: vec2<f32>, eta: f32, power: f32) -> vec2<f32> {
    return refract_uv(dome_slope(local), eta, power);
}

/// 斜角（bevel）带的折射权重：轮廓处 1、带内端 0。与 `glass::bevel_weight`
/// 一致。
///
/// # 为什么不是 `exp(-de · 0.4)`
///
/// 指数衰减的尺度与形状无关，且带内端还剩 1.8% 时被 SDF 的零法线**突然
/// 截断** ⇒ 平滑表面上留一条隐约的棱。Apple 的模型是「带斜边的板」：
/// 斜边宽度 = 圆角半径，剖面在轮廓处最陡、在带内端水平。位移 ∝ 剖面斜率
/// ⇒ 取 `(1 - x)²`（`x = de / radius`）：轮廓处 1、内端 0 且斜率为 0，
/// 与平坦区严丝合缝，带宽还自动跟着圆角走。
fn bevel_weight(de: f32, radius: f32) -> f32 {
    let x = clamp(de / max(radius, 1.0), 0.0, 1.0);
    let t = 1.0 - x;
    return t * t;
}

/// 相对亮度（Rec.709）。
fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

/// 亮度自适应：**非对称**地压缩底图亮度（三通道同比例 ⇒ 只动亮度、
/// 不动色相）。与 `glass::luminosity_adapt` 一致，理由见那里。
///
/// 亮底线性压暗到 `1 - dim_max`、暗底线性提亮到 `1 + lift_max`。
/// 对称式的 `1 + gain·(target − luma)` 对「白底 + 深色文字」压得不够，
/// 而玻璃变透之后可读性全靠这一层。
fn adapt_luma(c: vec3<f32>, dim_max: f32, lift_max: f32) -> vec3<f32> {
    let l = luma(c);
    var k = 1.0;
    if (l > LUMA_TARGET) {
        k = 1.0 - dim_max * (l - LUMA_TARGET) / max(1.0 - LUMA_TARGET, 1e-3);
    } else {
        k = 1.0 + lift_max * (LUMA_TARGET - l) / max(LUMA_TARGET, 1e-3);
    }
    return c * max(k, 0.0);
}

@fragment
fn fs_glass(in: VertexOutput) -> @location(0) vec4<f32> {
    // 磨砂底图：不在任何卡片内的像素原样输出（与 Acrylic 逐字节一致）。
    // UV 位移与 Acrylic 共用一套（磨砂移动预测，见 CopyOffset 的注释）。
    let blurred = textureSampleLevel(blur_tex, glass_sampler, in.uv + copy_pc.offset, 0.0);
    var out = blurred;

    // uv → 物理像素（egui y-down 同向，见模块头「坐标约定」）。
    let pixel = vec2<f32>(in.uv.x * u.size.x, in.uv.y * u.size.y);

    for (var i: u32 = 0u; i < u.count; i = i + 1u) {
        let rc = u.rects[i];
        let center = rc.xy;
        let half = rc.zw;
        let rel = pixel - center;

        // ---- AABB 粗判（先于 SDF）----
        //
        // 卡片像素占多数，但**卡片之间的空隙**、窗口里没有卡片的区域
        // 仍要走完整个循环。粗判只要 4 次比较，把那些像素的成本从
        // 「一次 SDF（含 sqrt）」降一个量级。
        let outside = abs(rel) - half;
        if (outside.x > 0.0 || outside.y > 0.0) {
            continue;
        }

        // ---- SDF 与解析法线一次算出 ----
        //
        // 列表卡片互不重叠，第一张命中的就是最终结果；顶栏/搜索面板/
        // 类型栏同样不重叠。命中的卡片处理完即结束。
        let sn = sdf_rr_n(rel, half, u.radius);
        if (sn.d >= 0.0) {
            continue;   // 落在圆角切掉的角上
        }
        let p = rel;
        let d = sn.d;
        let n = sn.n;

        // ---- 距边距离 / 斜角权重 ----
        let de = dist_edge(p, half, d);
        // 斜角带：只在距边 `radius` 像素以内折，带外严格为 0。
        //
        // CAUTION: `u.radius` 目前取**第一张**面板的圆角（`GlassParams::uniform`
        // 的注释有说明）。斜角带宽跟着它走，所以同一帧里的面板应当共用
        // 同一档圆角——本项目实际如此（都走 `paint` 的圆角常量），差个
        // 一两像素只表现为亮带宽一点，不会出错。
        let w = bevel_weight(de, u.radius);
        let local = p / half;               // 归一化到 [-1,1]
        let slope = dome_slope(local);

        // ---- 两级折射：穹顶（中心）+ 边缘轮廓（贴边）----
        //
        // 穹顶**只算一次**（用默认 IOR）：三个 IOR 对穹顶位移的影响不到
        // 一个纹素的 1%，分通道算纯属浪费（每次 refract 含一个 sqrt）。
        // 色散的差异只体现在边缘轮廓折射上——那里位移大、差异才看得见。
        let dome_c = dome_offset(local, DEFAULT_ETA, u.refract * DOME_RATIO);
        // 参考位移（`dispersion = 0` 时三通道共用它）。
        let dref = refract_uv(n, DEFAULT_ETA, u.refract) * w + dome_c;
        let dg = refract_uv(n, 1.0 / IOR_G, u.refract) * w + dome_c;
        // 色散只在**靠近边缘**时启用（见 `DISPERSION_BAND`）：内部三通道
        // 位移几乎重合，分开采样纯浪费带宽。
        let eff_disp = select(0.0, u.dispersion, de < DISPERSION_BAND);

        var sharp_rgb: vec3<f32>;
        if (eff_disp > 0.001) {
            let dr = refract_uv(n, 1.0 / IOR_R, u.refract) * w + dome_c;
            let db = refract_uv(n, 1.0 / IOR_B, u.refract) * w + dome_c;
            let off_r = mix(dref, dr, eff_disp);
            let off_g = mix(dref, dg, eff_disp);
            let off_b = mix(dref, db, eff_disp);
            // 三次采样重合成一帧。清晰图采样同样带 UV 位移（与磨砂同步）。
            let sr = textureSampleLevel(sharp_tex, glass_sampler, in.uv + copy_pc.offset + off_r, 0.0);
            let sg = textureSampleLevel(sharp_tex, glass_sampler, in.uv + copy_pc.offset + off_g, 0.0);
            let sb = textureSampleLevel(sharp_tex, glass_sampler, in.uv + copy_pc.offset + off_b, 0.0);
            sharp_rgb = vec3<f32>(sr.r, sg.g, sb.b);
        } else {
            // 单次采样：位移取绿通道（三通道的中间值）。
            let sg = textureSampleLevel(sharp_tex, glass_sampler, in.uv + copy_pc.offset + mix(dref, dg, eff_disp), 0.0);
            sharp_rgb = sg.rgb;
        }

        // ---- 磨砂 / 清晰混合：边缘清晰、中心磨砂 ----
        //
        // `de` 是**距边距离**：贴边处 `frost → 0` 取折射后的清晰图，
        // 深入内部 `frost → FROST_MAX` 才是磨砂。与斜角权重 `w` 正好
        // 反向——贴边处折射最强且最清晰（那是液态玻璃的辨识特征），
        // 内部折射归零、退回磨砂。
        //
        // CAUTION: 上限取 `FROST_MAX`（0.5）而不是 1.0：整片纯磨砂就退回
        // Acrylic。2026-10-27 之前是 0.85，只留 15% 清晰分量，在深色壁纸上
        // 玻璃读成一块发灰色块——「通透」这件事一半靠这个上限，另一半靠
        // 上面的 `FROST_SCALE`。
        //
        // 磨砂那一路**也**按折射位移采样（见 `BLUR_REFRACT`）：位移图在
        // Apple 的实现里作用在模糊之后的背景上，只让清晰图位移会让占
        // 面积大多数的中心完全没有透镜感。
        let bref = mix(dref, dg, eff_disp) * BLUR_REFRACT;
        let blur_rgb = textureSampleLevel(blur_tex, glass_sampler, in.uv + copy_pc.offset + bref, 0.0).rgb;
        let frost = clamp(de / FROST_SCALE, 0.0, 1.0) * FROST_MAX;
        var rgb = mix(sharp_rgb, blur_rgb, frost);

        // ---- 亮度自适应（Apple regular 变体）----
        //
        // 亮底压暗、暗底提亮：卡片上的前景文字对比度不随壁纸/桌面内容
        // 漂移。放在混合**之后**、高光**之前**——高光是光源的反光，
        // 不该被底图的亮度修正吃掉。
        rgb = adapt_luma(rgb, LUMA_DIM_MAX, LUMA_LIFT_MAX);

        // ---- 卡片底色（tint）：玻璃层自己合成 ----
        //
        // 位置在亮度自适应**之后**、高光**之前**：
        // - 在自适应之后：tint 是「玻璃本身的颜色」，不该被底图亮度修正
        //   拽着走（亮壁纸下卡片不该被一起提亮成灰白）；
        // - 在高光之前：高光是光源的反光，叠在 tinted 玻璃之上。
        //
        // 早前这一层由 egui 在玻璃 pass 之后再画一个矩形——那是**两次
        // 覆盖**：卡片色不透明时玻璃被整个盖死，半透明时又多叠一份桌面。
        rgb = mix(rgb, u.tint, clamp(u.tint_strength, 0.0, 1.0));

        // ---- 轮廓：Fresnel（宽、各向同性）+ 镜面高光（锐、方向性）----
        //
        // Fresnel 用**斜率场**而不是 SDF 梯度：后者在内部恒 0、贴边为 1，
        // 用 `1-|n|` 形式的公式会让整片内部最亮（物理映射反了）。
        // 见 `glass::fresnel_edge` 的 CAUTION。这一层是「环境反射」，
        // 四边都有一点。
        let f = pow(clamp(length(slope), 0.0, 1.0), max(u.edge, 0.0));
        rgb = rgb + RIM_TINT * f;
        //
        // 镜面高光才是 Apple 那句「**用一圈高光取代描边**」：朝向光源的
        // 一侧最亮、背光一侧为 0。关键在两点——
        //   ① 用斜角带的**朝外法线** `n` 与光源方向点积（方向性）；
        //   ② 再乘斜角权重 `w`：它只在轮廓带上成立，平坦区 `n = 0`
        //      本来也不会有高光，乘 `w` 是为了让它在带内端平滑收尾。
        // 少任何一点都会退回「四边一样亮的描边」。
        let spec = pow(max(dot(n, LIGHT_DIR), 0.0), max(u.edge, 0.5) * SPEC_SHININESS);
        rgb = rgb + SPEC_TINT * (spec * SPEC_GAIN * w);

        // ---- 输出：必须**预乘** ----
        //
        // 交换链是按 `PRE_MULTIPLIED` 合成的（`Swapchain::new_for_surface`
        // 里的 `composite_alpha` 选择）：DWM 算的是
        // `src.rgb + (1 - src.a) * 桌面`。所以 RGB 必须已经乘过 alpha。
        //
        // CAUTION: 早前写成 `vec4(rgb, blurred.a * u.card_alpha)`——RGB
        // 是**未预乘**的折射结果。于是 `card_opacity < 1` 时合成出来是
        // `玻璃 + (1-a)·桌面`，比玻璃和桌面**都亮**（a=0.5 时凭空多出
        // 0.25 份桌面），表现为半透明卡片发白、发灰，且调低不透明度
        // 反而更亮——与「越透明越淡」的直觉相反，且**不报任何错**。
        //
        // `blurred.a` 恒为 1（GDI 捕获已强制 alpha=255，卷积保持），
        // 它在这里只是「底图自身的覆盖度」，与卡片透明度相乘即为
        // 玻璃层最终覆盖度。
        let a = clamp(u.card_alpha, 0.0, 1.0) * blurred.a;
        out = vec4<f32>(rgb * a, a);
        break;
    }

    return out;
}
