// ModularClipboard 自研模糊着色器。
//
// 一套着色器里放三个入口点，由同一个 `ShaderModule` 引用：
// - `vs_fullscreen`：全屏三角形顶点（无顶点缓冲，`draw(3,1,0,0)`）
// - `fs_blur`：分离式高斯的一维卷积（横向 / 纵向由 uniform 的 `dir` 决定）
// - `fs_copy`：直通采样（用于降采样与最终合成）
//
// # 为什么用全屏三角形而不是全屏四边形
//
// 四边形需要 4 顶点 + 2 三角形（6 索引），三角形只要 3 个顶点且
// 覆盖整个裁剪空间（`(-1,-1) (3,-1) (-1,3)`）。少一次顶点变换、
// 少一条对角线上的二次光栅化，且不需要顶点缓冲与索引缓冲。
//
// # 为什么权重在着色器里现算
//
// WGSL 的 uniform 地址空间里 `array<f32, N>` 的元素步长必须是 16 字节
// 的整数倍（std140 规则），直接写 `array<f32, 13>` 会被 naga 判为
// 非法布局。把「半径 + σ」传进来、在片元里用 `exp` 现算权重，
// 既绕开这条限制，又让同一份着色器能服务任意半径（CPU 端不必为
// 每个半径编译一份 SPIR-V）。
//
// # 线性空间的取舍（必须知道）
//
// 严格正确的高斯模糊应当在线性光空间做。但本项目的离屏目标与交换链
// 同格式（多为 `B8G8R8A8_UNORM`，非 sRGB），存的就是已编码值。
// 对剪贴板这类「大面积纯色 + 文字」的界面，在编码空间直接卷积的
// 视觉差异不可辨（暗部过渡略有偏差），却省掉两趟 sRGB 编解码。
// 若将来离屏目标改用 `*_SRGB`，采样器会自动解码、写入自动编码，
// 这段着色器无需改动。

struct BlurUniforms {
    // 采样方向与步长（**纹素**单位）：横向为 (1/w, 0)，纵向为 (0, 1/h)。
    dir: vec2<f32>,
    // 高斯半径（整数，CPU 端已 clamp 到 MAX_BLUR_RADIUS）。
    radius: f32,
    // 高斯标准差。
    sigma: f32,
    // 源纹理尺寸（纹素）。当前未直接使用，保留给将来的
    // 「按屏幕比例固定模糊半径」等变体，避免改动结构体布局。
    texel: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: BlurUniforms;
@group(0) @binding(1) var blur_sampler: sampler;
@group(0) @binding(2) var src: texture_2d<f32>;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_fullscreen(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    // 全屏三角形：覆盖 NDC 的 [-1,1]²，多出来的部分落在视口外被裁掉。
    var pts = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let xy = pts[vertex_index];

    var out: VertexOutput;
    out.clip_position = vec4<f32>(xy, 0.0, 1.0);
    // NDC → UV。y 轴翻转：NDC 的 y 向上，纹理坐标的 v 向下
    // （图像的 (0,0) 在左上）。翻转写错会让模糊结果上下颠倒，
    // 且因为模糊是对称的、画面看起来「只是有点怪」——很难察觉。
    out.uv = vec2<f32>(xy.x * 0.5 + 0.5, 0.5 - xy.y * 0.5);
    return out;
}

@fragment
fn fs_blur(in: VertexOutput) -> @location(0) vec4<f32> {
    let r = i32(u.radius);
    // σ 太小会让卷积核退化成「只取中心像素」（除零/欠采样），
    // 下限 0.5 保证至少是 3-tap 的平均。
    let s = max(u.sigma, 0.5);
    let s2 = s * s;

    var acc = vec4<f32>(0.0);
    var weight_sum = 0.0;

    var i = -r;
    loop {
        if (i > r) {
            break;
        }
        let fi = f32(i);
        // 高斯核：g(k) = exp(-k² / (2σ²))。常数项被归一化约掉，故省略。
        let w = exp(-0.5 * fi * fi / s2);
        // IMPORTANT: 必须用 `textureSampleLevel`（显式 LOD 0）而非
        // `textureSample`。后者的隐式 LOD 需要屏幕空间导数，而 WGSL 要求
        // 它处于 uniform control flow 中——循环体内的调用不是。
        // 非 uniform 控制流里用 `textureSample` 是未定义行为，且验证层不报。
        acc = acc + textureSampleLevel(src, blur_sampler, in.uv + u.dir * fi, 0.0) * w;
        weight_sum = weight_sum + w;
        i = i + 1;
    }

    // 归一化放在着色器里：CPU 端不必逐权重归一，也不怕浮点累积误差。
    return acc / weight_sum;
}

@fragment
fn fs_copy(in: VertexOutput) -> @location(0) vec4<f32> {
    // 降采样与最终合成都走这里。降采样时源是全分辨率场景、
    // 目标是 1/N 分辨率，线性采样器自动做一次双线性平均，
    // 等价于一次 2x2 的盒式滤波（N=2 时精确，N=4 时近似）。
    return textureSampleLevel(src, blur_sampler, in.uv, 0.0);
}
