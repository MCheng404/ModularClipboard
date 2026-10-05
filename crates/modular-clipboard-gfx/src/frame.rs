//! 帧图与呈现：acquire → record → submit → present，以及交换链重建。
//!
//! 本模块是渲染器的帧循环骨架，也是**唯一**持有交换链派生资源的地方
//! （framebuffer、命令缓冲、信号量、栅栏、描述符集）。上层只管提供
//! 「画什么」，不管「画到哪里」——目标图像由 [`FrameRenderer::acquire`] 决定。
//!
//! # 一帧的流程
//!
//! ```
//! use ash::vk;
//! use modular_clipboard_gfx::frame::{DrawBatch, DrawInput, FrameRenderer, PresentResult};
//!
//! # fn run(fr: &mut FrameRenderer<'_>) -> anyhow::Result<()> {
//! let Some(frame) = fr.acquire()? else {
//!     // acquire 返回 None 表示交换链已过期，必须重建后才能继续
//!     fr.rebuild_swapchain(Default::default())?;
//!     return Ok(());
//! };
//! let input = DrawInput {
//!     vertex_buffer: vk::Buffer::null(),
//!     index_buffer: vk::Buffer::null(),
//!     batches: &[DrawBatch { index_offset: 0, index_count: 3 }],
//!     ..Default::default()
//! };
//! fr.record(&input)?;
//! if fr.present(frame)? == PresentResult::Outdated {
//!     // 呈现时才发现过期：同样要重建，下一帧的 acquire 才会成功
//!     fr.rebuild_swapchain(Default::default())?;
//! }
//! # Ok(()) }
//! ```
//!
//! # 布局转换为什么必须逐图像跟踪
//!
//! 交换链的图像在两次 [`acquire_next_image`] 之间**不会**被重置布局——
//! 上一帧结束时它们停在 `PRESENT_SRC_KHR`，下次 acquire 拿到的可能还是
//! 同一张。因此不能用 `UNDEFINED` 一刀切，必须为每张图像单独记当前布局。
//! 见 [`LayoutTracker`]。
//!
//! # staging 内存为什么必须跨帧持有
//!
//! `cmd_copy_buffer_to_image` 只是把命令**写进命令缓冲**，GPU 何时真正
//! 读staging 内存由驱动决定。若在上传函数里`let staging = Buffer::new(..)`
//! 然后函数返回，staging 的生命周期就只覆盖「录制」而不覆盖「执行」——
//! 这是 use-after-free。
//!
//! 注意本项目的 `Buffer` **不实现 `Drop`**（销毁需要 `&Device`），
//! 因此那个版本的实际症状是**显存泄漏**而非立即崩溃：泄漏同样不可接受，
//! 且修法一致——把 staging 提升为帧层持有的 [`StagingArena`]，
//! 由提交时的栅栏来保证「GPU 读完之后才允许复用」。
//! 见 [`StagingArena`] 与 [`FrameRenderer::record_texture_upload`]。

use ash::vk::Handle;
use ash::{Device, khr, vk};
use std::time::Instant;

use crate::buffer::align_up;
use crate::pipeline::{BINDING_SAMPLER, BINDING_TEXTURE, BINDING_UNIFORM};
use crate::{Gpu, Swapchain, image_barrier};

/// `VK_SUBOPTIMAL_KHR` 的原始返回码。
///
/// ash 0.38 **没有**为它生成常量（`vk::Result` 里只有 Vulkan 1.0 核心的
/// 那批），只能自己按扩展规定的数值比较。
pub const RESULT_SUBOPTIMAL_KHR: i32 = 1_000_001_003;

/// `VK_ERROR_OUT_OF_DATE_KHR` 的原始返回码。同样没有现成常量。
pub const RESULT_OUT_OF_DATE_KHR: i32 = -1_000_001_004;

/// 无法从表面能力推断尺寸时使用的兜底窗口尺寸。
///
/// Windows 的 `current_extent` 总是有效（等于真实客户区尺寸），因此这个
/// 兜底值实际上只在离屏/异常表面上才会被用到。
const FALLBACK_EXTENT: vk::Extent2D = vk::Extent2D {
    width: 1280,
    height: 800,
};

/// acquire 的等待上限。取 u64::MAX 表示「一直等到有图像可用」。
const ACQUIRE_TIMEOUT: u64 = u64::MAX;

/// 栅栏等待上限。取 u64::MAX 表示「等 GPU 真的画完」。
const FENCE_TIMEOUT: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// 契约类型
// ---------------------------------------------------------------------------

/// 渲染所需的全部管线对象，打包交给 [`FrameRenderer`] 持有引用。
///
/// 只存裸句柄、不接管所有权：这些对象由调用方创建与销毁，
/// `FrameRenderer` 销毁时不会去动它们。这样重建交换链时
/// 管线可以继续复用（前提是表面格式没变，见 [`FrameRenderer::rebuild_swapchain`]）。
#[derive(Debug, Clone, Copy)]
pub struct PipelineBundle {
    /// 渲染通道。附件格式必须与交换链图像格式一致。
    pub render_pass: vk::RenderPass,
    /// 管线布局，内含描述符集布局。
    pub pipeline_layout: vk::PipelineLayout,
    /// 图形管线。
    pub pipeline: vk::Pipeline,
    /// 描述符集布局。`FrameRenderer` 用它分配 [`FrameRenderer`] 自己的描述符集。
    pub descriptor_set_layout: vk::DescriptorSetLayout,
}

impl PipelineBundle {
    /// 从三件套 + 描述符集布局组装。
    pub fn new(
        render_pass: vk::RenderPass,
        pipeline_layout: vk::PipelineLayout,
        pipeline: vk::Pipeline,
        descriptor_set_layout: vk::DescriptorSetLayout,
    ) -> Self {
        Self {
            render_pass,
            pipeline_layout,
            pipeline,
            descriptor_set_layout,
        }
    }
}

/// 一次成功的 acquire。
#[derive(Debug, Clone)]
pub struct AcquiredFrame {
    /// 交换链图像索引。
    pub image_index: u32,
    /// 与 `image_index` 对应的 framebuffer。**不能传空句柄**，否则驱动崩溃。
    pub framebuffer: vk::Framebuffer,
    /// 与 `image_index` 对应的图像。
    pub image: vk::Image,
    /// acquire 完成的时刻，用于上层统计帧耗时。
    pub acquired_at: Instant,
}

/// 呈现结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentResult {
    /// 画面已提交。交换链仍与表面匹配。
    Presented,
    /// 交换链已过期或不匹配，**调用方必须重建交换链**后才能继续 acquire。
    Outdated,
}

/// acquire 的原始返回在本模块的归一化结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// 拿到了可渲染的图像。`suboptimal` 为真时仍可渲染，
    /// 但表面已不匹配，建议尽快重建。
    Ready {
        image_index: u32,
        suboptimal: bool,
    },
    /// 交换链已过期，必须重建。本帧不产生任何 GPU 工作。
    Rebuild,
}

/// 绘制批次：索引缓冲上的一段连续区间，对应一次 `draw_indexed`。
///
/// egui 的裁剪矩形在 CPU 侧 tessellation 时就已反映为「被裁掉的顶点
/// 不进入网格」，因此这里不需要逐批次的 scissor。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawBatch {
    /// 索引缓冲中的起始索引。
    pub index_offset: u32,
    /// 索引个数。为 0 的批次会被跳过（`draw_indexed` 传 0 是未定义行为）。
    pub index_count: u32,
}

/// 一帧的绘制输入。
///
/// 顶点/索引缓冲由 GPU 资源层（`buffer.rs`）拥有并上传，
/// 本模块只负责绑定句柄并发出绘制命令，不关心数据从哪来。
#[derive(Debug, Clone, Copy)]
pub struct DrawInput<'a> {
    /// 顶点缓冲句柄。
    pub vertex_buffer: vk::Buffer,
    /// 索引缓冲句柄。
    pub index_buffer: vk::Buffer,
    /// 顶点缓冲绑定偏移。
    pub vertex_offset: vk::DeviceSize,
    /// 索引缓冲绑定偏移。
    pub index_offset: vk::DeviceSize,
    /// 索引类型。默认 `UINT32`，与 `pipeline.rs` 的属性布局配套。
    pub index_type: vk::IndexType,
    /// 绘制批次。为空则只清屏不绘制。
    pub batches: &'a [DrawBatch],
    /// 覆盖默认描述符集。为 `null` 时使用该帧槽自带的描述符集。
    pub descriptor_set: vk::DescriptorSet,
}

// 不能 derive(Default)：`vk::IndexType` 的 Default 是 UINT16，
// 而本项目的索引缓冲统一按 u32 上传。默认值错了不会崩，只会静默画出乱码，
// 因此这里手写 Default 把索引类型钉死为 UINT32。
impl Default for DrawInput<'_> {
    fn default() -> Self {
        Self {
            vertex_buffer: vk::Buffer::null(),
            index_buffer: vk::Buffer::null(),
            vertex_offset: 0,
            index_offset: 0,
            index_type: vk::IndexType::UINT32,
            batches: &[],
            descriptor_set: vk::DescriptorSet::null(),
        }
    }
}

impl<'a> DrawInput<'a> {
    /// 只画一个三角形批次，其余字段取默认值。
    pub fn single_batch(vertex_buffer: vk::Buffer, index_buffer: vk::Buffer) -> Self {
        Self {
            vertex_buffer,
            index_buffer,
            batches: &[DrawBatch {
                index_offset: 0,
                index_count: 3,
            }],
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// 可单测的纯逻辑
// ---------------------------------------------------------------------------

/// 计算真正要用的交换链尺寸。
///
/// 规则来自 Vulkan 规范的 `VkSurfaceCapabilitiesKHR::currentExtent`：
/// - **非 0 时必须原样采用**，不能自行 clamp——这是规范强制要求，
///   自行计算出的尺寸在某些表面上会导致 `vkCreateSwapchainKHR` 失败；
/// - 为 0（0 表示「由应用自行决定」）时才用 `desired` 并 clamp 到
///   `[min_image_extent, max_image_extent]`。
pub fn resolve_extent(
    caps: &vk::SurfaceCapabilitiesKHR,
    desired: vk::Extent2D,
) -> vk::Extent2D {
    if caps.current_extent.width != 0 && caps.current_extent.height != 0 {
        return caps.current_extent;
    }
    let axis = |want: u32, min: u32, max: u32| {
        // 上限小于下限是驱动报告异常数据。`clamp` 在这种输入下会 panic，
        // 因此退化为「取下限」，保证函数总是不崩。
        if max < min { min } else { want.clamp(min, max) }
    };
    vk::Extent2D {
        width: axis(desired.width, caps.min_image_extent.width, caps.max_image_extent.width),
        height: axis(desired.height, caps.min_image_extent.height, caps.max_image_extent.height),
    }
}

/// 上传录制的前置条件判定。
///
/// # 为什么需要它
///
/// `record_texture_upload` 会往**帧槽位的命令缓冲**追加命令。若该槽位
/// 上一轮提交后GPU 仍在执行，追加命令会破坏在途录制——Vulkan 明确
/// 禁止修改处于可执行状态的命令缓冲。这类错误不会立刻崩，而是表现为
/// 「画面偶尔花一帧」，极难定位，因此必须在 API 边界挡掉。
///
/// # 纯逻辑
///
/// 只依赖两个布尔量，因此可脱离 GPU 单测——真实路径上GPU 是否执行完
/// 由栅栏保证，本函数只负责把「状态 → 允许/拒绝」这条规则固定下来。
pub fn can_record_upload(pending: bool, in_flight: bool) -> anyhow::Result<()> {
    if !pending {
        anyhow::bail!("record_texture_upload 必须在 acquire 之后调用");
    }
    if in_flight {
        anyhow::bail!(
            "帧槽位的命令缓冲已提交、GPU 仍在执行，不能重复录制。\
             请先present/acquire 推进到下一帧（每帧每个槽位只录一次）"
        );
    }
    Ok(())
}

/// 把 `queue_present` 的双重返回（整体码 + 每交换链码）归一化。///
/// 整体码与 `pResults` 可能给出不同结论：整体 `SUCCESS` 但某个交换链
/// 单独 `OUT_OF_DATE` 的情况真实存在（多交换链时），只看整体码会漏掉重建信号。
pub fn map_present_result(
    overall: Result<bool, vk::Result>,
    per_swapchain: Option<vk::Result>,
) -> anyhow::Result<PresentResult> {
    let outdated = |r: vk::Result| r == vk::Result::from_raw(RESULT_OUT_OF_DATE_KHR);
    if let Some(inner) = per_swapchain
        && outdated(inner)
    {
        return Ok(PresentResult::Outdated);
    }
    match overall {
        // 第二个值是「suboptimal」：能显示，但表面已不匹配。
        Ok(false) => Ok(PresentResult::Presented),
        Ok(true) => Ok(PresentResult::Outdated),
        Err(e) if outdated(e) => Ok(PresentResult::Outdated),
        Err(e) => Err(anyhow::anyhow!("queue_present 失败: {e:?}")),
    }
}

/// 把 `acquire_next_image` 的返回归一化。
pub fn map_acquire_result(
    raw: Result<(u32, bool), vk::Result>,
) -> anyhow::Result<AcquireOutcome> {
    match raw {
        // ash 0.38 把 SUBOPTIMAL 折叠成 Ok((idx, true))，
        // OUTDATED 才是 Err——网上 0.37 的示例写法在这里是错的。
        Ok((image_index, suboptimal)) => Ok(AcquireOutcome::Ready {
            image_index,
            suboptimal,
        }),
        Err(e) if e.as_raw() == RESULT_OUT_OF_DATE_KHR => Ok(AcquireOutcome::Rebuild),
        // 超时意味着这一帧没抢到图像，同样按「下一帧重建」处理，
        // 否则会陷入「返回 None 但永不重建」的死循环。
        Err(vk::Result::TIMEOUT) => Ok(AcquireOutcome::Rebuild),
        Err(e) => Err(anyhow::anyhow!("acquire_next_image 失败: {e:?}")),
    }
}

/// 逐图像跟踪当前布局。
///
/// 交换链图像的布局在帧与帧之间**不会**自动回到 `UNDEFINED`。
/// 每张图像各自停在上一帧结束时的布局（通常是 `PRESENT_SRC_KHR`），
/// 下次 acquire 到同一张时必须从那个布局转出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutTracker {
    layouts: Vec<vk::ImageLayout>,
}

impl LayoutTracker {
    /// 为 `image_count` 张图像建立跟踪，全部初始为 `UNDEFINED`。
    pub fn new(image_count: u32) -> Self {
        Self {
            layouts: vec![vk::ImageLayout::UNDEFINED; image_count as usize],
        }
    }

    /// 交换链重建后调用：图像是全新的，全部回到 `UNDEFINED`。
    pub fn reset(&mut self, image_count: u32) {
        self.layouts.clear();
        self.layouts
            .resize(image_count as usize, vk::ImageLayout::UNDEFINED);
    }

    /// 图像总数。
    pub fn len(&self) -> usize {
        self.layouts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.layouts.is_empty()
    }

    /// 读某张图像的当前布局。索引越界返回 `None`。
    pub fn get(&self, index: u32) -> Option<vk::ImageLayout> {
        self.layouts.get(index as usize).copied()
    }

    /// 写某张图像的当前布局。索引越界返回 `false`（不 panic）。
    pub fn set(&mut self, index: u32, layout: vk::ImageLayout) -> bool {
        match self.layouts.get_mut(index as usize) {
            Some(slot) => {
                *slot = layout;
                true
            }
            None => false,
        }
    }

    /// 状态机核心：把某张图像从当前布局迁移到 `to`。
    ///
    /// 返回 `(旧布局, 新布局)`；索引越界返回 `None`。
    /// 旧布局等于新布局时不做任何事，但仍返回 `(l, l)` 便于调用方断言。
    pub fn transition(&mut self, index: u32, to: vk::ImageLayout) -> Option<(vk::ImageLayout, vk::ImageLayout)> {
        let from = *self.layouts.get(index as usize)?;
        self.layouts[index as usize] = to;
        Some((from, to))
    }
}

// ---------------------------------------------------------------------------
// StagingArena
// ---------------------------------------------------------------------------

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
    /// 应在 [`FrameRenderer::acquire`] 等完该槽位栅栏**之后**调用——
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
    /// [`FrameRenderer`] 传本帧槽位的栅栏。
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
    /// 实际上等栅栏这件事本身就是退休凭据：[`FrameRenderer::acquire`]
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
    /// 供不方便拿到具体栅栏的场景使用（如 [`FrameRenderer`] 重建
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

// ---------------------------------------------------------------------------
// FrameRenderer
// ---------------------------------------------------------------------------

/// 一帧在飞所独占的同步对象。
///
/// 按**在飞帧槽位**索引，不是按交换链图像索引：图像索引由驱动决定，
/// 我们无法提前知道，因此「等栅栏」只能按槽位轮转。
struct FrameSlot {
    command_buffer: vk::CommandBuffer,
    /// **只**交给 `queue_submit`。
    ///
    /// # 为什么必须与 `acquire_fence` 分开
    ///
    /// 曾让同一个 fence 同时传给 `acquire_next_image` 与 `queue_submit`。
    /// 规范允许这么做，但该fence 会有**两个** signal 来源：
    /// acquire 成功时驱动 signal 一次，submit 完成时再 signal 一次。
    /// 配合「等完就`reset_fences`」的写法，手工维护的 `fence_signaled`
    /// 标志必然与驱动实际状态错位——某帧的 wait 会提前返回，
    /// 于是reset 了GPU 仍在读的命令缓冲，表现为随机丢设备。
    ///
    /// 拆开后每个 fence 只有一个 signal 源，配合
    /// [`FrameRenderer::fence_signaled`] 直接查驱动状态，
    /// 就不需要任何手工标志了。
    submit_fence: vk::Fence,
    /// **只**交给 `acquire_next_image`。语义与 [`Self::submit_fence`] 对称。
    ///
    /// 实测中它从未被等待过——acquire 的等待由 `acquire_semaphore` 与
    /// 槽位轮转天然保证。保留它是为了满足「信号量与 fence 至少给一个」
    /// 的显式契约，并让两处 signal 来源在结构上就分开。
    acquire_fence: vk::Fence,
    /// acquire signal → submit wait。
    acquire_semaphore: vk::Semaphore,
    /// submit signal → present wait。
    present_semaphore: vk::Semaphore,
    descriptor_set: vk::DescriptorSet,
    /// 该槽位的命令缓冲**已被提交、GPU 可能仍在执行**。
    ///
    /// 这个标记存在的唯一原因是防止「排队帧重复录制」：
    /// `record_texture_upload` 若在 GPU 还没跑完上一轮提交时再次往
    /// 同一命令缓冲追加命令，就会破坏在途录制——Vulkan 明确禁止
    /// 在命令缓冲处于可执行状态时修改它。
    ///
    /// 生命周期：`present` 提交后置真，`acquire` 里等完栅栏后置假。
    /// 换言之，它等价于「本槽位是否有未完成的 GPU 工作」。
    in_flight: bool,
    /// 该槽位**上一轮**使用的 arena 帧号。
    ///
    /// `acquire` 等完本槽位栅栏时，这个帧号就是「GPU 肯定已读完」的
    /// 精确凭据，用来退休该帧占用的 staging 块（见
    /// [`StagingArena::retire_frame`]）。`u64::MAX` 表示从未用过。
    last_frame: u64,
}

/// 帧循环核心。
pub struct FrameRenderer<'a> {
    gpu: &'a Gpu,
    bundle: PipelineBundle,
    /// 自行创建的表面 loader，用于**重建时重新查询**能力。
    ///
    /// `Gpu` 里的 loader 是私有字段，且其 `surface_caps` 是初始化时的快照——
    /// 窗口 resize 后 `current_extent` 会变，必须重查。
    surface_loader: khr::surface::Instance,
    swapchain_loader: khr::swapchain::Device,
    swapchain: Swapchain,
    /// 交换链格式。渲染通道按它创建；重建时若格式变了必须报错。
    pipeline_format: vk::Format,
    command_pool: vk::CommandPool,
    descriptor_pool: vk::DescriptorPool,
    slots: Vec<FrameSlot>,
    /// 在飞帧槽位的轮转游标。
    cursor: usize,
    layouts: LayoutTracker,
    /// 当前待完成的帧。未 acquire 时为 `None`。
    pending: Option<Pending>,
    clear_color: [f32; 4],
    /// 跨帧存活的 staging 环形缓冲。
    ///
    /// 生命周期必须覆盖 **GPU 执行时间**而不只是录制时间，因此不能是
    /// 上传函数里的局部变量。块数与在飞槽位数一致，提交时由栅栏记账，
    /// 确保「GPU 读完之后才允许复用」。
    staging: StagingArena<'a>,
}

/// 正在录制中的帧。
struct Pending {
    frame: AcquiredFrame,
    /// 使用的帧槽位下标。
    slot: usize,
    suboptimal: bool,
}

impl<'a> FrameRenderer<'a> {
    /// 创建帧渲染器并建立初始交换链。
    ///
    /// 尺寸取自表面能力的 `current_extent`（Windows 上恒为有效值），
    /// 不可用时退化为 [`FALLBACK_EXTENT`]。窗口尺寸确定后调用方应主动
    /// [`FrameRenderer::rebuild_swapchain`] 一次。
    ///
    /// `pipeline_bundle` 的渲染通道格式必须与表面选中的格式一致，
    /// 否则 [`Swapchain::new`] 建出的 framebuffer 与渲染通道不兼容。
    pub fn new(gpu: &'a Gpu, pipeline_bundle: PipelineBundle) -> anyhow::Result<Self> {
        // 自行建一套 Entry/SurfaceLoader：Gpu 的 surface_caps 是启动时快照，
        // 重建交换链必须重新查询。Entry::load 只是再取一次已加载的
        // vulkan-1.dll 的函数地址，开销可忽略。
        let entry = unsafe { ash::Entry::load()? };
        let surface_loader = khr::surface::Instance::new(&entry, &gpu.instance);
        let swapchain_loader = khr::swapchain::Device::new(&gpu.instance, &gpu.device);

        let caps = gpu.surface_caps;
        let extent = resolve_extent(&caps, FALLBACK_EXTENT);
        let swapchain = Swapchain::new(gpu, extent.width, extent.height, pipeline_bundle.render_pass)?;

        // 渲染通道是按某个格式建的。若交换链最终选中的格式不同，
        // framebuffer 与渲染通道不兼容——直接失败，别让驱动在运行时炸。
        let pipeline_format = swapchain_format_of(gpu)?;
        if pipeline_format != swapchain.format {
            anyhow::bail!(
                "渲染通道格式 {pipeline_format:?} 与交换链格式 {:?} 不一致",
                swapchain.format
            );
        }

        let command_pool = create_command_pool(&gpu.device, gpu.queue_family)?;

        let mut this = Self {
            gpu,
            bundle: pipeline_bundle,
            surface_loader,
            swapchain_loader,
            swapchain,
            pipeline_format,
            command_pool,
            descriptor_pool: vk::DescriptorPool::null(),
            slots: Vec::new(),
            cursor: 0,
            layouts: LayoutTracker::new(0),
            pending: None,
            clear_color: [0.10, 0.10, 0.12, 1.0],
            // 块数随后按交换链图像数确定，见下方。
            staging: StagingArena::new(gpu, 1)?,
        };
        this.allocate_frame_resources()?;

        // 布局跟踪表必须按交换链图像数建立。
        // 漏了这一步（曾写成 `LayoutTracker::new(0)`）会让**首帧**
        // acquire 直接失败：「图像索引 0 超出布局跟踪表」——
        // 而单测与 pipeline_probe 都碰不到 FrameRenderer::acquire，
        // 因此这个缺陷能一路活到接UI 才暴露。
        this.layouts.reset(this.swapchain.image_count);

        // 交换链建好才知道在飞帧数，按它重建 arena 的块数。
        // 必须 ≥ slot_count：每个在飞帧各占一块，否则下一帧会覆写
        // GPU 尚未读完的 staging 内存。
        this.staging = StagingArena::new(gpu, this.slots.len())?;

        tracing::debug!(
            extent = ?this.swapchain.extent,
            images = this.swapchain.image_count,
            slots = this.slots.len(),
            staging_chunks = this.staging.chunk_count(),
            "FrameRenderer 就绪"
        );
        Ok(this)
    }

    /// 当前交换链尺寸。
    pub fn extent(&self) -> vk::Extent2D {
        self.swapchain.extent
    }

    /// 交换链格式。
    pub fn format(&self) -> vk::Format {
        self.swapchain.format
    }

    /// 在飞帧槽位数量（等于交换链图像数）。
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// 当前帧正在使用的槽位下标。
    ///
    /// 资源层用它索引**按槽位持有**的暂存缓冲（staging ring）。
    /// 帧层是多帧在途的，同一份 CPU 数据不能在下一帧覆写 GPU 尚未读完的
    /// staging 内存。
    ///
    /// # 为什么这已经足够安全
    ///
    /// [`FrameRenderer::acquire`] 在返回前会**等待并重置当前槽位的栅栏**，
    /// 因此「拿到 `current_slot()` 就往该槽位写」这件事本身就是安全的：
    /// 上一轮占用该槽位的 GPU 工作此时已经完成。资源层**不需要**额外等待，
    /// 也不需要自己维护一套与帧层不同步的轮转。
    ///
    /// 未 acquire 时返回 `None`。
    pub fn current_slot(&self) -> Option<usize> {
        self.pending.as_ref().map(|p| p.slot)
    }

    /// 当前帧的命令缓冲，供资源层把上传等命令**录进同一份提交**。
    ///
    /// 纹理上传必须与绘制共用一次 `queue_submit`，否则需要额外的同步等待。
    /// 未 acquire 时返回 `None`——此时录制命令会被后续 `present` 遗弃。
    ///
    /// # 生命周期
    ///
    /// 返回的句柄由帧层拥有，**不要**自行 `end_command_buffer` 或销毁；
    /// [`FrameRenderer::present`] 负责收尾。录完即可，句柄随后失效。
    pub fn current_command_buffer(&self) -> Option<vk::CommandBuffer> {
        self.pending
            .as_ref()
            .and_then(|p| self.slots.get(p.slot))
            .map(|s| s.command_buffer)
    }

    /// acquire 是否返回了 suboptimal（画面能出，但表面已不匹配）。
    pub fn last_acquire_was_suboptimal(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| p.suboptimal)
    }

    /// 设置清屏颜色（线性空间 RGBA）。
    pub fn set_clear_color(&mut self, rgba: [f32; 4]) {
        self.clear_color = rgba;
    }

    /// 查询栅栏是否已 signal——**直接问驱动**，而不是靠手工标志。
    ///
    /// 这是消除「标志与驱动状态脱节」的关键：手工标志只在某条路径上被
    /// 置位/清除，而 fence 可能被多处 signal（`acquire_next_image` 与
    /// `queue_submit` 各一次），标志迟早对不上。对不上时 `wait_for_fences`
    /// 会提前返回，于是 reset 了GPU 仍在读的命令缓冲 → 设备丢失。
    ///
    /// 查不到状态（`ERROR`）时按「未 signal」处理：宁可多等一次，
    /// 也不能因为误判「已完成」而破坏在途工作。
    fn fence_signaled(&self, fence: vk::Fence) -> bool {
        if fence.is_null() {
            return false;
        }
        unsafe { self.gpu.device.get_fence_status(fence).unwrap_or(false) }
    }

    /// 取某个帧槽位的描述符集，供资源层填充 uniform / 纹理。
    pub fn descriptor_set(&self, slot: usize) -> anyhow::Result<vk::DescriptorSet> {
        self.slots
            .get(slot)
            .map(|s| s.descriptor_set)
            .ok_or_else(|| anyhow::anyhow!("帧槽位 {slot} 越界（共 {} 个）", self.slots.len()))
    }

    /// 更新某帧槽位描述符集里的 uniform 绑定。
    ///
    /// **每个在飞槽位都要写一遍**：GPU 可能还在读上一个槽位的描述符集，
    /// 只写当前帧会导致其它帧读到未初始化的绑定。
    pub fn update_uniform_binding(
        &self,
        slot: usize,
        buffer: vk::Buffer,
        offset: vk::DeviceSize,
        range: vk::DeviceSize,
    ) -> anyhow::Result<()> {
        let set = self.descriptor_set(slot)?;
        let info = vk::DescriptorBufferInfo {
            buffer,
            offset,
            range,
        };
        let write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(BINDING_UNIFORM)
            .descriptor_count(1)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .buffer_info(std::slice::from_ref(&info));
        unsafe {
            // ash 0.38 的方法名是 update_descriptor_sets（规范名 vkUpdateDescriptorSets），
            // 不是网上常见的 write_descriptor_sets。
            self.gpu.device.update_descriptor_sets(
                std::slice::from_ref(&write),
                &[],
            );
        }
        Ok(())
    }

    /// 更新某帧槽位描述符集里的采样器 + 字体纹理绑定。
    pub fn update_texture_binding(
        &self,
        slot: usize,
        sampler: vk::Sampler,
        image_view: vk::ImageView,
        layout: vk::ImageLayout,
    ) -> anyhow::Result<()> {
        let set = self.descriptor_set(slot)?;
        let info = vk::DescriptorImageInfo {
            sampler,
            image_view,
            image_layout: layout,
        };
        // 两个绑定共用一次 update 调用：sampler 是裸 sampler，
        // texture 是 combined image sampler，各写一条。
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_SAMPLER)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(std::slice::from_ref(&info)),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_TEXTURE)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&info)),
        ];
        unsafe { self.gpu.device.update_descriptor_sets(&writes, &[]) };
        Ok(())
    }

    /// 交换链重建后，把「绑定到交换链图像」的资源重新绑一遍。
    ///
    /// # 为什么必须显式调用
    ///
    /// 顶点/索引缓冲若被绑定到某个交换链图像视图（Opaque 资源用法），
    /// 重建后那张图像视图已被销毁——继续用会读到已释放的资源，
    /// 表现为崩溃或画面变花（字体变豆腐块）。
    ///
    /// Vulkan 没有「自动重绑」这种机制：描述符是**静态绑定**，
    /// 图像视图销毁后不会自动指向新视图。所以这里选择**显式失败**
    /// 而不是静默失效——宁可让上层立刻知道，也别让画面坏得莫名其妙。
    ///
    /// 调用时机：`rebuild_swapchain` 成功之后、下一帧 `acquire` 之前。
    ///
    /// # 参数
    ///
    /// - `sampler`：字体图集的采样器。
    /// - `font_view` / `font_layout`：字体图集的图像视图与布局。
    ///
    /// 注意：顶点/索引缓冲若曾通过描述符绑定到交换链图像，
    /// 调用方需自行用 [`Self::update_texture_binding`] 之外的途径重绑——
    /// 本项目当前的绘制路径**不**把顶点缓冲绑到交换链图像
    /// （顶点缓冲是独立 device-local 资源，经 `vk::CmdBufferBindVertexBuffers`
    /// 绑定，不走描述符），因此交换链重建不影响它们。
    ///
    /// # 示例
    ///
    /// ```ignore
    /// fr.rebuild_swapchain(new_extent)?;
    /// fr.rebind_after_rebuild(sampler, font_view, font_layout)?;
    /// ```
    pub fn rebind_after_rebuild(
        &self,
        sampler: vk::Sampler,
        font_view: vk::ImageView,
        font_layout: vk::ImageLayout,
    ) -> anyhow::Result<()> {
        // 每个帧槽位有独立的描述符集，必须逐个重绑。
        for slot in 0..self.slots.len() {
            self.update_texture_binding(slot, sampler, font_view, font_layout)?;
        }
        tracing::debug!(slots = self.slots.len(), "交换链重建后已完成描述符重绑");
        Ok(())
    }

    /// 取下一张可绘制的交换链图像。
    ///
    /// **重要**：`FrameRenderer::new` 分配描述符集后**不会**填充它们——
    /// 因为此时还没有字体纹理。调用方必须在第一次 `acquire` 之前调用
    /// [`Self::update_uniform_binding`] 与 [`Self::update_texture_binding`]。
    ///
    /// 跳过这一步的后果不是「画错」，而是**设备丢失**：未初始化的描述符集
    /// 内容是未定义值，着色器一旦解引用就是 UB，驱动会返回
    /// `ERROR_DEVICE_LOST`。这是实测踩过的坑（`upload_probe` 崩在
    /// `acquire`，报 "descriptor set not updated"）。
    ///
    /// 交换链重建后同理要重绑，见 [`Self::rebind_after_rebuild`]。
    ///
    /// 返回 `Ok(None)` 表示交换链已过期（或本次没抢到图像），
    /// 调用方**必须**先 [`FrameRenderer::rebuild_swapchain`] 再继续。
    /// 成功时本帧的命令录制已经开始（布局转换 + 渲染通道开启 + 管线绑定）。
    pub fn acquire(&mut self) -> anyhow::Result<Option<AcquiredFrame>> {
        if self.pending.is_some() {
            anyhow::bail!("上一帧尚未 present，不能开始新帧");
        }
        let slot = self.cursor % self.slots.len();

        // 复用槽位前，若该槽位**有未完成的提交**，必须等GPU 跑完。
        //
        // 判据是「有没有待完成的提交」（`in_flight`），而**不是**
        // 「栅栏是否已 signal」——后者恰好是错的：多帧在途下进入这里时
        // GPU 通常还在忙、栅栏**未 signal**，据此跳过等待就会往
        // GPU 仍在读的命令缓冲追加命令（实测报「不能重复录制」）。
        //
        // `wait_for_fences` 对未 signal 的栅栏会**阻塞**到signal，
        // 这正是所需语义；等完再向驱动复核一次。
        if self.slots[slot].in_flight {
            let fence = self.slots[slot].submit_fence;
            unsafe { self.gpu.device.wait_for_fences(&[fence], true, FENCE_TIMEOUT)? };

            // 等完必须复核：驱动若报错/状态异常，继续下去会破坏在途工作。
            anyhow::ensure!(
                self.fence_signaled(fence),
                "槽位 {slot} 的 submit 栅栏等待后仍未 signal，                 同步状态不可信，拒绝继续以免破坏在途工作"
            );

            // 栅栏已 signal ⇒ 上一轮占用该槽位的那一帧，GPU 必已读完它写入的
            // staging。按**该帧的帧号**精确退休，而不是「落后 N 帧」——
            // 后者的 cutoff 恒小于当前帧，永远追不上，导致 arena 耗尽。
            let done_frame = self.slots[slot].last_frame;
            if done_frame != u64::MAX {
                self.staging.retire_frame(done_frame);
            }
            self.slots[slot].last_frame = u64::MAX;
            self.slots[slot].in_flight = false;

            // 注意：**先退休、后 reset**。`get_fence_status` 在 reset 之后
            // 读到的永远是 false，若把退休放到 reset 之后，
            // staging 块将永远查不到 signal，arena 会在几帧内耗尽。
            unsafe { self.gpu.device.reset_fences(&[fence])? };
        }
        // acquire_fence 同样复位到未signal，好让下次 acquire 拿到干净状态。
        // （它由本次 acquire signal，槽位再次被复用前必须复位。）
        {
            let af = self.slots[slot].acquire_fence;
            if self.fence_signaled(af) {
                unsafe { self.gpu.device.reset_fences(&[af])? };
            }
        }
        self.staging.begin_frame();

        let (image_index, suboptimal) = match map_acquire_result(unsafe {
            self.swapchain_loader.acquire_next_image(
                self.swapchain.handle,
                ACQUIRE_TIMEOUT,
                self.slots[slot].acquire_semaphore,
                self.slots[slot].acquire_fence,
            )
        })? {
            AcquireOutcome::Ready {
                image_index,
                suboptimal,
            } => (image_index, suboptimal),
            AcquireOutcome::Rebuild => return Ok(None),
        };

        let image_index = image_index as usize;
        let Some(&image) = self.swapchain.images.get(image_index) else {
            anyhow::bail!("驱动返回的图像索引 {image_index} 超出交换链图像数");
        };
        let Some(&framebuffer) = self.swapchain.framebuffers.get(image_index) else {
            anyhow::bail!("交换链图像 {image_index} 缺少对应 framebuffer");
        };
        // image_index 必然落在 layouts 的长度内（长度等于图像数），
        // 但驱动返回越界索引时上面的 get 已经拦住了，这里用 get 保持一致。
        if self.layouts.get(image_index as u32).is_none() {
            anyhow::bail!("图像索引 {image_index} 超出布局跟踪表");
        }

        let frame = AcquiredFrame {
            image_index: image_index as u32,
            framebuffer,
            image,
            acquired_at: Instant::now(),
        };

        self.cursor += 1;
        self.open_command_buffer(slot, &frame)?;
        self.pending = Some(Pending {
            frame: frame.clone(),
            slot,
            suboptimal,
        });
        Ok(Some(frame))
    }

    /// 录制绘制命令。必须紧跟在成功的 [`FrameRenderer::acquire`] 之后。
    ///
    /// # 前置条件
    ///
    /// `batches` 非空时，**必须**已经对该帧槽位调用过
    /// [`FrameRenderer::update_uniform_binding`] 与
    /// [`FrameRenderer::update_texture_binding`]。
    /// `allocate_descriptor_sets` 分配出的集合内容是未初始化的，
    /// 绑定后交给 GPU 会解引用垃圾值（UB → 丢设备）。
    /// 本方法不代为校验这一点——它无法知道资源层是否已写入。
    ///
    /// `batches` 为空时只清屏、不绑定任何描述符集，因此是安全的。
    pub fn record(&mut self, input: &DrawInput<'_>) -> anyhow::Result<()> {
        let Some(pending) = self.pending.as_ref() else {
            anyhow::bail!("record 必须在 acquire 之后调用");
        };
        let slot = pending.slot;
        // 先取出再borrow self：下面 begin_render_pass 需要 &mut self。
        let framebuffer = pending.frame.framebuffer;
        let device = &self.gpu.device;
        let cmd = self.slots[slot].command_buffer;

        // 视口与剪裁跟随当前交换链尺寸；交换链重建后自动更新，
        // 不需要重建管线（管线里声明的是 dynamic state）。
        let extent = self.swapchain.extent;
        let viewport = vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let scissor = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        unsafe {
            device.cmd_set_viewport(cmd, 0, std::slice::from_ref(&viewport));
            device.cmd_set_scissor(cmd, 0, std::slice::from_ref(&scissor));
        }

        // 没有批次就只清屏。
        //
        // ⚠️ 渲染通道**此刻才开启**（而非在 acquire 时）：纹理上传必须录在
        // 渲染通道作用域**之外**，Vulkan 不允许在通道内录传输命令。
        //
        // ⚠️ 描述符集绑定必须放在 `drawable.is_empty()` 判断**之后**。
        // `allocate_descriptor_sets` 分配出的集合内容是**未初始化**的，
        // 着色器又声明了 binding 0/1/2，绑上去GPU 就会解引用垃圾值
        // （UB → 丢设备）。「不绑定」严格优于「绑定一个假的」。
        let drawable: Vec<DrawBatch> = input
            .batches
            .iter()
            .copied()
            .filter(|b| b.index_count > 0)
            .collect();

        // 通道总是要开——即使无批次也要走一遍 clear + present，
        // 否则交换链图像内容未定义。
        self.begin_render_pass(framebuffer);
        if drawable.is_empty() {
            return Ok(());
        }

        let cmd = self.slots[slot].command_buffer;
        unsafe {
            device.cmd_bind_pipeline(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.bundle.pipeline,
            );
        }

        let set = if input.descriptor_set.is_null() {
            self.slots[slot].descriptor_set
        } else {
            input.descriptor_set
        };
        let sets = [set];
        unsafe {
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.bundle.pipeline_layout,
                0,
                &sets,
                &[],
            );
        }
        let buffers = [input.vertex_buffer];
        let offsets = [input.vertex_offset];
        let index_buffer = input.index_buffer;
        let index_offset = input.index_offset;
        let index_type = input.index_type;
        unsafe {
            device.cmd_bind_vertex_buffers(cmd, 0, &buffers, &offsets);
            device.cmd_bind_index_buffer(cmd, index_buffer, index_offset, index_type);
        }
        for batch in drawable {
            unsafe {
                device.cmd_draw_indexed(cmd, batch.index_count, 1, batch.index_offset, 0, 0);
            }
        }
        Ok(())
    }

    /// 结束渲染通道、提交并呈现。
    ///
    /// `frame` 必须是本帧 [`FrameRenderer::acquire`] 返回的那一个
    /// （用于校验调用方没有把帧搞混）。
    pub fn present(&mut self, frame: AcquiredFrame) -> anyhow::Result<PresentResult> {
        let Some(pending) = self.pending.take() else {
            anyhow::bail!("present 必须在 acquire 之后调用");
        };
        if pending.frame.image_index != frame.image_index {
            anyhow::bail!(
                "present 的图像索引 {} 与 acquire 的 {} 不一致",
                frame.image_index,
                pending.frame.image_index
            );
        }

        let slot = pending.slot;
        let image_index = frame.image_index as usize;
        let device = &self.gpu.device;
        let cmd = self.slots[slot].command_buffer;
        // 渲染通道内的绘制已经结束，先关通道再做布局转换——
        // 顺序反了会让转换屏障落在一个正在被写入的附件上。
        unsafe { device.cmd_end_render_pass(cmd) };

        let image = self.swapchain.images[image_index];
        let (old_layout, new_layout) = self
            .layouts
            .transition(image_index as u32, vk::ImageLayout::PRESENT_SRC_KHR)
            .ok_or_else(|| anyhow::anyhow!("图像索引 {image_index} 超出布局跟踪表"))?;
        let mut barrier = image_barrier(
            image,
            old_layout,
            new_layout,
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            vk::AccessFlags::empty(),
        );
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_mut(&mut barrier),
            );
            device.end_command_buffer(cmd)?;
        }

        // 提交。注意 builder 的调用顺序：ash 0.38 的 `wait_dst_stage_mask`
        // 会顺手把 `wait_semaphore_count` 也设成切片长度，两者必须都以
        // 1 为长度才不出错——先设 stage mask，再设 semaphores 最保险。
        let wait_stage = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let acquire_sem = self.slots[slot].acquire_semaphore;
        let present_sem = self.slots[slot].present_semaphore;
        let submit = vk::SubmitInfo::default()
            .wait_dst_stage_mask(&wait_stage)
            .wait_semaphores(std::slice::from_ref(&acquire_sem))
            .command_buffers(std::slice::from_ref(&cmd))
            .signal_semaphores(std::slice::from_ref(&present_sem));
        unsafe {
            device.queue_submit(
                self.gpu.queue,
                std::slice::from_ref(&submit),
                // 只给 submit_fence：它**只有这一个** signal 来源。
                self.slots[slot].submit_fence,
            )?;
        }
        // 命令缓冲此刻已交给 GPU，**不可再录制**。见 `FrameSlot::in_flight`。
        self.slots[slot].in_flight = true;
        // 把本帧占用的 staging 块与该栅栏绑定：栅栏 signal 之后
        // 才会允许复用它们。
        let fence = self.slots[slot].submit_fence;
        self.staging.note_submitted(fence);
        self.staging.end_frame();
        // 记下本槽位这一轮用的帧号：下次 acquire 等完栅栏后，
        // 就可据此精确退休这一帧占用的 staging 块。
        self.slots[slot].last_frame = self.staging.current_frame();

        let swapchains = [self.swapchain.handle];
        let image_indices = [frame.image_index];
        let mut results = [vk::Result::SUCCESS];
        let present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(std::slice::from_ref(&present_sem))
            .results(&mut results)
            .image_indices(&image_indices)
            .swapchains(&swapchains);
        let overall = unsafe {
            self.swapchain_loader
                .queue_present(self.gpu.queue, &present_info)
        };
        map_present_result(overall, Some(results[0]))
    }

    /// 录制一次纹理上传。
    ///
    /// 这是**纹理上传的唯一正确入口**：它从 [`StagingArena`] 取一块
    /// 生命周期覆盖 GPU 执行时间的 staging 空间，因此不存在
    /// 「函数返回后 GPU 还在读已释放内存」的问题。
    ///
    /// # 参数
    /// - `image`：目标纹理。会被推进到 `SHADER_READ_ONLY_OPTIMAL` 布局。
    /// - `bytes`：覆盖率数据，长度须等于 `patch` 的像素数。
    /// - `offset`：更新起始像素。`None` 表示整图更新。
    /// - `patch`：本次覆盖的矩形尺寸 `(宽, 高)`。
    ///
    /// # 约束
    ///
    /// 必须在 [`FrameRenderer::acquire`] 成功之后、
    /// [`FrameRenderer::present`] 之前调用。若该槽位的命令缓冲仍在GPU
    /// 执行中，返回错误而非静默重复录制（见 [`FrameSlot::in_flight`]）。
    pub fn record_texture_upload(
        &mut self,
        image: &mut crate::texture::DeviceImage,
        bytes: &[u8],
        offset: Option<(u32, u32)>,
        patch: (u32, u32),
    ) -> anyhow::Result<()> {
        let Some(pending) = self.pending.as_ref() else {
            anyhow::bail!("record_texture_upload 必须在 acquire 之后调用");
        };
        let slot = pending.slot;
        // 规则本体在 can_record_upload（可单测），此处只喂真实状态。
        can_record_upload(true, self.slots[slot].in_flight)?;
        let cmd = self.slots[slot].command_buffer;

        // 从 arena 取空间：偏移已按STAGING_ALIGNMENT 对齐，
        // 且这块内存的生命周期由 arena 与本帧栅栏共同保证。
        let staging = self.staging.allocate(bytes.len() as vk::DeviceSize)?;
        staging.write(&self.gpu.device, bytes)?;

        image.upload(self.gpu, cmd, staging, bytes, offset, patch)
    }

    /// 推进 staging 退休记账（可选显式调用）。
    ///
    /// [`FrameRenderer::acquire`] 内部已经做了这件事，因此正常帧循环
    /// **不需要**调用本方法。保留它是为了让「等完栅栏 → 退休 → 推进帧号」
    /// 这套记账在需要精细控制时也能被驱动（例如上传发生在 acquire 之前
    /// 的自定义流程）。重复调用是安全的。
    pub fn drive_staging_retire(&mut self) {
        self.staging.retire();
        self.staging.begin_frame();
    }

    /// 重建交换链。
    ///
    /// `desired_extent` 只在表面能力的 `current_extent` 为 0 时才起作用；
    /// 非 0 时**必须**采用表面报告的尺寸（见 [`resolve_extent`]）。
    /// 传 [`vk::Extent2D::default()`] 表示「由表面决定」。
    ///
    /// 重建后视口与剪裁自动跟随新尺寸（它们是动态状态，无需重建管线）。
    /// 若表面格式发生变化则直接失败——此时渲染通道与图形管线都必须重建，
    /// 已超出本模块的职责。
    pub fn rebuild_swapchain(&mut self, desired_extent: vk::Extent2D) -> anyhow::Result<()> {
        // 必须先等 GPU 空闲：旧交换链的 framebuffer 还在被在飞命令引用。
        self.gpu.wait_idle();

        let caps = unsafe {
            self.surface_loader.get_physical_device_surface_capabilities(
                self.gpu.physical_device,
                self.gpu.surface(),
            )?
        };
        let desired = if desired_extent.width == 0 || desired_extent.height == 0 {
            FALLBACK_EXTENT
        } else {
            desired_extent
        };
        let extent = resolve_extent(&caps, desired);

        // 重建前先确认格式没变——渲染通道与图形管线都是按旧格式建的。
        let (format, _) = crate::pick_format(
            unsafe {
                self.surface_loader.get_physical_device_surface_formats(
                    self.gpu.physical_device,
                    self.gpu.surface(),
                )?
            }
            .as_slice(),
        )?;
        if format != self.pipeline_format {
            anyhow::bail!(
                "重建时表面格式从 {:?} 变为 {format:?}，需重建渲染通道与图形管线",
                self.pipeline_format
            );
        }

        // 销毁顺序由 Swapchain::destroy 保证：framebuffer → image_view → swapchain。
        // 反序会留下悬空引用。
        self.swapchain.destroy(&self.gpu.device);
        self.swapchain =
            Swapchain::new(self.gpu, extent.width, extent.height, self.bundle.render_pass)?;
        if self.swapchain.format != self.pipeline_format {
            anyhow::bail!("重建后交换链格式与渲染通道不一致");
        }

        // 图像是全新的，布局全部回到 UNDEFINED。
        self.layouts.reset(self.swapchain.image_count);
        self.allocate_frame_resources()?;

        // 槽位数可能变化，arena 的块数必须跟着变（块数须≥ 在飞帧数）。
        // 本函数开头已 wait_idle，因此销毁旧 staging 是安全的。
        self.staging.destroy();
        self.staging = StagingArena::new(self.gpu, self.slots.len())?;

        tracing::debug!(
            extent = ?self.swapchain.extent,
            images = self.swapchain.image_count,
            slots = self.slots.len(),
            staging_chunks = self.staging.chunk_count(),
            "交换链已重建"
        );
        Ok(())
    }

    // -- 内部 ------------------------------------------------------------

    /// 开启本帧的命令录制：清空命令缓冲、布局转换、开渲染通道、绑管线。
    fn open_command_buffer(&mut self, slot: usize, frame: &AcquiredFrame) -> anyhow::Result<()> {
        let device = &self.gpu.device;
        let cmd = self.slots[slot].command_buffer;
        unsafe {
            device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
        }

        // 渲染前：当前布局 → COLOR_ATTACHMENT_OPTIMAL。
        let (old_layout, new_layout) = self
            .layouts
            .transition(frame.image_index, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .ok_or_else(|| anyhow::anyhow!("图像索引超出布局跟踪表"))?;
        // 从 UNDEFINED 转出时没有可等待的写入，直接清空访问掩码。
        let src_access = if old_layout == vk::ImageLayout::UNDEFINED {
            vk::AccessFlags::empty()
        } else {
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE
        };
        let mut barrier = image_barrier(
            frame.image,
            old_layout,
            new_layout,
            src_access,
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        );
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_mut(&mut barrier),
            );
        }

        // 渲染通道**不在这里**开启。
        //
        // Vulkan 规定：渲染通道作用域内只能录制绘制/丢弃类命令，
        // `vkCmdCopyBufferToImage` 等传输命令在渲染通道内是**非法**的，
        // 驱动会报验证错误甚至直接丢设备（实测 ERROR_DEVICE_LOST）。
        // 而纹理上传必须在 acquire 之后、record 之前发生——若此处开了
        // 通道，上传命令就一定落在作用域内。
        //
        // 因此改为延迟到 [`FrameRenderer::record`] 再开启，
        // 让 acquire 与 record 之间的上传录制处于通道之外。
        let _ = frame;
        Ok(())
    }

    /// 开启本帧的渲染通道（由 [`FrameRenderer::record`] 调用）。
    fn begin_render_pass(&mut self, framebuffer: vk::Framebuffer) {
        let device = &self.gpu.device;
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let cmd = self.slots[pending.slot].command_buffer;
        let clear = vk::ClearValue {
            color: vk::ClearColorValue {
                float32: self.clear_color,
            },
        };
        let clear_values = [clear];
        let area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: self.swapchain.extent,
        };
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.bundle.render_pass)
            // 绝不能传空 framebuffer：未定义行为，驱动直接崩。
            .framebuffer(framebuffer)
            .render_area(area)
            .clear_values(&clear_values);
        unsafe {
            device.cmd_begin_render_pass(cmd, &begin, vk::SubpassContents::INLINE);
        }
    }

    /// 按当前交换链的图像数重建命令缓冲 / 栅栏 / 信号量 / 描述符集。
    ///
    /// 数量可能变化（`min_image_count + 1` 与驱动上限共同决定），
    /// 因此整体销毁后重建，而不是尝试增量调整。
    fn allocate_frame_resources(&mut self) -> anyhow::Result<()> {
        self.destroy_frame_resources();

        let count = self.swapchain.image_count.max(1);
        let device = &self.gpu.device;

        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: count,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLER,
                descriptor_count: count,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: count,
            },
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(count)
            .pool_sizes(&pool_sizes);
        self.descriptor_pool =
            unsafe { device.create_descriptor_pool(&pool_info, None) }
                .map_err(|e| anyhow::anyhow!("创建描述符池失败: {e:?}"))?;

        // 描述符集要按**在飞帧数**各分配一份（GPU 可能还在读上一帧的
        // 那一份，见 `update_uniform_binding` 的说明）。
        //
        // ash 0.38 的坑 15：`set_layouts()` 的**切片长度就是分配数量**。
        // 只写 `&[layout]`（长度 1）会只分配出 1 个描述符集，
        // 而下面断言期望 `count` 个 —— 于是 `FrameRenderer::new` 直接失败。
        // 因此这里必须把布局重复 `count` 次。
        let layouts = vec![self.bundle.descriptor_set_layout; count as usize];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&layouts);
        // 数量取自 allocate_info 的切片长度（坑 15），不设则返回空 Vec。
        let sets = unsafe { device.allocate_descriptor_sets(&alloc_info)? };
        anyhow::ensure!(
            sets.len() == count as usize,
            "描述符集分配数量不符：期望 {count}，实际 {}",
            sets.len()
        );

        let cmd_alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(count);
        let buffers = unsafe { device.allocate_command_buffers(&cmd_alloc)? };
        anyhow::ensure!(
            buffers.len() == count as usize,
            "命令缓冲分配数量不符：期望 {count}，实际 {}",
            buffers.len()
        );

        let mut slots = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            // 两个 fence 各自**只有一个** signal 来源：
            // submit_fence 由 queue_submit signal，acquire_fence 由
            // acquire_next_image signal。绝不让两者共用一个——
            // 共用会产生两个 signal 源，使「是否已完成」的判断不可靠。
            //
            // 初始建成 SIGNALED：让首次复用该槽位时 wait 立刻通过
            // （此时确实没有在途工作，符合实际状态）。
            let fence_info = vk::FenceCreateInfo::default()
                .flags(vk::FenceCreateFlags::SIGNALED);
            let submit_fence = unsafe { device.create_fence(&fence_info, None) }
                .map_err(|e| anyhow::anyhow!("创建 submit 栅栏失败: {e:?}"))?;
            let acquire_fence = unsafe { device.create_fence(&fence_info, None) }
                .map_err(|e| anyhow::anyhow!("创建 acquire 栅栏失败: {e:?}"))?;
            let semaphore_info = vk::SemaphoreCreateInfo::default();
            let acquire_semaphore = unsafe { device.create_semaphore(&semaphore_info, None) }
                .map_err(|e| anyhow::anyhow!("创建 acquire 信号量失败: {e:?}"))?;
            let present_semaphore = unsafe { device.create_semaphore(&semaphore_info, None) }
                .map_err(|e| anyhow::anyhow!("创建 present 信号量失败: {e:?}"))?;
            slots.push(FrameSlot {
                command_buffer: buffers[i],
                submit_fence,
                acquire_fence,
                // 初始没有在途提交，可以自由录制。
                in_flight: false,
                // 从未使用过，没有可退休的上一轮帧。
                last_frame: u64::MAX,
                acquire_semaphore,
                present_semaphore,
                descriptor_set: sets[i],
            });
        }
        self.slots = slots;
        self.cursor = 0;
        Ok(())
    }

    fn destroy_frame_resources(&mut self) {
        let device = &self.gpu.device;
        for slot in &self.slots {
            unsafe {
                device.destroy_fence(slot.submit_fence, None);
                device.destroy_fence(slot.acquire_fence, None);
                device.destroy_semaphore(slot.acquire_semaphore, None);
                device.destroy_semaphore(slot.present_semaphore, None);
            }
        }
        // 命令缓冲随命令池一起回收，不单独销毁。
        self.slots.clear();
        if !self.descriptor_pool.is_null() {
            unsafe { device.destroy_descriptor_pool(self.descriptor_pool, None) };
            self.descriptor_pool = vk::DescriptorPool::null();
        }
    }
}

impl Drop for FrameRenderer<'_> {
    fn drop(&mut self) {
        // 在飞命令可能还引用着交换链图像与 framebuffer，必须等 GPU 空闲。
        self.gpu.wait_idle();
        // wait_idle 之后销毁 staging 才是安全的：GPU 可能还在读它，
        // 而它是 `cmd_copy_buffer_to_image` 的数据源。
        self.staging.destroy();
        self.destroy_frame_resources();
        unsafe { self.gpu.device.destroy_command_pool(self.command_pool, None) };
        // framebuffer → image_view → swapchain 的销毁顺序由 Swapchain 保证。
        self.swapchain.destroy(&self.gpu.device);
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn create_command_pool(device: &Device, queue_family: u32) -> anyhow::Result<vk::CommandPool> {
    let info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(queue_family)
        // 每帧都要 reset 命令缓冲，这个标志位是必需的。
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    unsafe { device.create_command_pool(&info, None) }
        .map_err(|e| anyhow::anyhow!("创建命令池失败: {e:?}"))
}

/// 预知交换链将选中的格式。
///
/// 复用 `crate::pick_format`——**不另写副本**。渲染通道的附件格式必须与
/// 交换链完全一致，两份实现一旦分叉就是静默失配，表现为驱动在
/// `cmd_begin_render_pass` 时崩溃，极难定位。
fn swapchain_format_of(gpu: &Gpu) -> anyhow::Result<vk::Format> {
    Ok(crate::pick_format(&gpu.surface_formats)?.0)
}

// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(
        cur_w: u32,
        cur_h: u32,
        min: vk::Extent2D,
        max: vk::Extent2D,
    ) -> vk::SurfaceCapabilitiesKHR {
        vk::SurfaceCapabilitiesKHR {
            current_extent: vk::Extent2D {
                width: cur_w,
                height: cur_h,
            },
            min_image_extent: min,
            max_image_extent: max,
            ..Default::default()
        }
    }

    #[test]
    fn current_extent_wins_over_our_own_calculation() {
        // 规范强制：current_extent 非 0 时必须原样采用，
        // 哪怕它落在 [min, max] 之外、哪怕我们想要别的尺寸。
        let c = caps(
            1281,
            721,
            vk::Extent2D { width: 1, height: 1 },
            vk::Extent2D {
                width: 4096,
                height: 4096,
            },
        );
        let got = resolve_extent(&c, vk::Extent2D { width: 800, height: 600 });
        assert_eq!(got.width, 1281);
        assert_eq!(got.height, 721);
    }

    #[test]
    fn zero_current_extent_falls_back_to_clamped_desired() {
        let c = caps(
            0,
            0,
            vk::Extent2D {
                width: 320,
                height: 240,
            },
            vk::Extent2D {
                width: 1920,
                height: 1080,
            },
        );
        let got = resolve_extent(&c, vk::Extent2D { width: 800, height: 600 });
        assert_eq!((got.width, got.height), (800, 600));

        // 超出上限要被压回上限
        let big = resolve_extent(&c, vk::Extent2D { width: 4000, height: 4000 });
        assert_eq!((big.width, big.height), (1920, 1080));

        // 低于下限要被抬到下限
        let small = resolve_extent(&c, vk::Extent2D { width: 1, height: 1 });
        assert_eq!((small.width, small.height), (320, 240));
    }

    #[test]
    fn extent_survives_inverted_caps() {
        // 驱动报告 max < min 是异常数据，但 clamp 会因此 panic。
        // 函数必须给出确定结果而不是崩掉。
        let c = caps(
            0,
            0,
            vk::Extent2D { width: 800, height: 600 },
            vk::Extent2D { width: 100, height: 100 },
        );
        let got = resolve_extent(&c, vk::Extent2D { width: 640, height: 480 });
        assert_eq!((got.width, got.height), (800, 600));
    }

    #[test]
    fn extent_only_treats_partial_zero_as_free() {
        // 只给宽、不给高不构成「由应用决定」，仍应视为无效并回退。
        let c = caps(
            640,
            0,
            vk::Extent2D { width: 1, height: 1 },
            vk::Extent2D {
                width: 4096,
                height: 4096,
            },
        );
        let got = resolve_extent(&c, vk::Extent2D { width: 800, height: 600 });
        assert_eq!((got.width, got.height), (800, 600));
    }

    #[test]
    fn layout_tracker_tracks_each_image_independently() {
        let mut t = LayoutTracker::new(3);
        assert_eq!(t.len(), 3);
        for i in 0..3 {
            assert_eq!(t.get(i), Some(vk::ImageLayout::UNDEFINED));
        }

        // 只推进第 1 张
        assert_eq!(
            t.transition(1, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL),
            Some((vk::ImageLayout::UNDEFINED, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL))
        );
        // 关键断言：其余两张不能跟着变。
        // 用 UNDEFINED 一刀切会让第 0、2 张的 PRESENT_SRC_KHR 状态丢失。
        assert_eq!(t.get(0), Some(vk::ImageLayout::UNDEFINED));
        assert_eq!(t.get(2), Some(vk::ImageLayout::UNDEFINED));

        // 推进到 PRESENT 后再取一次，仍只影响第 1 张
        assert_eq!(
            t.transition(1, vk::ImageLayout::PRESENT_SRC_KHR),
            Some((
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::PRESENT_SRC_KHR
            ))
        );
        assert_eq!(t.get(1), Some(vk::ImageLayout::PRESENT_SRC_KHR));
        assert_eq!(t.get(0), Some(vk::ImageLayout::UNDEFINED));
    }

    #[test]
    fn layout_tracker_round_trips_a_second_frame() {
        // 模拟真实的两帧：同一批图像连续被 acquire 两次。
        // 第二帧必须从上一帧留下的 PRESENT_SRC_KHR 出发，
        // 这正是「不能假设都是 UNDEFINED」的根据。
        let mut t = LayoutTracker::new(2);
        let mut observed: Vec<vk::ImageLayout> = Vec::new();
        for _ in 0..2 {
            for idx in 0..2u32 {
                let (from, _) = t
                    .transition(idx, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                    .unwrap();
                observed.push(from);
                t.transition(idx, vk::ImageLayout::PRESENT_SRC_KHR).unwrap();
            }
        }
        // 第一轮全是 UNDEFINED（首帧），第二轮全是 PRESENT_SRC_KHR（复用图像）
        assert_eq!(
            observed[..2],
            [vk::ImageLayout::UNDEFINED, vk::ImageLayout::UNDEFINED]
        );
        assert_eq!(
            observed[2..],
            [
                vk::ImageLayout::PRESENT_SRC_KHR,
                vk::ImageLayout::PRESENT_SRC_KHR
            ]
        );
    }

    #[test]
    fn layout_tracker_rejects_out_of_range() {
        let mut t = LayoutTracker::new(2);
        assert_eq!(t.get(2), None);
        assert_eq!(t.transition(9, vk::ImageLayout::PRESENT_SRC_KHR), None);
        assert!(!t.set(9, vk::ImageLayout::PRESENT_SRC_KHR));
        // 越界不能污染状态
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn layout_tracker_reset_clears_all() {
        let mut t = LayoutTracker::new(2);
        t.transition(0, vk::ImageLayout::PRESENT_SRC_KHR).unwrap();
        // 重建后图像数可能变化
        t.reset(4);
        assert_eq!(t.len(), 4);
        for i in 0..4 {
            assert_eq!(t.get(i), Some(vk::ImageLayout::UNDEFINED));
        }
        // 缩容也要生效
        t.reset(1);
        assert_eq!(t.len(), 1);
        assert_eq!(t.get(1), None);
    }

    #[test]
    fn layout_transition_to_same_layout_is_allowed() {
        let mut t = LayoutTracker::new(1);
        assert_eq!(
            t.transition(0, vk::ImageLayout::PRESENT_SRC_KHR),
            Some((vk::ImageLayout::UNDEFINED, vk::ImageLayout::PRESENT_SRC_KHR))
        );
        // 幂等：PRESENT → PRESENT
        assert_eq!(
            t.transition(0, vk::ImageLayout::PRESENT_SRC_KHR),
            Some((
                vk::ImageLayout::PRESENT_SRC_KHR,
                vk::ImageLayout::PRESENT_SRC_KHR
            ))
        );
    }

    #[test]
    fn acquire_success_and_suboptimal_are_both_runnable() {
        assert_eq!(
            map_acquire_result(Ok((2, false))).unwrap(),
            AcquireOutcome::Ready {
                image_index: 2,
                suboptimal: false
            }
        );
        // suboptimal 仍可渲染，只是要提示重建
        assert_eq!(
            map_acquire_result(Ok((0, true))).unwrap(),
            AcquireOutcome::Ready {
                image_index: 0,
                suboptimal: true
            }
        );
    }

    #[test]
    fn acquire_outdated_and_timeout_request_rebuild() {
        assert_eq!(
            map_acquire_result(Err(vk::Result::from_raw(RESULT_OUT_OF_DATE_KHR))).unwrap(),
            AcquireOutcome::Rebuild
        );
        // 超时不能当成错误，否则窗口最小化时会刷屏报错
        assert_eq!(
            map_acquire_result(Err(vk::Result::TIMEOUT)).unwrap(),
            AcquireOutcome::Rebuild
        );
    }

    #[test]
    fn acquire_propagates_real_errors() {
        // 设备丢失、内存不足这类是真错误，必须上抛而不是伪装成「重建」。
        for e in [
            vk::Result::ERROR_DEVICE_LOST,
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
            vk::Result::ERROR_INITIALIZATION_FAILED,
        ] {
            assert!(map_acquire_result(Err(e)).is_err(), "{e:?} 应上抛");
        }
    }

    #[test]
    fn present_ok_is_presented_and_suboptimal_is_outdated() {
        assert_eq!(
            map_present_result(Ok(false), None).unwrap(),
            PresentResult::Presented
        );
        // ash 0.38 的第二个值是 suboptimal，等价于需要重建
        assert_eq!(
            map_present_result(Ok(true), None).unwrap(),
            PresentResult::Outdated
        );
    }

    #[test]
    fn present_outdated_maps_to_rebuild() {
        assert_eq!(
            map_present_result(Err(vk::Result::from_raw(RESULT_OUT_OF_DATE_KHR)), None).unwrap(),
            PresentResult::Outdated
        );
    }

    #[test]
    fn present_reads_per_swapchain_result() {
        // 整体 SUCCESS 但单个交换链 OUT_OF_DATE：只看整体码会漏掉重建信号。
        assert_eq!(
            map_present_result(
                Ok(false),
                Some(vk::Result::from_raw(RESULT_OUT_OF_DATE_KHR))
            )
            .unwrap(),
            PresentResult::Outdated
        );
        // 单交换链 suboptimal 也要能看出来
        assert_eq!(
            map_present_result(
                Ok(false),
                Some(vk::Result::from_raw(RESULT_SUBOPTIMAL_KHR))
            )
            .unwrap(),
            PresentResult::Presented
        );
    }

    #[test]
    fn present_propagates_other_errors() {
        for e in [
            vk::Result::ERROR_DEVICE_LOST,
            vk::Result::ERROR_OUT_OF_HOST_MEMORY,
            // 扩展错误码：OUT_OF_DATE 之外的 -1000001003 家族成员
            vk::Result::from_raw(RESULT_SUBOPTIMAL_KHR - 1),
        ] {
            assert!(
                map_present_result(Err(e), None).is_err(),
                "{e:?} 不应被当成 Outdated"
            );
        }
        // 表面丢失（-1000000000）同样必须上抛
        assert!(map_present_result(Err(vk::Result::from_raw(-1_000_000_000)), None).is_err());
    }

    #[test]
    fn draw_input_helper_uses_uint32_indices_by_default() {
        let input = DrawInput::single_batch(vk::Buffer::null(), vk::Buffer::null());
        assert_eq!(input.index_type, vk::IndexType::UINT32);
        assert_eq!(input.batches.len(), 1);
        assert_eq!(input.batches[0].index_count, 3);
        assert_eq!(input.vertex_offset, 0);
        assert_eq!(input.index_offset, 0);
    }

    #[test]
    fn zero_index_count_batches_must_be_filtered() {
        // draw_indexed 传 index_count = 0 是未定义行为，
        // record() 里的过滤就是为此存在；这里守住该前提。
        let input = DrawInput {
            batches: &[
                DrawBatch {
                    index_offset: 0,
                    index_count: 0,
                },
                DrawBatch {
                    index_offset: 3,
                    index_count: 6,
                },
            ],
            ..Default::default()
        };
        let drawable: Vec<DrawBatch> = input
            .batches
            .iter()
            .copied()
            .filter(|b| b.index_count > 0)
            .collect();
        assert_eq!(drawable.len(), 1);
        assert_eq!(drawable[0].index_offset, 3);
    }

    #[test]
    fn vertex_stride_matches_pipeline_layout() {
        // 与 pipeline.rs 的属性布局对齐：pos(8) + uv(8) + color(4)
        assert_eq!(2 * 4 + 2 * 4 + 4, 20);
    }

    #[test]
    fn format_selection_matches_swapchain_rules() {
        assert!(crate::pick_format(&[]).is_err());
        // 单格式原样采用
        assert_eq!(
            crate::pick_format(&[vk::SurfaceFormatKHR {
                format: vk::Format::R8G8B8A8_UNORM,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            }])
            .unwrap()
            .0,
            vk::Format::R8G8B8A8_UNORM
        );
        // 多格式选 8 位
        let both = [
            vk::SurfaceFormatKHR {
                format: vk::Format::R16G16B16A16_SFLOAT,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
            vk::SurfaceFormatKHR {
                format: vk::Format::B8G8R8A8_UNORM,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
        ];
        assert_eq!(
            crate::pick_format(&both).unwrap().0,
            vk::Format::B8G8R8A8_UNORM
        );
        // 只有一种格式时必须原样采用——哪怕它不是 8 位 RGBA。
        // Vulkan 规范的明确要求，与 lib.rs 的 pick_format 保持一致。
        let only = [vk::SurfaceFormatKHR {
            format: vk::Format::D32_SFLOAT,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        }];
        assert_eq!(
            crate::pick_format(&only).unwrap().0,
            vk::Format::D32_SFLOAT
        );
        // 只有多种格式且全都不支持 8 位 RGBA 时才报错
        let none = [
            vk::SurfaceFormatKHR {
                format: vk::Format::D32_SFLOAT,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
            vk::SurfaceFormatKHR {
                format: vk::Format::R16G16B16A16_SFLOAT,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
        ];
        assert!(crate::pick_format(&none).is_err());
    }

    #[test]
    fn format_selection_has_single_source_of_truth() {
        // 历史上 frame.rs 曾持有 pick_format 的副本，两份逻辑一旦分叉就会
        // 出现「framebuffer 附件格式 ≠ 渲染通道格式」，驱动在
        // cmd_begin_render_pass 时崩溃，且错误信息完全不指向根因。
        //
        // 现在已统一：frame.rs 直接调用 crate::pick_format，不存在副本。
        // 本测试改为守住「重建时拿到的格式必须与建渲染通道时一致」这条不变量。
        let probe = crate::pick_format(&[vk::SurfaceFormatKHR {
            format: vk::Format::B8G8R8A8_UNORM,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        }])
        .unwrap();
        // FrameRenderer::new / rebuild_swapchain 都会把交换链实际返回的格式
        // 与这个值比对，不等即 bail。渲染通道就是按这个 probe 建的。
        assert_eq!(probe.0, vk::Format::B8G8R8A8_UNORM);
    }

    #[test]
    fn draw_input_default_has_no_descriptor_override() {
        // descriptor_set 为 null 时 record() 回退到帧槽位自带的描述符集。
        // 若默认值不是 null，上层忘填就会绑到空描述符集上，驱动直接崩。
        let input = DrawInput::single_batch(vk::Buffer::null(), vk::Buffer::null());
        assert!(input.descriptor_set.is_null());
    }

    // -- StagingArena 的纯逻辑 -------------------------------------------

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

    // -- in_flight 拒绝重复录制 -------------------------------------------

    #[test]
    fn upload_requires_acquire_first() {
        // 未 acquire 就录制：命令会被后续 present 遗弃
        assert!(can_record_upload(false, false).is_err());
    }

    #[test]
    fn upload_rejected_while_slot_in_flight() {
        // 核心回归：GPU 还在执行时重复录制必须报错而非静默重复录制。
        let err = can_record_upload(true, true).unwrap_err().to_string();
        assert!(err.contains("GPU"), "错误信息应说明原因，实际：{err}");
        assert!(
            err.contains("acquire"),
            "错误信息应指引调用方如何恢复，实际：{err}"
        );
    }

    #[test]
    fn upload_allowed_when_slot_idle() {
        // 正常路径：已 acquire 且上一轮 GPU 工作已完成 → 放行
        assert!(can_record_upload(true, false).is_ok());
    }

    #[test]
    fn in_flight_check_precedes_nothing_else() {
        // 两个错误都要能被区分：未 acquire 的错误不能被误报成 in_flight，
        // 否则调用方会去调 acquire 之外的错误路径。
        let not_pending = can_record_upload(false, false).unwrap_err().to_string();
        assert!(not_pending.contains("acquire"));
        assert!(!not_pending.contains("GPU"));
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
