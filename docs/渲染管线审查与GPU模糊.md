# 渲染管线审查与自研 GPU 模糊

> 2026-10-11。本轮由 3 个只读 Agent 并行审查渲染管线，随后实现自研 GPU 模糊
> 并接入界面。本文记录审查结论、实现方案与验证结果。

---

## 一、审查方法与范围

3 个 Agent 并行、只读，分工如下：

| Agent | 范围 | 产出 |
|---|---|---|
| A | `gfx` 渲染管线（lib/frame/pipeline/texture/buffer/staging/shader + WGSL） | 多 pass 的硬障碍清单、同步/生命周期缺陷、接入 API 清单 |
| B | `ui` 绘制链（renderer/paint/app/settingswin/multiwindow） | 图元链路、模糊接入点、`PaintCallback` 跳过点 |
| C | 模糊方案设计（WGSL + naga + ash 约束） | 四步架构、卷积核、WGSL 草稿、风险 |

### 最关键的一条结论（来自 Agent B）

**「模糊设置面板背后的主界面」在空间上不成立。** 设置界面早已从「主窗口覆盖层」
迁移为**独立顶层窗口**，与主窗口**并排**摆放（`settingswin.rs`），背后是桌面而不是
主界面。真正能模糊、也确实需要模糊的目标只有一个：**设置窗口自身的内容，被 MCP
覆盖层盖住的那部分**。当前实现用一层 148-alpha 的半透明遮罩「假装失焦」。

这条结论把任务从「跨窗口合成」降级为「单窗口内多 pass」，可行性完全不同。

### 其余关键发现

- 现有管线**单 pass 直写交换链、无任何中间目标**（`frame.rs::record` 写死
  `pending.frame.framebuffer`），这是接入模糊的最大结构障碍。
- `PipelineBundle` 只携带单条渲染通道/管线（`frame.rs`），`begin_render_pass`
  固定在 acquire/record/present 三段里——同一帧插额外 pass 无入口。
- `paint.rs` 里那条「模糊是后续工作」的注释说的是**设置窗口内的 MCP 覆盖层**，
  不是主窗口背景遮罩。
- `PaintCallback` 被 `renderer.rs` 的 `continue` 跳过（`Primitive::Callback` 分支）。
- naga 25.0.1 **支持** compute，但项目**没有 compute 队列**、没有跨队列同步、
  没有 ownership transfer ⇒ 用 **fragment 分离式高斯** 改动面远小于引入 compute 队列。
- `RenderPass` 的附件格式由 `pick_format` 唯一决定；离屏图取**同一格式**即可复用
  同一个渲染通道（只需换 framebuffer），省掉一整套通道与格式分叉风险。

---

## 二、实现的模糊方案

### 2.1 数据流

```text
① 场景      egui 内容 → scene（全分辨率，离屏）
② 降采样    scene → blur_a（1/4 分辨率，线性采样即一次双线性平均）
③ 横向卷积  blur_a → blur_b
④ 纵向卷积  blur_b → blur_a
⑤ 合成      blur_a 作全屏底图铺满交换链 + 覆盖层面板清晰叠加
```

分离式高斯的收益：半径 8 时 `(2r+1)² = 289` 次采样降到 `2×(2r+1) = 34` 次。
1/4 降采样再让像素数降到 1/16，半径 8 在 1/4 分辨率下等效全分辨率 ~32px。

### 2.2 关键取舍

| 取舍 | 选择 | 理由 |
|---|---|---|
| 着色器类型 | **fragment**（非 compute） | 项目无 compute 队列；fragment 改动面小一个数量级 |
| 卷积核权重 | **着色器内由 σ 现算** | WGSL uniform 里 `array<f32,N>` 步长必须 16 字节对齐，现算绕开该限制且同一份 SPIR-V 服务任意半径 |
| 采样调用 | `textureSampleLevel`（显式 LOD 0） | 循环体内的 `textureSample` 是非 uniform 控制流，属未定义行为 |
| 中间格式 | 与交换链**同格式** | 复用同一个渲染通道；避免 sRGB 二次编解码偏色 |
| 采样器 | 线性 + `CLAMP_TO_EDGE` | 线性让降采样平滑；REPEAT 会把对侧边缘卷进来 |
| 布局 | 显式 `image_barrier`，禁用 `GENERAL` | 与既有「渲染通道不做隐式转换」的设计一致 |

### 2.3 分割机制：用 `PaintCallback` 当分段信号

覆盖层要求「它之前的内容模糊、它本身清晰」。渲染层需要知道**从哪里切**。
方案：UI 在画覆盖层之前放一个**空操作** `PaintCallback`，渲染层在
`tessellate_into` 里记录它出现时的批次下标（`blur_split`）。

选它的理由：`ClippedPrimitive` 不携带分段信息，而 `PaintCallback` 是 egui
**自带**的「此处插入自定义绘制」机制，在 tessellation 里必然保留为独立图元
（不会与相邻网格合批）——比自造「魔法颜色矩形」之类的协议可靠得多。

---

## 三、管线改动清单

### 新增

| 文件 | 内容 |
|---|---|
| `gfx/shaders/blur.wgsl` | `vs_fullscreen` + `fs_blur`（分离式高斯）+ `fs_copy`（降采样/合成） |
| `gfx/src/blur.rs` | `BlurUniforms` / `BlurTarget` / `BlurChain` / `BlurPipeline` + 纯逻辑单测 |

### 修改

| 文件 | 改动 |
|---|---|
| `gfx/src/shader.rs` | 加载 `blur.wgsl` 模块；新增按名取入口的 `vertex_stage_named` / `fragment_stage_named` |
| `gfx/src/pipeline.rs` | 新增 `DescriptorLayout::blur`（3 绑定）与 `GraphicsPipeline::fullscreen`（无顶点输入、关混合）；抽出 `from_bindings` 统一标志逻辑 |
| `gfx/src/texture.rs` | 新增 `Sampler::new_linear_clamped` |
| `gfx/src/lib.rs` | 新增 `pub mod blur` 与 `create_framebuffer` 公共助手 |
| `gfx/src/frame.rs` | `DrawInput.prelude`（全屏底图）；`record_to(fb, extent, input)`（可录到离屏）；`end_pass()`（离屏 pass 收尾）；`render_pass()` 访问器 |
| `ui/src/renderer.rs` | `tessellate_into` 返回分割点；`Painter::paint_blurred`；懒创建模糊资源 `ensure_blur`；`Drop`/`on_swapchain_rebuilt` 销毁 |
| `ui/src/paint.rs` | 覆盖层前放分割标记；遮罩 alpha 148→96（模糊才是主体） |
| `ui/src/settingswin.rs` | 覆盖层打开时走 `paint_blurred` |
| `ui/src/app.rs` | 新增 `mcp_overlay_open()` 访问器 |

### 一个必须记下的坑

`record` / `record_to` 只**开启**渲染通道（交换链那趟由 `present` 关闭）。
离屏 pass 不走 `present`，因此 `record_to` 之后**必须**显式 `end_pass()`，
否则紧跟着的卷积 pass 会因「已有通道激活」全部失败。首版实现漏了这一步，
是靠通读 begin/end 配对才发现的——单测与探针都覆盖不到「两趟 pass 交替」这个组合。

---

## 四、验证结果

### 4.1 单测

`cargo test --workspace --lib --tests` **586 项全通过**，其中新增：

- `gfx::blur` 11 项：uniform 布局、方向正交、半径钳制、σ 下限、权重对称/单调/归一、
  降采样尺寸（含 0 尺寸钳到 1）
- `gfx::shader` 2 项：模糊 SPIR-V 头部、入口点名与 egui 模块不冲突
- `ui::renderer` 4 项：分割点落在批边界、无标记时等于批次数、只认第一个标记、
  标记在开头时为 0

### 4.2 端到端探针 `gfx/examples/blur_probe.rs`

灌入 1px 黑白棋盘 → 上传离屏场景图 → 三趟卷积 → 读回低分辨率结果 → 比较相邻像素差：

```text
[probe] 原始棋盘相邻像素差 = 255.00 / 255
[probe] 模糊结果相邻像素差 = 0.00 / 255
[probe] 高频衰减 = 100.0%
[probe] PASS：GPU 模糊确实抹掉了高频
```

> 第一版探针用「方差」判定，误报 FAIL。根因：方差把 alpha 通道一起算了，
> 而模糊后 RGB 是均匀灰 127.5、alpha 恒 255，两者之差凭空贡献约 3000 的方差。
> 换成「相邻像素差」后判据干净——它直接度量高频，且不受常量通道干扰。

### 4.3 真实程序截图验证

`MC_SETTINGS=1` 打开设置窗口 + 临时强制覆盖层打开，抓图确认：

- 背景（左侧导航项、右侧开关）明显**虚化**；
- 中间 MCP 面板**保持锐利**；
- 运行期日志无 error / panic / VUID / device lost。

截图：`target/blur_verify.png`。

---

## 五、已知限制与后续

1. **只接了设置窗口的 MCP 覆盖层**。主窗口与卡片子窗口不创建模糊资源（不付显存开销）。
2. **模糊在编码空间做**（非线性光）。对「大面积纯色 + 文字」的界面视觉差异不可辨；
   若将来离屏目标改 `*_SRGB`，采样器会自动解码/编码，着色器无需改动。
3. **固定 1/4 降采样 + 半径 8**，未暴露为配置项。
4. **未做线性采样合并 tap**（`9-tap → 5 fetch`）。当前半径 8 在 1/4 分辨率下代价可接受，
   4K 全屏覆盖层才需要该优化。
5. **未接 Vulkan 验证层**做一次带验证的跑测。当前靠 `device_wait_idle` 与显式屏障
   保证正确性，验证层能进一步确认无同步误报。

---

## 六、本轮未做的（附理由）

| 项 | 为什么不做 |
|---|---|
| compute shader 版模糊 | 需要新建 compute 队列 + 跨队列 ownership transfer，改动面远大于收益 |
| 把模糊半径/强度做成设置项 | 当前没有产品需求；先证明效果，再谈可调 |
| 主窗口磨砂改用自研模糊 | 主窗口「磨砂」是 DWM 的 `DWMWA_SYSTEMBACKDROP_TYPE`，跨窗口合成代价高、收益不明 |
| 线性采样合并 tap | 需要改着色器循环步进与权重公式，属优化而非功能；当前规模不值得 |
