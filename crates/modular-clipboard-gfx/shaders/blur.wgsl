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
//
// # 合成时的 UV 位移（push constant）
//
// 自研磨砂的底图是**上一次捕获**的桌面快照：窗口拖动时，捕获发生在
// 位置 P_cap，而帧呈现时窗口已经到 P_now——底图内容相对窗口「过期」
// 了一段位移 d = P_now - P_cap，表现为磨砂背景跟不上窗口（拖影）。
// 合成阶段用 push constant 把采样坐标平移 d/尺寸（目标 UV 单位），
// 快照即与窗口当前所在位置对齐；再叠加一帧**预测**位移（速度×帧
// 间隔），补偿「提交 → 呈现」之间窗口又走过的距离。位移为零时与
// 旧行为完全一致（其它拷贝用途恒推零）。
//
// 为什么用 push constant 而不是 uniform：这是**每次绘制**变的参数，
// push constant 正是 Vulkan 为此设计的通道；写 uniform 要多一块缓冲
// 和一次描述符写入，还会与模糊共用的描述符布局纠缠不清。

struct BlurUniforms {
    // 采样方向与步长（**纹素**单位）：横向为 (1/w, 0)，纵向为 (0, 1/h)。
    dir: vec2<f32>,
    // 高斯半径（整数，CPU 端已 clamp 到 MAX_BLUR_RADIUS）。
    radius: f32,
    // 高斯标准差。
    sigma: f32,
};

// 合成拷贝的 UV 位移（目标 UV 单位，见模块头「合成时的 UV 位移」）。
// 高斯卷积不读它，只有 `fs_copy` 用；离屏拷贝（降采样等）每次绘制前
// 由 CPU 端推零。
struct CopyOffset {
    offset: vec2<f32>,
};

var<push_constant> copy_pc: CopyOffset;

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
    // 高斯核：g(k) = exp(-k² / (2σ²))。常数项被归一化约掉，故省略。
    //
    // # 2026-10-17 优化：相邻 tap 双线性合并（仅宽核）
    //
    // 朴素写法是「逐个 tap 采样」：2r+1 次纹理采样 + 2r+1 次 exp
    // （默认 r=8 ⇒ 17/17）。模糊 pass 是**内存带宽瓶颈**，采样次数
    // 几乎线性决定耗时。把相隔 1 纹素的两次 tap 合并为一次采样，
    // 采样 17 → 9（r=8）、exp 不变（权重仍逐个精确计算）。
    //
    // 1. **合并的依据**：线性采样器取「两个相邻纹素正中间」时，结果
    //    恰好是两者的 50/50 平均（CLAMP_TO_EDGE 在边缘同样成立），
    //    于是 tap (i, i+1) 可用一次中点采样表示，权重取两权**精确
    //    之和**——权重不近似，代价只剩中点采样的边缘误差（见下）。
    // 2. **只在宽核下合并**（σ ≥ 2.5，本项目 σ = r/3 ⇒ r ≥ 8）：
    //    合并误差与**局部梯度**成正比，核越宽误差越小。窄核逐 tap
    //    精采——低强度档一共也没几个 tap，省不下什么。
    //    实测最坏信号（1 纹素棋盘格）偏差：r=6 ⇒ 0.10、r=8 ⇒ 0.074、
    //    r=12 ⇒ 0.049；阈值 2.5 把可见区间排除在合并之外。
    //    `blur.rs` 有黄金模型测试逐半径量化该偏差。
    //
    // # 唯一的近似：与局部梯度成正比的边缘误差
    //
    // 一对的贡献 = (g_i + g_{i+1})·(T_i + T_{i+1})/2，逐 tap 则为
    // g_i·T_i + g_{i+1}·T_{i+1}。两者之差恒等于
    //     (g_{i+1} − g_i)/2 · (T_{i+1} − T_i)
    // ——只与**相邻纹素的差**（局部梯度）成正比，平坦区域恒为零。
    // 权重取两权精确之和（不近似），因此不存在核收窄。
    // r=8 实测：相邻差 0.35 的高频信号偏差 0.0067（1.7/255），
    // 相邻差 0.08 的真实桌面内容 < 0.4/255；磨砂本身也是要抹掉
    // 细节 ⇒ 不可辨。这是 Unity/Unreal bloom 与各类高斯实现沿用的
    // 标准做法。
    //
    // 若将来要严格逐 tap：把下方 `let merge = s >= 2.5` 改为
    // `let merge = false` 即可（一处）。
    //
    // IMPORTANT: 采样必须用 `textureSampleLevel`（显式 LOD 0）而非
    // `textureSample`。后者的隐式 LOD 需要屏幕空间导数，而 WGSL 要求
    // 它处于 uniform control flow 中——循环体内的调用不是。
    // 非 uniform 控制流里用 `textureSample` 是未定义行为，且验证层不报。

    let r = i32(u.radius);
    let s = max(u.sigma, 0.5);
    let s2 = s * s;

    // 是否启用相邻合并。σ ≥ 2.5（r ≥ 8）才值得付那点边缘误差。
    let merge = s >= 2.5;

    // 中心 tap（k = 0）。
    var acc = textureSampleLevel(src, blur_sampler, in.uv, 0.0);
    var weight_sum = 1.0;

    // 其余 tap 成对处理。合并时 i 每次前进 2（一对一次采样打 ± 两侧），
    // 否则逐 tap 前进 1。
    var i = 1;
    loop {
        if (i > r) {
            break;
        }
        let fi = f32(i);
        let w = exp(-0.5 * fi * fi / s2);
        if (merge && i + 1 <= r) {
            // 成对：权重 = g(i) + g(i+1)，采样点取两纹素中点
            // （i + 0.5），线性过滤器自动给出 50/50 平均。
            let wm = w + exp(-0.5 * (fi + 1.0) * (fi + 1.0) / s2);
            let off = u.dir * (fi + 0.5);
            let t = textureSampleLevel(src, blur_sampler, in.uv + off, 0.0)
                  + textureSampleLevel(src, blur_sampler, in.uv - off, 0.0);
            acc = acc + t * wm;
            weight_sum = weight_sum + 2.0 * wm;
            i = i + 2;
        } else {
            // 逐 tap（低强度档，或奇数半径多出来的最后一个）。
            let off = u.dir * fi;
            let t = textureSampleLevel(src, blur_sampler, in.uv + off, 0.0)
                  + textureSampleLevel(src, blur_sampler, in.uv - off, 0.0);
            acc = acc + t * w;
            weight_sum = weight_sum + 2.0 * w;
            i = i + 1;
        }
    }

    // 归一化放在着色器里：CPU 端不必逐权重归一，也不怕浮点累积误差。
    return acc / weight_sum;
}

@fragment
fn fs_copy(in: VertexOutput) -> @location(0) vec4<f32> {
    // 降采样与最终合成都走这里。降采样时源是全分辨率场景、
    // 目标是 1/N 分辨率，线性采样器自动做一次双线性平均，
    // 等价于一次 2x2 的盒式滤波（N=2 时精确，N=4 时近似）。
    //
    // `copy_pc.offset`：只有自研磨砂的**合成**用非零值——把过期
    // 快照按窗口位移平移回当前位置（见模块头「合成时的 UV 位移」）。
    // 离屏拷贝恒为零，采样点退回 in.uv，与加 push constant 前一致。
    // CLAMP_TO_EDGE 采样器会把越界坐标钳到边缘：快速拖动时 trailing
    // 边缘的一小条会被拉伸，而模糊半径远大于该宽度，观感无差别。
    return textureSampleLevel(src, blur_sampler, in.uv + copy_pc.offset, 0.0);
}
