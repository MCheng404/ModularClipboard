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

pub mod backdrop;
pub mod capture_worker;
pub mod blur;
pub mod buffer;
pub mod chrome;
pub mod frame;
pub mod glass;
pub mod glass_pass;
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
/// > `UPDATE_UNUSED_WHILE_PENDING_BIT`，其描述符集就不得被处于
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

/// 超采样倍率环境变量。
pub const ENV_RENDER_SCALE: &str = "MODULARCLIPBOARD_SSAA";

/// 默认超采样倍率。
///
/// 取 2.0：界面先渲染到 2× 分辨率的离屏目标，再线性降采样到交换链。
/// 这样**几何边缘（圆角、描边）与字体**同时获得抗锯齿——MSAA 只能
/// 改善几何，而字形的锯齿来自纹理采样，MSAA 无能为力。
///
/// 代价是片元工作量 ×4。本程序的界面以大面积纯色矩形为主，
/// 720×800 的窗口放大到 1440×1600 也只有 2.3M 像素，桌面 GPU 毫无压力。
pub const DEFAULT_RENDER_SCALE: f32 = 2.0;

/// 超采样倍率上限对应的 **ppp 上限**。
///
/// # 为什么必须有这个上限
///
/// 超采样通过抬高 egui 的 `pixels_per_point` 实现（见
/// [`crate::window::EventLoop::egui_input`]）。实测：`ppp = 2.25` 时
/// 界面一切正常，`ppp = 3.0` 时**图标字形被画坏**——齿轮丢齿、
/// 垃圾桶丢盖、图钉丢针，看起来像「只画出了字形中心、外围被裁掉」。
///
/// 根因尚未完全定位（已排除字体图集容量：上限从 2048 提到设备的
/// 16384 后现象不变）。在查清之前，用这个上限把 ppp 挡在已知安全的
/// 区间内——`ppp = 3.0` 恰好是 **200% DPI 屏 × 倍率 1.5** 会踩到的值，
/// 不加限制等于给高分屏用户埋雷。
///
/// 取值 2.25 是**实测安全边界**，不是理论值。
pub const MAX_RENDER_PPP: f32 = 2.25;

/// 读取超采样倍率（进程内缓存）。
///
/// 取值来自 [`ENV_RENDER_SCALE`]，缺省 [`DEFAULT_RENDER_SCALE`]。
/// 结果钳到 `[1.0, 3.0]`：
/// - **下限 1.0** 表示关闭超采样，直接渲染到交换链（旧的、最省的做法），
///   这是出问题时的逃生开关；
/// - 上限 3.0 防止误设成 100 之类的值把显存吃光。
///
/// 非法值（非数字、NaN）回落到默认值而不是报错——这是显示质量选项，
/// 不该因为一个环境变量写错就让程序起不来。
pub fn render_scale() -> f32 {
    use std::sync::OnceLock;
    static CACHE: OnceLock<f32> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let raw = std::env::var(ENV_RENDER_SCALE).ok();
        let v = raw
            .as_deref()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(DEFAULT_RENDER_SCALE)
            .clamp(1.0, 3.0);
        if let Some(r) = raw.as_deref() {
            tracing::info!(raw = r, used = v, "超采样倍率（{ENV_RENDER_SCALE}）");
        }
        v
    })
}

/// 按窗口 DPI缩放求**实际生效**的超采样倍率。
///
/// 与 [`render_scale`] 的区别：这里把「`ppp = dpi_scale ×倍率` 不超过
/// [`MAX_RENDER_PPP`]」这条约束算进去。
///
/// # 为什么窗口层与渲染层必须调用同一个函数
///
/// 渲染目标尺寸 = `客户区物理像素 × 倍率`，而 `ppp = dpi_scale × 倍率`。
/// 两处若各算各的（例如窗口层夹了上限、渲染层没夹），
/// `screen_rect × ppp == 渲染目标像素` 这条不变式就断了——
/// 表现是画面整体缩放错误，且**不报任何 GPU 错误**。
///
/// # 为什么允许倍率低于 1.0
///
/// 早前的写法是 `min(want, (MAX/s).max(1.0))`：那个 `.max(1.0)` 是为了
/// 「DPI 已经很高时不再超采样」，但它同时把**上限本身架空**了——
///
/// ```text
/// s = 2.5（250% DPI）⇒ MAX/s = 0.9 → 被抬回 1.0 ⇒ ppp = 2.5  > 2.25
/// s = 3.0（300% DPI）⇒ MAX/s = 0.75 → 被抬回 1.0 ⇒ ppp = 3.0 > 2.25
/// ```
///
/// 而 `ppp > 2.25` 恰好是 [`MAX_RENDER_PPP`] 记录的**图标字形被画坏**
/// 区间（齿轮丢齿、垃圾桶丢盖）。也就是说：300% DPI 屏上这份「保护」
/// 反而把人推进了坏区间。物理像素本来已经足够密，超采样对该屏只有
/// 副作用，所以正确做法是**降到 1.0 以下**（渲染目标小于交换链），
/// 让 `ppp` 回到已知安全区间。
///
/// 代价是 250%/300% 屏上画面按 `MAX/s` 的比例**欠采样**后再放大，
/// 边缘略软。这比字形画坏好得多，且只在超高 DPI 时发生。
pub fn render_scale_for(dpi_scale: f32) -> f32 {
    let s = if dpi_scale > 0.01 { dpi_scale } else { 1.0 };
    let want = render_scale();
    // ppp = s * rs ≤ MAX_RENDER_PPP ⇒ rs ≤ MAX_RENDER_PPP / s。
    // 注意这里**不钳下限 1.0**：那是架空上限的根源（见上文）。
    // 只在结果小到毫无意义（配置被手改成极大 render_scale 且 DPI 极高）
    // 时兜一个正数下限，避免 render target 退化成 0 像素。
    let ceiling = MAX_RENDER_PPP / s;
    // 下限与上限联动（P2-2）：s > MAX_RENDER_PPP/0.25（=9）时 ceiling
    // 已低于 0.25，若仍硬抬到 0.25，ppp = 0.25·s 会突破 MAX_RENDER_PPP
    // ——恰好落进图标字形画坏区间。下限取 `ceiling.min(0.25)`：
    // s≤9 时行为与旧版一致；s>9 时贴住上限。
    let floor = ceiling.min(0.25);
    want.min(ceiling).max(floor)
}

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
        let app_info = vk::ApplicationInfo::default()
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
            .application_info(&app_info)
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
        let mut feats12 = vk::PhysicalDeviceVulkan12Features {
            descriptor_binding_uniform_buffer_update_after_bind: desc_caps
                .uniform_buffer_update_after_bind
                .into(),
            descriptor_binding_sampled_image_update_after_bind: desc_caps
                .sampled_image_update_after_bind
                .into(),
            ..Default::default()
        };

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

    /// 为**另一个窗口**创建 Vulkan 表面。
    ///
    /// # 为什么需要这个方法
    ///
    /// [`Gpu`] 在 [`Self::new`] 里为传入的 `hwnd` 建了一个表面，
    /// 而 [`crate::frame::Swapchain`] 建交换链时固定用
    /// [`Self::surface`]。于是「新窗口复用这个 `Gpu`」会撞上
    /// `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`——同一个 `hwnd` 只能
    /// 绑定一个表面。
    ///
    /// 多窗口（卡片拖出成独立子窗口）必须**一个窗口一个表面**，
    /// 但设备、队列、管线仍然共享。
    ///
    /// 返回的表面由调用方负责在不再使用前 `destroy`。
    pub fn create_surface_for(
        &self,
        hinstance: windows::Win32::Foundation::HINSTANCE,
        hwnd: windows::Win32::Foundation::HWND,
    ) -> anyhow::Result<vk::SurfaceKHR> {
        let create_info = vk::Win32SurfaceCreateInfoKHR {
            // ash 0.38 起各扩展 loader 独立持有函数指针，
            // 与 `Gpu::new` 里建主窗口表面时同一套写法。
            hinstance: hinstance.0 as _,
            hwnd: hwnd.0 as _,
            ..Default::default()
        };
        let entry = unsafe { Entry::load()? };
        let win32_loader = khr::win32_surface::Instance::new(&entry, &self.instance);
        // SAFETY：`create_info` 的两个句柄在调用期间有效；
        // `self.instance` 由本结构持有且未销毁。
        let surface = unsafe { win32_loader.create_win32_surface(&create_info, None)? };
        tracing::debug!(?hwnd, ?surface, "为新窗口创建 Vulkan 表面");
        Ok(surface)
    }

    /// 查询某个表面支持的交换链格式与能力。
    ///
    /// 与 [`Self::new`] 里对主窗口表面做的是同一套查询，
    /// 抽出来供子窗口复用——**必须复用**，否则两处各查一次，
    /// 挑出的格式可能不一致，而不一致会在 `FrameRenderer::new`
    /// 的校验里报错。
    pub fn query_surface_formats(
        &self,
        surface: vk::SurfaceKHR,
    ) -> anyhow::Result<Vec<vk::SurfaceFormatKHR>> {
        // SAFETY：`surface` 必须是本实例创建且尚未销毁的表面。
        // 调用方负责保证这一点（本方法只读，不延长生命周期）。
        unsafe {
            self.surface_loader
                .get_physical_device_surface_formats(self.physical_device, surface)
                .map(|f| f.into_iter().collect())
                .map_err(|e| anyhow::anyhow!("查询表面格式失败: {e}"))
        }
    }

    /// 本机所有可用的 Vulkan 设备，供设置页展示。
    pub fn enumerate_devices(
        _entry: &Entry,
        instance: AshInstance,
    ) -> Vec<(String, vk::PhysicalDeviceType)> {
        unsafe { instance.enumerate_physical_devices() }
            .unwrap_or_default()
            .iter()
            .map(|d| {
                let p = unsafe { instance.get_physical_device_properties(*d) };
                let name: Vec<u8> = p
                    .device_name
                    .iter()
                    .map(|c| *c as u8)
                    .take_while(|b| *b != 0)
                    .collect();
                (String::from_utf8_lossy(&name).to_string(), p.device_type)
            })
            .collect()
    }

    /// 等待设备空闲。交换链重建前后必须调用。
    pub fn wait_idle(&self) {
        if let Err(e) = unsafe { self.device.device_wait_idle() } {
            tracing::warn!(?e, "等待设备空闲失败");
        }
    }
    /// 销毁由 [`Self::create_surface_for`] 创建的表面。
    ///
    /// # 为什么必须有这个入口
    ///
    /// `create_surface_for` 的文档写着「返回的表面由调用方负责在不再
    /// 使用前 destroy」，但 `Gpu` 早前**没有**提供销毁方法，
    /// `FrameRenderer::drop` 也只销毁交换链/池/命令池、不动 surface。
    /// 于是 UI 侧 `ChildWindow` / `SettingsWindow` 建出来的
    /// `vk::SurfaceKHR` 句柄创建后即丢失，**每次创建一个窗口泄漏一个**
    /// （含 DWM 侧的呈现资源）。
    ///
    /// 设置窗口是「每次开关都新建」（关掉时 `settings_win = None`），
    /// 所以反复开关设置会持续泄漏。
    ///
    /// IMPORTANT: 调用前必须 `device_wait_idle()`：交换链可能还在引用该表面。
    ///
    /// # Safety
    ///
    /// `surface` 必须是本 `Gpu` 创建的、且尚未销毁的句柄。
    pub unsafe fn destroy_surface(&self, surface: vk::SurfaceKHR) {
        if surface == vk::SurfaceKHR::null() {
            return;
        }
        // 复用已有的等待助手（它自己处理了错误日志）。
        self.wait_idle();
        // SAFETY: 调用方保证 surface 来自本 Gpu 且未销毁；本函数是 unsafe。
        unsafe { self.surface_loader.destroy_surface(surface, None) };
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
    /// 把 `v`夹进 `[lo, hi]`；`lo > hi`（驱动报告异常）时退回 `lo`。
///
/// 与 `frame.rs::clamp_axis` 同义。`f32::clamp` 在 `lo > hi` 时会 panic，
/// 而「驱动报告的上下限反了」不该让程序崩——最坏结果只是那一次
/// 建出的交换链尺寸不理想。
fn clamp_axis(v: u32, lo: u32, hi: u32) -> u32 {
    if lo > hi {
        return lo;
    }
    v.clamp(lo, hi)
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
        // 主窗口：表面与格式都用 `Gpu` 里已查好的。
        Self::new_for_surface(
            gpu,
            gpu.surface(),
            &gpu.surface_formats,
            &gpu.present_modes,
            width,
            height,
            render_pass,
            caps,
        )
    }

    /// 为**指定表面**创建交换链（多窗口用）。
    ///
    /// # 为什么必须有这个变体
    ///
    /// [`Self::new`] 固定用 `gpu.surface()`——那是 `Gpu::new` 时为主窗口
    /// 建的表面。而 [`vk::SurfaceKHR`] 与 `hwnd` 一一对应，
    /// **同一个 hwnd 只能绑一个表面**：拿主窗口的表面去给子窗口建交换链，
    /// `vkCreateSwapchainKHR` 会报 `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`
    /// （实测子窗口创建时必现）。
    ///
    /// 所以每个窗口都要：用自己的 hwnd 建表面 → 查它的格式与呈现模式 →
    /// 调本方法。
    #[allow(clippy::too_many_arguments)]
    pub fn new_for_surface(
        gpu: &Gpu,
        surface: vk::SurfaceKHR,
        formats: &[vk::SurfaceFormatKHR],
        present_modes: &[vk::PresentModeKHR],
        width: u32,
        height: u32,
        render_pass: vk::RenderPass,
        caps: &vk::SurfaceCapabilitiesKHR,
    ) -> anyhow::Result<Self> {
        // 调用方（`resolve_extent`）已按 caps clamp 过，
        // 这里再clamp 一次作为纵深防御——
        // 用**传入的** caps，不是快照。
        let extent = vk::Extent2D {
            // CAUTION: 不用 `clamp`：它在 `min > max` 时会 panic。
            //
            // 驱动报告 `min_image_extent > max_image_extent` 是**可能发生**
            // 的异常（能力查询出错、或某些虚拟化驱动）。`frame.rs` 的
            // `clamp_axis` 早就专门处理过这种情况（退化为取下限），
            // 这里却漏了——同一个文件里的同类逻辑必须一致。
            width: clamp_axis(width, caps.min_image_extent.width, caps.max_image_extent.width),
            height: clamp_axis(height, caps.min_image_extent.height, caps.max_image_extent.height),
        };

        let (format, color_space) = pick_format(formats)?;
        let present_mode = pick_present_mode(present_modes);

        // 图像数量：min+1 可避免驱动等待，但不能超过上限
        let mut image_count = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            image_count = image_count.min(caps.max_image_count);
        }

        // 合成 alpha：优先 PRE_MULTIPLIED。
        //
        // 磨砂（Acrylic）模式要求 DWM 按**像素 alpha** 合成——背景层
        // 画半透明时透出 blur。`OPAQUE` 下 DWM 忽略像素 alpha，磨砂
        // 会被背景完全盖住（用户可见为「磨砂没生效」）。
        //
        // 对**不透明**内容（默认绘制、子窗口、设置窗口）PRE_MULTIPLIED
        // 与 OPAQUE 显示完全一致，因此全局切到 PRE_MULTIPLIED 是安全的；
        // 若驱动不支持则回退 OPAQUE（透明背景失效但程序不崩）。
        let composite_alpha =
            if caps.supported_composite_alpha.contains(vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED) {
                vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED
            } else {
                vk::CompositeAlphaFlagsKHR::OPAQUE
            };
        tracing::info!(
            supported = ?caps.supported_composite_alpha,
            chosen = ?composite_alpha,
            format = ?format,
            "交换链合成 alpha 选择"
        );

        let create_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(image_count)
            .image_format(format)
            .image_color_space(color_space)
            .image_extent(extent)
            .image_array_layers(1)
            // 合成沿用 FIFO 变换，避免对不支持的变换做额外拷贝
            .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
            .composite_alpha(composite_alpha)
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
// 参数逐一对应 VkImageMemoryBarrier，是 FFI 的忠实镜像；打包结构体的
// churn 大于收益（同 texture.rs::transition_to 的取舍）。
#[allow(clippy::too_many_arguments)]
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

/// 为一张图像视图创建 framebuffer。
///
/// 抽成公共助手的原因：交换链与离屏目标都要建 framebuffer，而
/// Vulkan 的 framebuffer **必须绑定渲染通道**（不是裸图像视图）——
/// 传空句柄或漏绑会让驱动在 `cmd_begin_render_pass` 时直接崩
/// （0xC0000005，本项目的历史坑）。两处各写一份迟早分叉。
pub fn create_framebuffer(
    device: &Device,
    render_pass: vk::RenderPass,
    view: vk::ImageView,
    extent: vk::Extent2D,
) -> anyhow::Result<vk::Framebuffer> {
    // 尺寸为 0 的 framebuffer 是非法对象（规范要求 width/height ≥ 1），
    // 而驱动往往不报错、只在首次使用时崩，因此在这里提前拦下。
    anyhow::ensure!(
        extent.width > 0 && extent.height > 0,
        "framebuffer 尺寸必须大于 0，得到 {extent:?}"
    );
    let attachments = [view];
    let info = vk::FramebufferCreateInfo::default()
        .render_pass(render_pass)
        .attachments(&attachments)
        .width(extent.width)
        .height(extent.height)
        .layers(1);
    unsafe { device.create_framebuffer(&info, None) }
        .map_err(|e| anyhow::anyhow!("创建 framebuffer 失败: {e:?}"))
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

    /// **`ppp` 不得超过 [`MAX_RENDER_PPP`]——任何 DPI 下。**
    ///
    /// 回归守卫：`render_scale_for` 早先是
    /// `min(want, (MAX/s).max(1.0))`，那个 `.max(1.0)` 把上限架空了：
    ///
    /// ```text
    /// s = 2.5 ⇒ 2.25/2.5 = 0.9 → 抬回 1.0 ⇒ ppp = 2.5 > 2.25
    /// s = 3.0 ⇒ 2.25/3.0 = 0.75 → 抬回 1.0 ⇒ ppp = 3.0 > 2.25
    /// ```
    ///
    /// 而 `ppp > 2.25` 正是 `MAX_RENDER_PPP` 记录的**图标字形被画坏**
    /// 区间（齿轮丢齿、垃圾桶丢盖）——这份「保护」在高 DPI 屏上反而
    /// 亲手把人推进了坏区间。修法是允许倍率低于 1.0（欠采样）。
    ///
    /// 用例覆盖到 300%（Windows 缩放的实际档位上限之一），并顺带
    /// 钉住「脏输入不产生脏倍率」这条性质。
    #[test]
    fn render_ppp_never_exceeds_cap_at_any_dpi() {
        // 量程含 s > MAX_RENDER_PPP/0.25 = 9 的理论极端（P2-2，2026-10-19
        // DPI 治理）：该区间下限必须**贴住上限**（ceiling.min(0.25)），
        // 不能再硬抬到 0.25——否则 ppp = 0.25·s 突破 MAX_RENDER_PPP，
        // 恰好落进图标字形画坏区间。
        for s in [
            0.5_f32, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 2.75, 3.0, 4.0, 9.0, 12.0,
            16.0,
        ] {
            let rs = render_scale_for(s);
            let ppp = s * rs;
            assert!(
                ppp <= MAX_RENDER_PPP + 1e-4,
                "DPI {s}: ppp={ppp} 超过上限 {MAX_RENDER_PPP}（倍率 {rs}）"
            );
            // 倍率必须为正且有限。下限语义是 `min(ceiling, 0.25)`（随
            // DPI 浮动）——s>9 时允许低于 0.25，以贴住 ppp 上限优先。
            assert!(
                rs > 0.0 && rs.is_finite(),
                "DPI {s}: 倍率 {rs} 非法（会退化成无意义的渲染目标）"
            );
        }
        // 脏输入（0 / 负数 / NaN）必须按 1.0 处理，不得产出 NaN 倍率。
        for s in [0.0, -1.0, f32::NAN] {
            let rs = render_scale_for(s);
            assert!(rs.is_finite() && rs > 0.0, "脏 DPI {s} 产出非法倍率 {rs}");
        }
    }

    /// 常规 DPI（≤200%）下倍率仍是「超采样」方向，不因本次修复而改变。
    ///
    /// 单独列出是因为修复把下限从 1.0 放开了——必须确认**只有超高 DPI**
    /// 才走到欠采样那条路，否则 100% 屏用户会莫名拿到 0.9 倍画面。
    #[test]
    fn render_scale_stays_supersampled_on_normal_dpi() {
        for s in [1.0_f32, 1.25, 1.5, 1.75, 2.0] {
            let rs = render_scale_for(s);
            assert!(
                rs >= 1.0,
                "DPI {s}: 倍率 {rs} 不该欠采样（该区间物理像素仍值得超采样）"
            );
            // 仍然受上限约束。
            assert!(s * rs <= MAX_RENDER_PPP + 1e-4, "DPI {s} 超过上限");
        }
    }

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
