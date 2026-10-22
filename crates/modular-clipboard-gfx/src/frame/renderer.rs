//! `FrameRenderer`：交换链 + 在飞帧槽位 + 录制/呈现。
//!
//! 本文件只放 FrameRenderer 本体（含 `FrameSlot` / `Pending` /
//! `Drop`）；类型定义、纯函数与辅助在根。

use ash::khr;
use ash::vk;
use crate::Gpu;
use crate::Swapchain;
use crate::image_barrier;
use crate::pipeline::BINDING_SAMPLER;
use crate::pipeline::BINDING_TEXTURE;
use crate::pipeline::BINDING_UNIFORM;
use crate::pipeline::BINDING_USER_SAMPLER;
use crate::pipeline::BINDING_USER_TEXTURE;
use std::time::Instant;
use ash::vk::Handle;

use super::{ACQUIRE_TIMEOUT, AcquireOutcome, AcquiredFrame, CommonBindings, DrawBatch, DrawInput, FALLBACK_EXTENT, FENCE_TIMEOUT, LayoutTracker, PipelineBundle, PresentResult, StagingArena, USER_TEXTURE_SET_CAP, can_record_upload, create_command_pool, map_acquire_result, map_present_result, present_semaphore_index, resolve_extent, sanitize_scissor};

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
    /// IMPORTANT: 多窗口必须每个窗口一个表面（`vk::SurfaceKHR` 与 `hwnd`
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
        // CAUTION: 必须用**本窗口**的 formats（上面刚查到的），不能用
        // `gpu.surface_formats` —— 那是**主窗口**的创建时快照。
        //
        // 两者在多数机器上相同，所以平时不发作；但两块显示器色彩能力
        // 不同、或某表面只报单一格式时，这里会误报「渲染通道格式 X 与
        // 交换链格式 Y 不一致」而**直接失败**（子窗口开不出来），
        // 反过来也可能放过真正的不一致。
        let pipeline_format = crate::pick_format(&formats)?.0;
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
        // GPU 尚未读完的 staging 内存。再多留 [`staging::EXTRA_CHUNKS`]
        // 块：块被占用后要等一整轮在飞帧才退休，磨砂捕获这类「每帧都要
        // 上传一点」的额外上传者需要这点余量（详见该常量的注释）。
        this.staging = StagingArena::new(gpu, crate::staging::chunk_count_for(this.slots.len()))?;

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

    /// 结束当前渲染通道。
    ///
    /// # 为什么需要它
    ///
    /// [`Self::record`] / [`Self::record_to`] 只负责**开启**通道——
    /// 交换链那一趟由 [`Self::present`] 关闭（那里还要先关通道再做
    /// 呈现布局转换）。但**离屏** pass 不走 `present`：模糊链在
    /// 交换链 pass 之前跑三趟离屏 pass，每趟都必须自成一对
    /// begin/end，否则下一趟 `cmd_begin_render_pass` 会因
    /// 「已有通道处于激活状态」而失败（验证层报
    /// `VUID-vkCmdBeginRenderPass-renderPass-06055` 一类的错）。
    ///
    /// 因此离屏绘制必须 `record_to(...)` 之后显式调用本方法。
    /// 交换链那一趟**不要**调用它——`present` 会做，重复调用会让
    /// 后面的布局转换屏障落在没有激活通道的状态上（同样违规）。
    pub fn end_pass(&mut self) {
        let device = &self.gpu.device;
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let cmd = self.slots[pending.slot].command_buffer;
        // SAFETY：调用方保证此刻确实有一个由 `record_to` 开启的通道。
        unsafe { device.cmd_end_render_pass(cmd) };
    }

    /// 本渲染器使用的渲染通道句柄。
    ///
    /// 离屏目标要用它创建 framebuffer：Vulkan 的 framebuffer 必须绑定
    /// 渲染通道。离屏链刻意与交换链**同格式**，因此可以直接复用这一个
    /// 通道，不需要为离屏再建一个（也就少一个「两个通道附件格式不一致」
    /// 的历史坑）。
    pub fn render_pass(&self) -> vk::RenderPass {
        self.bundle.render_pass
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
    ///
    /// 同时写 `BINDING_USER_SAMPLER` 的**占位值**（复用字体采样器）：
    /// 着色器静态声明了该绑定，任何被绑的集合都必须给它合法句柄。
    /// 帧槽集合只服务字体/形状批（`tex_id == 0`），不采样 user_tex——
    /// 真正的缩略图用各自纹理的集合（linear 采样器写在那边）。
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
        // 三个绑定共用一次 update 调用：两个 sampler 是裸 sampler，
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
            // 用户纹理采样器的占位值（见方法文档：帧槽集合不采样它）。
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_USER_SAMPLER)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
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

    /// 为一张**用户纹理**（图片缩略图）分配专属描述符集并写满四个绑定。
    ///
    /// # 与帧槽描述符集的关系
    ///
    /// 帧槽自带的集合仍然写 `BINDING_USER_TEXTURE`（占位图）——
    /// 着色器静态使用该绑定，任何被绑的集合都必须给它一个合法值。
    /// 用户纹理批在 `record_to` 里改绑本方法产出的集合，
    /// 于是「多张缩略图同屏」各自采样各自的纹理。
    ///
    /// # 调用时机约束
    ///
    /// 必须在**命令缓冲录制之外**调用（`upload_font_delta` 所在阶段），
    /// 且写入时不得有在飞 GPU 工作引用本集合——本集合是新建的，
    /// 天然满足。`font_view` 变化（字体图集扩容）后调用
    /// [`Self::rewrite_user_texture_set`] 刷新全部旧集合。
    pub fn allocate_user_texture_set(
        &self,
        common: CommonBindings,
        user_view: vk::ImageView,
        user_layout: vk::ImageLayout,
    ) -> anyhow::Result<vk::DescriptorSet> {
        let layouts = [self.bundle.descriptor_set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.descriptor_pool)
            .set_layouts(&layouts);
        let sets = unsafe { self.gpu.device.allocate_descriptor_sets(&alloc_info)? };
        let set = sets
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("用户纹理描述符集分配失败（池已满？）"))?;
        self.write_user_texture_set(set, common, user_view, user_layout)?;
        Ok(set)
    }

    /// 重写一张已分配的用户纹理描述符集（字体图集换视图 / 纹理换图时用）。
    ///
    /// # 安全前提
    ///
    /// 调用方必须保证没有在飞 GPU 工作正在读该集合（换字体视图的路径
    /// 已 `wait_idle`；换纹理图的路径同样先 `wait_idle` 再销毁旧图）。
    pub fn rewrite_user_texture_set(
        &self,
        set: vk::DescriptorSet,
        common: CommonBindings,
        user_view: vk::ImageView,
        user_layout: vk::ImageLayout,
    ) -> anyhow::Result<()> {
        self.write_user_texture_set(set, common, user_view, user_layout)
    }

    /// 释放一张用户纹理描述符集（LRU 淘汰纹理时调用）。
    ///
    /// 同样要求无在飞 GPU 工作引用它。
    pub fn free_user_texture_set(&self, set: vk::DescriptorSet) -> anyhow::Result<()> {
        anyhow::ensure!(!set.is_null(), "不能释放空的用户纹理描述符集");
        // ash 的 `free_descriptor_sets` 按值收 `DescriptorPool`（Handle 是 Copy），
        // 传引用会 E0308。
        unsafe {
            self.gpu
                .device
                .free_descriptor_sets(self.descriptor_pool, &[set])?;
        }
        Ok(())
    }

    /// 五个绑定的一次性写入（`allocate` 与 `rewrite` 的共同实现）。
    fn write_user_texture_set(
        &self,
        set: vk::DescriptorSet,
        common: CommonBindings,
        user_view: vk::ImageView,
        user_layout: vk::ImageLayout,
    ) -> anyhow::Result<()> {
        let ubo = vk::DescriptorBufferInfo {
            buffer: common.uniform_buffer,
            offset: common.uniform_offset,
            range: common.uniform_range,
        };
        let font = vk::DescriptorImageInfo {
            sampler: common.sampler,
            image_view: common.font_view,
            image_layout: common.font_layout,
        };
        // 用户纹理用**自己的 linear 采样器**：缩略图显示尺寸 ≠ 纹理
        // 尺寸，nearest 会出摩尔纹（见 `CommonBindings::user_sampler`）。
        let user = vk::DescriptorImageInfo {
            sampler: common.user_sampler,
            image_view: user_view,
            image_layout: user_layout,
        };
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_UNIFORM)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(std::slice::from_ref(&ubo)),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_SAMPLER)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(std::slice::from_ref(&font)),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_TEXTURE)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&font)),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_USER_TEXTURE)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(std::slice::from_ref(&user)),
            // 用户纹理的独立采样器（linear）。
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(BINDING_USER_SAMPLER)
                .descriptor_count(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(std::slice::from_ref(&user)),
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
    /// `batches` 为空时只清屏、不绑定任何描述符集，因此是安全的。
    pub fn record(&mut self, input: &DrawInput<'_>) -> anyhow::Result<()> {
        let Some(pending) = self.pending.as_ref() else {
            anyhow::bail!("record 必须在 acquire 之后调用");
        };
        let framebuffer = pending.frame.framebuffer;
        let extent = self.swapchain.extent;
        self.record_to(framebuffer, extent, input)
    }

    /// 录制绘制命令到**指定 framebuffer**（离屏目标或交换链）。
    ///
    /// # 为什么需要它
    ///
    /// 自研模糊要求先把界面画到**离屏场景图**、再卷积、最后合成到交换链。
    /// 早前 `record` 把目标写死为 `self.pending.frame.framebuffer`，
    /// 无法把界面画到别处。抽出本方法后，同一套「开通道 → 绑管线 →
    /// 逐批次绘制」逻辑对两种目标通用，只有 framebuffer 与尺寸不同。
    ///
    /// # 前置条件
    ///
    /// 与 [`Self::record`] 相同：必须在 `acquire` 之后调用；`batches`
    /// 非空时该帧槽位的描述符集必须已填充（否则着色器解引用垃圾值）。
    ///
    /// # 参数
    /// - `framebuffer`：目标。离屏目标的 framebuffer 必须与本渲染器的
    ///   渲染通道兼容（同附件格式）——模糊链刻意复用同一格式正是为此。
    /// - `extent`：目标尺寸，决定视口、剪裁与 `render_area`。低分辨率
    ///   离屏目标传自己的尺寸，不能传交换链尺寸。
    pub fn record_to(
        &mut self,
        framebuffer: vk::Framebuffer,
        extent: vk::Extent2D,
        input: &DrawInput<'_>,
    ) -> anyhow::Result<()> {
        let Some(pending) = self.pending.as_ref() else {
            anyhow::bail!("record_to 必须在 acquire 之后调用");
        };
        let slot = pending.slot;
        let device = &self.gpu.device;
        let cmd = self.slots[slot].command_buffer;

        // 视口与剪裁跟随**目标**尺寸；交换链重建后自动更新，
        // 不需要重建管线（管线里声明的是 dynamic state）。
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
        // IMPORTANT: 渲染通道**此刻才开启**（而非在 acquire 时）：纹理上传必须录在
        // 渲染通道作用域**之外**，Vulkan 不允许在通道内录传输命令。
        //
        // IMPORTANT: 描述符集绑定必须放在 `drawable.is_empty()` 判断**之后**。
        // `allocate_descriptor_sets` 分配出的集合内容是**未初始化**的，
        // 着色器又声明了 binding 0/1/2，绑上去GPU 就会解引用垃圾值
        // （UB → 丢设备）。「不绑定」严格优于「绑定一个假的」。
        let drawable: Vec<DrawBatch> = input
            .batches
            .iter()
            .copied()
            .filter(|b| b.index_count > 0)
            .collect();

        // 通道总是要开——即使无批次也要走一遍 clear，否则目标内容未定义。
        self.begin_render_pass(framebuffer, extent);

        // 模糊合成：先铺一张全屏底图（模糊结果），再叠加清晰图元。
        //
        // 底图必须用**独立的管线与描述符集**：它采样的是低分辨率模糊图，
        // 与 egui 图元管线（采样字体图集/缩略图）不是同一套布局。
        //
        // push constant：底图绘制**每次都要推** `pre.uv_offset`。
        // 着色器 `fs_copy` 声明了这块常量（自研磨砂移动预测的采样
        // 位移），不推 = 未定义内容 = 底图随机平移。零位移路径
        // （覆盖层模糊 / 超采样降采样）推的也是零，行为与加常量前一致。
        if let Some(pre) = input.prelude {
            let pre_sets = [pre.descriptor_set];
            unsafe {
                device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pre.pipeline);
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    pre.layout,
                    0,
                    &pre_sets,
                    &[],
                );
                // 全屏三角形：3 顶点、无顶点缓冲。
                device.cmd_push_constants(
                    cmd,
                    pre.layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    bytemuck::bytes_of(&pre.uv_offset),
                );
                device.cmd_draw(cmd, 3, 1, 0, 0);
            }
        }

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
            // NOTE: 逐批次 scissor：这是 UI 裁剪**唯一**生效的地方。
            //
            // 上面那次全屏 scissor 只是给「清屏后第一批」一个合理初值；
            // 每个批次开始前必须按自己的 clip 重设，否则所有
            // `ui.painter_at(rect)` 的裁剪都会被全屏范围覆盖掉。
            let rect = batch.clip.unwrap_or(scissor);

            // ---- 纵深防御：与 framebuffer 求交并钳到合法范围 ----------
            //
            // `DrawBatch.clip` 是 **pub**字段，注释只写「已是物理像素」，
            // 本模块不做任何越界/零面积检查。当前唯一生产调用方
            // （`ui/src/renderer.rs` 的 `to_draw`）确实做了
            // 「负坐标钳0 + 与视口求交 + 零面积跳过」，所以现在没出问题。
            //
            // 但 gfx 的 `examples/` 与任何**新**调用方都能构造出负 offset
            // 或超出 framebuffer 的rect。Vulkan 对越界 scissor 是
            // **未定义行为**（`VUID-vkCmdSetScissor-...`），而这类错误
            // **不报 GPU 错误**，只是「裁剪失效 / 画面错」，极难定位。
            let rect = sanitize_scissor(rect, extent);
            if rect.extent.width == 0 || rect.extent.height == 0 {
                // 完全被裁掉 ⇒ 无需绘制。
                continue;
            }

            unsafe {
                device.cmd_set_scissor(cmd, 0, std::slice::from_ref(&rect));
                // 用户纹理批：换成该纹理自己的描述符集再画。
                //
                // `vkCmdBindDescriptorSets` 是**录制命令**，在渲染通道内
                // 反复调用完全合法（egui-wgpu 对多纹理正是这么做的）。
                // 集合内容在上传阶段（录制之前）写好，帧内只做绑定，
                // 不存在「改在飞描述符」的竞态。
                if !batch.user_set.is_null() {
                    let user_sets = [batch.user_set];
                    device.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        self.bundle.pipeline_layout,
                        0,
                        &user_sets,
                        &[],
                    );
                }
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
        // IMPORTANT: 呈现信号量必须按 **image_index** 取，不是按 slot。
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
        // 同样只等本窗口（理由见 `wait_own_inflight`）：本函数是**每帧
        // 可达**的路径（纹理上传失败就会走到），用 `device_wait_idle`
        // 会让一次上传失败把主窗口的管线一起排空 ⇒ 界面整体顿一下。
        // 而它要保护的只是本窗口的命令缓冲不被重录时冲突。
        let _ = self.wait_own_inflight();
    }

    pub fn rebuild_swapchain(&mut self, desired_extent: vk::Extent2D) -> anyhow::Result<()> {
        // 必须先等**本窗口**的 GPU 工作结束：旧交换链的 framebuffer、
        // 命令缓冲与 staging 都还被在飞命令引用。
        //
        // # CAUTION: **不能**用 `gpu.wait_idle()`（2026-10-22 修复）
        //
        // 那是 `device_wait_idle`，**设备级**同步——排空所有窗口的在途
        // GPU 工作。多窗口共享一台设备（见 `ui::multiwindow`），于是
        // 任何一个窗口重建交换链都会把主窗口的管线一并排空 ⇒
        // 表现为「某个附属窗口一动，主窗口跟着卡/闪」。
        //
        // 而重建真正需要的只是「本窗口的资源不再被引用」，引用它们的
        // 也只有本窗口提交的那些命令 —— 等本窗口各槽位的 `submit_fence`
        // 就足够，且不波及其他窗口。
        self.wait_own_inflight()?;

        // IMPORTANT: 重建后**必须清掉 `pending`**。
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
        // 这里已经等完**本窗口**的全部在飞命令，清 `pending` 是安全的。
        self.pending = None;

        // IMPORTANT: 必须查**本窗口自己的**表面。
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

        // 槽位数可能变化，arena 的块数必须跟着变（块数须≥ 在飞帧数，
        // 额外留 `EXTRA_CHUNKS` 块余量）。
        // 本函数开头已等完本窗口的在途帧，因此销毁旧 staging 是安全的。
        self.staging.destroy();
        self.staging =
            StagingArena::new(self.gpu, crate::staging::chunk_count_for(self.slots.len()))?;

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

    /// 等待**本渲染器**所有在途提交完成。
    ///
    /// # 为什么要这个入口（而不是 `device_wait_idle`）
    ///
    /// `vkDeviceWaitIdle` 是设备级同步，多窗口共享设备时会把**所有**
    /// 窗口的在途工作一起排空。而本渲染器要销毁的资源（framebuffer、
    /// 命令缓冲、staging、描述符集）只可能被**自己**提交的命令引用，
    /// 等自己的 `submit_fence` 既是充分的、也是最小代价的。
    ///
    /// 只等 `in_flight` 的槽位：从未提交过的栅栏处于 unsignaled 状态，
    /// 把它们混进 `wait_all=true` 的等待会**永久阻塞**（该栅栏永远不会
    /// 被 signal）。这是本函数唯一需要小心的地方。
    fn wait_own_inflight(&mut self) -> anyhow::Result<()> {
        let fences: Vec<vk::Fence> = self
            .slots
            .iter()
            .filter(|s| s.in_flight)
            .map(|s| s.submit_fence)
            .collect();
        if fences.is_empty() {
            return Ok(());
        }
        // SAFETY: 句柄由本结构持有且尚未销毁（调用方都在
        // `destroy_frame_resources` 之前调用本函数）。
        // NOTE: `vk::Result` 不实现 `std::error::Error`，不能用
        // `anyhow::Context`，手工包一层。
        unsafe { self.gpu.device.wait_for_fences(&fences, true, FENCE_TIMEOUT) }
            .map_err(|e| anyhow::anyhow!("等待本窗口在途帧完成失败: {e:?}"))?;
        // 等待成功后清标记：GPU 已读完这些槽位的资源，后续无论是销毁
        // 重建还是继续复用，状态都是自洽的。
        for s in self.slots.iter_mut() {
            s.in_flight = false;
        }
        Ok(())
    }

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

    /// 开启渲染通道（由 [`FrameRenderer::record_to`] 调用）。
    ///
    /// `extent` 决定 `render_area`，必须与 `framebuffer` 的实际尺寸一致，
    /// 否则 Vulkan 报 `VUID-vkCmdBeginRenderPass-renderArea-...`。
    fn begin_render_pass(&mut self, framebuffer: vk::Framebuffer, extent: vk::Extent2D) {
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
            extent,
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

        // 用户纹理（缩略图）描述符集的容量上限。
        //
        // 每张同时在场的用户纹理占**一个**描述符集（不是每帧槽一个）：
        // 集合内容只在该纹理创建/换尺寸时写（写之前调用方已 wait_idle），
        // 帧内只绑定不修改，因此无需按在飞帧复制。
        //
        // 64 张：列表同屏可见的图片行通常个位数，64 已留足滚动余量；
        // 超出由调用方按 LRU 淘汰（`free_user_texture_set` 收回）。
        // 每张集合含 uniform/sampler/两张 combined image sampler。
        let user_cap = USER_TEXTURE_SET_CAP;
        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::UNIFORM_BUFFER,
                descriptor_count: count + user_cap,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::SAMPLER,
                descriptor_count: count + user_cap,
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
                descriptor_count: count * 2 + user_cap * 2,
            },
        ];
        // 用了 `UPDATE_AFTER_BIND` 的绑定，其描述符池**必须**带
        // `UPDATE_AFTER_BIND` 标志（规范强制要求，不是可选优化）。
        // 不加会报 `VUID-vkCreateDescriptorPool-pPoolCreateInfo-03111`。
        //
        // ash 0.38 陷阱：`DescriptorPoolCreateFlags` 在 `vk::bitflags` 里
        // 只暴露了 `FREE_DESCRIPTOR_SET`，`UPDATE_AFTER_BIND` 属于
        // Vulkan 1.2 段（在 `vk::feature_extensions`），照抄 0.37 示例找不到。
        //
        // `FREE_DESCRIPTOR_SET`：用户纹理按 LRU 淘汰时要收回集合，
        // 否则池很快耗尽（每张纹理一个集合，只进不出）。
        let mut pool_flags = vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET;
        if self.desc_update_after_bind {
            pool_flags |= vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND;
        }
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .flags(pool_flags)
            .max_sets(count + user_cap)
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
            // IMPORTANT: fence 必须以 **UNSIGNALED** 创建
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
        // 命令缓冲必须**显式归还池子**，不存在「随命令池一起回收」。
        //
        // 2026-10-17 修的泄漏：命令池跨 rebuild 长期存活（只在 `Drop`
        // 里销毁），于是 `allocate_frame_resources` 每轮从池里
        // `allocate_command_buffers(image_count)` 而旧缓冲从未 reset 或
        // free ⇒ 池内句柄计数单调增长（每轮泄漏 swap_image_count 个）。
        // 重建（拖动窗口十几次）时这是几十个僵尸命令缓冲。
        //
        // 用 `reset_command_pool` 而不是逐缓冲 `free_command_buffers`：
        // 这个池是帧槽专用的（全仓唯一分配点就是 `allocate_frame_resources`），
        // 整池 reset 一次到位、也不用先收集句柄。调用方保证已 `wait_idle`
        // （`rebuild_swapchain` 与 `Drop` 都先等）。
        unsafe {
            if let Err(e) =
                device.reset_command_pool(self.command_pool, vk::CommandPoolResetFlags::empty())
            {
                tracing::error!(?e, "重置帧命令池失败：旧命令缓冲可能仍被在途提交引用");
            }
        }
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
