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

use crate::buffer::{align_up, Buffer};
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
    /// - `bytes`：覆盖率数据。整图更新时长度须等于 `w * h`；
    ///   局部更新时长度须等于被覆盖矩形 `patch` 的像素数。
    /// - `offset`：更新起始像素坐标。`None` 表示从原点开始（整图更新）。
    /// - `patch`：本次覆盖的矩形尺寸 `(宽, 高)`。
    ///
    /// `offset` 与 `patch` 分开传而不是让调用方自己算，
    /// 是因为 `image_extent` 必须是**被写入区域**的尺寸——
    /// 局部更新若误传整图尺寸，Vulkan 会把超出图像的部分按未定义行为处理。
    pub fn upload(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        bytes: &[u8],
        offset: Option<(u32, u32)>,
        patch: (u32, u32),
    ) -> anyhow::Result<()> {
        let (x, y) = offset.unwrap_or((0, 0));
        let (w, h) = patch;
        validate_region(self.size, (x, y), patch)?;

        let needed = staging_size_r8(w, h);
        anyhow::ensure!(
            bytes.len() as vk::DeviceSize == needed,
            "staging 数据 {} 字节，与 {w}x{h} 的 R8 区域（需 {needed} 字节）不符",
            bytes.len()
        );

        // staging 缓冲只活到本次提交结束，因此用主机可见内存直接映射写入。
        let mut staging = Buffer::new_host_visible(
            gpu,
            needed,
            vk::BufferUsageFlags::TRANSFER_SRC,
            vk::SharingMode::EXCLUSIVE,
        )?;
        staging.write(&gpu.device, 0, bytes)?;

        let device = &gpu.device;
        let old_layout = self.layout;

        // 第一次过渡：进入传输目标布局。
        let to_transfer = image_barrier(
            self.image,
            old_layout,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            match old_layout {
                // UNDEFINED 表示内容无需保留，此前没有任何访问需要等待。
                vk::ImageLayout::UNDEFINED => vk::AccessFlags::empty(),
                _ => vk::AccessFlags::SHADER_READ,
            },
            vk::AccessFlags::TRANSFER_WRITE,
        );
        // 第二次过渡：交给片元着色器读。
        let to_shader = image_barrier(
            self.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::SHADER_READ,
        );

        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_transfer],
            );
            device.cmd_copy_buffer_to_image(
                cmd,
                staging.handle(),
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[buffer_image_copy(offset, w, h)],
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

        // staging 缓冲必须等命令执行完才能销毁。这里只解除映射，
        // 实际显存释放依赖调用方在提交后的device_wait_idle。
        // 若帧层改为双缓冲提交，需要把 staging 提升为跨帧对象。
        drop(staging);
        Ok(())
    }

    /// 供描述符写入使用的 [`vk::DescriptorImageInfo`]。
    pub fn descriptor_info(&self, sampler: vk::Sampler) -> vk::DescriptorImageInfo {
        vk::DescriptorImageInfo::default()
            .sampler(sampler)
            .image_view(self.view)
            .image_layout(self.layout)
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
pub fn staging_size_r8(width: u32, height: u32) -> vk::DeviceSize {
    width as vk::DeviceSize * height as vk::DeviceSize
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
    pub fn upload(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        bytes: &[u8],
        offset: Option<(u32, u32)>,
        patch: (u32, u32),
    ) -> anyhow::Result<()> {
        self.image.upload(gpu, cmd, bytes, offset, patch)
    }

    /// 按 egui 的整图/局部增量更新纹理。
    ///
    /// `delta.pos` 为 `None` 时整图覆盖；`Some((x, y))` 时只更新该矩形。
    pub fn apply_delta(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
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
        self.image.upload(gpu, cmd, &bytes, offset, (w, h))
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
    pub fn apply(
        &mut self,
        gpu: &Gpu,
        cmd: vk::CommandBuffer,
        delta: &egui::TexturesDelta,
    ) -> anyhow::Result<()> {
        for (id, deltas) in &delta.set {
            // 字体图集走专用路径：它有独立的采样器与描述符绑定。
            if *id == FONT_TEXTURE_ID {
                self.update_font(gpu, cmd, deltas)?;
                continue;
            }
            for d in deltas {
                self.update_user(gpu, cmd, *id, d)?;
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

        let image = self.textures.get_mut(&id).expect("上方已确保存在");
        let bytes = DeviceImage::coverage_bytes(&delta.image);
        let offset = delta.pos.map(|p| (p[0] as u32, p[1] as u32));
        image.upload(gpu, cmd, &bytes, offset, (w, h))
    }

    fn update_font(
        &mut self,
        gpu: &Gpu,
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
        let font = self.font.as_mut().expect("上方已确保存在");
        for d in deltas {
            font.apply_delta(gpu, cmd, d)?;
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
mod tests {
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

    #[test]
    fn store_starts_empty() {
        let s = TextureStore::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert!(s.font.is_none());
        assert!(s.font_descriptor_info().is_none());
    }
}
