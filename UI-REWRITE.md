# 新 UI 架构：多卡片工作区

> 目标：替换掉 `layout.rs`(3230行) + `view.rs`(1678行) + `panels.rs`(938行)
> 这套「降级阶梯 + 双实现」的手工布局，换成卡片实体模型。
> 渲染层（`gfx`）**保留**：逐批次 scissor 与尺寸同步已实测修复，不重写。

## 为什么旧架构必然反复出问题

旧架构有一个致命结构缺陷：**同一件事算两遍**。

| 事实 | 布局侧 | 绘制侧 | 后果 |
|---|---|---|---|
| 某模块是否折叠 | `Tier::visibility()` | `is_veiled()` | 两边判据要人工保持同步 |
| 面板矩形 | `layout::solve()` | `solved.rects[i]` | 索引错位则内容画到别处 |
| 降级档位 | `Tier::for_width()` | 分支判断 | 改一处忘另一处 ⇒ 实机错、单测过 |

历史症状：详情面板折叠态、状态栏提示压按钮，都是这个成因。
`solve()` 单函数 292 行，5 档降级阶梯，档位与面板的交叉组合
是 4×5 = 20 种，每种都要在两处写对。

## 新架构：卡片是一等实体

###核心倒转

旧：先按宽度选一个「档位」，再让每个面板查自己的可见性。
新：**每张卡片自己知道该显示什么**，布局只是问每张卡��要多少宽度。

```text
Workspace（工作区）
  └── Cards: Vec<Card>          卡片列表，顺序即Z 序
        Card {
          id: CardId,
          kind: CardKind,       History / Pinned / Detail / Rail
          host: CardHost,       Docked | Window
          rect: Rect,           由布局求解，绘制层直接读
          size: Vec2,           用户拖出来的目标尺寸
          collapsed: bool,      折叠 = 只留把手
        }
```

`LayoutSolver` 只做一件事：给每张卡分配一个 `Rect`，然后**返回**。
绘制层遍历卡片、读`card.rect`，不做任何二次判断。

### 分配算法（取代 5 档降级阶梯）

```text
1. 收集可见卡片的最小宽度需求:  Σ min_width
2. 若 Σ min_width ≤ 可用宽度 → 全部展开，按用户比例分配剩余空间
3. 否则按优先级依次折叠（Detail → Rail → Pinned），
   每折叠一张就重新检查是否够用
```

- **无档位**：宽度是连续函数，不是 5 个离散档。
- **优先级写在一处**：`CardKind::collapse_priority()`。
- **可测试**：判据是「分配后所有卡片矩形互不相交且都在区内」，
  与 `solve()` 输出无关——不需要两处保持一致。

### 置顶的两种模式

用户要求：置顶有「共用单栏」与「独立子窗口分栏」两种。

```rust
enum PinnedMode {
    /// 共用单栏：置顶与历史共享一个列表容器，顶部一段，共享选中态
    SharedColumn,
    /// 独立分栏：置顶是独立卡片，可拖出成子窗口
    OwnCard,
}
```

第一阶段只实现 `SharedColumn`。它反而比旧实现更简单：
置顶不是独立面板，而是历史列表顶部的一段，数据由同一个
列表容器按 `pinned` 分区渲染。

### 卡片分离为子窗口

```text
拖拽卡片标题栏 → 移出工作区边界 ⇒ CardHost 变为 Window
                → 创建真实 Win32 子窗口，各自带独立 EventLoop 状态
拖回工作区内 ⇒ 销毁子窗口，卡片回到 Docked
```

第二阶段实现。接口先留好：`CardHost` 已经是枚举，
`Workspace` 对两种 host 都只读 `card.rect`。

## 分层

```text
ui/
  card.rs        卡片模型：CardId / CardKind / Card / CardHost   【新】
  workspace.rs   工作区：卡片集合、顺序、增删、Z 序              【新】
  solver.rs      布局求解：需求收集 → 贪心分配 → 返回 Rect 列表  【新】
  paint.rs       绘制：遍历卡片读 rect，不做二次判断             【新】
  theme.rs       设计令牌（保留并精简）
  icons.rs       图标（保留）
  thumbnail.rs   缩略图缓存（保留）
  presence.rs    托盘（保留）
  renderer.rs    Vulkan 桥（保留，已修好 scissor）
  app.rs         帧循环（保留，改为遍历 workspace）
```

被删除：`layout.rs`（3230）、`view.rs`（1678）、`panels.rs`（938）、
`titlebar.rs`（833）。合计约 6700 行 → 预计新代码 2000 行以内。

## 实施顺序（每步都可验证）

| 步 | 内容 | 验证方式 |
|---|---|---|
| 1 | `card.rs` + `workspace.rs` + `solver.rs` + 布局守卫测试 | 纯逻辑测试：任意宽度下不重叠、不越界 |
| 2 | `paint.rs` 单窗口绘制，跑通主窗口 | 截图 + 像素探针（已有 `verify_render.ps1`） |
| 3 | 置顶 `SharedColumn` 模式 | 截图确认置顶与历史同栏 |
| 4 | 卡片拖拽分离为子窗口 | 端到端脚本：拖出 → 独立窗口 → 拖回 |

**每一步都保持主程序可编译可运行**，不搞一次性大爆炸。
