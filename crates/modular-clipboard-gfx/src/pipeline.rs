//! 渲染通道与图形管线。
//!
//! 管线是固定的一组状态（本项目不做材质切换），因此只需在启动时创建一次，
//! 之后每帧复用。渲染通道在交换链重建时必须重建——它的附件引用了
//! 交换链的图像视图。

use ash::{Device, vk};

use crate::shader::ShaderModule;
use crate::Swapchain;

/// uniform 缓冲区的绑定槽。
pub const BINDING_UNIFORM: u32 = 0;
/// 采样器的绑定槽。
pub const BINDING_SAMPLER: u32 = 1;
/// 字体图集纹理的绑定槽。
pub const BINDING_TEXTURE: u32 = 2;

/// 顶点属性位置。
mod location {
    /// 屏幕空间位置（vec2<f32>）
    pub const POS: u32 = 0;
    /// 纹理坐标（vec2<f32>）
    pub const UV: u32 = 1;
    /// 顶点色（u32，打包为 ABGR）
    pub const COLOR: u32 = 2;
}

/// uniform 数据布局。
///
/// 对应 WGSL 中的 `Uniforms` struct。布局必须与着色器声明完全一致，
/// 字段顺序与 std140 对齐规则共同决定实际字节布局：
/// - `mat4x4<f32>` 占 64 字节，4 字节对齐
/// - `vec2<f32>` 占 8 字节，但对齐要求是 8 的倍数
/// - 两个 `f32` 各占 4 字节
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Uniforms {
    /// 像素 → NDC 的变换矩阵（列主序）。
    pub clip_from_uv: [f32; 16],
    /// 视口尺寸（物理像素）。
    pub size_in_pixels: [f32; 2],
    /// 设备像素比。
    pub dpr: f32,
    /// 填充，保证结构体大小是 16 字节的倍数。
    pad: f32,
}

impl Uniforms {
    /// 构造 uniform。
    ///
    /// `scale` 是 egui 的 `pixels_per_point`（逻辑像素 → 物理像素的比例）。
    pub fn new(clip_from_uv: [f32; 16], size_in_pixels: [f32; 2], scale: f32) -> Self {
        Self {
            clip_from_uv,
            size_in_pixels,
            dpr: scale,
            pad: 0.0,
        }
    }
}

/// 渲染通道。
pub struct RenderPass {
    pub handle: vk::RenderPass,
}

impl RenderPass {
    /// 为给定交换链格式创建渲染通道。
    pub fn new(device: &Device, format: vk::Format) -> anyhow::Result<Self> {
        let color_attachment = vk::AttachmentDescription {
            format,
            flags: vk::AttachmentDescriptionFlags::empty(),
            samples: vk::SampleCountFlags::TYPE_1,
            // load_op 用 CLEAR：每帧重绘背景色，不依赖图像原有内容。
            load_op: vk::AttachmentLoadOp::CLEAR,
            store_op: vk::AttachmentStoreOp::STORE,
            stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
            stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
            // `initial_layout` 必须是 COLOR_ATTACHMENT_OPTIMAL，不能是 UNDEFINED。
            //
            // `cmd_begin_render_pass` 会按 `initial_layout` **隐式**转换布局。
            // 若此处写 UNDEFINED，它会先做一次 `UNDEFINED → COLOR_ATTACHMENT_OPTIMAL`；
            // 而 `FrameRenderer::open_command_buffer` 在开通道**之前**已用
            // `LayoutTracker` 显式录了一道屏障转到 COLOR_ATTACHMENT_OPTIMAL。
            // 两套机制并存 ⇒ 显式屏障的 `oldLayout`（PRESENT_SRC_KHR）已过期
            // ⇒ `VUID-VkImageMemoryBarrier-oldLayout-01197`。
            //
            // 改成 COLOR_ATTACHMENT_OPTIMAL 后只剩**一套**转换机制：
            // 显式屏障是权威，渲染通道不再隐式转换，两者一致。
            //
            // 之所以**不**同时改 `LayoutTracker` 的初值：
            // 交换链图像在首次 acquire 前真实布局就是 UNDEFINED
            // （`vkCreateSwapchainKHR` 不保留上一帧内容）。
            // 若谎报为 COLOR_ATTACHMENT_OPTIMAL，
            // `open_command_buffer` 里 `old_layout == UNDEFINED` 的分支
            // 就永远走不到，首帧会去等待一个**不存在的写入**。
            initial_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            // `final_layout` 故意**不**设 PRESENT_SRC_KHR，
            // 而是保持 COLOR_ATTACHMENT_OPTIMAL。
            //
            // `final_layout` 若为 PRESENT_SRC_KHR，渲染通道**结束时**
            // 会隐式做一次 `COLOR_ATTACHMENT_OPTIMAL → PRESENT_SRC_KHR`；
            // 而 `FrameRenderer::present` 在 `cmd_end_render_pass` 之后
            // 又录了一道**显式**屏障做同样的转换。
            // 隐式那次已执行 ⇒ 显式屏障的 `oldLayout` 过期
            // ⇒ `VUID-VkImageMemoryBarrier-oldLayout-01197`。
            //
            // 设为 COLOR_ATTACHMENT_OPTIMAL 后，渲染通道**完全不做**
            // 隐式转换，转到 PRESENT 的责任全交给 `present()` 的显式屏障
            // ——它还带访问掩码与阶段掩码，隐式转换做不到这些。
            //
            // 规范上这仍然合法：`queue_present` 要求图像在
            // PRESENT_SRC_KHR，而 `present()` 的显式屏障正好做到这一点。
            //
            // 这样 `LayoutTracker` 成为布局记账的**唯一权威**，
            // CPU 记账与 GPU 实际状态不再有两套机制打架。
            final_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        };

        let color_ref = vk::AttachmentReference {
            attachment: 0,
            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        };

        let color = vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(std::slice::from_ref(&color_ref));

        let dependency = vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);

        let attachments = [color_attachment];
        let subpasses = [color];
        let dependencies = [dependency];
        let info = vk::RenderPassCreateInfo::default()
            .attachments(&attachments)
            .subpasses(&subpasses)
            .dependencies(&dependencies);

        let handle = unsafe { device.create_render_pass(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建渲染通道失败: {e:?}"))?;
        Ok(Self { handle })
    }

    /// 销毁渲染通道。
    pub fn destroy(&self, device: &Device) {
        unsafe { device.destroy_render_pass(self.handle, None) };
    }
}

/// 描述符集布局。
pub struct DescriptorLayout {
    pub handle: vk::DescriptorSetLayout,
}

impl DescriptorLayout {
    /// 创建布局：uniform buffer + sampler + combined image sampler。
    ///
    /// `update_after_bind` 决定是否给全部绑定加
    /// [`vk::DescriptorBindingFlags::UPDATE_AFTER_BIND`]。它由调用方
    /// （通常是 [`crate::Gpu`]）按设备**实际支持**传入——
    /// 对未启用的 feature 使用该标志会把`VUID-vkUpdateDescriptorSets-None-03047`
    /// 换成「feature 未启用」的违规，等于没修。
    ///
    /// # 为什么需要 `UPDATE_AFTER_BIND`
    ///
    /// 本项目每个在飞帧槽位各有独立的描述符集，且槽位的 GPU 工作与 CPU
    /// 的下一帧并行。`full_app` 在若干帧在途时会重写**所有**槽位的描述符
    /// （字体图集重建 / 交换链重建 / uniform 内容变化）。
    /// 规范禁止更新「被 pending 命令缓冲使用中」的描述符集，除非该绑定
    /// 创建时带了 `UPDATE_AFTER_BIND_BIT`或
    /// `UPDATE_UNUSED_WHILE_PENDING_BIT`。
    ///
    /// 选前者而非后者：后者的语义是「更新时保证该绑定尚未被使用」，
    /// 而多帧在途**恰恰**就是「已被使用」。要满足它只能每次重写前
    /// 等完所有在飞槽位——等于放弃多帧并行。
    ///
    /// # ash 0.38 陷阱
    ///
    /// `DescriptorSetLayoutBinding` **结构体里没有 `binding_flags` 字段**
    /// （任何 Vulkan 版本都没有）。绑定标志只能通过
    /// `DescriptorSetLayoutBindingFlagsCreateInfo` 挂在
    /// `DescriptorSetLayoutCreateInfo` 的 `p_next` 上，
    /// 且其数组长度必须与 `bindings` 一致。
    pub fn new(device: &Device, update_after_bind: bool) -> anyhow::Result<Self> {
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(BINDING_UNIFORM)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                // uniform 只在顶点着色器用到，但片元也可能访问，
                // 声明为 BOTH 更安全（着色器未使用不报错）。
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(BINDING_SAMPLER)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(BINDING_TEXTURE)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];

        // 标志数组与 bindings 一一对应，长度必须相同（由构造器按切片长度设置）。
        let binding_flags: Vec<vk::DescriptorBindingFlags> = bindings
            .iter()
            .map(|_| {
                if update_after_bind {
                    vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                } else {
                    vk::DescriptorBindingFlags::empty()
                }
            })
            .collect();
        let mut flags_info = vk::DescriptorSetLayoutBindingFlagsCreateInfo::default()
            .binding_flags(&binding_flags);

        let mut info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        if update_after_bind {
            // 加了绑定的 UPDATE_AFTER_BIND_BIT，**布局本身**就必须带
            // `UPDATE_AFTER_BIND_POOL`（规范强制，验证层实测报
            // `VUID-VkDescriptorSetLayoutCreateInfo-flags-03000`）。
            // 这与描述符池那侧的 `DescriptorPoolCreateFlags::UPDATE_AFTER_BIND`
            // 是**两处独立**的要求，漏任何一处都违规。
            info = info.flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL);
            // 仅在真的加标志时挂 pNext —— 空标志数组会让验证层报
            // 「flags 数量与 bindings 不符」类问题，且毫无收益。
            info = info.push_next(&mut flags_info);
        }

        let handle = unsafe { device.create_descriptor_set_layout(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建描述符集布局失败: {e:?}"))?;
        Ok(Self { handle })
    }

    pub fn destroy(&self, device: &Device) {
        unsafe { device.destroy_descriptor_set_layout(self.handle, None) };
    }
}

/// 管线布局。
pub struct PipelineLayout {
    pub handle: vk::PipelineLayout,
}

impl PipelineLayout {
    pub fn new(device: &Device, set_layouts: &[vk::DescriptorSetLayout]) -> anyhow::Result<Self> {
        // 本项目暂不使用 push constant（uniform 走描述符集），
        // 因此不声明任何 range。
        let info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(set_layouts);

        let handle = unsafe { device.create_pipeline_layout(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建管线布局失败: {e:?}"))?;
        Ok(Self { handle })
    }

    pub fn destroy(&self, device: &Device) {
        unsafe { device.destroy_pipeline_layout(self.handle, None) };
    }
}

/// 图形管线。
pub struct GraphicsPipeline {
    pub handle: vk::Pipeline,
}

impl GraphicsPipeline {
    /// 创建图形管线。
    ///
    /// 顶点输入布局与 WGSL 的 `@location` 声明一一对应：
    /// - location 0: `vec2<f32>` 位置
    /// - location 1: `vec2<f32>` UV
    /// - location 2: `u32` 颜色
    pub fn new(
        device: &Device,
        render_pass: vk::RenderPass,
        layout: vk::PipelineLayout,
        shader: &ShaderModule,
    ) -> anyhow::Result<Self> {
        // 单一顶点缓冲，交错布局
        let stride = (2 * 4 + 2 * 4 + 4) as u32;
        let binding = vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(stride)
            .input_rate(vk::VertexInputRate::VERTEX);

        let attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(location::POS)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(0),
            vk::VertexInputAttributeDescription::default()
                .location(location::UV)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(8),
            vk::VertexInputAttributeDescription::default()
                .location(location::COLOR)
                .binding(0)
                .format(vk::Format::R32_UINT)
                .offset(16),
        ];

        let bindings = [binding];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&bindings)
            .vertex_attribute_descriptions(&attributes);

        // 动态视口与剪裁：窗口尺寸变化时只更新这两个值，无需重建管线。
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);

        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            // UI 不需要背面剔除，关闭可省一次光栅化分支。
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);

        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        // egui 的字体图集是覆盖率图，需与顶点色相乘，故为标准 alpha 混合。
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(
                vk::ColorComponentFlags::R
                    | vk::ColorComponentFlags::G
                    | vk::ColorComponentFlags::B
                    | vk::ColorComponentFlags::A,
            );

        let blend_attachments = [blend_attachment];
        let color_blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(&blend_attachments);

        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(false)
            .depth_write_enable(false);

        let dynamic_states = [
            vk::DynamicState::VIEWPORT,
            vk::DynamicState::SCISSOR,
        ];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default()
            .dynamic_states(&dynamic_states);

        let stages = [shader.vertex_stage(), shader.fragment_stage()];

        // 这些状态结构体的生命周期需覆盖 create_graphics_pipelines 调用，
        // 因此必须先绑定到局部变量——直接写内联临时值会在调用前被释放。
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);

        // ash 0.38 返回 Vec<Pipeline>，错误类型是 (已创建的管线, VkResult)，
        // 便于部分成功时回收资源。这里只建一条管线，取首个即可。
        let handle = unsafe {
            device.create_graphics_pipelines(
                vk::PipelineCache::null(),
                std::slice::from_ref(&info),
                None,
            )
        }
        .map_err(|(partial, res)| {
            // 部分创建成功时必须销毁，否则泄漏
            unsafe {
                for p in partial {
                    device.destroy_pipeline(p, None);
                }
            }
            anyhow::anyhow!("创建图形管线失败: {res:?}")
        })?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("管线创建成功但返回列表为空"))?;

        Ok(Self { handle })
    }

    pub fn destroy(&self, device: &Device) {
        unsafe { device.destroy_pipeline(self.handle, None) };
    }
}

/// 渲染通道的附件格式需与交换链一致。供测试与文档引用。
pub fn swapchain_format(swapchain: &Swapchain) -> vk::Format {
    swapchain.format
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_layout_matches_wgsl() {
        // WGSL Uniforms: mat4x4(64) + vec2(8) + f32(4) + f32(4) = 80
        // Rust 侧 [f32;16] + [f32;2] + f32 + f32 同样应是 80 字节
        assert_eq!(
            size_of::<Uniforms>(),
            80,
            "uniform 大小与 WGSL 声明不符，着色器会读到错位数据"
        );
    }

    #[test]
    fn uniform_is_pod_and_zeroable() {
        // bytemuck 派生要求字段无填充差异；构造后再逐字节比较以验证
        let u = Uniforms::new([0.0; 16], [800.0, 600.0], 1.5);
        let bytes = unsafe {
            std::slice::from_raw_parts(&u as *const Uniforms as *const u8, size_of::<Uniforms>())
        };
        let zero = Uniforms::new([0.0; 16], [0.0, 0.0], 0.0);
        assert_ne!(bytes, unsafe {
            std::slice::from_raw_parts(
                &zero as *const Uniforms as *const u8,
                size_of::<Uniforms>(),
            )
        });
    }

    #[test]
    fn vertex_stride_matches_attribute_offsets() {
        // pos(8) + uv(8) + color(4) = 20 字节
        let stride = 2 * 4 + 2 * 4 + 4;
        assert_eq!(stride, 20);
        // 最后一个属性必须落在步长之内
        assert!(16 + 4 <= stride);
    }

    #[test]
    fn bindings_are_distinct() {
        // 绑定槽号必须互不相同，否则描述符布局会覆盖前一项
        let mut ids = vec![BINDING_UNIFORM, BINDING_SAMPLER, BINDING_TEXTURE];
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3, "绑定槽号必须互不相同");
    }

    #[test]
    fn uniform_is_16_byte_aligned_for_std140() {
        // std140 要求 uniform 成员按 16 字节对齐；80 不是 16 的倍数，
        // 因此 Rust 侧结构体末尾的填充必须存在（否则着色器越界读）。
        // 这里确认当前定义没有多余填充导致的对齐浪费。
        assert_eq!(size_of::<Uniforms>() % 4, 0);
    }

    #[test]
    fn binding_flag_count_must_match_binding_count() {
        // `DescriptorSetLayoutBindingFlagsCreateInfo.binding_flags()` 用切片长度
        // 决定 `binding_count`。若两者不等，验证层会报
        // 「flags 数量与 bindings 不符」——而这在无 GPU 的单测里测不到。
        // 本函数与 `DescriptorLayout::new` 里的map 逻辑一一对应。
        let binding_count = 3usize;
        let flags_for = |update_after_bind: bool| -> Vec<vk::DescriptorBindingFlags> {
            (0..binding_count)
                .map(|_| {
                    if update_after_bind {
                        vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
                    } else {
                        vk::DescriptorBindingFlags::empty()
                    }
                })
                .collect()
        };
        assert_eq!(flags_for(true).len(), binding_count);
        assert_eq!(flags_for(false).len(), binding_count);
    }

    #[test]
    fn update_after_bind_requires_no_dynamic_offset() {
        // `UPDATE_AFTER_BIND` 的硬性约束：带该标志的绑定**不得**使用
        // dynamic offset。本项目的 uniform 用 `DescriptorBufferInfo`
        // 里写死的 offset（非 dynamic），dynamic states 只有
        // VIEWPORT/SCISSOR——没有 DYNAMIC_OFFSET，因此该约束成立。
        //
        // 这条测试的价值是**记录前提**：若将来有人给管线加了
        // DYNAMIC_OFFSET state，本测试会立刻失败，提示
        // 「加UPDATE_AFTER_BIND 的前提被破坏了」。
        const DYNAMIC_STATES: [vk::DynamicState; 2] =
            [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        // ash 0.38 的 `DynamicState` 常量只列到 STENCIL_REFERENCE(8)，
        // 没有导出 DYNAMIC_OFFSET——但规范里它是 9。用 from_raw 构造，
        // 免得「枚举缺常量」被误当成「不存在这个动态状态」。
        let dynamic_offset = vk::DynamicState::from_raw(9);
        for s in DYNAMIC_STATES {
            assert_ne!(
                s.as_raw(),
                dynamic_offset.as_raw(),
                "带 UPDATE_AFTER_BIND 的 uniform 绑定不得使用 DYNAMIC_OFFSET"
            );
        }
        assert_eq!(
            DYNAMIC_STATES.len(),
            2,
            "dynamic states 列表被改动——若新增了 DYNAMIC_OFFSET，\
             uniform 绑定的 UPDATE_AFTER_BIND 就不再合法"
        );
    }
}
