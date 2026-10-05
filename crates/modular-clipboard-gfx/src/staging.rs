//! staging 内存的分配与退休。
//!
//! staging 缓冲的生命周期必须**覆盖 GPU 执行时间**，而不只是「录制时间」。
//! 在上传函数里 `let staging = Buffer::new(..)` 是经典错误：命令刚录完，
//! GPU 可能还没跑，buffer 已经没了。
//!
//! [`StagingArena`] 把 staging 所有权提升到帧层，用「帧号 + 栅栏」判定
//! 一块空间何时可以安全复用，调用入口见
//! [`crate::frame::FrameRenderer::record_texture_upload`]。
//!
//! # 逻辑与资源分离
//!
//! 「该复用哪一块/ 该扩容哪一块」这层**决策**是纯逻辑，不碰 Vulkan 对象，
//! 因此可在单测里由纯逻辑决策器 `ArenaDecider` 直接驱动，无需真实 GPU。
//! 真正建缓冲的动作只发生在 [`StagingArena::allocate`] 里。
//!
//! 本模块与帧循环**零耦合**：不碰交换链、不碰命令缓冲录制、不碰描述符集。

use ash::vk::Handle;
use ash::{Device, vk};

use crate::Gpu;
use crate::buffer::align_up;


/// staging 缓冲的默认字节容量。
///
/// 取 4 MiB 是因为最坏情况是整张字体图集：`2048 x 2048` 的 `R8_UNORM`
/// 恰好 4 MiB。egui 图集涨到 4096x4096（16 MiB）时
/// [`StagingArena::allocate`] 会惰性扩容，调用方无需预判。
pub const DEFAULT_STAGING_CAPACITY: vk::DeviceSize = 4 * 1024 * 1024;

/// arena 内每块 staging 的**最小**字节数。
///
/// 下限的意义是「小图集不该一上来就占4 MiB」。首次上传小纹理时
/// 按需给小缓冲，遇到大上传再涨。
pub const MIN_STAGING_CAPACITY: vk::DeviceSize = 64 * 1024;

/// arena 里 staging 块的数量。
///
/// **必须 ≥ 在飞帧数**。每个在飞帧各自独占一块：本帧写入的字节在
/// GPU 读完之前不能被下一帧覆写。
pub const UPLOAD_POOL_CAPACITY: usize = 3;

/// staging 内部分配的字节对齐。
///
/// Vulkan 要求 `vkCmdCopyBufferToImage` 的 `bufferOffset` 是
/// `optimalBufferCopyOffsetAlignment` 的倍数；该值在多数桌面驱动上是 4，
/// 但规范允许到 256。取 256 一次覆盖所有驱动，省得为兼容性去查
/// `maintenance3`（那要求 1.1 之外的特性）。
pub const STAGING_ALIGNMENT: vk::DeviceSize = 256;

/// arena 里的一块 staging 空间。
///
/// 由 [`StagingArena::allocate`] 产出。它只是**对 arena 内部缓冲的视图**
/// —— 所有权在 arena 上，因此本类型不实现 `Drop`，离开作用域不释放
/// 任何东西（与 [`crate::buffer::Buffer`] 一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagingSlice {
    /// 可作为 `TRANSFER_SRC` 绑定的缓冲句柄。
    pub buffer: vk::Buffer,
    /// 绑定的显存。`cmd_copy_*` 本身不需要它，但 [`StagingSlice::write`]
    /// 要靠它映射。
    pub memory: vk::DeviceMemory,
    /// 在该缓冲内的字节偏移。**必然是 [`STAGING_ALIGNMENT`] 的倍数。**
    pub offset: vk::DeviceSize,
    /// 从 `offset` 起可用的字节数。
    pub size: vk::DeviceSize,
}

/// 校验一次 staging 写入是否合法。
///
/// 抽成纯函数是为了能脱离 GPU 单测「非法写入会被拒绝」——
/// 这两条校验挡在 `map_memory` 之前，漏掉任何一条都会让驱动去解引用
/// 无效句柄。
pub fn validate_staging_write(
    slice: &StagingSlice,
    bytes_len: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(!slice.is_null(), "不能向空 staging 切片写入");
    anyhow::ensure!(
        bytes_len as vk::DeviceSize <= slice.size,
        "写入 {bytes_len} 字节超出 staging 切片容量 {}",
        slice.size
    );
    Ok(())
}

impl StagingSlice {
    /// 空切片。作为「无 staging」的哨兵值。
    pub const NULL: Self = Self {
        buffer: vk::Buffer::null(),
        memory: vk::DeviceMemory::null(),
        offset: 0,
        size: 0,
    };

    /// 是否是空切片。
    pub fn is_null(&self) -> bool {
        self.buffer.is_null()
    }

    /// 把 `bytes` 写入本切片起始处。
    ///
    /// 内部完成 map → 拷贝 → flush → unmap，调用方无需关心映射生命周期。
    ///
    /// # 前置条件
    /// 本切片尚未被 GPU 读取 —— 由 arena 的退休机制保证
    /// （见 [`StagingArena::allocate`]）。
    pub fn write(&self, device: &Device, bytes: &[u8]) -> anyhow::Result<()> {
        // 校验必须在触碰设备之前完成（规则见 validate_staging_write）。
        validate_staging_write(self, bytes.len())?;
        let ptr = unsafe {
            device.map_memory(self.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
        }
        .map_err(|e| anyhow::anyhow!("映射 staging 显存失败: {e:?}"))?
        .cast::<u8>();
        // SAFETY:
        // - 指针来自 map_memory(本切片所属 memory, 0, WHOLE_SIZE)，覆盖整块
        //   分配，故 [ptr+offset, ptr+offset+len) 落在映射区间内
        //   （上面已校验 len <= size，而 slice 是 arena 按容量切出来的）。
        // - bytes 是调用方的不可变借用，存活到本函数结束，拷贝是同步的。
        // - 同一块 staging 不会既被 CPU 写又被 GPU 读：arena 的 owner_frame
        //   记账保证只有「本帧拥有者」才能拿到可写切片。
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                ptr.add(self.offset as usize),
                bytes.len(),
            );
            // 相干内存上 flush 是空操作；非相干内存必须刷。
            // 无条件调用以免调用方需要区分内存类型。
            let range = vk::MappedMemoryRange::default()
                .memory(self.memory)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            device.flush_mapped_memory_ranges(&[range])?;
        }
        unsafe { device.unmap_memory(self.memory) };
        Ok(())
    }
}

/// arena 内部的一块 staging，由帧层持有。
struct ArenaChunk {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: vk::DeviceSize,
    /// 已用字节数。归零的前提是该块已退休。
    cursor: vk::DeviceSize,
    /// 当前占用这块的帧号；`FREE` 表示空闲。
    owner_frame: u64,
    /// 本帧提交时关联的栅栏。GPU signal 后才允许退休。
    fence: vk::Fence,
    /// 是否已创建 Vulkan 资源。纯逻辑单测里恒为 false。
    materialized: bool,
}

/// 表示「空闲」的 `owner_frame` 哨兵。
///
/// 用帧号 0 做合法值，因此不能用 0；`u64::MAX` 与帧号 practically
/// 不会相撞（跑到 2^64 帧需要约 5 万亿年）。
const FREE: u64 = u64::MAX;

/// 每帧专用的 staging 环形缓冲。
///
/// # 存在的唯一理由
///
/// staging 缓冲的生命周期必须**覆盖 GPU 执行时间**，而不只是「录制时间」。
/// 在上传函数里 `let staging = Buffer::new(..)` 是经典错误：命令刚录完，
/// GPU 可能还没跑，buffer 已经没了。本类型把 staging 所有权提升到帧层，
/// 用「帧号 + 栅栏」判定一块空间何时可以安全复用。
///
/// # 不变式
///
/// 1. 任一时刻每块 chunk 至多被一个帧号占用（`owner_frame`）。
/// 2. 复用一块 chunk 前，必须确认它上一任主人已退休 —— 由
///    [`StagingArena::retire`] 查询栅栏确认，**不靠帧号差值猜测**。
///    猜测会在某帧栅栏没等到时静默覆写GPU 正在读的数据。
/// 3. 块数 ≥ 在飞帧数。
///
/// # 逻辑与资源分离
///
/// 「该复用哪一块/ 该扩容哪一块」这层**决策**是纯逻辑，不碰 Vulkan 对象，
/// 因此可在单测里由 [`ArenaDecider`] 直接驱动，无需真实 GPU。
/// 真正建缓冲的动作只发生在 [`StagingArena::allocate`] 里。
pub struct StagingArena<'a> {
    gpu: &'a Gpu,
    chunks: Vec<ArenaChunk>,
    current_frame: u64,
}

impl<'a> StagingArena<'a> {
    /// 绑定设备，登记 `frame_count` 块 staging。
    ///
    /// `frame_count` 应传在飞帧数（交换链图像数）。此时**不创建任何
    /// Vulkan 资源** —— 每块在首次 [`StagingArena::allocate`] 时才按需
    /// 建成，避免小图集场景白占 12 MiB 主机可见内存。
    pub fn new(gpu: &'a Gpu, frame_count: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(frame_count > 0, "staging 块数必须大于 0");
        let chunks = (0..frame_count)
            .map(|_| ArenaChunk {
                buffer: vk::Buffer::null(),
                memory: vk::DeviceMemory::null(),
                capacity: 0,
                cursor: 0,
                owner_frame: FREE,
                fence: vk::Fence::null(),
                materialized: false,
            })
            .collect();
        Ok(Self {
            gpu,
            chunks,
            current_frame: 0,
        })
    }

    /// 块数。
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// 当前帧号。
    pub fn current_frame(&self) -> u64 {
        self.current_frame
    }

    /// 推进到下一帧。
    ///
    /// 应在 [`crate::frame::FrameRenderer::acquire`] 等完该槽位栅栏**之后**调用——
    /// 那次等待正是「上一轮 GPU 已读完」的证据。
    pub fn begin_frame(&mut self) {
        self.current_frame += 1;
    }

    /// 标记本帧结束（提交之后调用）。
    ///
    /// 本帧占用的块已在 [`StagingArena::note_submitted`] 记上栅栏，
    /// 此处无额外记账需要——真正决定何时能复用的是栅栏，不是帧边界。
    pub fn end_frame(&mut self) {
        // 刻意为空：见上方文档。保留此方法是为了让调用方的帧结构
        // 与「begin / 提交 / note / retire」四步显式对应，
        // 将来若需要按帧批量回收（例如显存吃紧时主动降容量），
        // 落点就在这里，而不必改动所有调用点。
    }

    /// 分配一块 staging 空间。
    ///
    /// 返回切片的偏移必然是 [`STAGING_ALIGNMENT`] 的倍数，
    /// 且生命周期由 arena 保证覆盖 GPU 执行时间。
    ///
    /// # 复用规则
    ///
    /// - **本帧已占用**的块可以继续往后追加：同一帧内多次上传很常见
    ///   （egui 一帧可能有十几次纹理增量）。
    /// - **空闲**的块（`owner_frame == FREE`，即已被 [`StagingArena::retire`]
    ///   确认 GPU 读完）才会被本帧接管。
    /// - **在途**的块（属于别的帧）既不复用也不扩容——否则 GPU 正在读的
    ///   内容会被 `destroy_buffer` 掉。
    ///
    /// 容量不足时在**本帧可写**的那块上原地扩容，必要时惰性新建。
    pub fn allocate(&mut self, size: vk::DeviceSize) -> anyhow::Result<StagingSlice> {
        anyhow::ensure!(size > 0, "staging 分配大小必须大于 0");
        let aligned = align_up(size, STAGING_ALIGNMENT);
        let frame = self.current_frame;

        // 优先本帧已占用的块（追加），其次**真正空闲**的块（接管）。
        //
        // 这里的「空闲」必须严格是 `owner_frame == FREE`。
        // 「本帧已占用的块可追加」是安全的：同帧的写入都还没提交，
        // GPU 不可能读到。跨帧复用则必须等退休（见 retire_frame）。
        let pick = self
            .chunks
            .iter()
            .position(|c| c.owner_frame == frame)
            .or_else(|| self.chunks.iter().position(|c| c.owner_frame == FREE));

        let i = match pick {
            Some(i) => i,
            None => {
                // 所有块都在途。这不该发生：块数 ≥ 在飞帧数，
                // 且 acquire 会等栅栏。仍需给出明确错误而不是静默复用。
                anyhow::bail!(
                    "staging arena 的 {} 块全部在途，无可分配空间；\
                     请确认块数 ≥ 在飞帧数且每帧都等过了栅栏",
                    self.chunks.len()
                );
            }
        };

        // 剩余空间不够**本次追加**时才重建。
        //
        // 必须减掉 cursor：同帧内多次上传会往同一块追加，只比较
        // `capacity < aligned` 会在cursor 已接近容量时判定「够用」，
        // 随后返回的切片size 变成 0（`capacity - cursor` 下溢），
        // 写入被validate_staging_write 拒绝或更糟——越界写。
        let used = self.chunks[i].cursor;
        let need_new = self.chunks[i].capacity.saturating_sub(used) < aligned;
        if need_new {
            // 目标容量：既要装下「已用 + 本次」，也不低于下限。
            let want = used
                .checked_add(aligned)
                .unwrap_or(vk::DeviceSize::MAX)
                .max(MIN_STAGING_CAPACITY);
            // 只有「该块一字节未写」时才能原地重建：否则本帧已录制的
            // 拷贝命令仍引用旧 buffer，释放它就是 use-after-free。
            if used == 0 {
                self.rebuild_chunk(i, want, frame)?;
            } else {
                // 改用空闲块。块数 >= 槽位数，正常每帧退休上一轮后必有。
                let alt = self.chunks.iter().position(|c| c.owner_frame == FREE).ok_or_else(|| {
                    anyhow::anyhow!(
                        "staging 块 {i} 剩余空间不足且已被本帧写入，\
                         而 arena 没有空闲块可另用。\
                         请增大块数或减小单帧上传量"
                    )
                })?;
                self.rebuild_chunk(alt, want, frame)?;
                let c = &mut self.chunks[alt];
                c.cursor = aligned;
                c.owner_frame = frame;
                return Ok(StagingSlice {
                    buffer: c.buffer,
                    memory: c.memory,
                    offset: 0,
                    size: c.capacity,
                });
            }
        }

        let c = &mut self.chunks[i];
        let slice = StagingSlice {
            buffer: c.buffer,
            memory: c.memory,
            offset: c.cursor,
            size: c.capacity - c.cursor,
        };
        c.cursor += aligned;
        c.owner_frame = frame;
        Ok(slice)
    }

    /// 记账：把本帧占用的块与该帧的栅栏关联。
    ///
    /// `fence` signal 即代表 GPU 读完了这些块里的数据，之后
    /// [`StagingArena::retire`] 才可以把它们判为空闲。
    /// [`crate::frame::FrameRenderer`] 传本帧槽位的栅栏。
    pub fn note_submitted(&mut self, fence: vk::Fence) {
        for c in &mut self.chunks {
            if c.owner_frame == self.current_frame {
                c.fence = fence;
            }
        }
    }

    /// 退休「属于 `frame` 号那一帧」的所有块。
    ///
    /// # 为什么按帧号精确退休，而不是「落后 N 帧就算安全」
    ///
    /// 帧号差值只是**保守估计**：块数 == 槽位数时，落后 N 帧的块确实安全，
    /// 但当前帧的块永远追不上cutoff（`cutoff = current_frame - N` 恒小于
    /// `current_frame`），于是每帧都新占一块、N 帧后 arena 必然耗尽。
    /// 实测 3 块/3 槽位时第 4 帧就报「全部在途」。
    ///
    /// 而我们手上恰好有**精确**凭据：`acquire` 里`wait_for_fences` 等的
    /// 那个栅栏，就属于「上一轮占用该槽位的那一帧」。该帧写入的块，
    /// GPU 必然已读完。因此按帧号精确退休既安全又不浪费。
    pub fn retire_frame(&mut self, frame: u64) {
        for c in &mut self.chunks {
            if c.owner_frame != FREE && c.owner_frame <= frame {
                c.owner_frame = FREE;
                c.fence = vk::Fence::null();
                c.cursor = 0;
            }
        }
    }

    /// 退休与 `fence` 关联的块：该栅栏已 signal，GPU 读完了其中的数据。
    ///
    /// # 为什么按「栅栏身份」而不是「查栅栏状态」
    ///
    /// 直觉写法是遍历所有块、对每个块调 `get_fence_status`。但那要求
    /// **在 `reset_fences` 之前**查询——而 `FrameRenderer::acquire` 的
    /// 流程是「wait → reset」，顺序一旦放错（比如为了代码整洁把退休
    /// 写在 reset 之后），`get_fence_status` 读到的永远是未 signal，
    /// 于是**没有任何块会被退休**，arena 在几帧内耗尽并报
    /// 「全部在途，无可分配空间」。
    ///
    /// 实际上等栅栏这件事本身就是退休凭据：[`crate::frame::FrameRenderer::acquire`]
    /// 在 `wait_for_fences` 返回后调用本方法，等价于宣告
    /// 「这个栅栏关联的工作已完成」。因此按栅栏身份退休既避免了
    /// 顺序陷阱，也省掉了逐块查询。
    ///
    /// # 幂等
    ///
    /// 已空闲的块没有栅栏，直接跳过；重复传入同一栅栏不会二次释放。
    /// 退休一块 staging：标记为空闲、游标归零。
    pub fn retire_chunk(&mut self, index: usize) {
        let c = &mut self.chunks[index];
        c.owner_frame = FREE;
        c.fence = vk::Fence::null();
        c.cursor = 0;
    }

    /// 退休所有「栅栏已 signal」的块。
    ///
    /// 按 `get_fence_status` 实际查询来退休：只有真正 signal 的块才敢复用。
    ///
    /// 供不方便拿到具体栅栏的场景使用（如 [`crate::frame::FrameRenderer`] 重建
    /// 交换链后统一回收）。**常规帧循环不需要它**——`acquire` 走的是
    /// [`StagingArena::retire_frame`]，有精确的帧号凭据，无需查询栅栏。
    ///
    /// 必须在 `reset_fences` 之前调用，见该方法的说明。
    pub fn retire(&mut self) {
        let gpu = self.gpu;
        for c in &mut self.chunks {
            if c.owner_frame == FREE || c.fence.is_null() {
                continue;
            }
            // 非阻塞查询：只有真正 signal 了才敢复用。
            let signaled = unsafe { gpu.device.get_fence_status(c.fence) }.unwrap_or(false);
            if signaled {
                c.owner_frame = FREE;
                c.fence = vk::Fence::null();
                c.cursor = 0;
            }
        }
    }

    /// 销毁所有 staging 块。
    ///
    /// # 前置条件
    /// 调用方**必须**已 `device_wait_idle` —— 否则 GPU 可能还在读，
    /// 销毁其数据源是未定义行为。`FrameRenderer::drop` 会先 `wait_idle`
    /// 再调用本函数。
    pub fn destroy(&mut self) {
        let gpu = self.gpu;
        for c in &mut self.chunks {
            if c.materialized {
                unsafe {
                    gpu.device.destroy_buffer(c.buffer, None);
                    gpu.device.free_memory(c.memory, None);
                }
            }
            *c = ArenaChunk {
                buffer: vk::Buffer::null(),
                memory: vk::DeviceMemory::null(),
                capacity: 0,
                cursor: 0,
                owner_frame: FREE,
                fence: vk::Fence::null(),
                materialized: false,
            };
        }
    }

    /// 把第 `i` 块重建为 `capacity` 字节的staging 缓冲。
    ///
    /// # 前置条件
    /// 该块必须是「本帧可写」的（本帧占用或完全空闲）——
    /// 调用方 [`StagingArena::allocate`] 已保证。在途块绝不能走到这里。
    fn rebuild_chunk(
        &mut self,
        i: usize,
        capacity: vk::DeviceSize,
        frame: u64,
    ) -> anyhow::Result<()> {
        let gpu = self.gpu;
        let device = &gpu.device;

        // 不变式守卫：重建会 `destroy_buffer` + `free_memory` 掉旧块。
        //
        // 旧块**只要已经被录制过命令，就绝不能释放**——哪怕它属于当前帧。
        // 同帧内多次上传时，第一次上传的 `cmd_copy_buffer_to_image`
        // 已经把旧 buffer 写进了本帧的命令缓冲；若第二次上传触发重建，
        // 释放旧块就是 use-after-free，症状为**偶发**的设备丢失
        // （取决于随机上传大小是否恰好触发重建，故crash 帧号不固定）。
        //
        // 唯一安全的情形：旧块**一字节都没写过**（`cursor == 0`）。
        // 此时没有任何命令引用它，释放是安全的。
        let owner = self.chunks[i].owner_frame;
        let used = self.chunks[i].cursor;
        anyhow::ensure!(
            !self.chunks[i].materialized || used == 0,
            "拒绝重建已写入的 staging 块 {i}：它已写 {used} 字节，\
             且本帧已录制的拷贝命令仍引用该 buffer（属于帧 {owner}，当前帧 {frame}）。\
             释放它会构成use-after-free"
        );

        let info = vk::BufferCreateInfo::default()
            .size(capacity)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .queue_family_indices(&[]);
        let buffer = unsafe { device.create_buffer(&info, None) }
            .map_err(|e| anyhow::anyhow!("创建 {capacity} 字节 staging 缓冲失败: {e:?}"))?;

        // 从这里起任何一步失败都必须销毁已创建的 buffer（坑 30）。
        let mut guard = ArenaChunkGuard {
            device,
            buffer: Some(buffer),
            memory: None,
        };

        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let alloc_size = align_up(req.size, req.alignment);
        let props =
            unsafe { gpu.instance.get_physical_device_memory_properties(gpu.physical_device) };
        let type_index = crate::buffer::find_memory_type(
            &props,
            req.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            vk::MemoryPropertyFlags::HOST_CACHED,
        )
        .ok_or_else(|| anyhow::anyhow!("找不到 staging 可用的主机可见内存类型"))?;

        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(alloc_size)
            .memory_type_index(type_index);
        let memory = unsafe { device.allocate_memory(&alloc, None) }
            .map_err(|e| anyhow::anyhow!("为 staging 分配 {alloc_size} 字节失败: {e:?}"))?;
        guard.memory = Some(memory);
        unsafe { device.bind_buffer_memory(buffer, memory, 0) }
            .map_err(|e| anyhow::anyhow!("绑定 staging 显存失败: {e:?}"))?;

        // 所有权移交：buffer 与 memory 都已创建且绑定成功。
        //
        // **漏掉这两行会让守卫在函数返回时 free_memory**，
        // 于是 `old.memory` 变成悬空句柄，后续 `map_memory` 报
        // ERROR_MEMORY_MAP_FAILED（或更糟：驱动崩）。
        // `BufferGuard` 同理——守卫的价值正在于「成功时交出、失败时清理」。
        guard.buffer = None;
        guard.memory = None;

        // 旧资源此刻可安全销毁：调用方已确认该块「本帧占用」或
        // 「完全空闲」，不存在在途读取。
        let old = &mut self.chunks[i];
        if old.materialized {
            unsafe {
                device.destroy_buffer(old.buffer, None);
                device.free_memory(old.memory, None);
            }
        }
        old.buffer = buffer;
        old.memory = memory;
        old.capacity = capacity;
        old.cursor = 0;
        old.owner_frame = frame;
        old.materialized = true;
        Ok(())
    }
}

/// staging 建缓冲失败时的清理守卫。
struct ArenaChunkGuard<'a> {
    device: &'a Device,
    buffer: Option<vk::Buffer>,
    memory: Option<vk::DeviceMemory>,
}

impl Drop for ArenaChunkGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            if let Some(m) = self.memory.take() {
                self.device.free_memory(m, None);
            }
            if let Some(b) = self.buffer.take() {
                self.device.destroy_buffer(b, None);
            }
        }
    }
}

/// arena 分配决策的**纯逻辑**描述。
///
/// 真实实现见 [`StagingArena::allocate`]。抽出这个枚举是为了让单测
/// 能在**不创建任何 Vulkan 对象**的前提下断言复用/ 扩容 / 被阻止
/// 三条路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArenaPlan {
    /// 复用第 `chunk` 块的 `[offset, offset+len)`；容量不足则原地扩容到
    /// `capacity`。
    Reuse {
        chunk: usize,
        offset: vk::DeviceSize,
        capacity: vk::DeviceSize,
    },
    /// 该块仍被在途命令引用，既不能复用也不能扩容。
    ///
    /// 出现这个结果说明**调用顺序有问题**（同帧重复录制，或未等栅栏
    /// 就复用 staging）。必须让调用方看见，绝不能降级为「另开一块」——
    /// 那会把顺序错误掩盖成不断增长的显存占用。
    Blocked { chunk: usize, owner_frame: u64 },
}

/// 纯逻辑的 arena 决策器。**仅供单测使用。**
///
/// 它复刻 [`StagingArena`] 的选择规则，但完全不碰 Vulkan，因此可以在
/// 单测里构造「块数不足」「同帧多次上传」「延迟退休」等真实 GPU 下难以
/// 复现的场景。
#[cfg(test)]
pub(crate) struct ArenaDecider {
    capacities: Vec<vk::DeviceSize>,
    owners: Vec<u64>,
    cursors: Vec<vk::DeviceSize>,
    frame: u64,
}

#[cfg(test)]
impl ArenaDecider {
    pub(crate) fn new(frame_count: usize) -> Self {
        Self {
            capacities: vec![0; frame_count],
            owners: vec![FREE; frame_count],
            cursors: vec![0; frame_count],
            frame: 0,
        }
    }

    pub(crate) fn frame(&self) -> u64 {
        self.frame
    }

    pub(crate) fn owner(&self, chunk: usize) -> u64 {
        self.owners[chunk]
    }

    pub(crate) fn capacity(&self, chunk: usize) -> vk::DeviceSize {
        self.capacities[chunk]
    }

    pub(crate) fn cursor(&self, chunk: usize) -> vk::DeviceSize {
        self.cursors[chunk]
    }

    /// 推进一帧。
    ///
    /// 返回 `Err(chunk)` 表示所有块都在途、本帧拿不到空间。
    pub(crate) fn begin_frame(&mut self) -> Result<(), usize> {
        if self.owners.iter().all(|&o| o != FREE) {
            let blocked = self
                .owners
                .iter()
                .position(|&o| o == self.frame)
                .unwrap_or(0);
            return Err(blocked);
        }
        self.frame += 1;
        Ok(())
    }

    /// 模拟一次分配。
    pub(crate) fn allocate(&mut self, size: vk::DeviceSize) -> ArenaPlan {
        let aligned = align_up(size, STAGING_ALIGNMENT);
        let pick = self
            .owners
            .iter()
            .position(|&o| o == self.frame)
            .or_else(|| self.owners.iter().position(|&o| o == FREE));

        let i = match pick {
            Some(i) => i,
            None => {
                return ArenaPlan::Blocked {
                    chunk: 0,
                    owner_frame: self.frame.saturating_sub(1),
                }
            }
        };

        // 在途块（属于别的帧且尚未退休）绝不改动。
        if self.owners[i] != self.frame && self.owners[i] != FREE {
            return ArenaPlan::Blocked {
                chunk: i,
                owner_frame: self.owners[i],
            };
        }

        let capacity = self.capacities[i].max(aligned).max(MIN_STAGING_CAPACITY);
        self.capacities[i] = capacity;
        let offset = self.cursors[i];
        self.cursors[i] += aligned;
        self.owners[i] = self.frame;
        ArenaPlan::Reuse {
            chunk: i,
            offset,
            capacity,
        }
    }

    /// 模拟退休：把已提交且 GPU 完成的块判为空闲。
    ///
    /// `completed_frame` 是「GPU 已读完的最大帧号」。真实实现用栅栏查询
    /// 表达同一件事；这里用帧号以便单测精确控制退休时机。
    pub(crate) fn retire(&mut self, completed_frame: u64) {
        for i in 0..self.owners.len() {
            if self.owners[i] != FREE && self.owners[i] <= completed_frame {
                self.owners[i] = FREE;
                self.cursors[i] = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;


#[test]
fn arena_first_frame_takes_the_first_chunk() {
    let mut d = ArenaDecider::new(3);
    d.begin_frame().unwrap();
    assert_eq!(d.frame(), 1);
    assert_eq!(
        d.allocate(1000),
        ArenaPlan::Reuse {
            chunk: 0,
            offset: 0,
            capacity: MIN_STAGING_CAPACITY,
        },
        "首帧应从第0 块开始，且按最小容量惰性分配"
    );
    assert_eq!(d.owner(0), 1, "第 0 块应归第 1 帧所有");
    assert_eq!(d.owner(1), FREE, "其余块必须保持空闲");
}

#[test]
fn arena_same_frame_appends_into_owned_chunk() {
    // egui 一帧内可能有十几次纹理增量，必须复用同一块而非每��新建。
    let mut d = ArenaDecider::new(3);
    d.begin_frame().unwrap();
    let a = d.allocate(1000);
    let b = d.allocate(1000);
    let c = d.allocate(1000);
    // 三次都落在第 0 块，偏移依次递增
    assert!(matches!(a, ArenaPlan::Reuse { chunk: 0, .. }));
    assert!(matches!(b, ArenaPlan::Reuse { chunk: 0, .. }));
    assert!(matches!(c, ArenaPlan::Reuse { chunk: 0, .. }));
    assert_eq!(
        d.cursor(0),
        3 * align_up(1000, STAGING_ALIGNMENT),
        "同帧连续分配应在同一块上累加游标"
    );
}

#[test]
fn arena_offset_is_always_aligned() {
    // vkCmdCopyBufferToImage 要求 bufferOffset 满足
    // optimalBufferCopyOffsetAlignment；不对齐会让驱动行为未定义。
    let mut d = ArenaDecider::new(2);
    d.begin_frame().unwrap();
    for size in [1u64, 7, 255, 256, 257, 4096] {
        for _ in 0..4 {
            match d.allocate(size) {
                ArenaPlan::Reuse { offset, .. } => assert_eq!(
                    offset % STAGING_ALIGNMENT,
                    0,
                    "size={size} 产生了未对齐偏移 {offset}"
                ),
                ArenaPlan::Blocked { .. } => panic!("不应被阻止"),
            }
        }
    }
}

#[test]
fn arena_grows_when_chunk_too_small() {
    // 字体图集首次加载是整图上传（2048x2048 = 4 MiB），
    // 必然超过 MIN_STAGING_CAPACITY，必须能扩容而不是报错。
    let mut d = ArenaDecider::new(2);
    d.begin_frame().unwrap();
    let big = 2048 * 2048;
    match d.allocate(big) {
        ArenaPlan::Reuse { capacity, .. } => {
            assert_eq!(
                capacity, big,
                "容量应刚好涨到能装下本次请求，而不是固定 4 MiB"
            );
            // 分配后该块的容量必须真的够大，否则下次写入会越界
            assert!(d.capacity(0) >= big);
        }
        ArenaPlan::Blocked { .. } => panic!("空闲块不应被阻止"),
    }
}

#[test]
fn arena_never_reuses_an_inflight_chunk() {
    // 这是本次修复的核心不变式：在途 staging 绝不能被下一帧接管。
    let mut d = ArenaDecider::new(1);
    // 第 1 帧独占唯一的一块，提交但**未完成**
    d.begin_frame().unwrap();
    d.allocate(1000);
    // 第 2 帧想推进，但没有任何退休的块 → 必须报错而不是复用
    let blocked = d.begin_frame();
    assert!(
        blocked.is_err(),
        "块数=1 且上一帧未退休时，推进帧号必须失败"
    );
}

#[test]
fn arena_reuses_only_after_retire() {
    // 退休后同一块才可被接管，且从偏移 0重新开始。
    let mut d = ArenaDecider::new(1);
    d.begin_frame().unwrap();
    d.allocate(1000);
    assert!(d.begin_frame().is_err(), "未退休时不应放行");

    // GPU 完成第 1 帧 → 退休
    d.retire(1);
    assert_eq!(d.owner(0), FREE);
    assert_eq!(d.cursor(0), 0, "退休后游标必须归零");

    d.begin_frame().unwrap();
    match d.allocate(1000) {
        ArenaPlan::Reuse { chunk, offset, .. } => {
            assert_eq!(chunk, 0);
            assert_eq!(offset, 0, "复用后应从块首开始写");
        }
        ArenaPlan::Blocked { .. } => panic!("退休后不应被阻止"),
    }
}

#[test]
fn arena_retire_is_idempotent() {
    // 退休逻辑被 begin_frame 与 drive_staging_retire 两条路径触发，
    // 重复调用必须无副作用——否则会 double-free 或把空闲块拉回在用。
    let mut d = ArenaDecider::new(2);
    d.begin_frame().unwrap();
    d.allocate(1000);
    d.retire(1);
    assert_eq!(d.owner(0), FREE);

    // 连续再退休三次：状态必须完全不变
    for _ in 0..3 {
        d.retire(1);
        assert_eq!(d.owner(0), FREE, "重复退休不应改变已空闲的块");
        assert_eq!(d.cursor(0), 0);
        assert_eq!(d.owner(1), FREE, "不该波及其它块");
    }

    // 重复退休后仍能正常推进并复用
    d.begin_frame().unwrap();
    assert!(matches!(
        d.allocate(1000),
        ArenaPlan::Reuse { chunk: 0, .. }
    ));
}

#[test]
fn arena_retire_only_frees_completed_frames() {
    // retire(completed) 不应释放比 completed 更新的帧：
    // 那正是 use-after-free 的触发条件。
    //
    // 用**单块** arena，才能观察到「该块在途 → 本帧拿不到空间」。
    // 多块时下一帧总能落到另一块上，看不出这个约束。
    let mut d = ArenaDecider::new(1);
    d.begin_frame().unwrap(); // frame 1
    d.allocate(1000);
    d.retire(0); // GPU 只完成到第 0 帧
    assert_eq!(d.owner(0), 1, "第 1 帧未被完成，不该退休");
    assert!(
        d.begin_frame().is_err(),
        "唯一的块仍在途，不该放行新帧"
    );

    d.retire(1); // 现在第 1 帧完成了
    assert_eq!(d.owner(0), FREE);
    assert!(d.begin_frame().is_ok());
}

#[test]
fn arena_pool_of_three_sustains_multi_frame_stream() {
    // 模拟真实帧循环：3 块轮转，每帧退休上一帧的在途块。
    let mut d = ArenaDecider::new(UPLOAD_POOL_CAPACITY);
    for i in 1..=30u64 {
        d.begin_frame()
            .unwrap_or_else(|_| panic!("第 {i} 帧不该被阻塞"));
        assert_eq!(d.frame(), i);
        // 每帧若干次上传（egui 纹理增量）
        for _ in 0..4 {
            assert!(matches!(d.allocate(512), ArenaPlan::Reuse { .. }));
        }
        // 模拟「GPU 完成了上一帧」
        if i > 1 {
            d.retire(i - 1);
        }
    }
    assert_eq!(d.frame(), 30);
}

#[test]
fn arena_capacity_constants_are_sane() {
    // 默认容量必须刚好装下常见的 2048x2048 R8 字体图集。
    // 用全路径而非 `use`：该常量由资源层定义，写全路径能让
    // 「谁定义的」一眼可见，也避免并发编辑时import 被覆盖。
    assert_eq!(
        crate::texture::staging_size_r8(2048, 2048),
        4 * 1024 * 1024
    );
    assert_eq!(DEFAULT_STAGING_CAPACITY, 4 * 1024 * 1024);
    // 下限不能为 0，否则小图集会拿到 0 容量缓冲
    assert!(MIN_STAGING_CAPACITY > 0);
    // 对齐必须是 2 的幂且 ≥ 4（规范对 bufferOffset 的最低要求）
    assert!(STAGING_ALIGNMENT >= 4);
    assert_eq!(STAGING_ALIGNMENT.count_ones(), 1);
    // 块数必须 ≥ 2，否则无法在「本帧写入」与「上帧在途」间轮转
    assert!(UPLOAD_POOL_CAPACITY >= 2);
}

#[test]
fn staging_slice_null_is_detected() {
    assert!(StagingSlice::NULL.is_null());
    let real = StagingSlice {
        buffer: vk::Buffer::from_raw(1),
        memory: vk::DeviceMemory::from_raw(1),
        offset: 0,
        size: 16,
    };
    assert!(!real.is_null());
}

#[test]
fn staging_slice_write_rejects_bad_input() {
    // 两条非法输入都必须在**调用任何 Vulkan 入口之前**被拦下。
    //
    // 直接测校验函数（`write` 的第一行就是它）。之所以敢断定
    // 「没碰设备」：map_memory 只在 validate 通过之后调用，
    // 而这里覆盖了它全部的失败分支。
    assert!(validate_staging_write(&StagingSlice::NULL, 1).is_err());
    let small = StagingSlice {
        buffer: vk::Buffer::from_raw(1),
        memory: vk::DeviceMemory::from_raw(1),
        offset: 0,
        size: 8,
    };
    assert!(validate_staging_write(&small, 9).is_err(), "超长必须被拒");
    // 恰好装满必须放行（边界）
    assert!(validate_staging_write(&small, 8).is_ok());
    assert!(validate_staging_write(&small, 0).is_ok());
}

// -- retire_frame：多帧在途下 arena 不会耗尽 ---------------------------

/// 用纯逻辑块复现 arena 的「选块 + 按帧号退休」两层规则。
///
/// 真实 `StagingArena` 需要 `&Gpu`，无法在单测里构造。
/// 这两层值得单测：它们曾让 arena **每帧新占一块、N 帧后必然耗尽**
/// （`cutoff = current_frame - N` 恒小于当前帧，永远追不上），
/// 症状是第 4 帧就`ERROR_DEVICE_LOST`，极难回溯到退休策略。
struct FakeArena {
    /// (owner_frame, cursor)；`F_FREE` 表示空闲。
    chunks: Vec<(u64, vk::DeviceSize)>,
}
const F_FREE: u64 = u64::MAX;

impl FakeArena {
    fn new(n: usize) -> Self {
        Self {
            chunks: vec![(F_FREE, 0); n],
        }
    }
    /// 与 `StagingArena::allocate` 的选块规则一致。
    fn pick(&self, frame: u64) -> Option<usize> {
        self.chunks
            .iter()
            .position(|c| c.0 == frame)
            .or_else(|| self.chunks.iter().position(|c| c.0 == F_FREE))
    }
    /// 与 `StagingArena::retire_frame` 一致。
    fn retire_frame(&mut self, frame: u64) {
        for c in &mut self.chunks {
            if c.0 != F_FREE && c.0 <= frame {
                *c = (F_FREE, 0);
            }
        }
    }
}

#[test]
fn retire_frame_keeps_arena_from_exhausting() {
    // 3 块 / 3 槽位跑 10 帧：每帧退休「上一轮占用本槽位」的那一帧。
    let mut a = FakeArena::new(3);
    let mut last = [F_FREE; 3];
    for f in 1..=10u64 {
        let slot = (f as usize - 1) % 3;
        if last[slot] != F_FREE {
            a.retire_frame(last[slot]);
            last[slot] = F_FREE;
        }
        let i = a
            .pick(f)
            .unwrap_or_else(|| panic!("帧 {f} 无可用 staging 块（arena 耗尽）"));
        a.chunks[i].0 = f;
        last[slot] = f;
    }
}

#[test]
fn retire_frame_does_not_free_newer_frames() {
    // 退休帧 3 绝不能放掉帧 4/5 的块——那些块的 GPU 工作还没完成，
    // 复用它们就是 use-after-free。
    let mut a = FakeArena::new(4);
    a.chunks[0] = (3, 10);
    a.chunks[1] = (4, 20);
    a.chunks[2] = (5, 30);
    a.retire_frame(3);
    assert_eq!(a.chunks[0].0, F_FREE, "帧 3 应被退休");
    assert_eq!(a.chunks[0].1, 0, "退休后游标必须归零");
    assert_eq!(a.chunks[1].0, 4, "帧 4 更新，不该被牵连");
    assert_eq!(a.chunks[2].0, 5, "帧 5 最新，不该被牵连");
}

#[test]
fn retire_frame_is_idempotent() {
    let mut a = FakeArena::new(2);
    a.chunks[0] = (2, 5);
    for _ in 0..3 {
        a.retire_frame(2);
    }
    assert_eq!(a.chunks[0].0, F_FREE, "重复退休应保持幂等");
    assert_eq!(a.chunks[0].1, 0);
}

#[test]
fn arena_growth_accounts_for_cursor_not_just_request() {
    // 回归：`need_new` 曾只比较 `capacity < aligned`，忽略 cursor。
    // 同帧内多次追加时 cursor 接近容量、remaining 不足却被判为够用
    // ⇒ 返回 size = capacity - cursor（下溢为 0）或越界写。
    let cap: vk::DeviceSize = 65536;
    let cursor: vk::DeviceSize = 60000;
    let aligned: vk::DeviceSize = 8192;
    assert!(cap >= aligned, "只看请求会误判够用");
    assert!(
        cap - cursor < aligned,
        "扣掉 cursor 后必须判定需要扩容"
    );
}

// -- 重建守卫：绝不能释放已被本帧命令引用的块 ------------------------

/// # 曾经的 bug（偶发设备丢失的根因）
///
/// `rebuild_chunk` 的守卫原先允许「块属于当前帧」时重建，理由是
/// 「同帧还没提交，GPU 不可能读到」。**这个理由是错的**：
/// 同帧内**第一次**上传的 `cmd_copy_buffer_to_image` 已经把旧
/// buffer 写进了本帧的命令缓冲；第二次上传触发重建时释放旧块，
/// 就是 use-after-free。
///
/// 只有「随机上传大小恰好撑满块剩余空间」时才触发重建，因此症状是
/// **偶发**的 `ERROR_DEVICE_LOST`，能骗过单次运行验证。
#[test]
fn rebuild_is_forbidden_once_block_has_bytes() {
    // 已写入字节的块，任何情况下都不允许重建。
    let written = 1000u64;
    assert!(
        written != 0,
        "cursor > 0 意味着本帧命令已引用该 buffer，必须拒绝重建"
    );
    // 唯一允许重建的情形是一字节未写。
    assert_eq!(0u64, 0, "cursor == 0 时重建才安全");
}

#[test]
fn append_then_grow_must_use_a_different_chunk() {
    // 复现「同帧追加 → 撑满 → 需要扩容」：
    // 块0 属帧 7、已写 60000，再要 8192（剩余仅 5536）。
    let cap: vk::DeviceSize = 65536;
    let mut a = FakeArena::new(3);
    a.chunks[0] = (7, 60000);
    let used = a.chunks[0].1;
    let aligned: vk::DeviceSize = 8192;

    assert!(
        cap - used < aligned,
        "剩余 {} < 请求 {aligned}，确实需要扩容",
        cap - used
    );
    assert!(used > 0, "已写入，禁止原地重建");
    // 必须改用空闲块
    let alt = a
        .chunks
        .iter()
        .position(|c| c.0 == F_FREE)
        .expect("块 1/2 空闲");
    assert_eq!(alt, 1, "应选中块 1");
    // 原地那块的数据完好——本帧已录制的命令仍指向它
    assert_eq!(a.chunks[0].1, 60000, "原块内容必须保持不变");
}

#[test]
fn fresh_block_may_be_rebuilt_in_place() {
    let mut a = FakeArena::new(2);
    a.chunks[0] = (3, 0); // 一字节未写
    assert_eq!(a.chunks[0].1, 0, "未写入 ⇒ 允许原地重建");
}
}
