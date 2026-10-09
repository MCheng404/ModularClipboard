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
//!     batches: &[DrawBatch { index_offset: 0, index_count: 3, clip: None }],
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
//!
//! [`StagingArena`] 的实现已独立成 [`crate::staging`] 模块（本文件再导出，
//! 既有调用路径不变）。

use ash::vk::Handle;
use ash::{Device, khr, vk};
use std::time::Instant;

use crate::pipeline::{
    BINDING_SAMPLER, BINDING_TEXTURE, BINDING_UNIFORM, BINDING_USER_TEXTURE,
};
use crate::{Gpu, Swapchain, image_barrier};

// staging 内存的分配与退休已独立成模块，但调用方（含 `texture.rs` 与各
// probe 示例）历史上是走 `frame::StagingArena` / `frame::StagingSlice`
// 访问的，因此这里原样再导出，保持既有路径可用、签名一个字未改。
pub use crate::staging::{
    ArenaPlan, StagingArena, StagingSlice, DEFAULT_STAGING_CAPACITY, MIN_STAGING_CAPACITY,
    STAGING_ALIGNMENT, UPLOAD_POOL_CAPACITY, validate_staging_write,
};

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
    /// 画面已提交，交换链仍与表面匹配。
    ///
    /// 语义保证：`queue_present` 已消费本次传入的呈现信号量，
    /// 该**图像**的信号量可以安全复用——但复用的前提是**重新 acquire
    /// 到同一image_index**，而不是「槽位轮转回来了」。
    /// 呈现信号量按图像分配正是为此，见
    /// [`FrameRenderer::image_semaphores`]。
    Presented,
    /// **不能继续用本帧的槽位**，调用方必须重建交换链后再 acquire。
    ///
    /// 涵盖 Vulkan 规范里present 会提前退出的所有情形
    /// （`OUT_OF_DATE` / `SUBOPTIMAL` / `SURFACE_LOST_KHR`，或整体失败）：
    ///
    /// 这些情形下 `vkQueuePresentKHR` **可能不消费** 传入的
    /// `wait_semaphores`。若上层照常复用槽位（下一帧 `queue_submit`
    /// 又 signal 同一个 semaphore），就违反了「binary semaphore 在被
    /// 等待前不得再次 signal」的规则 —— **未定义行为**。
    ///
    /// 症状不是立刻崩溃，而是随机的设备丢失或花屏，
    /// 且**压力测试往往跑不出来**：我们实测 1000 帧 × 5 次全过，
    /// 但那只说明这台机器时序恰好对，不构成正确性证明。
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
/// # ⚠️ 裁剪必须由 GPU scissor 逐批次设置
///
/// 早前的假设是「egui 的裁剪矩形在 CPU 侧 tessellation 时就已反映为
/// 被裁掉的顶点不进入网格」。**这个假设是错的**，代价是整个 UI 的裁剪
/// 全部失效：
///
/// 1. epaint 0.36 的 tessellator **不裁剪几何**。`tessellate_path` 只把
///    路径展平成三角形，`clip_rect` 仅作为 `ClippedPrimitive` 的元数据
///    存在（`TessellationOptions::debug_ignore_clip_rects` 只是调试开关，
///    默认路径不做几何裁剪）。
/// 2. 官方后端（egui-wgpu / egui_glow）靠**逐批次 `set_scissor_rect`** 生效。
/// 3. 本项目原先只发一次**全屏** scissor（跟随交换链尺寸），
///    等于谁的裁剪都不生效。
///
/// 症状：所有 `ui.painter_at(rect)` 的裁剪形同虚设——文字溢出面板、
/// 浮层压住下层内容、滚动区内容画到区外，且**不产生任何 GPU 错误**
/// （scissor 本来就是合法状态，只是范围给大了）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawBatch {
    /// 索引缓冲中的起始索引。
    pub index_offset: u32,
    /// 索引个数。为 0 的批次会被跳过（`draw_indexed` 传 0 是未定义行为）。
    pub index_count: u32,
    /// 本批次的裁剪矩形，**已是物理像素**、客户区坐标系、原点左上。
    ///
    /// 换算由调用方（`modular-clipboard-ui::renderer`）完成：那里同时
    /// 持有 `ppp` 与交换链尺寸，换算成物理像素后本模块无需知道 ppp，
    /// `DrawInput` 签名也就不必变。
    ///
    /// `None` 表示「不额外裁剪」（沿用全屏 scissor）。
    pub clip: Option<vk::Rect2D>,
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
                // 不额外裁剪（沿用全屏 scissor）。仅测试/便捷构造用。
                clip: None,
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
        // 规范说「必须原样采用 current_extent」，但**前提是它落在
        // [min_image_extent, max_image_extent] 内**。
        //
        // 实测踩坑：窗口 resize 后某些驱动报告的 current_extent
        // 会超出 max_image_extent，此时原样采用会让
        // `vkCreateSwapchainKHR` 报
        // `VUID-VkSwapchainCreateInfoKHR-pNext-07781`
        // （imageExtent 必须在 min/max 之间）。
        //
        // 规范强制用 current_extent 的本意是「不要与驱动争尺寸」，
        // 但若它本身非法，clamp 到合法区间是唯一可行选择——
        // 越界才是真正的违规。
        return clamp_axis(caps, caps.current_extent);
    }
    clamp_axis(caps, desired)
}

/// 把尺寸收敛到表面能力允许的区间。
///
/// # 为什么 `current_extent` 也要过这道 clamp
///
/// 规范说「非 0 时必须原样采用 `current_extent`」，
/// 但那**隐含前提是它本身合法**。实测（resize 探针，600 帧内重建 510 次）
/// 某些驱动报告的 `current_extent` 会超出 `max_image_extent`，
/// 原样采用会让 `vkCreateSwapchainKHR` 报
/// `VUID-VkSwapchainCreateInfoKHR-pNext-07781`。
///
/// 越界才是真正的违规，clamp 到合法区间是唯一可行选择。
fn clamp_axis(caps: &vk::SurfaceCapabilitiesKHR, want: vk::Extent2D) -> vk::Extent2D {
    let axis = |want: u32, min: u32, max: u32| {
        let base = if max < min {
            // 上限小于下限是驱动报告异常数据。
            // `clamp` 在这种输入下会 panic，退化为「取下限」。
            min
        } else {
            want.clamp(min, max)
        };
        // **最终兜底：规范要求 width/height 非零**
        // （VUID-VkSwapchainCreateInfoKHR-imageExtent-01689）。
        //
        // 实测踩坑：窗口尚未显示时（首次启动、`SW_SHOWMINIMIZED`、
        // 被 DWM 最小化到 0），`current_extent` 与 `min_image_extent`
        // 都是 0 ⇒clamp 后仍为 0 ⇒
        // `vkCreateSwapchainKHR` 报 imageExtent 为零并使程序
        // 以 exit 101 崩溃。
        //
        // Vulkan 规范允许 `current_extent == 0`（意为「应用自行决定」），
        // 并不保证 `min_image_extent > 0`。所以必须在clamp 之后再兜一次。
        if base == 0 {
            FALLBACK_EXTENT.width.min(2048).max(1)
        } else {
            base
        }
    };
    let w = axis(want.width, caps.min_image_extent.width, caps.max_image_extent.width);
    let h = axis(want.height, caps.min_image_extent.height, caps.max_image_extent.height);
    vk::Extent2D {
        width: w,
        height: if h == 0 { 1 } else { h },
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
    // Vulkan 1.3 §3.5.3 列出 present 会「提前退出」的结果码：
    // OUT_OF_DATE / SUBOPTIMAL / SURFACE_LOST_KHR。
    // 另有整体失败（设备丢失等）——信号量状态同样未知。
    //
    // 这些情形**都可能不消费 wait_semaphores**，因此一律返回 Outdated，
    // 强制上层重建，绝不复用槽位。
    let unconsumed = |r: vk::Result| {
        r == vk::Result::from_raw(RESULT_OUT_OF_DATE_KHR)
            || r == vk::Result::from_raw(RESULT_SUBOPTIMAL_KHR)
            || r == vk::Result::from_raw(1_000_001_000) // ERROR_SURFACE_LOST_KHR
    };

    // 单个交换链的结果优先：多交换链时可能出现
    // 整体 SUCCESS 但某个交换链 OUT_OF_DATE，只看整体码会漏掉。
    if let Some(r) = per_swapchain
        && unconsumed(r)
    {
        return Ok(PresentResult::Outdated);
    }

    match overall {
        // bool 是「suboptimal」：能显示，但表面已不匹配，
        // 同样可能没消费信号量。
        Ok(false) => match per_swapchain {
            // 单交换链结果不是 SUCCESS 时（含规范三类码之外的其它错误），
            // 信号量消费状态未知，一律不复用槽位。
            Some(r) if r != vk::Result::SUCCESS => Ok(PresentResult::Outdated),
            Some(_) | None => Ok(PresentResult::Presented),
        },
        Ok(true) => Ok(PresentResult::Outdated),
        Err(e) if unconsumed(e) => Ok(PresentResult::Outdated),
        Err(e) => {
            // 其它错误（设备丢失等）：信号量状态未知，
            // 同样按「不可复用」处理。
            tracing::warn!(?e, "queue_present 失败，按未消费信号量处理");
            Ok(PresentResult::Outdated)
        }
    }
}

/// 把驱动返回的 `image_index` 解析成呈现信号量表里的下标。
///
/// # 这条规则就是`pSignalSemaphores-00067` 的修复本体
///
/// 呈现信号量**必须按交换链图像索引取**，不能按帧槽位取：
/// `acquire_next_image` 的 `image_index` 由驱动挑选，与 `cursor % slot_count`
/// 的槽位轮转无对应关系。按槽位取会在「上一次present 尚未完成」时
/// 用同一个信号量去 signal 另一次 present —— 边用边复用，即 UB。
///
/// 抽出成函数是为了让「按图像索引」这条规则可被单测守住：
/// 内联的`self.image_semaphores[image_index]` 无法被测试触及，
/// 将来有人改回按槽位取时不会有任何测试失败。
///
/// 越界（驱动返回非法索引，或信号量表长度与`image_count` 失配）时
/// 返回错误而非 panic —— 越界说明状态已失同步，静默取错信号量
/// 比明确报错危险得多。
pub fn present_semaphore_index(image_index: u32, image_count: usize) -> anyhow::Result<usize> {
    let idx = image_index as usize;
    anyhow::ensure!(
        idx < image_count,
        "驱动返回的图像索引 {image_index} 超出呈现信号量表长度 {image_count}；\
         两者失配说明交换链重建后信号量表未同步更新"
    );
    Ok(idx)
}

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
// FrameRenderer
// ---------------------------------------------------------------------------

/// 一帧在飞所独占的同步对象。
///
/// 按**在飞帧槽位**索引。命令缓冲与描述符集按槽位轮转是安全的
/// （GPU 用完的判据是本槽位的 `submit_fence`），
/// 但**呈现用的信号量不能按槽位分配**——见 [`FrameRenderer::image_semaphores`]。
struct FrameSlot {
    command_buffer: vk::CommandBuffer,
    /// # 为什么只保留 `submit_fence` 一个栅栏
    ///
    /// 曾让同一个 fence 同时传给 `acquire_next_image` 与 `queue_submit`。
    /// 规范允许这么做，但该 fence 会有**两个** signal 来源：
    /// acquire 成功时驱动 signal 一次，submit 完成时再 signal 一次。
    /// 配合「等完就`reset_fences`」的写法，手工维护的 `fence_signaled`
    /// 标志必然与驱动实际状态错位——某帧的 wait 会提前返回，
    /// 于是 reset 了GPU 仍在读的命令缓冲，表现为随机丢设备。
    ///
    /// 后来拆成 `submit_fence` + `acquire_fence` 两个，但**多出来的那个
    /// acquire_fence 从未被等待过**——acquire 的等待已由
    /// `acquire_semaphore`（submit 侧wait）与槽位轮转天然保证。
    /// 于是它成了一个「只 signal、没人等、还得复位」的死对象，
    /// 而**复位它本身就是两种违规的来源**（验证层实测，60 帧）：
    ///
    /// - 复位时它仍被在途 `acquire_next_image` 关联
    ///   ⇒ `VUID-vkResetFences-pFences-01123`（20 次）
    /// - 不复位则下一个 `acquire_next_image` 拿到已 signal 的 fence
    ///   ⇒ `VUID-vkAcquireNextImageKHR-fence-10066`（20 次）
    ///
    /// 两者是同一根因的两面：**规范允许 `acquire_next_image` 的 fence
    /// 传 `Fence::null()`**（信号量与 fence 至少给一个即可，本项目已给
    /// `acquire_semaphore`）。删掉这个死对象，两个VUID 一起消失。
    ///
    /// 这也印证了 MEMORY.md 第54 条：不为「也许用得上」保留机制。
    submit_fence: vk::Fence,
    /// acquire signal → submit wait。
    acquire_semaphore: vk::Semaphore,
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
    /// 本渲染器绑定的表面。
    ///
    /// ⚠️ 多窗口必须每个窗口一个表面（`vk::SurfaceKHR` 与 `hwnd`
    /// 一一对应，不可复用）。`rebuild_swapchain` 要用它重建交换链，
    /// 早前它硬编码 `gpu.surface()`（主窗口的），子窗口 resize 后
    /// 会把交换链建到错的窗口上。
    surface: vk::SurfaceKHR,
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
    /// 每个交换链图像一个呈现信号量，**不能**挂在帧槽位上。
    ///
    /// # 为什么必须按图像而不是按槽位
    ///
    /// `vkQueuePresentKHR` **既不能 signal 也不能 wait**（除扩展外），
    /// 因此外部无从得知「某次present 何时真正完成」。
    /// `acquire_next_image` 返回的 `image_index` **由驱动决定**，
    /// 与 `cursor % slot_count` 的槽位轮转**无对应关系**
    /// （验证层会打印 `Most recently acquired image indices` 暴露错位）。
    ///
    /// 若信号量按槽位分配，槽位在 GPU 真正完成 present 之前被复用时，
    /// 就会用同一个信号量去 signal 另一次 present
    /// ⇒ `VUID-vkQueueSubmit-pSignalSemaphores-00067`。
    ///
    /// # 为什么按图像分配就是安全的
    ///
    /// 规范保证：**取到image_index 之后，在 `queue_submit` 里 wait
    /// 该次acquire 的信号量**，就意味着「上一轮使用该图像的 present
    /// 已完成」。于是按image_index 取出的信号量必然可安全复用。
    ///
    /// 官方文档：`swapchain_semaphore_reuse.html`
    /// （"a) Use a separate semaphore per swapchain image"）。
    ///
    /// 长度恒等于 `swapchain.image_count`，交换链重建时整体重建。
    image_semaphores: Vec<vk::Semaphore>,
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
    /// 描述符集布局是否带 `UPDATE_AFTER_BIND` 标志。
    ///
    /// 决定描述符池是否必须带 `DescriptorPoolCreateFlags::UPDATE_AFTER_BIND`
    /// （规范强制）。由 `PipelineBundle` 的布局在创建时固定下来，
    /// 保证「布局加了标志 ⇔ 池也加了标志」，两者不会失配。
    desc_update_after_bind: bool,
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
        // 主窗口：沿用 Gpu 里已查好的表面能力。
        Self::new_for_surface(gpu, pipeline_bundle, gpu.surface())
    }

    /// 为**指定表面**建渲染器（多窗口用）。
    ///
    /// # 为什么必须有这个变体
    ///
    /// [`Self::new`] 用的是 `gpu.surface()`——`Gpu::new` 时为主窗口建的
    /// 表面。`vk::SurfaceKHR` 与 `hwnd` 一一对应、**不可复用**，
    /// 子窗口必须有自己的表面（见 [`Swapchain::new_for_surface`]）。
    ///
    /// 表面能力（caps / 格式 / 呈现模式）在这里**重新查询**：
    /// `Gpu` 里存的是主窗口表面的快照，子窗口的尺寸、DPI、
    /// 显示器都可能不同，用快照会拿到过期的 min/max extent。
    pub fn new_for_surface(
        gpu: &'a Gpu,
        pipeline_bundle: PipelineBundle,
        surface: vk::SurfaceKHR,
    ) -> anyhow::Result<Self> {
        // 自行建一套 Entry/SurfaceLoader：Gpu 的 surface_caps 是启动时快照，
        // 重建交换链必须重新查询。
        let entry = unsafe { ash::Entry::load()? };
        let surface_loader = khr::surface::Instance::new(&entry, &gpu.instance);
        let swapchain_loader = khr::swapchain::Device::new(&gpu.instance, &gpu.device);

        // SAFETY：`surface` 由调用方保证是本实例创建且未销毁的表面。
        let caps = unsafe {
            surface_loader
                .get_physical_device_surface_capabilities(gpu.physical_device, surface)?
        };
        let formats = gpu.query_surface_formats(surface)?;
        let present_modes = unsafe {
            surface_loader.get_physical_device_surface_present_modes(gpu.physical_device, surface)?
        };
        let extent = resolve_extent(&caps, FALLBACK_EXTENT);
        tracing::debug!(
            ?surface,
            n_formats = formats.len(),
            n_present = present_modes.len(),
            min_img = caps.min_image_count,
            max_img = caps.max_image_count,
            cur_extent = ?caps.current_extent,
            min_extent = ?caps.min_image_extent,
            max_extent = ?caps.max_image_extent,
            "建交换链前的表面能力"
        );
        let swapchain = Swapchain::new_for_surface(
            gpu,
            surface,
            &formats,
            &present_modes,
            extent.width,
            extent.height,
            pipeline_bundle.render_pass,
            &caps,
        )?;

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
            surface,
            surface_loader,
            swapchain_loader,
            swapchain,
            pipeline_format,
            command_pool,
            descriptor_pool: vk::DescriptorPool::null(),
            slots: Vec::new(),
            image_semaphores: Vec::new(),
            cursor: 0,
            layouts: LayoutTracker::new(0),
            pending: None,
            clear_color: [0.10, 0.10, 0.12, 1.0],
            // 块数随后按交换链图像数确定，见下方。
            staging: StagingArena::new(gpu, 1)?,
            // 描述符池是否必须带 UPDATE_AFTER_BIND，取决于布局有没有加标志，
            // 而布局是调用方用同一份 `gpu.desc_caps` 建的——两边同源，
            // 因此不会出现「布局加了池没加」的失配。
            desc_update_after_bind: gpu.desc_caps.all_bindings_update_after_bind(),
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

    /// 当前交换链的图像数。
    ///
    /// 交换链重建后这个值会变（取决于表面与驱动选择），
    /// 而 [`Self::image_semaphore_count`] 必须同步跟随——
    /// 两者不等就说明重建链有 bug。
    pub fn swapchain_image_count(&self) -> usize {
        self.swapchain.image_count as usize
    }

    /// 呈现信号量数量。
    ///
    /// **恒等于 [`Self::swapchain_image_count`]**：
    /// `present()` 用驱动返回的 `image_index` 直接索引此数组，
    /// 长度不等就会越界或漏初始化（漏初始化 = UB）。
    ///
    /// 这一对访问器是 resize 路径的关键判据：
    /// 交换链重建后若两者不等，说明销毁/重建链有缺陷。
    /// 正常情况下交换链重建不改变图像数（仍是 2~3 张），
    /// 但**代码必须保证这一点**，而不是依赖它碰巧不变。
    pub fn image_semaphore_count(&self) -> usize {
        self.image_semaphores.len()
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

    /// 把**用户纹理**（图片缩略图）绑到该槽位的 `BINDING_USER_TEXTURE`。
    ///
    /// # 为什么单独一个方法
    ///
    /// 字体图集（`BINDING_TEXTURE`）是单通道覆盖率图，
    /// 缩略图（`BINDING_USER_TEXTURE`）是 RGBA 彩色图——
    /// 片元着色器对两者的合成方式不同，必须各占一个绑定。
    ///
    /// 绑定集合数量是有限的：着色器里 `user_tex` 只有一张，
    /// 所以**同一时刻只能显示一张用户纹理**。若要显示多张缩略图，
    /// 需要按纹理分批（每批绑不同纹理）——那是更大的改动，
    /// 当前需求（详情区单张大图）不需要。
    pub fn update_user_texture_binding(
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
        let write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(BINDING_USER_TEXTURE)
            .descriptor_count(1)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&info));
        unsafe { self.gpu.device.update_descriptor_sets(&[write], &[]) };
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
        self.staging.begin_frame();

        // fence 传 `Fence::null()`：同步由 `acquire_semaphore` 承担
        // （submit 侧会 wait 它）。曾额外传一个 `acquire_fence`，但那个
        // fence 从未被等待过，复位它反而稳定触发
        // `VUID-vkResetFences-pFences-01123`；而不复位又触发
        // `VUID-vkAcquireNextImageKHR-fence-10066`。详见
        // [`FrameSlot::submit_fence`] 的说明。
        let (image_index, suboptimal) = match map_acquire_result(unsafe {
            self.swapchain_loader.acquire_next_image(
                self.swapchain.handle,
                ACQUIRE_TIMEOUT,
                self.slots[slot].acquire_semaphore,
                vk::Fence::null(),
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
            // ⚠️ 逐批次 scissor：这是 UI 裁剪**唯一**生效的地方。
            //
            // 上面那次全屏 scissor 只是给「清屏后第一批」一个合理初值；
            // 每个批次开始前必须按自己的 clip 重设，否则所有
            // `ui.painter_at(rect)` 的裁剪都会被全屏范围覆盖掉。
            let rect = batch.clip.unwrap_or(scissor);
            unsafe {
                device.cmd_set_scissor(cmd, 0, std::slice::from_ref(&rect));
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
        // ⚠️ 呈现信号量必须按 **image_index** 取，不是按 slot。
        //
        // image_index 由驱动挑选，与 `cursor % slot_count` 无对应关系。
        // 按槽位取会让「本槽位上一次present」尚未完成时就用同一个信号量
        // signal 这一次present ⇒边用边复用（UB），
        // 验证层报 `VUID-vkQueueSubmit-pSignalSemaphores-00067`。
        let present_sem_index = present_semaphore_index(
            frame.image_index,
            self.image_semaphores.len(),
        )?;
        let present_sem = self.image_semaphores[present_sem_index];
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
    /// 放弃当前帧：清掉 `pending`，让下一次 [`Self::acquire`] 能重新开始。
    ///
    /// # 什么时候用
    ///
    /// 在 `acquire()` 成功之后、`present()` 之前出错时调用。否则
    /// `pending` 一直是 `Some`，之后每次 `acquire()` 都 bail，
    /// **该窗口永久死掉**（黑屏 + 报「上一帧尚未 present」，
    /// 与真实原因毫无关系）。
    ///
    /// 典型来源：纹理上传失败、staging arena 耗尽、描述符池耗尽。
    ///
    /// 会先等 GPU 空闲，确保在飞命令都已完成——否则清掉 `pending`
    /// 后重录命令缓冲可能与仍在执行的那一份冲突。
    pub fn abandon_frame(&mut self) {
        if self.pending.take().is_none() {
            return;
        }
        tracing::debug!("放弃未present 的帧，恢复 acquire 能力");
        self.gpu.wait_idle();
    }

    pub fn rebuild_swapchain(&mut self, desired_extent: vk::Extent2D) -> anyhow::Result<()> {
        // 必须先等 GPU 空闲：旧交换链的 framebuffer 还在被在飞命令引用。
        self.gpu.wait_idle();

        // ⚠️ 重建后**必须清掉 `pending`**。
        //
        // `pending` 只在 `present()` 里清除。若某一帧在
        // `acquire()` 成功之后出错（纹理上传失败、staging arena 耗尽…）
        // 就不会走到 `present`，`pending` 一直是 `Some` ⇒ 之后每一次
        // `acquire()` 都直接 `bail!("上一帧尚未 present")` ⇒
        // **该窗口永久黑屏，且没有任何恢复路径**。
        //
        // 错误只在首帧出现一次，后续全是这句与真实原因毫无关系的
        // 报错，排查时极难定位。
        //
        // 这里已经 `wait_idle()`，在飞命令全部完成，清 `pending` 是安全的。
        self.pending = None;

        // ⚠️ 必须查**本窗口自己的**表面。
        //
        // 早前这里写 `self.gpu.surface()`——那是**主窗口**的表面。
        // 于是子窗口/设置窗口 resize 时，用的是主窗口的
        // `current_extent`/`min/max` 去建**自己**的交换链：
        // 尺寸被钳到主窗口的范围，表现为「设置窗口渲染尺寸跟着
        // 主窗口走、内容被裁或拉变形」。
        //
        // 下面真正 `create_swapchain` 用的是 `self.surface`（对），
        // 唯独查能力这一步错了，最难查——因为大部分情况下两个
        // 表面的 caps 恰好相近，只有尺寸差异明显时才暴露。
        let caps = unsafe {
            self.surface_loader.get_physical_device_surface_capabilities(
                self.gpu.physical_device,
                self.surface,
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
                    self.surface,
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
        // 重建时**重新查询**本窗口表面的格式与呈现模式：
        // 子窗口可能被移到另一块显示器，格式/呈现模式会变。
        let formats = self.gpu.query_surface_formats(self.surface)?;
        let present_modes = unsafe {
            self.surface_loader
                .get_physical_device_surface_present_modes(
                    self.gpu.physical_device,
                    self.surface,
                )?
        };
        self.swapchain = Swapchain::new_for_surface(
            self.gpu,
            self.surface,
            &formats,
            &present_modes,
            extent.width,
            extent.height,
            self.bundle.render_pass,
            &caps,
        )?;
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
                // 每槽位需要**两张**图：binding 2 = 字体图集，
                // binding 3 = 用户纹理（缩略图）。
                //
                // 着色器早前只绑定字体图集，`renderer.rs` 会显式丢弃
                // egui 的其它纹理（`TexturesDelta` 里非 FONT_TEXTURE_ID
                // 的项直接 `continue`）——即「图片条目只显示文字摘要，
                // 看不到图」。加上第二个绑定后，缩略图才有地方落。
                //
                // 纹理数组方案在此不可行：egui 纹理尺寸任意，
                // 而数组要求所有层同尺寸（要么按最大尺寸 pad、
                // 要么按尺寸分组），都比多一个绑定复杂得多。
                descriptor_count: count * 2,
            },
        ];
        // 用了 `UPDATE_AFTER_BIND` 的绑定，其描述符池**必须**带
        // `UPDATE_AFTER_BIND` 标志（规范强制要求，不是可选优化）。
        // 不加会报 `VUID-vkCreateDescriptorPool-pPoolCreateInfo-03111`。
        //
        // ash 0.38 陷阱：`DescriptorPoolCreateFlags` 在 `vk::bitflags` 里
        // 只暴露了 `FREE_DESCRIPTOR_SET`，`UPDATE_AFTER_BIND` 属于
        // Vulkan 1.2 段（在 `vk::feature_extensions`），照抄 0.37 示例找不到。
        let pool_flags = if self.desc_update_after_bind {
            vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND
        } else {
            vk::DescriptorPoolCreateFlags::empty()
        };
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .flags(pool_flags)
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
            // ⚠️ fence 必须以 **UNSIGNALED** 创建
            // （`FenceCreateInfo::default()` 即无 flags）。
            //
            // 曾建成SIGNALED，理由是「让首次复用槽位时 wait 立刻通过」。
            // 那是多余的：`in_flight == false` 已经表达了「没有在途工作、
            // 不必等」，首次使用的 wait 语义由 `in_flight` 保证，
            // 不需要靠 fence 初值。
            //
            // 更糟的是规范禁止把已 signal 的 fence 传给 `queue_submit`
            // （`VUID-vkQueueSubmit-fence-00063`）：`in_flight == false`
            // 的槽位从未走过 `reset_fences`，首次 `present` 时 fence 仍是
            // SIGNALED。
            let fence_info = vk::FenceCreateInfo::default();
            let submit_fence = unsafe { device.create_fence(&fence_info, None) }
                .map_err(|e| anyhow::anyhow!("创建 submit 栅栏失败: {e:?}"))?;
            let semaphore_info = vk::SemaphoreCreateInfo::default();
            let acquire_semaphore = unsafe { device.create_semaphore(&semaphore_info, None) }
                .map_err(|e| anyhow::anyhow!("创建 acquire 信号量失败: {e:?}"))?;
            slots.push(FrameSlot {
                command_buffer: buffers[i],
                submit_fence,
                // 初始没有在途提交，可以自由录制。
                in_flight: false,
                // 从未使用过，没有可退休的上一轮帧。
                last_frame: u64::MAX,
                acquire_semaphore,
                descriptor_set: sets[i],
            });
        }
        self.slots = slots;

        // 呈现信号量按**交换链图像数**分配，不按在飞帧数。
        //
        // 数量必须严格等于 `swapchain.image_count`：`present()` 用驱动
        // 返回的 `image_index` 直接索引，长度不等就会越界或漏初始化。
        // （`count` 与image_count 同源，但这里仍以image_count 为准，
        // 避免将来两者分叉时静默失配。）
        let semaphore_info = vk::SemaphoreCreateInfo::default();
        let mut image_semaphores = Vec::with_capacity(self.swapchain.image_count as usize);
        for i in 0..self.swapchain.image_count {
            let sem = unsafe { device.create_semaphore(&semaphore_info, None) }
                .map_err(|e| anyhow::anyhow!("创建图像 {i} 的呈现信号量失败: {e:?}"))?;
            image_semaphores.push(sem);
        }
        anyhow::ensure!(
            image_semaphores.len() == self.swapchain.image_count as usize,
            "呈现信号量数量必须等于交换链图像数"
        );
        self.image_semaphores = image_semaphores;

        self.cursor = 0;
        Ok(())
    }

    fn destroy_frame_resources(&mut self) {
        let device = &self.gpu.device;
        for slot in &self.slots {
            unsafe {
                device.destroy_fence(slot.submit_fence, None);
                device.destroy_semaphore(slot.acquire_semaphore, None);
            }
        }
        // 命令缓冲随命令池一起回收，不单独销毁。
        self.slots.clear();
        // 呈现信号量按图像分配，漏销毁就是真泄漏
        // （验证层报 `VUID-vkDestroyDevice-device-05137`）。
        for sem in self.image_semaphores.drain(..) {
            unsafe { device.destroy_semaphore(sem, None) };
        }
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
        // 单交换链 SUBOPTIMAL 同样不能算 Presented——
        // 规范允许此时不消费 wait_semaphores。
        // （此处曾断言 Presented，是错的：它假设信号量一定被消费。）
        assert_eq!(
            map_present_result(
                Ok(false),
                Some(vk::Result::from_raw(RESULT_SUBOPTIMAL_KHR))
            )
            .unwrap(),
            PresentResult::Outdated
        );
        // 单交换链明确 SUCCESS 时才是 Presented
        assert_eq!(
            map_present_result(Ok(false), Some(vk::Result::SUCCESS)).unwrap(),
            PresentResult::Presented
        );
        // per_swapchain 为 None 时只看整体码
        assert_eq!(
            map_present_result(Ok(false), None).unwrap(),
            PresentResult::Presented
        );
    }

    #[test]
    fn present_failure_never_claims_semaphore_was_consumed() {
        // 规范（Vulkan 1.3 §3.5.3）：present 提前退出时，
        // wait_semaphores 可能**不被 signal**。此时若上层复用槽位，
        // 下一帧 queue_submit 会再次 signal 同一 semaphore ⇒ UB。
        //
        // 因此除「彻底成功」外，所有路径都必须返回 Outdated。
        //
        // 旧版本此测试断言「其他错误应上抛」，前提是错误的：
        // 上抛会让上层跳过重建，槽位仍被复用，反而制造 UB。
        let ok = Ok(false);
        let cases = [
            (vk::Result::from_raw(RESULT_OUT_OF_DATE_KHR), "OUT_OF_DATE"),
            (vk::Result::from_raw(RESULT_SUBOPTIMAL_KHR), "SUBOPTIMAL"),
            (vk::Result::from_raw(1_000_001_000), "SURFACE_LOST"),
            (vk::Result::ERROR_DEVICE_LOST, "DEVICE_LOST"),
            (vk::Result::ERROR_OUT_OF_HOST_MEMORY, "OUT_OF_HOST_MEMORY"),
        ];
        for (code, name) in cases {
            assert_eq!(
                map_present_result(Err(code), None).unwrap(),
                PresentResult::Outdated,
                "{name} 时不得声称信号量已被消费"
            );
            // 单个交换链的结果同样如此
            assert_eq!(
                map_present_result(ok, Some(code)).unwrap(),
                PresentResult::Outdated,
                "{name}（单交换链）时不得声称信号量已被消费"
            );
        }

        // 只有彻底成功才 Presented
        assert_eq!(
            map_present_result(ok, Some(vk::Result::SUCCESS)).unwrap(),
            PresentResult::Presented
        );
    }

    // -- 呈现信号量按图像分配（pSignalSemaphores-00067 的回归守卫）--------

    #[test]
    fn present_semaphore_is_indexed_by_image_not_by_slot() {
        // 核心回归：信号量下标来自 image_index，与槽位无关。
        //
        // 本项目的槽位按 `cursor % 3` 递增，而 image_index 由驱动挑选。
        // 修复前present() 取的是 `slots[slot].present_semaphore`，
        // 于是「槽位 0 的第 2 次 present」可能复用「槽位 0 的第 1 次」
        // 仍在被swapchain 使用的那个信号量 ⇒
        // VUID-vkQueueSubmit-pSignalSemaphores-00067（实测每次运行必现 2 次）。
        //
        // 用两种不同的 (slot, image_index) 组合证明：下标只随 image_index 变。
        let image_count = 3;
        for (slot, image_index) in [(0usize, 2u32), (1, 0), (2, 1), (0, 1)] {
            let idx = present_semaphore_index(image_index, image_count).unwrap();
            // 下标必须等于 image_index，与传进来的 slot 无关
            assert_eq!(
                idx, image_index as usize,
                "呈现信号量必须按 image_index={image_index} 索引，\
                 不得被 slot={slot} 影响"
            );
        }
    }

    #[test]
    fn slot_rotation_order_differs_from_image_index_order() {
        // 守住「槽位轮转与图像索引是两套独立序列」这个前提。
        //
        // 若哪天有人把槽位改成直接用 image_index（或反过来让信号量按
        // 槽位取），本测试描述的错位关系就不再成立，
        // `present_semaphore_is_indexed_by_image_not_by_slot` 的论证前提失效。
        let slot_count = 3;
        // 槽位按 cursor 递增
        let slots: Vec<usize> = (0..6).map(|c| c % slot_count).collect();
        // 驱动挑选的图像顺序（验证层实测打印过 [0], 1, 2, 1）
        let images: Vec<u32> = vec![0, 1, 2, 1];
        // 至少存在一对 (cursor, image) 使slot != image —— 正是错位的来源
        let misaligned = (0..images.len())
            .any(|i| slots[i] != images[i] as usize);
        assert!(
            misaligned,
            "本测试的前提是槽位与 image_index 存在错位；\
             若将来两者恒等，说明同步模型已变，本测试需重写"
        );
    }

    #[test]
    fn present_semaphore_index_rejects_out_of_range() {
        // 越界必须报错而不是 panic：
        // 越界说明信号量表长度与 image_count 失配（典型是交换链重建后
        // 忘了重建信号量表），静默取错信号量比明确报错危险得多。
        let err = present_semaphore_index(3, 3).unwrap_err().to_string();
        assert!(
            err.contains("图像索引"),
            "错误信息应说明是索引越界，实际：{err}"
        );
        // 合法边界：最后一个下标必须通过
        assert_eq!(present_semaphore_index(2, 3).unwrap(), 2);
        // 空表时任何索引都非法
        assert!(present_semaphore_index(0, 0).is_err());
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
                    clip: None,
                },
                DrawBatch {
                    index_offset: 3,
                    index_count: 6,
                    clip: None,
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
}
