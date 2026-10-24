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
//
// # 坐标约定
//
// `in.uv` 与 egui 的 y-down 屏幕坐标同向（`v=0` 是 framebuffer 顶行），
// 所以 `pixel = uv * size` 直接就是 egui 的物理像素坐标，卡片矩形可以
// 原样比较。这条约定由 `card_blur_uv` 的用法反推并在此固定。

/// 同时存在的玻璃面板上限。
///
/// 列表行卡片约 10 张 + 顶栏 + 搜索面板 + 类型栏 ≈ 13，取 16 留余量。
/// 超出部分**静默丢弃**（不报错）：玻璃是观感层，多两张少两张不该让
/// 界面崩。真要超，先把可见行数降下来更合理。
const MAX_GLASS_CARDS: u32 = 16u;

/// 色散的三个折射率（R 最小、B 最大 ⇒ 蓝光弯得最多，边缘出现彩色描边）。
/// 与 `glass::IOR_R/G/B` 一致。
const IOR_R: f32 = 1.50;
const IOR_G: f32 = 1.52;
const IOR_B: f32 = 1.54;

/// 默认折射率比值 `eta = 1 / ior`（ior = 1.5）。
const DEFAULT_ETA: f32 = 1.0 / 1.5;

/// 穹顶折射相对边缘轮廓折射的强度比。
///
/// Rust 侧的测试用 `dome power = 0.03`、`edge power = 0.35`，比值 0.086。
/// 取 0.08：穹顶只负责「中心也有折射」，主角是边缘轮廓。
const DOME_RATIO: f32 = 0.08;

/// 磨砂/清晰混合的距离尺度（像素）：距边这么多像素以内渐变为磨砂。
///
/// 中心清晰（能看见折射后的桌面）、边缘磨砂——液态玻璃的辨识度全在
/// 这个分布上。整片都清晰就只是「透明卡片」，整片都糊就退回 Acrylic。
const FROST_SCALE: f32 = 36.0;

/// 色散生效的**距边带宽**（像素）。
///
/// 三个通道的位移差与到边缘的距离成正比：卡片内部它们几乎重合，
/// 分开采样得到的结果与单次采样差不到一个纹素，却要多付两次纹理
/// 读取。模糊/玻璃类 pass 是**带宽瓶颈**，能省的每次采样都值。
///
/// 24px：视觉上彩边只出现在边缘一两毫米内，再往里看不出来。
const DISPERSION_BAND: f32 = 24.0;

/// Fresnel 亮边的 RGB 增益（冷白色，略偏蓝）。
const RIM_TINT: vec3<f32> = vec3<f32>(0.20, 0.23, 0.28);

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
    /// 卡片底色 alpha：玻璃只改 RGB，alpha 交给它——半透明卡片下
    /// 玻璃层该透多少由绘制层决定，与背景层的 alpha 语义解耦。
    card_alpha: f32,
    /// 占位，把结构体补到 16 字节倍数（WGSL uniform 的规则）。
    ///
    /// CAUTION: 必须是 `vec4<f32>` 而不是 `f32`。Rust 侧
    /// `GlassUniforms::pad` 是 `[f32; 4]`，两者合起来正好 304 字节且
    /// **无隐式填充**（bytemuck::Pod 要求没有填充字节）。写成 `f32`
    /// 会让 WGSL 认为结构体尾部还有 12 字节填充，与 Rust 的 304 不一致。
    pad: vec4<f32>,
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

/// `refract(I, N, eta)` 的 2D 形式，返回 UV 位移。
///
/// 与 `glass::refract_offset` 逐行对应。`incident = -normal`（光从玻璃外
/// 垂直射入曲面）。
fn refract_uv(n: vec2<f32>, eta: f32, power: f32) -> vec2<f32> {
    let i = -n;
    let d = dot(n, i);
    let k = max(1.0 - eta * eta * (1.0 - d * d), 0.0);
    let s = sqrt(k);
    return (eta * i - (eta * d + s) * n) * power;
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

        // ---- 距边距离 / 衰减 ----
        let de = dist_edge(p, half, d);
        let fall = exp(-de * 0.4);          // 边缘处 ≈1，深入内部 →0
        let local = p / half;               // 归一化到 [-1,1]
        let slope = dome_slope(local);

        // ---- 两级折射：穹顶（中心）+ 边缘轮廓（贴边）----
        //
        // 穹顶**只算一次**（用默认 IOR）：三个 IOR 对穹顶位移的影响不到
        // 一个纹素的 1%，分通道算纯属浪费（每次 refract 含一个 sqrt）。
        // 色散的差异只体现在边缘轮廓折射上——那里位移大、差异才看得见。
        let dome_c = dome_offset(local, DEFAULT_ETA, u.refract * DOME_RATIO);
        // 参考位移（`dispersion = 0` 时三通道共用它）。
        let dref = refract_uv(n, DEFAULT_ETA, u.refract) * fall + dome_c;
        let dg = refract_uv(n, 1.0 / IOR_G, u.refract) * fall + dome_c;
        // 色散只在**靠近边缘**时启用（见 `DISPERSION_BAND`）：内部三通道
        // 位移几乎重合，分开采样纯浪费带宽。
        let eff_disp = select(0.0, u.dispersion, de < DISPERSION_BAND);

        var sharp_rgb: vec3<f32>;
        if (eff_disp > 0.001) {
            let dr = refract_uv(n, 1.0 / IOR_R, u.refract) * fall + dome_c;
            let db = refract_uv(n, 1.0 / IOR_B, u.refract) * fall + dome_c;
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

        // ---- 磨砂 / 清晰混合：中心清晰、边缘磨砂 ----
        //
        // 液态玻璃的辨识度全在这个分布上。整片清晰 = 透明卡片，
        // 整片磨砂 = 退回 Acrylic。
        let frost = clamp(de / FROST_SCALE, 0.0, 1.0) * 0.85;
        var rgb = mix(sharp_rgb, blurred.rgb, frost);

        // ---- Fresnel 边缘光 ----
        //
        // 用**斜率场**而不是 SDF 梯度：后者在内部恒 0、贴边为 1，
        // 用 `1-|n|` 形式的公式会让整片内部最亮（物理映射反了）。
        // 见 `glass::fresnel_edge` 的 CAUTION。
        let f = pow(clamp(length(slope), 0.0, 1.0), max(u.edge, 0.0));
        rgb = rgb + RIM_TINT * f;

        // alpha 取磨砂底图的：玻璃只改颜色，覆盖语义不变。
        out = vec4<f32>(rgb, blurred.a * u.card_alpha);
        break;
    }

    return out;
}
