# qa-ui：UI/UX 与交互动效审查报告（2026-10-12）

> 只读审查，未修改任何文件、未运行 cargo。
> 范围：modular-clipboard-ui/src/ 全部 16 文件 + gfx/src 中 UI 相关部分。

## A. UI 问题清单

### P0（功能性缺陷，用户可感知的错误行为）

- [UI-P0-1] P0 | mcp_status.rs（全仓）+ paint.rs:390 | MCP 状态探测从未接线，覆盖层永远显示"未连接"
  `McpProbe` 实现了完整后台线程探测与 `McpConn` 四态，但全仓无任何 `McpProbe::start()` 调用——探测线程从未启动；`UiState::mcp_conn` 文档称"由 App 每帧从探测器同步过来"，实际全仓无写入方。后果：MCP 覆盖层状态行永远显示"未连接·总开关已关闭"，能力徽标永远未连接样式，即使 MCP 服务实际在运行。
  建议：App 初始化时 `McpProbe::start()`，帧循环内将探测结果写入 `UiState::mcp_conn`；或删掉该字段与覆盖层状态行。二选一，不可维持现状。

- [UI-P0-2] P0 | modular-clipboard-app/src/lib.rs:603（写入方）+ 全仓（无读取方）| `Service::notify` 的 notice 全仓无读取方，8 处用户反馈静默丢失
  `state.notice = Some((msg, now))` 是唯一写入点，全仓无读取。`svc.notify()` 在 app.rs 被调用 8 处（复制失败、删除失败、清空失败、已复制配对提示词、保存失败等）全部写进去即蒸发。文档承诺的"状态栏显示 3 秒过期"已失效（STATUSBAR_HEIGHT=0，状态栏已移除）。
  建议：落地轻量 toast（右上角滑入，3s 自动消失），渲染点放在 draw() 顶层；或顶栏 24px 状态条。失败类通知必须有可见反馈。

- [UI-P0-3] P0 | childwin.rs:364（render 全程）| 卡片子窗口没有拖动区，鼠标无法移动窗口
  Win32 无边框窗口（WS_POPUP）必须每帧向 WM_NCHITTEST 上报 HTCAPTION 拖动区。主窗口（app.rs:618）和设置窗口（settingswin.rs:230/235）都做了，唯独 childwin.rs 的 render 从不调用。后果：卡片拖出为独立窗口后只能边缘 resize + Alt+Space 键盘移动，鼠标无法拖动——直接违反"拖出去"功能的存在意义。
  建议：childwin.rs render 内 run_ui 之后调用 `window.set_drag_region(Some(标题栏矩形))`，挖空按钮区（chrome 表已支持 MAX_DRAG_SEGS=8 分段）。

### P1

- [UI-P1-1] P1 | childwin.rs:364 | 子窗口 Context 不做 theme::apply，滚动条/tooltip 观感与主窗口不一致
  主窗口（paint.rs:4751）与设置窗口（settingswin.rs:281）每帧调 theme::apply 把 Palette 灌入 egui::Visuals；子窗口用 egui 默认 visuals。后果：① 子窗口 ScrollArea 滚动条用默认样式，与主窗口视觉断裂；② tooltip 用默认深色观感；③ paint.rs 的 SCROLLBAR_RESERVE=10 假设 floating 10pt，在子窗口默认滚动条下假设失效。
  建议：childwin render 内 run_ui 前补 `crate::theme::apply(&self.ctx, app.palette())`。

- [UI-P1-2] P1 | paint.rs:3390-3400、3757-3799、settings_item_row | 每帧全量分配，直接挤压 60fps 预算
  ① draw_history_body 每帧 `items.iter().cloned().collect()` 克隆全部 ClipItem（含 String）；② row_height_for 每帧对每条 item 做完整 layout_job 只为量高；③ heights/offsets 两个 Vec 每帧重建；④ settings_item_row 经 is_changed() 每项每帧构造 Config::default()。动效上线后每帧还要叠加补间计算，CPU 侧预算会先于 GPU 爆掉。
  建议：引入动效**之前**先治理——ClipItem 改用 &[ClipItem] 借用或 Rc；行高按"内容签名"缓存；Config::default() 提到循环外。

- [UI-P1-3] P1 | app.rs Op::SetThemeMode 处理路径 | 主题切换瞬时硬切，无任何过渡
  切换明暗/16 主题色时 App.pal 立即替换 + 重灌 Visuals，下一帧整窗跳变。纯平毛玻璃审美下跳变尤其刺眼。
  建议：颜色补间 200-250ms，补间值灌 Visuals；磨砂背景图不变、仅前景色渐变，GPU 零新增开销。

- [UI-P1-4] P1 | paint.rs:3987 | 选中态描边 `2.0 * scale` 违反逻辑点约定
  egui Stroke 宽度单位是逻辑点，乘 scale 后 150% DPI 下实际 3pt，与注释"2pt"不符，且与其他 1.0 逻辑点描边不成体系。同类：paint.rs:3182 CARD_STROKE * scale（HISTORY_UNITY=true 已早退，死分支里的潜在 bug）。
  建议：统一去掉 * scale，按逻辑点声明；确需物理像素对齐时用 round_to_pixel 而非线性缩放。

### P2

- [UI-P2-1] P2 | paint.rs 20+ 处 | 描边宽度硬编码，绕过 stroke_* 令牌
  Stroke::new(1.0, pal.border) 等直接写字面量宽度，Palette 有 55+ 字段但宽度维度无令牌，同一"边框"概念宽度 1.0/1.5/2.0 混用。
  建议：theme.rs 增加 stroke_hairline/stroke_emphasis 等宽度令牌，paint 侧全部替换；同时消掉 P1-4 的 scale 乘法。

- [UI-P2-2] P2 | paint.rs:2604-2605、2687/2697、2835-2836、3049/3065、2651/2653 | 魔法数字集中区
  开关 h=20.0; w=h*1.9、分段控件 h=30.0/+20.0、MCP 覆盖层面板 480.0/560.0、能力徽标 44.0×20.0、Color32::WHITE、from_black_alpha(45)。改一处审美要全文搜数字。
  建议：提取为 paint 模块常量或入 Palette（颜色部分必须入 Palette，否则暗色主题下 WHITE/黑 45% 会失控）。

- [UI-P2-3] P2 | paint.rs:3182 | CARD_STROKE * scale 位于 HISTORY_UNITY 早退后的死分支
  HISTORY_UNITY=true 时函数提前返回，3182 行永不执行；一旦关闭 UNITY，带 * scale 的潜在 bug 直接复活。
  建议：死分支要么删、要么随 P1-4 一并修正。

- [UI-P2-4] P2 | paint.rs draw_search / settings_mcp_overlay / draw_segmented | 展开/开合/选中全部硬切换
  搜索框 140pt 宽度瞬现、MCP 覆盖层遮罩+面板瞬现、分段控件选中块位置瞬切。硬切换让界面显得"廉价"。
  建议：见 B 部分对应动效条目。

### P3

- [UI-P3-1] P3 | paint.rs draw_history_body/draw_item_row | 键盘可达性缺失
  无方向键导航、无 Enter 复制、无 Esc 关闭设置窗口/覆盖层。焦点管理完全依赖鼠标。
  建议：至少补 ↑/↓ 移动选择、Enter 复制选中项、Esc 关设置/关覆盖层。优先级高于动效。

- [UI-P3-2] P3 | thumbnail.rs | 陈旧文档引用 + GPU 缩略图显示未接线
  draw_image_preview 在 paint.rs 中已不存在（陈旧引用）；CPU 侧解码/降采样/负缓存齐全，但 GPU 用户纹理显示路径未接线（用户纹理被渲染器丢弃）。
  建议：更新文档；图片类剪贴项要么接线 GPU 显示，要么明确标注"当前版本不显示图片内容"。

- [UI-P3-3] P3 | presence.rs set_cursor_icon | 光标图标映射不全
  仅 Default/Text/PointingHand 三档；可点击图标按钮、拖动区、resize 边缘没有对应光标（SizeAll/SizeNs 等）。
  建议：按交互语义补全映射，成本极低、可感知收益高。

- [UI-P3-4] P3 | paint.rs draw_history_body | 列表项增删无入场/退场过渡
  建议：见 B 部分；注意与 P1-2 的每帧分配治理协同做。

## B. 动效机会清单（按优先级）

| # | 位置 | 当前行为 | 建议动效 | 时长ms | 性能注意点 |
|---|---|---|---|---|---|
| 1 | draw_switch 开关滑块 paint.rs:2639-2645 | ✅ 已有 animate_bool | 保持现状（全 UI 唯一动画） | ~150(现有) | 零分配，已是范本 |
| 2 | 主题切换 app.rs SetThemeMode | 整窗瞬跳 | Palette 颜色补间（前景渐变，背景模糊不动） | 200-250 | 每帧只改 Palette 字段+重灌 Visuals，无分配；勿动 backdrop 纹理 |
| 3 | 行 hover paint.rs draw_item_row | 背景色瞬切 | 背景色 ease-out 补间 | 100-120 | 复用 animate_bool 机制（Id 加行号），零分配 |
| 4 | 行选中 | 背景+描边瞬切 | 同上，选中权重高可 120-150 | 120-150 | 描边宽度不要参与补间，只补颜色 |
| 5 | MCP 覆盖层 paint.rs settings_mcp_overlay | 遮罩+面板瞬现 | 遮罩淡入 + 面板上移 8px/淡入 | 150-200 | alpha 补间只改 tessellation 顶点色，零 GPU 新增 |
| 6 | 搜索框展开 paint.rs draw_search | 140pt 瞬现 | 宽度补间 + 内容淡入 | 150-180 | max_rect 逐帧变触发子 UI re-layout，控制嵌套深度 |
| 7 | 分段控件选中 paint.rs draw_segmented | 选中块瞬切 | 选中块 x 位置 ease-in-out 滑动 | 150 | rect 补间，每帧 1 次 lerp，无分配 |
| 8 | 设置窗口显隐 settingswin.rs | SW_HIDE/SW_RESTORE 硬切 | 淡入 120-150ms | 120-150 | Win32 SetLayeredWindowAttributes 改 alpha 不重排 |
| 9 | 列表项入场/退场 draw_history_body | 瞬时 | 新增项淡入+上移 8px；删除项淡出后回收 | 120 | 只对 delta 项动画；退场需延迟一帧删除数据 |
| 10 | 卡片展开/折叠 | 高度瞬切 | solver 矩形高度补间 | 150 | solve_scaled 是纯函数，天然支持 rect 插值 |
| 11 | 窗口唤起/隐藏到托盘 presence.rs | 显隐硬切 | 淡入淡出 | 100-150 | 同 #8 |
| — | 滚动条显隐 | egui 内部行为 | 不可控 | — | 接受现状 |

**性能总预算**：磨砂捕获 30fps 节流（约 1ms/次，占帧预算 3%）；GPU 模糊链在 1/4 分辨率做三趟卷积。动效全部是 CPU 侧标量/颜色补间 + tessellation 顶点色变化，不新增 GPU pass、不新增纹理上传；真正瓶颈在 P1-2 的每帧分配，必须先治理。

## C. 动效库方案建议

### 推荐：自研轻量补间模块（约 150 行，新文件 ui/src/anim.rs）

不引外部 crate 的理由：① egui 生态动画 crate 普遍假设 eframe/winit 时间源，本项目 ash 直驱、无 eframe；② 需求面窄——标量/颜色/rect 补间 + 缓动 + 按 Context 的动画注册表，引依赖收益/风险比不成立；③ egui 自带 animate_bool 只覆盖 0→1 布尔场景。

### API 草图

```rust
// ui/src/anim.rs（约 150 行，零依赖）
use std::time::{Duration, Instant};
use egui::{Id, Color32, Rect, Context};

#[derive(Clone, Copy)]
pub enum Ease { Linear, OutCubic, OutExpo, InOutCubic }

/// 每个补间：目标值变化时自动重启动画（retarget 语义）
struct Tween { from: f32, to: f32, t0: Instant, dur: Duration, ease: Ease }

/// 每 Context 一份，存在 egui 的 data store 里
#[derive(Default)]
pub struct AnimStore { map: std::collections::HashMap<Id, Tween> }

/// 核心：取 id 对应的动画当前值；target 变化时重设基线
pub fn tween_f32(ctx: &Context, id: Id, target: f32, dur: Duration, ease: Ease) -> f32 {
    let now = ctx.input(|i| i.time);
    // 存 ctx.data 临时 store；retarget 从当前显示值续接
}

pub fn tween_color(ctx: &Context, id: Id, target: Color32, dur: Duration) -> Color32 { /* 四通道 */ }
pub fn tween_rect(ctx: &Context, id: Id, target: Rect, dur: Duration) -> Rect { /* 四点 */ }
```

要点：① 时间源用 ctx.input(|i| i.time)（egui 自管）；② retarget 语义保证交互连贯；③ 状态存 ctx.data 临时 store，无全局可变状态；④ 每帧每活动动画仅几次 f32 运算 + 一次 HashMap 查找，零堆分配。

### 落地步骤

1. 先治理 P1-2（每帧分配）；
2. 新增 anim.rs + 单测（缓动单调性、retarget 连续性、完成吸附）；
3. 迁移顺序：行 hover（#3）→ 开关迁移统一 API（#1）→ 主题切换（#2）→ MCP 覆盖层（#5）→ 搜索展开（#6）→ 其余；
4. 每迁一处验证：150%/200% DPI 下时长观感一致、无掉帧。

### 风险与缓解

- 动画值参与 layout：只让叶子级 rect 动，容器尺寸固定+clip；
- 与磨砂 30fps 节流叠加：动画启动首 200ms 临时提高捕获频率到 60fps，或先实测再决定；
- retarget 抖动：从当前值续接 + EPSILON 吸附；
- 删除项退场与数据模型竞态：退场项标记后由 paint 层插值绘制，不进 solver 输入。
