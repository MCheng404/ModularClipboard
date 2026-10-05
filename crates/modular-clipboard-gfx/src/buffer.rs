//! 缓冲与其后备显存。
//!
//! # 为什么每个缓冲独占一次分配
//!
//! Vulkan 允许把多个资源绑到同一块`vk::DeviceMemory` 的不同偏移上
//! （子分配），能显著减少分配次数。但本项目的缓冲数量是个位数
//! （顶点、索引、每帧 uniform、若干 staging），这点开销可以忽略，
//! 而子分配会引入「谁负责偏移对齐」「释放时要不要拆分」两层复杂度。
//! 因此这里走「一个缓冲一次分配」的简单路线，把复杂度留给真正需要的地方。
//!
//! # 销毁顺序
//!
//! [`Buffer`] 不实现 `Drop`——它需要`&Device` 才能销毁，而 `Gpu::Drop`
//! 会先销毁逻辑设备。因此必须由调用方显式 [`Buffer::destroy`]，
//! 且**调用前必须已`device_wait_idle`**，否则设备可能仍在读取这块显存。
//! 这一约束由 [`crate::Gpu::wait_idle`] 提供。

use ash::{Device, vk};

use crate::pipeline::Uniforms;
use crate::Gpu;

/// 索引类型。
///
/// egui 单帧的图元数量轻松突破 65535（中文字体图集 + 大量 clip rect
/// 会产生上万个三角形），16 位索引会绕回，必须用 32 位。
pub const INDEX_TYPE: vk::IndexType = vk::IndexType::UINT32;

/// uniform 缓冲区的最小对齐。
///
/// Vulkan 规范要求`VkPhysicalDeviceLimits::min_uniform_buffer_offset_alignment`
/// 的整数倍。桌面驱动上通常是 256，规格下限是 64。
///
/// 用 [`Gpu::properties`] 里的真实值而非硬编码，避免在低端设备上
/// 描述符偏移非法导致驱动行为未定义。
pub fn uniform_buffer_alignment(gpu: &Gpu) -> vk::DeviceSize {
    let a = gpu.properties.limits.min_uniform_buffer_offset_alignment;
    // 规范保证该值 ≥ 1；万一驱动报告 0，兜底为 1（等价于无对齐要求），
    // 免得后续的取整运算除零。
    a.max(1)
}

/// 把字节数向上取整到 `alignment` 的倍数。
///
/// `alignment` 为 0 时返回 `value` 原值——调用方若从驱动查询得到 0，
/// 不应因此崩在这里。
pub fn align_up(value: vk::DeviceSize, alignment: vk::DeviceSize) -> vk::DeviceSize {
    if alignment == 0 {
        return value;
    }
    value.div_ceil(alignment) * alignment
}

/// 选出满足 `type_mask` 与 `required` 的内存类型索引。
///
/// 纯函数：只依赖传入的 [`vk::PhysicalDeviceMemoryProperties`]，不碰设备，
/// 因此可以注入伪造的属性表做单测。
///
/// # 参数
/// - `props`：物理设备的内存属性，通常来自
///   `instance.get_physical_device_memory_properties`。
/// - `type_mask`：资源通过`memory_type_bits` 声明「我接受哪些类型」的位掩码。
///   Vulkan 规范用它在 [`find_memory_type`] 的候选里做交集。
/// - `required`：必须全部具备的属性。
/// - `preferred`：若存在同时满足 `required` 与 `preferred` 的类型就优先用它。
///   独立显卡与核显混用的机器上，显存（`DEVICE_LOCAL`）与共享内存
///   （`HOST_CISIBLE`）性能差一个数量级，能挑显存就挑显存。
///
/// 找不到时返回 `None`——宁可让调用方明确失败，也不要悄悄退回共享内存：
/// 后者会让「能跑」掩盖「慢十倍」。
pub fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_mask: u32,
    required: vk::MemoryPropertyFlags,
    preferred: vk::MemoryPropertyFlags,
) -> Option<u32> {
    let count = (props.memory_type_count as usize).min(props.memory_types.len());
    let mut fallback = None;

    for i in 0..count {
        // 资源的 memory_type_bits 第 i 位为 0 表示不兼容该类型。
        if type_mask & (1 << i) == 0 {
            continue;
        }
        let flags = props.memory_types[i].property_flags;
        if !flags.contains(required) {
            continue;
        }
        if flags.contains(preferred) {
            return Some(i as u32);
        }
        fallback.get_or_insert(i as u32);
    }

    fallback
}

/// 一块`vk::Buffer` 及其绑定的 [`vk::DeviceMemory`]。
pub struct Buffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// 分配时的字节数（已按`req.alignment` 向上取整）。
    size: vk::DeviceSize,
    /// 创建时的用途位。存下来是为了 [`Buffer::device_address`] 能自查
    /// 是否具备前提条件——usage 在创建后不可查询，丢了就没法判断了。
    usage: vk::BufferUsageFlags,
    /// 映射后的裸指针，仅在 [`Buffer::is_mapped`] 为真时有效。
    mapped: Option<*mut u8>,
    /// 内存是否`HOST_COHERENT`。非相干时 [`Buffer::flush`] 不可省。
    coherent: bool,
}

impl Buffer {
    /// 创建缓冲并绑定一块合适的内存。
    ///
    /// `usage` 需包含实际会用到的用途位（如 `VERTEX_BUFFER`），
    /// 否则 `bind_vertex_buffers` 会报 `ERROR_PERMISSION_DENIED`。
    pub fn new(
        gpu: &Gpu,
        size: vk::DeviceSize,
        usage: vk::BufferUsageFlags,
        sharing: vk::SharingMode,
    ) -> anyhow::Result<Self> {
        Self::new_with_memory(
            gpu,
            size,
            usage,
            sharing,
            vk::MemoryPropertyFlags::empty(),
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
    }

    /// 创建显式要求驻留在主机可见内存中的缓冲。
    ///
    /// 用于 staging（CPU 写入后交给 GPU 拷贝）与 uniform（每帧改写）。
    /// 强制要求 `HOST_COHERENT`：非相干内存每次改写都要 [`Buffer::flush`]，
    /// 而本项目的写入频率是每帧多次，隐式flush 的心智负担大于省下的那点开销。
    pub fn new_host_visible(
        gpu: &Gpu,
        size: vk::DeviceSize,
        usage: vk::BufferUsageFlags,
        sharing: vk::SharingMode,
    ) -> anyhow::Result<Self> {
        Self::new_with_memory(
            gpu,
            size,
            usage,
            sharing,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            // 主机缓存的内存读取更快，但独立显卡上可能没有这种类型，
            // 因此只作为偏好而非要求。
            vk::MemoryPropertyFlags::HOST_CACHED,
        )
    }

    /// 通用构造函数。`required` 必须满足，`preferred`尽量满足。
    pub fn new_with_memory(
        gpu: &Gpu,
        size: vk::DeviceSize,
        usage: vk::BufferUsageFlags,
        sharing: vk::SharingMode,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(size > 0, "缓冲大小必须大于 0");

        let device = &gpu.device;

        let info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(sharing)
            // CONCURRENT 模式才需要列出队列族；EXCLUSIVE 下留空。
            .queue_family_indices(&[]);

        let buffer = unsafe { device.create_buffer(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建 {size} 字节缓冲失败: {e:?}"))?;

        // 从这里开始任何一步失败都必须销毁已创建的 buffer，否则泄漏。
        // 用一个显式的守卫结构把清理逻辑收在一处。
        let mut guard = BufferGuard {
            device: &gpu.device,
            buffer: Some(buffer),
            memory: None,
        };

        let req = unsafe { device.get_buffer_memory_requirements(buffer) };

        // 分配大小向上取整到 alignment：
        // vkBindBufferMemory 的 offset 必须是 alignment 的倍数，
        // 分配大小同样按对齐向上取整才不会在后续子分配时越界。
        let alloc_size = align_up(req.size, req.alignment);

        let props = unsafe {
            gpu.instance
                .get_physical_device_memory_properties(gpu.physical_device)
        };
        let type_index = find_memory_type(&props, req.memory_type_bits, required, preferred)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "找不到满足条件的内存类型（需要 {required:?}，资源允许的位掩码 {:#x}）",
                    req.memory_type_bits
                )
            })?;

        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(alloc_size)
            .memory_type_index(type_index);
        let memory = unsafe { device.allocate_memory(&alloc, None) }.map_err(|e| {
            // 显存不足是最常见的失败原因，值得单独提示。
            if e == vk::Result::ERROR_OUT_OF_DEVICE_MEMORY || e == vk::Result::ERROR_OUT_OF_HOST_MEMORY
            {
                anyhow::anyhow!("分配 {alloc_size} 字节显存失败（显存不足）: {e:?}")
            } else {
                anyhow::anyhow!("分配 {alloc_size} 字节显存失败: {e:?}")
            }
        })?;
        guard.memory = Some(memory);

        unsafe { device.bind_buffer_memory(buffer, memory, 0) }
            .map_err(|e| anyhow::anyhow!("绑定显存到缓冲失败: {e:?}"))?;

        let coherent = props.memory_types[type_index as usize]
            .property_flags
            .contains(vk::MemoryPropertyFlags::HOST_COHERENT);

        tracing::debug!(
            size,
            alloc_size,
            alignment = req.alignment,
            memory_type = type_index,
            coherent,
            "缓冲创建成功"
        );

        // 所有权移交：buffer 与 memory 都已创建且绑定成功。
        guard.buffer = None;
        guard.memory = None;

        Ok(Self {
            buffer,
            memory,
            size: alloc_size,
            usage,
            mapped: None,
            coherent,
        })
    }

    /// 句柄。
    pub fn handle(&self) -> vk::Buffer {
        self.buffer
    }

    /// 内存句柄。写描述符时需要。
    pub fn memory(&self) -> vk::DeviceMemory {
        self.memory
    }

    /// 实际分配大小（已对齐）。
    pub fn size(&self) -> vk::DeviceSize {
        self.size
    }

    /// 创建时声明的用途位。
    pub fn usage(&self) -> vk::BufferUsageFlags {
        self.usage
    }

    /// 内存是否主机相干。非相干时 [`Buffer::flush`] 不可省。
    pub fn is_coherent(&self) -> bool {
        self.coherent
    }

    /// 当前是否处于映射状态。
    pub fn is_mapped(&self) -> bool {
        self.mapped.is_some()
    }

    /// 映射整块内存。
    ///
    /// 返回的指针在 [`Buffer::unmap`] 或 [`Buffer::destroy`] 前有效。
    /// 重复映射是调用方的逻辑错误——规范规定此时必须返回空指针，
    /// 这里提前拦截以给出更清楚的错误。
    pub fn map(&mut self, device: &Device) -> anyhow::Result<*mut u8> {
        anyhow::ensure!(!self.is_mapped(), "缓冲已处于映射状态");
        let ptr = unsafe { device.map_memory(self.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }
            .map_err(|e| anyhow::anyhow!("映射 {} 字节显存失败: {e:?}", self.size))?;
        // map_memory 返回的是 c_void 裸指针，偏移 0 起始、覆盖整个分配。
        let ptr = ptr.cast::<u8>();
        self.mapped = Some(ptr);
        Ok(ptr)
    }

    /// 把字节写入主机可见内存。
    ///
    /// 内部完成 map → 拷贝 → unmap 全流程，调用方无需关心映射生命周期。
    /// 数据在返回时已对设备可见（`HOST_COHERENT` 时直接可见，
    /// 否则 [`Buffer::unmap`] 内部会 flush）。
    pub fn write(
        &mut self,
        device: &Device,
        offset: vk::DeviceSize,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.is_mapped(),
            "缓冲已处于映射状态，不能在映射期间写入"
        );
        let end = offset
            .checked_add(bytes.len() as vk::DeviceSize)
            .ok_or_else(|| anyhow::anyhow!("写入偏移 {} 加上长度 {} 溢出", offset, bytes.len()))?;
        anyhow::ensure!(
            end <= self.size,
            "写入区间 [{offset}, {end}) 超出缓冲容量 {}",
            self.size
        );

        let ptr = self.map(device)?;
        // SAFETY:
        // - `ptr` 来自本缓冲的 map_memory，偏移 0，覆盖整个分配，
        //   因此 [ptr+offset, ptr+end) 落在映射区间内（上面已校验 end <= size）。
        // - `bytes` 是借用自调用方的不可变切片，存活到本函数结束，
        //   而拷贝是同步的 mem::copy，不持有指针。
        // - `ptr` 由当前线程独占（映射状态已断言），无并发写同一映射的别名问题。
        //   Vulkan 规范要求映射的内存不要被其它主机访问同时读写。
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(offset as usize), bytes.len()) };

        self.unmap(device)
    }

    /// 解除映射。非`HOST_COHERENT` 内存会先自动 flush。
    pub fn unmap(&mut self, device: &Device) -> anyhow::Result<()> {
        if self.mapped.is_none() {
            return Ok(());
        }
        if !self.coherent {
            self.flush(device)?;
        }
        unsafe { device.unmap_memory(self.memory) };
        self.mapped = None;
        Ok(())
    }

    /// 把主机改动刷到设备可见内存。
    ///
    /// `HOST_COHERENT` 内存无需调用——此方法直接返回成功，
    /// 让调用方无需为两种内存类型写两套代码。
    pub fn flush(&self, device: &Device) -> anyhow::Result<()> {
        if self.coherent {
            return Ok(());
        }
        // 非相干内存的 flush 区间必须按 nonCoherentAtomSize 对齐。
        // 整块刷是最简单也最不容易出错的做法。
        let range = vk::MappedMemoryRange::default()
            .memory(self.memory)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        unsafe { device.flush_mapped_memory_ranges(&[range]) }
            .map_err(|e| anyhow::anyhow!("刷新映射内存失败: {e:?}"))
    }

    /// 从设备可见内存读回内容（截图、读回测试用）。
    pub fn read<T: bytemuck::Pod>(&self) -> anyhow::Result<Vec<T>> {
        anyhow::ensure!(
            self.is_mapped(),
            "读回前必须先映射缓冲（map）"
        );
        let bytes = self.size as usize;
        anyhow::ensure!(
            bytes % size_of::<T>() == 0,
            "缓冲大小 {} 不是 {} 的整数倍，无法按该类型读回",
            bytes,
            size_of::<T>()
        );
        let ptr = self.mapped.expect("已确认处于映射状态");
        // SAFETY: 映射区间覆盖整块分配（offset 0，size 为分配大小），
        // 因此指针至少指向 size 个有效字节；上面已确认 size 是
        // size_of::<T>() 的倍数，按 T 读取不会越过界。
        Ok(unsafe { std::slice::from_raw_parts(ptr.cast::<T>(), bytes / size_of::<T>()) }.to_vec())
    }

    /// 供 SSBO 使用的设备地址。
    ///
    /// # 前置条件
    /// 设备需满足其一：
    /// - API 版本 ≥ 1.2（核心功能），或
    /// - 启用了 `VK_KHR_buffer_device_address` 扩展**且**打开了
    ///   `bufferDeviceAddress` 特性；
    ///
    /// 并且本缓冲创建时的 usage 含 `SHADER_DEVICE_ADDRESS`。
    ///
    /// 不满足时返回 `Err` 而**不是**让驱动崩掉：`vkGetBufferDeviceAddress`
    /// 属于 Vulkan 1.2 核心入口，而本项目 `Gpu::new` 只申请了 1.1 基线。
    /// ash 在函数指针取不到时安装的是一个 **panic 桩**，直接调用会 abort。
    pub fn device_address(&self, gpu: &Gpu) -> anyhow::Result<vk::DeviceAddress> {
        anyhow::ensure!(
            self.usage.contains(vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS),
            "取设备地址要求缓冲 usage 含 SHADER_DEVICE_ADDRESS"
        );
        anyhow::ensure!(
            vk::api_version_major(gpu.properties.api_version) > 1
                || vk::api_version_minor(gpu.properties.api_version) >= 2,
            "取设备地址需要 Vulkan 1.2 或 VK_KHR_buffer_device_address，\
             当前设备 API 版本为 {}.{}.{}",
            vk::api_version_major(gpu.properties.api_version),
            vk::api_version_minor(gpu.properties.api_version),
            vk::api_version_patch(gpu.properties.api_version),
        );

        let info = vk::BufferDeviceAddressInfo::default().buffer(self.buffer);
        let addr = unsafe { gpu.device.get_buffer_device_address(&info) };
        anyhow::ensure!(
            addr != 0,
            "驱动返回了空设备地址：bufferDeviceAddress 特性可能未启用"
        );
        Ok(addr)
    }

    /// 供描述符写入使用的 [`vk::DescriptorBufferInfo`]。
    pub fn descriptor_info(&self) -> vk::DescriptorBufferInfo {
        vk::DescriptorBufferInfo::default()
            .buffer(self.buffer)
            .offset(0)
            .range(self.size)
    }

    /// 销毁缓冲与显存。
    ///
    /// # 前置条件
    /// 调用方**必须**已经 `device_wait_idle`——设备可能仍在读取这块显存，
    /// 提前释放是未定义行为。约定俗成的做法是在`Gpu` 层面调用
    /// [`crate::Gpu::wait_idle`] 后再逐个销毁。
    pub fn destroy(&mut self, device: &Device) {
        if let Err(e) = self.unmap(device) {
            tracing::warn!(?e, "销毁前解除映射失败");
        }
        unsafe {
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
        self.buffer = vk::Buffer::null();
        self.memory = vk::DeviceMemory::null();
        self.size = 0;
        self.mapped = None;
    }
}

/// 创建过程中的清理守卫。
///
/// Rust 没有 `?` 的自动析构（没有 RAII guard 惯用法），
/// 这里手写一个：任一步失败返回 `Err` 时，`Drop` 会销毁已创建的资源。
struct BufferGuard<'a> {
    device: &'a Device,
    buffer: Option<vk::Buffer>,
    memory: Option<vk::DeviceMemory>,
}

impl Drop for BufferGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            if let Some(memory) = self.memory.take() {
                self.device.free_memory(memory, None);
            }
            if let Some(buffer) = self.buffer.take() {
                self.device.destroy_buffer(buffer, None);
            }
        }
    }
}

/// 顶点缓冲。内部委托给 [`Buffer`]。
///
/// 数据布局与 `pipeline.rs` 中声明的顶点输入一致：
/// `pos: vec2<f32>` + `uv: vec2<f32>` + `color: u32`，共 20 字节。
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Vertex {
    /// 屏幕空间位置（逻辑像素）。
    pub pos: [f32; 2],
    /// 纹理坐标。
    pub uv: [f32; 2],
    /// 顶点色，打包为 ABGR 小端序的 `u32`。
    pub color: u32,
}

/// 顶点步长（字节）。
pub const VERTEX_STRIDE: vk::DeviceSize = size_of::<Vertex>() as vk::DeviceSize;

pub struct VertexBuffer {
    buffer: Buffer,
    /// 容量（顶点数），不是字节数。
    capacity: usize,
}

impl VertexBuffer {
    /// 创建能容纳 `capacity` 个顶点的缓冲。
    pub fn new(gpu: &Gpu, capacity: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(capacity > 0, "顶点缓冲容量必须大于 0");
        let size = capacity as vk::DeviceSize * VERTEX_STRIDE;
        let buffer = Buffer::new(
            gpu,
            size,
            vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            vk::SharingMode::EXCLUSIVE,
        )?;
        Ok(Self { buffer, capacity })
    }

    /// 顶点容量。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 是否装得下 `count` 个顶点。
    pub fn fits(&self, count: usize) -> bool {
        count <= self.capacity
    }

    /// 内部缓冲。
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn handle(&self) -> vk::Buffer {
        self.buffer.handle()
    }

    pub fn destroy(&mut self, device: &Device) {
        self.buffer.destroy(device);
        self.capacity = 0;
    }
}

/// 索引缓冲。索引类型固定为 [`INDEX_TYPE`]。
pub struct IndexBuffer {
    buffer: Buffer,
    capacity: usize,
}

impl IndexBuffer {
    /// 创建能容纳 `capacity` 个索引的缓冲。
    pub fn new(gpu: &Gpu, capacity: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(capacity > 0, "索引缓冲容量必须大于 0");
        let size = capacity as vk::DeviceSize * size_of::<u32>() as vk::DeviceSize;
        let buffer = Buffer::new(
            gpu,
            size,
            vk::BufferUsageFlags::INDEX_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            vk::SharingMode::EXCLUSIVE,
        )?;
        Ok(Self { buffer, capacity })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn fits(&self, count: usize) -> bool {
        count <= self.capacity
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn handle(&self) -> vk::Buffer {
        self.buffer.handle()
    }

    pub fn destroy(&mut self, device: &Device) {
        self.buffer.destroy(device);
        self.capacity = 0;
    }
}

/// uniform 缓冲。持有 [`Uniforms`] 的CPU 副本以便随时重写。
///
/// 分配在主机可见内存中：uniform 每帧都要改写，走staging 拷贝会多出
/// 一次 GPU往返，得不偿失。
pub struct UniformBuffer {
    buffer: Buffer,
    data: Uniforms,
}

impl UniformBuffer {
    /// 创建能容纳一个 [`Uniforms`] 的缓冲。
    pub fn new(gpu: &Gpu) -> anyhow::Result<Self> {
        let raw = size_of::<Uniforms>() as vk::DeviceSize;
        // 分配大小按驱动要求的对齐向上取整。虽然只有一个 uniform，
        // 但按规范分配可避免后续扩成多槽时越界。
        let size = align_up(raw, uniform_buffer_alignment(gpu));
        let buffer = Buffer::new_host_visible(
            gpu,
            size,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            vk::SharingMode::EXCLUSIVE,
        )?;
        Ok(Self {
            buffer,
            data: Uniforms::new([0.0; 16], [1.0, 1.0], 1.0),
        })
    }

    /// 写入新的 uniform 值。
    pub fn write(&mut self, device: &Device, data: &Uniforms) -> anyhow::Result<()> {
        self.data = *data;
        let bytes = bytemuck::bytes_of(&self.data);
        self.buffer.write(device, 0, bytes)
    }

    /// 直接写入原始字节，供 frame 层做批量更新。
    pub fn write_raw(&mut self, device: &Device, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            bytes.len() as vk::DeviceSize <= self.buffer.size(),
            "写入 {} 字节超出 uniform 缓冲容量 {}",
            bytes.len(),
            self.buffer.size()
        );
        self.buffer.write(device, 0, bytes)
    }

    /// CPU 侧的副本。
    pub fn data(&self) -> &Uniforms {
        &self.data
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn descriptor_info(&self) -> vk::DescriptorBufferInfo {
        // 范围只覆盖结构体本身，不含对齐产生的尾部填充——
        // 着色器按WGSL 声明的结构体大小读取，多报会让校验层报警。
        self.buffer.descriptor_info().range(size_of::<Uniforms>() as vk::DeviceSize)
    }

    pub fn destroy(&mut self, device: &Device) {
        self.buffer.destroy(device);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一份可注入的内存属性表。
    fn props(entries: &[(vk::MemoryPropertyFlags, u32)]) -> vk::PhysicalDeviceMemoryProperties {
        let mut p = vk::PhysicalDeviceMemoryProperties::default();
        p.memory_type_count = entries.len() as u32;
        for (i, (flags, heap)) in entries.iter().enumerate() {
            p.memory_types[i] = vk::MemoryType {
                property_flags: *flags,
                heap_index: *heap,
            };
        }
        p
    }

    const D: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
    const H: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::HOST_VISIBLE;
    const C: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::HOST_COHERENT;
    const K: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::HOST_CACHED;

    #[test]
    fn picks_device_local_when_available() {
        // 典型双显卡布局：0=显存，1=共享内存
        let p = props(&[(D, 0), (H | C, 1)]);
        let idx = find_memory_type(&p, 0b11, D, D).unwrap();
        assert_eq!(idx, 0, "应优先选显存");
    }

    #[test]
    fn empty_required_falls_back_to_shared_memory() {
        // 核显机器：只有共享内存。Buffer::new 传的 required 是空集
        // （任何类型都接受），DEVICE_LOCAL 只是偏好，因此应正常退化。
        let p = props(&[(H | C, 0)]);
        let idx = find_memory_type(&p, 0b1, vk::MemoryPropertyFlags::empty(), D).unwrap();
        assert_eq!(idx, 0, "偏好不满足时必须能退化到共享内存");
    }

    #[test]
    fn unsatisfiable_required_returns_none() {
        // 与上面相反：若调用方把 DEVICE_LOCAL 当硬要求
        // （如 new_host_visible 要求 HOST_VISIBLE）而设备不满足，
        // 必须返回 None 让调用方明确失败。
        // 悄悄退回共享内存会让「能跑」掩盖「慢一个数量级」。
        let p = props(&[(D, 0)]);
        assert!(
            find_memory_type(&p, 0b1, H | C, H | C).is_none(),
            "硬要求主机可见但不满足时不应返回任何类型"
        );
    }

    #[test]
    fn respects_type_mask_intersection() {
        // 资源只接受类型 1（掩码 0b10），即使类型 0 更「完美」也不能选
        let p = props(&[(D, 0), (H | C, 1)]);
        let idx = find_memory_type(&p, 0b10, H | C, H | C).unwrap();
        assert_eq!(idx, 1, "必须遵守资源声明的 memory_type_bits");
    }

    #[test]
    fn prefers_host_cached_but_not_required() {
        // 0=共享但不可缓存，1=共享且可缓存 → 应选 1
        let p = props(&[(H | C, 0), (H | C | K, 0)]);
        let idx = find_memory_type(&p, 0b11, H | C, K).unwrap();
        assert_eq!(idx, 1, "HOST_CACHED 应作为偏好生效");
        // 若没有可缓存类型，退回任一满足 required 的
        let p2 = props(&[(H | C, 0)]);
        assert_eq!(find_memory_type(&p2, 0b1, H | C, K).unwrap(), 0);
    }

    #[test]
    fn no_match_returns_none() {
        let p = props(&[(D, 0)]);
        // 要求主机可见但只有显存
        assert!(find_memory_type(&p, 0b1, H, H).is_none());
        // 掩码指向不存在的类型
        assert!(find_memory_type(&p, 0b1000, D, D).is_none());
        // 空表
        assert!(find_memory_type(&props(&[]), 0b1, D, D).is_none());
    }

    #[test]
    fn ignores_types_beyond_declared_count() {
        // memory_type_count 声称只有 1 个类型，但底层数组里第 1 项是显存。
        // 若实现忽略了 count 就会错选第 1 项。
        let mut p = props(&[(H | C, 0)]);
        p.memory_types[1] = vk::MemoryType {
            property_flags: D,
            heap_index: 0,
        };
        assert_eq!(find_memory_type(&p, 0b11, H | C, D).unwrap(), 0);
    }

    #[test]
    fn align_up_rounds_to_multiple() {
        assert_eq!(align_up(0, 256), 0);
        assert_eq!(align_up(1, 256), 256);
        assert_eq!(align_up(256, 256), 256);
        assert_eq!(align_up(257, 256), 512);
        // 非 2 的幂次对齐
        assert_eq!(align_up(10, 3), 12);
        assert_eq!(align_up(12, 3), 12);
        // 对齐为 0 时原样返回，避免除零 panic
        assert_eq!(align_up(123, 0), 123);
    }

    #[test]
    fn uniform_allocation_covers_struct_with_padding() {
        // 无法在此构造 Gpu，因此只验证纯逻辑：
        // 对齐后的分配大小必须 ≥ 结构体大小。
        let raw = size_of::<Uniforms>() as vk::DeviceSize;
        for alignment in [1u64, 64, 128, 256, 512] {
            let alloc = align_up(raw, alignment);
            assert!(alloc >= raw, "对齐后反而变小了：{alloc} < {raw}");
            assert_eq!(alloc % alignment, 0, "分配大小必须是 {alignment} 的倍数");
        }
    }

    #[test]
    fn vertex_layout_matches_pipeline_attributes() {
        // pipeline.rs 声明：location0 offset 0、location1 offset 8、
        // location2 offset 16，步长 = 2*4+2*4+4 = 20。
        assert_eq!(size_of::<Vertex>(), 20, "顶点结构体大小与管线步长不符");
        assert_eq!(VERTEX_STRIDE, 20);
        let v = Vertex {
            pos: [1.0, 2.0],
            uv: [0.5, 0.25],
            color: 0xFF00_00FF,
        };
        let base = &v as *const Vertex as usize;
        let pos = &v.pos as *const _ as usize - base;
        let uv = &v.uv as *const _ as usize - base;
        let color = &v.color as *const _ as usize - base;
        assert_eq!((pos, uv, color), (0, 8, 16), "字段偏移与管线属性描述不符");
    }

    #[test]
    fn index_type_is_32_bit() {
        // 16 位索引在 egui 图元数上会绕回，必须是 UINT32
        assert_eq!(INDEX_TYPE, vk::IndexType::UINT32);
        assert_eq!(size_of::<u32>(), 4);
    }

    #[test]
    fn buffer_sizes_scale_with_capacity() {
        // 1000 个顶点 = 20000 字节；1024 个索引 = 4096 字节
        let vb = 1000u64 * VERTEX_STRIDE;
        assert_eq!(vb, 20_000);
        let ib = 1024u64 * size_of::<u32>() as u64;
        assert_eq!(ib, 4096);
    }
}
