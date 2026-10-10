# 主窗口视觉原型 · 规范说明

对应文件：[`main-window.html`](./main-window.html)（单文件，直接用浏览器打开）。

> 本文档与原型**只是视觉与交互的沟通稿**，不参与 Rust 构建。
> 正式取值的唯一权威是 `crates/modular-clipboard-ui/src/theme.rs` 与
> `crates/modular-clipboard-ui/src/paint.rs`。
> **在明确「可以动了」之前，不修改任何 Rust 代码。**

---

## 0. 现有实现的依据

原型不是凭空画的，几何与配色全部来自现有代码：

| 内容 | 出处 |
|---|---|
| 全部颜色 token | `theme.rs` 的 `palette!` 宏（约 160 行） |
| 顶栏几何与拖动区 | `paint.rs::draw_topbar` / `draw_topbar_icons` |
| 卡片与内嵌置顶区 | `paint.rs::draw_expanded_card` / `embedded_pinned_height` |
| 列表与行卡片 | `paint.rs::draw_history_body` / `draw_item_row` |
| 行高公式 | `paint.rs::row_height_for` |
| 布局常量 | `solver.rs::TOPBAR_HEIGHT`、`paint.rs::` 下各 `const` |
| 图标集合 | `icons.rs::Icon`（PUA 码位 `U+E000..=U+E011`） |
| 默认窗口尺寸 | `config.rs`：`720 × 800` **物理**像素 |

---

## 1. 组件树

```text
App  (无边框窗口；整个界面按「逻辑点」布局，zoom = pixels_per_point)
│    · 窗口圆角 8 —— 由 DWM 提供，**半径系统定、改不了**（见 §2.5.1）
│
├─ TopBar  h=44 · bg=surface_variant · 底边 1px border_subtle
│  │        内边距左右各 space_md(12)
│  ├─ DragRegion·left ........ 左缘 → 搜索图标左缘 − 2pt
│  ├─ SearchBox ............. w=140 · surface + 1px border · r=6 · font_md(14)
│  │  └─ TextEdit ........... hint「搜索历史…」 · margin(2,0) · frame=NONE
│  ├─ SearchIcon ........... 17×17 · 折叠 ⇄ 展开（不占位，点击才弹出）
│  ├─ IconButton·close ..... Icon::Minimize ·「关闭到托盘」· Op::CloseWindow
│  ├─ IconButton·clear ..... Icon::Trash · 破坏性 · hover=danger · Op::ClearAll
│  ├─ IconButton·pin ....... 新增 · 开关态 · 见 §3.1.1
│  ├─ IconButton·settings .. Icon::Settings · Op::ToggleSettings
│  └─ DragRegion·right ..... 设置按钮**真实右缘** → 窗口右缘
└─ CardArea  (bg) · 内缩 space_sm(8)（圆角改造新增，见 §2.5.2）
   └─ Card·History  整卡底=list_bg · 1px border_subtle · r=12 · 内边距 13 · 无标题行
      │  head_h = 0（历史栏按用户要求去掉「历史」标题）
      │
      ├─ PinnedBand  仅当「置顶内嵌」且存在置顶条目；高度封顶 34%
      │  ├─ Label「置顶」...... font_xs(11) · text_dim
      │  ├─ Separator .......... 1px border_subtle
      │  └─ Row × N ........... 独立滚动槽位（与主列表不串）
      │
      └─ List  左右 padding 8（同时是拖动区）
         │  ScrollArea: floating · bar_width=10 · 覆盖内容右缘，不占布局宽度
         └─ Row × N  gap 6 · min-h 58 · 右缘让出 SCROLLBAR_RESERVE(10)
            │  背景 surface_raised + 1px border_subtle + r=12
            ├─ Act·pin ........... 15.4×15.4 · 右上角 · Icon::Pinned/Unpinned
            ├─ Act·copy .......... Icon::File（无更贴切图标，近似表达「复制」）
            ├─ Act·delete ........ Icon::Close · hover=danger
            ├─ KindIcon .......... 16×16 · 垂直居中 · 文本/HTML/图片/文件
            ├─ Meta·top .......... 来源程序 · 时间 · font_xs · text_dim · 左对齐
            ├─ Preview ........... font_md(14) · 行高 1.35 · **按宽度折行、不截断**
            └─ Meta·bottom ....... 类型 · 大小 · font_xs · text_dim · 右对齐
```

### 行内三段布局

```text
┌──────────────────────────────────────────────┐
│ [图标]  来源程序 · 时间                     │  ← top_band  (pad_v=5, meta_h≥11)
│         预览文本，可折行，不截断        ⊕ ⧉ ✕│  ← 按钮**垂直居中**（原贴顶）
│                            类型 · 大小       │  ← bot_band  (右对齐)
└──────────────────────────────────────────────┘
   ↑ preview_band 与上下两段各留 2pt
```

### 行内操作按钮的位置（本轮调整）

| | 原先 | 改为 |
|---|---|---|
| 垂直 | `top: pad_v` —— 与左上的「来源 · 时间」同一水平线 | **垂直居中**（`top: 50%`，与左侧类型图标对齐） |
| 水平 | 右缘 `r.max.x`（贴边） | 右缘再内收 `space_sm(8)` |

原先贴顶的坏处：按钮与来源行争同一行空间，长来源名（完整 exe 路径）
被 elide 截断得只剩几个字符。垂直居中后上下都留出余量，两者不再打架。

对应 `text_x1` 也要跟着收 `space_sm`，否则文字会顶到按钮上——
`preview_width()` 里减的是 `acts_w`，那是**按钮组自身的宽**，
不含新加的右侧留白。

高度公式（`row_height_for` 与 `draw_item_row` 共用同一份）：

```text
row_h = max(折行数 × font_md × 1.35 + meta_h×2 + pad_v×2 + 4, ROW_HEIGHT_MIN=58)
```

文本区右边界：

```text
preview_width = 行卡片宽 − icon_w(24) − acts_w(58.2)
             = r.max.x − 58.2        ← 恰好停在操作区左边
```

所以长来源名会被 `elide_text` 截成省略号，而**不会**排到按钮底下。

### 顶栏按钮的三层尺寸（容易混）

```text
btn       = topbar_h × 0.62                      = 27.28   （btn_area 的基准）
btn_area  宽 = btn×3 + gap×2，gap = space_sm×2 = 16 → 113.84
step      = btn_area 宽 / 3                       = 37.95   （slot 步距）
热区 sq   = step × 0.62                          = 23.53   ← 实际可点区域
字形      = icon_size                            = 16      ← 比热区小一圈
```

---

## 2. 设计 Token

### 2.1 颜色（取自 `palette!`，深色在前）

| 组 | Token | 深色 | 浅色 |
|---|---|---|---|
| 背景 | `bg` | `#14161A` | `#F2F4F7` |
| | `surface` | `#1E2127` | `#FAFBFC` |
| | `surface_variant` | `#262A31` | `#ECEFF3` |
| | `list_bg` | `#101215` | `#E8EBF0` |
| | `surface_raised` | `#2E333B` | `#FFFFFF` |
| 描边 | `border` | `#6E7886` | `#7C8692` |
| | `border_strong` | `#9CA6B4` | `#4A5562` |
| | `border_subtle` | `#2A2F36` | `#DDE2E8` |
| 文本 | `text` | `#E3E6E8` | `#24292E` |
| | `text_dim` | `#C2C2C2` | `#545454` |
| | `text_bright` | `#FCFCFC` | `#0F0F0F` |
| | `text_on_accent` | `#0A0D12` | `#FFFFFF` |
| 状态 | `accent` | `#84B7EB` | `#1D65AF` |
| | `accent_hover` | `#7EB6F1` | `#1464B8` |
| | `accent_active` | `#79B6F6` | `#0C64C0` |
| | `success` | `#A0DAAE` | `#307B43` |
| | `danger` | `#E28D8F` | `#B42D31` |
| | `danger_hover` | `#EB8487` | `#C11F24` |
| | `warn` | `#EBC98E` | `#906419` |
| | `info` | `#95CFE4` | `#257593` |

交互态底色是**半透明**的，叠在 `surface_raised` 上：

| Token | 深色 | 浅色 | 用途 |
|---|---|---|---|
| `row_alt` | `rgba(255,255,255,.063)` | `rgba(0,0,0,.039)` | 隔行 |
| `row_hover` | `rgba(255,255,255,.118)` | `rgba(0,0,0,.102)` | 悬停 |
| `row_selected` | `rgba(132,183,235,.220)` | `rgba(29,101,175,.204)` | 选中 |
| `row_active` | `rgba(255,255,255,.204)` | `rgba(0,0,0,.204)` | 按下 |

> 设计意图：状态三态（normal / hover / active）**共用亮度档位**，
> 区分靠**饱和度**。亮度一旦为了表达「按下」而下调，状态色作为前景
> 会掉出 4.5:1，作为浅色主题的填充底色又要求更深——两者不可兼得。

### 2.2 尺度

| 组 | Token | 值 |
|---|---|---|
| 圆角 | `radius_sm` / `radius_md` / `radius_lg` | 4 / 6 / 10 |
| | **`win_radius`**（新增） | 16 ※ **系统定，改不了**，见 §2.5 |
| | **`card_radius`**（新增） | 12 · 卡片圆角（原 `CARD_RADIUS = 0`） |
| | **`row_radius`**（新增） | 12 · 行卡片圆角（原用 `radius_sm = 4`） |
| | **`card_pad`**（新增） | 13 · 卡片内边距，必须 ≥ `card_radius` |
| | **`cardarea_pad`**（新增） | 8 · 卡片距窗口边缘的缩进 |
| 间距 | `space_xs` / `_sm` / `_md` / `_lg` / `_xl` | 4 / 8 / 12 / 16 / 24 |
| 字号 | `font_xs` / `_sm` / `_md` / `_lg` | 11 / 12 / 14 / 16 |
| 控件 | `control_height` / `icon_size` | 28 / 16 |
| 描边 | `stroke_thin` / `_normal` / `_thick` | 1 / 1.5 / 2 |
| 动效 | `anim_fast` / `anim_normal` | 90ms / 160ms |

字号是**逻辑点**，不再乘 DPI：`sized(size, _scale)` 直接忽略 `scale`，
由 egui 负责缩放。早前自绘文字乘了一次、egui 内部又乘一次，
高 DPI 下自绘文字比原生控件大一倍。

### 2.3 布局常量

| 常量 | 值 | 说明 |
|---|---|---|
| `TOPBAR_HEIGHT` | 44 | 顶栏 |
| `STATUSBAR_HEIGHT` | 0 | 状态栏已取消 |
| `LIST_PAD_X` | 8 | 列表左右留白，**同时是拖动区** |
| `ROW_GAP` | 6 | 行卡片垂直间隙 |
| `ROW_HEIGHT_MIN` | 58 | 行高下限 |
| `SCROLLBAR_RESERVE` | 10 | 右缘让给滚动条；**必须**对齐 egui `ScrollStyle::floating().bar_width` |
| `SEARCH_EXPAND_W` | 140 | 搜索框展开宽度 |
| `LINE_H` | 1.35 | 行内文字行高倍率 |
| `CARD_RADIUS` / `CARD_STROKE` | 0 / 1 | 历史卡片：直角 + 1px 描边 |
| 卡片最小宽度 | History 160 / Pinned 120 / Detail 180 / Rail 88 | 分配时的下限 |

### 2.4 层次关系

```text
窗口底 bg
  └─ 顶栏 surface_variant          （比窗口底略亮）
  └─ 历史卡片 list_bg               （比窗口底**更暗**一档）
       └─ 行卡片 surface_raised     （提亮的浮层）
            └─ 悬停/选中半透明叠加
```

「亮卡片、暗容器」是刻意的：历史列表整块压暗，行卡片提亮浮在上面。
早前卡片 frame 用 `surface`，结果列表四周露出一圈底色（浅色主题下
是白边，用户反馈「列表边上还是白色」）。

### 2.5 圆角改造

#### 2.5.1 窗口圆角：**半径改不了**

现状：窗口圆角**早就有了** —— `chrome::apply_rounded_corners` 在
`window.rs:330` 被调用。但：

> **CAUTION: `DWMWA_WINDOW_CORNER_PREFERENCE` 收的是枚举，不是数值。**
> 可选值只有 `DWMWCP_DEFAULT` / `DWMWCP_DONOTROUND` / `DWMWCP_ROUND` /
> `DWMWCP_ROUNDSMALL`（**没有 ROUNDLARGE**），实际半径由系统决定 ——
> Win11 在 100% 缩放下约 8 物理像素，150% 下约 12，折算成逻辑点
> 始终是 **8 左右**。想让它更圆，改代码是没有用的。

「窗口圆角不够明显」是**真实存在的限制**，不是没改到位。

三条出路：

| 方案 | 效果 | 代价 |
|---|---|---|
| **A. 接受系统 8pt，靠内容层主导**（推荐） | 卡片/行圆角放大，视觉主体是卡片，窗口圆角只作外层 | 无 |
| B. `SetWindowRgn` + `CreateRoundRectRgn` | 半径任意 | **像素级区域裁剪，四角无抗锯齿，有明显锯齿**；且 DPI 变化、尺寸变化都要重建 region |
| C. 自绘圆角窗口（四角透明） | 半径任意 + 抗锯齿 | 需要交换链 alpha（`VK_COMPOSITE_ALPHA_PREMULTIPLIED`）+ 全链路 premultiplied 绘制 + 圆角处抗锯齿；且要与 DWM 圆角共存（否则双重圆角）。改动面极大 |

原型的 `--win-radius: 16px` 是**演示值**，只为了让「圆角更大是什么观感」
可见；真实可达值约 8pt。换句话说：**原型里窗口圆角那一圈当作理想上限看，
不要当成能落地的数值。**

#### 2.5.2 内容圆角：真正要改的部分

| # | 项 | 现状 | 改为 |
|---|---|---|---|
| 1 | 卡片圆角 `CARD_RADIUS` | `0.0`（直角） | `12.0` |
| 2 | 行卡片圆角 | `pal.radius_sm` = 4 | `12.0` |
| 3 | 卡片内边距 | `space_sm(8)` | `13.0` |
| 4 | `card_area` 缩进 | **无**（卡片贴窗口边缘） | 四周 `space_sm(8)` |

第 3、4 条必须和第 1、2 条一起改，否则会出两类可见缺陷：

```text
card_area() = 顶栏之下 → 窗口底部，gap 默认 0，卡片紧贴窗口边缘
```

- **卡片不缩进** ⇒ 卡片四角（自绘 12）与窗口四角（DWM 8）重叠错开，
  边缘露出两道弧。加 8pt 缩进后变成「窗口圆角 → 缝 → 卡片圆角」的层次。
- **内边距 < 卡片圆角** ⇒ 行卡片的方角从卡片圆角底下露出来。
  硬约束：`card_pad ≥ card_radius`。取 13 而不是 12：相切时行卡片边缘
  正好落在圆角切点上，亚像素舍入会让它偶尔露出一点点方角。

这个方案**不依赖 DWM**：Win10 上 DWM 属性返回 `E_INVALIDARG`（代码里
已优雅降级），窗口仍是直角，但卡片圆角照常成立——不会出现
「窗口直角 + 卡片圆角探出边缘」的破相。

#### 2.5.3 三个圆角值的关系

```text
窗口 8（系统）  <  卡片 12  =  行 12
```

不是「窗口最大」——恰恰相反。因为窗口圆角被系统锁死在 8，想让整体
显得圆润，只能让**内容圆角比窗口圆角更大**，由卡片承担视觉主体。
用户提到的「与窗口圆角对齐」在这个前提下有两种理解：

1. **数值相等** ⇒ 只能都取 8，比现在还小，与「弧度不够」的诉求相反；
2. **视觉同一套体系**（当前采用）⇒ 卡片与行取同一个值（12），
   窗口圆角作为最外层，三层圆角梯度 8 → 12 → 12。

若确实想要 1，得先解决窗口圆角的半径问题（见 §2.5.1 的方案 B/C）。

---

## 3. 交互清单

### 3.1 顶栏

| 元素 | 触发 | Op | 备注 |
|---|---|---|---|
| DragRegion·left/right | 按下拖动 | — | `WM_NCHITTEST` → `HTCAPTION` |
| SearchIcon | 单击 | — | 切换折叠；展开时**立即**把焦点给输入框 |
| SearchBox | 输入 | — | 单行；走 FTS 过滤 |
| close | 单击 | `CloseWindow` | 「关闭到托盘」，**不是退出** |
| clear | 单击 | `ClearAll` | 破坏性，hover 用 `danger` |
| **pin** | 单击 | `SetBool{Ui, always_on_top}` | 新增 · 切换窗口置顶，见 §3.1.1 |
| settings | 单击 | `ToggleSettings` | 打开独立设置窗口 |

#### 3.1.1 窗口置顶（新增）

**不需要新建机制**——基础设施已经齐了：

| 已有 | 位置 |
|---|---|
| `presence::set_topmost(hwnd, bool)` | 启动时应用一次 |
| 每帧比对 `always_on_top` 配置并应用 | 帧循环末尾，用 `applied_topmost` 去重 |
| `Op::SetBool { group: Ui, field: "always_on_top" }` | `set_bool_field` 已有该分支 |

所以按钮只需要：**发一个 `Op::SetBool`**。写入后每帧比对会自动调
`SetWindowPos`，而设置面板里那个同名开关也会同步（两处读同一个配置字段）。

**按钮几何**：顶栏按钮从 3 枚变 4 枚。

```text
            3 枚              4 枚
btn         27.28   →  27.28   （topbar_h × 0.62，不变）
btn_area 宽 113.84  →  157.12  （btn×n + gap×(n−1)，gap = 16）
step        37.95   →  39.28   （btn_area 宽 / n）
热区 sq     23.53   →  24.35   （step × 0.62）
字形        16      →  16      （icon_size，不变）
```

> CAUTION: `btn_count` 与 `step` 的分母**写在两个地方**
> （`draw_topbar` 的 `btn_count` 与 `draw_topbar_icons` 的 `step`）。
> 两边不一致 ⇒ 按钮区与实际按钮错位，表现为「最右那枚被搜索框压住」
> 或「点齿轮变成拖窗口」。改一处必须同步另一处。

**图标与配色**：

- 未置顶 → `Icon::Unpinned`（描边图钉）+ `text_dim`
- 已置顶 → `Icon::Pinned`（实心图钉）+ **`accent`**（已置顶）/ `accent_hover`（悬停）

只换图标形状是不够的：16pt 下实心与描边的差异很小，不配色用户分不出
「已置顶」和「没置顶」。所以 `icon_button` 需要加一个 `active` 参数，
配色优先级为 **破坏性悬停 > 激活态 > 悬停提亮 > 常态弱化**。

**位置**：放在「清空」与「设置」之间（自左向右 close / clear / **pin** / settings）。
「设置」保持最右端——`draw_topbar` 的右段拖动区是按 `topbar_settings`
的**真实右缘**算的，动了顺序要一并复核。

### 3.2 列表行

| 元素 | 触发 | Op | 备注 |
|---|---|---|---|
| Row | 单击 | `Select(id)` | 选中 |
| Row | 双击 | `Copy(id)` | 复制到剪贴板 |
| Row | 滚轮 | — | 仅悬停本列表时消费；步长 ×3（一个刻度滚 3 行） |
| Act·pin | 单击 | `TogglePin(id)` | 置顶 ⇄ 取消置顶；**任何状态都可点** |
| Act·copy | 单击 | `Copy(id)` | 与双击同义 |
| Act·delete | 单击 | `Delete(id)` | hover 转 `danger` |

### 3.3 三条必须记住的交互约束

1. **顶栏拖动区必须「挖空」**。`WM_NCHITTEST` 命中拖动区会返回
   `HTCAPTION`，系统随即发 `WM_NCLBUTTONDOWN`（非客户区消息），
   **不会产生** `WM_LBUTTONDOWN`，egui 收不到点击。整块上报的症状是
   「齿轮/垃圾桶/搜索框全都点不动」。

2. **行右缘必须让出 `SCROLLBAR_RESERVE`**。egui 的浮动滚动条覆盖内容
   右缘 10pt 且**不占布局宽度**，而它在内容闭包**之后**才注册交互，
   hit-test 取「最上层」⇒ 滚动条压过行内所有控件。症状极隐蔽：
   条目少、不满一屏时**没有滚动条**，一切正常；条目一多删除就点不动。

3. **行交互必须挂在 ScrollArea 的内容 `ui` 上**。挂根 `ui` 时裁剪矩形是
   整个窗口，被滚出视口的半截行仍然可点——点在卡片标题甚至顶栏上
   都会选中列表条目。

---

## 4. 状态说明

### 4.1 行状态

| 状态 | 表现 |
|---|---|
| 常态 | `surface_raised` + 1px `border_subtle` |
| 悬停 | 叠加 `row_hover`；**描边保持不变**（否则边框会「消失一瞬」，视觉跳变） |
| 选中 | 叠加 `row_selected` + 左侧 2px `accent` 条；预览转 `text_bright`，类型图标转 `accent` |
| 置顶 | 移入内嵌置顶区；pin 图标由 Unpinned（描边）变 Pinned（实心） |

### 4.2 搜索状态

| 状态 | 表现 |
|---|---|
| 折叠（默认） | 只留 17pt 放大镜，顶栏左侧整片让给拖动区 |
| 展开 | 输入框向左弹出 140pt；拖动区收缩到输入框左缘，右端控件之后仍剩约 46pt |
| 有查询无结果 | 空态文案「没有匹配的结果」 |

### 4.3 列表状态

| 状态 | 表现 |
|---|---|
| 空 | 「还没有剪贴板记录」 |
| 置顶为空 | 置顶区整段**不占高度**（否则为 0 条记录占掉一条横幅） |
| 置顶过多 | 封顶 34%，其余靠置顶区自己的滚动 |
| 可滚动 | 出现浮动滚动条，覆盖右缘 10pt |
| 极窄 | `text_x1 − text_x0 < 20` 时跳过文字绘制，**但行仍可点** |

> 最后一条是个已修的坑：早前在「太窄画不下文字」处裸 `return`，
> 跳过了 `resp.clicked()`，于是极窄栏时整行不可点，却仍然画了
> hover 底色和选中条——视觉上像是能点，一点没反应。

### 4.4 主题

`FollowSystem`（默认）/ `Dark` / `Light`。
半透明 token 的对比度必须先合成到 `surface` 再算，不能直接取值比较。

### 4.5 窗口置顶状态（新增）

> **语义（已确认）**：始终显示在最上层，不会被其它应用遮住。

| 状态 | 表现 |
|---|---|
| 未置顶 | 图钉描边 · `text_dim`；窗口会被其它窗口盖住、失焦后沉到下层 |
| 已置顶 | 图钉实心 · `accent`；窗口始终在其它窗口之上，**包括它失去焦点时** |
| 悬停 | 已置顶 → `accent_hover`；未置顶 → `text` |

「包括焦点」这一条是**容易被误解的地方**：`HWND_TOPMOST` 只改 z 序，
**不抢焦点**——你点了别的程序，焦点照样在别的程序上，只是主窗口不会
被盖住。这正好是「一直看得见，但不打断你」的行为，也是置顶该有的样子。
（`SetWindowPos` + `HWND_TOPMOST` 在 `presence::set_topmost` 里已实现。）

> 原型的桌面场景（两个假的「别的程序窗口」）就是为了让这个差异可见：
> 置顶关掉时它们会压住主窗口，打开时主窗口完整浮在上面。
> 光看单窗口截图是看不出置顶的——它改变的是 z 序，不是外观。

### 4.6 滚动条状态（本轮改版）

改成 **overlay 风格：静止隐形、悬停淡入** —— 这是当下的主流做法
（macOS / Win11 / 主流 Web 组件库一致）。三级反馈：

| 状态 | 表现 |
|---|---|
| 静止 | **完全透明**，列表区域干净无干扰 |
| 列表悬停 | 细滑块淡入（视觉宽 4pt，`--border` 60% 透明） |
| 滑块悬停 | 加深为 `border_strong` + 加粗到 6pt（「抓住了」的反馈） |

两个必须守住的约束：

1. **热区仍是 `SCROLLBAR_RESERVE = 10`**，视觉宽度靠 `border +
   background-clip` 收窄到 4。<br>
   不能为了「细」去改那个常量 —— 行内按钮的右基准靠它避让，
   改小会让删除按钮重新落回滚动条下面（那是个很难自查的老 bug：
   条目少时没滚动条、一切正常；条目一多就点不动）。
2. **不能把滚动条彻底隐藏**（`display: none`）。完全看不见会让用户
   不知道这里可以滚。悬停显形是「干净」与「可发现」之间的平衡点。

> 原型里加了 `#scroll` 这个 hash（→ `.force-scrollbar`），用来在
> headless 截图里强行显形 —— 截图模拟不了 `:hover`。

### 4.7 窗口状态

隐藏到托盘（顶栏「关闭」）/ 失焦隐藏（可配置，默认关）/ 最小化。
隐藏态下仍要 poll 消息并**排空**事件，否则事件积压会在下次打开时
一次性回放，表现为「一打开就误点」。

---

## 5. 原型怎么用

打开 `main-window.html` 后：

**桌面是一个场景，不是一个窗口截图**：里面有两个假的「别的程序窗口」，
置顶时会从主窗口边缘露出来。这样「置顶」这个纯 z 序的行为才有对照物。

- **顶部第一排**：主题、窗口尺寸（逻辑点）、**窗口置顶**、**圆角风格**。
  默认配置 720×800 **物理**像素 ÷ DPI 1.5 = 480×533 逻辑点。
- **顶部第二排**：状态切换（正常 / 悬停行 / 选中行 / 搜索展开 / 空态 /
  含置顶 / 窄窗）。
- **右侧面板**：组件树、Token、交互、状态四页。
  Token 页每个值都可直接改，**改完立即生效**（含 `--win-radius`、
  `--card-radius`、`--row-radius`、`--card-pad`、`--cardarea-pad`）——
  这正是先在 HTML 上试、再决定要不要动 Rust 的意义。

调完之后，把认可的取值告诉我，我再改 Rust。

---

## 6. 待决项（原型阶段发现，需要拍板）

这几条不是 bug，是**设计取舍**，原型阶段定下来比实现后返工便宜。

| # | 事项 | 现状 | 可选方向 |
|---|---|---|---|
| 1 | **来源名过长** | `来源 · 时间` 走 `elide_text`，长 exe 路径会被截成省略号（原型首行即是该情形） | ① 存 basename 而不是完整路径（需确认来源是从哪取的名字）② 保持截断 ③ 悬停时显示完整（tooltip） |
| 2 | **「复制」图标语义偏** | 图标集里没有 clipboard，`Op::Copy` 用 `Icon::File`（文档）近似 | ① 加一枚新图标（PUA 还剩码位）② 换成别的语义 ③ 改用文字按钮 |
| 3 | **关闭用横线不用叉** | 「关闭到托盘」语义上是收起，叉在中文界面更常被理解成退出 | 已定，但可复核 |
| 4 | **没有最小化按钮** | 与「关闭到托盘」效果近似（都让窗口消失），程序常驻托盘 | 已定 |
| 5 | **行的预览不截断、按宽度折行** | 行高因此可变，长内容会把行撑高 | 已定（用户要求「完整显示内容」） |
| 6 | **置顶区封顶 34%** | 置顶过多时靠置顶区自己滚动 | 34% 是拍的，可按实际使用调 |
| 7 | **窗口圆角在 Win10 不生效** | DWM 属性返回 `E_INVALIDARG`，代码已优雅降级为直角 | 若必须 Win10 也圆角，只能 `SetWindowRgn`（像素级裁剪、无抗锯齿，边缘会有锯齿）——不推荐 |
| 8 | **三个圆角值** | 窗口 12 / 卡片 10 / 行 6 是本轮的提案值 | 可以整体放大或统一（例如全部 8）；原型里改 token 即可实时比对 |
| 9 | **置顶按钮的默认状态** | 配置默认 `always_on_top: true` | 若希望默认不置顶，改 `config.rs` 的默认值而不是按钮 |

> 第 1 条我一度判断成「长来源名会排到按钮底下被盖住」，
> 读了 `preview_width()` 才发现那里已经减掉了 `acts_w`——
> 那是**截断**不是**重叠**。这类「看着像 bug」的判断，先读代码再下结论。
> 原型夹具里保留了一条超长来源名的行，就是为了随时能验证这一点没变。

---

## 7. 截图

`_shots/` 下由无头 Chrome 生成，可直接对比改动前后：

| 文件 | 内容 |
|---|---|
| `v2-normal.png` | **v2 · 未置顶**（被其它窗口遮挡） |
| `v2-wintop.png` | **v2 · 已置顶**（覆盖在其它窗口之上） |
| `v2-scrollbar.png` | 滚动条显形态（`#normal+scroll`） |
| `v2-dark.png` | 深色 |
| `v2-dark-scrollbar.png` | 深色 · 滚动条显形 |
| `v2-embedded.png` | 含内嵌置顶区 |
| `zoom-normal.png` / `zoom-wintop.png` | v1 对照（圆角较小、按钮贴顶） |

重拍：

```bash
CHROME=~/.agent-browser/browsers/chrome-*/chrome.exe
"$CHROME" --headless --disable-gpu --no-sandbox \
  --virtual-time-budget=2200 --window-size=1140,900 \
  --screenshot=_shots/v2-normal.png \
  "file://$PWD/main-window.html#normal"
```

> NOTE: **不要加 `--hide-scrollbars`** —— 那样永远看不到滚动条，
> 改了半天无从验证。要看滚动条显形态用 `#scroll`（见 §4.6）。

> NOTE: 截图路径**用正斜杠**。反斜杠在 bash 双引号里会让 `\$nm` 之类的
> 变量引用失效，Chrome 会写到别处，表现为「命令成功但文件没出现」。

> NOTE: 视口宽度要**两头兼顾**，第一版两个值都踩过坑：
>
> - **1400 太宽** —— 窗口在整图里只占一半，12px 圆角肉眼分不出，
>   对比「圆角 / 直角」两张图居然看不出差别；
> - **1000 太窄** —— 右侧规范面板固定 420px，留给画布只剩 580，
>   640 宽的桌面会被裁，主窗口直接溢出桌面。
>
> 1140 是实测下来能同时满足「圆角看得清」和「桌面装得下」的值。