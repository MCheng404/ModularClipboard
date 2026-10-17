// ModularClipboard 的 egui 绘制着色器。
//
// 数据布局与 egui 的 `ClippedPrimitive` 对应：
// - 顶点：屏幕空间位置、纹理坐标、顶点色（打包为 u32 以节省带宽）
// - 裁剪：egui 会为每个图元给出 `clip_rect`，通过 uniform 传入以避免顶点撕裂
// - 混合：标准 src-alpha 混合
//
// 顶点位置以「像素」传入，乘以 dpr 转成物理像素，再换算成 NDC。
//
// 2026-10-17：删掉了 `clip_from_uv`（恒为单位矩阵的 mat4x4）——它从来
// 只有一个值，而像素→NDC 由本着色器自己完成，留下它等于每顶点白算
// 16 个 FMA，还埋着「谁设成非单位矩阵就二次变换」的陷阱。裁剪现在
// 完全由 uniform 的 `size_in_pixels` + CPU 侧 scissor 负责。
//
// 历史版本的头注释写的是「变换矩阵而非逐顶点乘算是为了让 egui 侧的
// 裁剪与变换逻辑保持单一来源」——那是矩阵还在时的理由，现已作废。

struct Uniforms {
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
// 用户纹理的采样器（linear，2026-10-17 起与字体图集分开）。
//
// 为什么必须是 linear：缩略图的**显示尺寸与纹理尺寸几乎从不相等的**：
// 512² 纹理要显示成 320pt（100% DPI 下 320px）或 696pt 大图，都是
// 1.6~2.4× 的**缩小**。nearest 采样在非 1:1 比例下按纹素抽点，
// 照片细节直接变成摩尔纹/锯齿（实测）。
// 字体图集正相反——它必须 nearest（小字号线性会糊），故两者各一个
// sampler 绑定，见 `pipeline.rs::BINDING_USER_SAMPLER`。
//
// 帧槽集合里这个绑定写的是占位值（字体批次不采样 user_tex，
// 静态声明要求合法句柄即可）；真正的线性采样器写在每张用户纹理
// 自己的描述符集里（`frame.rs::allocate_user_texture_set`）。
@group(0) @binding(4) var user_sampler: sampler;

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

    // 直接输出 NDC（2026-10-17 起不再乘 clip_from_uv，见头注释）。
    out.clip_position = vec4<f32>(ndc_x, ndc_y, 0.0, 1.0);
    out.uv = in.uv;
    // egui 把颜色打包为 ABGR 小端序的 u32
    out.color = unpack4x8unorm(in.color);
    out.tex_id = in.tex_id;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // IMPORTANT: 必须用 `textureSampleLevel`（显式 LOD 0）而**不是** `textureSample`。
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
        // 字体图集是单通道覆盖率。顶点色是**预乘**的（egui 的 `Color32`
        // 语义：RGB 已经乘过 alpha），因此覆盖率必须乘到**全部四个通道**上，
        // 结果才是预乘色，才能交给 `ONE, ONE_MINUS_SRC_ALPHA` 混合。
        //
        // ERROR: 早前写成 `in.color * vec4<f32>(1.0, 1.0, 1.0, cov)`
        // ——只把 alpha 乘了覆盖率、RGB 没乘。文字恰好「看起来对」（因为
        // 文字顶点色通常不透明），但**形状**（矩形/圆角）走的是同一条
        // 分支且覆盖率恒为 1，配上当时错误的直通 alpha 混合，颜色被乘了
        // 两次 alpha，每条边都镶一圈深色。
        let cov = textureSampleLevel(font_tex, tex_sampler, in.uv, 0.0).r;
        return in.color * cov;
    }
    // 用户纹理（图片缩略图）是**直通 alpha**（PNG 解码结果），
    // 必须先预乘再交给预乘混合，否则半透明像素会偏亮。
    // 用独立的 `user_sampler`（linear）采样：显示尺寸 ≠ 纹理尺寸时
    // 最近邻会让照片出摩尔纹（见绑定处的长注释）。
    let c = textureSampleLevel(user_tex, user_sampler, in.uv, 0.0);
    return vec4<f32>(c.rgb * c.a, c.a);
}
