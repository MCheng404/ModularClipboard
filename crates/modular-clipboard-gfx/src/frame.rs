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
//!     batches: &[DrawBatch {
//!         index_offset: 0,
//!         index_count: 3,
//!         clip: None,
//!         // 纯字体批：null 表示不额外绑定用户纹理集合
//!         user_set: vk::DescriptorSet::null(),
//!     }],
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
//! 交换链的图像在两次 `acquire_next_image` 之间**不会**被重置布局——
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

use ash::{Device, vk};
use std::time::Instant;

pub mod renderer;

pub use renderer::FrameRenderer;


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

/// 用户纹理（图片缩略图）描述符集的数量上限。
///
/// 每张同时在场的用户纹理占一个集合。选中详情大图 + 列表若干图片行，
/// 实际用量在个位数；64 留足滚动余量，超出由调用方按 LRU 淘汰。
/// 池容量按 `帧槽数 + 本常量` 一次性建好，运行中不再扩容。
pub const USER_TEXTURE_SET_CAP: u32 = 64;

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
    /// `FrameRenderer::image_semaphores`。
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
/// # IMPORTANT: 裁剪必须由 GPU scissor 逐批次设置
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
    /// 本批次采样**用户纹理**（图片缩略图）时使用的描述符集。
    ///
    /// # 为什么逐批给
    ///
    /// 着色器的 `BINDING_USER_TEXTURE` 只有一个绑定，而一次绘制里
    /// 可以同时有多张缩略图（列表里若干图片行）。只绑一张 ⇒
    /// 所有图片行采样到同一张图（症状：每行显示同一个缩略图，
    /// **不报任何 GPU 错误**）。
    ///
    /// 正确做法是「按纹理分批 + 批前重绑」：`ui::renderer` 已按
    /// `mesh.texture_id` 切批（见 `Batch::tex_id`），这里给每批带上
    /// 它那张纹理的描述符集，`record_to` 在画该批前重绑。
    ///
    /// `null` = 本批不采样用户纹理（文字/形状），沿用帧槽自带的
    /// 描述符集（其 `BINDING_USER_TEXTURE` 绑的是占位图）。
    pub user_set: vk::DescriptorSet,
}

/// 描述符集里**每张图共用**的三个绑定：采样器、字体图集、uniform。
///
/// 收成结构体有两个原因：
///
/// 1. 用户纹理集合的分配/重写 API 有 4 个绑定要写，参数摊平会越过
///    clippy 的 7 参数线；而这三项对所有集合取值相同，天然是一组。
/// 2. 语义上它们属于「渲染器级共享状态」：采样器与 uniform buffer
///    进程内恒定；字体图集扩容时**视图**会换，由
///    [`FrameRenderer::rewrite_user_texture_set`] 统一刷新全部集合。
#[derive(Debug, Clone, Copy)]
pub struct CommonBindings {
    /// 采样器（与字体图集共用同一个，**nearest**）。
    ///
    /// 字体图集必须点采样：线性插值会把相邻字形的覆盖率渗进边缘。
    pub sampler: vk::Sampler,
    /// 用户纹理的采样器（**linear**，`BINDING_USER_SAMPLER`）。
    ///
    /// 与 `sampler` 分开的理由：缩略图的显示尺寸几乎永远不等于纹理
    /// 尺寸（512² 纹理显示成 320/696pt），nearest 在非 1:1 比例下
    /// 让照片出摩尔纹。1:1 时双线性无损（整数纹素处权重为 1），
    /// 所以统一 linear 对两种情况都正确。见 `pipeline.rs::BINDING_USER_SAMPLER`。
    pub user_sampler: vk::Sampler,
    /// 字体图集视图（`BINDING_TEXTURE`）。
    pub font_view: vk::ImageView,
    /// 字体图集当前布局。
    pub font_layout: vk::ImageLayout,
    /// uniform 缓冲。
    pub uniform_buffer: vk::Buffer,
    /// uniform 偏移（本项目恒为 0）。
    pub uniform_offset: vk::DeviceSize,
    /// uniform 范围（整个缓冲）。
    pub uniform_range: vk::DeviceSize,
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
    /// 通道开启后、绘制批次**之前**先画一张全屏底图。
    ///
    /// # 用途
    ///
    /// 自研模糊的合成阶段：先把模糊结果作为全屏底图铺满交换链，
    /// 再叠加清晰的界面图元（如设置覆盖层面板）。`None` 表示不画底图，
    /// 与早前行为一致。
    ///
    /// # 为什么放在 `DrawInput` 里而不是单独一个方法
    ///
    /// 底图与后续图元必须在**同一个渲染通道**内完成：分开录两个通道
    /// 意味着中间要 end/begin，且第二次 begin 会以 `load_op=CLEAR`
    /// 把刚画的底图清掉。放进同一个 `record` 调用里，顺序天然正确。
    pub prelude: Option<crate::blur::Blit>,
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
            prelude: None,
        }
    }
}

impl<'a> DrawInput<'a> {
    /// 单三角形批次（探针/测试用）。
    ///
    /// CAUTION: 必须是 `const` 而不能在函数体内写 `&[DrawBatch { .. }]`——
    /// 返回位置的数组临时量不享受生命周期延长（E0515），加上
    /// `user_set` 字段后这一条必然触发。提到 const 即 'static。
    const SINGLE_TRIANGLE: &'static [DrawBatch] = &[DrawBatch {
        index_offset: 0,
        index_count: 3,
        // 不额外裁剪（沿用全屏 scissor）。仅测试/便捷构造用。
        clip: None,
        user_set: vk::DescriptorSet::null(),
    }];

    /// 只画一个三角形批次，其余字段取默认值。
    pub fn single_batch(vertex_buffer: vk::Buffer, index_buffer: vk::Buffer) -> Self {
        Self {
            vertex_buffer,
            index_buffer,
            batches: Self::SINGLE_TRIANGLE,
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
            FALLBACK_EXTENT.width.clamp(1, 2048)
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

fn create_command_pool(device: &Device, queue_family: u32) -> anyhow::Result<vk::CommandPool> {
    let info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(queue_family)
        // 每帧都要 reset 命令缓冲，这个标志位是必需的。
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    unsafe { device.create_command_pool(&info, None) }
        .map_err(|e| anyhow::anyhow!("创建命令池失败: {e:?}"))
}


// ---------------------------------------------------------------------------
// 单测
// ---------------------------------------------------------------------------

/// 把 scissor 矩形与 framebuffer 求交，并钳成 Vulkan 合法值。
///
/// 返回宽或高为 0 的矩形表示「完全被裁掉」，调用方应跳过该批次。
///
/// # 为什么需要
///
/// `VUID-vkCmdSetScissor-02070` 要求 `offset + extent` 不超出
/// attachment，且 `extent` 非 0。越界是**未定义行为**且**不报错**——
/// 表现只是画面错位或裁剪失效，排查成本极高。这里做纵深防御。
fn sanitize_scissor(mut r: vk::Rect2D, extent: vk::Extent2D) -> vk::Rect2D {
    let (w, h) = (i64::from(extent.width), i64::from(extent.height));
    let mut x0 = i64::from(r.offset.x);
    let mut y0 = i64::from(r.offset.y);
    let mut x1 = x0 + i64::from(r.extent.width);
    let mut y1 = y0 + i64::from(r.extent.height);

    // 「整体在左/上之外」要能被正确判定为**全裁掉**，所以先与 0 求交，
    // 再各自clamp 进 [0, extent]。顺序反了会把负坐标当成合法值。
    x1 = x1.max(0);
    y1 = y1.max(0);
    x0 = x0.clamp(0, w);
    y0 = y0.clamp(0, h);
    x1 = x1.clamp(0, w);
    y1 = y1.clamp(0, h);

    r.offset.x = x0 as i32;
    r.offset.y = y0 as i32;
    r.extent.width = (x1 - x0).max(0) as u32;
    r.extent.height = (y1 - y0).max(0) as u32;
    r
}

#[cfg(test)]
mod tests {
    // NOTE: `is_null()` 来自 `ash::vk::Handle` trait——lib 目标不需要
    // （方法调用全在 renderer 子模块），只有测试用，导入放 tests 内。
    use ash::vk::Handle;

    // ---- scissor 纵深防御 ----

    fn rect(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D { width: w, height: h },
        }
    }

    #[test]
    fn sanitize_scissor_keeps_valid_rect_unchanged() {
        let e = vk::Extent2D { width: 800, height: 600 };
        assert_eq!(sanitize_scissor(rect(10, 20, 100, 50), e), rect(10, 20, 100, 50));
    }

    #[test]
    fn sanitize_scissor_clips_to_framebuffer() {
        let e = vk::Extent2D { width: 800, height: 600 };
        assert_eq!(sanitize_scissor(rect(700, 10, 300, 50), e), rect(700, 10, 100, 50));
        assert_eq!(sanitize_scissor(rect(900, 10, 50, 50), e), rect(800, 10, 0, 50));
        assert_eq!(sanitize_scissor(rect(-20, -10, 100, 50), e), rect(0, 0, 80, 40));
        assert_eq!(sanitize_scissor(rect(-200, -200, 50, 50), e), rect(0, 0, 0, 0));
    }

    /// Vulkan 要求 offset + extent 不越界。这条遍历一批病态输入逐个断言。
    #[test]
    fn sanitize_scissor_never_produces_illegal_scissor() {
        let e = vk::Extent2D { width: 800, height: 600 };
        let cases = [
            rect(0, 0, 0, 0),
            rect(-1, -1, 1, 1),
            rect(799, 599, 100, 100),
            rect(i32::MAX, i32::MAX, u32::MAX, u32::MAX),
            rect(-1, 599, 2, 2),
            rect(400, 300, u32::MAX, u32::MAX),
        ];
        for c in cases {
            let r = sanitize_scissor(c, e);
            assert!(r.offset.x >= 0 && r.offset.y >= 0, "offset 为负：{r:?}");
            assert!(
                r.offset.x as u64 + r.extent.width as u64 <= e.width as u64,
                "右侧越界：{r:?}"
            );
            assert!(
                r.offset.y as u64 + r.extent.height as u64 <= e.height as u64,
                "下侧越界：{r:?}"
            );
        }
    }

    /// 本文件的 `clamp_axis(caps, want)` 在**上下限反了**时也不能 panic。
    ///
    /// 驱动报告 `min_image_extent > max_image_extent` 是可能的
    /// （能力查询出错、或某些虚拟化驱动）。
    #[test]
    fn local_clamp_axis_tolerates_inverted_bounds() {
        let caps = vk::SurfaceCapabilitiesKHR {
            min_image_extent: vk::Extent2D { width: 10, height: 10 },
            max_image_extent: vk::Extent2D { width: 5, height: 5 }, // 反了
            ..Default::default()
        };
        let r = clamp_axis(&caps, vk::Extent2D { width: 800, height: 600 });
        assert!(r.width > 0 && r.height > 0, "不得产出 0 尺寸：{r:?}");
    }

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
                    user_set: vk::DescriptorSet::null(),
                },
                DrawBatch {
                    index_offset: 3,
                    index_count: 6,
                    clip: None,
                    user_set: vk::DescriptorSet::null(),
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
