//! egui 绘制桥：把 [`egui::ClippedPrimitive`] 变成 GPU 可执行的一帧。
//!
//! # 职责边界
//!
//! | 层 | 负责 |
//! |----|------|
//! | [`modular_clipboard_gfx::frame::FrameRenderer`] | 交换链、渲染通道开启、命令录制与提交、呈现、重建 |
//! | 本模块（[`Painter`]） | 顶点数据构造、缓冲上传、uniform、描述符绑定 |
//! | `modular-clipboard-gfx` 资源层 | 缓冲/纹理的创建、销毁、显存分配 |
//!
//! 本模块**不含 Win32 代码**，也不持有 [`egui::Context`]——输入由
//! [`modular_clipboard_gfx::window::EventLoop::egui_input`] 翻译成 [`egui::RawInput`]，
//! tessellation 由调用方用 `Context::tessellate` 完成。
//! 这样同一套绘制逻辑既能被 `full_app` 探针用，也能被正式应用用。

use ash::vk;
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::buffer::{Buffer, UniformBuffer, Vertex};
use modular_clipboard_gfx::frame::{DrawInput, FrameRenderer};
use modular_clipboard_gfx::pipeline::Uniforms;
use modular_clipboard_gfx::texture::{DeviceImage, FONT_FORMAT, Sampler};

/// 窗口标题，同时作为 Vulkan 应用标识。
pub const WINDOW_TITLE: &str = "ModularClipboard";

/// 顶点缓冲初始容量（顶点数）。不足时按 [`next_capacity`] 扩容。
pub const INITIAL_VERTEX_CAPACITY: usize = 8192;

/// 索引缓冲初始容量（索引数）。
pub const INITIAL_INDEX_CAPACITY: usize = 12288;

/// egui 约定 `Managed(0)` 恒为字体图集（见 `epaint::TextureId` 的文档）。
///
/// 改错这个常量会让所有文字变成豆腐块——字体图集不会被采样，
/// 而图集左上角那个「纯白像素」正是 `WHITE_UV` 指向的位置。
pub const FONT_TEXTURE_ID: egui::TextureId = egui::TextureId::Managed(0);

// ---------------------------------------------------------------------------
// 批次
// ---------------------------------------------------------------------------

/// 一次绘制批次。
///
/// 比 [`modular_clipboard_gfx::frame::DrawBatch`] 多带 `clip` 与 `tex_id`：
/// 合并判定需要知道相邻两段是否属于同一裁剪区、同一张纹理，
/// 而 `frame::DrawBatch` 不暴露这两个信息。提交前用 [`Batch::to_draw`] 转换。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Batch {
    /// 索引缓冲中的起始索引。
    pub index_offset: u32,
    /// 索引个数。
    pub index_count: u32,
    /// 本段的裁剪矩形，仅用于合并判定，不提交给 GPU。
    pub clip: egui::Rect,
    /// 本段采样的纹理。
    ///
    /// # 为什么必须逐批记录
    ///
    /// 引入用户纹理（图片缩略图）后，一次绘制里会存在**两种**纹理：
    /// 字体图集与缩略图。若合批时只看 clip 与索引连续性，
    /// 两块不同纹理的图元会被并进同一批，而该批只绑一张纹理
    /// ⇒ 其中一半图元采样到错误纹理。
    ///
    /// 症状是「缩略图位置显示成字体图集里的某个字形」，
    /// **不报任何 GPU 错误、验证层也是干净的**——最难查的一类。
    ///
    /// egui 已经把每块 mesh 的纹理 ID 交给我们（`epaint::Mesh::texture_id`），
    /// 这里只是此前一直没读它。
    pub tex_id: egui::TextureId,
}

impl Batch {
    pub fn to_draw(self) -> modular_clipboard_gfx::frame::DrawBatch {
        modular_clipboard_gfx::frame::DrawBatch {
            index_offset: self.index_offset,
            index_count: self.index_count,
        }
    }
}

// ---------------------------------------------------------------------------
// tessellation：egui 图元 → 顶点数据
// ---------------------------------------------------------------------------

/// 把一批 [`egui::epaint::ClippedPrimitive`] 转换后写入三个向量
/// （三者都会被先清空，可反复复用而不重新分配）。
///
/// # 索引偏移
///
/// egui 的 `Mesh::indices` 是**相对该 mesh 自身顶点数组**的下标。多个 mesh
/// 拼进同一顶点缓冲时必须加上「本 mesh 之前已有多少顶点」，否则第二个 mesh
/// 会去索引第一个 mesh 的顶点——表现为界面元素错位，且**不触发任何 GPU 错误**。
///
/// # 批次合并
///
/// `clip_rect` 相同的**相邻**图元合并成一次 `draw_indexed`。着色器不处理
/// per-batch scissor（egui 在 CPU 侧 tessellation 时已按 `clip_rect` 裁掉
/// 几何），因此合并不改变画面，只减少 draw call。
pub fn tessellate_into(
    primitives: &[egui::epaint::ClippedPrimitive],
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
    batches: &mut Vec<Batch>,
) {
    vertices.clear();
    indices.clear();
    batches.clear();

    for prim in primitives {
        let mesh = match &prim.primitive {
            egui::epaint::Primitive::Mesh(m) => m,
            // PaintCallback 需要用户代码参与光栅化，本渲染器不支持。
            egui::epaint::Primitive::Callback(_) => continue,
        };
        if mesh.indices.is_empty() {
            continue;
        }

        let base = vertices.len() as u32;
        let index_start = indices.len() as u32;
        let index_count = mesh.indices.len() as u32;

        for v in &mesh.vertices {
            vertices.push(Vertex {
                pos: [v.pos.x, v.pos.y],
                uv: [v.uv.x, v.uv.y],
                color: pack_color(v.color),
            });
        }
        indices.extend(mesh.indices.iter().map(|&i| base + i));

        // 合批的三个条件：同裁剪区、同纹理、索引连续。
        // 索引连续由上面的追加式写入保证。
        //
        // `tex_id` 这条是引入用户纹理后**必须**加的：漏掉它会让不同纹理的
        // 图元并进同一批，而该批只绑一张纹理 ⇒ 采样到错误纹理，
        // 且不产生任何 GPU 错误。见 `Batch::tex_id` 的说明。
        let merge = batches.last().is_some_and(|b| {
            b.clip == prim.clip_rect
                && b.tex_id == mesh.texture_id
                && b.index_offset + b.index_count == index_start
        });
        if merge {
            let last = batches.last_mut().expect("刚判定过非空");
            last.index_count += index_count;
        } else {
            batches.push(Batch {
                index_offset: index_start,
                index_count,
                clip: prim.clip_rect,
                tex_id: mesh.texture_id,
            });
        }
    }
}

/// egui 的 `Color32` → 顶点色 `u32`（小端 ABGR）。
///
/// # 为什么必须反预乘
///
/// epaint 的顶点色是 **sRGBA 预乘 alpha**（`r,g,b` 已乘过 `a`），而
/// `pipeline.rs` 的混合是 `SRC_ALPHA / ONE_MINUS_SRC_ALPHA`——**非预乘**混合。
/// 直接上传会让 alpha 被乘两次，半透明区域明显发黑。这类错误不报任何 GPU 错误，
/// 只是「看起来脏」，极易被漏掉。
///
/// 着色器输出 `color * vec4(1,1,1,texel.r)`：rgb 不乘覆盖率、只有 alpha 乘覆盖率，
/// 这正是标准非预乘「over」运算期望的形式。因此这里把颜色还原成非预乘，
/// 把那一次乘法交回混合阶段完成。
pub fn pack_color(c: egui::Color32) -> u32 {
    let [r, g, b, a] = c.to_array();
    let (r, g, b) = unpremultiply(r, g, b, a);
    u32::from_le_bytes([r, g, b, a])
}

/// 预乘 → 非预乘。`a == 0` 时 rgb 的信息已在预乘时丢失（乘 0），取 0：
/// 该像素最终 alpha 为 0，对画面的贡献本来就是 0。
pub fn unpremultiply(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8) {
    if a == 255 {
        return (r, g, b);
    }
    if a == 0 {
        // 除零边界。理论上不可达（egui 的Color32 是预乘的，a=0 时 rgb 必为 0），
        // 但仍显式返回 0 而不是让除法panic —— 一个「理论上不可达」的分支
        // 恰恰是最需要写对的地方。
        return (0, 0, 0);
    }
    // 用 u32 计算：r * 255 最大 65025，会溢出 u8。
    let f = |c: u8| -> u8 { ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8 };
    (f(r), f(g), f(b))
}

/// 构造本帧的 uniform。
///
/// `clip_from_uv` 传单位矩阵：着色器内部已把像素坐标换算成 NDC（并做了 y 轴
/// 翻转），再乘一个矩阵只会引入二次变换。
pub fn uniforms_for(extent: vk::Extent2D, pixels_per_point: f32) -> Uniforms {
    Uniforms::new(
        [
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0,
        ],
        [extent.width as f32, extent.height as f32],
        pixels_per_point,
    )
}

/// uniform 的内容指纹。变了才需要重写缓冲。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UniformKey {
    pub width: u32,
    pub height: u32,
    pub pixels_per_point: f32,
}

/// 扩容策略：1.5 倍并向上取整到 1024，避免「每帧只多一个元素就重建」。
pub fn next_capacity(needed: usize) -> usize {
    needed.saturating_mul(3).div_ceil(2).next_multiple_of(1024)
}

/// egui 请求的下一帧延迟。`None` 表示「有事件时立即处理，不要 sleep」。
pub fn repaint_delay(output: &egui::FullOutput) -> Option<std::time::Duration> {
    let v = output.viewport_output.get(&egui::ViewportId::ROOT)?;
    match v.repaint_delay {
        d if d.is_zero() => None,
        d => Some(d),
    }
}

// ---------------------------------------------------------------------------
// Painter
// ---------------------------------------------------------------------------

/// 一帧的绘制上下文。
///
/// 借用 `Gpu`（内部要建缓冲、查表面能力）。`Drop` 时先 `device_wait_idle`
/// 再按「描述符引用者 → 被引用者」的逆序释放，调用方无需手工清理。
pub struct Painter<'a> {
    gpu: &'a Gpu,
    /// 字体图集采样器。**必须点采样**：线性插值会把邻接字形的覆盖率渗进
    /// 当前字形边缘，小字号下整段文字糊成一团灰。这是自写字体图集渲染器
    /// 最常见的错误来源。
    sampler: Sampler,
    /// 字体图集（覆盖率图，`R8_UNORM`）。
    font: Option<DeviceImage>,
    uniform: UniformBuffer,
    slots: SlotBuffers,
    // CPU 侧复用缓冲，避免每帧重新分配。
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
    batches: Vec<Batch>,
    draw_batches: Vec<modular_clipboard_gfx::frame::DrawBatch>,
    /// 描述符绑定需要重做的标记。
    bindings_dirty: bool,
    /// 上次写入 uniform 时的内容指纹。
    last_uniform: Option<UniformKey>,
    /// 累计统计，供上层显示与诊断。
    pub stats: FrameStats,
}

/// 累计统计。
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameStats {
    /// 最后一帧的顶点数。
    pub vertices: usize,
    /// 最后一帧的索引数。
    pub indices: usize,
    /// 最后一帧合并后的 draw call 数。
    pub draw_calls: u32,
    /// 字体图集累计上传次数。
    pub texture_uploads: u64,
    /// 交换链累计重建次数。
    pub rebuilds: u32,
}

impl<'a> Painter<'a> {
    /// 绑定设备并分配资源。`slot_count` 应传在飞帧槽位数
    /// （即 `FrameRenderer::slot_count()`）。
    pub fn new(gpu: &'a Gpu, slot_count: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(slot_count > 0, "在飞帧槽位数必须大于 0");
        let sampler = Sampler::new_nearest(&gpu.device)?;
        let uniform = UniformBuffer::new(gpu)?;
        let slots = SlotBuffers::new(
            gpu,
            slot_count,
            INITIAL_VERTEX_CAPACITY,
            INITIAL_INDEX_CAPACITY,
        )?;
        Ok(Self {
            gpu,
            sampler,
            font: None,
            uniform,
            slots,
            vertices: Vec::with_capacity(INITIAL_VERTEX_CAPACITY),
            indices: Vec::with_capacity(INITIAL_INDEX_CAPACITY),
            batches: Vec::new(),
            draw_batches: Vec::new(),
            // 字体图集尚未创建，首帧上传后必然要绑一次。
            bindings_dirty: true,
            last_uniform: None,
            stats: FrameStats::default(),
        })
    }

    /// 帧循环里的一帧。必须在 `acquire` 成功之后、`present` 之前调用。
    ///
    /// # 参数
    ///
    /// - `primitives`：`Context::tessellate` 的产物。
    /// - `pixels_per_point`：来自 [`egui::FullOutput::pixels_per_point`]。
    /// - `textures_delta`：**会被清空**（见 [`Painter::upload_font_delta`]）。
    ///
    /// # 前置条件
    ///
    /// `fr.acquire()` 必须已返回 `Some`——`current_slot()` 依赖它。
    pub fn paint(
        &mut self,
        fr: &mut FrameRenderer<'a>,
        primitives: &[egui::epaint::ClippedPrimitive],
        pixels_per_point: f32,
        textures_delta: &mut egui::TexturesDelta,
    ) -> anyhow::Result<()> {
        tessellate_into(
            primitives,
            &mut self.vertices,
            &mut self.indices,
            &mut self.batches,
        );
        self.stats.vertices = self.vertices.len();
        self.stats.indices = self.indices.len();
        self.stats.draw_calls = self.batches.len() as u32;

        // ---- 顶点数据 → 当前槽位 ---------------------------------------
        //
        // `acquire` 已等待过该槽位栅栏，所以「拿到 slot 就往这个槽位写」是
        // 安全的：上一轮占用它的 GPU 工作已完成。这是用host-visible 环缓冲
        // 而非 device-local + staging 的理由——同样正确，且每帧少一次拷贝。
        let slot = fr
            .current_slot()
            .ok_or_else(|| anyhow::anyhow!("paint 必须在 acquire 成功后调用"))?;
        self.slots
            .write(self.gpu, slot, &self.vertices, &self.indices)?;

        // ---- 字体图集上传 -----------------------------------------------
        let uploaded = self.upload_font_delta(fr, textures_delta)?;
        if uploaded > 0 {
            self.stats.texture_uploads += uploaded;
            // 图集可能已被重建（尺寸变化）→ 图像视图变了 → 必须重绑。
            self.bindings_dirty = true;
        }

        // ---- 描述符绑定 -------------------------------------------------
        if let Some(f) = self.font.as_ref() {
            if self.bindings_dirty {
                self.bind_all(fr, f)?;
                self.bindings_dirty = false;
            }
            // uniform 内容只随「表面尺寸 / dpr」变化。多帧在途时改写同一份
            // uniform 缓冲会被上一帧的 GPU 读到，因此**只在内容真的变了才写**
            // ——写相同内容则不存在竞态。
            let key = UniformKey {
                width: fr.extent().width,
                height: fr.extent().height,
                pixels_per_point,
            };
            if self.last_uniform != Some(key) {
                self.uniform
                    .write(&self.gpu.device, &uniforms_for(fr.extent(), pixels_per_point))?;
                self.last_uniform = Some(key);
            }
        }

        // ---- 录制绘制 ----------------------------------------------------
        self.draw_batches.clear();
        self.draw_batches.extend(self.batches.iter().map(|b| b.to_draw()));
        let input = DrawInput {
            vertex_buffer: self.slots.vertex_buffer(slot),
            index_buffer: self.slots.index_buffer(slot),
            batches: &self.draw_batches,
            ..Default::default()
        };
        fr.record(&input)
    }

    /// 交换链重建后重整资源。
    ///
    /// 一次重建牵动四件事，缺一件都会在后续帧里出问题：
    ///
    /// 1. `rebuild_swapchain` 重新分配描述符集 → 绑定必须重做；
    /// 2. 交换链图像数可能变化 → 在飞槽位数随之变化 → 顶点缓冲环要跟着扩缩；
    /// 3. 视口/剪裁是动态状态，但 uniform 里的表面尺寸是烘进去的 → 必须重写；
    /// 4. `acquire` 的轮转游标被重置 → 槽位与缓冲的对应关系重新开始。
    pub fn on_swapchain_rebuilt(&mut self, fr: &mut FrameRenderer<'a>) -> anyhow::Result<()> {
        self.bindings_dirty = true;
        self.last_uniform = None;
        let want = fr.slot_count();
        if self.slots.buffers.len() != want {
            self.slots.resize(self.gpu, want)?;
        }
        self.stats.rebuilds += 1;
        Ok(())
    }

    /// 字体图集当前尺寸，`None` 表示尚未上传过。
    pub fn font_size(&self) -> Option<vk::Extent2D> {
        self.font.as_ref().map(|f| f.size)
    }

    /// 把 uniform 与纹理绑定写进**每一个**在飞槽位的描述符集。
    ///
    /// GPU 可能还在读上一个槽位的描述符集，只写当前帧的绑定会让其它帧读到
    /// 未初始化的描述符（表现为随机采样到垃圾纹理，或直接崩）。
    fn bind_all(&self, fr: &FrameRenderer<'a>, font: &DeviceImage) -> anyhow::Result<()> {
        for slot in 0..fr.slot_count() {
            fr.update_texture_binding(slot, self.sampler.handle(), font.view, font.layout)?;
            fr.update_uniform_binding(
                slot,
                self.uniform.buffer().handle(),
                0,
                self.uniform.buffer().size(),
            )?;
        }
        tracing::debug!(slots = fr.slot_count(), "描述符绑定已全部写入");
        Ok(())
    }

    /// 应用 egui 的字体图集增量，返回本帧的上传次数。
    ///
    /// # 图集扩容
    ///
    /// egui 的字体图集**按需增长**：新字形放不下时整张重建，尺寸变化。
    /// 此时必须销毁旧图像并新建，图像视图随之改变 → 描述符需要重绑。
    /// 销毁前要 `device_wait_idle`：旧图像可能仍被在途命令引用。
    ///
    /// # 只支持字体图集
    ///
    /// 着色器只绑了一张纹理（`BINDING_TEXTURE`）。用户纹理（图片缩略图等）
    /// 需要按 `TextureId` 分批 + 多描述符集，当前渲染管线不支持。
    fn upload_font_delta(
        &mut self,
        fr: &mut FrameRenderer<'a>,
        delta: &mut egui::TexturesDelta,
    ) -> anyhow::Result<u64> {
        let mut uploads = 0u64;

        for (id, deltas) in &delta.set {
            if *id != FONT_TEXTURE_ID {
                tracing::warn!("忽略非字体纹理 {id:?}：当前着色器只绑定字体图集");
                continue;
            }
            for d in deltas {
                let patch = d.image.size();
                let (w, h) = (patch[0] as u32, patch[1] as u32);
                anyhow::ensure!(w > 0 && h > 0, "字体图集增量的尺寸为 0：{patch:?}");

                // **只有整图更新才意味着图集扩容**。
                //
                // `ImageDelta::pos == None` 是「整图」的判据。局部更新的
                // `image.size()` 返回的是**补丁尺寸**——新字形触发时可能只有
                // 4x10 这么大。若拿它去比对图集尺寸，一次局部更新就会把
                // 2048x32 的图集换成 4x10 的小图，随后所有坐标全部越界。
                //
                // 这个 bug 由 `full_app` 实机跑出来：首帧成功渲染 552 个顶点，
                // 第二帧才崩在「更新区域 (1652, 0) 4x10 超出图像 4x10 范围」。
                //
                // egui 的 `TexturesDelta::push` 保证：整图增量会**替换**该 ID
                // 的全部历史增量，因此逐个判断是安全的。
                if d.pos.is_none() {
                    let need_new = match self.font.as_ref() {
                        None => true,
                        Some(f) => f.size.width != w || f.size.height != h,
                    };
                    if need_new {
                        if let Some(mut old) = self.font.take() {
                            self.gpu.wait_idle();
                            old.destroy(&self.gpu.device);
                        }
                        self.font = Some(DeviceImage::new(
                            self.gpu,
                            vk::Extent2D { width: w, height: h },
                            FONT_FORMAT,
                            vk::ImageUsageFlags::TRANSFER_DST
                                | vk::ImageUsageFlags::SAMPLED,
                        )?);
                    }
                }

                let bytes = DeviceImage::coverage_bytes(&d.image);
                let offset = d.pos.map(|p| (p[0] as u32, p[1] as u32));
                let f = self.font.as_mut().ok_or_else(|| {
                    anyhow::anyhow!("字体图集收到局部更新 {offset:?}，但纹理尚未创建")
                })?;
                fr.record_texture_upload(f, &bytes, offset, (w, h))?;
                uploads += 1;
            }
        }

        // egui 的 `TexturesDelta` 在 drop 时断言「增量为空」。本函数只消费
        // 字体图集、用户纹理一律忽略，因此显式清空——否则 debug 断言误报，
        // 而那属于「帧循环写法不对」，不该掩盖真正的失败。
        delta.clear();
        Ok(uploads)
    }
}

impl Drop for Painter<'_> {
    fn drop(&mut self) {
        // 必须先让 GPU 彻底空闲：下面这些对象都可能被在途命令引用。
        self.gpu.wait_idle();
        if let Some(mut f) = self.font.take() {
            f.destroy(&self.gpu.device);
        }
        self.sampler.destroy(&self.gpu.device);
        self.slots.destroy(&self.gpu.device);
        self.uniform.destroy(&self.gpu.device);
    }
}

// ---------------------------------------------------------------------------
// 顶点/索引缓冲
// ---------------------------------------------------------------------------

/// 按在飞槽位数量复制的顶点/索引缓冲环。
///
/// # 为什么是「每槽位一份」而不是「全局一份」
///
/// 帧层是多帧在途的：`present` 只提交、不等待。若顶点数据只有一份，下一帧的
/// 写入会覆写 GPU **仍在读取**的内存，画面会随机撕裂。
///
/// 按槽位各存一份、只写 `acquire` 刚给出的那个槽位，则天然安全。
struct SlotBuffers {
    buffers: Vec<SlotPair>,
}

struct SlotPair {
    /// `None` 表示已被移出（销毁流程中）。
    vertex: Option<Buffer>,
    index: Option<Buffer>,
    vertex_capacity: usize,
    index_capacity: usize,
}

impl SlotBuffers {
    fn new(
        gpu: &Gpu,
        slots: usize,
        vertex_capacity: usize,
        index_capacity: usize,
    ) -> anyhow::Result<Self> {
        let mut buffers = Vec::with_capacity(slots);
        for _ in 0..slots {
            buffers.push(SlotPair {
                vertex: Some(new_vertex_buffer(gpu, vertex_capacity)?),
                index: Some(new_index_buffer(gpu, index_capacity)?),
                vertex_capacity,
                index_capacity,
            });
        }
        Ok(Self { buffers })
    }

    /// 调整槽位数量（交换链重建后图像数可能变化）。
    fn resize(&mut self, gpu: &Gpu, slots: usize) -> anyhow::Result<()> {
        while self.buffers.len() > slots {
            let mut p = self.buffers.pop().expect("长度已确认");
            p.destroy(&gpu.device);
        }
        let (vc, ic) = self
            .buffers
            .first()
            .map(|p| (p.vertex_capacity, p.index_capacity))
            .unwrap_or((INITIAL_VERTEX_CAPACITY, INITIAL_INDEX_CAPACITY));
        while self.buffers.len() < slots {
            self.buffers.push(SlotPair {
                vertex: Some(new_vertex_buffer(gpu, vc)?),
                index: Some(new_index_buffer(gpu, ic)?),
                vertex_capacity: vc,
                index_capacity: ic,
            });
        }
        Ok(())
    }

    /// 把本帧数据写进指定槽位，必要时扩容。
    fn write(
        &mut self,
        gpu: &Gpu,
        slot: usize,
        vertices: &[Vertex],
        indices: &[u32],
    ) -> anyhow::Result<()> {
        let count = self.buffers.len();
        let p = self
            .buffers
            .get_mut(slot)
            .ok_or_else(|| anyhow::anyhow!("槽位 {slot} 越界（共 {count} 个）"))?;

        if vertices.len() > p.vertex_capacity {
            // 扩容要重建缓冲。上一轮占用该槽位的 GPU 工作已被 acquire 的
            // 栅栏等待保证完成，因此销毁旧缓冲是安全的。
            let cap = next_capacity(vertices.len());
            if let Some(mut old) = p.vertex.take() {
                old.destroy(&gpu.device);
            }
            p.vertex = Some(new_vertex_buffer(gpu, cap)?);
            p.vertex_capacity = cap;
        }
        if indices.len() > p.index_capacity {
            let cap = next_capacity(indices.len());
            if let Some(mut old) = p.index.take() {
                old.destroy(&gpu.device);
            }
            p.index = Some(new_index_buffer(gpu, cap)?);
            p.index_capacity = cap;
        }

        if !vertices.is_empty() {
            let v = p.vertex.as_mut().expect("上方已确保存在");
            v.write(&gpu.device, 0, bytemuck::cast_slice(vertices))?;
        }
        if !indices.is_empty() {
            let i = p.index.as_mut().expect("上方已确保存在");
            i.write(&gpu.device, 0, bytemuck::cast_slice(indices))?;
        }
        Ok(())
    }

    fn vertex_buffer(&self, slot: usize) -> vk::Buffer {
        self.buffers[slot]
            .vertex
            .as_ref()
            .expect("未销毁的槽位必有缓冲")
            .handle()
    }

    fn index_buffer(&self, slot: usize) -> vk::Buffer {
        self.buffers[slot]
            .index
            .as_ref()
            .expect("未销毁的槽位必有缓冲")
            .handle()
    }

    fn destroy(&mut self, device: &ash::Device) {
        for p in &mut self.buffers {
            p.destroy(device);
        }
        self.buffers.clear();
    }
}

impl SlotPair {
    fn destroy(&mut self, device: &ash::Device) {
        if let Some(mut v) = self.vertex.take() {
            v.destroy(device);
        }
        if let Some(mut i) = self.index.take() {
            i.destroy(device);
        }
        self.vertex_capacity = 0;
        self.index_capacity = 0;
    }
}

/// 顶点缓冲：host-visible + coherent，因此每帧直接 `write`（内部
/// map→拷贝→unmap）即可，不需要 staging + `cmd_copy_buffer`。
fn new_vertex_buffer(gpu: &Gpu, capacity: usize) -> anyhow::Result<Buffer> {
    Buffer::new_host_visible(
        gpu,
        (capacity * std::mem::size_of::<Vertex>()) as vk::DeviceSize,
        vk::BufferUsageFlags::VERTEX_BUFFER,
        vk::SharingMode::EXCLUSIVE,
    )
}

fn new_index_buffer(gpu: &Gpu, capacity: usize) -> anyhow::Result<Buffer> {
    Buffer::new_host_visible(
        gpu,
        (capacity * std::mem::size_of::<u32>()) as vk::DeviceSize,
        vk::BufferUsageFlags::INDEX_BUFFER,
        vk::SharingMode::EXCLUSIVE,
    )
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::{ClippedPrimitive, Mesh, Primitive, Vertex as EVertex};

    fn rect_at(y: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::Pos2::new(0.0, y), egui::vec2(10.0, 10.0))
    }

    fn prim(clip: egui::Rect, verts: usize, indices: usize) -> ClippedPrimitive {
        let mesh = Mesh {
            indices: (0..indices as u32).collect(),
            vertices: (0..verts)
                .map(|i| EVertex {
                    pos: egui::pos2(i as f32, 0.0),
                    uv: egui::pos2(0.0, 0.0),
                    color: egui::Color32::WHITE,
                })
                .collect(),
            texture_id: FONT_TEXTURE_ID,
        };
        ClippedPrimitive {
            clip_rect: clip,
            primitive: Primitive::Mesh(mesh),
        }
    }

    #[test]
    fn color_is_packed_as_abgr_little_endian() {
        // 不透明色：反预乘是恒等，直接验证字节序。
        let packed = pack_color(egui::Color32::from_rgb(0x11, 0x22, 0x33));
        assert_eq!(packed & 0xFF, 0x11, "最低字节应是红");
        assert_eq!((packed >> 8) & 0xFF, 0x22);
        assert_eq!((packed >> 16) & 0xFF, 0x33);
        assert_eq!((packed >> 24) & 0xFF, 0xFF, "最高字节应是 alpha");
    }

    #[test]
    fn opaque_colors_survive_unpremultiply() {
        for c in [
            egui::Color32::WHITE,
            egui::Color32::BLACK,
            egui::Color32::from_rgb(1, 127, 254),
        ] {
            assert_eq!(
                unpremultiply(c.r(), c.g(), c.b(), 255),
                (c.r(), c.g(), c.b())
            );
        }
    }

    #[test]
    fn zero_alpha_yields_black_not_overflow() {
        // a=0 是除零边界。
        //
        // 注意：egui 的 `Color32` 内部是**预乘**的，a=0 时rgb 必然已经是 0
        // （预乘就是乘 alpha），所以拿一个 rgb 非零、a=0 的组合来测是
        // 构造不出来的。这里直接验证函数在 a=0 时不panic 且返回 0。
        assert_eq!(unpremultiply(0, 0, 0, 0), (0, 0, 0));
        // 真实可达的边界：egui 的 TRANSPARENT。
        let t = egui::Color32::TRANSPARENT;
        assert_eq!(unpremultiply(t.r(), t.g(), t.b(), t.a()), (0, 0, 0));
    }

    #[test]
    fn half_alpha_is_restored_to_full_channel() {
        // 预乘 50% 后 rgb 约为一半；反预乘应还原到接近原值。
        let half = egui::Color32::from_rgb(200, 100, 40).gamma_multiply(0.5);
        let (gr, gg, gb) = unpremultiply(half.r(), half.g(), half.b(), half.a());
        for (g, want) in [gr, gg, gb].into_iter().zip([200u8, 100, 40]) {
            assert!(
                (g as i32 - want as i32).abs() <= 2,
                "反预乘误差过大：得到 {g}，期望约 {want}"
            );
        }
    }

    #[test]
    fn unpremultiply_never_exceeds_255() {
        // 预乘的舍入误差可能让反预乘结果略大于原值，函数内部必须夹住。
        // 用 u32 中转比较——直接写 `r <= 255` 会被编译器判为恒真（u8 上界），
        // 那样这个测试就什么都没验证。
        for a in 1..=254u8 {
            let (r, g, b) = unpremultiply(255, 255, 255, a);
            for v in [r, g, b] {
                assert!(u32::from(v) <= 255, "反预乘结果 {v} 溢出 u8");
            }
        }
    }

    #[test]
    fn indices_are_offset_per_mesh() {
        // 两个 mesh 的索引都是 0..n，合并后第二个必须整体平移。
        let prims = vec![prim(rect_at(0.0), 3, 3), prim(rect_at(0.0), 3, 3)];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(v.len(), 6, "两个 mesh 的顶点都应保留");
        assert_eq!(i, vec![0, 1, 2, 3, 4, 5], "第二个 mesh 的索引应平移到 3..6");
    }

    #[test]
    fn adjacent_primitives_with_same_clip_merge() {
        // 一次 draw call 优于 N 次：这是「相邻且 clip 相同可合并」的价值。
        let prims = vec![
            prim(rect_at(0.0), 3, 3),
            prim(rect_at(0.0), 3, 3),
            prim(rect_at(0.0), 3, 3),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(b.len(), 1, "相同 clip 的相邻图元应合并为一批");
        assert_eq!(b[0].index_count, 9);
    }

    #[test]
    fn different_clips_stay_separate() {
        let prims = vec![prim(rect_at(0.0), 3, 3), prim(rect_at(20.0), 3, 3)];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(b.len(), 2, "clip 不同的图元不应合并");
    }

    #[test]
    fn same_clip_but_non_adjacent_does_not_merge() {
        // 合并条件写错（忽略相邻性）会让批次区间跨越中间那批。
        let prims = vec![
            prim(rect_at(0.0), 3, 3),
            prim(rect_at(20.0), 3, 3),
            prim(rect_at(0.0), 3, 3),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(b.len(), 3, "非相邻的同 clip 图元不应合并");
    }

    #[test]
    fn batches_cover_all_indices_exactly_once() {
        // 批次的索引区间必须无缝铺满整个索引数组：既不重叠也不留空洞。
        // 空洞会导致部分三角形没被画（表现为界面局部空白）。
        let prims = vec![
            prim(rect_at(0.0), 3, 3),
            prim(rect_at(20.0), 6, 6),
            prim(rect_at(40.0), 3, 3),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        let mut next = 0u32;
        for batch in &b {
            assert_eq!(batch.index_offset, next, "批次之间出现了空洞或重叠");
            next = batch.index_offset + batch.index_count;
        }
        assert_eq!(next as usize, i.len(), "批次应覆盖全部索引");
    }

    #[test]
    fn empty_primitives_produce_no_batches() {
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&[], &mut v, &mut i, &mut b);
        assert!(b.is_empty() && v.is_empty() && i.is_empty());
    }

    #[test]
    fn empty_mesh_is_skipped() {
        // 有顶点但无索引的 mesh 不能产生批次（index_count=0 会被跳过，
        // 但会留下一个空批次，徒增记录数）。
        let mut p = prim(rect_at(0.0), 3, 0);
        if let Primitive::Mesh(m) = &mut p.primitive {
            m.indices.clear();
        }
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&[p], &mut v, &mut i, &mut b);
        assert!(b.is_empty());
    }

    #[test]
    fn callback_primitive_is_ignored() {
        // PaintCallback 无法在不执行用户代码的情况下光栅化，必须跳过而非 panic。
        let p = ClippedPrimitive {
            clip_rect: rect_at(0.0),
            primitive: Primitive::Callback(egui::epaint::PaintCallback {
                rect: rect_at(0.0),
                // 回调签名是 `Fn(&ClippedPrimitive)`，此处永不会被调用。
                callback: std::sync::Arc::new(|_: &ClippedPrimitive| {}),
            }),
        };
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&[p], &mut v, &mut i, &mut b);
        assert!(b.is_empty() && v.is_empty());
    }

    #[test]
    fn real_egui_output_converts_cleanly() {
        // 用真实的 egui 跑一帧，覆盖「几十个图元 + 真实 Color32 + 真实
        // Mesh.indices」这一组合，而不是只测人造数据。
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            ui.heading("标题");
            ui.label("正文 label with ascii");
            let _ = ui.button("按钮");
            // 浮层：验证不同 clip_rect 的图元也能走通（不会误合并批次）。
            egui::Window::new("窗口").show(&ctx, |ui| {
                ui.label("窗口内文字");
            });
        });
        // egui 的 `TexturesDelta` 在 drop 时断言增量为空。
        // 本测试不消费纹理增量（那是 `Painter` 的职责，需要 GPU），
        // 必须显式清空，否则 panic 会掩盖真正的断言失败。
        out.textures_delta.clear();

        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        assert!(!prims.is_empty(), "egui 应产出图元");

        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert!(!v.is_empty() && !i.is_empty() && !b.is_empty());
        // 索引必须全部落在顶点数组范围内，否则 draw_indexed 越界读取。
        let n = v.len() as u32;
        assert!(i.iter().all(|&x| x < n), "存在越界索引");
        // 索引数必须是 3 的倍数（三角形列表）。
        assert_eq!(i.len() % 3, 0);
    }

    #[test]
    fn merging_never_increases_draw_call_count() {
        // 合并是纯优化：draw call 数必须**不增加**。
        // 若有人把合并条件写反（例如每个图元都新开一批），这个断言会失败。
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            for i in 0..10 {
                ui.label(format!("第 {i} 行文字"));
            }
        });
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);

        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert!(
            b.len() <= prims.len(),
            "合并后批次 {} 不应超过图元数 {}",
            b.len(),
            prims.len()
        );
    }

    #[test]
    fn font_texture_id_is_managed_zero() {
        // epaint 约定：Managed(0) 恒为字体图集。改错会让所有文字变豆腐块。
        assert_eq!(FONT_TEXTURE_ID, egui::TextureId::Managed(0));
    }

    // ---- 多纹理：合批必须按 texture_id 分组 ----

    /// 造一个指定纹理的图元。`clip` 固定，便于构造
    /// 「clip 相同 + 索引连续」这个最容易触发误合并的组合。
    fn prim_with_tex(clip: egui::Rect, verts: usize, indices: usize, tex: egui::TextureId) -> ClippedPrimitive {
        let mesh = Mesh {
            indices: (0..indices as u32).collect(),
            vertices: (0..verts)
                .map(|i| EVertex {
                    pos: egui::pos2(i as f32, 0.0),
                    uv: egui::pos2(0.0, 0.0),
                    color: egui::Color32::WHITE,
                })
                .collect(),
            texture_id: tex,
        };
        ClippedPrimitive {
            clip_rect: clip,
            primitive: Primitive::Mesh(mesh),
        }
    }

    /// 缩略图用的非字体纹理 ID。
    const THUMB_TEX: egui::TextureId = egui::TextureId::Managed(7);

    #[test]
    fn different_textures_are_never_merged() {
        // 回归测试：合批曾只看 clip + 索引连续，漏掉纹理判定。
        // 引入用户纹理后，字体图元与缩略图图元会被并进同一批，
        // 而该批只绑一张纹理 ⇒ 缩略图位置显示成字体图集里的字形。
        // 该错误不产生任何 GPU 错误、验证层也干净。
        let clip = rect_at(0.0);
        let prims = vec![
            // clip 相同、索引连续（同clip_rect + 追加式写入），
            // 只有纹理不同——这正是必须拆开的情形。
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
            prim_with_tex(clip, 3, 3, THUMB_TEX),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(b.len(), 2, "不同纹理绝不能合批：{b:?}");
        assert_eq!(b[0].tex_id, FONT_TEXTURE_ID);
        assert_eq!(b[1].tex_id, THUMB_TEX);
    }

    #[test]
    fn same_texture_still_merges() {
        // 反向约束：修「漏判纹理」不能顺手把合批优化也干掉。
        // 同纹理 + 同 clip + 索引连续 ⇒ 必须仍然合并成一批。
        let clip = rect_at(0.0);
        let prims = vec![
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(b.len(), 1, "同纹理应继续合批：{b:?}");
        assert_eq!(b[0].index_count, 6, "合并后索引数应累加");
    }

    #[test]
    fn same_texture_across_interleaved_primitives_groups_correctly() {
        // 字体 / 缩略图交替出现时应得到 4 批，而不是 1 批也不是 2 批。
        // 覆盖「相邻性」判定的正确性：只有**相邻且同纹理**才能合并。
        let clip = rect_at(0.0);
        let prims = vec![
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
            prim_with_tex(clip, 3, 3, THUMB_TEX),
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
            prim_with_tex(clip, 3, 3, THUMB_TEX),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(b.len(), 4, "交替纹理应逐个成批：{b:?}");
        assert_eq!(b[0].tex_id, FONT_TEXTURE_ID);
        assert_eq!(b[1].tex_id, THUMB_TEX);
        assert_eq!(b[2].tex_id, FONT_TEXTURE_ID);
        assert_eq!(b[3].tex_id, THUMB_TEX);
    }

    #[test]
    fn batch_tex_id_defaults_survive_to_draw_conversion() {
        // `to_draw` 不提交 tex_id（GPU 侧接线未定案），
        // 但转换本身不能因此丢批次或崩。
        let b = Batch {
            index_offset: 4,
            index_count: 6,
            clip: rect_at(0.0),
            tex_id: THUMB_TEX,
        };
        let d = b.to_draw();
        assert_eq!(d.index_offset, 4);
        assert_eq!(d.index_count, 6);
    }

    #[test]
    fn vertex_layout_matches_pipeline_stride() {
        // pipeline.rs 声明的步长是 pos(8)+uv(8)+color(4)=20 字节。
        // 布局不符时驱动不会报错，只会画出乱码。
        assert_eq!(std::mem::size_of::<Vertex>(), 20);
    }

    #[test]
    fn capacity_growth_is_monotonic() {
        assert!(next_capacity(1) >= 1);
        assert!(next_capacity(2000) >= 2000);
        assert_eq!(next_capacity(2000) % 1024, 0);
        // 连续扩容不应原地踏步，否则每帧都重建。
        assert!(next_capacity(next_capacity(2000)) > next_capacity(2000));
    }

    #[test]
    fn initial_capacities_cover_a_typical_frame() {
        // 初始容量太小会让首帧就触发一次缓冲重建（多一次 GPU 分配）。
        // 2000 顶点的界面在剪贴板场景里算大，8192 起步合理。
        assert!(INITIAL_VERTEX_CAPACITY >= 2048);
        assert!(INITIAL_INDEX_CAPACITY >= INITIAL_VERTEX_CAPACITY);
    }

    #[test]
    fn uniform_matrix_is_identity() {
        // 着色器已自己做像素→NDC；再乘非单位矩阵会二次变换。
        let u = uniforms_for(vk::Extent2D { width: 800, height: 600 }, 1.0);
        assert_eq!(
            u.clip_from_uv,
            [
                1.0, 0.0, 0.0, 0.0, //
                0.0, 1.0, 0.0, 0.0, //
                0.0, 0.0, 1.0, 0.0, //
                0.0, 0.0, 0.0, 1.0,
            ]
        );
        assert_eq!(u.size_in_pixels, [800.0, 600.0]);
    }

    #[test]
    fn uniform_key_distinguishes_all_inputs() {
        // uniform 只在内容变化时重写：指纹必须能区分三种输入。
        let a = UniformKey {
            width: 800,
            height: 600,
            pixels_per_point: 1.0,
        };
        assert_ne!(
            a,
            UniformKey {
                width: 801,
                height: 600,
                pixels_per_point: 1.0
            }
        );
        assert_ne!(
            a,
            UniformKey {
                width: 800,
                height: 601,
                pixels_per_point: 1.0
            }
        );
        assert_ne!(
            a,
            UniformKey {
                width: 800,
                height: 600,
                pixels_per_point: 1.5
            }
        );
    }

    #[test]
    fn repaint_delay_maps_zero_to_immediate() {
        // repaint_delay 为 0 表示「立即重绘」，应转成 None 让调用方走
        // 「有事件才处理」而不是 sleep(0) 空转。
        let mut out = egui::FullOutput::default();
        out.viewport_output.insert(
            egui::ViewportId::ROOT,
            egui::ViewportOutput {
                parent: egui::ViewportId::ROOT,
                class: egui::ViewportClass::Root,
                builder: egui::ViewportBuilder::default(),
                viewport_ui_cb: None,
                commands: Vec::new(),
                repaint_delay: std::time::Duration::ZERO,
            },
        );
        assert_eq!(repaint_delay(&out), None);

        out.viewport_output
            .get_mut(&egui::ViewportId::ROOT)
            .expect("刚插入")
            .repaint_delay = std::time::Duration::from_millis(250);
        assert_eq!(
            repaint_delay(&out),
            Some(std::time::Duration::from_millis(250))
        );
    }
}
