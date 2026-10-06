//! 原生 Vulkan 渲染后端。
//!
//! 本 crate 用 `ash`（纯 Rust 的 Vulkan 绑定）直接驱动 Vulkan，
//! **不经过 wgpu 等中间抽象层**。原因：抽象层在初始化时要同时编译并
//! 枚举两套后端代码路径（DX12 与 Vulkan），且会为每个枚举到的 Adapter
//! 加载对应厂商驱动栈。对常驻小工具而言，这部分开销与收益不成比例。
//! 直接用 ash 只初始化 Vulkan 一条路径。
//!
//! 分层：
//! - [`Gpu`]：实例、表面、物理设备与逻辑设备
//! - [`Swapchain`]：交换链与呈现模式
//!
//! egui 只负责 CPU 端的排版与图元生成（tessellation），
//! 本 crate 负责把 `egui::ClippedPrimitive` 转成 Vulkan 绘制调用。

use std::ffi::CString;

pub mod buffer;
pub mod frame;
pub mod pipeline;
pub mod shader;
pub mod staging;
pub mod texture;
pub mod window;

use ash::{Device, Entry, Instance as AshInstance, khr, vk};
use khr::surface::Instance as SurfaceLoader;

/// 逻辑设备启用的描述符更新能力。
///
/// # 为什么需要它
///
/// 本项目每个在飞帧槽位各有独立的描述符集，而槽位的 GPU 工作与 CPU 的
/// 下一帧是**并行**的——`full_app` 在若干帧在途的同时会重写所有槽位的
/// 描述符（字体图集重建、交换链重建、uniform 内容变化时）。
/// 这在 Vulkan 1.1 下是 `VUID-vkUpdateDescriptorSets-None-03047` 违规：
///
/// > 绑定若创建时未带 `UPDATE_AFTER_BIND_BIT` 或
/// `UPDATE_UNUSED_WHILE_PENDING_BIT`，其描述符集就不得被处于
/// > pending 状态的命令缓冲使用中的描述符集。
///
/// # 为什么不能靠等fence 规避
///
/// `UPDATE_UNUSED_WHILE_PENDING` 的语义是「更新时保证该绑定尚未被使用」，
/// 而本项目的多帧在途**正是**「已被使用」的状态。要满足它就得在每次
/// 重写前把全部在飞槽位等一遍，等于放弃多帧并行——用同步换规范合规，
/// 是把设计退回到`device_wait_idle`。而`UPDATE_AFTER_BIND` 恰好
/// 描述的正是我们的用法：绑定之后仍可更新。
///
/// # ash 0.38 陷阱
///
/// `PhysicalDeviceVulkan12Features` **没有 builder 构造器**（相邻的
/// `PhysicalDeviceFeatures2` 才有 `.features()`），只能逐字段赋值。
/// 且它是 Vulkan 1.2 核心结构体，常量位于 `vk::feature_extensions`
/// 而非 `vk::bitflags`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescriptorUpdateCaps {
    /// 可给UNIFORM_BUFFER 绑定加 `UPDATE_AFTER_BIND`。
    pub uniform_buffer_update_after_bind: bool,
    /// 可给 SAMPLER / COMBINED_IMAGE_SAMPLER 绑定加 `UPDATE_AFTER_BIND`。
    pub sampled_image_update_after_bind: bool,
    /// 设备本身是否支持 Vulkan 1.2（未启用任何 1.2 特性时为 false）。
    pub vulkan_1_2: bool,
}

impl DescriptorUpdateCaps {
    /// 三个绑定（uniform / sampler / texture）是否都能加 `UPDATE_AFTER_BIND`。
    ///
    /// 缺任一项时，**必须**由调用方保证「重写描述符时该槽位GPU 已完成」
    /// （例如只在交换链重建、全局 `wait_idle` 之后写），否则仍会违规。
    pub fn all_bindings_update_after_bind(&self) -> bool {
        self.uniform_buffer_update_after_bind && self.sampled_image_update_after_bind
    }
}

/// 查询设备支持的描述符更新能力。
///
/// 只**查询**不启用——启用发生在 [`Gpu::new`] 创建逻辑设备时。
fn query_descriptor_update_caps(
    instance: &AshInstance,
    physical_device: vk::PhysicalDevice,
) -> DescriptorUpdateCaps {
    let mut feats12 = vk::PhysicalDeviceVulkan12Features::default();
    // ash 0.38用 `push_next`（不是 `p_next`）挂 pNext 链。
    let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut feats12);
    // 入口属实例级 1.1 核心，本项目已申请 API_VERSION_1_1，可安全调用。
    unsafe { instance.get_physical_device_features2(physical_device, &mut features2) };

    DescriptorUpdateCaps {
        uniform_buffer_update_after_bind: feats12.descriptor_binding_uniform_buffer_update_after_bind
            != vk::FALSE,
        sampled_image_update_after_bind: feats12.descriptor_binding_sampled_image_update_after_bind
            != vk::FALSE,
        vulkan_1_2: feats12.descriptor_indexing != vk::FALSE,
    }
}

/// 首选 GPU 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuPreference {
    /// 独显。
    Discrete,
    /// 核显。轻负载程序更适合，避免占用独显。
    Integrated,
    /// 不指定，由 Vulkan 自行选择。
    Auto,
}

impl GpuPreference {
    /// 解析环境变量取值。无法识别时返回 `Auto`。
    pub fn from_env() -> Self {
        match std::env::var(ENV_GPU_PREF)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "discrete" | "d" | "独显" => GpuPreference::Discrete,
            "integrated" | "i" | "核显" => GpuPreference::Integrated,
            _ => GpuPreference::Auto,
        }
    }
}

/// GPU 选择偏好环境变量。
pub const ENV_GPU_PREF: &str = "MODULARCLIPBOARD_GPU";

/// 已初始化的 Vulkan 设备上下文。
pub struct Gpu {
    #[allow(dead_code)]
    entry: Entry,
    pub instance: AshInstance,
    surface_loader: SurfaceLoader,
    surface: vk::SurfaceKHR,
    pub physical_device: vk::PhysicalDevice,
    pub properties: vk::PhysicalDeviceProperties,
    pub device_type: vk::PhysicalDeviceType,
    /// 图形队列族索引。
    pub queue_family: u32,
    pub queue: vk::Queue,
    pub device: ash::Device,
    pub swapchain_loader: khr::swapchain::Device,
    /// 表面支持的能力（图像数、尺寸范围）。
    pub surface_caps: vk::SurfaceCapabilitiesKHR,
    pub surface_formats: Vec<vk::SurfaceFormatKHR>,
    pub present_modes: Vec<vk::PresentModeKHR>,
    /// 设备支持的描述符更新能力（决定能否加 `UPDATE_AFTER_BIND`）。
    pub desc_caps: DescriptorUpdateCaps,
}

impl Gpu {
    /// 创建实例、表面并选择物理设备与逻辑设备。
    ///
    /// `hinstance` / `hwnd` 为 Win32 句柄。
    pub fn new(
        app_name: &str,
        hinstance: windows::Win32::Foundation::HINSTANCE,
        hwnd: windows::Win32::Foundation::HWND,
    ) -> anyhow::Result<Self> {
        let entry = unsafe { Entry::load()? };
        tracing::debug!("Vulkan 动态库加载成功");

        let app_c = CString::new(app_name)?;
        let mut app_info = vk::ApplicationInfo::default()
            .application_name(&app_c)
            .application_version(vk::make_api_version(0, 1, 0, 0))
            .engine_name(c"modular-clipboard")
            .engine_version(vk::make_api_version(0, 1, 0, 0))
            // 请求 1.2：描述符的 `UPDATE_AFTER_BIND` 是 1.2 核心特性，
            // 而本项目每帧在多帧在途的同时会重写所有槽位的描述符
            // （见 `DescriptorUpdateCaps`）。
            //
            // 保持 1.1 会让验证层报
            // `VUID-VkDescriptorSetLayoutCreateInfo-flags-parameter`
            // ——「这些标志位需要 VK_EXT_descriptor_indexing 扩展」：
            // 1.1 实例下这些 1.2 核心标志位不被识别，被当作扩展才有的。
            //
            // `load()` 不校验版本，`create_instance` 才校验；设备实际支持到
            // 1.4（已实测），1.2 是安全下限。真要退回 1.1，得同时改用
            // `VK_EXT_descriptor_indexing` 扩展路径，不能只降版本号。
            .api_version(vk::API_VERSION_1_2);

        // 实例级扩展：只有「表面」相关的两个。
        // 注意 VK_KHR_swapchain 是**设备级**扩展，不能在实例创建时启用——
        // 误加会让 create_instance 报 "Extension specified does not exist"。
        // 名称大小写敏感：swapchain 全小写，写成 VK_KHR_swap_chain 同样失败。
        let surface_ext = c"VK_KHR_surface";
        let win32_ext = c"VK_KHR_win32_surface";
        let exts = [surface_ext.as_ptr(), win32_ext.as_ptr()];

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&mut app_info)
            .enabled_extension_names(&exts);

        let instance = unsafe { entry.create_instance(&create_info, None)? };
        tracing::debug!("Vulkan 实例创建成功");

        let surface_loader = SurfaceLoader::new(&entry, &instance);
        let surface_info = vk::Win32SurfaceCreateInfoKHR {
            hinstance: hinstance.0 as _,
            hwnd: hwnd.0 as _,
            ..Default::default()
        };
        // win32_surface 需要自己的 loader：ash 0.38 起各扩展 loader 独立持有
        // 函数指针，不再共享实例 loader 的 functor。
        let win32_loader = khr::win32_surface::Instance::new(&entry, &instance);
        let surface = unsafe { win32_loader.create_win32_surface(&surface_info, None)? };
        tracing::debug!("Vulkan 表面创建成功");

        let candidates = unsafe { instance.enumerate_physical_devices()? };
        if candidates.is_empty() {
            anyhow::bail!("未找到可用的 Vulkan 物理设备。请确认显卡驱动已包含 Vulkan 支持。");
        }

        let pref = GpuPreference::from_env();
        let (physical_device, properties, device_type, queue_family) =
            select_device(&instance, &surface_loader, surface, &candidates, pref)?;

        let name: Vec<u8> = properties
            .device_name
            .iter()
            .map(|c| *c as u8)
            .take_while(|b| *b != 0)
            .collect();
        let name = String::from_utf8_lossy(&name).to_string();
        let kind = match device_type {
            vk::PhysicalDeviceType::DISCRETE_GPU => "独显",
            vk::PhysicalDeviceType::INTEGRATED_GPU => "核显",
            vk::PhysicalDeviceType::VIRTUAL_GPU => "虚拟 GPU",
            vk::PhysicalDeviceType::CPU => "软件渲染",
            _ => "其它",
        };
        tracing::info!(
            device = %name,
            kind,
            api_major = vk::api_version_major(properties.api_version),
            api_minor = vk::api_version_minor(properties.api_version),
            "已选择 Vulkan 物理设备"
        );

        // 查询表面能力，决定交换链格式与呈现模式
        let caps =
            unsafe { surface_loader.get_physical_device_surface_capabilities(physical_device, surface)? };
        let formats =
            unsafe { surface_loader.get_physical_device_surface_formats(physical_device, surface)? };
        let present_modes = unsafe {
            surface_loader
                .get_physical_device_surface_present_modes(physical_device, surface)?
        };
        tracing::debug!(
            surface_formats = formats.len(),
            present_modes = present_modes.len(),
            min_images = caps.min_image_count,
            "表面能力查询完成"
        );

        let queue_priorities = [1.0f32];
        let device_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&queue_priorities);

        // ---- 描述符更新能力：查询后再按实际支持启用 ----
        //
        // 不查询就无条件置位会得到 `VK_ERROR_FEATURE_NOT_PRESENT`（设备创建直接失败）；
        // 不启用就加 `UPDATE_AFTER_BIND_BIT` 则是「使用未启用的 feature」——
        // 违规从 `03047` 换成另一个 VUID，等于没修。
        let desc_caps = query_descriptor_update_caps(&instance, physical_device);
        // 只启用**确实需要**的那两项：uniform 与 sampled image（采样器 + 纹理）。
        // `descriptor_binding_update_unused_while_pending` 本项目用不到
        // （见 `DescriptorUpdateCaps` 的说明），故不启用。
        let mut feats12 = vk::PhysicalDeviceVulkan12Features::default();
        feats12.descriptor_binding_uniform_buffer_update_after_bind =
            desc_caps.uniform_buffer_update_after_bind.into();
        feats12.descriptor_binding_sampled_image_update_after_bind =
            desc_caps.sampled_image_update_after_bind.into();

        let device_exts = [c"VK_KHR_swapchain".as_ptr()];
        let device_create = vk::DeviceCreateInfo::default()
            .queue_create_infos(std::slice::from_ref(&device_info))
            .enabled_extension_names(&device_exts)
            // 链入 1.2 特性。结构体须存活到 create_device 返回，故绑定为局部变量。
            .push_next(&mut feats12);

        let device = unsafe { instance.create_device(physical_device, &device_create, None)? };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let swapchain_loader = khr::swapchain::Device::new(&instance, &device);

        Ok(Self {
            entry,
            instance,
            surface_loader,
            surface,
            physical_device,
            properties,
            device_type,
            queue_family,
            queue,
            device,
            swapchain_loader,
            surface_caps: caps,
            surface_formats: formats,
            present_modes,
            desc_caps,
        })
    }

    /// 表面句柄。
    pub fn surface(&self) -> vk::SurfaceKHR {
        self.surface
    }

    /// 本机所有可用的 Vulkan 设备，供设置页展示。
    pub fn enumerate_devices(
        _entry: &Entry,
        instance: AshInstance,
    ) -> Vec<(String, vk::PhysicalDeviceType)> {
        unsafe { instance.enumerate_physical_devices() }
            .unwrap_or_default()
            .iter()
            .filter_map(|d| {
                let p = unsafe { instance.get_physical_device_properties(*d) };
                let name: Vec<u8> = p
                    .device_name
                    .iter()
                    .map(|c| *c as u8)
                    .take_while(|b| *b != 0)
                    .collect();
                Some((String::from_utf8_lossy(&name).to_string(), p.device_type))
            })
            .collect()
    }

    /// 等待设备空闲。交换链重建前后必须调用。
    pub fn wait_idle(&self) {
        if let Err(e) = unsafe { self.device.device_wait_idle() } {
            tracing::warn!(?e, "等待设备空闲失败");
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

/// 按偏好选择物理设备与队列族。
///
/// 队列族必须同时支持 `GRAPHICS` 与 `PRESENT`，否则无法把画面显示到窗口。
fn select_device(
    instance: &AshInstance,
    surface_loader: &SurfaceLoader,
    surface: vk::SurfaceKHR,
    devices: &[vk::PhysicalDevice],
    pref: GpuPreference,
) -> anyhow::Result<(
    vk::PhysicalDevice,
    vk::PhysicalDeviceProperties,
    vk::PhysicalDeviceType,
    u32,
)> {
    let mut best: Option<(i32, vk::PhysicalDevice, vk::PhysicalDeviceProperties, u32)> = None;

    for dev in devices {
        let (props, families) = unsafe { (
            instance.get_physical_device_properties(*dev),
            instance.get_physical_device_queue_family_properties(*dev),
        ) };

        // 找一个同时支持图形与呈现的队列族
        let queue_family = (0..families.len() as u32).find(|&i| {
            families[i as usize]
                .queue_flags
                .contains(vk::QueueFlags::GRAPHICS)
                && unsafe {
                    surface_loader
                        .get_physical_device_surface_support(*dev, i, surface)
                        .unwrap_or(false)
                }
        });
        let Some(queue_family) = queue_family else {
            tracing::debug!("跳过不支持呈现的设备");
            continue;
        };

        // 评分：偏好匹配最高，其次独显
        let score = match (pref, props.device_type) {
            (GpuPreference::Discrete, vk::PhysicalDeviceType::DISCRETE_GPU) => 100,
            (GpuPreference::Integrated, vk::PhysicalDeviceType::INTEGRATED_GPU) => 100,
            (GpuPreference::Auto, vk::PhysicalDeviceType::DISCRETE_GPU) => 60,
            (GpuPreference::Auto, _) => 50,
            (_, vk::PhysicalDeviceType::DISCRETE_GPU) => 40,
            _ => 10,
        };

        if best.as_ref().is_none_or(|(s, ..)| score > *s) {
            best = Some((score, *dev, props, queue_family));
        }
    }

    let (_, pd, props, qf) =
        best.ok_or_else(|| anyhow::anyhow!("没有同时支持图形与呈现的 Vulkan 设备"))?;
    let dtype = props.device_type;
    Ok((pd, props, dtype, qf))
}

/// 交换链。
pub struct Swapchain {
    pub handle: vk::SwapchainKHR,
    pub images: Vec<vk::Image>,
    pub image_views: Vec<vk::ImageView>,
    /// 与图像视图一一对应的 framebuffer。
    ///
    /// 渲染通道必须绑定 framebuffer 而非裸图像视图；传空句柄是未定义行为，
    /// 驱动会直接崩溃。
    pub framebuffers: Vec<vk::Framebuffer>,
    pub format: vk::Format,
    pub extent: vk::Extent2D,
    pub present_mode: vk::PresentModeKHR,
    pub image_count: u32,
    loader: khr::swapchain::Device,
}

impl Swapchain {
    /// 为给定尺寸创建交换链。
    ///
    /// `render_pass` 需已创建——framebuffer 依赖它的附件描述。
    /// `caps` 必须是**当前**的表面能力。
    ///
    /// # 为什么不能退回用 `gpu.surface_caps`
    ///
    /// `gpu.surface_caps` 是 `Gpu::new` 时的**快照**。
    /// 窗口 resize 后某些驱动会改变 `min/max_image_extent`
    /// （例如最小尺寸随窗口装饰、DPI 变化而调整）。
    /// 用旧快照 clamp 会得到一个**按过期范围裁剪过的**尺寸，
    /// `vkCreateSwapchainKHR` 于是报
    /// `VUID-VkSwapchainCreateInfoKHR-pNext-07781`
    /// （imageExtent 必须在**当前**的 min/max 之间）。
    pub fn new(
        gpu: &Gpu,
        width: u32,
        height: u32,
        render_pass: vk::RenderPass,
        caps: &vk::SurfaceCapabilitiesKHR,
    ) -> anyhow::Result<Self> {
        // 调用方（`resolve_extent`）已按 caps clamp 过，
        // 这里再clamp 一次作为纵深防御——
        // 用**传入的** caps，不是快照。
        let extent = vk::Extent2D {
            width: width.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
            height: height.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
        };

        let (format, color_space) = pick_format(&gpu.surface_formats)?;
        let present_mode = pick_present_mode(&gpu.present_modes);

        // 图像数量：min+1 可避免驱动等待，但不能超过上限
        let mut image_count = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            image_count = image_count.min(caps.max_image_count);
        }

        let create_info = vk::SwapchainCreateInfoKHR::default()
            .surface(gpu.surface())
            .min_image_count(image_count)
            .image_format(format)
            .image_color_space(color_space)
            .image_extent(extent)
            .image_array_layers(1)
            // 合成沿用 FIFO 变换，避免对不支持的变换做额外拷贝
            .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(present_mode)
            .clipped(true)
            .old_swapchain(vk::SwapchainKHR::null())
            // 后续由本渲染器自行渲染到中间图像
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_DST);

        let handle = unsafe { gpu.swapchain_loader.create_swapchain(&create_info, None)? };
        let images = unsafe { gpu.swapchain_loader.get_swapchain_images(handle)? };
        let image_count = images.len() as u32;

        let mut image_views = Vec::with_capacity(images.len());
        for image in &images {
            image_views.push(create_image_view(gpu, *image, format)?);
        }

        // framebuffer 必须在图像视图与渲染通道都就绪后创建
        let mut framebuffers = Vec::with_capacity(image_views.len());
        for view in &image_views {
            let attachments = [*view];
            let fb_info = vk::FramebufferCreateInfo::default()
                .render_pass(render_pass)
                .attachments(&attachments)
                .width(extent.width)
                .height(extent.height)
                .layers(1);
            let fb = unsafe { gpu.device.create_framebuffer(&fb_info, None) }
                .map_err(|e| anyhow::anyhow!("创建 framebuffer 失败: {e:?}"))?;
            framebuffers.push(fb);
        }

        tracing::debug!(
            ?format,
            present_mode = ?present_mode,
            width = extent.width,
            height = extent.height,
            images = images.len(),
            framebuffers = framebuffers.len(),
            "交换链创建成功"
        );

        Ok(Self {
            handle,
            images,
            image_views,
            framebuffers,
            format,
            extent,
            present_mode,
            image_count,
            loader: khr::swapchain::Device::new(&gpu.instance, &gpu.device),
        })
    }

    /// 销毁资源。需在渲染通道不再引用图像视图后调用。
    pub fn destroy(&mut self, device: &Device) {
        unsafe {
            // 顺序很重要：framebuffer 引用了图像视图，
            // 必须先销毁 framebuffer 再销毁视图。
            for fb in &self.framebuffers {
                device.destroy_framebuffer(*fb, None);
            }
            for view in &self.image_views {
                device.destroy_image_view(*view, None);
            }
            self.loader.destroy_swapchain(self.handle, None);
        }
        self.framebuffers.clear();
        self.image_views.clear();
        self.images.clear();
    }
}

fn create_image_view(gpu: &Gpu, image: vk::Image, format: vk::Format) -> anyhow::Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
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
    Ok(unsafe { gpu.device.create_image_view(&info, None)? })
}

/// 选交换链格式与色彩空间。
///
/// 返回 `(format, color_space)`。表面只报告一种格式时必须原样采用，
/// 不能自行挑选——这是 Vulkan 规范的明确要求。
///
/// **本函数是格式选择的唯一实现**。渲染通道的附件格式必须与交换链一致，
/// 任何地方需要预知交换链格式都必须调用它，不得另写一份
/// （历史上frame.rs 曾有一份副本，两份逻辑一旦分叉会导致
/// 「渲染通道格式 ≠ 交换链格式」，驱动在 `cmd_begin_render_pass` 时崩溃）。
pub(crate) fn pick_format(
    formats: &[vk::SurfaceFormatKHR],
) -> anyhow::Result<(vk::Format, vk::ColorSpaceKHR)> {
    if formats.is_empty() {
        anyhow::bail!("表面未报告任何可用格式");
    }
    // 单格式时直接使用，格式与色彩空间一并取出
    if formats.len() == 1 {
        let f = formats[0];
        return Ok((f.format, f.color_space));
    }
    formats
        .iter()
        .find(|f| {
            matches!(
                f.format,
                vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM
            )
        })
        .map(|f| (f.format, f.color_space))
        .ok_or_else(|| anyhow::anyhow!("表面不支持可用的颜色格式"))
}

/// 选呈现模式。
///
/// 剪贴板工具优先 FIFO（垂直同步、功耗最低），不支持再退MAILBOX。
fn pick_present_mode(modes: &[vk::PresentModeKHR]) -> vk::PresentModeKHR {
    if modes.contains(&vk::PresentModeKHR::FIFO) {
        vk::PresentModeKHR::FIFO
    } else if modes.contains(&vk::PresentModeKHR::MAILBOX) {
        vk::PresentModeKHR::MAILBOX
    } else {
        tracing::warn!("没有理想的呈现模式，使用第一个可用的");
        modes.first().copied().unwrap_or(vk::PresentModeKHR::FIFO)
    }
}

/// 构造图像布局过渡屏障。
/// **crate 私有**。
///
/// 外部调用方（探针、示例）改布局**必须**走
/// [`texture::DeviceImage::transition_to`] / `prepare_for_readback` /
/// `restore_after_readback` —— 它们会同步维护 `DeviceImage::layout`。
///
/// # 为什么不设为 pub
///
/// 本夜的实际故障：探针为图省事直接调本函数改布局，
/// 却没回写 `image.layout` 字段。下一次 `upload` 读到的
/// `old_layout` 因此是**过期的**，用它算出的布局屏障
/// 基于错误的前置状态 ⇒ 随机 device lost。
///
/// 症状极具迷惑性：`full_app`（每帧 1 次上传）20/20 通过，
/// 只有 `upload_probe`（每帧 1~3 次，布局频繁切换）才偶发失败。
/// 这类 bug 靠 review 抓不住——**只能在编译期拦住**。
///
/// `pipeline_probe` 仍在用（它是最小复现用例，不持有 `DeviceImage`），
/// 已在 crate 内做了 `pub(crate)` 例外处理。
/// 供探针与示例使用：录一条**一次性**图像布局屏障。
///
/// 与 crate 内部的 [`image_barrier`] 的区别：调用方**自己**保证
/// 布局状态正确（典型场景是探针手工驱动整个流程，不持有
/// [`texture::DeviceImage`]）。
///
/// 持有 `DeviceImage` 的代码**必须**用
/// [`texture::DeviceImage::transition_to`] —— 它会同步 `layout` 字段。
/// 用本函数绕过会在状态失同步时导致随机 device lost。
///
/// # Safety
///
/// `device` 必须是创建 `image` 的那个设备；调用方必须确保
/// 记录的 `old_layout` 与 GPU 上的实际布局一致，且已正确
/// 安排 `src_stage` / `dst_stage` / 访问掩码。
pub unsafe fn record_one_shot_image_barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
    src_stage: vk::PipelineStageFlags,
    dst_stage: vk::PipelineStageFlags,
) {
    let barrier = image_barrier(image, old_layout, new_layout, src_access, dst_access);
    // 本 crate 开启了 `unsafe_op_in_unsafe_fn`，
    // 即使在 `unsafe fn` 内部也要求显式 `unsafe` 块。
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            src_stage,
            dst_stage,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            std::slice::from_ref(&barrier),
        );
    }
}

pub(crate) fn image_barrier<'a>(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
) -> vk::ImageMemoryBarrier<'a> {
    vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        // 队列族设为 IGNORED，语义等价于「不变换归属」
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desc_caps_require_both_features() {
        // 三 个绑定（uniform / sampler / texture）都要能加标志，
        // 缺任一项就不能加——否则该绑定在多帧在途时重写仍会违规。
        let both = DescriptorUpdateCaps {
            uniform_buffer_update_after_bind: true,
            sampled_image_update_after_bind: true,
            vulkan_1_2: true,
        };
        assert!(both.all_bindings_update_after_bind());

        // 缺 sampler 侧（COMBINED_IMAGE_SAMPLER 走 sampled image 特性）
        let no_sampler = DescriptorUpdateCaps {
            uniform_buffer_update_after_bind: true,
            sampled_image_update_after_bind: false,
            vulkan_1_2: true,
        };
        assert!(!no_sampler.all_bindings_update_after_bind());

        let no_uniform = DescriptorUpdateCaps {
            uniform_buffer_update_after_bind: false,
            sampled_image_update_after_bind: true,
            vulkan_1_2: true,
        };
        assert!(!no_uniform.all_bindings_update_after_bind());
    }

    #[test]
    fn desc_caps_default_to_no_update_after_bind() {
        // 能力全无时必须退回「不加标志」，而不是乐观假设支持。
        // 乐观假设的后果是设备创建直接失败（VK_ERROR_FEATURE_NOT_PRESENT）
        // 或布局创建违规。
        let none = DescriptorUpdateCaps {
            uniform_buffer_update_after_bind: false,
            sampled_image_update_after_bind: false,
            vulkan_1_2: false,
        };
        assert!(!none.all_bindings_update_after_bind());
    }

    #[test]
    fn gpu_preference_parsing_rules() {
        // 直接测试解析规则，不依赖进程环境
        fn parse(v: &str) -> GpuPreference {
            match v.trim().to_ascii_lowercase().as_str() {
                "discrete" | "d" | "独显" => GpuPreference::Discrete,
                "integrated" | "i" | "核显" => GpuPreference::Integrated,
                _ => GpuPreference::Auto,
            }
        }
        assert_eq!(parse("discrete"), GpuPreference::Discrete);
        assert_eq!(parse("INTEGRATED"), GpuPreference::Integrated);
        assert_eq!(parse(" 独显 "), GpuPreference::Discrete);
        assert_eq!(parse("garbage"), GpuPreference::Auto);
        assert_eq!(parse(""), GpuPreference::Auto);
    }

    #[test]
    fn present_mode_prefers_fifo() {
        assert_eq!(
            pick_present_mode(&[vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::FIFO]),
            vk::PresentModeKHR::FIFO
        );
        assert_eq!(
            pick_present_mode(&[vk::PresentModeKHR::MAILBOX]),
            vk::PresentModeKHR::MAILBOX
        );
        // 空列表不应 panic
        assert_eq!(pick_present_mode(&[]), vk::PresentModeKHR::FIFO);
    }

    fn sf(format: vk::Format) -> vk::SurfaceFormatKHR {
        vk::SurfaceFormatKHR {
            format,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        }
    }

    #[test]
    fn format_selection_rejects_unsupported() {
        assert!(pick_format(&[]).is_err());
        // 单格式时必须原样采用（规范要求）
        assert_eq!(
            pick_format(&[sf(vk::Format::R8G8B8A8_UNORM)]).unwrap().0,
            vk::Format::R8G8B8A8_UNORM
        );
        // 多格式时挑选 8 位 RGBA/BGRA
        let both = [sf(vk::Format::R16G16B16A16_SFLOAT), sf(vk::Format::B8G8R8A8_UNORM)];
        assert_eq!(pick_format(&both).unwrap().0, vk::Format::B8G8R8A8_UNORM);
        // 注意：单格式列表即使不是 8 位 RGBA 也必须接受——
        // Vulkan 规范要求此时原样使用驱动报告的唯一格式。
        assert_eq!(
            pick_format(&[sf(vk::Format::D32_SFLOAT)]).unwrap().0,
            vk::Format::D32_SFLOAT
        );
        // 多格式且都不支持 8 位 RGBA 时才报错
        assert!(pick_format(&[sf(vk::Format::D32_SFLOAT), sf(vk::Format::R16G16B16A16_SFLOAT)]).is_err());
    }

    #[test]
    fn color_space_comes_from_selected_format() {
        // ash 只为 SRGB_NONLINEAR 生成了常量；用 from_raw 构造任意色彩空间，
        // 验证选中格式的色彩空间被原样透传（而非被硬编码覆盖）。
        let custom = vk::ColorSpaceKHR::from_raw(100);
        let f = vk::SurfaceFormatKHR {
            format: vk::Format::B8G8R8A8_UNORM,
            color_space: custom,
        };
        let (_, cs) = pick_format(&[f]).unwrap();
        assert_eq!(cs, custom);
    }

    #[test]
    fn image_barrier_uses_ignored_queue_family() {
        let b = image_barrier(
            vk::Image::null(),
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_WRITE,
        );
        assert_eq!(b.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
        assert_eq!(b.dst_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
        assert_eq!(b.subresource_range.layer_count, 1);
        assert_eq!(b.subresource_range.level_count, 1);
    }
}
