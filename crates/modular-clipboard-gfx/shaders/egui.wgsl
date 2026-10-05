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

struct VertexInput {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: u32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
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
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(font_tex, tex_sampler, in.uv);
    // egui 的字体图集是覆盖率（单通道），需乘顶点色得到最终 RGBA
    return in.color * vec4<f32>(1.0, 1.0, 1.0, texel.r);
}
