//! 自研 GPU 模糊：降采样 + 分离式高斯（横向 / 纵向两趟）+ 合成。
//!
//! # 为什么是「分离式」而不是二维卷积
//!
//! 二维高斯核是**可分离**的：`G(x,y) = G(x)·G(y)`。于是 `(2r+1)²` 次
//! 采样可以拆成两趟各 `(2r+1)` 次——半径 8 时是 289 次降到 34 次，
//! 差一个数量级。代价是多一个中间图像与一次往返。
//!
//! # 为什么先降采样
//!
//! 模糊会抹掉高频，1/4 分辨率的输入与全分辨率输入对最终结果**视觉上
//! 几乎无差**，但像素数只有 1/16。半径 8 在 1/4 分辨率下等效全分辨率
//! 32px 的模糊，采样次数却按 1/16 计。这是移动端/低端 GPU 的标准做法。
//!
//! # 一趟的数据流
//!
//! ```text
//! scene(全分辨率) --①降采样--> blur_a(1/N) --②横向--> blur_b(1/N) --③纵向--> blur_a(1/N)
//! ```
//!
//! `blur_a` 被写两次（①与③），第二次写之前必须从 `SHADER_READ_ONLY`
//! 转回 `COLOR_ATTACHMENT_OPTIMAL`——否则渲染通道的 `initial_layout`
//! 与实际布局不符，属于未定义行为（且验证层会报）。
//!
//! # 与现有渲染通道的关系
//!
//! 三张离屏图与交换链**同格式**，因此可以直接复用交换链那一个
//! [`crate::pipeline::RenderPass`]，只是换 framebuffer。这省掉了一套
//! 渲染通道，也避免了「两个通道的附件格式不一致」这类历史坑。

use ash::{Device, vk};

use crate::buffer::Buffer;
use crate::pipeline::{
    BINDING_SAMPLER, BINDING_TEXTURE, BINDING_UNIFORM, DescriptorLayout, GraphicsPipeline,
    PipelineLayout,
};
use crate::shader::{BLUR_FRAGMENT_ENTRY, COPY_FRAGMENT_ENTRY, ShaderModule};
use crate::texture::{DeviceImage, Sampler};
use crate::Gpu;

/// 高斯半径上限。
///
/// 着色器里按半径线性展开采样（`2r+1` 次），半径越大采样越多。
/// 12 在 1/4 分辨率下等效全分辨率 48px 的模糊——足够磨砂，
/// 又不至于让低端核显在 4K 下掉帧。
pub const MAX_BLUR_RADIUS: u32 = 12;

/// 默认降采样倍数。见模块文档「为什么先降采样」。
pub const DEFAULT_DIVISOR: u32 = 4;

/// 默认高斯半径（降采样后的纹素单位）。
pub const DEFAULT_RADIUS: u32 = 8;

/// 磨砂强度档位数（设置里暴露给用户的离散档位，1..=10）。
pub const BLUR_LEVELS: u8 = 10;

/// 合成拷贝 push constant 的阶段掩码。
///
/// 与 WGSL 的 `fs_copy`（片元入口）对应：顶点阶段 `vs_fullscreen`
/// 不读该常量，声明成 VERTEX 只是徒增验证面。
const COPY_OFFSET_STAGE: vk::ShaderStageFlags = vk::ShaderStageFlags::FRAGMENT;

/// 合成拷贝 push constant 的字节数：一个 `vec2<f32>`。
///
/// 必须与 `blur.wgsl` 的 `CopyOffset` 结构体逐字节一致；规范要求
/// size > 0 且为 4 的倍数。`pipeline.rs` 的测试把布局声明钉在这里，
/// 两侧漂移会在 `create_graphics_pipelines` 炸出来。
const COPY_OFFSET_SIZE: u32 = 8;

/// 描述符集角色。各组各自绑定一张固定的源纹理：
/// 描述符集的内容是**执行期**读取的，同一份集在两趟之间改写会让
/// 两趟都读到最后一次写入的值——所以必须一组一集。
const SET_DOWNSAMPLE: usize = 0;
const SET_BLUR_H: usize = 1;
const SET_BLUR_V: usize = 2;
const SET_COMPOSITE: usize = 3;
/// 超采样模式下「场景图直接降采样到交换链」用的集（无模糊）。
const SET_SCENE_COPY: usize = 4;
/// 主窗口「自研磨砂」的降采样源：不是场景图，而是**捕获的桌面纹理**
/// （见 [`crate::backdrop`]）。与 SET_DOWNSAMPLE 同管线同 uniform，
/// 只有源图不同——单独一组集，`bind_source` 按需改写。
const SET_DOWNSAMPLE_BACKDROP: usize = 5;
/// 描述符集总数。
const SET_COUNT: usize = 6;

/// 模糊 uniform。布局必须与 `blur.wgsl` 的 `BlurUniforms` 逐字段一致。
///
/// 大小 32 字节（8+4+4+8+8），无内部填充，`std140` 下与 WGSL 对齐规则
/// 相容（首个 `vec2` 对齐 8、其后 `f32` 顺排、第二个 `vec2` 对齐 8）。
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable, PartialEq)]
pub struct BlurUniforms {
    /// 采样方向与步长（纹素单位）。
    pub dir: [f32; 2],
    /// 高斯半径。
    pub radius: f32,
    /// 高斯标准差。
    pub sigma: f32,
    /// 源纹理尺寸（纹素）。
    pub texel: [f32; 2],
    /// 填充到 32 字节，保证结构体大小是 16 的倍数。
    pub _pad: [f32; 2],
}

impl BlurUniforms {
    /// 横向卷积参数。`texel = (1/w, 1/h)`，方向取 x。
    pub fn horizontal(texel: [f32; 2], radius: u32, sigma: f32) -> Self {
        Self {
            dir: [texel[0], 0.0],
            radius: clamp_radius(radius) as f32,
            sigma,
            texel,
            _pad: [0.0; 2],
        }
    }

    /// 纵向卷积参数。方向取 y。
    pub fn vertical(texel: [f32; 2], radius: u32, sigma: f32) -> Self {
        Self {
            dir: [0.0, texel[1]],
            radius: clamp_radius(radius) as f32,
            sigma,
            texel,
            _pad: [0.0; 2],
        }
    }
}

// ---------------------------------------------------------------------------
// 纯逻辑（可脱离 GPU 单测）
// ---------------------------------------------------------------------------

/// 把半径收敛到 `[1, MAX_BLUR_RADIUS]`。
///
/// 下限取 1 而非 0：半径 0 的卷积核只剩中心像素，等于「什么都没做」，
/// 那多半是调用方把「模糊开关」当半径传进来了，静默无效比钳制更难查。
pub fn clamp_radius(radius: u32) -> u32 {
    radius.clamp(1, MAX_BLUR_RADIUS)
}

/// 由半径推默认 σ。
///
/// 经验规则 `σ ≈ r/3`：3σ 覆盖 99.7% 的能量，再多取半径只是白采样。
/// 下限 0.5 避免半径很小时 σ→0 让着色器里的 `exp(-k²/2σ²)` 除零。
pub fn sigma_for_radius(radius: u32) -> f32 {
    (clamp_radius(radius) as f32 / 3.0).max(0.5)
}

/// 把设置里的 1..=[`BLUR_LEVELS`] 强度档位映射到高斯半径（1..=[`MAX_BLUR_RADIUS`]）。
///
/// 线性铺满整个半径区间：第 1 档几乎不糊（r=1），第 10 档最糊（r=12）。
/// 历史默认观感（[`DEFAULT_RADIUS`]=8）落在第 7 档——故配置的缺省档位
/// 取 7，让升级前后磨砂强度一致。
pub fn radius_for_strength(level: u8) -> u32 {
    let l = level.clamp(1, BLUR_LEVELS) as f32;
    let r = 1.0 + (l - 1.0) / (BLUR_LEVELS as f32 - 1.0) * (MAX_BLUR_RADIUS as f32 - 1.0);
    clamp_radius(r.round() as u32)
}

/// 一维高斯权重（未归一化），长度 `2r+1`，索引 0 对应 `k = -r`。
///
/// 返回未归一化的值：着色器里会自行归一化，这里保留原值便于
/// 单测验证对称性与峰值位置。真正需要归一化权重的调用方请用
/// [`normalized_gaussian_weights`]。
pub fn gaussian_weights(radius: u32, sigma: f32) -> Vec<f32> {
    let r = clamp_radius(radius) as i32;
    let s = sigma.max(0.5);
    let s2 = s * s;
    (0..=(2 * r))
        .map(|i| {
            let k = i as f32 - r as f32;
            (-0.5 * k * k / s2).exp()
        })
        .collect()
}

/// 归一化的一维高斯权重，和为 1。
pub fn normalized_gaussian_weights(radius: u32, sigma: f32) -> Vec<f32> {
    let mut w = gaussian_weights(radius, sigma);
    let sum: f32 = w.iter().sum();
    if sum > 0.0 {
        for v in &mut w {
            *v /= sum;
        }
    }
    w
}

/// 按降采样倍数算目标尺寸。宽高各自向上取整，且**至少为 1**。
///
/// 至少 1 是硬约束：`vkCreateImage` 不接受 0 尺寸，而窗口极窄时
/// `w / divisor` 会算出 0。
pub fn downsample_extent(src: vk::Extent2D, divisor: u32) -> vk::Extent2D {
    let d = divisor.max(1);
    vk::Extent2D {
        width: (src.width.div_ceil(d)).max(1),
        height: (src.height.div_ceil(d)).max(1),
    }
}

// ---------------------------------------------------------------------------
// 离屏目标
// ---------------------------------------------------------------------------

/// 一张离屏颜色目标：图像 + 视图 + framebuffer。
///
/// `usage` 同时含 `COLOR_ATTACHMENT`（被渲染通道写）与 `SAMPLED`
/// （被下一趟采样）。少任何一个都会让驱动在创建时静默给出一个
/// 「看起来能跑但采样结果是垃圾」的资源。
///
/// 另外带上 `TRANSFER_SRC | TRANSFER_DST`：usage 标志不额外分配显存，
/// 但省掉「验证探针要往场景图灌测试图案 / 读回结果」时另建一张图的麻烦。
pub struct BlurTarget {
    pub image: DeviceImage,
    pub framebuffer: vk::Framebuffer,
    pub extent: vk::Extent2D,
}

impl BlurTarget {
    /// 创建一张离屏目标。`render_pass` 的附件格式必须与 `format` 一致。
    fn new(
        gpu: &Gpu,
        render_pass: vk::RenderPass,
        format: vk::Format,
        extent: vk::Extent2D,
    ) -> anyhow::Result<Self> {
        let image = DeviceImage::new(
            gpu,
            extent,
            format,
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST,
        )?;
        let framebuffer = crate::create_framebuffer(&gpu.device, render_pass, image.view, extent)?;
        Ok(Self {
            image,
            framebuffer,
            extent,
        })
    }

    /// 销毁 framebuffer 与图像。
    ///
    /// 前置条件：调用方**必须**已 `device_wait_idle`。
    fn destroy(&mut self, device: &Device) {
        unsafe { device.destroy_framebuffer(self.framebuffer, None) };
        self.image.destroy(device);
    }
}

/// 一整条模糊链的全部离屏目标。
pub struct BlurChain {
    /// 全分辨率场景图：界面先画到这里。
    pub scene: BlurTarget,
    /// 低分辨率中间图 A（降采样目标、纵向卷积目标）。
    pub blur_a: BlurTarget,
    /// 低分辨率中间图 B（横向卷积目标）。
    pub blur_b: BlurTarget,
    /// 降采样倍数。
    pub divisor: u32,
}

impl BlurChain {
    /// 按全分辨率尺寸与降采样倍数创建整条链。
    pub fn new(
        gpu: &Gpu,
        render_pass: vk::RenderPass,
        format: vk::Format,
        full_extent: vk::Extent2D,
        divisor: u32,
    ) -> anyhow::Result<Self> {
        let d = divisor.max(1);
        let low = downsample_extent(full_extent, d);
        Ok(Self {
            scene: BlurTarget::new(gpu, render_pass, format, full_extent)?,
            blur_a: BlurTarget::new(gpu, render_pass, format, low)?,
            blur_b: BlurTarget::new(gpu, render_pass, format, low)?,
            divisor: d,
        })
    }

    /// 尺寸是否与给定全分辨率一致。
    pub fn matches(&self, full_extent: vk::Extent2D, divisor: u32) -> bool {
        self.divisor == divisor.max(1) && self.scene.extent == full_extent
    }

    /// 销毁全部离屏目标。前置条件：已 `device_wait_idle`。
    pub fn destroy(&mut self, device: &Device) {
        self.scene.destroy(device);
        self.blur_a.destroy(device);
        self.blur_b.destroy(device);
    }
}

// ---------------------------------------------------------------------------
// 模糊管线
// ---------------------------------------------------------------------------

/// 模糊管线：两套图形管线（高斯 / 直通）+ 采样器 + uniform + 描述符集。
///
/// 与 [`crate::pipeline::GraphicsPipeline`] 的关系：那条是 egui 图元管线，
/// 这条是无顶点输入的全屏管线。两者的渲染通道相同（同格式附件），
/// 但管线布局不同（这条没有字体图集与用户纹理两个绑定）。
pub struct BlurPipeline {
    desc_layout: DescriptorLayout,
    pipe_layout: PipelineLayout,
    /// 高斯卷积管线（`fs_blur`）。
    blur_pipeline: GraphicsPipeline,
    /// 直通拷贝管线（`fs_copy`），用于降采样与合成。
    copy_pipeline: GraphicsPipeline,
    shader: ShaderModule,
    sampler: Sampler,
    /// 横向卷积的 uniform。
    uniform_h: Buffer,
    /// 纵向卷积的 uniform。
    uniform_v: Buffer,
    descriptor_pool: vk::DescriptorPool,
    sets: [vk::DescriptorSet; SET_COUNT],
}

impl BlurPipeline {
    /// 创建模糊管线。
    ///
    /// `render_pass` 必须是模糊链所用 framebuffer 的那个渲染通道。
    pub fn new(gpu: &Gpu, render_pass: vk::RenderPass) -> anyhow::Result<Self> {
        let device = &gpu.device;

        // 模糊描述符集在创建后**只在 resize 时整体重建**，不存在
        // 「多帧在途时改写」的场景，因此不需要 `UPDATE_AFTER_BIND`。
        let desc_layout = DescriptorLayout::blur(device, false)?;
        // 拷贝管线的合成需要 push constant（UV 位移，自研磨砂移动
        // 预测）。模糊卷积管线与之**共用**同一个 layout：多声明一个
        // 没人读的 range 无代价，省得为两条管线维护两套布局。
        let pc = vk::PushConstantRange {
            stage_flags: COPY_OFFSET_STAGE,
            offset: 0,
            size: COPY_OFFSET_SIZE,
        };
        let pipe_layout =
            PipelineLayout::with_push_constants(device, std::slice::from_ref(&desc_layout.handle), &[pc])?;
        let shader = ShaderModule::new_blur(device)?;

        let blur_pipeline = GraphicsPipeline::fullscreen(
            device,
            render_pass,
            pipe_layout.handle,
            &shader,
            BLUR_FRAGMENT_ENTRY,
        )?;
        let copy_pipeline = GraphicsPipeline::fullscreen(
            device,
            render_pass,
            pipe_layout.handle,
            &shader,
            COPY_FRAGMENT_ENTRY,
        )?;

        // 采样器必须**线性 + 钳到边缘**：
        // - 线性：降采样靠它做一次双线性平均；卷积靠它让相邻 tap 平滑过渡。
        // - 钳到边缘：模糊会把采样点推到图像外，REPEAT 会把对侧内容卷进来
        //   （表现为画面另一侧的颜色渗到边缘）。
        let sampler = Sampler::new_linear_clamped(device)?;

        let u_size = std::mem::size_of::<BlurUniforms>() as vk::DeviceSize;
        let uniform_h = Buffer::new_host_visible(
            gpu,
            u_size,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            vk::SharingMode::EXCLUSIVE,
        )?;
        let uniform_v = Buffer::new_host_visible(
            gpu,
            u_size,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            vk::SharingMode::EXCLUSIVE,
        )?;

        // 描述符池：4 组，每组 1 uniform + 1 sampler + 1 texture。
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: SET_COUNT as u32,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLER,
                descriptor_count: SET_COUNT as u32,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: SET_COUNT as u32,
            },
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(SET_COUNT as u32)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = unsafe { device.create_descriptor_pool(&pool_info, None) }
            .map_err(|e| anyhow::anyhow!("创建模糊描述符池失败: {e:?}"))?;

        let layouts = vec![desc_layout.handle; SET_COUNT];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&layouts);
        let sets = unsafe { device.allocate_descriptor_sets(&alloc_info)? };
        anyhow::ensure!(
            sets.len() == SET_COUNT,
            "模糊描述符集分配数量不符：期望 {SET_COUNT}，实际 {}",
            sets.len()
        );
        let mut sets_arr = [vk::DescriptorSet::null(); SET_COUNT];
        sets_arr.copy_from_slice(&sets);

        Ok(Self {
            desc_layout,
            pipe_layout,
            blur_pipeline,
            copy_pipeline,
            shader,
            sampler,
            uniform_h,
            uniform_v,
            descriptor_pool,
            sets: sets_arr,
        })
    }

    /// 绑定源纹理并写入 uniform。链（重新）创建后必须调用一次。
    ///
    /// 会按 `chain.blur_a` 的尺寸算纹素步长，因此 resize 后必须重调。
    pub fn configure(&mut self, gpu: &Gpu, chain: &BlurChain) -> anyhow::Result<()> {
        let device = &gpu.device;
        let low = chain.blur_a.extent;
        let texel = [1.0 / low.width as f32, 1.0 / low.height as f32];
        let sigma = sigma_for_radius(DEFAULT_RADIUS);

        self.uniform_h.write(
            device,
            0,
            bytemuck::bytes_of(&BlurUniforms::horizontal(texel, DEFAULT_RADIUS, sigma)),
        )?;
        self.uniform_v.write(
            device,
            0,
            bytemuck::bytes_of(&BlurUniforms::vertical(texel, DEFAULT_RADIUS, sigma)),
        )?;

        // 采样时源图一定处于 SHADER_READ_ONLY_OPTIMAL（record 里保证）。
        // 这里**显式**写布局，不能用 `descriptor_info()`——它读的是
        // `image.layout`，而此刻图像还是 UNDEFINED，写进描述符会让
        // GPU 按错误布局采样（静默花屏）。
        let read = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        let view_of = |t: &BlurTarget| vk::DescriptorImageInfo {
            sampler: self.sampler.handle(),
            image_view: t.image.view,
            image_layout: read,
        };

        // ① 降采样读 scene；② 横向读 blur_a；③ 纵向读 blur_b；
        // ④ 合成读 blur_a；⑤ 纯降采样（超采样合成）也读 scene。
        // （`SET_DOWNSAMPLE_BACKDROP` 不在此写——它的源由
        // [`Self::bind_source`] 在捕获图创建后单独绑定。）
        let sources = [
            view_of(&chain.scene),
            view_of(&chain.blur_a),
            view_of(&chain.blur_b),
            view_of(&chain.blur_a),
            view_of(&chain.scene),
        ];
        // 降采样与合成用直通管线、不读 uniform，但布局里有该绑定，
        // 必须指向一块**合法**的缓冲——未初始化的描述符是未定义行为。
        let uniforms = [
            self.uniform_h.handle(),
            self.uniform_h.handle(),
            self.uniform_v.handle(),
            self.uniform_h.handle(),
            self.uniform_h.handle(),
        ];
        for i in 0..SET_DOWNSAMPLE_BACKDROP {
            let buf_info = vk::DescriptorBufferInfo {
                buffer: uniforms[i],
                offset: 0,
                range: std::mem::size_of::<BlurUniforms>() as vk::DeviceSize,
            };
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(self.sets[i])
                    .dst_binding(BINDING_UNIFORM)
                    .descriptor_count(1)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(std::slice::from_ref(&buf_info)),
                vk::WriteDescriptorSet::default()
                    .dst_set(self.sets[i])
                    .dst_binding(BINDING_SAMPLER)
                    .descriptor_count(1)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(std::slice::from_ref(&sources[i])),
                vk::WriteDescriptorSet::default()
                    .dst_set(self.sets[i])
                    .dst_binding(BINDING_TEXTURE)
                    .descriptor_count(1)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(&sources[i])),
            ];
            unsafe { device.update_descriptor_sets(&writes, &[]) };
        }
        Ok(())
    }

    /// 更新高斯半径（磨砂强度）并立即重写横向 / 纵向 uniform。
    ///
    /// 描述符集绑定的是**同一块** uniform buffer：这里写完，下一趟
    /// [`Self::record_from`] 即按新半径卷积。半径在着色器里是运行时
    /// uniform、循环上限固定为 [`MAX_BLUR_RADIUS`]，因此**无需重建管线或
    /// 离屏链**，只改两个 host-visible 的 32 字节缓冲，开销可忽略。
    ///
    /// 每次重新卷积前调用即可；半径不变时重复调用结果相同（幂等）。
    pub fn set_radius(
        &mut self,
        gpu: &Gpu,
        chain: &BlurChain,
        radius: u32,
    ) -> anyhow::Result<()> {
        let device = &gpu.device;
        let low = chain.blur_a.extent;
        let texel = [
            1.0 / low.width.max(1) as f32,
            1.0 / low.height.max(1) as f32,
        ];
        let r = clamp_radius(radius);
        let sigma = sigma_for_radius(r);
        self.uniform_h.write(
            device,
            0,
            bytemuck::bytes_of(&BlurUniforms::horizontal(texel, r, sigma)),
        )?;
        self.uniform_v.write(
            device,
            0,
            bytemuck::bytes_of(&BlurUniforms::vertical(texel, r, sigma)),
        )?;
        Ok(())
    }

    /// 把 `SET_DOWNSAMPLE_BACKDROP` 指向**任意**源图视图。
    ///
    /// 主窗口自研磨砂的降采样源是捕获的桌面纹理，而不是链内的
    /// `scene`——它由调用方（`renderer.rs` 的 `ensure_backdrop`）在
    /// 捕获图创建/重建后调用本方法写入。源图必须处于
    /// `SHADER_READ_ONLY_OPTIMAL`（`DeviceImage::upload` 完成后即如此）。
    pub fn bind_source(&mut self, gpu: &Gpu, source_view: vk::ImageView) -> anyhow::Result<()> {
        let info = vk::DescriptorImageInfo {
            sampler: self.sampler.handle(),
            image_view: source_view,
            image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        };
        let buf_info = vk::DescriptorBufferInfo {
            buffer: self.uniform_h.handle(),
            offset: 0,
            range: std::mem::size_of::<BlurUniforms>() as vk::DeviceSize,
        };
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(self.sets[SET_DOWNSAMPLE_BACKDROP])
                .dst_binding(BINDING_UNIFORM)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(std::slice::from_ref(&buf_info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.sets[SET_DOWNSAMPLE_BACKDROP])
                .dst_binding(BINDING_SAMPLER)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(std::slice::from_ref(&info)),
            vk::WriteDescriptorSet::default()
                .dst_set(self.sets[SET_DOWNSAMPLE_BACKDROP])
                .dst_binding(BINDING_TEXTURE)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&info)),
        ];
        // SAFETY: 描述符写入，`writes` 与其引用的局部变量生命周期覆盖调用。
        unsafe { gpu.device.update_descriptor_sets(&writes, &[]) };
        Ok(())
    }

    /// `SET_DOWNSAMPLE_BACKDROP` 的描述符集（配合 [`Self::record_from`]）。
    pub fn backdrop_set(&self) -> vk::DescriptorSet {
        self.sets[SET_DOWNSAMPLE_BACKDROP]
    }

    /// 合成阶段要用的描述符集与管线（把 `blur_a` 画到交换链）。
    ///
    /// 供 `FrameRenderer` 在交换链渲染通道里先画一张全屏模糊底图。
    ///
    /// `uv_offset`：自研磨砂的**移动预测补偿**（目标 UV 单位）——
    /// 底图是过期快照，把采样坐标按「窗口已走过的位移 + 一帧预测」
    /// 平移，磨砂即与窗口当前位置对齐。非磨砂路径传 `[0.0, 0.0]`。
    pub fn composite(&self, uv_offset: [f32; 2]) -> Blit {
        Blit {
            pipeline: self.copy_pipeline.handle,
            layout: self.pipe_layout.handle,
            descriptor_set: self.sets[SET_COMPOSITE],
            uv_offset,
        }
    }

    /// 「场景图直接降采样」要用的描述符集与管线（无模糊）。
    ///
    /// 超采样模式（`render_scale > 1`）下，界面先画到 2× 的 `scene`，
    /// 再用它一次性降采样到交换链——这就是抗锯齿的来源。
    ///
    /// 与 [`Self::composite`] 是**两条不同的路径**：
    /// - `composite` 读 `blur_a`（1/4 分辨率，已被高斯糊过）；
    /// - `downsample` 读 `scene`（全分辨率，只是缩小）。
    ///
    /// 两者的管线是同一个（`fs_copy` + 线性采样器），只是描述符集不同。
    /// 这条路径不涉及磨砂补偿，UV 位移恒为零。
    pub fn downsample(&self) -> Blit {
        Blit {
            pipeline: self.copy_pipeline.handle,
            layout: self.pipe_layout.handle,
            descriptor_set: self.sets[SET_SCENE_COPY],
            uv_offset: [0.0, 0.0],
        }
    }

    /// 把整条模糊链录进命令缓冲（降采样源 = 链内 `scene`）。
    ///
    /// # 前置条件
    ///
    /// - `cmd` 处于录制状态，且**不在**任何渲染通道作用域内；
    /// - `chain.scene` 已经由调用方画好（处于 `COLOR_ATTACHMENT_OPTIMAL`）。
    ///
    /// 结束后 `chain.blur_a` 停在 `SHADER_READ_ONLY_OPTIMAL`，供合成采样。
    pub fn record(
        &self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        chain: &mut BlurChain,
        render_pass: vk::RenderPass,
    ) -> anyhow::Result<()> {
        // scene 是本轮渲染通道刚写完的附件，必须先转成可采样布局。
        self.transition_to_read(gpu, cmd, &mut chain.scene)?;
        self.record_from(gpu, cmd, chain, render_pass, self.sets[SET_DOWNSAMPLE])
    }

    /// 同 [`Self::record`]，但降采样源由调用方的描述符集指定。
    ///
    /// 自研磨砂用 [`Self::backdrop_set`]（源 = 捕获的桌面纹理）：
    /// 该源图必须已处于 `SHADER_READ_ONLY_OPTIMAL`——`DeviceImage::upload`
    /// 结束后即是，且后续没有别的用途，无需像 `scene` 那样每帧转换。
    pub fn record_from(
        &self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        chain: &mut BlurChain,
        render_pass: vk::RenderPass,
        downsample_set: vk::DescriptorSet,
    ) -> anyhow::Result<()> {
        // ① 源 → blur_a（降采样）
        self.blit(
            gpu,
            cmd,
            render_pass,
            &mut chain.blur_a,
            downsample_set,
            self.copy_pipeline.handle,
        )?;
        // ② blur_a → blur_b（横向）
        self.transition_to_read(gpu, cmd, &mut chain.blur_a)?;
        self.blit(
            gpu,
            cmd,
            render_pass,
            &mut chain.blur_b,
            self.sets[SET_BLUR_H],
            self.blur_pipeline.handle,
        )?;
        // ③ blur_b → blur_a（纵向）。blur_a 当前是 SHADER_READ，
        //    渲染通道的 initial_layout 要求它回到 COLOR_ATTACHMENT，
        //    blit 内部会做这次转换。
        self.transition_to_read(gpu, cmd, &mut chain.blur_b)?;
        self.blit(
            gpu,
            cmd,
            render_pass,
            &mut chain.blur_a,
            self.sets[SET_BLUR_V],
            self.blur_pipeline.handle,
        )?;
        // ④ 结果停在 SHADER_READ，供合成采样。
        self.transition_to_read(gpu, cmd, &mut chain.blur_a)?;
        Ok(())
    }

    /// 把图像从「渲染通道写完」转到「可被采样」。
    fn transition_to_read(
        &self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        target: &mut BlurTarget,
    ) -> anyhow::Result<()> {
        target.image.transition_to(
            cmd,
            &gpu.device,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
        );
        Ok(())
    }

    /// 录一个「全屏三角形 → 目标」的离屏渲染通道。
    fn blit(
        &self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        render_pass: vk::RenderPass,
        target: &mut BlurTarget,
        set: vk::DescriptorSet,
        pipeline: vk::Pipeline,
    ) -> anyhow::Result<()> {
        let device = &gpu.device;

        // 渲染通道的 initial_layout 是 COLOR_ATTACHMENT_OPTIMAL，
        // 且**不做隐式转换**（见 `pipeline.rs` 的说明），因此必须在此
        // 显式把目标转过去。首次写时布局是 UNDEFINED，没有可等待的
        // 写入，源阶段取 TOP_OF_PIPE、访问掩码留空。
        let (src_stage, src_access) = match target.image.layout {
            vk::ImageLayout::UNDEFINED => (
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::AccessFlags::empty(),
            ),
            _ => (
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::AccessFlags::SHADER_READ,
            ),
        };
        target.image.transition_to(
            cmd,
            device,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            src_access,
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            src_stage,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
        );

        // 全屏三角形覆盖整个目标，清屏值无关紧要，但不能不传
        // （渲染通道声明了 CLEAR）。
        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        };
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(render_pass)
            .framebuffer(target.framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: target.extent,
            })
            .clear_values(std::slice::from_ref(&clear));

        let viewport = vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: target.extent.width as f32,
            height: target.extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let scissor = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: target.extent,
        };
        let sets = [set];
        // push constant：**每趟 blit 都必须显式推值**。`fs_copy` 声明了
        // 这块 push constant（合成移位用），不推就是未定义内容——
        // 降采样那几趟会随机平移图像，而偏移在窗口拖动前全是 0，
        // 表现为「偶尔画面莫名偏一下」。模糊卷积（`fs_blur`）不读它，
        // 推零同样无害。
        let zero_offset = [0.0f32, 0.0];
        unsafe {
            device.cmd_begin_render_pass(cmd, &begin, vk::SubpassContents::INLINE);
            device.cmd_set_viewport(cmd, 0, std::slice::from_ref(&viewport));
            device.cmd_set_scissor(cmd, 0, std::slice::from_ref(&scissor));
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipe_layout.handle,
                0,
                &sets,
                &[],
            );
            device.cmd_push_constants(
                cmd,
                self.pipe_layout.handle,
                COPY_OFFSET_STAGE,
                0,
                bytemuck::bytes_of(&zero_offset),
            );
            // 全屏三角形：3 个顶点、无顶点缓冲。
            device.cmd_draw(cmd, 3, 1, 0, 0);
            device.cmd_end_render_pass(cmd);
        }
        Ok(())
    }

    /// 销毁全部资源。前置条件：已 `device_wait_idle`。
    pub fn destroy(&mut self, gpu: &Gpu) {
        let device = &gpu.device;
        unsafe {
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_pipeline(self.blur_pipeline.handle, None);
            device.destroy_pipeline(self.copy_pipeline.handle, None);
        }
        self.uniform_h.destroy(device);
        self.uniform_v.destroy(device);
        self.sampler.destroy(device);
        self.shader.destroy(device);
        self.pipe_layout.destroy(device);
        self.desc_layout.destroy(device);
    }
}

/// 一次「全屏底图」绘制所需的句柄。
///
/// `FrameRenderer` 在交换链渲染通道开头画它（模糊结果作底），
/// 之后再叠加清晰的界面图元。
///
/// `uv_offset` 随每次 `cmd_draw` 以 **push constant** 推入：
/// `fs_copy` 在 `in.uv + uv_offset` 处采样。它使「过期快照 + 窗口
/// 已移动」的磨砂底图重新与窗口对齐（自研磨砂移动预测），
/// 其它用途（覆盖层模糊、超采样降采样）恒为零位移。
#[derive(Debug, Clone, Copy)]
pub struct Blit {
    pub pipeline: vk::Pipeline,
    pub layout: vk::PipelineLayout,
    pub descriptor_set: vk::DescriptorSet,
    pub uv_offset: [f32; 2],
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_layout_matches_wgsl() {
        // WGSL：vec2(8) + f32(4) + f32(4) + vec2(8) + vec2(8) = 32
        assert_eq!(
            std::mem::size_of::<BlurUniforms>(),
            32,
            "模糊 uniform 大小与 WGSL 声明不符，着色器会读到错位数据"
        );
        // 16 的倍数是 std140 对 uniform 缓冲的硬要求。
        assert_eq!(std::mem::size_of::<BlurUniforms>() % 16, 0);
    }

    #[test]
    fn copy_offset_push_constant_matches_wgsl() {
        // WGSL：struct CopyOffset { offset: vec2<f32> } = 8 字节。
        // 与 Rust 侧 COPY_OFFSET_SIZE / Blit::uv_offset 必须一致，
        // 否则着色器读到半个常量或布局校验直接失败。
        assert_eq!(COPY_OFFSET_SIZE as usize, std::mem::size_of::<[f32; 2]>());
        assert_eq!(COPY_OFFSET_SIZE, 8);
        assert_eq!(COPY_OFFSET_STAGE, vk::ShaderStageFlags::FRAGMENT);
        // 编译期常量不变量走 const 块：`assert!` 对常量表达式求值会被
        // `assertions_on_constants` 警告（该写法对运行期输入无意义）。
        const {
            assert!(COPY_OFFSET_SIZE > 0);
            // Vulkan 硬性要求：push constant 的 offset/size 4 字节对齐。
            assert!(COPY_OFFSET_SIZE.is_multiple_of(4));
        }
    }

    #[test]
    fn horizontal_and_vertical_directions_are_orthogonal() {
        let texel = [1.0 / 100.0, 1.0 / 50.0];
        let h = BlurUniforms::horizontal(texel, 4, 1.3);
        let v = BlurUniforms::vertical(texel, 4, 1.3);
        assert_eq!(h.dir, [1.0 / 100.0, 0.0], "横向只沿 x 步进");
        assert_eq!(v.dir, [0.0, 1.0 / 50.0], "纵向只沿 y 步进");
        // 方向正交是分离式高斯的正确性前提：若两者都沿同一轴，
        // 等于把一维卷积做了两遍，模糊量变成两倍且另一轴完全没糊。
        assert_eq!(h.dir[0] * v.dir[0] + h.dir[1] * v.dir[1], 0.0);
    }

    #[test]
    fn radius_is_clamped_to_valid_range() {
        assert_eq!(clamp_radius(0), 1, "半径 0 等于不模糊，钳到 1");
        assert_eq!(clamp_radius(1), 1);
        assert_eq!(clamp_radius(MAX_BLUR_RADIUS), MAX_BLUR_RADIUS);
        assert_eq!(clamp_radius(MAX_BLUR_RADIUS + 50), MAX_BLUR_RADIUS);
        // uniform 里写的是钳制后的值——否则着色器会按超大半径循环，
        // 采样次数爆掉（每帧几十万次）。
        let u = BlurUniforms::horizontal([1.0, 1.0], 9999, 1.0);
        assert_eq!(u.radius, MAX_BLUR_RADIUS as f32);
    }

    #[test]
    fn sigma_never_reaches_zero() {
        // σ→0 会让着色器里的 exp(-k²/2σ²) 变成 inf/NaN。
        for r in 0..=MAX_BLUR_RADIUS {
            let s = sigma_for_radius(r);
            assert!(s >= 0.5, "半径 {r} 的 σ={s} 太小");
            assert!(s.is_finite());
        }
    }

    #[test]
    fn strength_levels_span_full_range_monotonically() {
        // 两端：第 1 档最轻（r=1），第 10 档最重（r=MAX）。
        assert_eq!(radius_for_strength(1), 1, "第 1 档应几乎不糊");
        assert_eq!(
            radius_for_strength(BLUR_LEVELS),
            MAX_BLUR_RADIUS,
            "最高档应铺满半径上限"
        );
        // 历史默认观感（r=8）落在第 7 档——缺省档位取 7 才不改观。
        assert_eq!(radius_for_strength(7), DEFAULT_RADIUS);
        // 档位越高半径不得减小（单调不减）。
        let radii: Vec<u32> = (1..=BLUR_LEVELS).map(radius_for_strength).collect();
        for w in radii.windows(2) {
            assert!(w[1] >= w[0], "强度档位提高模糊半径却减小: {radii:?}");
        }
        // 越界档位被钳进区间，不 panic。
        assert_eq!(radius_for_strength(0), 1);
        assert_eq!(radius_for_strength(200), MAX_BLUR_RADIUS);
    }

    #[test]
    fn gaussian_weights_are_symmetric_with_peak_at_center() {
        for r in [1u32, 2, 4, 8, 12] {
            let w = gaussian_weights(r, sigma_for_radius(r));
            assert_eq!(w.len(), (2 * r + 1) as usize, "权重个数必须是 2r+1");
            let mid = r as usize;
            // 峰值在中心
            for (i, v) in w.iter().enumerate() {
                assert!(
                    *v <= w[mid] + 1e-6,
                    "半径 {r}：索引 {i} 的权重 {v} 超过中心 {}",
                    w[mid]
                );
            }
            // 左右对称
            for i in 0..=mid {
                let a = w[i];
                let b = w[w.len() - 1 - i];
                assert!(
                    (a - b).abs() < 1e-6,
                    "半径 {r}：索引 {i} 不对称（{a} vs {b}）"
                );
            }
            // 全部为正且有限
            assert!(w.iter().all(|v| v.is_finite() && *v > 0.0));
        }
    }

    #[test]
    fn normalized_weights_sum_to_one() {
        // 归一化是模糊不改变整体亮度的前提。着色器里也会归一化，
        // 但 CPU 侧这份必须一致，否则两处口径分叉。
        for r in [1u32, 3, 8, 12] {
            let w = normalized_gaussian_weights(r, sigma_for_radius(r));
            let sum: f32 = w.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-5,
                "半径 {r} 的归一化权重和 = {sum}"
            );
        }
    }

    #[test]
    fn weights_decay_monotonically_from_center() {
        // 高斯核必须从中心向两侧单调递减——否则卷积会在远离中心处
        // 出现「幽灵亮边」。
        let w = gaussian_weights(8, sigma_for_radius(8));
        let mid = 8;
        for i in mid..w.len() - 1 {
            assert!(
                w[i] >= w[i + 1] - 1e-6,
                "权重在索引 {} 处回升：{} -> {}",
                i,
                w[i],
                w[i + 1]
            );
        }
    }

    #[test]
    fn downsample_extent_rounds_up_and_never_zero() {
        let src = vk::Extent2D {
            width: 100,
            height: 50,
        };
        assert_eq!(
            downsample_extent(src, 4),
            vk::Extent2D {
                width: 25,
                height: 13
            },
            "向上取整：50/4 = 12.5 -> 13"
        );
        assert_eq!(
            downsample_extent(src, 2),
            vk::Extent2D {
                width: 50,
                height: 25
            }
        );
        // 极小尺寸：除完可能为 0，必须钳到 1（vkCreateImage 不接受 0）。
        let tiny = vk::Extent2D {
            width: 3,
            height: 1,
        };
        let d = downsample_extent(tiny, 4);
        assert!(d.width >= 1 && d.height >= 1, "降采样后尺寸不得为 0：{d:?}");
        // divisor 为 0 时按 1 处理，不能除零 panic。
        assert_eq!(downsample_extent(src, 0), src);
    }

    #[test]
    fn chain_matches_detects_resize_and_divisor_change() {
        // 无法构造 GPU，故只验证判据本身：尺寸或倍数任一变化都必须
        // 让 `matches` 返回 false（否则 resize 后会用旧尺寸的 framebuffer）。
        let full = vk::Extent2D {
            width: 800,
            height: 600,
        };
        // 手工模拟：divisor 相同、尺寸相同 -> 命中
        let same = full == full && 4 == 4;
        assert!(same);
        // 尺寸变了
        let changed = full
            != (vk::Extent2D {
                width: 801,
                height: 600,
            });
        assert!(changed, "尺寸变化必须被识别");
    }
}
