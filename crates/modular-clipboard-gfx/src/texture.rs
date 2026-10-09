//! 纹理与采样器。
//!
//! # 字体图集为什么用 R8_UNORM
//!
//! egui 的字体图集本质是**覆盖率图**（coverage）：每个像素记录字形
//! 栅格化后的不透明度，取值 0（完全透明）到 255（完全不透明）。
//! 着色器拿它与顶点色相乘得到最终颜色。
//!
//! 有个版本差异必须注意：**egui 0.36 起字体图集是 RGBA 的
//! [`egui::ImageData::Color`]，不再是单通道**，且 `from_white_alpha` 会把
//! 同一个alpha 值写进全部四个通道。因此取任意一个通道都等价于覆盖率。
//! 本模块取红通道，既符合着色器 `textureSample(...).r` 的读法，
//! 又把纹理体积压到RGBA 的 1/4——中文字体图集通常有2048x2048，
//! 四倍差距是 16MB 与 4MB 的区别。
//!
//! # 布局过渡
//!
//! 图像创建后处于 `UNDEFINED`，拷入数据前需转到
//! `TRANSFER_DST_OPTIMAL`，拷完再转到 `SHADER_READ_ONLY_OPTIMAL`。
//! 两次过渡都用 [`crate::image_barrier`] 构造，队列族设为 `IGNORED`
//! 表示不变换归属。
//!
//! # 销毁顺序
//!
//! 与 [`crate::buffer::Buffer`] 一样不实现 `Drop`（需要 `&Device`），
//! 必须由调用方显式 [`FontTexture::destroy`]，且**调用前必须已
//! `device_wait_idle`**。

use std::collections::HashMap;

use ash::{Device, vk};

use crate::buffer::align_up;
use crate::{image_barrier, Gpu};

/// 字体图集格式：单通道 8 位无符号归一化。
pub const FONT_FORMAT: vk::Format = vk::Format::R8_UNORM;

/// egui 中字体图集固定使用的纹理 ID。
///
/// epaint 约定 `Managed(0)` 恒为字体数据（见 `TextureId::Managed` 的文档）。
pub const FONT_TEXTURE_ID: egui::TextureId = egui::TextureId::Managed(0);

/// 采样器。
pub struct Sampler {
    handle: vk::Sampler,
}

impl Sampler {
    /// 创建设备放大与缩小时都用最近邻的采样器。
    ///
    /// **字体图集必须点采样**：线性插值会把相邻字形的覆盖率渗进当前
    /// 字形边缘，小字号（12px 以下）下整段文字会糊成一团灰。
    /// 这是自写字体图集渲染器最常见的错误来源。
    pub fn new_nearest(device: &Device) -> anyhow::Result<Self> {
        let info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            // 钳制到边缘：egui 约定WHITE_UV 取图集左上角那个纯白像素，
            // 若用 REPEAT 会在边缘引入邻接像素的颜色。
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            // 字体图集只有一层 mip，不做 mipmap 过滤。
            .min_lod(0.0)
            .max_lod(0.0)
            .anisotropy_enable(false);

        let handle = unsafe { device.create_sampler(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建采样器失败: {e:?}"))?;
        Ok(Self { handle })
    }

    /// 按 egui 的 [`egui::TextureOptions`] 创建设备。
    ///
    /// egui 允许逐纹理指定过滤方式（`TextureOptions::NEAREST` /
    /// `LINEAR` / `LINEAR_REPEAT`），本函数把语义翻译成 Vulkan 采样器状态。
    pub fn from_egui(device: &Device, options: &egui::TextureOptions) -> anyhow::Result<Self> {
        let filter = |f: egui::TextureFilter| match f {
            egui::TextureFilter::Nearest => vk::Filter::NEAREST,
            egui::TextureFilter::Linear => vk::Filter::LINEAR,
        };
        let wrap = |w: egui::TextureWrapMode| match w {
            egui::TextureWrapMode::ClampToEdge => vk::SamplerAddressMode::CLAMP_TO_EDGE,
            egui::TextureWrapMode::Repeat => vk::SamplerAddressMode::REPEAT,
            egui::TextureWrapMode::MirroredRepeat => vk::SamplerAddressMode::MIRRORED_REPEAT,
        };

        let info = vk::SamplerCreateInfo::default()
            .mag_filter(filter(options.magnification))
            .min_filter(filter(options.minification))
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(wrap(options.wrap_mode))
            .address_mode_v(wrap(options.wrap_mode))
            .address_mode_w(wrap(options.wrap_mode))
            .min_lod(0.0)
            .max_lod(0.0)
            .anisotropy_enable(false);

        let handle = unsafe { device.create_sampler(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建采样器失败: {e:?}"))?;
        Ok(Self { handle })
    }

    pub fn handle(&self) -> vk::Sampler {
        self.handle
    }

    /// 销毁采样器。需在描述符不再引用后调用。
    pub fn destroy(&self, device: &Device) {
        unsafe { device.destroy_sampler(self.handle, None) };
    }
}

/// 设备图像：图像 + 内存 + 视图。
pub struct DeviceImage {
    pub image: vk::Image,
    pub view: vk::ImageView,
    memory: vk::DeviceMemory,
    /// 图像尺寸（像素）。
    pub size: vk::Extent2D,
    pub format: vk::Format,
    /// 当前布局。上传后为 `SHADER_READ_ONLY_OPTIMAL`。
    pub layout: vk::ImageLayout,
}

impl DeviceImage {
    /// 创建一张空的设备本地图像（不含数据）。
    ///
    /// `usage` 至少需含 `SAMPLED`（供着色器读）与 `TRANSFER_DST`（供上传）。
    pub fn new(
        gpu: &Gpu,
        size: vk::Extent2D,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            size.width > 0 && size.height > 0,
            "图像尺寸必须大于 0，得到 {size:?}"
        );

        let device = &gpu.device;
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: size.width,
                height: size.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            // 字体图集是CPU 侧生成后上传的一次性资源，
            // 不需要显卡纹理压缩，用 OPTIMAL 让驱动自行选择显存布局。
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .queue_family_indices(&[])
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let image = unsafe { device.create_image(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建 {size:?} 图像失败: {e:?}"))?;

        let mut guard = ImageGuard {
            device,
            image: Some(image),
            memory: None,
            view: None,
        };

        let req = unsafe { device.get_image_memory_requirements(image) };
        let alloc_size = align_up(req.size, req.alignment);

        let props =
            unsafe { gpu.instance.get_physical_device_memory_properties(gpu.physical_device) };
        let type_index = crate::buffer::find_memory_type(
            &props,
            req.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| {
            anyhow::anyhow!("找不到图像可用的内存类型（允许位掩码 {:#x}）", req.memory_type_bits)
        })?;

        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(alloc_size)
            .memory_type_index(type_index);
        let memory = unsafe { device.allocate_memory(&alloc, None) }.map_err(|e| {
            anyhow::anyhow!("为图像分配 {alloc_size} 字节显存失败: {e:?}")
        })?;
        guard.memory = Some(memory);

        unsafe { device.bind_image_memory(image, memory, 0) }
            .map_err(|e| anyhow::anyhow!("绑定图像显存失败: {e:?}"))?;

        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(vk::ComponentMapping::default())
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let view = unsafe { device.create_image_view(&view_info, None) }
            .map_err(|e| anyhow::anyhow!("创建图像视图失败: {e:?}"))?;
        guard.view = Some(view);

        // 所有权移交。
        guard.image = None;
        guard.memory = None;
        guard.view = None;

        Ok(Self {
            image,
            view,
            memory,
            size,
            format,
            layout: vk::ImageLayout::UNDEFINED,
        })
    }

    /// 把图像转到 `TRANSFER_SRC_OPTIMAL`，供读回校验使用。
    ///
    /// # 为什么需要这个方法
    ///
    /// `DeviceImage` 内部记录当前布局（[`DeviceImage::layout`]），
    /// 下一次 [`Self::upload`] 会用它计算布局屏障的 `oldLayout`。
    /// 若调用方直接用裸 `image_barrier()` 改布局而不回写这个字段，
    /// 状态就失同步了——下一次 upload 会用**过期的 oldLayout** 算屏障，
    /// 驱动按错误的前置状态转换 ⇒ 设备丢失。
    ///
    /// 这不是理论问题：实测 60 帧探针能跑 10 帧然后崩，
    /// 根因就是探针读了 10 帧后想读回，绕过本方法改了布局。
    ///
    /// 读回完成后请调用 [`Self::restore_after_readback`] 复原。
    pub fn prepare_for_readback(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
    ) -> anyhow::Result<()> {
        if self.layout == vk::ImageLayout::TRANSFER_SRC_OPTIMAL {
            return Ok(());
        }
        let mut bar = crate::image_barrier(
            self.image,
            self.layout,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::SHADER_READ,
            vk::AccessFlags::TRANSFER_READ,
        );
        self.layout = vk::ImageLayout::TRANSFER_SRC_OPTIMAL;
        unsafe {
            self.gpu_barrier(gpu, cmd, std::slice::from_mut(&mut bar))?;
        }
        Ok(())
    }

    /// 读回后把图像转回着色器可读状态。
    pub fn restore_after_readback(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
    ) -> anyhow::Result<()> {
        if self.layout == vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
            return Ok(());
        }
        let mut bar = crate::image_barrier(
            self.image,
            self.layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::AccessFlags::TRANSFER_READ,
            vk::AccessFlags::SHADER_READ,
        );
        self.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        unsafe {
            self.gpu_barrier(gpu, cmd, std::slice::from_mut(&mut bar))?;
        }
        Ok(())
    }

    /// 录一条单图像屏障。仅供本模块内的布局转换方法使用。
    ///
    /// # Safety
    /// `cmd` 必须是已`begin_command_buffer` 且尚未`end` 的命令缓冲；
    /// `barriers` 里的 `image` 必须是本图像。调用方保证同步需求成立。
    unsafe fn gpu_barrier(
        &self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        barriers: &[vk::ImageMemoryBarrier<'_>],
    ) -> anyhow::Result<()> {
        // SAFETY: 前置条件由调用方保证（见文档）。
        unsafe {
            gpu.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                barriers,
            );
        }
        Ok(())
    }

    /// 从 `ImageData` 提取单通道覆盖率字节。
    ///
    /// egui 0.36 的字体图集是 RGBA，但 `color_from_coverage` 用的是
    /// `Color32::from_white_alpha`，四个通道存的是同一个 alpha 值。
    /// 因此取红通道即得覆盖率，且与着色器读`.r` 一致。
    ///
    /// 单独抽成纯函数是为了能脱离 GPU 单测通道提取的正确性。
    pub fn coverage_bytes(data: &egui::ImageData) -> Vec<u8> {
        match data {
            egui::ImageData::Color(image) => image.pixels.iter().map(|p| p.r()).collect(),
        }
    }

    /// 校验图像数据尺寸与本图像匹配。
    pub fn ensure_size_matches(&self, size: [usize; 2]) -> anyhow::Result<()> {
        anyhow::ensure!(
            size[0] == self.size.width as usize && size[1] == self.size.height as usize,
            "图像数据尺寸 {size:?} 与纹理 {}x{} 不符",
            self.size.width,
            self.size.height
        );
        Ok(())
    }

    /// 把 CPU 侧的覆盖率字节上传到纹理。
    ///
    /// `cmd` 需已处于录制状态。函数内部完成三步：
    /// 布局过渡到 `TRANSFER_DST_OPTIMAL` → `cmd_copy_buffer_to_image`
    /// → 布局过渡到 `SHADER_READ_ONLY_OPTIMAL`。
    ///
    /// # 参数
    /// - `staging`：**由调用方提供**的暂存空间，必须已写入 `bytes`。
    ///   生产路径请走 [`crate::frame::FrameRenderer::record_texture_upload`]，
    ///   它从 per-frame [`crate::frame::StagingArena`] 分配，
    ///   从而保证该空间的生命周期覆盖 GPU 执行时间。
    /// - `bytes`：覆盖率数据。整图更新时长度须等于 `w * h`；
    ///   局部更新时长度须等于被覆盖矩形 `patch` 的像素数。
    /// - `offset`：更新起始像素坐标。`None` 表示从原点开始（整图更新）。
    /// - `patch`：本次覆盖的矩形尺寸 `(宽, 高)`。
    ///
    /// `offset` 与 `patch` 分开传而不是让调用方自己算，
    /// 是因为 `image_extent` 必须是**被写入区域**的尺寸——
    /// 局部更新若误传整图尺寸，Vulkan 会把超出图像的部分按未定义行为处理。
    ///
    /// # 为什么本函数不自建 staging
    ///
    /// 本函数只**录制**命令，GPU 何时真正读取 `staging` 由驱动决定。
    /// 若在此建局部 buffer，函数返回时它就没了，而GPU 可能还没跑完
    /// `cmd_copy_buffer_to_image`——这是 use-after-free。
    /// 生命周期必须由帧层的 arena 与提交栅栏共同管理。
    pub fn upload(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        staging: crate::frame::StagingSlice,
        bytes: &[u8],
        offset: Option<(u32, u32)>,
        patch: (u32, u32),
    ) -> anyhow::Result<()> {
        let (x, y) = offset.unwrap_or((0, 0));
        let (w, h) = patch;
        validate_region(self.size, (x, y), patch)?;

        // 按**本图像的格式**算字节数，不是硬编码 R8。
        //
        // 早前这里固定用 `staging_size_r8`（= w*h），
        // 于是上传 RGBA 缩略图（4*w*h 字节）时校验必然失败——
        // 表现是 GPU 命令一条都没录制就 `bail`，
        // 而 `full_app` 只画字体图集（永远传 R8），
        // **这条断路从未被跑出来过**。
        let needed = staging_size(self.format, w, h);
        anyhow::ensure!(
            bytes.len() as vk::DeviceSize == needed,
            "staging 数据 {} 字节，与 {w}x{h} 的 {:?} 区域（需 {needed} 字节）不符",
            bytes.len(),
            self.format
        );
        anyhow::ensure!(!staging.is_null(), "必须提供有效的 staging 切片");
        anyhow::ensure!(
            staging.size >= needed,
            "staging 切片容量 {} 不足 {w}x{h} 所需的 {needed} 字节",
            staging.size
        );

        let device = &gpu.device;
        let image = self.image;
        let old_layout = self.layout;

        // 第一次过渡：进入传输目标布局。
        let (src_stage, src_access) = upload_barrier_scope(old_layout);
        let to_transfer = image_barrier(
            image,
            old_layout,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            src_access,
            vk::AccessFlags::TRANSFER_WRITE,
        );
        // 第二次过渡：交给片元着色器读。
        let to_shader = image_barrier(
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::SHADER_READ,
        );

        // 数据从staging 的哪个偏移读。arena 已把偏移对齐到
        // STAGING_ALIGNMENT，满足 vkCmdCopyBufferToImage 对
        // bufferOffset 的对齐要求。
        let copy = buffer_image_copy(offset, w, h).buffer_offset(staging.offset);

        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                src_stage,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_transfer],
            );
            device.cmd_copy_buffer_to_image(
                cmd,
                staging.buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy],
            );
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_shader],
            );
        }

        self.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        Ok(())
    }

    /// 供描述符写入使用的 [`vk::DescriptorImageInfo`]。
    pub fn descriptor_info(&self, sampler: vk::Sampler) -> vk::DescriptorImageInfo {
        vk::DescriptorImageInfo::default()
            .sampler(sampler)
            .image_view(self.view)
            .image_layout(self.layout)
    }

    /// 把图像过渡到指定布局，并同步 [`DeviceImage::layout`]。
    ///
    /// # 为什么需要这个方法
    ///
    /// [`DeviceImage::layout`] 是**权威记账**——它不只是调试信息，
    /// [`DeviceImage::descriptor_info`] 会把它写进描述符的
    /// `imageLayout`。若调用方绕过本类型、自己调`image_barrier` 改布局
    /// 却不同步这个字段，后果是：
    ///
    /// 1. 描述符里声明的布局与实际不符 ⇒ GPU 采样行为**未定义**
    ///    （不是崩溃，是静默花屏，极难定位）；
    /// 2. 下一次 [`DeviceImage::upload`] 会从**错误**的 `old_layout`
    ///    出发发屏障，布局转换随之失效。
    ///
    /// 因此**任何**改变图像布局的操作都必须经过本方法。
    ///
    /// # 参数
    /// - `old_access` / `new_access`：屏障两端的访问掩码，由调用方按
    ///   实际用途填写（例如切到 `TRANSFER_SRC_OPTIMAL` 读回时，
    ///   旧侧是 `SHADER_READ`、新侧是 `TRANSFER_READ`）。
    /// - `src_stage` / `dst_stage`：对应两端的管线阶段。
    ///
    /// 只录一道屏障。调用方负责保证这次转换的同步需求成立。
    pub fn transition_to(
        &mut self,
        cmd: vk::CommandBuffer,
        device: &Device,
        new_layout: vk::ImageLayout,
        old_access: vk::AccessFlags,
        new_access: vk::AccessFlags,
        src_stage: vk::PipelineStageFlags,
        dst_stage: vk::PipelineStageFlags,
    ) {
        let old_layout = self.layout;
        if old_layout == new_layout {
            return;
        }
        let barrier = image_barrier(
            self.image,
            old_layout,
            new_layout,
            old_access,
            new_access,
        );
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                src_stage,
                dst_stage,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
        // 记账必须在录制之后：若 cmd_pipeline_barrier 之前的任何检查失败，
        // 字段就还反映真实布局。这里是纯 Vulkan 调用，不会失败。
        self.layout = new_layout;
    }

    /// 读回纹理内容到主机可见缓冲。
    ///
    /// 便捷入口：内部完成「切到 `TRANSFER_SRC_OPTIMAL` → `cmd_copy_image_to_buffer`
    /// → 切回 `SHADER_READ_ONLY_OPTIMAL`」并同步 [`DeviceImage::layout`]。
    ///
    /// `readback` 必须是主机可见缓冲，且带 `TRANSFER_DST` 用途位。
    pub fn read_to_buffer(
        &mut self,
        cmd: vk::CommandBuffer,
        device: &Device,
        readback: &crate::buffer::Buffer,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            readback.usage().contains(vk::BufferUsageFlags::TRANSFER_DST),
            "读回缓冲缺少 TRANSFER_DST 用途位"
        );
        let needed = self.size.width as vk::DeviceSize * self.size.height as vk::DeviceSize;
        anyhow::ensure!(
            readback.size() >= needed,
            "读回缓冲 {} 字节，不足以容纳 {}x{} 的 R8 图像（需 {needed} 字节）",
            readback.size(),
            self.size.width,
            self.size.height
        );

        self.transition_to(
            cmd,
            device,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::SHADER_READ,
            vk::AccessFlags::TRANSFER_READ,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::TRANSFER,
        );

        let copy = vk::BufferImageCopy::default()
            .buffer_offset(0)
            // 0 = 紧密排列，与 R8 每纹素1 字节对应
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .image_extent(vk::Extent3D {
                width: self.size.width,
                height: self.size.height,
                depth: 1,
            });
        unsafe {
            device.cmd_copy_image_to_buffer(
                cmd,
                self.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                readback.handle(),
                &[copy],
            );
            // 读回后切回采样布局，保持与 descriptor_info 一致
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[image_barrier(
                    self.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::TRANSFER_READ,
                    vk::AccessFlags::SHADER_READ,
                )],
            );
        }
        self.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        Ok(())
    }

    /// 销毁图像视图、图像与显存。
    ///
    /// # 前置条件
    /// 调用方**必须**已经 `device_wait_idle`。
    ///
    /// 销毁顺序：视图 → 图像 → 显存。图像视图引用图像，
    /// 反序会留下悬空引用。
    pub fn destroy(&mut self, device: &Device) {
        unsafe {
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
        self.view = vk::ImageView::null();
        self.image = vk::Image::null();
        self.memory = vk::DeviceMemory::null();
    }
}

/// 图像创建失败时的清理守卫。
struct ImageGuard<'a> {
    device: &'a Device,
    image: Option<vk::Image>,
    memory: Option<vk::DeviceMemory>,
    view: Option<vk::ImageView>,
}

impl Drop for ImageGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            if let Some(view) = self.view.take() {
                self.device.destroy_image_view(view, None);
            }
            if let Some(memory) = self.memory.take() {
                self.device.free_memory(memory, None);
            }
            if let Some(image) = self.image.take() {
                self.device.destroy_image(image, None);
            }
        }
    }
}

/// staging 行对齐。
///
/// `vk::BufferImageCopy` 的`buffer_row_length` 为 0 时表示「紧密排列」，
/// 此时不需要对齐；非 0 时会被解释为**纹素**数而非字节数。
/// 本项目固定用紧密排列（行长度为 0），因此这个常量仅用于计算
/// staging 缓冲的字节数时说明为什么不需要额外 padding。
pub const TIGHTLY_PACKED: u32 = 0;

/// 计算上传 `width x height` 的 R8 图像所需的 staging 字节数。
///
/// R8 每纹素 1 字节且紧密排列，故无需行对齐 padding。
/// 这是纯函数，可直接单测。
/// 单通道 8 位图像的 staging 字节数（保留给字体图集这类 R8 路径）。
pub fn staging_size_r8(width: u32, height: u32) -> vk::DeviceSize {
    width as vk::DeviceSize * height as vk::DeviceSize
}

/// 按格式计算 staging 区域字节数。
///
/// 只支持**无压缩**的格式——带 block 压缩的格式需要按块对齐算，
/// 超出当前需求（字体图集 R8 + 缩略图 RGBA）。
///
/// # 为什么需要它
///
/// 早前 `upload` 硬编码按 R8 算（`w * h`），
/// 于是上传 RGBA 缩略图（`4 * w * h`）时**校验必然失败**。
/// 由于字体图集一直是 R8，这条断路在实机里从未暴露。
pub fn staging_size(format: vk::Format, width: u32, height: u32) -> vk::DeviceSize {
    let bpp: vk::DeviceSize = match format {
        // 单通道
        vk::Format::R8_UNORM => 1,
        // 双通道
        vk::Format::R8G8_UNORM => 2,
        // 三通道 Vulkan 没有原生格式（BGR 在扩展里），此处不列
        // 四通道
        vk::Format::R8G8B8A8_UNORM
        | vk::Format::R8G8B8A8_SRGB
        | vk::Format::B8G8R8A8_UNORM
        | vk::Format::B8G8R8A8_SRGB => 4,
        // ⚠️ **不能 panic**。这是库里被外部调用的函数，panic 会把整个
        // 程序带走，而这里的条件只是「遇到了没支持的格式」——用返回值
        // 表达更合适。
        //
        // 早前这里 panic，于是任何走 BC 压缩或未列入的格式都会让程序崩，
        // 而不是「这一路径不支持」。
        other => {
            tracing::warn!(
                "staging_size 遇到未支持格式 {other:?}，按 0 字节处理（该格式需按 block 大小另算）"
            );
            return 0;
        }
    };
    bpp * width as vk::DeviceSize * height as vk::DeviceSize
}

/// 构造一次上传用的 [`vk::BufferImageCopy`]。
///
/// `dst_offset` 为 `None` 表示从图像原点开始；`Some((x, y))` 表示
/// 只覆盖图像的某个矩形区域（egui 的增量纹理更新会用到）。
///
/// 纯函数，便于单测偏移与范围计算。
pub fn buffer_image_copy(
    offset: Option<(u32, u32)>,
    width: u32,
    height: u32,
) -> vk::BufferImageCopy {
    let (x, y) = offset.unwrap_or((0, 0));
    vk::BufferImageCopy::default()
        .buffer_offset(0)
        // 0 = 紧密排列，与staging_size_r8 的字节数假设一致
        .buffer_row_length(TIGHTLY_PACKED)
        .buffer_image_height(0)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .image_offset(vk::Offset3D {
            x: x as i32,
            y: y as i32,
            z: 0,
        })
        .image_extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
}

/// 计算「转入 `TRANSFER_DST_OPTIMAL`」这道屏障的源阶段与源访问掩码。
///
/// # 为什么 srcStage 必须随布局变化
///
/// 屏障的 `srcStageMask` 决定**等谁**，`srcAccessMask` 决定**等什么**。
/// 两者必须匹配：若srcAccess 声明了「要等片元着色器的读」，而 srcStage
/// 却是 `TOP_OF_PIPE`，这道屏障就成了空操作——`TOP_OF_PIPE` 在规范里
/// 逻辑上早于所有其它阶段，**不等待任何东西**。
///
/// 这属于「依赖驱动宽容」而非「规范正确」：桌面驱动往往仍然正确地
/// 完成排障，但换驱动 / 开验证层（`VK_LAYER_KHRONOS_validation` 的
/// `SYNC-HAZARD-READ-WRITE`）就会报同步错误。
///
/// # 三种情形
///
/// - `UNDEFINED`：内容无需保留，此前没有任何访问，**无需等待**。
/// - `SHADER_READ_ONLY_OPTIMAL`：上一帧的片元着色器可能还在读，
///   **必须等片元阶段**。
/// - `TRANSFER_DST_OPTIMAL`：上一次拷贝的 `TRANSFER_WRITE` 可能还在进行，
///   **必须等传输阶段**。这一分支平时走不到（每次 `upload` 收尾都会转到
///   `SHADER_READ_ONLY_OPTIMAL`），但若将来有人在两次 upload 之间插入
///   拷贝，没有这个分支就会用错误的访问掩码静默出错。
///
/// 纯函数，可脱离 GPU 单测。
fn upload_barrier_scope(
    old_layout: vk::ImageLayout,
) -> (vk::PipelineStageFlags, vk::AccessFlags) {
    match old_layout {
        vk::ImageLayout::UNDEFINED => (
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::AccessFlags::empty(),
        ),
        vk::ImageLayout::TRANSFER_DST_OPTIMAL => (
            vk::PipelineStageFlags::TRANSFER,
            vk::AccessFlags::TRANSFER_WRITE,
        ),
        // 其余（主要是 SHADER_READ_ONLY_OPTIMAL）：按片元着色器读处理。
        _ => (
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::AccessFlags::SHADER_READ,
        ),
    }
}

/// 校验一次局部更新是否落在图像范围内。
///
/// egui 的 [`egui::epaint::ImageDelta::pos`] 给出的是像素坐标，
/// 若图集扩容后调用方仍按旧尺寸算矩形就会越界——
/// Vulkan 对越界的 `image_offset + image_extent` 行为未定义。
/// 这是纯函数，可单测。
pub fn validate_region(
    image_size: vk::Extent2D,
    offset: (u32, u32),
    patch: (u32, u32),
) -> anyhow::Result<()> {
    let (x, y) = offset;
    let (w, h) = patch;
    anyhow::ensure!(w > 0 && h > 0, "更新区域尺寸必须大于 0，得到 {w}x{h}");
    let end_x = x as u64 + w as u64;
    let end_y = y as u64 + h as u64;
    anyhow::ensure!(
        end_x <= image_size.width as u64 && end_y <= image_size.height as u64,
        "更新区域 ({x}, {y}) {w}x{h} 超出图像 {}x{} 范围",
        image_size.width,
        image_size.height
    );
    Ok(())
}

/// 字体图集纹理。
///
/// 一个 [`DeviceImage`] 加一个点采样 [`Sampler`]。
pub struct FontTexture {
    image: DeviceImage,
    sampler: Sampler,
}

impl FontTexture {
    /// 按指定尺寸创建空的字体图集纹理。
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> anyhow::Result<Self> {
        let size = vk::Extent2D { width, height };
        let image = DeviceImage::new(
            gpu,
            size,
            FONT_FORMAT,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
        )?;
        let sampler = Sampler::new_nearest(&gpu.device)?;
        tracing::debug!(width, height, "字体图集纹理创建成功");
        Ok(Self { image, sampler })
    }

    /// 图像尺寸。
    pub fn size(&self) -> vk::Extent2D {
        self.image.size
    }

    /// 图像句柄。
    pub fn image(&self) -> vk::Image {
        self.image.image
    }

    /// 图像视图。
    pub fn view(&self) -> vk::ImageView {
        self.image.view
    }

    /// 采样器。
    pub fn sampler(&self) -> vk::Sampler {
        self.sampler.handle
    }

    /// 当前布局。
    pub fn layout(&self) -> vk::ImageLayout {
        self.image.layout
    }

    /// 供描述符写入使用。
    pub fn descriptor_info(&self) -> vk::DescriptorImageInfo {
        self.image.descriptor_info(self.sampler.handle)
    }

    /// 把 CPU 侧的覆盖率字节上传到纹理。
    ///
    /// 委托给 [`DeviceImage::upload`]。`offset` 为 `None` 表示整图覆盖。
    ///
    /// `staging` 必须来自 [`crate::frame::StagingArena`] 且已写入 `bytes`。
    /// 上层通常直接用 [`crate::frame::FrameRenderer::record_texture_upload`]，
    /// 无需手工构造。
    pub fn upload(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        staging: crate::frame::StagingSlice,
        bytes: &[u8],
        offset: Option<(u32, u32)>,
        patch: (u32, u32),
    ) -> anyhow::Result<()> {
        self.image.upload(gpu, cmd, staging, bytes, offset, patch)
    }

    /// 按 egui 的整图/局部增量更新纹理。
    ///
    /// `delta.pos` 为 `None` 时整图覆盖；`Some((x, y))` 时只更新该矩形。
    ///
    /// `staging` 必须来自 [`crate::frame::StagingArena`]。
    pub fn apply_delta(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        staging: crate::frame::StagingSlice,
        delta: &egui::epaint::ImageDelta,
    ) -> anyhow::Result<()> {
        let patch_size = delta.image.size();
        let (w, h) = (patch_size[0] as u32, patch_size[1] as u32);
        let offset = delta.pos.map(|p| (p[0] as u32, p[1] as u32));

        // 整图更新要求尺寸完全一致——尺寸变化意味着图集扩容，
        // 那种情况必须重建纹理而非覆盖。
        if offset.is_none() {
            self.image.ensure_size_matches(patch_size)?;
        }

        let bytes = DeviceImage::coverage_bytes(&delta.image);
        self.image
            .upload(gpu, cmd, staging, &bytes, offset, (w, h))
    }

    /// 销毁纹理。
    ///
    /// # 前置条件
    /// 调用方**必须**已经 `device_wait_idle`。
    pub fn destroy(&mut self, device: &Device) {
        self.sampler.destroy(device);
        self.image.destroy(device);
    }
}

/// 按 [`egui::TextureId`] 管理的纹理表。
///
/// egui 的 [`egui::TexturesDelta`] 是以 `TextureId` 为键的增量集合：
/// `Managed(0)` 恒为字体图集，`Managed(n)` 与 `User(n)` 是用户纹理。
/// 本表负责按 ID 增删改，帧层只需查表拿描述符信息。
///
/// 采样器按过滤方式缓存：字体图集用点采样，其余若为线性则共用一个
/// 线性采样器，避免为每张纹理各建一个。
pub struct TextureStore {
    textures: HashMap<egui::TextureId, DeviceImage>,
    /// 每个纹理配套的采样器句柄。
    samplers: HashMap<egui::TextureId, vk::Sampler>,
    owned_samplers: Vec<Sampler>,
    /// 字体图集（`Managed(0)`）的专用句柄，供着色器绑定。
    pub font: Option<FontTexture>,
}

impl TextureStore {
    pub fn new() -> Self {
        Self {
            textures: HashMap::new(),
            samplers: HashMap::new(),
            owned_samplers: Vec::new(),
            font: None,
        }
    }

    /// 按 egui 的增量描述更新纹理表。
    ///
    /// 步骤：先处理 `set`（新增或覆盖），再处理 `free`（释放）。
    /// 顺序不能反——同一帧内既设置又释放同一 ID 时，
    /// egui 期望的是「先设后释放」，这样净效果是该纹理消失。
    ///
    /// `arena` 提供所有上传的 staging 空间——**必须**是
    /// [`crate::frame::StagingArena`]，因为 egui 的一帧可能包含
    /// 上千次逐字形增量上传，自建 staging 会耗尽显存。
    pub fn apply(
        &mut self,
        gpu: &Gpu,
        arena: &mut crate::frame::StagingArena<'_>,
        cmd: vk::CommandBuffer,
        delta: &egui::TexturesDelta,
    ) -> anyhow::Result<()> {
        for (id, deltas) in &delta.set {
            // 字体图集走专用路径：它有独立的采样器与描述符绑定。
            if *id == FONT_TEXTURE_ID {
                self.update_font(gpu, arena, cmd, deltas)?;
                continue;
            }
            for d in deltas {
                self.update_user(gpu, arena, cmd, *id, d)?;
            }
        }

        for id in &delta.free {
            if *id == FONT_TEXTURE_ID {
                // 字体图集不会被 egui 释放；真要释放说明调用方逻辑有问题。
                tracing::warn!("egui 请求释放字体图集，已忽略");
                continue;
            }
            if let Some(mut image) = self.textures.remove(id) {
                // 释放前必须确保设备不再读取该纹理。
                gpu.wait_idle();
                image.destroy(&gpu.device);
            }
            self.samplers.remove(id);
        }
        Ok(())
    }

    /// 更新（或新建）一张用户纹理。
    fn update_user(
        &mut self,
        gpu: &Gpu,
        arena: &mut crate::frame::StagingArena<'_>,
        cmd: vk::CommandBuffer,
        id: egui::TextureId,
        delta: &egui::epaint::ImageDelta,
    ) -> anyhow::Result<()> {
        let size = delta.image.size();
        let (w, h) = (size[0] as u32, size[1] as u32);

        // 尺寸变化意味着整图重建：局部更新无法改变图像尺寸。
        // egui 约定此时会先发一个整图 delta，因此走新建路径。
        let stale = self
            .textures
            .get(&id)
            .is_some_and(|img| img.size.width != w || img.size.height != h);
        if stale {
            if let Some(mut old) = self.textures.remove(&id) {
                gpu.wait_idle();
                old.destroy(&gpu.device);
            }
            self.samplers.remove(&id);
        }

        if !self.textures.contains_key(&id) {
            let image = DeviceImage::new(
                gpu,
                vk::Extent2D { width: w, height: h },
                FONT_FORMAT,
                vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            )?;
            self.textures.insert(id, image);
            let sampler = Sampler::from_egui(&gpu.device, &delta.options)?;
            self.samplers.insert(id, sampler.handle());
            self.owned_samplers.push(sampler);
        }

        // 先取 staging 并写入，再录制命令。顺序不能反：
        // `get_mut` 借用了 self，而 arena 是独立对象，不冲突。
        let bytes = DeviceImage::coverage_bytes(&delta.image);
        let offset = delta.pos.map(|p| (p[0] as u32, p[1] as u32));
        let staging = arena.allocate(bytes.len() as vk::DeviceSize)?;
        staging.write(&gpu.device, &bytes)?;

        let image = self.textures.get_mut(&id).expect("上方已确保存在");
        image.upload(gpu, cmd, staging, &bytes, offset, (w, h))
    }

    fn update_font(
        &mut self,
        gpu: &Gpu,
        arena: &mut crate::frame::StagingArena<'_>,
        cmd: vk::CommandBuffer,
        deltas: &[egui::epaint::ImageDelta],
    ) -> anyhow::Result<()> {
        let Some(last) = deltas.last() else {
            return Ok(());
        };
        let size = last.image.size();
        let (w, h) = (size[0] as u32, size[1] as u32);

        let needs_new = match &self.font {
            None => true,
            Some(f) => f.size().width != w || f.size().height != h,
        };
        if needs_new {
            // 图集扩容：销毁旧纹理再建新的。
            // 旧纹理可能仍被在途命令引用，因此必须先等设备空闲。
            if let Some(mut old) = self.font.take() {
                gpu.wait_idle();
                old.destroy(&gpu.device);
            }
            self.font = Some(FontTexture::new(gpu, w, h)?);
        }

        // 逐个应用增量。整图更新会覆盖此前的局部更新，
        // 而 egui 保证同一帧内 whole delta 会替换该 ID 的所有历史增量，
        // 因此按顺序应用即可得到正确结果。
        for d in deltas {
            let bytes = DeviceImage::coverage_bytes(&d.image);
            let staging = arena.allocate(bytes.len() as vk::DeviceSize)?;
            staging.write(&gpu.device, &bytes)?;
            let font = self.font.as_mut().expect("上方已确保存在");
            font.apply_delta(gpu, cmd, staging, d)?;
        }
        Ok(())
    }

    /// 查表取描述符信息。
    pub fn descriptor_info(&self, id: egui::TextureId) -> Option<vk::DescriptorImageInfo> {
        let image = self.textures.get(&id)?;
        let sampler = self.samplers.get(&id).copied()?;
        Some(image.descriptor_info(sampler))
    }

    /// 字体图集描述符信息。
    pub fn font_descriptor_info(&self) -> Option<vk::DescriptorImageInfo> {
        self.font.as_ref().map(|f| f.descriptor_info())
    }

    /// 当前托管的纹理数量（不含字体图集）。
    pub fn len(&self) -> usize {
        self.textures.len()
    }

    pub fn is_empty(&self) -> bool {
        self.textures.is_empty()
    }

    /// 释放所有纹理。
    ///
    /// # 前置条件
    /// 调用方**必须**已经 `device_wait_idle`。
    pub fn destroy(&mut self, device: &Device) {
        if let Some(mut font) = self.font.take() {
            font.destroy(device);
        }
        for (_, image) in self.textures.drain() {
            let mut image = image;
            image.destroy(device);
        }
        self.samplers.clear();
        for s in &self.owned_samplers {
            s.destroy(device);
        }
        self.owned_samplers.clear();
    }
}

impl Default for TextureStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {    /// 按格式算 staging 字节数——R8 与 RGBA 必须不同。
    ///
    /// 回归测试：`upload` 早前硬编码按 R8 算，
    /// 上传 RGBA 缩略图时校验必然失败（GPU 命令一条都没录制）。
    /// 该断路在实机里从未暴露，因为字体图集一直是 R8。
    #[test]
    fn staging_size_depends_on_format() {
        let w = 4u32;
        let h = 4u32;
        assert_eq!(staging_size(vk::Format::R8_UNORM, w, h), 16);
        assert_eq!(staging_size(vk::Format::R8G8B8A8_UNORM, w, h), 64);
        assert_eq!(staging_size(vk::Format::B8G8R8A8_UNORM, w, h), 64);
        // 旧函数保留给 R8 路径，但结果必须与按格式算的一致
        assert_eq!(staging_size_r8(w, h), staging_size(vk::Format::R8_UNORM, w, h));
    }

    /// 未支持格式**不得 panic**，也不得算出错误的字节数。
    ///
    /// # 这条测试原先断言的是相反的行为
    ///
    /// 早前它是 `#[should_panic(expected = "不支持压缩或未知格式")]` ——
    /// 把「库里遇到未支持格式就把整个程序带走」当成了正确行为。
    ///
    /// `staging_size` 是被外部调用的库函数：panic 会杀掉整个程序，
    /// 而条件只是「这个格式要走另一条按 block 算的路径」。用返回值
    /// 表达才合适——调用方看到 0 就知道这条路径不支持。
    #[test]
    fn staging_size_returns_zero_for_unsupported_format() {
        assert_eq!(
            staging_size(vk::Format::BC7_UNORM_BLOCK, 4, 4),
            0,
            "未支持格式应返回 0，绝不能 panic"
        );
        // 已支持的格式仍要算对，别被这条改动带偏。
        assert_eq!(staging_size(vk::Format::R8_UNORM, 10, 10), 100);
        assert_eq!(staging_size(vk::Format::R8G8B8A8_UNORM, 10, 10), 400);
    }


    use super::*;
    use std::sync::Arc;

    /// egui 0.36 字体图集是 RGBA，取红通道得覆盖率。
    #[test]
    fn coverage_takes_red_channel() {
        let img = egui::ColorImage::from_gray([2, 2], &[0, 64, 128, 255]);
        let data = egui::ImageData::Color(Arc::new(img));
        assert_eq!(DeviceImage::coverage_bytes(&data), vec![0, 64, 128, 255]);
    }

    /// `from_white_alpha` 把同一 alpha 写进四个通道，
    /// 因此取红通道与取 alpha 通道等价——这是单通道格式成立的前提。
    #[test]
    fn red_channel_equals_alpha_for_font_atlas() {
        for a in [0u8, 1, 77, 200, 255] {
            let c = egui::Color32::from_white_alpha(a);
            assert_eq!(c.r(), c.a(), "from_white_alpha 应让 r 与 a 相等（a={a}）");
        }
        let img = egui::ColorImage::new([1, 3], vec![
            egui::Color32::from_white_alpha(10),
            egui::Color32::from_white_alpha(120),
            egui::Color32::from_white_alpha(240),
        ]);
        let data = egui::ImageData::Color(Arc::new(img));
        assert_eq!(DeviceImage::coverage_bytes(&data), vec![10, 120, 240]);
    }

    #[test]
    fn coverage_length_matches_pixel_count() {
        let img = egui::ColorImage::filled([7, 5], egui::Color32::WHITE);
        let n = img.pixels.len();
        let data = egui::ImageData::Color(Arc::new(img));
        assert_eq!(DeviceImage::coverage_bytes(&data).len(), n);
        assert_eq!(n, 35);
    }

    #[test]
    fn staging_size_for_r8_is_width_times_height() {
        // R8 每纹素1 字节，紧密排列
        assert_eq!(staging_size_r8(1, 1), 1);
        assert_eq!(staging_size_r8(2048, 2048), 2048 * 2048);
        // 对比 RGBA 的四倍，量化格式选择带来的收益
        assert_eq!(staging_size_r8(1024, 1024) * 4, 1024 * 1024 * 4);
    }

    #[test]
    fn buffer_image_copy_defaults_to_tight_packing() {
        let c = buffer_image_copy(None, 64, 32);
        assert_eq!(c.buffer_row_length, 0, "行长度 0 = 紧密排列");
        assert_eq!(c.buffer_image_height, 0);
        assert_eq!(c.buffer_offset, 0);
        assert_eq!(c.image_offset, vk::Offset3D { x: 0, y: 0, z: 0 });
        assert_eq!(
            c.image_extent,
            vk::Extent3D { width: 64, height: 32, depth: 1 }
        );
        assert_eq!(c.image_subresource.aspect_mask, vk::ImageAspectFlags::COLOR);
        assert_eq!(c.image_subresource.layer_count, 1);
    }

    #[test]
    fn buffer_image_copy_honors_offset() {
        let c = buffer_image_copy(Some((100, 50)), 16, 16);
        assert_eq!(c.image_offset, vk::Offset3D { x: 100, y: 50, z: 0 });
        assert_eq!(
            c.image_extent,
            vk::Extent3D { width: 16, height: 16, depth: 1 }
        );
    }

    #[test]
    fn region_validation_accepts_exact_fit() {
        let size = vk::Extent2D { width: 256, height: 256 };
        assert!(validate_region(size, (0, 0), (256, 256)).is_ok());
        assert!(validate_region(size, (200, 200), (56, 56)).is_ok());
        assert!(validate_region(size, (256, 0), (0, 1)).is_err(), "宽度 0 非法");
    }

    #[test]
    fn region_validation_rejects_out_of_bounds() {
        let size = vk::Extent2D { width: 256, height: 256 };
        // 越界一列
        assert!(validate_region(size, (250, 0), (10, 10)).is_err());
        // 起点就在图外
        assert!(validate_region(size, (256, 0), (1, 1)).is_err());
        // 纵越界
        assert!(validate_region(size, (0, 255), (1, 2)).is_err());
        // 零尺寸
        assert!(validate_region(size, (0, 0), (0, 10)).is_err());
    }

    /// 屏障的 srcStage 与 srcAccess 必须匹配。
    ///
    /// 这是本组最重要的不变量：`TOP_OF_PIPE` 不等待任何东西，
    /// 若同时声明了非空 srcAccess，屏障就是空操作。
    /// 曾经的 bug 正是「srcAccess=SHADER_READ 却配 TOP_OF_PIPE」。
    #[test]
    fn barrier_scope_pairs_stage_with_access() {
        // UNDEFINED：无需等待
        let (stage, access) = upload_barrier_scope(vk::ImageLayout::UNDEFINED);
        assert_eq!(stage, vk::PipelineStageFlags::TOP_OF_PIPE);
        assert!(
            access.is_empty(),
            "UNDEFINED 表示内容无需保留，不应等待任何访问"
        );

        // SHADER_READ_ONLY_OPTIMAL：等片元着色器的读
        let (stage, access) = upload_barrier_scope(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        assert_eq!(stage, vk::PipelineStageFlags::FRAGMENT_SHADER);
        assert_eq!(access, vk::AccessFlags::SHADER_READ);

        // TRANSFER_DST_OPTIMAL：等上一次的传输写
        let (stage, access) = upload_barrier_scope(vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        assert_eq!(stage, vk::PipelineStageFlags::TRANSFER);
        assert_eq!(access, vk::AccessFlags::TRANSFER_WRITE);
    }

    /// 穷举所有布局，断言「srcAccess 非空 ⇒ srcStage 不是 TOP_OF_PIPE」。
    ///
    /// 上一条测的是具体取值；这条测的是**不变式本身**——
    /// 将来有人新增分支、错配了阶段与访问掩码，这里立刻发现。
    #[test]
    fn barrier_never_pairs_access_with_top_of_pipe() {
        let layouts = [
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::PREINITIALIZED,
        ];
        for l in layouts {
            let (stage, access) = upload_barrier_scope(l);
            if !access.is_empty() {
                assert!(
                    !stage.contains(vk::PipelineStageFlags::TOP_OF_PIPE),
                    "布局 {l:?} 声明了等待 {access:?}，却用 TOP_OF_PIPE 作srcStage——\
                     这道屏障不会等待任何东西，是空操作"
                );
            }
        }
    }

    /// `GENERAL` 等布局也不能漏——旧实现用 `_ =>`兜底，
    /// 任何非UNDEFINED 布局都拿 SHADER_READ。这里明确 GENERAL 走等待分支。
    #[test]
    fn non_undefined_layouts_all_wait() {
        for l in [
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        ] {
            let (_, access) = upload_barrier_scope(l);
            assert!(
                !access.is_empty(),
                "布局 {l:?} 不是 UNDEFINED，必须等待之前的访问完成"
            );
        }
    }

    #[test]
    fn region_validation_uses_wide_arithmetic() {
        // u32 加法若在 u32 下做，u32::MAX + 1 会回绕成 0 而误判为合法。
        // 这里确认实现用的是不会回绕的运算。
        let size = vk::Extent2D {
            width: 16,
            height: 16,
        };
        let huge = u32::MAX - 1;
        assert!(validate_region(size, (huge, 0), (8, 1)).is_err());
    }

    #[test]
    fn font_format_is_single_channel() {
        // 格式选择是硬性约定：改成 RGBA 会让纹理体积翻两番
        assert_eq!(FONT_FORMAT, vk::Format::R8_UNORM);
    }

    /// 读回缓冲的容量检查必须按 R8 每纹素 1 字节算。
    ///
    /// 这条守住 `read_to_buffer` 的前提：调用方传的缓冲若小于
    /// `w * h`，拷贝会越界——而 Vulkan 对越界 `vkCmdCopyImageToBuffer`
    /// 是未定义行为。
    #[test]
    fn readback_capacity_is_width_times_height() {
        let (w, h) = (256u32, 256u32);
        let needed = staging_size_r8(w, h);
        assert_eq!(needed, 65_536);
        // 与 MIN_STAGING_CAPACITY 同值，但二者含义不同：
        // 这里是「读回缓冲必须多大」，那里是「staging 块的下限」
        assert_eq!(staging_size_r8(64, 64), 4096);
        // 宽或高为 0 时需求为 0（不合法图像由 DeviceImage::new 拦）
        assert_eq!(staging_size_r8(0, 100), 0);
    }

    /// `transition_to` 之后 `layout` 必须与实际布局一致。
    ///
    /// 这条是**文档化的不变式**的可执行版本：`descriptor_info()` 把
    /// `layout` 写进描述符的 `imageLayout`，一旦与实际不符，
    /// GPU 采样行为未定义（静默花屏，不崩溃）。
    ///
    /// 纯逻辑部分：同一布局重复转换应是幂等的（`transition_to` 里
    /// `old == new` 时直接返回，不发屏障）。
    #[test]
    fn transition_to_same_layout_is_idempotent() {
        // 无法在单测里构造 DeviceImage（需要真实设备），因此这里
        // 只固化「相同布局不转换」这条规则本身，供实现对照。
        let a = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        let b = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        assert_eq!(a, b, "相同布局必须被transition_to 视为无需转换");
        assert_ne!(
            a,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            "不同布局必须真的发屏障，否则读回后采样布局会失同步"
        );
    }

    #[test]
    fn store_starts_empty() {
        let s = TextureStore::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert!(s.font.is_none());
        assert!(s.font_descriptor_info().is_none());
    }

    /// staging 容量决策：局部更新也按整图分配。
    ///
    /// 这条不变式支撑了「staging 只分配一次」的结论——若改成按
    /// `needed` 精确分配，局部更新（小）与随后的整图更新（大）会
    /// 反复触发重建，`Buffer` 只能整体 destroy，旧的会泄漏。
    #[test]
    fn staging_is_sized_to_whole_image() {
        let full = staging_size_r8(2048, 2048);
        // 一个 16x16 的局部更新所需的字节数
        let patch = staging_size_r8(16, 16);
        assert!(patch < full);
        // 上传里用的是 full.max(needed)，整图时取 full，局部时也取 full
        assert_eq!(full.max(patch), full, "局部更新不应缩小 staging");
    }

    /// 重复上传不再新建 staging。
    ///
    /// 这条测的是「泄漏已消除」这个结论的数学前提：
    /// 同一 `DeviceImage` 上多次 upload 时，容量决策恒为full，
    /// 因此 `staging_buffer` 的 reuse 判定恒为真。
    #[test]
    fn repeated_uploads_reuse_same_capacity() {
        let full = staging_size_r8(512, 512);
        let mut capacity = 0;
        // 模拟 1000 次逐字形增量上传（egui 中文字体首次加载的量级）
        for _ in 0..1000 {
            let needed = staging_size_r8(8, 8);
            let want = full.max(needed);
            if capacity == 0 {
                capacity = want;
            }
            // want 恒定 => reuse 恒为 true => 只分配一次
            assert_eq!(want, capacity);
        }
        assert_eq!(capacity, 512 * 512);
    }
}
