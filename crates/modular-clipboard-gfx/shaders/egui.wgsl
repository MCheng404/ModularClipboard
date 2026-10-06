// ModularClipboard 的 egui 绘制着色器。
//
// 数据布局与 egui 的 `ClippedPrimitive` 对应：
// - 顶点：屏幕空间位置、纹理坐标、顶点色（打包为 u32 以节省带宽）
// - 裁剪：egui 会为每个图元给出 `clip_rect`，通过 uniform 传入以避免顶点撕裂
// - 混合：标准 src-alpha 混合
//
// 顶点位置以「像素」传入，乘以 dpr 转成物理像素，再经 clip_from_uv 变换到 NDC。
// 变换矩阵而非逐顶点乘算是为了让 egui 侧的裁剪与变换逻辑保持单一来源。

struct Uniforms {
    // 像素坐标 → NDC 的变换矩阵（列主序）
    clip_from_uv: mat4x4<f32>,
    // 视口尺寸（物理像素）
    size_in_pixels: vec2<f32>,
    // 设备像素比
    dpr: f32,
    // 采样模式标志，保留给后续的纹理过滤切换
    _pad: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var tex_sampler: sampler;
@group(0) @binding(2) var font_tex: texture_2d<f32>;
// 用户纹理（图片缩略图）。字体图集是单通道覆盖率，
// 缩略图是 RGBA 彩色图——两者语义不同，故各用一张。
@group(0) @binding(3) var user_tex: texture_2d<f32>;

struct VertexInput {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: u32,
    // 0 = 字体图集，1 = 用户纹理。逐顶点不同（同一批内混两种），
    // 正是这一点决定了采样必须用 textureSampleLevel 而非 textureSample。
    @location(3) tex_id: u32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    // 逐顶点插值没有意义，取 provoking vertex 即可；
    // 同一图元内所有顶点 tex_id 必然相同（egui 按 mesh 整体给纹理）。
    @location(2) @interpolate(flat) tex_id: u32,
};

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;

    // pos 为逻辑像素，乘 dpr 得到物理像素。y 轴需翻转：
    // egui 的 y 向下增长，NDC 的 y 向上。
    let scaled = vec2<f32>(in.pos.x * u.dpr, in.pos.y * u.dpr);
    let ndc_y = 1.0 - 2.0 * scaled.y / u.size_in_pixels.y;
    let ndc_x = 2.0 * scaled.x / u.size_in_pixels.x - 1.0;

    out.clip_position = u.clip_from_uv * vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.uv = in.uv;
    // egui 把颜色打包为 ABGR 小端序的 u32
    out.color = unpack4x8unorm(in.color);
    out.tex_id = in.tex_id;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // ⚠️ 必须用 `textureSampleLevel`（显式 LOD 0）而**不是** `textureSample`。
    //
    // `textureSample` 内部用隐式 LOD，需要计算屏幕空间导数；
    // WGSL 与 Vulkan 都要求它处于 **uniform control flow** 中。
    // 而这里的分支条件 `in.tex_id` 是**逐片元**的（同一图元内相同，
    // 但不同图元不同）⇒ 非 uniform control flow。
    //
    // 在非 uniform 控制流里调用 `textureSample` 是**未定义行为**，
    // 症状是驱动行为不可预测（可能返回垃圾、可能设备丢失），
    // 且**验证层不会报错**。
    //
    // `textureSampleLevel` 显式指定 mip 层级，不需求导数，
    // 允许非 uniform 控制流——这正是我们要的。
    //
    // 代价：失去三线性过滤的自动选择。我们用 nearest 采样器
    // （字体图集本就需要 nearest，避免字形糊），故无实际损失。
    if (in.tex_id == 0u) {
        // 字体图集是单通道覆盖率，乘顶点色得到最终 RGBA
        let cov = textureSampleLevel(font_tex, tex_sampler, in.uv, 0.0).r;
        return in.color * vec4<f32>(1.0, 1.0, 1.0, cov);
    }
    // 用户纹理是 RGBA 彩色图（图片缩略图），已含颜色
    return textureSampleLevel(user_tex, tex_sampler, in.uv, 0.0);
}
