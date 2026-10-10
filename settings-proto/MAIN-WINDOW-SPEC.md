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
App  (无边框窗口；整个界面按「逻辑点」布局，外层 zoom = pixels_per_point)
│
├─ TopBar  h=44 · bg=surface_variant · 底边 1px border_subtle
│  │        内边距左右各 space_md(12)
│  ├─ DragRegion·left ........ 左缘 → 搜索图标左缘 − 2pt
│  ├─ SearchBox ............. w=140 · surface + 1px border · r=6 · font_md(14)
│  │  └─ TextEdit ........... hint「搜索历史…」 · margin(2,0) · frame=NONE
│  ├─ SearchIcon ........... 17×17 · 折叠 ⇄ 展开（不占位，点击才弹出）
│  ├─ IconButton·close ..... Icon::Minimize ·「关闭到托盘」· Op::CloseWindow
│  ├─ IconButton·clear ..... Icon::Trash · 破坏性 · hover=danger · Op::ClearAll
│  ├─ IconButton·settings .. Icon::Settings · Op::ToggleSettings
│  └─ DragRegion·right ..... 设置按钮**真实右缘** → 窗口右缘
└─ CardArea  (bg)
   └─ Card·History  整卡底=list_bg · 1px border_subtle · r=0 · 无标题行
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
            │  背景 surface_raised + 1px border_subtle + r=4
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
│ [图标]  来源程序 · 时间                  ⊕ ⧉ ✕│  ← top_band   (pad_v=5, meta_h≥11)
│         预览文本，可折行，不截断               │
│         类型 · 大小                              │  ← bot_band   (右对齐)
└──────────────────────────────────────────────┘
   ↑ preview_band 与上下两段各留 2pt
```

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
| settings | 单击 | `ToggleSettings` | 打开独立设置窗口 |

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

### 4.5 窗口状态

隐藏到托盘（顶栏「关闭」）/ 失焦隐藏（可配置，默认关）/ 最小化。
隐藏态下仍要 poll 消息并**排空**事件，否则事件积压会在下次打开时
一次性回放，表现为「一打开就误点」。

---

## 5. 原型怎么用

打开 `main-window.html` 后：

- **顶部第一排**：主题切换、窗口尺寸（逻辑点）、DPI 缩放。
  默认配置 720×800 **物理**像素 ÷ 1.5 = 480×533 逻辑点。
- **顶部第二排**：状态切换（正常 / 悬停行 / 选中行 / 搜索展开 / 空态 /
  含置顶 / 窄窗）。
- **右侧面板**：组件树、Token、交互、状态四页。
  Token 页每个值都可直接改，**改完立即生效**，用于快速验证
  「换一个值会怎样」——这正是先在 HTML 上试、再决定要不要动 Rust 的意义。

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

> 第 1 条我一度判断成「长来源名会排到按钮底下被盖住」，
> 读了 `preview_width()` 才发现那里已经减掉了 `acts_w`——
> 那是**截断**不是**重叠**。这类「看着像 bug」的判断，先读代码再下结论。
> 原型夹具里保留了一条超长来源名的行，就是为了随时能验证这一点没变。

---

## 7. 截图

`_shots/` 下由无头 Chrome 生成，可直接对比改动前后：

| 文件 | 内容 |
|---|---|
| `main-normal.png` | 浅色 · 正常 |
| `main-dark.png` | 深色 · 正常 |
| `main-pinned.png` | 含内嵌置顶区 |
| `main-selected.png` | 选中行 |

重拍：

```bash
CHROME=~/.agent-browser/browsers/chrome-*/chrome.exe
"$CHROME" --headless --disable-gpu --no-sandbox --hide-scrollbars \
  --virtual-time-budget=2500 --window-size=1400,1000 \
  --screenshot=_shots/main-normal.png \
  "file://$PWD/main-window.html#normal"
```