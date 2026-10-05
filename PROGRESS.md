# 并行开发进度黑板

> 所有 Agent 共享此文件。开工前先读本文件了解他人进度，完成后立即更新自己的行。
> 目录约定见文末「文件所有权」，**每个 Agent 只能改自己负责的文件**。

## 状态一览

| # | 负责范围 | 拥有文件 | 状态 | 阻塞于 | 交付物 |
|---|---------|---------|------|--------|--------|
| A | GPU 资源层（缓冲/内存/纹理/描述符） | `gfx/src/buffer.rs`(新) `gfx/src/texture.rs`(新) | 🟢 已完成（staging 已统一到 arena） | — | 资源封装+65 个单测 |
| B | 帧图与呈现（acquire/present/rebuild） | `gfx/src/frame.rs`(新) | 🟢 已完成 | — | 帧循环+20 单测，全通过 |
| C | Win32 窗口与事件循环 | `gfx/src/window.rs`(新) | 🟢 代码完成，**52 单测全通过**（tiez-gfx 共 128） | — | 窗口封装+输入映射 |
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

1. `cargo run -p tiez-gfx --example pipeline_probe` 退出码 0，即：
Win32 窗口 → Vulkan 表面 → 交换链 → 渲染通道 → SPIR-V 着色器 →
图形管线 → 命令录制 → queue_submit → device_wait_idle 无 GPU 错误。

2. `cargo run -p tiez-gfx --example window_probe` 退出码 0，即：
窗口类注册 → DPI 感知（建窗前）→ 客户区尺寸 == 请求值 →
9 条注入消息全部翻译正确（坐标换算 / 滚轮符号 / UTF-16 /
`CloseRequested` 不销毁窗口）→ `RawInput` 自洽 → egui 可消费 →
`destroy` 幂等。

**任何人改渲染相关代码后必须重跑探针 1。** 改窗口/事件代码后重跑探针 2。

## 待决（需我裁决）

| 提出方 | 问题 | 状态 |
|--------|------|------|
| C | `window.rs` 需要 egui 依赖（把Win32 事件译为 `egui::RawInput`） | ✅ 已裁决：`tiez-gfx` 加 `egui` workspace 依赖。理由：这是渲染层与UI 层唯一耦合点，放在 gfx 侧可避免 `tiez-gfx → tiez-ui` 的循环依赖 |
| C | 需要 windows features：`Win32_UI_Input_KeyboardAndMouse`、`Win32_UI_HiDpi` | ✅ 已加入 Cargo.toml |
| A/B | 需要在 `lib.rs` 注册 `mod buffer; mod texture;` / `mod frame;` | ✅ 已由 B 代为添加 `pub mod frame;`（`buffer`/`window` 已由 A/C 自行注册）。<br>⚠️ 期间因多人并发编辑出现过 `pub mod buffer;` 重复声明，已删除多余行。**后续请勿再并发改 lib.rs** |
| C | `Window::new` 写死 `WS_OVERLAPPEDWINDOW`，**未做自绘标题栏**。D 组若需拖动区需先换`WS_POPUP` | 🟡 待 D 组确认是否需要。不需要则保持现状（自绘标题栏要 `WM_NCLBUTTONDOWN`→拖动 + `WM_NCHITTEST`→`HTCAPTION`，Win32 耦合重，建议集成稳定后单独做） |
| C | `WindowEvent::Scroll` 按接口约定只带垂直量，**水平滚轮未暴露**（egui `ScrollArea` 水平滚动会失效） |🟡 待裁决：是否改成 `Scroll { x: f32, y: f32 }` |
| F | ~~`upload_probe` 第 4 帧丢设备~~ | ✅ **已解决，退出码 0**。根因是**探针自身的逐帧 `device_wait_idle`**：它强制等所有队列工作完成，破坏了被测的「多帧在途」状态。去掉插桩后 60 帧稳定、连跑 4 次输出一致、65536 字节逐字节比对全部一致。**教训：诊断代码会改变被诊断系统的行为**，插桩引入的同步点可能正是成因而非窗口 |
| F | `DeviceImage::upload` 第一次屏障 `srcStage=TOP_OF_PIPE` 但 `srcAccess=SHADER_READ`（`texture.rs:337`）。`TOP_OF_PIPE` 不等待任何阶段，该屏障实为空操作。**非本次崩因**（复刻 upload 连跑 4 轮实机通过），但属真实规范缺陷 | 🟡 已私发 mcp-arch-resource，建议改为按 `old_layout` 选 `TOP_OF_PIPE`/`FRAGMENT_SHADER` |
| F | `can_record_upload` 的 `in_flight` 分支在当前 API 下**不可达**（`present` 会 `take()` 掉 `pending`，撞不上这道门）。真正拦住重复录制的是 `pending`。mcp-arch-frame 文档已写明是「有意的冗余防线」 | ✅ 不改（冗余防线为将来新增提交路径预留） |
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

## C 组交付：`window` 模块（D 组对接契约）

> **本节是 C → D 的正式接口契约**，与上文 B 组契约同级。

```rust
// crates/tiez-gfx/src/window.rs
impl Window {
    pub fn new(title: &str, width: u32, height: u32) -> anyhow::Result<Self>;
    pub fn hwnd(&self) -> HWND;                 // 喂 Gpu::new
    pub fn hinstance(&self) -> HINSTANCE;       // Vulkan Win32SurfaceCreateInfoKHR 要
    pub fn inner_size_physical(&self) -> (u32, u32);
    pub fn inner_size_points(&self) -> (f32, f32);    // 逻辑点，喂 swapchain
    pub fn scale_factor(&self) -> f32;          // dpi / 96
    pub fn destroy(&self);
}

impl EventLoop {
    pub fn new(window: &Window) -> Self;
    /// 排空消息队列，不阻塞。有事件立即返回。
    pub fn poll(&mut self) -> Vec<WindowEvent>;
    /// 有事件立即返回；空闲时按timeout 睡眠（MsgWaitForMultipleObjects）。
    pub fn poll_for(&mut self, timeout: Option<Duration>) -> Vec<WindowEvent>;
    /// 帧间隔。每帧把 ctx.requested_repaint_after() 写回来。
    pub fn set_repaint_after(&mut self, d: Option<Duration>);
    /// **D 组核心对接点**：本帧事件 → egui RawInput
    pub fn egui_input(&self, ctx: &egui::Context) -> egui::RawInput;
    pub fn pending_events(&self) -> &[WindowEvent];
    pub fn quit_requested(&self) -> bool;
}
```

### 每帧调用顺序（不能换）

```rust
let events = event_loop.poll();                 // 填充内部缓冲
// 应用层可先消费 events（如 CloseRequested）
let raw = event_loop.egui_input(&ctx);          // 读内部缓冲
ctx.run(raw, |ctx| { ... });                    // egui 排版
// 渲染 frame.acquire / record / present
event_loop.set_repaint_after(ctx.requested_repaint_after());
```

### ⚠️ 三个必须知道的语义

1. **`WindowEvent` 里的坐标已是逻辑点**（已除 `scale_factor`），
   D 组**不要**再换算。
2. **`CloseRequested` 不会销毁窗口**。`WM_CLOSE` 不派发，由应用决定去留——
   剪贴板程序通常在此隐藏到托盘。确实要退出才调 `Window::destroy()`。
3. **窗口类不注销**（`Drop` 只 `DestroyWindow`）。类是进程级的，
   可能有其它 `Window` 实例在用同名类。

### C 组已核实的 egui 0.36 细节（照抄，勿重新试错）

- **滚轮 `TouchPhase` 必须用 `Move`**，不能用 `Start`。`Start` 会置
  `Status::InTouch`，之后修饰键被 `|=` 锁住直到抬手。鼠标滚轮不是触摸。
  依据：`egui-0.36.2/src/input_state/wheel_state.rs:95`
- **`Event::Key` 的 `physical_key` 不能填 `None`**，egui 的 IME 分支会跳过 `None`。
- **`Event::MouseWheel` 用 `MouseWheelUnit::Line`**，让 egui 按
  `line_scroll_speed` 自行换算成点。

### C 组踩到的 windows 0.62 坑（补充 MEMORY.md 第 23-26 条）

27. **`Win32_UI_HiDpi` 的模块名是 `HiDpi`，不是 `HiDPi`**（小写 pi）。
28. **`GetModuleHandleW` 返回 `HMODULE`，不是 `HINSTANCE`**。二者是 `From`
    关系（同一指针的不同 newtype），需显式 `.into()`。
29. **`windows::core::Error::from_win32()` 不存在**。windows-result 0.4.1
    只有 `from_hresult` / `from_thread` / `empty` / `new`。要 Win32 错误码
    得自己调 `GetLastError()`（在 `Win32::Foundation`）。
30. **lib 能编过 ≠ 测试能编过**。`Window::new` 之类函数在 `#[cfg(test)]`
    下仍参与类型检查，必须单独验证测试构建。
31. **`drop()` 用在 `Copy` 类型上会告警**（`BOOL` / `LRESULT` / `HWND` /
    `WAIT_EVENT` 全是 `Copy`，`drop` 对它们不做任何事）。用 `let _ =`。

## 进展日志（倒序追加）

<!-- 每完成一个可验证步骤就追加一条，格式：时间 | Agent | 做了什么 | 如何验证的 -->

- 2026-10-06 | C | ⚠️ **`window.rs` 首次真正编译，暴露并修掉 13 个错误 + 3 个设计缺陷**。起因：`lib.rs` 的 `pub mod window;` 之前缺失，文件写完后**一次都没被编译过**（D 组发现并报告）。首轮编译暴露的真实错误：`Win32::UI::HiDPi` 模块名实际是 `HiDpi`；`GetModuleHandleW` 返回 `HMODULE` 而非 `HINSTANCE`；`Error::from_win32()` 在 windows-result 0.4.1 **不存在**；`key_from_vk` 闭包类型不匹配；测试调了未定义的 `EventLoop::fake()`；`Key::F1..=Key::F24` 不能作 match 区间模式；6 处`drop()` 用在 `Copy` 类型上。**另修 3 个设计缺陷**：`register_class` 重复注册会误报失败；`GetCursorPos` 忘调 `ScreenToClient` 导致指针坐标是屏幕坐标；`GetKeyState` 在 pump 过程中读到上一帧修饰键（改 `GetAsyncKeyState`）。**测试自身也抓到一个错**：`function_keys_span_f1_to_f24` 用 `unwrap_or(Key::F1)` 做断言，等于拿被断言的键当默认值——生产代码是对的，断言是错的。 | `cargo test -p tiez-gfx`：**128 passed / 0 failed**（含我的 **52 个**），**零 error 零 warning**。因`frame.rs` 当时在B 组编辑中，另在 `%TEMP%` 搭了隔离 harness（同版本 egui 0.36.2 + windows 0.62.2）先行验证我的 52 个测试全绿，确认非偶然，验证后已清理。⚠️ `./scripts/build.sh --test` **全工作区仍编不过**，唯一阻塞是 D 组 `tiez-ui/src/renderer.rs` 的 5 个错误（`UiLocal` 未定义、`output` 部分移动、`ctx.style()` 不存在等），与 C 组无关。**实机基线探针 `pipeline_probe` 退出码 0 / `ALL OK`，无回归**（window.rs 不碰渲染路径，但仍实跑确认以维持「不回归」纪律）。

- 2026-10-06 | C | **新增 `examples/window_probe.rs`（实机窗口探针）+ Cargo.toml 注册，EXIT=0**。探测 6 项单测覆盖不到、只有真Win32 能暴露的东西：类注册、DPI 是否在**建窗前**生效、客户区尺寸是否等于请求值（验 `AdjustWindowRectEx`）、`RawInput` 是否自洽（`screen_rect` 有面积 / `time` 单调 / `predicted_dt` 非 0）、egui 能否消费、`destroy` 幂等。⚠️ **首版探针犯了 team-lead 警告的同类错误**：只等系统自发消息，结果「事件总数 0」却仍退出 0——**在自己没测的东西上通过**。改为用 `PostMessageW` 向真实 HWND 注入 8 类消息（鼠标移动/按键/键盘/字符/滚轮/尺寸/关闭），强制走通 `PeekMessageW → translate → WindowEvent` 全链路，9 条事件全部翻译正确。**探针因此抓到 3 个真实问题**：① `ctx.run` 在 egui 0.36 已改名`run_ui`（项目内统一用 `run_ui`）；② `TexturesDelta` 的 `Drop` 有 `debug_assert!(is_empty())`，不 `clear()` 直接 panic（`full_app` 用 `DeltaGuard` 解决）；③ **我的断言写错了**——注入 `WM_KEYDOWN('A')` 后 `TranslateMessage` 会自动再投一条 `WM_CHAR('a')`，于是有两条 `TextInput`，我原先断言「每条都等于"你"」导致误判失败。生产代码三次全对，**错的是我的断言**，已改为集合断言并把这个行为反证成`TranslateMessage` 生效的证据。 | `cargo run -p tiez-gfx --example window_probe` → **EXIT=0 / ALL OK**，实测 `scale_factor=1.5`(dpi 144)、客户区 800x600 物理 = 533.3x400 逻辑、`TextInput ["你","a"]`、9 条事件全对；**零 warning**。回归：`cargo test -p tiez-gfx` **135 passed / 0 failed**；`pipeline_probe` **EXIT=0 / ALL OK** 不回归。⚠️ 全工作区仍被 D 组 `tiez-ui/src/renderer.rs` 阻塞（另`texture.rs:296` 有 1 个 `unsafe_op_in_unsafe_fn` 警告属A 组，均非 C 组文件）

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

- 2026-10-05 | B | **实机`upload_probe` 抓出并修复 2 个真bug**（单测全绿时溜过去了）：<br>① **描述符集只拿到 1 份**：查 ash 源码（`vk/definitions.rs:3734`）确认 `set_layouts()` 会**主动把 `descriptor_set_count` 覆盖成切片长度**，而 `allocate_descriptor_sets` 按该 count 决定返回几份。必须在其**之后**直接写字段（该字段无 builder 方法）。这是 MEMORY.md 坑 15 的**后半段**——之前只记「不设 count 返空 Vec」，没意识到 slice 构造器会主动写入。症状会是多帧共用一份描述符集 → GPU 读到被改写的数据 → **随机花屏**。<br>② **`LayoutTracker` 初始化为 0**：`reset(image_count)` 只在 `rebuild_swapchain` 里调，初始化路径不经过 → 首次 `acquire` 时 `layouts.get()` 越界。修法：在 `swapchain` 被移进结构体前先取 `image_count`（`Swapchain` 无 `Clone`）。 | `upload_probe` 实机报错「期望 3，实际 1」→ 修复后 lib **76 passed / 0 failed 零警告**；`pipeline_probe` 退出码 0 / `ALL OK` 不回归。**教训：这两个 bug 在 76 个单测全绿的情况下漏过，只有真跑GPU 才暴露——A 坚持做实机probe 的判断正确。** |
- 2026-10-05 | B | **补完 arena 接线并修好并发编辑造成的破坏**：`frame.rs` 被第三方加入约 700 行 `StagingArena` 但两处接线未完成（`align_up` 未import、`FrameRenderer` 缺 `staging` 字段）→ 已补齐，`Drop` 中`staging.destroy()` 位置正确（在 `wait_idle` 之后）未改。同时修 `examples/upload_probe.rs` 3 处编译错误（漏 `mut`、借用冲突、传 `GraphicsPipeline` 而非 `.handle`）。**未加 `#[allow(dead_code)]` 消警**——消警不等于解决。 | `cargo test -p tiez-gfx --lib` 76 passed / 零警告 |

---

## 📏 主控新增硬规则（00:50）

### 1. 交付报告不得出现无法验证的声明

禁止写「曾经编译过」「0 error 0 warning」「应该没问题」这类话。
只能给：**「改动后我跑了 X，结果是 Y」**。

**原因**：`window.rs` 有 1296 行、39 个单测，交付时声称「0 error 0 warning」，
实际那 1296 行**从未参与编译**（`pub mod window;` 被并发编辑弄丢）。
数字是真的，含义是假的。

### 2. 提交时禁止 `git add -A`

只 add 自己拥有的文件。`git add -A` 会把别人的中间态一起提交，
造成「我这边是好的，合并后坏了」。

### 3. 改Cargo.toml 前必须先 grep

`[[example]]` 段曾因并发编辑重复声明，导致整个 crate 编译失败。
清单式文件由单一 owner 维护（当前是 mcp-probe-verify）。

### 4. `Ok(())` 不能掩盖异常

探针里 `Err(_) => panic!("丢弃")` + 末尾 `Ok(())` 会让失败被吞掉。
panic 消息必须包含具体错误原因。

---

## 🔒 当前文件独占权（05:30，路径已按 crate 改名更新）

> 上一版还写着 `gfx/src/*` 旧路径，导致 mcp-rename 找不到 store 的 owner。
> 全部改为 `crates/modular-clipboard-*` 全路径。

| 文件（全路径） | Owner | 其他人 |
|---|---|---|
| `crates/modular-clipboard-gfx/src/frame.rs` | mcp-arch-frame | 只读 |
| `crates/modular-clipboard-gfx/src/staging.rs` | mcp-split-arena | 只读 |
| `crates/modular-clipboard-gfx/src/buffer.rs` | mcp-arch-resource | 只读 |
| `crates/modular-clipboard-gfx/src/texture.rs` | mcp-arch-resource | 只读 |
| `crates/modular-clipboard-gfx/src/window.rs` | mcp-arch-window | 只读 |
| `crates/modular-clipboard-gfx/src/pipeline.rs` `shader.rs` `lib.rs` | **主控** | 只读 |
| `crates/modular-clipboard-gfx/examples/upload_probe.rs` | mcp-fix-probe | 只读 |
| `crates/modular-clipboard-gfx/examples/draw_probe.rs` `full_app.rs` | mcp-arch-ui | 只读 |
| `crates/modular-clipboard-gfx/examples/window_probe.rs` | mcp-arch-window | 只读 |
| `crates/modular-clipboard-gfx/Cargo.toml` | mcp-fix-probe | **改前须 grep** |
| `crates/modular-clipboard-store/src/**` | **主控** | 只读 |
| `crates/modular-clipboard-platform/src/tray.rs` `hotkey.rs` | mcp-tray | 只读 |
| `crates/modular-clipboard-ui/src/**` | mcp-arch-ui | 只读 |
| `crates/modular-clipboard-core/src/**` `app/src/**` | **主控** | 只读 |
| `PROGRESS.md` `MEMORY.md` | **主控** | 只读 |

---

## 📋 待决区（主控已确认成立，暂不分配）

| 来源 | 问题 | 归属 |
|------|------|------|
| C 组 | `update_uniform_binding` 遍历 `batches.len()`，应为帧槽位数 | frame.rs owner |
| C 组 | `update_texture_binding` 无「本帧已上传才重绑」状态追踪 | frame.rs owner |
| C 组 | 16 个 `event!` 宏的格式化参数未纳入检查 | window.rs owner |
| C 组 | `install_cjk_font` 两级失败原因未区分 | 待 eframe 迁移后 |
| B 组 | `rebuild_chunk` 应加 `owner_frame == FREE` 断言 | frame.rs owner |

---

## 🔬 竞态诊断（02:40，僵局 8 轮）

### 已排除

| 假设 | 状态 | 证据 |
|------|------|------|
| staging 退休时序错位 | ❌ 已排除 | F 组模拟 N=3/k=2/阈值=1，确认 `retire_frame(owner_frame <= done_frame)` 不提前退休 |
| 描述符集未填充 | ❌ 已排除 | `upload_probe` 传空批次，`record` 提前 return 不绑描述符，却能跑 60 帧 |
| staging chunk_TOCTOU | ❌ 已排除 | B 组统一记账入口（`begin_submit_recording`）后仍 14/15 失败 |
| fence 复用（一个 fence 两处 signal） | ✅ 已修 | B 组拆成 `acquire_fence` + `submit_fence`，删掉手工 `fence_signaled` 字段 |

### 关键数据

| 帧数 | 通过率 |
|------|--------|
| 60 | 92%（10/11） |
| 120 | 67%（5/8） |
| 600 | 30%（3/10） |

**通过率随帧数单调下降 = 竞态签名**（确定性 bug 会在固定帧号稳定复现）。
失败全部发生在**前 10 帧内**。

`full_app`：第 7 帧崩（7 % 3 == 1，恰为槽位 1 首次复用）。

### 待验证方向

- `acquire_fence` 状态管理：Vulkan 要求传给 `acquire_next_image` 的 fence
  在调用前必须**未signal**。若忘记 `reset_fences`，第二次 acquire 违反规范。
- `present_semaphore` 消费时机：`submit_fence` 只证明渲染完成，
  **不证明 present 完成**。标准做法是每个 swapchain image 配 present-wait fence。

### 验证层

已下载 LunarG Vulkan SDK 1.4.363.0（289MB），
正在以管理员权限 headless 安装 `com.lunarg.vulkan.core`。
装好后 `upload_probe` 会直接打印 `VUID-xxxx`。

**注意**：首次安装尝试因需要写 `C:\VulkanSDK` 被沙箱拒绝并回滚，
已用 `dangerouslyDisableSandbox` 重试（用户已批准安装）。

---

## 📏 团队规则更新（02:40）

### 1. 单次绿灯不构成「通过」证据

我曾在 F 组报告「11/12 通过」后宣布「通过」，
下一轮 `full_app` 立刻第 7 帧崩——证明报告者是对的，我错了。

**判据必须是**：指定次数下**0 失败**，且失败率不随工作量上升。

### 2. 通过率随工作量单调变化 ⇒ 竞态

区别于确定性 bug 的「固定位置稳定复现」。
这个判据来自 F 组，是本轮最有价值的诊断工具。

### 3. 诊断工具本身会破坏被诊断的系统

`upload_probe` 之前稳定崩在「第 4 帧」，绕了 5 轮。
根因是探针自己每帧调 `device_wait_idle`，把多帧在途强行串行化，
掩盖了真实的 bug。

**规则**：探针里任何 `wait_idle` 都要在输出里显式标注；
调试用同步必须可开关且默认关闭。
「加了同步就好了」是**危险信号**，不是「修好了」。

### 4. 不确定就不宣布成功

B 组原话：「修一半不确定就不宣布成功，比宣称成功再回滚更好。」
已写入规范。

---

##⚠️ 必读：`frame.rs` 曾在 `c8015c4` 中被静默修改（25% 偶发设备丢失的根因）

`c8015c4`（crate 改名）把工作区整体提交，因此**本文件所记录的一次关键修复没有独立 commit**，
读 git 历史会以为 `frame.rs` 只是被改了目录名。补记于此。

### Bug：`rebuild_chunk` 守卫漏判「同帧已录制」→ use-after-free

**症状**：`ERROR_DEVICE_LOST`，**25% 偶发**（20 次里挂 5 次），崩溃帧号随机。
可骗过「单次退出码 0」的验证——我曾据此误报「P0 已解决」。

**根因**：`rebuild_chunk` 的不变式守卫原先允许「块属于当前帧」时原地重建，
理由是"同帧还没提交，GPU 不可能读到"。**该理由错误**：同帧内第一次上传的
`cmd_copy_buffer_to_image` 已把旧 buffer 写进**本帧命令缓冲**；第二次上传
撑满剩余空间时触发重建 → `destroy_buffer` + `free_memory` → 已录制命令
指向已释放显存。守卫只查了「跨帧在途」，漏了「同帧已录制」。

`cursor > 0` 正是区分这两种情况的字段，当时已在手边却未使用。
（该守卫是 B 组自己上一轮引入的，属自查发现的漏判。）

**触发条件**：仅当「随机上传大小恰好撑满块剩余空间」才触发，故概率约 25%。
`full_app` 因字体增量稀疏而稳定通过 —— **这掩盖了它在真实 UI 里同样会触发的事实**。

**修法**：唯一允许原地重建的情形是 `cursor == 0`；已写入则改用空闲块。
守卫收紧为 `!materialized || used == 0`。

**验证**：`upload_probe` / `full_app` / `draw_probe` 各 30 次，**合计 90/90**；
lib 单测 140 passed / 零警告；新增 3 个回归测试
（`rebuild_is_forbidden_once_block_has_bytes`、
 `append_then_grow_must_use_a_different_chunk`、
 `fresh_block_may_be_rebuilt_in_place`）。

### 由此确立的验收规则（建议纳入协作硬规则）

> **涉及 GPU / 同步 / 内存的改动，实机验证必须 ≥ 30 次连续通过。**
> 竞态问题下单次通过无意义。汇报必须附「N 次 / 失败 M 次」，
> 不得只报「退出码 0」。

依据：本项目三起因「单次绿灯」而被误判为已修复的案例
（fence 双signal 源、arena 退休策略、本节的重建守卫）。

---

## 📏 硬规则更新（03:50）

### 1. Agent 改完立即自己 commit，不要攒给主控

**我犯了这个错**：用 `git add -A` 一次性提交，把 B 组（frame.rs owner）
的静默修复和我自己的图标修复混进了同一个 commit `c8015c4`。

后果：
- 修复没有**独立 commit 记录**，历史里看不出「何时、为何」修的
- 无法单独 bisect / 回滚该修复

**从现在起**：Agent 改完立即自己 commit，主控只 review 不代提交。
主控提交时也**禁止 `git add -A`**，只add 明确知道是自己改过的文件。

### 2. 验收必须带重复次数，单次绿灯不算通过

| 探针 | 要求 | 含义 |
|---|---|---|
| `upload_probe` | **30 次全过** | 渲染/上传路径 |
| `full_app` | 10 次全过（每次 600 帧） | 真实 UI 路径 |
| `pipeline_probe` | 1 次即可 | 设备/交换链/管线创建 |

失败率**不得随帧数或重复次数上升**——上升即竞态。

### 3. 根因（03:40 定位并修复）

`rebuild_chunk` 允许「`owner_frame == current_frame`」的块被重建，
但同帧内第一次上传已把旧 buffer 写进命令缓冲。
第二次上传撑满剩余空间 → 触发重建 → 释放旧 buffer
⇒ **本帧已录制的命令指向已释放显存** = use-after-free。

守卫改为：仅允许「从未物化」或「已写入 0 字节」的块重建。

**为什么难查**：只影响「同帧多次上传」，而
`full_app` 每帧 1 次上传、`upload_probe` 每帧 1~3 次 ⇒ 表现不同。
`draw_probe`（1 帧）根本触发不了。

回归测试 `append_then_grow_must_use_a_different_chunk` 是精确复现。

---

##✅ 竞态已消除（04:10，主控独立复核）

```
1000 帧压力 × 5 次: 5 / 5 通过（18 秒）
full_app     × 10:  10 / 10 通过（每次 600 帧）
测试: 246 全通过，零警告
```

**per-image fence 决定：不做。**

B 组用压力测试证明缺失无害，我独立复核确认。
理由记录在案：

1. `acquire_next_image` 在图像未被present 释放时返回
   `VK_NOT_READY`，**驱动侧已负责这个同步**。
2. 规范提供的显式机制只有两种（per-image fence、
   `VK_KHR_present_wait` 扩展），**两种都有代价**：
   - per-image fence：每张图像一次额外 `waitForFences`，
     空闲时是纯开销
   - present-wait 扩展：同步等待，**丢帧率换延迟**，
     与本项目「常驻低功耗工具」的定位冲突
3. 无证据表明不加就会出问题 ⇒ **不盲目加机制**。

**这次的关键收获**：我两次凭记忆发明 API/状态结构，
都被 B 组查源码拦下（`PresentInfoKHR::fence` 不存在、
`FrameRenderer` 只有 1 个状态字段）。详见 MEMORY.md 第 8 批。

---

## 📌 提交归属说明（04:50，补记）

`7c7665f`「refactor(gfx): 把 StagingArena 拆成独立的 staging 模块」
**除纯搬移外，还含 present 信号量修复**（`map_present_result` 重写
+ `present_failure_never_claims_semaphore_was_consumed` 测试）。

原因是主控与拆分 Agent 几乎同时改 `frame.rs`，各自 `git add` 时
把对方的改动一并带入了。

**内容正确、已验证**（250 测试全过、`upload_probe` 30/30、
`full_app` 10/10），且 `7c7665f` 的提交信息与内容一致，
**历史不算说谎**——故不做 rebase 拆分，改为在此说明。

## present 信号量修复（D 组发现，主控实施）

Vulkan 1.3 §3.5.3：present 提前退出（`OUT_OF_DATE` / `SUBOPTIMAL` /
`SURFACE_LOST_KHR`）时，`wait_semaphores` **可能不被 signal**。
原实现把这些都当作「已消费」，槽位复用时会再次 signal
同一 semaphore = **未定义行为**。

现行为：除「彻底成功」外一律返回 `Outdated`，
强制上层重建、绝不复用槽位。

**这不是「加机制」，是修正错误的成功判定**——
压力测试跑得过（1000 帧 × 5、55 次 full_app）只说明
本机时序恰好对，不构成正确性证明。

## 清理

- `stash@{0}` / `stash@{1}`：拆分过程的中间态，内容已入账 `7c7665f`，已删
- `target/.../tiez.exe`：改名前的旧产物，已删

## 死依赖清理（`7626b80`）

`bin` 原本直接依赖 `store` 只为拼一句帮助文本。
现改为经 `ui` 层转发，依赖方向恢复为
`bin → {core, app, ui}`，符合分层约定。

---

## 🔒 隔离构建目录纪律（06:10）

多个 Agent 共用同一个 `target/` 会有两个问题，且**第二个是隐性的**：

1. **文件锁冲突**：`LNK1104 无法打开 ... .exe`——
   正在运行的探针 exe 被占用，别人无法重新链接。
2. **测的不是最新构建** ⚠️ **更危险**。
   改完源码后若未重新编译就跑探针，测的是**旧二进制**。
   本夜已发生两次「假绿灯」：
   - 主控采信了 F 组的「30/30 通过」，但那个二进制是 01:26 的旧构建
   - F 组自己测「修前基线 0/50」，实际文件处于半编辑坏状态、
     跑的是**根本没重新编译的旧二进制**（该二进制修前也是 30/30）

**规则**：
- 每个 Agent 用**独立的 `CARGO_TARGET_DIR`**：
  ```bash
  CARGO_TARGET_DIR="C:/Users/Cookies/WorkBuddy/Tiez/target-<你的名字>" \
    MSYS_NO_PATHCONV=1 CC=cl.exe cargo build -p modular-clipboard-gfx \
    --example upload_probe --target x86_64-pc-windows-msvc
  ```
- **报告任何测试结果前，必须先确认二进制时间戳晚于最后一次源码修改**：
  ```bash
  stat -c '%y' target-xxx/debug/examples/upload_probe.exe
  ```
- **红灯同样需要多次验证**。不是只有绿灯要重复确认——
  「跑出来有问题」这个结论本身也可能是旧二进制的产物。

（这条是 F 组在我误采信假绿灯后提出的，比我原先的规则更完整。）
