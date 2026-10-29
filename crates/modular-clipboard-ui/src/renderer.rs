//! egui 绘制桥：把 [`egui::ClippedPrimitive`] 变成 GPU 可执行的一帧。
//!
//! # 职责边界
//!
//! | 层 | 负责 |
//! |----|------|
//! | [`modular_clipboard_gfx::frame::FrameRenderer`] | 交换链、渲染通道开启、命令录制与提交、呈现、重建 |
//! | 本模块（`Painter`） | 顶点数据构造、缓冲上传、uniform、描述符绑定 |
//! | `modular-clipboard-gfx` 资源层 | 缓冲/纹理的创建、销毁、显存分配 |
//!
//! 本模块**不含 Win32 代码**，也不持有 [`egui::Context`]——输入由
//! [`modular_clipboard_gfx::window::EventLoop::egui_input`] 翻译成 [`egui::RawInput`]，
//! tessellation 由调用方用 `Context::tessellate` 完成。
//! 这样同一套绘制逻辑既能被 `full_app` 探针用，也能被正式应用用。

use ash::vk;
use modular_clipboard_gfx::Gpu;
use modular_clipboard_gfx::buffer::{Buffer, Vertex};
use modular_clipboard_gfx::pipeline::Uniforms;


mod painter;

pub(crate) use painter::Painter;


/// 窗口标题，同时作为 Vulkan 应用标识。
/// 主窗口标题。
///
/// 单一数据源在 `modular-clipboard-ipc`：常驻进程要用同一个字符串
/// `FindWindow` 来唤起界面（托盘点击 / 重复双击图标）。两边各写一份时，
/// 改标题会让「点了托盘没反应」，且很难联想到是标题漂移。
pub const WINDOW_TITLE: &str = modular_clipboard_ipc::UI_WINDOW_TITLE;

/// 顶点缓冲初始容量（顶点数）。不足时按 [`next_capacity`] 扩容。
pub const INITIAL_VERTEX_CAPACITY: usize = 8192;

/// 索引缓冲初始容量（索引数）。
pub const INITIAL_INDEX_CAPACITY: usize = 12288;

/// egui 约定 `Managed(0)` 恒为字体图集（见 `epaint::TextureId` 的文档）。
///
/// 改错这个常量会让所有文字变成豆腐块——字体图集不会被采样，
/// 而图集左上角那个「纯白像素」正是 `WHITE_UV` 指向的位置。
pub const FONT_TEXTURE_ID: egui::TextureId = egui::TextureId::Managed(0);

/// 着色器里的「采样哪张纹理」：`0` = 字体图集，`1` = 用户纹理。
pub const TEX_SLOT_FONT: u32 = 0;
/// 见 [`TEX_SLOT_FONT`]。
pub const TEX_SLOT_USER: u32 = 1;

/// 把 egui 的 [`egui::TextureId`] 映射到着色器用的纹理槽位号。
///
/// # 为什么压成 0/1 而不是透传 `TextureId`
///
/// egui 的 `TextureId` 是 `Managed(u64) | Viewport(u32)`，直接传下去需要
/// 着色器侧再解一次枚举。缩略图只有「字体」与「非字体」两类，
/// 着色器一个 `if` 就够⇒ 顶点里带一个 `u32` 槽位号即可。
///
/// # 未知纹理一律当作用户纹理
///
/// 字体图集是 `Managed(0)`（egui 的硬约定）。其余任何纹理都归到
/// 「用户纹理」槽——出错时表现为**采样一张空白/错误图**，
/// 而静默把字体当用户纹理会让**所有文字消失**（字体图集是
/// 单通道覆盖率图，当彩色图采样出来几乎全黑）。
/// 两种错法都不报错，但后者影响面大得多。
pub fn texture_slot(id: egui::TextureId) -> u32 {
    if id == FONT_TEXTURE_ID {
        TEX_SLOT_FONT
    } else {
        TEX_SLOT_USER
    }
}

/// 纹理增量是否为「整图更新」。
///
/// # 这个判据曾经写反过（2026-10-16 修）
///
/// `upload_font_delta` 早前写成 `let Some(_pos) = last.pos else { continue }`
/// ——把 `pos == None`（**整图更新**，`ctx.load_texture` 的唯一形态）
/// 当成「局部更新」跳过 ⇒ 缩略图纹理从未上传，症状是
/// **「缩略图区域一片空白」且不报任何 GPU 错误**（描述符绑的是
/// 占位图，一切合法，只是没有内容）。
///
/// egui 语义（`epaint::ImageDelta`）：`pos == None` ⇒ 整图替换；
/// `Some(offset)` ⇒ 纹理内某个子区域的补丁。
pub fn is_full_image_update(pos: Option<[usize; 2]>) -> bool {
    pos.is_none()
}

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
    /// 转成提交给渲染层的批次，并把裁剪矩形换算成**物理像素**。
    ///
    /// # 为什么必须在这里换算
    ///
    /// `prim.clip_rect` 是**逻辑点**，而 `vk::Rect2D` 要的是物理像素且
    /// 取整。渲染层（`gfx::frame`）不持有 `ppp`，所以换算只能在这里做。
    ///
    /// #NOTE: 换算里的三个坑
    ///
    /// 1. **负坐标必须钳到 0**。`painter_at` 允许传入屏幕外/负值的矩形
    ///    （例如浮层被拖到窗口左侧），`offset` 为负会让 `cmd_set_scissor`
    ///    触发 Vulkan 的 `vk::Scissor` 非法值（`x + extent <= x`），
    ///    规范下属于**未定义行为**——可能整帧不渲染或校验层报错。
    /// 2. **宽高必须至少 1 像素**。宽度为 0 的 scissor 是合法的「什么都不画」，
    ///    但 `extent` 为 0 且 `offset` 也为 0 时同样踩到上面的边界条件；
    ///    钳成 0×0 会被下面 `if w == 0` 分支整体跳过，语义更明确。
    /// 3. **必须与交换链尺寸求交**。裁剪矩形超出视口时 Vulkan 会把
    ///    矩形裁到视口内（这是合法的），但显式求交能让「整块被裁掉」
    ///    的情形走跳过分支，省掉一次 `draw_indexed`。
    pub fn to_draw(self, extent: vk::Extent2D, pixels_per_point: f32) -> modular_clipboard_gfx::frame::DrawBatch {
        let ppp = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
            pixels_per_point
        } else {
            1.0
        };
        let c = self.clip;
        let x0 = (c.min.x * ppp).floor().max(0.0) as i32;
        let y0 = (c.min.y * ppp).floor().max(0.0) as i32;
        let x1 = (c.max.x * ppp).ceil().max(0.0) as i32;
        let y1 = (c.max.y * ppp).ceil().max(0.0) as i32;
        // 与视口求交（extent 是 u32，转 i64 避免溢出）。
        let vw = extent.width as i64;
        let vh = extent.height as i64;
        let x0 = x0.clamp(0, vw as i32);
        let y0 = y0.clamp(0, vh as i32);
        let x1 = x1.clamp(0, vw as i32);
        let y1 = y1.clamp(0, vh as i32);
        let w = (x1 - x0).max(0) as u32;
        let h = (y1 - y0).max(0) as u32;

        modular_clipboard_gfx::frame::DrawBatch {
            index_offset: self.index_offset,
            index_count: self.index_count,
            // 宽或高为 0 ⇒ 整批被裁掉，用零 scissor（合法的「不画」）
            // 而不是 `None`：后者会退回全屏，等于裁剪没生效。
            clip: Some(vk::Rect2D {
                offset: vk::Offset2D { x: x0, y: y0 },
                extent: vk::Extent2D { width: w, height: h },
            }),
            // 默认不重绑（文字/形状批）。缩略图批由 paint() 在转换后
            // 按 `tex_id` 从 `Painter::user_textures` 查表填上该纹理
            // 的描述符集——本方法拿不到纹理表。
            user_set: vk::DescriptorSet::null(),
        }
    }
}

// ---------------------------------------------------------------------------
// tessellation：egui 图元 → 顶点数据
// ---------------------------------------------------------------------------

/// 把一批 [`egui::epaint::ClippedPrimitive`] 转换后写入三个向量
/// （三者都会被先清空，可反复复用而不重新分配）。
///
/// # 返回值：模糊分割点
///
/// 返回**首个** `Primitive::Callback` 出现时已生成的批次数。
///
/// # 为什么用 `PaintCallback` 当分割标记
///
/// 自研模糊要求把界面拆成「覆盖层之前的内容（要被模糊）」与
/// 「覆盖层本身（保持清晰）」两段。`ClippedPrimitive` 本身不带
/// 「这是第几段」的信息，而 `PaintCallback` 是 egui **自带**的
/// 「此处插入自定义绘制」机制——把它当成渲染层与 UI 层之间的
/// 分段信号，不需要额外发明协议，且它在 tessellation 里必然保留
/// 为一个独立的 primitive（不会与相邻网格合并）。
///
/// 没有标记时返回 `batches.len()`，调用方据此走普通绘制路径。
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
) -> usize {
    vertices.clear();
    indices.clear();
    batches.clear();
    // 首个 Callback 出现时的批次数。`None` 表示本帧没有分割标记。
    let mut blur_split: Option<usize> = None;

    for prim in primitives {
        let mesh = match &prim.primitive {
            egui::epaint::Primitive::Mesh(m) => m,
            // PaintCallback 需要用户代码参与光栅化，本渲染器不支持。
            //
            // 但**不丢弃它的位置信息**：它标记了「模糊分割点」——
            // 之前的批次画到离屏场景图并卷积，之后的批次保持清晰。
            // 只取第一个标记（一帧内最多一段模糊背景）。
            egui::epaint::Primitive::Callback(_) => {
                blur_split.get_or_insert(batches.len());
                continue;
            }
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
                tex_id: texture_slot(mesh.texture_id),
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

    // 没有分割标记时返回批次数（等价于「全部内容」），调用方据此走普通路径。
    blur_split.unwrap_or(batches.len())
}

/// egui 的 `Color32` → 顶点色 `u32`（小端 RGBA，着色器按
/// `unpack4x8unorm` 逐字节展开）。
///
/// # IMPORTANT: 必须原样上传，**不能**反预乘
///
/// epaint 的顶点色是 **sRGBA 预乘 alpha**（`r,g,b` 已乘过 `a`），而
/// `pipeline.rs` 的混合是预乘公式 `ONE / ONE_MINUS_SRC_ALPHA`（与
/// egui-wgpu 一致）。两者恰好配套：预乘色交给预乘混合，一次完成。
///
/// 早前这里做过一次「反预乘」——依据是当时认为混合是直通公式
/// （`SRC_ALPHA`）。后来管线为修「圆角羽化边缘发暗」改成了预乘公式，
/// 这里的反预乘却没有跟着删，于是 alpha 被抵消两次：
/// 半透明填充（选中行 `row_selected`、悬停 `row_hover`、遮罩）的 RGB
/// 以**全强度**叠加进画面，超出 1.0 的通道被裁剪——浅色主题下
/// 选中行 (29,101,175)@α52 叠在近白底上裁成 (228,255,255)，即
/// 用户看到的「选中态发青偏绿」。文字不受影响（alpha=255 时反预乘
/// 是恒等），所以这个错误长期只在这类低 alpha 大面积填充上显形。
///
/// 着色器输出 `color * vec4(1,1,1,cov)`（覆盖率乘全部四通道）：
/// 预乘色 × 覆盖率仍是预乘色，正是预乘「over」期望的形式。
pub fn pack_color(c: egui::Color32) -> u32 {
    let [r, g, b, a] = c.to_array();
    u32::from_le_bytes([r, g, b, a])
}

/// 构造本帧的 uniform。
///
/// # IMPORTANT: 坐标系不变式（务必与 `EventLoop::egui_input` 成对阅读）
///
/// * `size_in_pixels` = **物理像素**（交换链尺寸），来自 `Painter::paint` 的
///   `fr.extent()`；
/// * `pixels_per_point` = egui 的**逻辑点→物理像素**比例，即
///   [`egui::FullOutput::pixels_per_point`]；
/// * 着色器算`ndc = 2 * (pos * dpr) / size_in_pixels`，其中 `pos` 是
///   `epaint` 吐出的**逻辑点**（`epaint::Vertex::pos` 的文档即如此）。
///
/// 于是必须满足
///
/// ```text
/// screen_rect.size() * pixels_per_point == size_in_pixels
/// ```
///
/// 这与 egui 官方后端 `egui-wgpu` 0.36 的写法**完全等价**：官方把
/// `size_in_pixels / ppp`（逻辑点）塞进 uniform，顶点着色器直接
/// `2 * pos / screen_size` 而不乘 dpr。同一件事的两种写法，本项目选
/// 「物理视口 + 乘 dpr」。
///
/// NOTE: 只要 `ppp` 被漏设成 1.0（DPI 144 时真实值是 1.5），这条不变式就破了：
/// 布局按逻辑点算好、换算按物理视口算，于是内容只占视口的 1/1.5，
/// 右侧与底部各空 1/3，且**不产生任何 GPU 错误**。
pub fn uniforms_for(extent: vk::Extent2D, pixels_per_point: f32) -> Uniforms {
    Uniforms::new(
        [extent.width as f32, extent.height as f32],
        pixels_per_point,
    )
}

/// uniform 的内容指纹。变了才需要重写缓冲。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UniformKey {
    pub width: u32,
    pub height: u32,
    /// egui 的逻辑点→物理像素比例。
    ///
    /// NOTE: 注意命名不一致：本字段叫 `pixels_per_point`，而它写进去的
    /// `crate::pipeline::Uniforms` 里对应字段叫 **`dpr`**。
    /// 同一个值两个名字，改动时容易写错。
    pub pixels_per_point: f32,
}

/// 扩容策略：1.5 倍并向上取整到 1024，避免「每帧只多一个元素就重建」。
pub fn next_capacity(needed: usize) -> usize {
    needed.saturating_mul(3).div_ceil(2).next_multiple_of(1024)
}

/// egui 请求的下一帧延迟。`None` 表示「有事件时立即处理，不要 sleep」。
/// 本帧之后还需等多久再重绘（`None` = 立即重绘）。
///
/// # egui 的 `repaint_delay` 语义（很容易搞反）
///
/// | 值 | 含义 |
/// |---|---|
/// | `Duration::MAX` | **不重绘**——初始值，等事件驱动 |
/// | `Duration::ZERO` | **立即重绘** |
/// | `Duration::from_millis(n)` | n 毫秒后重绘 |
///
/// 出处：`egui::Context` 的 `ViewportRepaintInfo::default()` 把
/// `repaint_delay` 设为 `Duration::MAX`（注释写着 "We haven't
/// scheduled a repaint yet."），而 `request_repaint()` 才置 ZERO。
///
/// NOTE: 早前一版把 `ZERO` 映射成 `None` 并注释说
///「ZERO 表示立即重绘，应转成 None 让调用方走有事件才处理」——
/// **语义反了**：调用方拿到 None 就不会再触发这一帧重绘，
/// 于是「有事件才处理」的路径永远等不到这次立即重绘，
/// 表现为控件状态该变却没变（聚焦高亮、光标闪烁都靠它驱动）。
pub fn repaint_delay(output: &egui::FullOutput) -> Option<std::time::Duration> {
    let v = output.viewport_output.get(&egui::ViewportId::ROOT)?;
    match v.repaint_delay {
        // Duration::MAX = egui 明确表示「这一帧不需要重绘」。
        d if d == std::time::Duration::MAX => None,
        // ZERO = 立即重绘，调用方应尽快跑下一帧。
        d => Some(d),
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
    use super::{
        painter::*,
    };
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

    /// 磨砂移动预测的 UV 位移必须与**物理像素**口径对齐。
    ///
    /// 钉住两件事：
    /// 1. 位移 = 屏幕物理像素差 ÷ 客户区物理像素尺寸（归一化）；
    /// 2. **与超采样倍率无关**——用 `render_extent`（物理像素 × 倍率）
    ///    当分母会让补偿小 `ssaa` 倍，表现为「2× 超采样时拖动窗口、
    ///    磨砂底图跟不上半个身位」。
    #[test]
    fn backdrop_uv_offset_uses_physical_pixel_extent() {
        let ext = vk::Extent2D {
            width: 1000,
            height: 500,
        };
        // 无位移 → 零。
        assert_eq!(
            backdrop_uv_offset_from((100.0, 200.0), (100.0, 200.0), ext),
            [0.0, 0.0]
        );
        // 移动 100/1000 = 0.1、200/500 = 0.4。
        let off = backdrop_uv_offset_from((200.0, 400.0), (100.0, 200.0), ext);
        assert!((off[0] - 0.1).abs() < 1e-6, "x 位移应按宽度归一化：{off:?}");
        assert!((off[1] - 0.4).abs() < 1e-6, "y 位移应按高度归一化：{off:?}");
        // 负位移（窗口向左/上移动）同样成立。
        let back = backdrop_uv_offset_from((0.0, 0.0), (100.0, 200.0), ext);
        assert!((back[0] + 0.1).abs() < 1e-6, "反向位移应取负：{back:?}");
        // 若误用 `render_extent`（×ssaa）当分母，结果会小 ssaa 倍——
        // 这里显式算出那个错误值，证明两者确实不同（防「测试恒真」）。
        let ssaa = 2.0_f32;
        let wrong = backdrop_uv_offset_from(
            (200.0, 400.0),
            (100.0, 200.0),
            vk::Extent2D {
                width: (ext.width as f32 * ssaa) as u32,
                height: (ext.height as f32 * ssaa) as u32,
            },
        );
        assert!(
            (wrong[0] - off[0]).abs() > 1e-3,
            "用 render_extent 当分母会得到不同的（小 ssaa 倍的）值——\
             本测试才有意义"
        );
        // 退化尺寸不得除零。
        let tiny = backdrop_uv_offset_from(
            (10.0, 10.0),
            (0.0, 0.0),
            vk::Extent2D {
                width: 0,
                height: 0,
            },
        );
        assert!(
            tiny[0].is_finite() && tiny[1].is_finite(),
            "零尺寸不得产生 NaN"
        );
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
    fn semi_transparent_colors_are_uploaded_premultiplied() {
        // 回归：早前这里对预乘顶点色做了「反预乘」，与管线的预乘混合
        // （ONE / ONE_MINUS_SRC_ALPHA）双重抵消，半透明选中色以全强度
        // 叠加、超界通道被裁剪，浅色主题下选中行发青偏绿。
        // 正确行为：预乘色原样上传，不做任何换算。
        let c = egui::Color32::from_rgba_unmultiplied(0x1D, 0x65, 0xAF, 52);
        let packed = pack_color(c);
        // egui 存储即预乘：r = round(29×52/255) = 6
        assert_eq!(packed & 0xFF, 6, "最低字节应是预乘后的红");
        assert_eq!((packed >> 8) & 0xFF, 21, "预乘后的绿");
        assert_eq!((packed >> 16) & 0xFF, 36, "预乘后的蓝");
        assert_eq!((packed >> 24) & 0xFF, 52, "alpha 原样");
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

    /// 造一个 `PaintCallback` 图元，充当模糊分割标记。
    fn callback_prim() -> ClippedPrimitive {
        ClippedPrimitive {
            clip_rect: rect_at(0.0),
            primitive: Primitive::Callback(egui::epaint::PaintCallback {
                rect: rect_at(0.0),
                callback: std::sync::Arc::new(|_: &ClippedPrimitive| {}),
            }),
        }
    }

    #[test]
    fn callback_marks_blur_split_at_batch_boundary() {
        // 标记前两个网格、标记后一个 ⇒ 分割点应为 2（标记本身不成批）。
        let prims = vec![
            prim_with_clip(rect_at(0.0)),
            prim_with_clip(rect_at(20.0)),
            callback_prim(),
            prim_with_clip(rect_at(40.0)),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        let split = tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(split, 2, "分割点必须落在标记之前已生成的批次数上");
        assert_eq!(b.len(), 3, "标记本身不产生批次");
        // 分割点必须严格落在批次区间内，否则调用方会切出空区间。
        assert!(split < b.len());
    }

    #[test]
    fn no_callback_means_split_is_all_batches() {
        // 没有标记 ⇒ 分割点 = 批次数，调用方据此走普通绘制路径
        // （不能把「全部内容」误判成「全部要模糊」）。
        let prims = vec![prim_with_clip(rect_at(0.0)), prim_with_clip(rect_at(20.0))];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        let split = tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(split, b.len(), "无标记时分割点应等于批次数");
    }

    #[test]
    fn first_callback_wins_when_multiple_present() {
        // 一帧内最多一段模糊背景：出现多个标记时只认第一个，
        // 否则后面的标记会把分割点推到覆盖层内部。
        let prims = vec![
            prim_with_clip(rect_at(0.0)),
            callback_prim(),
            prim_with_clip(rect_at(20.0)),
            callback_prim(),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        let split = tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(split, 1, "只认第一个标记");
    }

    #[test]
    fn callback_at_start_yields_zero_split() {
        // 标记在第一个图元之前 ⇒ 分割点为 0。调用方据此判定
        // 「标记前没有内容」，退回普通路径（模糊没有可糊的东西）。
        let prims = vec![callback_prim(), prim_with_clip(rect_at(0.0))];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        let split = tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert_eq!(split, 0);
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

    #[test]
    fn texture_slot_maps_font_to_zero_and_others_to_one() {
        // 着色器只需二分：0 = 字体图集，1 = 用户纹理。
        assert_eq!(texture_slot(FONT_TEXTURE_ID), TEX_SLOT_FONT);
        assert_eq!(texture_slot(egui::TextureId::Managed(1)), TEX_SLOT_USER);
        assert_eq!(texture_slot(egui::TextureId::Managed(7)), TEX_SLOT_USER);
        // epaint 0.36 的 `TextureId` 只有 `Managed` / `User` 两个变体，
        // 没有 `Viewport`（那是更早版本的形状）。两者都必须落到用户槽。
        assert_eq!(texture_slot(egui::TextureId::User(0)), TEX_SLOT_USER);
    }

    #[test]
    fn vertices_carry_tex_id_matching_their_mesh() {
        // 端到端验证 `tessellate_into` 真的把纹理写进了顶点。
        // 单看 `texture_slot` 的单测不够——映射对了但没接上，
        // 或者接反了，这里才抓得住。
        let clip = rect_at(0.0);
        let prims = vec![
            prim_with_tex(clip, 3, 3, FONT_TEXTURE_ID),
            prim_with_tex(clip, 3, 3, THUMB_TEX),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_eq!(v.len(), 6, "两个三角形共 6 个顶点");
        // 前 3 个来自字体图集，后 3 个来自缩略图。
        for vert in &v[..3] {
            assert_eq!(vert.tex_id, TEX_SLOT_FONT, "字体图元应带槽位 0");
        }
        for vert in &v[3..] {
            assert_eq!(vert.tex_id, TEX_SLOT_USER, "缩略图元应带槽位 1");
        }
    }

    #[test]
    fn real_egui_text_stays_on_the_font_slot() {
        // 用真实 egui 跑一帧：真实文字的 mesh 一定带字体图集 ID，
        // 顶点必须全是槽位 0。
        //
        // 回归意义：若 `texture_slot` 反了（字体判成 1），
        // 字体图集会被当彩色图采样 ⇒ **所有文字消失**且不报错。
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
            ui.label("正文");
        });
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);

        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert!(!v.is_empty(), "egui 应产出顶点");
        assert!(
            v.iter().all(|x| x.tex_id == TEX_SLOT_FONT),
            "纯文字界面的所有顶点都应走字体图集"
        );
    }

    #[test]
    fn uploaded_image_uses_the_user_slot() {
        // 真实走一次 `load_texture` + `Image`，确认 egui 派发的图元
        // 带的是**用户**纹理 ID，且被映射成槽位 1。
        // 这覆盖了「egui 怎么给用户纹理分配 TextureId」这个我无法预设的细节。
        let ctx = egui::Context::default();
        // ColorImage::new 需要像素数组，不是单个颜色
        let img = egui::ColorImage::new([2, 2], vec![egui::Color32::WHITE; 4]);
        let handle = ctx.load_texture(
            "unit-test-thumb",
            img,
            egui::TextureOptions::LINEAR,
        );
        let tex_id = handle.id();

        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            // egui 0.36 只为 (TextureId, Vec2) 实现了 From，
            // 不为 (TextureHandle, Vec2) 实现
            ui.image((handle.id(), egui::vec2(64.0, 64.0)));
        });
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);

        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);

        assert_ne!(tex_id, FONT_TEXTURE_ID, "用户纹理不应与字体图集同 ID");
        assert!(
            v.iter().any(|x| x.tex_id == TEX_SLOT_USER),
            "图片图元应带槽位 1（tex_id = {tex_id:?}）"
        );
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
        // `to_draw` 本身不查纹理表（拿不到），默认 `user_set` 为 null
        // （文字/形状批就该用帧槽集合）；缩略图批的集合由 paint()
        // 在转换后按 `tex_id` 填。转换本身不能因此丢批次或崩。
        let b = Batch {
            index_offset: 4,
            index_count: 6,
            clip: rect_at(0.0),
            tex_id: THUMB_TEX,
        };
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let d = b.to_draw(extent, 1.0);
        assert_eq!(d.index_offset, 4);
        assert_eq!(d.index_count, 6);
        assert_eq!(
            d.user_set,
            vk::DescriptorSet::null(),
            "默认不得重绑用户纹理"
        );
    }

    // ---- 纹理增量的「整图 / 局部」判据 ---------------------------------

    #[test]
    fn full_image_update_is_pos_none() {
        // 回归测试：判据反过会让整图更新被当成局部更新跳过，
        // 缩略图永远上传不上去——表现为「缩略图区域一片空白」
        // （用户实机反馈：渲染空白画布），且不报任何 GPU 错误。
        assert!(is_full_image_update(None), "pos=None 才是整图更新");
        assert!(!is_full_image_update(Some([0, 0])), "pos=Some 是局部更新");
        assert!(!is_full_image_update(Some([16, 32])));
    }

    // ---- 裁剪矩形换算 --------------------------------------------------
    //
    // 这组测试守的是「UI 裁剪真的生效」这条链路的换算端。
    // 缺陷历史：clip_rect 曾被完全忽略（只用于合批判定），
    // 导致所有 painter_at 裁剪形同虚设、文字溢出面板。

    fn clip_of(r: egui::Rect) -> egui::Rect {
        r
    }

    #[test]
    fn clip_scales_by_ppp_into_physical_pixels() {
        // 逻辑点 (10,20)-(110,70)，ppp=1.5 ⇒ 物理 (15,30)-(165,105)
        let b = Batch {
            index_offset: 0,
            index_count: 3,
            clip: clip_of(egui::Rect::from_min_max(
                egui::pos2(10.0, 20.0),
                egui::pos2(110.0, 70.0),
            )),
            tex_id: FONT_TEXTURE_ID,
        };
        let d = b.to_draw(
            vk::Extent2D {
                width: 900,
                height: 800,
            },
            1.5,
        );
        let c = d.clip.expect("必须带裁剪矩形，否则裁剪等于没做");
        assert_eq!((c.offset.x, c.offset.y), (15, 30));
        assert_eq!((c.extent.width, c.extent.height), (150, 75));
    }

    #[test]
    fn clip_negative_origin_is_clamped_to_zero() {
        // 浮层被拖到窗口左侧：x 为负。
        // Vulkan 的 scissor 要求 x + width > x，负 offset 属未定义行为，
        // 必须钳到 0 而不是原样传下去。
        let b = Batch {
            index_offset: 0,
            index_count: 3,
            clip: clip_of(egui::Rect::from_min_max(
                egui::pos2(-50.0, -20.0),
                egui::pos2(30.0, 40.0),
            )),
            tex_id: FONT_TEXTURE_ID,
        };
        let d = b.to_draw(
            vk::Extent2D {
                width: 800,
                height: 600,
            },
            1.0,
        );
        let c = d.clip.expect("必须带裁剪矩形");
        assert_eq!(c.offset.x, 0, "负 x 必须钳到 0");
        assert_eq!(c.offset.y, 0, "负 y 必须钳到 0");
        // 原矩形 x∈[-50,30]、y∈[-20,40]；钳位后可见部分
        // 是 x∈[0,30]、y∈[0,40] ⇒ 宽 30、高 40。
        // NOTE: 期望值是「钳位后的可见区域」，不是原矩形的宽高——
        //   写成 80/60（原点 + 原宽高）会算出一个延伸到 x=80 的
        //   矩形，把本该被裁掉的部分又画回来。
        assert_eq!(c.extent.width, 30, "宽度应是钳位后与视口相交的部分");
        assert_eq!(c.extent.height, 40);
    }

    #[test]
    fn clip_fully_outside_viewport_becomes_zero_area_not_fullscreen() {
        // NOTE: 这一条是「裁剪失效」与「裁剪生效」的分界：
        // 整块被裁掉时若退回 `None`（= 全屏），文字就会重新画到窗口上。
        let b = Batch {
            index_offset: 0,
            index_count: 3,
            clip: clip_of(egui::Rect::from_min_max(
                egui::pos2(5000.0, 5000.0),
                egui::pos2(6000.0, 6000.0),
            )),
            tex_id: FONT_TEXTURE_ID,
        };
        let d = b.to_draw(
            vk::Extent2D {
                width: 800,
                height: 600,
            },
            1.0,
        );
        let c = d.clip.expect("必须是 Some(零面积)，不能是 None");
        assert_eq!((c.offset.x, c.offset.y), (800, 600));
        assert_eq!((c.extent.width, c.extent.height), (0, 0));
    }

    #[test]
    fn clip_is_intersected_with_viewport() {
        // 右侧超出视口：显式求交，避免依赖 Vulkan 的隐式裁剪。
        let b = Batch {
            index_offset: 0,
            index_count: 3,
            clip: clip_of(egui::Rect::from_min_max(
                egui::pos2(700.0, 100.0),
                egui::pos2(2000.0, 200.0),
            )),
            tex_id: FONT_TEXTURE_ID,
        };
        let d = b.to_draw(
            vk::Extent2D {
                width: 800,
                height: 600,
            },
            1.0,
        );
        let c = d.clip.expect("必须带裁剪矩形");
        assert_eq!(c.extent.width, 100, "右缘应被视口右边界截断");
        assert_eq!(c.extent.height, 100);
    }

    #[test]
    fn clip_survives_degenerate_ppp() {
        // ppp 传0 / NaN / 负数时不能变成 0 宽或 panic，
        // 否则整个界面会在一次异常 ppp 后变成空白。
        for bad in [0.0f32, -1.0, f32::NAN, f32::INFINITY] {
            let b = Batch {
                index_offset: 0,
                index_count: 3,
                clip: clip_of(egui::Rect::from_min_max(
                    egui::pos2(0.0, 0.0),
                    egui::pos2(100.0, 50.0),
                )),
                tex_id: FONT_TEXTURE_ID,
            };
            let d = b.to_draw(
                vk::Extent2D {
                    width: 800,
                    height: 600,
                },
                bad,
            );
            let c = d.clip.expect("退化 ppp 也必须带裁剪矩形");
            assert!(
                c.extent.width > 0 && c.extent.height > 0,
                "ppp={bad} 时裁剪矩形退化为零面积，界面会变空白"
            );
        }
    }

    #[test]
    fn every_batch_carries_a_clip_rect() {
        // 合批后每个批次都必须有 clip：漏一个就等于那一段不裁剪。
        let prims = vec![
            prim_with_clip(egui::Rect::from_min_max(
                egui::pos2(0.0, 0.0),
                egui::pos2(50.0, 50.0),
            )),
            prim_with_clip(egui::Rect::from_min_max(
                egui::pos2(60.0, 0.0),
                egui::pos2(120.0, 50.0),
            )),
        ];
        let (mut v, mut i, mut b) = (Vec::new(), Vec::new(), Vec::new());
        tessellate_into(&prims, &mut v, &mut i, &mut b);
        assert!(b.len() >= 2, "不同 clip 不应合批");
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        for batch in &b {
            assert!(
                batch.to_draw(extent, 1.0).clip.is_some(),
                "每个批次都必须带 clip，否则该段不裁剪"
            );
        }
    }

    /// 造一个带指定裁剪矩形的最小图元（一个 2x2 的矩形网格）。
    fn prim_with_clip(clip: egui::Rect) -> egui::epaint::ClippedPrimitive {
        use egui::epaint::{Color32, Mesh, Primitive, Vertex};
        let mut mesh = Mesh::default();
        for (x, y) in [(0.0f32, 0.0f32), (10.0, 0.0), (0.0, 10.0), (10.0, 10.0)] {
            mesh.vertices.push(Vertex {
                pos: egui::pos2(x, y),
                uv: egui::pos2(0.0, 0.0),
                color: Color32::WHITE,
            });
        }
        mesh.indices.extend_from_slice(&[0, 1, 2, 1, 3, 2]);
        egui::epaint::ClippedPrimitive {
            primitive: Primitive::Mesh(mesh),
            clip_rect: clip,
        }
    }

    #[test]
    fn vertex_layout_matches_pipeline_stride() {
        // pipeline.rs 声明的步长是 pos(8)+uv(8)+color(4)+tex_id(4)=24 字节。
        // 布局不符时驱动不会报错，只会画出乱码。
        //
        // 这条断言在本项目改过两次：加 `tex_id` 前是 20 字节。
        // 两次都是「Rust 侧改了、忘了同步管线」的类型——
        // 由本测试当场抓住，而不是等到实机看见乱码。
        assert_eq!(std::mem::size_of::<Vertex>(), 24);
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
        const {
            assert!(INITIAL_VERTEX_CAPACITY >= 2048);
            assert!(INITIAL_INDEX_CAPACITY >= INITIAL_VERTEX_CAPACITY);
        }
    }

    #[test]
    fn uniforms_carry_viewport_and_dpr() {
        // 本缺陷的判据（渲染侧）：uniform 只带视口尺寸与 dpr 两个换算
        // 依据，顶点是逻辑点、视口是物理像素，`screen_rect * ppp ==
        // size_in_pixels` 是唯一不变式（详见 `uniforms_for` 的文档）。
        //
        // 2026-10-17 前这里断言 `clip_from_uv` 是单位矩阵——该字段
        // （恒为单位矩阵的 64 字节 mat4x4）已删除：着色器自己完成
        // 像素→NDC，留它只会让「谁设非单位矩阵」变成二次变换陷阱。
        let u = uniforms_for(vk::Extent2D { width: 800, height: 600 }, 1.0);
        assert_eq!(u.size_in_pixels, [800.0, 600.0]);
        assert_eq!(u.dpr, 1.0);
        // uniform 必须保持 16 字节（std140 与 WGSL 布局一致）。
        assert_eq!(std::mem::size_of_val(&u), 16);
    }

    /// **本缺陷的判据（渲染侧）**：顶点是逻辑点、视口是物理像素，
    /// 而两者之间唯一的换算依据是 `dpr`。
    ///
    /// # 判据为什么必须是这个不变式
    ///
    /// 本项目吃过三次判据失效的亏（数字对了但用户看到重影、截图没开 DPI
    /// 感知、数字判据与视觉判据不同源），所以这里明确选定**唯一**判据：
    ///
    /// ```text
    /// screen_rect.size() * dpr == size_in_pixels
    /// ```
    ///
    /// 它失败时画面整体缩放错（内容只占视口一部分），且**不产生任何
    /// GPU 错误、验证层也干净**——靠「有没有报错」或「三段宽度之和 ≤
    /// 可用宽度」都抓不到。
    ///
    /// 用真实的 `egui::Context` + `set_pixels_per_point`（与
    /// `EventLoop::egui_input` 里同源）产出真实 `ppp`，而不是手写常量，
    /// 这样本测试与`window.rs` 的守卫真的在同一条链路上。
    #[test]
    fn uniform_dpr_times_logical_canvas_equals_physical_viewport() {
        // 实机遇到的组合：420x560 物理像素 @ DPI 144（ppp 应为 1.5）。
        const PHYSICAL: vk::Extent2D = vk::Extent2D {
            width: 420,
            height: 560,
        };

        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            let logical = egui::vec2(
                PHYSICAL.width as f32 / scale,
                PHYSICAL.height as f32 / scale,
            );
            let ctx = egui::Context::default();
            // 与 window.rs 里的调用完全一致。
            ctx.set_pixels_per_point(scale);
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, logical)),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |_ui| {});
            out.textures_delta.clear();

            // 真实 ppp 来自 egui 自身，而非我们手写。
            let u = uniforms_for(PHYSICAL, out.pixels_per_point);
            assert_eq!(
                logical * u.dpr,
                egui::vec2(PHYSICAL.width as f32, PHYSICAL.height as f32),
                "scale={scale}: screen_rect{logical:?} * dpr({}) 必须等于物理视口 {PHYSICAL:?}",
                u.dpr
            );
            // `size_in_pixels` 必须始终是**物理像素**，不能被顺手改成逻辑点
            // （否则就与顶点侧的 dpr 换算重复乘一次）。
            assert_eq!(
                u.size_in_pixels,
                [PHYSICAL.width as f32, PHYSICAL.height as f32],
                "size_in_pixels 必须是物理像素"
            );
        }
    }

    /// 同一不变式的端到端版：真跑一帧 egui，铺满逻辑画布的矩形经着色器
    /// 公式换算后必须正好铺满物理视口（`ndc` 落在 ±1）。
    ///
    /// 上一条验的是「输入之间自洽」，这条验的是「换算结果真的对」——
    /// 它直接对应实机现象：改前`ndc` 只到 ±1/1.5，右侧/底部空1/3。
    #[test]
    fn full_canvas_rect_maps_to_ndc_edges() {
        const PHYSICAL: vk::Extent2D = vk::Extent2D {
            width: 420,
            height: 560,
        };
        let scale = 1.5_f32;
        let logical = egui::vec2(PHYSICAL.width as f32 / scale, PHYSICAL.height as f32 / scale);

        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(scale);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, logical)),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ctx| {
            // 铺满整个逻辑画布的矩形——实机 UI 的背景就是这么画的。
            let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, logical);
            egui::Painter::new(ctx.clone(), egui::LayerId::background(), canvas)
                .rect_filled(canvas, 0.0, egui::Color32::WHITE);
        });
        out.textures_delta.clear();
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        let u = uniforms_for(PHYSICAL, out.pixels_per_point);

        // 顶点最大坐标（逻辑点）应覆盖整个逻辑画布。
        let mut max_x = 0.0_f32;
        let mut max_y = 0.0_f32;
        for p in &prims {
            if let egui::epaint::Primitive::Mesh(m) = &p.primitive {
                for v in &m.vertices {
                    max_x = max_x.max(v.pos.x);
                    max_y = max_y.max(v.pos.y);
                }
            }
        }
        assert!(max_x >= logical.x - 1.0, "背景矩形应铺满画布宽，实际 {max_x}");

        // 复刻着色器 `vs_main` 的换算（egui.wgsl:53-55）。
        let ndc_x = 2.0 * (max_x * u.dpr) / u.size_in_pixels[0] - 1.0;
        let ndc_y = 1.0 - 2.0 * (max_y * u.dpr) / u.size_in_pixels[1];
        assert!(
            (ndc_x - 1.0).abs() < 0.01,
            "右边缘 ndc 应为 1.0（铺满视口），实际 {ndc_x}——\
             小于 1 即右侧留白"
        );
        assert!(
            (ndc_y + 1.0).abs() < 0.01,
            "下边缘 ndc 应为 -1.0（铺满视口），实际 {ndc_y}——\
             大于 -1 即底部留白"
        );
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
    fn repaint_delay_maps_egui_duration_to_idle() {
        // 语义基准来自 `egui::Context`：`ViewportRepaintInfo::default()`
        // 把 `repaint_delay` 设为 `Duration::MAX`，注释是
        // "We haven't scheduled a repaint yet."；
        // `request_repaint()` 把它置为 `Duration::ZERO`。
        //
        // 因此：**MAX = 不重绘（该睡）**，**ZERO = 立即重绘（不该睡）**。
        let mut out = egui::FullOutput::default();
        let vp = || egui::ViewportId::ROOT;
        out.viewport_output.insert(
            vp(),
            egui::ViewportOutput {
                parent: vp(),
                class: egui::ViewportClass::Root,
                builder: egui::ViewportBuilder::default(),
                viewport_ui_cb: None,
                commands: Vec::new(),
                repaint_delay: std::time::Duration::MAX,
            },
        );
        assert_eq!(
            repaint_delay(&out),
            None,
            "Duration::MAX 表示 egui 不需要重绘 ⇒ 该交给事件驱动（None）"
        );

        // ZERO 必须是 Some(ZERO)：它表示「立刻再跑一帧」。
        // 早前一版把它映射成 None，会让这次立即重绘请求丢失。
        out.viewport_output
            .get_mut(&vp())
            .expect("刚插入")
            .repaint_delay = std::time::Duration::ZERO;
        assert_eq!(
            repaint_delay(&out),
            Some(std::time::Duration::ZERO),
            "Duration::ZERO 是「立即重绘」，必须原样传出而不是当成 None"
        );

        out.viewport_output
            .get_mut(&vp())
            .expect("刚插入")
            .repaint_delay = std::time::Duration::from_millis(250);
        assert_eq!(
            repaint_delay(&out),
            Some(std::time::Duration::from_millis(250))
        );
    }
}