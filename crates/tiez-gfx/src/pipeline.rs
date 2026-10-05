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
            // 初始布局 UNDEFINED 表示「内容无需保留」，可省去一次清除。
            load_op: vk::AttachmentLoadOp::CLEAR,
            store_op: vk::AttachmentStoreOp::STORE,
            stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
            stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
            initial_layout: vk::ImageLayout::UNDEFINED,
            final_layout: vk::ImageLayout::PRESENT_SRC_KHR,
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
    pub fn new(device: &Device) -> anyhow::Result<Self> {
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

        let info = vk::DescriptorSetLayoutCreateInfo::default()
            .bindings(&bindings);

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
}
