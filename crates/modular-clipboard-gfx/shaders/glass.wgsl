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
// | `frost_scale` / `frost_mix` | `glass::frost_scale` / `glass::frost_mix` |
// | `dispersion_scale` | `glass::dispersion_scale` |
// | `refract_f` / `raise_normal` | `glass::refract_f` / `glass::raise_normal` |
// | `bevel_lambert` | `glass::bevel_lambert` |
// | `thin_line` | `glass::thin_line` |
// | `content_clear` | `glass::content_clear` |
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

// ---- 位移量纲（像素，不是 UV）------------------------------------------
//
// CAUTION: `u.refract` / `u.dispersion` / `u.content_inset` 都是**物理像素**，
// 由 `GlassParams::uniform` 换算好再送进来；着色器只按 `u.size` 逐轴折成 UV。
//
// 早前 `u.refract` 直接当 UV 用：`0.35` 在 878px 宽的窗口上是 307px 的
// 位移，卡片边缘把**三百像素外**的桌面内容拉进来，与身后真实背景完全对
// 不上——观感就是「玻璃里糊着一张不相干的文字」（2026-10-29「玻璃看着有
// 点脏」的根因之二）。真实斜边的位移量级由斜边宽度决定（`radius` 十几
// 像素 ⇒ 位移也是十几像素）。
//
// 像素基准由 Rust 侧的 `glass::{REFRACT_MAX_PX, DISPERSION_PX}` 定义。

/// 2D 斜率场抬起成 3D 表面法线时 xy 分量的放大倍数。与 `glass::SURFACE_TILT` 一致。
const SURFACE_TILT: f32 = 2.0;
/// 单位长 2D 法线抬起后的 `|N.xy|`（`TILT / sqrt(TILT² + 1)`）。
const EDGE_NXY: f32 = 0.8944272;
/// 参考 eta 在边缘（`|n| = 1`）处的折射因子
/// `sqrt(1 - eta²(1 - N.z²)) - eta·N.z`，用来把 `refract_uv` 归一到
/// 「`power` = 边缘最大位移」。
const EDGE_FACTOR: f32 = 0.5046306;

/// 穹顶折射相对边缘轮廓折射的强度比（两者都按像素算，见上）。
///
/// 边缘轮廓折射只在 `radius` 宽的斜边带里成立；穹顶负责「整卡也有轻微
/// 透镜感」，位移沿归一化坐标径向展开。取 0.25：行卡片（半高 29px）在
/// 1.0 滑块下约 10px、默认 0.35 滑块下约 3.5px——够看出「有厚度」，
/// 又不会把内容整体挪走。
const DOME_RATIO: f32 = 0.25;

/// 磨砂尺度的**相对系数**：以卡片短半轴为基准的倍数。与 `glass::FROST_RATIO` 一致。
///
/// 液态玻璃的辨识度全在这个分布上：
/// **边缘清晰**（能看见折射后的桌面，配合最强的折射与 Fresnel 亮边），
/// **中心磨砂**（`de` 接近短半轴时满档 + 其余清晰）。
///
/// CAUTION: 方向与早期注释相反——`frost` 随**距边距离**单调**递增**
/// （`de = 0` 在边缘），所以是「越往里越糊」。整片清晰就只是「透明卡片」，
/// 整片磨砂就退回 Acrylic，两者都不是玻璃。
///
/// CAUTION: 这里**不能**写绝对像素（早前是 56）。磨砂梯度沿「距边距离」
/// 展开，而它的上限是卡片短半轴——一个随窗口高度、字号、DPI 变的量。
/// 用绝对像素定尺度，等于让「清晰带占卡片高度的比例」随设备和字号漂移：
/// 行卡片 58pt 高时短半轴只有 29px（100% DPI），56 的尺度让中心只到
/// `0.5 × 29/56 ≈ 0.26`（近乎全清晰、背景条纹穿透文字），同一张卡在
/// 150% DPI 下又变成 0.39。改成比例后任何尺寸/DPI 都是同一条曲线。
///
/// 取 1.0（梯度铺满整个半高）而不是更小：0.667 那版让清晰带只占卡片
/// 高度三分之一、其余全糊，实测行卡片读成一块糊卡，折射带只剩贴边
/// 十几像素，玻璃的辨识度反而丢了。
const FROST_RATIO: f32 = 1.0;

/// 自适应尺度的下限（像素），与 `glass::FROST_SCALE_MIN` 一致。
/// 类型栏那种 96px 见方的小弹窗不能把尺度压到个位数，否则梯度变硬边。
const FROST_SCALE_MIN: f32 = 8.0;

/// 自适应尺度的上限（像素），与 `glass::FROST_SCALE_MAX` 一致。
/// 大面板按比例会算出比屏幕还大的尺度 ⇒ 中心永远到不了磨砂 ⇒ 大卡最「生」。
const FROST_SCALE_MAX: f32 = 120.0;

/// 内部（远离边缘）保留的磨砂比例上限。
///
/// 0 = 中心完全清晰（纯透镜），1 = 中心完全磨砂（退回 Acrylic）。
/// 取 0.32（2026-10-28 从 0.5 下调，理由同 `glass::FROST_MAX`）：卡片
/// 最深处仍透出 68% 的清晰底图（带折射位移），磨砂只负责压噪点；
/// 0.5 那档让中心一半像素来自 1/4 分辨率的高斯链，玻璃与 Acrylic 的
/// 差别只剩边缘一圈。0.85 那档（2026-10-27）在深色壁纸上会把玻璃读成
/// 一块发灰的色块。
const FROST_MAX: f32 = 0.32;

/// **窗口背景层**（不属于任何卡片的那些像素）的磨砂比例。
///
/// # CAUTION: 初值不能再是模糊底图
///
/// 早前 `out` 的初值直接是 `blurred`（Acrylic 那张模糊结果）。于是玻璃
/// 模式下整窗背景与磨砂模式**逐字节相同**，只有卡片里才有玻璃——用户
/// 看到的就是「背景玻璃没渲染」（2026-10-28 报障）。
///
/// 玻璃的背景该是**清晰**的：一块架在桌面上的透明玻璃，不是一层毛玻璃
/// 滤镜。只留一成柔化：模糊链是 1/4 分辨率的双滤波结果，混多了会在清晰
/// 底图上叠一层低分辨率的「纱」——桌面上的文字周围出现晕圈，整窗发灰
/// （2026-10-29「玻璃看着有点脏」的成因之一）。0.18 → 0.10。
const BG_FROST: f32 = 0.10;

/// 背景层不透明度的**默认档**（实际值由 `u.bg_alpha` 逐帧给出，这个常量
/// 只是文档与守门用的基准）。理由同 `glass::BG_ALPHA_DEFAULT`：恒为 1.0
/// 会把整窗铺成不透明底图，透明效果彻底失效。
const BG_ALPHA_DEFAULT: f32 = 0.35;

/// 磨砂爬升曲线指数（< 1 ⇒ 中段更快饱和）。与 `glass::FROST_GAMMA` 一致。
///
/// 线性曲线下卡片正中间只有四分之一的磨砂量，而那里恰恰是文字最密、
/// 最需要「背景被压平」的位置。
const FROST_GAMMA: f32 = 0.9;

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

// ---- 2.5D 立体斜边 ----------------------------------------------------
//
// 玻璃卡片的「厚度」不是靠描边，而是靠**斜边受光**：一块边缘倒角的板，
// 朝光的一侧把光反到眼睛里（亮棱），背光一侧只剩环境光（暗棱），
// 内缘再有一道细高光（斜边与平面交界处的折线），外缘一道极窄的暗接触
// 线（板与桌面之间的缝）。四层叠起来，二维矩形就有了厚度。
//
// 全部按**物理像素**表达，与 DPI 无关——细线宽度必须与 1px 同量级，
// 用 pt 会让 2× 屏上的「细线」变成 2px 的粗边。

/// 光源的 Z 分量（灯在玻璃前方多高）。与 `LIGHT_DIR`（xy）共同定义
/// 单位光向量：`normalize(vec3(LIGHT_DIR, LIGHT_Z))`。
///
/// 取 0.55 而不是 1.0：太正的光会让「迎光面」与「平面」的差变小，
/// 斜边读不出来；太小则只在极小角度上亮，亮棱会断成几段。
const LIGHT_Z: f32 = 0.55;

/// 受光侧亮棱的增益。乘在 [`SPEC_TINT`]（冷白）上。
///
/// # 为什么不用 [`RIM_TINT`]
///
/// 它是暗冷灰（0.20/0.23/0.28），叠在接近纯白的浅色卡片上几乎看不出
/// 变化——第一版就是这样，2.5D 立体感在浅色主题下完全消失。亮棱是
/// **光的反射**，得用接近白的颜色才有明度差。
const BEVEL_LIT_GAIN: f32 = 1.0;

/// 背光侧暗棱的增益（按亮度**乘性压暗**，不是加黑）。
///
/// 乘性而不是减性：加黑会在亮卡片上糊一块灰，乘 0.6 只是「这一侧没接到
/// 光」，颜色仍属于卡片自己。
const BEVEL_DARK_GAIN: f32 = 0.45;

/// 轮廓亮/暗棱的**半宽**（像素）。
///
/// # 为什么不用「`w` 的幂」收窄（第一版的做法）
///
/// `w = (1 - de/radius)²` 铺满整条斜边带（行卡片上是 18px）。浅色主题的
/// 卡片底已经接近 255，任何正增量都会**直接顶到白**：按 `w²` 收窄后亮棱
/// 仍有 5~6px 的纯白带，读作「一圈发光的边」——正是要避免的廉价感。
///
/// 改成显式宽度门控（3px 内从 1 收到 0）后，亮棱是**一条边**，宽度与
/// 圆角、DPI 都解耦。斜边面的柔和受光由上面那层 Fresnel 承担。
const BEVEL_LIT_WIDTH: f32 = 3.0;

/// 内缘高光线的位置（距边像素）与半宽。
///
/// 1.4px：紧贴轮廓内侧，正好在斜边与平面交界附近。位置取 0 会与外缘
/// 抗锯齿的过渡带打架（那半条带本来就半透明）。
const BEVEL_HAIRLINE_AT: f32 = 1.4;
const BEVEL_HAIRLINE_WIDTH: f32 = 1.1;

/// 内缘高光线的亮度增益（乘在 [`SPEC_TINT`] 上）。细线要「亮而窄」，
/// 加太浓就成了描边。
const BEVEL_HAIRLINE_GAIN: f32 = 0.22;

/// 外缘暗接触线的半宽（像素）与压暗增益。
///
/// 它承担「卡片压在桌面上」的分离感：没有它，浅色桌面上浅色卡片的
/// 轮廓会糊在一起（投影在玻璃模式下被裁到卡片外侧，帮不上这一层）。
const BEVEL_CONTACT_WIDTH: f32 = 1.2;
const BEVEL_CONTACT_GAIN: f32 = 0.22;

/// 「元素不参与玻璃计算」的羽化宽度（像素）。
///
/// 内容区边界若硬切，卡片内部会出现一条直角矩形接缝（比玻璃本身还显眼）。
/// 6px 的 smoothstep 让「玻璃处理」到「干净底图」之间看不出分界线。
const CONTENT_FEATHER: f32 = 6.0;

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
    /// 逐矩形的底色：`rgb` = 未预乘原色（0~1），`a` = 混合强度。
    ///
    /// 与 `alphas` 分开是因为一个 `vec4` 装不下五个分量
    /// （rgb + tint_strength + alpha）。两者都按 `rects` 的索引取。
    tints: array<vec4<f32>, MAX_GLASS_CARDS>,
    /// 逐矩形的**覆盖度**（0 = 完全透出桌面，1 = 不透明），取 `.x`。
    ///
    /// 与 `tints[i].a` 分工：这个决定「透过多少真实桌面」，那个决定
    /// 「看起来像不像一张有色卡片」。两者独立——覆盖度 1 + tint 0 就是
    /// 一块完全不透明、但完全通透的纯折射玻璃。
    ///
    /// CAUTION: 必须用 `vec4` 而不是 `f32`。uniform 空间要求数组元素
    /// 的**步长是 16 的倍数**，`array<f32, N>` 步长 4，naga 直接报
    /// `ArrayStride { stride: 4, alignment: Alignment(16) }` 校验失败。
    /// 只有 `x` 有意义。
    ///
    /// CAUTION: 它参与的是 `rgb * a` 的**预乘**，不是只写进 alpha 通道。
    /// 交换链按 PRE_MULTIPLIED 合成，只改 alpha 不改 RGB 会让半透明
    /// 卡片被额外叠一层桌面（见 `fs_glass` 末尾的 CAUTION）。
    alphas: array<vec4<f32>, MAX_GLASS_CARDS>,
    /// 逐矩形的圆角半径（像素），取 `.x`。
    ///
    /// # 为什么必须逐矩形
    ///
    /// 早前只有一份全局 `radius`，取**第一张**矩形的圆角。而 CPU 侧会把
    /// 顶栏（**直角**，半径 0）插到队首 ⇒ 整帧 `radius = 0` ⇒ SDF 退化成
    /// 普通矩形：内部处处 `d == 0`，而下面的命中判据是 `d >= 0` 直接
    /// 跳过 ⇒ **一个像素都不命中**，整窗只输出模糊底图，所有卡片与
    /// chrome 的玻璃全部消失，且不报任何错。
    ///
    /// 用 `vec4` 而不是 `f32`：uniform 数组元素步长必须是 16 的倍数。
    radii: array<vec4<f32>, MAX_GLASS_CARDS>,
    /// 目标图的像素尺寸 `(w, h)`。
    size: vec2<f32>,
    /// 边缘轮廓折射的最大位移（**物理像素**，`|n| = 1` 处的量）。
    ///
    /// CAUTION: 像素，不是 UV。见上面「位移量纲」那一段。
    refract: f32,
    /// 色散的单通道附加位移（**物理像素**，0 = 关掉彩色描边）。
    dispersion: f32,
    /// Fresnel 幂次（越小光晕越铺开，越大越集中在边缘一两像素）。
    edge: f32,
    /// 有效卡片数（`rects` 前多少项有意义）。
    count: u32,
    /// **背景层的不透明度**（不属于任何卡片的那些像素）。
    ///
    /// # 为什么必须有它（2026-10-28 修「无法渲染透明效果」）
    ///
    /// 早前背景层直接沿用 `blurred.a`（恒为 1：GDI 捕获强制 alpha=255，
    /// 卷积保持）⇒ **整窗不透明**。于是玻璃模式下窗口把真实桌面完全
    /// 盖住：用户拖任何透明度滑块都看不到桌面，玻璃模式与「不透明窗口」
    /// 没有区别——正是「alpha 通道异常、渲染不出透明」。
    ///
    /// 0 = 完全透出真实桌面（等同 Translucent 的背景），1 = 铺满捕获的
    /// 底图（等同 Acrylic）。默认 0.35：捕获底图只占三分之一权重，合成
    /// 结果 ≈ 真桌面 + 一档亮度自适应——**看得见桌面，又有可读性兜底**。
    bg_alpha: f32,
    /// 内容区向内收缩的距离（**物理像素**）。见 `content_clear`。
    ///
    /// **0 = 关闭**（整块都做玻璃处理）——`content_clear` 对 `inset <= 0`
    /// 直接返回 0。开关不单独占字段：少一个 `f32` 让结构体正好停在
    /// 1568（98×16），常量缓冲一个字节都不用加。
    content_inset: f32,
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

/// 2D 斜率场抬起成 3D 单位法线后的两个分量。
///
/// `nxy` = `N.xy`（已含 `SURFACE_TILT` 与归一化系数），`nz` = `N.z`。
/// 分开返回是因为折射的三个用途（基础位移、色散、2.5D 受光）都只要其中
/// 一部分，重复做 `length` + `sqrt` 是单像素成本里的大头。
struct RaisedNormal {
    nxy: vec2<f32>,
    nz: f32,
};

/// 斜率场 → 3D 法线。与 `glass::raise_normal` 逐行对应。
fn raise_normal(n: vec2<f32>) -> RaisedNormal {
    // `inv = 1/sqrt(t² + 1)` 一次给出 `N.xy`（乘 TILT·inv）与 `N.z`（= inv），
    // 省一次 normalize。
    let t = length(n) * SURFACE_TILT;
    let inv = inverseSqrt(t * t + 1.0);
    return RaisedNormal(n * (SURFACE_TILT * inv), inv);
}

/// 折射率项 `f(eta) = sqrt(1 - eta²(1 - N.z²)) - eta·N.z`。
/// 与 `glass::refract_f` 逐行对应。
///
/// # 为什么单独抽出来（延迟优化）
///
/// 色散要算三个 IOR 的**差值**，而三者只差在这个标量上。早前每个通道各
/// 调两次 `refract_uv`（每次都重做 `length` + 归一化 + `sqrt`）⇒ 单像素
/// 最多 6 次开方。抽成标量后：基础位移 1 次 + 每通道 1 次 = 4 次，且
/// `nxy` 只推导一次。
fn refract_f(nz: f32, eta: f32) -> f32 {
    let k = max(1.0 - eta * eta * (1.0 - nz * nz), 0.0);
    return sqrt(k) - eta * nz;
}

/// `refract(I, N, eta)` 的 2D 投影，返回**像素**位移。
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
/// 上式再乘 `(f / EDGE_FACTOR) / EDGE_NXY` 归一：
/// 于是 `|n| = 1` 且 `eta = DEFAULT_ETA` 时位移模长恰好等于 `power`。
fn refract_uv(n: vec2<f32>, eta: f32, power: f32) -> vec2<f32> {
    let r = raise_normal(n);
    let scale = (refract_f(r.nz, eta) / EDGE_FACTOR) / EDGE_NXY;
    return -r.nxy * (scale * power);
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

// ---- 2.5D 立体斜边 ----------------------------------------------------

/// 斜边带的受光差：`dot(N3, L) - L.z`，**平面处恰好为 0**。
/// 与 `glass::bevel_lambert` 逐行对应。
///
/// # 为什么减去 `L.z`
///
/// `w = 0`（斜边带以内）时 3D 法线是 `(0, 0, 1)`，`dot = L.z`。不减这一项，
/// 整块卡片平面都会拿到一个正的「受光」偏移（等于给卡片整体加亮），
/// 斜边的层次反而没了。减去之后：迎光斜边为正（亮棱）、背光斜边为负
/// （暗棱）、平面为 0。
///
/// `w`（斜边权重）本身也参与抬升：外缘最陡、带内端水平，与
/// [`bevel_weight`] 的剖面同源，所以受光强度沿带内自然衰减，不需要再
/// 乘一次权重。
fn bevel_lambert(n: vec2<f32>, w: f32, light: vec2<f32>) -> f32 {
    let l = normalize(vec3<f32>(light.x, light.y, LIGHT_Z));
    let t = clamp(w, 0.0, 1.0) * SURFACE_TILT;
    let inv = inverseSqrt(t * t + 1.0);
    let n3 = vec3<f32>(n * (t * inv), inv);
    return dot(n3, l) - l.z;
}

/// 细线凸起：`de = at` 处为 1、偏离 `width` 后为 0，两端斜率也为 0。
/// 与 `glass::thin_line` 逐行对应。
///
/// 用 `smoothstep` 三角窗而不是高斯：`exp` 在 GPU 上是 4~6 条指令，而
/// 这条线只值 1px 的观感；三角窗的平顶窄、两侧收得干净，实测亮度剖面
/// 与 1px 高斯几乎重合。
fn thin_line(de: f32, at: f32, width: f32) -> f32 {
    let x = clamp(1.0 - abs(de - at) / max(width, 1e-3), 0.0, 1.0);
    return x * x * (3.0 - 2.0 * x);
}

/// 内容区「退出玻璃计算」的掩码（0 = 照常做玻璃，1 = **完全透空**）。
/// 与 `glass::content_clear` 逐行对应。
///
/// 判定用**内缩矩形**而不是逐元素的真实包围盒：绘制层要区分文字 / 图标 /
/// 缩略图得把每个图元的矩形都送进来（uniform 装不下、每帧还要重传），
/// 而「卡片四周留 padding、中间是内容」这个格局对所有卡片都成立。
///
/// 边界必须羽化：内容区是直角矩形，硬切会在卡片内部留一条直角接缝，
/// 比玻璃本身还显眼（见 [`CONTENT_FEATHER`]）。
fn content_clear(p: vec2<f32>, half: vec2<f32>, inset: f32) -> f32 {
    if (inset <= 0.0) {
        return 0.0;
    }
    let inner = max(half - vec2<f32>(inset, inset), vec2<f32>(0.0));
    let cd = min(inner.x - abs(p.x), inner.y - abs(p.y));
    let x = clamp(cd / CONTENT_FEATHER, 0.0, 1.0);
    return x * x * (3.0 - 2.0 * x);
}

/// 边缘覆盖率过渡带的**半宽**（像素）。与 `glass::EDGE_AA_HALF_WIDTH` 一致。
///
/// 玻璃 pass 是全屏三角形 + 逐像素 SDF，边界频率不受几何超采样保护：
/// 2× SSAA 只是让目标分辨率翻倍，判据仍在放大后的像素格上二值化，
/// 阶梯变细但不会消失。必须在这里按 SDF 距离给覆盖率。
const EDGE_AA_HALF_WIDTH: f32 = 0.5;

/// AABB 粗判的放宽量（像素）。
///
/// 必须 ≥ `EDGE_AA_HALF_WIDTH`：覆盖率过渡带有一半落在**矩形之外**
/// （`d ∈ [0, 0.5]` 的圆角外侧），AABB 若按 0 截断，那半条带会被直接
/// 丢弃 ⇒ 抗锯齿只剩内半边，边缘看起来仍然偏硬。
const AA_PAD: f32 = 1.0;

/// 由 SDF 算出**内部深度**（内部为正、外部为负）。
/// 与 `glass::edge_depth` 逐行对应。
///
/// CAUTION: 不能直接拿 SDF 当深度。圆角矩形 SDF 在平坦区恒为 `-radius`：
/// 直角面板（r = 0）内部 `d == 0`，覆盖率会卡在 0.5 ⇒ **整块顶栏半透明**；
/// 圆角面板内部则被截断在 `-r`，深处与贴边得到同一个值。
/// CAUTION: 「到直边的距离」**只能用在内部**。
///
/// 早前内外都用 `max(-d, 到直边距离)`，结果**圆角被填成直角**
/// （2026-10-28 用户报障；只有玻璃模式有，其他背景模式的圆角由 egui
/// 自己画）。
///
/// 病根：圆角切掉的那一小块角落，点落在两条直边的**内侧**（「到直边
/// 距离」为正），却已被圆角弧切到形状外。`max` 把它判成内部 ⇒ 覆盖率 1
/// ⇒ 角落被填实，圆角越大填得越多。
///
/// 例：`half = (300, 29)`、`r = 12`，角落 `p = (-299, -28)`：
/// `d = +3.56`（外部）而 `min(300-299, 29-28) = 1.0` ⇒
/// `max(-3.56, 1.0) = 1.0` ⇒ 满覆盖。
///
/// 只有 SDF 知道弧在哪，所以**外部一律用 `-d`**。
fn edge_depth(p: vec2<f32>, half: vec2<f32>, sdf_d: f32) -> f32 {
    if (sdf_d > 0.0) {
        // 外部：含圆角切掉的角落，只有 SDF 是对的。
        return -sdf_d;
    }
    return max(-sdf_d, min(half.x - abs(p.x), half.y - abs(p.y)));
}

/// 由内部深度算出覆盖率（0 = 完全在外，1 = 完全在内）。
/// 与 `glass::edge_coverage` 逐行对应。
fn edge_coverage(depth: f32) -> f32 {
    return clamp(EDGE_AA_HALF_WIDTH + depth, 0.0, 1.0);
}

/// 按卡片短半轴算出磨砂尺度（像素）。与 `glass::frost_scale` 逐行对应。
///
/// 取 `min(half.x, half.y)`：磨砂沿「最薄的方向」展开，因为 `dist_edge`
/// 本身就在两个方向取最小值——用长边会让长条卡片的清晰带宽度远超
/// 卡片自身高度。
fn frost_scale(half: vec2<f32>) -> f32 {
    let m = min(abs(half.x), abs(half.y));
    return clamp(m * FROST_RATIO, FROST_SCALE_MIN, FROST_SCALE_MAX);
}

/// 某一点应有的磨砂比例（0 = 全清晰，`FROST_MAX` = 满档）。
/// 与 `glass::frost_mix` 逐行对应。
fn frost_mix(de: f32, half: vec2<f32>) -> f32 {
    let x = clamp(max(de, 0.0) / frost_scale(half), 0.0, 1.0);
    return pow(x, FROST_GAMMA) * FROST_MAX;
}

/// 某通道相对默认 IOR 的**色散系数**（无量纲）。
/// 与 `glass::dispersion_scale` 逐行对应。
///
/// 乘上 `-N.xy · 像素` 就是该通道比绿通道多走的位移。返回标量而不是向量
/// 是为了让调用方只推导一次 `N.xy`——早前三个通道各调两次 `refract_uv`，
/// 6 次开方里有 5 次算的是同一个方向。
fn dispersion_scale(nz: f32, eta: f32) -> f32 {
    return (refract_f(nz, eta) - refract_f(nz, DEFAULT_ETA)) / EDGE_FACTOR / EDGE_NXY;
}

/// 相对亮度（Rec.709）。
fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

/// 饱和度（HSV 口径）：`(max - min) / max`。纯白/灰/黑都是 0。
///
/// 与「色度向量偏离 1 多少」不同：它衡量这块颜色本身有多「彩」，
/// 与明暗无关。色度放大按它缩放，见 `glass_tint`（与 `glass::saturation`
/// 逐行对应）。
fn saturation(c: vec3<f32>) -> f32 {
    let hi = max(c.r, max(c.g, c.b));
    let lo = min(c.r, min(c.g, c.b));
    if (hi <= 1e-6) {
        return 0.0;
    }
    return clamp((hi - lo) / hi, 0.0, 1.0);
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

// ---- 卡片底色混色（tint）：吸收 + 散射 ----------------------------
//
// 早前只有一句 `mix(rgb, tint, a)`——那是**替换式**覆盖：卡片色按覆盖度
// 直接替换底图，把底图的明暗层次与高频一起压平。观感后果随主题分裂：
//
// - 浅色主题 `card_bg = #FFFFFF`（纯白）⇒ 替换式把底图**洗白**，卡片读成
//   一块牛奶板，折射与彩边都被冲掉——「浅色下发灰」的病根；
// - 深色主题 `card_bg = #2F3540`（luma 0.20）⇒ 替换式把底图**压暗**，
//   深色壁纸上整块糊成灰板（2026-10-25 那条旧账的残留）。
//
// 真实有色玻璃是**两件事同时发生**：
//
// 1. **吸收**（乘法）：光穿过玻璃，透过率由玻璃自身颜色决定 ⇒
//    `rgb × transmit`。乘法**保留底图的明暗结构**，只给整体染色；
// 2. **散射**（加性）：玻璃内部的雾度把自身颜色散出来 ⇒ `+ scatter`。
//    这一项才是「乳白 / 磨砂玻璃」的质感来源。
//
// 主题自适应**不需要额外 uniform**：`tint` 自己就带着主题信息
// （浅色主题是纯白 luma = 1.0，深色主题 luma ≈ 0.20）。

/// 吸收强度：`tint` 作为**透过率**被拉向自身颜色的程度。
///
/// 0 = 完全不吸收（玻璃全透，等于没染色），1 = 透过率 = `tint` 本身。
const TINT_ABSORB: f32 = 0.45;

/// 色度放大：把 `tint` 除以自身亮度得到「均值 1 的色度」，再按这个比例
/// 放大差距。
///
/// 深色主题的 `card_bg` 是冷蓝灰（R < G < B），透过率带上这个色偏后，
/// 卡片会明显**泛冷调**而不是单纯变暗——「混色」的可见度主要来自它。
/// 浅色主题的 `card_bg` 是纯白（色度恒为 1）⇒ 这一项自动失效，不会
/// 给浅色凭空染色。
///
/// CAUTION: 实乘量还要乘 `saturation(tint)`（见 `glass_tint`）。深色
/// 卡面 `#2F3540` 是近中性冷灰（饱和 0.27），拿满这个值会让透过率里
/// 蓝比红高 22%，整卡偏蓝（2026-10-29 用户报障）。
const TINT_CHROMA: f32 = 0.5;

/// 散射（乳光）的基准增益。
const TINT_SCATTER: f32 = 0.18;

/// 浅色 tint 的散射衰减（乘在 [`TINT_SCATTER`] 上）。
///
/// 散射是**加性**的，而浅色 tint 接近纯白 ⇒ 加进去的是白光，会把亮底
/// 一路推到饱和（卡片边缘的折射带、彩边全被冲平）。按 tint 亮度线性
/// 衰减：深色 = 1.0（原样），浅色 = 0.55。
const TINT_SCATTER_LIGHT_DAMP: f32 = 0.55;

/// 深色 tint 的**透过率地板**。
///
/// 深色玻璃若按 `mix(1, tint, ABSORB)` 一路算下去，暗底图会连同高光一起
/// 被吃掉 ⇒ 卡片变成「黑玻璃」，折射与边缘光全部消失（那正是
/// 2026-10-25「整卡平板化」的观感）。给透过率一个下限，保证身后内容
/// 始终可见。按 `1 - t_lum` 缩放：浅色主题地板为 0（纯白本来就不吸收）。
const TINT_FLOOR_DARK: f32 = 0.28;

/// 两段式模型相对「替换式」的占比。
const TINT_MIX: f32 = 0.9;

/// 有色玻璃混色。与 `glass::glass_tint` 逐行对应。
///
/// `a` 是卡片覆盖度（[`CARD_TINT_MAX`] 之后的值，≤ 0.45）。
///
/// # 端点语义不变
///
/// `a = 0`（无玻璃）→ 原样返回底图；`a = 1`（实心）→ 精确返回 `tint`。
/// 两段式只在**中间区间**生效（权重 `a(1-a)·4`，峰值在 `a = 0.5`）——
/// 老配置与新设置的观感因此连续，「卡片不透明度」滑块的语义不变。
fn glass_tint(rgb: vec3<f32>, tint: vec3<f32>, a: f32) -> vec3<f32> {
    let base = mix(rgb, tint, a);
    let t_lum = clamp(luma(tint), 0.0, 1.0);
    // 色度：`tint / luma` 的加权均值为 1，于是 `mix(1, chroma, k)` 是
    // 「围绕中性色的色偏放大」。纯白/纯灰的色度恒为 1 ⇒ 不染色。
    //
    // CAUTION: 放大系数按 tint **自身饱和度**缩放——近中性的深色卡面
    // 拿满 `TINT_CHROMA` 会把透过率染成蓝色滤镜（2026-10-29）。
    let chroma_k = TINT_CHROMA * saturation(tint);
    let chroma = mix(vec3<f32>(1.0), tint / max(t_lum, 1e-3), chroma_k);
    let floor = TINT_FLOOR_DARK * (1.0 - t_lum);
    let absorb = mix(vec3<f32>(1.0), tint, TINT_ABSORB) * chroma;
    let transmit = max(absorb, vec3<f32>(floor));
    let scatter = tint * (TINT_SCATTER * mix(1.0, TINT_SCATTER_LIGHT_DAMP, t_lum));
    let tinted = rgb * transmit + scatter;
    // `a(1-a)·4`：a=0 与 a=1 时为 0，a=0.5 时为 1。
    let k = clamp(TINT_MIX * (a * (1.0 - a) * 4.0), 0.0, 1.0);
    return mix(base, tinted, k);
}

/// 背景层（不属于任何卡片的像素）。
///
/// # 为什么抽成函数、并且**在卡片循环之后**才调（延迟优化）
///
/// 早前 `fs_glass` 一进来就为**每个像素**采两次底图（清晰 + 模糊）作为
/// `out` 的初值，而卡片像素随后会把它整个覆盖掉——列表卡片覆盖了窗口的
/// 大部分面积，那两次采样对它们纯属浪费。挪到循环之后、并且用 `return`
/// 直接结束，卡片像素一次底图都不采。
fn bg_layer(uv: vec2<f32>) -> vec4<f32> {
    let blurred = textureSampleLevel(blur_tex, glass_sampler, uv, 0.0);
    let sharp = textureSampleLevel(sharp_tex, glass_sampler, uv, 0.0);
    // 亮度自适应同样作用在背景层上：它是整窗面积最大的一块，亮壁纸
    // 下不压一档的话前景文字的对比度全靠 egui 那层半透明 `bg` 撑着。
    let rgb = adapt_luma(
        mix(sharp.rgb, blurred.rgb, BG_FROST),
        LUMA_DIM_MAX,
        LUMA_LIFT_MAX,
    );
    // CAUTION: 背景层的 alpha **不能**取 `blurred.a`（恒为 1）。那会把
    // 整个窗口铺成不透明底图，真实桌面一点都透不出来——用户报的
    // 「alpha 异常、渲染不出透明」就是这一处（2026-10-28）。
    //
    // 输出必须**预乘**（交换链是 PRE_MULTIPLIED），所以 RGB 也要乘同一
    // 个 alpha——只改 alpha 会把底图凭空叠一层桌面，越透明越亮。
    let a = clamp(u.bg_alpha, 0.0, 1.0) * blurred.a;
    return vec4<f32>(rgb * a, a);
}

@fragment
fn fs_glass(in: VertexOutput) -> @location(0) vec4<f32> {
    // uv → 物理像素（egui y-down 同向，见模块头「坐标约定」）。
    let pixel = in.uv * u.size;
    // 位移在着色器内部一律是**像素**，折成 UV 只在这里做一次除法
    // （逐轴，所以横竖位移的物理长度一致）。
    let inv_size = vec2<f32>(1.0) / max(u.size, vec2<f32>(1.0));
    let uv_src = in.uv + copy_pc.offset;

    for (var i: u32 = 0u; i < u.count; i = i + 1u) {
        let rc = u.rects[i];
        let half = rc.zw;
        let rel = pixel - rc.xy;

        // ---- AABB 粗判（先于 SDF）----
        //
        // 卡片像素占多数，但**卡片之间的空隙**、窗口里没有卡片的区域
        // 仍要走完整个循环。粗判只要 4 次比较，把那些像素的成本从
        // 「一次 SDF（含 sqrt）」降一个量级。
        //
        // CAUTION: 放宽 `AA_PAD` 而不是按 0 截断。抗锯齿的过渡带有**一半
        // 在矩形之外**（圆角外侧 `d ∈ [0, 0.5]`），按 0 截断会把那半条
        // 带整个丢掉，边缘只剩内半边渐变——看起来仍然偏硬，而且圆角处
        // 会出现「内圆滑、外圈毛」的错位感。
        let outside = abs(rel) - half;
        if (outside.x > AA_PAD || outside.y > AA_PAD) {
            continue;
        }

        // ---- SDF 与解析法线一次算出 ----
        //
        // 列表卡片互不重叠，第一张命中的就是最终结果；顶栏/搜索面板/
        // 类型栏同样不重叠。
        let r = u.radii[i].x;
        let sn = sdf_rr_n(rel, half, r);
        // 覆盖率为 0 才跳过（等价于深度 ≤ -0.5px）。圆角切掉的角上自然
        // 为 0；直角矩形内部深度为正（见 `edge_depth`），覆盖率恒为 1
        // ——不会重演「顶栏整块半透明」。
        let aa = edge_coverage(edge_depth(rel, half, sn.d));
        if (aa <= 0.0) {
            continue;   // 完全在形状外
        }

        // ---- 距边距离 / 斜角权重 ----
        let de = dist_edge(rel, half, sn.d);
        // 斜角带：只在距边 `r` 像素以内折，带外严格为 0。
        // 直角面板（r = 0）的带只有 1px——它本来就不该有斜角。
        let w = bevel_weight(de, r);
        let local = rel / max(half, vec2<f32>(1e-3, 1e-3));
        let slope = dome_slope(local);

        // ---- 内容区退出玻璃计算（可开关，默认关）----
        //
        // `clear = 1` 的像素**完全透空**：玻璃层在这里什么都不输出——
        // 连底图都不画。真桌面直接透上来，文字与图标因此浮在一块干净的
        // 透明玻璃上，而不是叠在一张被抓屏/折射/磨砂处理过的图上。
        //
        // 默认 `u.content_inset = 0` ⇒ `clear` 恒为 0，与关闭时逐点一致。
        let clear = content_clear(rel, half, u.content_inset);
        if (clear >= 1.0) {
            // 元素区：整块不参与玻璃计算。
            //
            // CAUTION: 这里**不能** `continue` 到 `bg_layer`——那张就是
            // 「底图」，会把捕获内容画回来（正是要去掉的东西）。
            return vec4<f32>(0.0);
        }

        // ---- 两级折射：穹顶（中心）+ 边缘轮廓（贴边）----
        //
        // `N.xy` 只推导一次，基础折射与三个色散通道共用（见
        // `raise_normal` / `dispersion_scale` 的注释）。
        let raised = raise_normal(sn.n);
        let base_scale = (refract_f(raised.nz, DEFAULT_ETA) / EDGE_FACTOR) / EDGE_NXY;
        let dome_px = refract_uv(slope, DEFAULT_ETA, u.refract * DOME_RATIO);
        let base_px = -raised.nxy * (base_scale * u.refract * w) + dome_px;

        // 色散只在**靠近边缘**时启用（见 `DISPERSION_BAND`）：内部三通道
        // 位移几乎重合，分开采样纯浪费带宽。开启内容区排除后它同样退出。
        let disp_px = select(0.0, u.dispersion, de < DISPERSION_BAND) * w;

        var sharp_rgb: vec3<f32>;
        if (disp_px > 0.001) {
            // 每通道一个标量系数（见 `dispersion_scale`），方向共用
            // 上面的 `raised.nxy`：早前要 6 次开方，现在 4 次。
            let kr = dispersion_scale(raised.nz, 1.0 / IOR_R);
            let kg = dispersion_scale(raised.nz, 1.0 / IOR_G);
            let kb = dispersion_scale(raised.nz, 1.0 / IOR_B);
            let uv_c = uv_src + base_px * inv_size;
            let sr = textureSampleLevel(sharp_tex, glass_sampler, uv_c + (-raised.nxy * (kr * disp_px)) * inv_size, 0.0);
            let sg = textureSampleLevel(sharp_tex, glass_sampler, uv_c + (-raised.nxy * (kg * disp_px)) * inv_size, 0.0);
            let sb = textureSampleLevel(sharp_tex, glass_sampler, uv_c + (-raised.nxy * (kb * disp_px)) * inv_size, 0.0);
            sharp_rgb = vec3<f32>(sr.r, sg.g, sb.b);
        } else {
            // 单次采样：位移取绿通道（三通道的中间值）。
            sharp_rgb = textureSampleLevel(sharp_tex, glass_sampler, uv_src + base_px * inv_size, 0.0).rgb;
        }

        // ---- 磨砂 / 清晰混合：边缘清晰、中心磨砂 ----
        //
        // `de` 是**距边距离**：贴边处 `frost → 0` 取折射后的清晰图，
        // 深入内部 `frost → FROST_MAX` 才是磨砂。与斜角权重 `w` 正好
        // 反向——贴边处折射最强且最清晰（那是液态玻璃的辨识特征），
        // 内部折射归零、退回磨砂。
        //
        // 磨砂那一路**也**按折射位移采样（见 `BLUR_REFRACT`）：位移图在
        // Apple 的实现里作用在模糊之后的背景上，只让清晰图位移会让占
        // 面积大多数的中心完全没有透镜感。模糊图**不做色散**：它已经没有
        // 高频，三通道分开采样的差值在一张糊图上不可见。
        let frost = frost_mix(de, half);
        let blur_rgb = textureSampleLevel(
            blur_tex,
            glass_sampler,
            uv_src + base_px * (inv_size * BLUR_REFRACT),
            0.0,
        ).rgb;
        var rgb = mix(sharp_rgb, blur_rgb, frost);

        // ---- 亮度自适应（Apple regular 变体）----
        //
        // 亮底压暗、暗底提亮：卡片上的前景文字对比度不随壁纸/桌面内容
        // 漂移。放在混合**之后**、高光**之前**——高光是光源的反光，
        // 不该被底图的亮度修正吃掉。内容区按 `clear` 线性退回原色。
        rgb = adapt_luma(rgb, LUMA_DIM_MAX, LUMA_LIFT_MAX);

        // ---- 卡片底色（tint）：玻璃层自己合成 ----
        //
        // 位置在亮度自适应**之后**、高光**之前**：
        // - 在自适应之后：tint 是「玻璃本身的颜色」，不该被底图亮度修正
        //   拽着走（亮壁纸下卡片不该被一起提亮成灰白）；
        // - 在高光之前：高光是光源的反光，叠在 tinted 玻璃之上。
        //
        // 底色与覆盖度都是**本矩形自己的**（`tints[i]` / `alphas[i]`）：
        // 行卡片、顶栏、搜索面板、类型栏各有各的底色，不能共用一份。
        let tint = u.tints[i];
        rgb = glass_tint(rgb, tint.rgb, clamp(tint.a, 0.0, 1.0));

        // ---- 轮廓：Fresnel（宽、各向同性）+ 镜面高光（锐、方向性）----
        //
        // Fresnel 用**斜率场**而不是 SDF 梯度：后者在内部恒 0、贴边为 1，
        // 用 `1-|n|` 形式的公式会让整片内部最亮（物理映射反了）。
        // 见 `fresnel_edge` 的 CAUTION。这一层是「环境反射」，四边都有一点。
        let f = pow(clamp(length(slope), 0.0, 1.0), max(u.edge, 0.0));
        rgb = rgb + RIM_TINT * f;
        //
        // 镜面高光才是 Apple 那句「**用一圈高光取代描边**」：朝向光源的
        // 一侧最亮、背光一侧为 0。关键在两点——
        //   ① 用斜角带的**朝外法线** `n` 与光源方向点积（方向性）；
        //   ② 再乘斜角权重 `w`：它只在轮廓带上成立，平坦区 `n = 0`
        //      本来也不会有高光，乘 `w` 是为了让它在带内端平滑收尾。
        let spec = pow(max(dot(sn.n, LIGHT_DIR), 0.0), max(u.edge, 0.5) * SPEC_SHININESS);
        rgb = rgb + SPEC_TINT * (spec * SPEC_GAIN * w);

        // ---- 2.5D：斜边受光 / 背光 + 内缘高光线 + 外缘接触线 ----
        //
        // 这四层合起来才是「一块有厚度的玻璃板」：
        //   · `lam > 0`（迎光斜边）⇒ 把光反进眼睛，加亮；
        //   · `lam < 0`（背光斜边）⇒ 只剩环境光，**乘性**压暗；
        //   · 内缘细线 ⇒ 斜边与平面的折线（厚度的直接线索）；
        //   · 外缘暗线 ⇒ 板与桌面之间的缝，浅色桌面上把轮廓分开。
        // 全部乘 `w`（或按距边定位）⇒ 只在轮廓一带成立，平面区严格为 0。
        let lam = bevel_lambert(sn.n, w, LIGHT_DIR);
        let gate = thin_line(de, 0.0, BEVEL_LIT_WIDTH);
        let lit = gate * max(lam, 0.0) * BEVEL_LIT_GAIN;
        let dark = gate * max(-lam, 0.0) * BEVEL_DARK_GAIN;
        let hair = thin_line(de, BEVEL_HAIRLINE_AT, BEVEL_HAIRLINE_WIDTH) * BEVEL_HAIRLINE_GAIN;
        let contact = thin_line(de, 0.0, BEVEL_CONTACT_WIDTH) * BEVEL_CONTACT_GAIN;
        rgb = rgb * (1.0 - dark - contact) + SPEC_TINT * (lit + hair);

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
        // `aa`（边缘覆盖率）无条件乘进来：它只在那 1px 过渡带里小于 1，
        // 带内像素因此按覆盖率把玻璃混到**底图**上（`bg_layer` 的输出），
        // 正是抗锯齿要的结果。预乘关系不变（RGB 与 alpha 同乘），所以不会
        // 重演上面 CAUTION 说的「越透明越亮」。
        //
        // 内缘高光线与外缘接触线都写在预乘**之前**：它们属于玻璃自身的
        // 颜色，乘 alpha 之后才是「这块玻璃盖住桌面多少」。
        // 元素区（`clear`）只在这里起作用：整块按 `(1 - clear)` 折叠到
        // 透明，羽化带因此是「玻璃边框 → 透空」的平滑过渡。
        let a = clamp(u.alphas[i].x, 0.0, 1.0) * aa * (1.0 - clear);
        if (a > 0.0) {
            return vec4<f32>(rgb * a, a);
        }
        // 覆盖度为 0（矩形完全透明）：什么都不做，交给背景层。
    }

    return bg_layer(uv_src);
}
