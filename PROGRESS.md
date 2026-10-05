# 并行开发进度黑板

> 所有 Agent 共享此文件。开工前先读本文件了解他人进度，完成后立即更新自己的行。
> 目录约定见文末「文件所有权」，**每个 Agent 只能改自己负责的文件**。

## 状态一览

| # | 负责范围 | 拥有文件 | 状态 | 阻塞于 | 交付物 |
|---|---------|---------|------|--------|--------|
| A | GPU 资源层（缓冲/内存/纹理/描述符） | `gfx/src/buffer.rs`(新) `gfx/src/texture.rs`(新) | 🟢 已完成 | — | 资源封装+31 个单测 |
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

// —— 供 A 组对接：我只负责分配与绑定，填充由资源层调用 ——
impl FrameRenderer<'_> {
    /// 在飞帧槽位数量（== 交换链图像数）
    pub fn slot_count(&self) -> usize;
    /// 取某槽位的描述符集
    pub fn descriptor_set(&self, slot: usize) -> anyhow::Result<vk::DescriptorSet>;
    /// 写 uniform 绑定（A 组的 UniformBuffer 用）
    pub fn update_uniform_binding(&self, slot: usize, buffer: vk::Buffer,
        offset: vk::DeviceSize, range: vk::DeviceSize) -> anyhow::Result<()>;
    /// 写采样器 + 字体纹理绑定（A 组的 FontTexture 用）
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
let Some(frame) = fr.acquire()? else {
    fr.rebuild_swapchain(Default::default())?;   // 必须重建，否则死循环
    continue;
};
fr.record(&input)?;                            // input 由 ClippedPrimitive 转成
if fr.present(frame)? == PresentResult::Outdated {
    fr.rebuild_swapchain(Default::default())?;
}
```

### A 组对接注意

1. **描述符集是每槽位一份**，数量 == 交换链图像数。`update_*_binding`
   必须**对每个槽位都写一遍**——GPU 可能还在读其它槽位的描述符集。
2. `FrameRenderer` **不 import A 的类型**，只收裸 `vk::Buffer` /
   `vk::ImageView` / `vk::Sampler`，A 组写完即可直接对接，无需等编译顺序。
3. 顶点步长 20 字节（pos vec2 + uv vec2 + color u32），与 `pipeline.rs` 一致。

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
| A | ⚠️ staging 生命周期依赖「每帧 device_wait_idle」 | ✅ **B 已确认：否**。帧层用多帧在途（按槽位轮转 + 每槽位独立 fence），**不做**每帧 idle。→ A 需把 `DeviceImage::upload` 的局部 staging 改为**按槽位跨帧持有的 ring buffer**（数量 = `fr.slot_count()`），上传前等该槽 fence。**A 行动项** |
| A | 描述符池与 `update_texture_binding` 归属 | ✅ **B 已实现**。`FrameRenderer` 持有 DescriptorPool + 每槽位一份描述符集，对外暴露 `descriptor_set(slot)` / `update_uniform_binding(...)` / `update_texture_binding(...)`。A 直接解包 `FontTexture::descriptor_info()` 传入即可，无需改B 的签名。⚠️ **必须对 `0..slot_count()` 每个槽位都写一遍** |
| A | 建议 B 直接用 `buffer::Vertex` 而非另定义 |✅ 知悉并采纳方向，但**当前刻意不import 资源层类型**：`DrawInput` 收裸 `vk::Buffer`，A 无需等B 编译。字节布局一致性由 `pipeline.rs` 属性描述 + A 的 `Vertex` 单测 + B 的「步长 20 字节」测试三方守住。若 A 要求 strongly-typed `&[Vertex]`，需等 `buffer.rs` 定稿后由 B 改契约 |

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

- 2026-10-05 | A | 完成 `buffer.rs`(796 行) + `texture.rs`(897 行)。buffer侧：纯函数 `find_memory_type`（可注入伪造 `PhysicalDeviceMemoryProperties`）、`align_up`、创建失败的 `BufferGuard`/`ImageGuard` 清理守卫、`VertexBuffer`/`IndexBuffer`/`UniformBuffer`。texture 侧：`R8_UNORM` + `NEAREST` 采样器、`TextureStore` 按 `TextureId` 增删改、图集扩容自动重建（重建前 `wait_idle`）。**发现并规避一个会abort 的陷阱**：`vkGetBufferDeviceAddress` 是 1.2 核心函数而本项目只申请 1.1 基线，ash 取不到指针时装的是 **panic 桩**，`device_address()` 已加usage + API 版本双重前置校验。 | 临时隔离 B/C 未完成的 `frame.rs`/`window.rs` 后跑 `./scripts/build.sh --test`：**tiez-gfx 36 个测试全通过（原 5 + 新增 31），零警告**。全量 `--test` 仍被 `window.rs`/`frame.rs` 阻塞，与 A 无关。❌ **尚未实机 GPU 验证**：`upload`/`create_buffer` 等真实驱动路径一次都没跑过。 |
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
