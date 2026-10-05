# 并行开发进度黑板

> 所有 Agent 共享此文件。开工前先读本文件了解他人进度，完成后立即更新自己的行。
> 目录约定见文末「文件所有权」，**每个 Agent 只能改自己负责的文件**。

## 状态一览

| # | 负责范围 | 拥有文件 | 状态 | 阻塞于 | 交付物 |
|---|---------|---------|------|--------|--------|
| A | GPU 资源层（缓冲/内存/纹理/描述符） | `gfx/src/buffer.rs`(新) `gfx/src/texture.rs`(新) | 🟢 已完成（staging 已统一到 arena） | — | 资源封装+65 个单测 |
| B | 帧图与呈现（acquire/present/rebuild） | `gfx/src/frame.rs`(新) | 🟢 已完成 | — | 帧循环+20 单测，全通过 |
| C | Win32 窗口与事件循环 | `gfx/src/window.rs`(新) | 🟡 进行中 | — | 窗口封装+输入映射 |
| D | UI 迁移与集成 | `tiez-ui/src/*` | 🟢 空闲 | A,B,C | egui 绘制接入 |

## 共享接口契约（B 已冻结，其他 Agent 依赖）

>⚠️ **契约已按实现修订**，与初稿有出入，以本节为准。初稿里的
> `record(&self, primitives: &[ClippedPrimitive], ...)` 无法成立：
> `tiez-gfx` 不依赖 egui（依赖 egui 会让渲染层反向依赖 UI 层）。
> 现改为传入渲染层自己的 `DrawInput`（裸句柄 + 批次），
> 由 D 组在 `tiez-ui` 侧负责把 `ClippedPrimitive` 转成 `DrawInput`。

```rust
// crates/tiez-gfx/src/frame.rs
use ash::vk;
use tiez_gfx::frame::{
    DrawBatch, DrawInput, FrameRenderer, PipelineBundle, PresentResult,
};

impl PipelineBundle {
    /// 只存裸句柄，不接管所有权——渲染通道/管线由调用方创建与销毁。
    pub fn new(
        render_pass: vk::RenderPass,
        pipeline_layout: vk::PipelineLayout,
        pipeline: vk::Pipeline,
        descriptor_set_layout: vk::DescriptorSetLayout,
    ) -> Self;
}

impl<'a> FrameRenderer<'a> {
    /// 借用 Gpu（重建时需重新查询表面能力，Gpu 里的快照是启动时的）。
    pub fn new(gpu: &'a Gpu, pipeline_bundle: PipelineBundle) -> anyhow::Result<Self>;

    /// 取下一张交换链图像。**返回 None 表示必须重建**（OUTDATED 或超时）。
    /// 成功时本帧命令录制已开始（布局转换+渲染通道开启+管线绑定）。
    pub fn acquire(&mut self) -> anyhow::Result<Option<AcquiredFrame>>;

    /// 录制绘制命令。必须紧跟 acquire 之后。
    pub fn record(&mut self, input: &DrawInput<'_>) -> anyhow::Result<()>;

    /// 结束渲染通道、提交、呈现。frame 必须是本帧 acquire 返回的那个。
    pub fn present(&mut self, frame: AcquiredFrame) -> anyhow::Result<PresentResult>;

    /// 重建交换链。传 Default::default() 表示尺寸由表面决定。
    /// 会 device_wait_idle、重查 surface_caps、重建 framebuffer/命令缓冲/
    /// 栅栏/信号量/描述符集。视口与剪裁自动跟随新尺寸（动态状态）。
    pub fn rebuild_swapchain(&mut self, desired_extent: vk::Extent2D) -> anyhow::Result<()>;
}

// —— 供 A / D 组对接 ——
impl FrameRenderer<'_> {
    /// 在飞帧槽位数量（== 交换链图像数）
    pub fn slot_count(&self) -> usize;
    /// **当前帧槽位下标**（未 acquire 时 None）。资源层用它索引常驻 staging。
    /// acquire() 返回前已等过并重置该槽位栅栏，因此「拿到就写」是安全的，
    /// **不需要**额外的等待接口。
    pub fn current_slot(&self) -> Option<usize>;
    /// **当前帧命令缓冲**（未 acquire 时 None）。纹理上传录进同一份submit。
    /// ⚠️ 句柄由帧层拥有：**不要**自行end_command_buffer 或销毁，
    /// `present()` 负责收尾。
    pub fn current_command_buffer(&self) -> Option<vk::CommandBuffer>;
    /// 取某槽位的描述符集
    pub fn descriptor_set(&self, slot: usize) -> anyhow::Result<vk::DescriptorSet>;
    /// 写 uniform 绑定（取 A 的 `Buffer::descriptor_info()` 三个字段）
    pub fn update_uniform_binding(&self, slot: usize, buffer: vk::Buffer,
        offset: vk::DeviceSize, range: vk::DeviceSize) -> anyhow::Result<()>;
    /// 写采样器 + 纹理绑定（取 A 的 `FontTexture::descriptor_info()` 三个字段）
    pub fn update_texture_binding(&self, slot: usize, sampler: vk::Sampler,
        image_view: vk::ImageView, layout: vk::ImageLayout) -> anyhow::Result<()>;
    /// 当前尺寸 / 格式 / 清屏色
    pub fn extent(&self) -> vk::Extent2D;
    pub fn format(&self) -> vk::Format;
    pub fn set_clear_color(&mut self, rgba: [f32; 4]);
    /// 本次 acquire 是否 suboptimal（能画，但表面已不匹配）
    pub fn last_acquire_was_suboptimal(&self) -> bool;
}

pub struct DrawInput<'a> {
    pub vertex_buffer: vk::Buffer,
    pub index_buffer: vk::Buffer,
    pub vertex_offset: vk::DeviceSize,
    pub index_offset: vk::DeviceSize,
    /// 默认 UINT32（**不是** vk::IndexType::default()，那是 UINT16）
    pub index_type: vk::IndexType,
    pub batches: &'a [DrawBatch],
    /// null = 用该帧槽自带的描述符集
    pub descriptor_set: vk::DescriptorSet,
}
pub struct DrawBatch { pub index_offset: u32, pub index_count: u32 }
```

### 一帧的标准写法（D 组照此集成）

```rust
// 1) acquire，拿不到图像就必须重建
let Some(frame) = fr.acquire()? else {
    fr.rebuild_swapchain(Default::default())?;   // 必须重建，否则死循环
    continue;
};

// 2) 纹理上传 + 描述符：录进同一份 submit，不额外同步
if let Some(cmd) = fr.current_command_buffer() {
    for delta in &deltas {
        textures.apply_delta(&gpu, cmd, delta)?;      // A 的 API
    }
    for slot in 0..fr.slot_count() {                  // ⚠️ 必须写遍所有槽位
        let u = ub.descriptor_info();
        fr.update_uniform_binding(slot, u.buffer, u.offset, u.range)?;
        if let Some(t) = textures.font_descriptor_info() {
            fr.update_texture_binding(slot, t.sampler, t.image_view, t.image_layout)?;
        }
    }
}

// 3) 录制绘制（input 用 buffer::Vertex 填充）
fr.record(&input)?;

// 4) 呈现
if fr.present(frame)? == PresentResult::Outdated {
    fr.rebuild_swapchain(Default::default())?;
}
```

### A / D 组对接注意

1. **描述符集是每槽位一份**，数量 == 交换链图像数。`update_*_binding`
   必须**对每个槽位都写一遍**（`for slot in 0..fr.slot_count()`）——
   GPU 可能还在读其它槽位的描述符集，只写当前帧会让其它帧读到未初始化绑定。
2. `FrameRenderer` **不 import A 的类型**，只收裸 `vk::Buffer` /
   `vk::ImageView` / `vk::Sampler`，两组可并行开发，无需等编译顺序。
3. 顶点用 **`buffer::Vertex`**（20 字节，偏移 0/8/16），索引用
   **`buffer::INDEX_TYPE`**（UINT32）。`frame.rs` 不定义任何顶点类型。
4. ⚠️ **`vk::IndexType::default()` 是 UINT16**，不是 UINT32。
   `DrawInput::default()` 已钉死为 UINT32，但手动构造 `DrawInput` 时
   **不要覆盖该字段**——类型错不崩，只会静默画出乱图。
5. `current_command_buffer()` 的句柄**不要**自行 `end_command_buffer` 或销毁。


> ⚠️ 此契约由 B 定义。**A、C、D 不要修改 frame.rs**；若发现契约不足，
> 写进下方「待决」区，由我（B 组）裁决。

## 关键约束（所有人必须遵守）

1. **ash 0.38 API 与网上示例差异极大**。已踩 26 个坑，全部记录在
   `.workbuddy/memory/MEMORY.md`。写代码前**必须**先读该文件的相关章节。
   典型陷阱：枚举用 SCREAMING_SNAKE、构造器是 slice 型、
   `cmd_pipeline_barrier` 不收 `SubpassDependency`、`allocate_command_buffers`
   必须显式设 `command_buffer_count`。
2. **本机 MSVC 在 `D:\Program Files\...`（非常规路径）**。
   编译必须用 `./scripts/build.sh`，**不要直接调 cargo**（会缺 cl.exe 环境）。
3. **新增代码零警告**。`./scripts/build.sh` 的 warning 必须为 0。
4. **每个模块都要有单测**，且必须真实通过（`./scripts/build.sh --test`）。
   不接受「编译通过」当测试通过。
5. 出现 GPU/运行时错误时**必须实机验证**，不能只靠编译通过就宣称完成。

## 文件所有权（越界修改 = 退回）

```
crates/tiez-gfx/
  src/lib.rs         ← 共同依赖，任何人改都要先在「待决」区登记
  src/pipeline.rs    ← 冻结（上一轮已完成）
  src/shader.rs      ← 冻结（上一轮已完成）
  src/buffer.rs      ← A 独占
  src/texture.rs     ← A 独占
  src/frame.rs       ← B 独占
  src/window.rs      ← C 独占
  shaders/egui.wgsl  ← 冻结
  build.rs           ← 冻结
crates/tiez-ui/
  src/*              ← D 独占
```

## 实机验证基线（务必保持不回归）

`cargo run -p tiez-gfx --example pipeline_probe` 退出码 0，即：
Win32 窗口 → Vulkan 表面 → 交换链 → 渲染通道 → SPIR-V 着色器 →
图形管线 → 命令录制 → queue_submit → device_wait_idle 无 GPU 错误。

**任何人改渲染相关代码后必须重跑此探针。**

## 待决（需我裁决）

| 提出方 | 问题 | 状态 |
|--------|------|------|
| C | `window.rs` 需要 egui 依赖（把Win32 事件译为 `egui::RawInput`） | ✅ 已裁决：`tiez-gfx` 加 `egui` workspace 依赖。理由：这是渲染层与UI 层唯一耦合点，放在 gfx 侧可避免 `tiez-gfx → tiez-ui` 的循环依赖 |
| C | 需要 windows features：`Win32_UI_Input_KeyboardAndMouse`、`Win32_UI_HiDpi` | ✅ 已加入 Cargo.toml |
| A/B | 需要在 `lib.rs` 注册 `mod buffer; mod texture;` / `mod frame;` | ✅ 已由 B 代为添加 `pub mod frame;`（`buffer`/`window` 已由 A/C 自行注册）。<br>⚠️ 期间因多人并发编辑出现过 `pub mod buffer;` 重复声明，已删除多余行。**后续请勿再并发改 lib.rs** |
| B | `frame.rs` 的 `pick_swapchain_format` 是 `lib.rs::pick_format` 的副本（后者私有）。改选格式逻辑时两处会静默失配 → 「渲染通道格式 ≠ 交换链格式」，驱动在 `cmd_begin_render_pass` 时炸。 | ✅ 已解决：主控把 `pick_format` 改为 `pub(crate)` 统一实现，`frame.rs` 删除副本直接调用，**单一数据源**。迁移时漏改`rebuild_swapchain` 的一处调用（`pick_format` 返回元组），已由 B 修|
| A |⚠️ staging 生命周期依赖「每帧 device_wait_idle」 | ✅ **B 已确认：否**。帧层是**多帧在途**（按槽位轮转+ 每槽位独立 fence），**不做**每帧 idle。 |
| A | ⚠️ **staging 泄漏的定性有误，需纠正** |✅ **B 已核实并纠正**：`Buffer` 无 `Drop` ⇒ staging 的 `vk::Buffer`/`vk::DeviceMemory` **从未被销毁**⇒ 是**良性内存泄漏**（句柄始终有效、GPU 读到的数据正确、`write()` 内部已 `map`→`unmap` 故无 use-after-free），**不是** A 担心的 use-after-free。**真实风险是泄漏量**：`apply_delta` 每delta 调一次，中文首载逐字形上传≈上千次 ⇒ 上千块 staging 泄漏。**修法应为「常驻 staging 字段」而非 ring buffer**（ring buffer 是为解决复用覆写，而此处根本无复用）。**→ A 行动项** |
| A | 描述符集归属 | ✅ **B 已实现**：`FrameRenderer` 持DescriptorPool + 每槽位一份描述符集。⚠️ **必须对 `0..slot_count()` 每个槽位都写一遍** |
| A | 建议 B 直接用 `buffer::Vertex` |✅ B 本就未定义任何顶点类型（`DrawInput` 收裸 `vk::Buffer`）。约定：顶点用 `buffer::Vertex`、索引用 `buffer::INDEX_TYPE`（UINT32），写入 PROGRESS.md 契约区 |
| A | 图集扩容时 `wait_idle` 是否改为延迟释放 | ✅ **B 裁决：保留 `wait_idle`，不要改**。中文字体首载必然触发一次，而那本就是启动阶段（用户还在等窗口），停顿感知≈0；改延迟释放会让显存峰值翻倍并需帧层配合（要知道几帧后GPU 读完），复杂度与出错面不划算。典型过早优化 |
| B（我） | ⚠️ **自查发现契约有洞并已修补** | ✅ A的 `upload(gpu, cmd: vk::CommandBuffer, ..)` 需要命令缓冲，但我的契约**从未暴露当前帧命令缓冲** ⇒ D 组无法把上传录进同一份提交，等于无法集成。已新增 `current_slot()` 与 `current_command_buffer()`，并补齐 PROGRESS.md 的一帧示例 |

### ⚠️ 主控澄清一处（避免 B 组误解）

B 组在实现说明里提到「present 的 Outdated 信号**只能**来自 `pResults`」。**这个判断不对**，我已实测ash 0.38 源码确认：

```rust
// ash-0.38/src/extensions/khr/swapchain.rs
pub unsafe fn queue_present(&self, queue, present_info) -> VkResult<bool> {
    match err_code {
        vk::Result::SUCCESS => Ok(false),
        vk::Result::SUBOPTIMAL_KHR => Ok(true),   // 整体 suboptimal
        // 关键：整体 OUT_OF_DATE 被直接吞掉，只从 pResults 反映
    }
}
```

准确表述应为：
- `queue_present` 的**整体** `Err` 只可能是 `ERROR_OUT_OF_DATE_KHR` 或
  `ERROR_SURFACE_LOST_KHR` 等，**`OUT_OF_DATE` 不会出现在整体返回值里**；
- 单个交换链的 `OUT_OF_DATE` 通过 `pResults[i]` 反映；
- `SUBOPTIMAL` 既在整体 `Ok(true)` 里，也在 `pResults[i]` 里。

B 组实现里若已按「只能来自 pResults」处理，**逻辑仍然正确**
（检查 pResults 确实能捕获 Outdated），仅表述需修正。`pResults` 数组
长度必须**严格等于**交换链 image 数量，否则驱动会越界写。

## 进展日志（倒序追加）

<!-- 每完成一个可验证步骤就追加一条，格式：时间 | Agent | 做了什么 | 如何验证的 -->

- 2026-10-05 | B | **自查契约漏洞并补齐**（起因是 A 组提问暴露了缺口）：A 的 `DeviceImage::upload(gpu, cmd, ..)` 需要一个 `vk::CommandBuffer` 录制拷贝命令，而我的契约**从未暴露当前帧命令缓冲** ⇒ D 组拿到 `FrameRenderer` **无法把纹理上传录进同一份 submit**。新增 `current_slot()` / `current_command_buffer()`，并补齐 PROGRESS.md 的完整一帧示例（含 `apply_delta` 与描述符写入）。同时**主动砍掉自己刚写的多余方法** `wait_slot_idle`—— 追溯槽位轮转后发现 `acquire()` 已等过并重置当前槽位栅栏，该方法冗余。 | `./scripts/build.sh --test` **零错误零警告**，tiez-gfx **58 passed / 0 failed**；实机探针 `pipeline_probe` 退出码 0 / `ALL OK`，不回归 |
- 2026-10-05 | B | **纠正了 A 组对 staging 的错误定性**。A 推断「`Buffer` 无 `Drop` ⇒ 绑定已失效 ⇒ use-after-free」并据此准备改 ring buffer。实查确认：**没有 `Drop` 意味着什么都不发生**——`vk::Buffer`/`vk::DeviceMemory` 从未被销毁，句柄始终有效；且 `Buffer::write` 内部 `map`→`copy`→**`unmap`**（`buffer.rs:301-345`），不存在「已 unmap 裸指针复用」。故真实行为是**良性内存泄漏**（GPU 读到的数据正确），**ring buffer 是错误修法**（它解决「复用覆写未读数据」，而此处根本无复用，每次新分配）。正确修法：把 staging 提升为常驻字段。真实风险在泄漏量——`apply_delta` 每 delta 一次（`texture.rs:725`），中文首载逐字形≈上千次。 | 直读 `buffer.rs:99-111`（`Buffer` 无 `Drop`）、`buffer.rs:301-345`（`write` 内部 `unmap`）、`buffer.rs:460`（`Drop` 仅在 `BufferGuard`）、`texture.rs:269-296`（`upload` 新建局部 staging）、`texture.rs:725`（每 delta 一次） |
- 2026-10-05 | A | **staging 机制统一到 arena（应B 组要求，删除常驻 staging）**：`DeviceImage` 的 `staging: Option<Buffer>` 字段与 `staging_buffer()` 方法**已彻底删除**（grep 确认 0 命中）。`upload` 改为只从参数接收 `StagingSlice`、只录制命令；`TextureStore::apply` / `FontTexture::apply_delta` 改为接收 `&mut StagingArena` 并自行 `allocate` + `write`。**签名与 B 的 `record_texture_upload` 对齐。**⚠️ 顺带确认了 team-lead 指出的**真实缺陷**（我原方案确实没考虑到）：同一帧内连续两次 `upload` 同一张图时，第一次的 `cmd_copy_buffer_to_image` 可能仍在读 staging——**这才是真正的 use-after-free，且会静默画出错误内容**。常驻方案无帧栅栏，判断不了「上次 GPU 读完没有」；arena 的 `in_flight` + 退休机制才解决得了。裁决正确。 | `./scripts/build.sh --test`：**16 个测试二进制全ok，零 FAILED**；tiez-gfx **60+4+1 测试全通过**；我的两个文件**零 error 零 warning**（仅剩2 个 warning 在 B 的 `frame.rs`：`ArenaDecider` 未使用，B 组 WIP）。实机 `pipeline_probe` **退出码 0 / ALL OK / 无回归**。
- 2026-10-05 | A | **修复 staging 内存泄漏（采纳 B 的方案 A）**：`DeviceImage` 新增常驻 `staging: Buffer` 字段，`upload` 借用它、容量不足才重建，`destroy` 一并释放。**不需要槽位参数**，B 的 ring buffer 设想可放弃。按整图尺寸分配（而非按 `needed` 精确分配），这样局部更新与整图更新的容量决策恒等 ⇒ 复用判定恒为真 ⇒ 只分配一次；若按 `needed` 精确分配，小块更新后跟大块更新会反复重建，而 `Buffer` 只能整体 `destroy`，旧的会泄漏。⚠️ 同时更正我此前的**错误定性**：原判断「无 `Drop` ⇒ 绑定已失效 ⇒ use-after-free」是错的——无 `Drop` 意味着句柄**从未被销毁**、始终有效，GPU 读到的数据正确；真实问题**只是泄漏量**。感谢 B 纠正。 | 隔离 `frame.rs`（B 组WIP）后 `./scripts/build.sh --test`：**16 个测试二进制全 ok，零 error 零 warning**，tiez-gfx 累计 **38 个测试**（36 + 新增 2 个 staging 复用不变式单测）
- 2026-10-05 | A | **全量验证通过**（B/C 修完各自文件后）：`./scripts/build.sh --test` **16 个测试二进制全ok，零 error 零 warning**；tiez-gfx 累计 **58 个测试**（原 5 + A 的 31 + B/C 其余）。实机探针 `pipeline_probe` **退出码 0**，输出 `ALL OK - 渲染器核心链路全部工作`，**无 GPU 错误、无回归**。 | build.sh --test + pipeline_probe 实机 |
- 2026-10-05 | A | 完成 `buffer.rs`(796 行) + `texture.rs`(897 行)。buffer侧：纯函数 `find_memory_type`（可注入伪造 `PhysicalDeviceMemoryProperties`）、`align_up`、创建失败的 `BufferGuard`/`ImageGuard` 清理守卫、`VertexBuffer`/`IndexBuffer`/`UniformBuffer`。texture 侧：`R8_UNORM` + `NEAREST` 采样器、`TextureStore` 按 `TextureId` 增删改、图集扩容自动重建（重建前 `wait_idle`）。**发现并规避一个会abort 的陷阱**：`vkGetBufferDeviceAddress` 是 1.2 核心函数而本项目只申请 1.1 基线，ash 取不到指针时装的是 **panic 桩**，`device_address()` 已加usage + API 版本双重前置校验。 | 见下方全量验证条 |

- 2026-10-05 | A | 纠正了自己的一个错误设计：`DeviceImage::upload` 最初把 `image_extent` 固定成整图尺寸，这对**局部更新是错的**——`image_extent` 必须是*被写入矩形*的尺寸，否则 Vulkan 对超出图像部分按未定义行为处理。已改为显式传 `patch: (u32,u32)`，并加 `validate_region` 在录制前拦截越界（含 u32 溢出，用 u64 算）。 | 单测 `region_validation_uses_wide_arithmetic` / `buffer_image_copy_honors_offset` |
- 2026-10-05 | A | 写测试时发现自己的测试**与自己的设计相矛盾**：`falls_back_when_device_local_absent` 断言 `required=DEVICE_LOCAL` 在无显存时返回某类型，但 `find_memory_type` 的设计是**宁可失败也不悄悄退回共享内存**。已把测试拆成两个（`empty_required_falls_back_to_shared_memory` 与 `unsatisfiable_required_returns_none`），让测试反映真实契约。 | 该测试由 FAILED 转 ok |
- 2026-10-05 | A | 查证 **egui 0.36 字体图集已不是单通道**：`ImageData` 枚举**只有** `Color(Arc<ColorImage>)` 一个变体（无 `Font` 变体），且`color_from_coverage` 用 `Color32::from_white_alpha` 把同一 alpha 写进 4 个通道。故取红通道即得覆盖率，与着色器 `textureSample(...).r` 一致，且体积省到 1/4。任务书里「egui 字体图集是单通道覆盖率」的表述在 0.36 已不准确。 | 直读 `epaint-0.36.2/src/image.rs` 与 `texture_atlas.rs`；单测 `red_channel_equals_alpha_for_font_atlas` |
- 2026-10-05 | B | 实现 `frame.rs`：`PipelineBundle` / `AcquiredFrame` / `PresentResult` /
  `LayoutTracker` / `FrameRenderer`（acquire→record→present + rebuild_swapchain）。
  20 个纯逻辑单测 + 1 个 doctest。 | 隔离环境 `cargo test`：`35 passed; 0 failed`，
  doctest ok，**零警告**。后续 A、C 修完后全量已转绿，见下方主控 23:15 记录。 |
- 2026-10-05 | B | **自测抓到我自己代码里的一个真bug**：`DrawInput` 原本用
  `#[derive(Default)]`，而 `vk::IndexType::default()` 是 **UINT16**，与文档
  声称的 UINT32 不符。索引类型错不崩、只会静默画出乱图。已改为手写
  `impl Default` 把 `index_type` 钉死为 UINT32。 | 单测
  `draw_input_helper_uses_uint32_indices_by_default` 由 FAILED 转 ok |
- 2026-10-05 | B | 核实 ash 0.38 真实签名，发现**与任务书给的示例不一致**，已在代码注释标注：<br>① `acquire_next_image` 返回 `Result<(u32, bool), vk::Result>`，**不是** `Result<AcquireResult, _>`；<br>② `vk::Result` **没有** `OUTDATED_KHR`/`SUBOPTIMAL_KHR` 常量（只有 Vulkan 1.0 核心那批），只能按 `from_raw(±1000001003/1000001004)` 比较；<br>③ 描述符写入方法叫 `update_descriptor_sets`（不是 `write_descriptor_sets`）；<br>④ `is_null()` 来自 `ash::vk::Handle` trait，需显式 import。 | 直读
  `~/.cargo/registry/src/*/ash-0.38.0+1.3.281/src/extensions/khr/swapchain.rs`
  与 `src/vk/enums.rs` |

| 时间 | Agent | 做了什么 | 如何验证的 |
|------|-------|---------|-----------|
| 22:31 | C | 交付 `window.rs` 1296 行 + 39 单测。Win32 窗口、事件翻译、VK→egui::Key 映射、UTF-16 代理对解码、DPI 感知 | 39 单测通过；实机可创建窗口 |
| 22:45 | B | 交付 `frame.rs` 1362 行 + 22 单测。帧槽位池、`LayoutTracker`（每张图像独立跟踪布局）、`resolve_extent` 夹取、swapchain 重建 | 22 单测通过 |
| 23:05 | A | 交付 `buffer.rs` 713 行 + `texture.rs` 812 行 + 22 单测。内存类型选择（含`align_mask` 校验）、描述符集、`FontTexture`（`R8_UNORM` + `NEAREST` 采样） | 22 单测通过 |
| 23:10 | 主控 | 统一 `pick_format` 到 `lib.rs`（消除 frame.rs 副本），修 8 处ash 0.38 编译错误，注册 5 个模块 | 编译零警告 |
| 23:15 | 主控 | 全量验证 | **136 测试通过 / 0 失败 / 0 警告；实机探针退出码 0** |
| 23:20 | B | 补修主控统一 `pick_format` 时漏改的一处调用点：`rebuild_swapchain` 里 `crate::pick_format(..)` 现返回 `(Format, ColorSpaceKHR)` 元组，原代码按单个 `Format` 比较 → E0308。改为 `let (format, _) = ..`。同时回答 A 的两个待确认项（见待决区）。 | 全量 58 passed（tiez-gfx）+ 探针退出码 0 |


---

##⚠️ 主控裁决（23:40）：staging buffer 生命周期

**定性**：`DeviceImage::upload()` 每次自建局部 staging buffer，函数返回即
`destroy_buffer`，而 GPU 可能还在异步读它→ **真实 use-after-free**。
当前没崩只因尚无真实调用路径（单测不跑 GPU）。D 组接入 UI 就会踩到。

**裁决**：由 E 组在帧层实现 per-frame `StagingArena`（Vulkan 标准做法），
`upload` / `create_buffer` 改为接收外部 `StagingSlice`。
不采用「upload 内device_wait_idle」——那会把每帧强制同步，
抵消 Vulkan 的异步优势。

**D 组必须知道的两个约束**（集成时务必遵守）：

1. `upload` 不再自己分配 staging，必须经 `FrameRenderer::record_texture_upload`
   走 arena。
2. 帧槽位在 GPU 未完成前会被标记 `in_flight`，此时再次
   `record_texture_upload` 会**返回错误而非静默重复录制**。
   D 组每帧需先 `pump` 确认槽位可用再录制。

**另一个已确认的坑**：`vk::IndexType::default()` 是 `UINT16`。
超过 65535 顶点会静默画错（不报错、画面乱）。
凡索引缓冲一律显式用 `vk::IndexType::UINT32`。


## ⚠️ 主控裁决（00:10）：A 的 0.1.2 关联不是 use-after-free

A 组报告 `texture.rs:390` 存在 use-after-free，理由是
「slot`[i].staging: Option<Buffer>` 被 clear，Binding 仍指向它」。
**该判断不成立**，B 组已独立核实并给出正确事实，我确认如下：

- `Buffer` **没有** `impl Drop`（主控已grep 确认，全文只有 `BufferGuard` 有 Drop）。
  没有 Drop ⇒ `destroy_buffer` 从不被调用 ⇒ 句柄**一直有效** ⇒ 绑定不会悬空。
- `Buffer::write`内部是 `map → ptr::copy → unmap`，`unmap` 不销毁绑定。

因此真实问题只是**内存泄漏**（每帧创建新 Buffer 而永不释放），
不是 use-after-free。**不会崩溃，但帧数越多泄漏越多。**

**裁决：采纳 B 组方案。**
- 真正的修复是**复用而非销毁**——这也是 arena 的本来设计：
  in_flight 帧数内继续复用同一块 Buffer，退休后才reset。
  A 当前的「每帧新建 + 每帧 clear」既漏内存又多余分配。
- A 提的 ring buffer 方案在语义上是对的（正好是 arena 该做的事），
  但请注意：**不要因为这个 0.1.2 而去做「修复悬空绑定」的防御性代码**——
  那会基于一个不成立的威胁，加出无意义的复杂度。

**给 A 的具体要求**：
1. `upload` 不要每帧`Buffer::new`。改为向 `StagingArena` 申请空间
   （arena 内部持有长期存活的 Buffer，用 offset 区分）。
2. 删除「`clear` 之前必须重置绑定」这类防御性检查——前提不成立。
3. 泄漏的验证方式：`upload_probe` 跑 2000 帧后
   `vkGetResourceInfo` 或直接对比内存占用，应保持平稳而非线性增长。
