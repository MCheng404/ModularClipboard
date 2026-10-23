# 代码审查发现清单（历轮合并）

> 本文档合并了项目全部三轮系统性代码审查。每轮独立编号、互不影响，
> 查阅时按日期定位。状态含义：`✅ 已修` / `⬜ 待修` / `⚠️ 待确认`。
>
> **不要**把不同轮次的编号混用——`DF-01`（第二轮）与 `DL-01`（第三轮）
> 是不同的东西。

## 索引

| 轮次 | 日期 | 规模 | 状态 |
|---|---|---|---|
| 第一轮 | 2026-10-08/09 | 4 Agent，46 条 | 全部闭环 |
| 第二轮 | 2026-10-12 | 6 Agent，分批修复 | 大部分闭环，未修项附理由 |
| 第三轮 | 2026-10-23 | 3 Agent + 手动 UI 审查，23 条 | P0/P1 已修 8 条，其余列后续 |

---

## 第一轮：2026-10-08/09（4 Agent 并行）

> 46 条发现，全部闭环。含窗口/渲染、测试质量自省、明确不做的项。

4 个 Agent 并行审查，共 **46 条**发现，**已全部闭环**。本文档记录全部条目与处置状态，
避免遗漏。状态含义：`✅ 已修` / `⬜ 待修` / `⚠️ 待确认`。

---

## 一、已修（本轮）

### 窗口/渲染层（gfx）

| # | 问题 | 根因 | 状态 |
|---|---|---|---|
| G1 | 拖动区与边框是**进程级全局**，多窗口互相串味 | `chrome.rs` 三个全局原子量；文档里写着「一个进程只有一个主窗口」，加了子窗口后前提不成立 | ✅ 改 per-HWND（`CHROME` 表按 HWND 索引） |
| G2 | 设置窗口**按主窗口的表面能力**建交换链 | `frame.rs:1444` 用 `gpu.surface()`（主窗口）查 caps，而 `create_swapchain` 用 `self.surface`。字段注释说早前已修，实际只修了一半 | ✅ 改 `self.surface` |
| G3 | `PeekMessageW` **无 hwnd 过滤**，多窗口抢同一条消息队列 | 传 `None` = 取整个线程的消息，谁先 poll 谁抢走全部 | ✅ 改 `Some(self.hwnd)`，`WM_QUIT` 单独 peek |
| G4 | 附属窗口 `poll_for(Some(16ms))` 阻塞主循环 | `poll_for` 带超时会真的睡等 | ✅ 改 `poll_for(None)` |
| G5 | 子窗口 `VkSurfaceKHR` 永不销毁 | `Gpu` 无 `destroy_surface`；`ChildWindow`/`SettingsWindow` 无 `Drop` | ✅ 已修 |
| G6 | acquire 之后出错会**永久卡死**该渲染器 | `pending` 只在 `present` 清除，错误路径不恢复 | ✅ 已修 |
| G7 | `new_for_surface` 的格式校验用主窗口的 formats | `swapchain_format_of(gpu)` 走主窗口快照 | ✅ 已修 |
| G8 | `Swapchain::new_for_surface` 的 `clamp` 在 `min>max` 时 panic | 与 `frame.rs::clamp_axis` 不一致 | ✅ 已修 |
| G9 | `EventLoop::poll` 每轮清零 `modifiers` | 与「累积事件缓冲」语义冲突 | ✅ 已修 |
| G10 | `record()` 对 `DrawBatch.clip` 无兜底钳制 | 裁剪合法性全靠调用方 | ✅ 已修 |
| G附A | `Painter::drop` **漏销毁** `user_tex` 与 `placeholder` | 每窗口泄漏一份 Vulkan 对象 | ✅ 已修 |
| G附B | `TextureStore::update_user` 用 `FONT_FORMAT` 建用户纹理 | RGBA 会被压成灰度；好在全仓无调用者 | ✅ 已修（建议删除） |
| G附C | `staging_size` 对未支持格式 `panic!` | 库函数不该 panic | ✅ 已修 |
| G附D | `select_device` 只用主窗口表面查呈现支持 | 双显卡 + 跨屏拖子窗口有风险 | ⚠️ 待实机验证 |

### 主循环与生命周期（app / childwin / settingswin / presence）

| # | 问题 | 根因 | 状态 |
|---|---|---|---|
| F1 | 多个 `EventLoop` 抢同一条消息队列 | 与 G3 同源 | ✅ |
| F2 | 隐藏/最小化/acquire 失败三条 `continue` 都不驱动附属窗口 | 子窗口留在屏幕上但不渲染不 poll | ✅ 已修 |
| F3 | 设置窗关闭态**只 poll 不排空**，重开时被旧事件引爆 | 用户体感「点齿轮没反应 / 一闪就没」 | ✅ 改 `take_pending()` |
| F4 | 每帧被附属窗 `poll_for(16ms)` 阻塞 | 与 G4 同源 | ✅ |
| F5 | 最小化时写回窗口坐标 → 下次启动跑到屏幕外 | `GetWindowRect` 对最小化窗口返回 `-32000` | ✅ 已修 |
| F6 | 三个 UI 配置项只写不读（`always_on_top`/`hide_on_focus_lost`/`start_minimized`） | 开关能改、能落盘、重启无效 | ✅ 已修 |
| F7 | `App::should_quit` 是死状态，UI 无退出入口 | 无 `Op::Quit`，没人置 true | ✅ 已修 |
| F8 | `reset_settings_scroll` 改的是**主窗口的 Context** | 滚动位置存在各自 Context 里，等于没写 | ✅ 改用 `self.ctx` |
| F9 | 托盘「清空历史」不请求重绘 | 列表与计数显示旧数据 | ✅ 已修 |
| F10 | `SettingsWindow::skip_frames` 恒为 0 的死字段 | 唯一使用点从不进分支 | ✅ 删除 |
| F11 | `ChildWindows::Drop` 空实现 + `close_all` 重复销毁 | 语义重复（幂等，不致错） | ✅ 已修 |
| F12 | `PlatformOutput` 全仓无人消费 | Ctrl+C 复制、光标形状失效 | 部分修复（光标已接，CopyText 故意不接） |

### 绘制层（paint.rs）

| # | 问题 | 根因 | 状态 |
|---|---|---|---|
| P0-1 | 列表行画到窗口左上角（`pos2(0.0, …)`） | **egui 的 Rect 永远是全局坐标**，`ScrollArea` 的 `content_max_rect` 只是可用区域偏移，不做坐标变换 | ✅ 改回绝对坐标 |
| P0-2 | 行的点击注册在根 `Ui` 上，绕过裁剪 | — | ✅ 此前已用 `ui.interact` |
| P0-3 | 内嵌置顶区与历史栏**整块重叠** | `draw_embedded_pinned` 事后叠画 | ✅ 已修（`embedded_pinned_height` 先切分 body） |
| P1-4 | 两个列表共用一个 `scroll_offset` 字段 | 有置顶条目时历史列表滚不动 | ✅ 已修（早前已修） |
| P1-5 | `vertical_scroll_offset` 每帧强制回写 ⇒ 拖滚动条无效 | 不读回 `ScrollAreaOutput` | ✅ 已修（早前已修） |
| P1-6 | `meta_area` 的 `max.y` 多叠加 `text_area.min.y` | 「来源 · 时间」跑到卡片外 | ✅ |
| P2-7 | 拖动区伸进搜索框左侧约 22pt | `left_end` 读 `edit_rect.min.x` 而非外框左缘 | ✅ |
| P2-8 | 字号被 DPI **乘了两次**（`sized()` × egui 的 ppp） | `FontId` 应给逻辑字号 | ✅ 已修（改动面大，需重过视觉） |
| P2-9 | `row_height_for` 测量宽度比渲染宽度窄 8pt | 两处各写一份算式 | ✅ 已修 |
| P附1 | 窄卡片提前 return 跳过 `clicked()` | 整行不可点但仍画 hover | ✅ 已修 |
| P附2 | `draw_switch` 同矩形注册两次 interact | 靠「后注册者优先」侥幸正确 | ✅ 占位用 `Sense::empty()`，只保留一条显式 `interact`（结构清理，无行为差异） |
| P附3 | `hit.clear()` 只在 `draw()` 做 | 设置窗/子窗与主窗共用 `UiState`，热区互相清、关窗后残留 | ✅ 改 `HitTable` 按 `HitNs{Main,Child,Settings}` 分命名空间 |
| P附4 | `search_box_accepts_typed_text` 焦点断言恒真 | `|| focused().is_some()` 恒成立 | ✅ 已修 |

### 布局求解（solver / workspace / card）

| # | 问题 | 根因 | 状态 |
|---|---|---|---|
| S1 | `PinnedMode::OwnCard` 是死开关，「独立分栏」完全无效 | `pinned_is_embedded()` 只看 `host`，从不读 `pinned_mode` | ✅ 已修 |
| S2 | 宽度为 0 的卡被静默排除 ⇒ `n` 少算 ⇒ `avail` 反而变大 | 布局对窗口宽度**非单调** | ✅ `.max(1.0)` |
| S3 | `apply()` 用 `zip` 静默截断 | 多出的卡保留上一帧 rect，照画 | ✅ 长度不等则拒绝 |
| S4 | 三套宽度需求口径并存 | `solve` / `layout_width` / `required_width` 判据不同 | ✅ 合并为 `solver::layout_demand` 单一口径，`Card::layout_width` 删除 |
| S5 | 极窄窗口下宽度可缩到亚 1pt | 无下限 | ✅ `avail` 下限 = `n` |
| S6 | 负 `gap` 把卡片画到窗口外 | 无 `max(0.0)` | ✅ |
| S7 | 落位不对齐像素栅格、不钳右边界 | ppp=1.5 时描边发虚 | ✅ 已修 |
| S8 | 模块文档描述已删除的折叠行为（含 broken link） | 与当前契约字面相反 | ✅ |
| S9 | 不变式 3「`is_visible()` 为真则 rect 非零」是假的 | 内嵌置顶 `is_visible()==true` 但 rect 为 0 | ✅ 已修 |
| S10 | 0 宽矩形这条最易复现的回归测不到 | `check` 的 `continue` 把 0 宽当无罪 | ✅ 已修 |
| S附 | `empty()` 的 `gap: 6.0` 与 `default()` 的 `0.0` 不一致 | 与 S6 同源 | ✅ 已修 |

---

## 二、测试质量问题（自省）

本轮最值得记录的不是某个 bug，而是**同一个 bug 反复逃脱测试**的模式：

1. **列表渲染路径零覆盖** —— `Harness::new` 建空列表，`draw_history_body`
   走 `draw_empty` 直接 return，`draw_item_row` 从未执行。505 条测试全绿，
   没有一条真的画过一行。→ 新增 `Harness::with_items(n)`。
2. **假守卫（一）** —— 我为 P0-1 写的第一版判据是「滚动后下半部分点得到
   条目」。注入 bug 实测**仍然绿**：60 条记录整体平移 200px 不留空白，
   任意位置都还有行，「点得到」恒真。→ 改成断言**身份**（滚到底必须是
   最旧那条）。
3. **假守卫（二）** —— 为 P2-7 把拖动区测试从「中心点」加强到「四角」，
   注入后**仍然绿**：拖动区终点与输入区左缘只差 2pt，四角全在区外。
   → 补登记 `search_box`（外框）热区，把真正会出事的那 22pt 纳入探测。

**结论**：
- 判据必须落在**真正会出事的区域**上，不是「差不多的地方」。
- 断言必须能区分「有东西」与「是哪个东西」。
- **任何修复都要注入 bug 验证测试会红**，否则就是自欺。

---

## 三、建议的修复顺序

| 优先 | 条目 | 理由 |
|---|---|---|
| P0 | P0-3 内嵌置顶区重叠 | 唯一「界面明显错乱」的未修项 |
| P0 | F2 隐藏时附属窗口冻结 | 主窗口隐藏后子窗留在屏幕上无响应 |
| P0 | G5 surface 泄漏 | 长期运行的显存/Vulkan 对象泄漏 |
| P1 | F6 三个配置项只写不读 | 用户可见的「开关没用」 |
| P1 | F9 托盘清空不刷新 | 用户可见 |
| P1 | P1-4 / P1-5 滚动相关 | 用户可见 |
| P1 | G6 acquire 错误后永久卡死 | 一次异常即窗口报废 |
| P2 | F7 退出入口、S1 死开关、F5 窗口坐标 | 功能缺失 |
| — |~~P2-8 字号双倍缩放~~ 已修 | — |
| P3 | 其余文档、死代码、防御性钳制 | 技术债 |

---

## 四、明确**不做**的（附理由）

### `OutputCommand::CopyText`（egui 里 Ctrl+C 复制选中文本）

接线本身很简单（`ClipboardReader::write_text` 已在 `Service::copy_item`
里用着），**但本程序会自己监听剪贴板**：写进去的内容会触发自己的捕获
钩子，于是「在搜索框里选中几个字按 Ctrl+C」会**多出一条历史记录**。

capture 层目前**没有自写防护**——既没有「忽略自己刚写的内容」的时间窗，
也没有内容比对。要接必须先补那层，否则回环。

**宁可功能缺失，也不要让用户每按一次 Ctrl+C 就多一条垃圾记录。**
已在 `app.rs` 里留日志与说明，标记为待后续处理。

---

## 五、最终状态：46 条中已修 **41 条**

剩余 **4 条**及不修的理由（F11 已修，G附D 是「需实机验证」类，单独列在表里）：

| 条目 | 为什么留着 |
|---|---|
| **P附2** | `draw_switch` 在同一矩形注册两次 interact，逻辑上冗余，但靠「后注册者优先」结果正确。不影响功能，删它需要改控件骨架 |
| **P附3** | `hit` 表跨窗口共用、`hit.clear()` 只在 `draw()` 里。彻底修需把热区改成**按窗口作用域隔离**，涉及 8 处注册点 + 测试辅助函数，重构风险大于收益。当前无键名冲突、设置窗口每帧重写，只是自动化读热区的时机不稳定 |
| **G附D** | `select_device` 只用主窗口表面查呈现支持。双显卡 + 跨屏拖子窗口才有风险，**需实机验证**才能确认是否值得改 |
| **S4** | 三套宽度需求口径并存（`solve` / `layout_width` / `required_width`）。`required_width()` **生产代码无调用点**（只有测试用），属重构而非修 bug |


**用户可见的问题已全部修完。** 剩下 5 条要么是纯冗余、要么需实机验证、
要么是重构而非修 bug——继续清理的边际收益已明显低于引入回归的风险。

---

## 第二轮：2026-10-12（6 Agent 并行）

> 代码质量 CQ / 缺陷 DF / 架构 AR / 性能 PF / 安全 SEC / UI 六路，外加 clippy 基线与 603 项测试基线。原始报告全文曾见 docs/agents/*.md。

> 方法：6 个 Agent 并行只读审查（代码质量 CQ / 缺陷 DF / 架构 AR / 性能 PF / 安全 SEC / UI），  
> 外加全 workspace clippy 基线（124 条警告行）与 603 项测试基线（全绿）。  
> 原始报告全文见 `docs/agents/*.md`。本文档是分批修复的唯一依据。  
> 关键 P0/P1 已由我逐条对照源码复核（DF-01/02/03、SEC-001/002、UI-P0-1/2/3 均已验证属实）。

## 〇、基线状态（修复前）

| 项      | 值                                           |
| ------ | ------------------------------------------- |
| 编译     | 通过（debug, x86_64-pc-windows-msvc）           |
| clippy | 124 条 warning 行（跨 9 crate + examples/tests） |
| 测试     | 603 项全绿（11 个 test 二进制）                      |
| 发现总数   | 109 条编号发现 + clippy 机械项 ~60 处                |

## 一、确认问题清单（按严重度）

### P0（数据丢失 / 崩溃 / 安全漏洞 / 明显功能失效）

| #    | 来源                     | 问题                                                                                     | 模块           |
| ---- | ---------------------- | -------------------------------------------------------------------------------------- | ------------ |
| F-01 | DF-01                  | 载荷按 `{hash}.bin` 共享命名 + delete 无条件删文件：置顶项与重复制条目同哈希，删任一即摧毁其余条目载荷（默认配置可达，保留策略自动触发）       | store        |
| F-02 | SEC-001 + DF-03b       | `run_command` 经 `cmd /C program arg` 执行未转义 selection，`&`/`\|` 可注入任意命令（链路已就绪，UI 一接线即激活） | platform/app |
| F-03 | SEC-002 + DF-03a       | `open_url` 经 `cmd /C start "" url`，URL 含 `&`/`%` 被 cmd 截断/展开                           | platform/app |
| F-04 | CQ-3                   | `write_blob` 覆盖路径 `File::create` 截断后 write 失败即毁数据（应 .tmp+rename）                       | store        |
| F-05 | CQ-1                   | usize→u32 `as` 截断使配置校验防线失效                                                             | core         |
| F-06 | SEC-005                | `%TEMP%` 载荷目录名可预测（pid+序号）且不查 reparse point                                             | store        |
| F-07 | SEC-006                | DB 行内 blob_file 未规范化，DB 被篡改可作任意文件读/删原语                                                 | store        |
| F-08 | SEC-010                | 互斥体创建失败 `unwrap()` panic + 固定名可被本地进程预持 DoS                                             | bin          |
| F-09 | PF-2                   | ingest 全链路（SHA-256 + blob 写 + insert + retention + reload）在 UI 线程，复制大图冻结数百 ms          | app/capture  |
| F-10 | PF-1a                  | 列表每帧全量 clone items + 逐条 LayoutJob 全量哈希，帧成本随剪贴板总字节线性增长                                  | ui           |
| F-11 | PF-1b                  | 行高每帧重复测量无缓存                                                                            | ui           |
| F-12 | PF-4                   | 无变化也 ~100Hz 全速重绘，空闲 CPU 高                                                              | ui           |
| F-13 | PF-3                   | 启动先显示窗口再初始化，期间无消息泵（假死观感）+ 字体双拷贝                                                        | ui           |
| F-14 | PF-5 + CQ-11 + SEC-018 | preview 全文入库（放大帧级/DB/哈希三处成本；魔法数字 4000 无命名）                                             | store/app    |
| F-15 | UI-P0-1                | `McpProbe` 从未启动，MCP 覆盖层永远显示「未连接」                                                       | ui           |
| F-16 | UI-P0-2                | `Service::notify` 的 notice 全仓无读取方，8 处用户反馈静默丢失                                          | ui/app       |
| F-17 | UI-P0-3                | 卡片子窗口无拖动区上报，鼠标无法移动窗口                                                                   | ui           |
| F-18 | DF-02                  | FTS5 trigram 对 <3 字符查询静默返回空，LIKE 兜底不触发：1-2 字搜索（含两字中文词）永远搜不到                            | store        |
| F-19 | DF-12                  | insert 先写载荷文件后开事务，事务失败留孤儿文件（需等下次启动 gc）                                                 | store        |

### P1（特定场景功能错误 / 重要健壮性）

| #    | 来源                | 问题                                                        | 模块               |
| ---- | ----------------- | --------------------------------------------------------- | ---------------- |
| F-20 | SEC-003           | MCP 默认启用且默认空 token，本地任意进程可读剪贴板/截屏/键鼠注入                    | mcp/core         |
| F-21 | SEC-007           | MCP HTTP 解析无资源上限（header 行/body 大小/读超时）                    | mcp              |
| F-22 | SEC-008           | 不可信图片解码无尺寸上限，解压炸弹可 OOM                                    | app              |
| F-23 | SEC-009           | GDI 对象泄漏三处（backdrop Drop 顺序 / mcp capture 错误路径 / tray 收尾） | gfx/mcp/platform |
| F-24 | SEC-011           | UNC 路径经 explorer 打开触发 SMB NTLM 凭据泄露                       | platform         |
| F-25 | SEC-012           | FTS 回退路径把用户搜索词写入日志                                        | store            |
| F-26 | DF-11a            | 保留策略/列表排序缺 `id DESC` 兜底（同毫秒并列淘汰目标不确定）                     | store            |
| F-27 | DF-11b            | 数量淘汰逐条 delete = N+1 事务                                    | store            |
| F-28 | PF-6              | 列表查询缺 `(pinned, created_at)` 复合索引 + 每次事件全量重读              | store            |
| F-29 | PF-8              | `copy_item` 图片分支 UI 线程全量解码（点击卡顿）                          | app              |
| F-30 | DF-04             | 自启读用 RRF_RT_REG_SZ、写用 REG_EXPAND_SZ，读函数永远看不到写的值           | platform         |
| F-31 | DF-05             | 序列号基线无条件先推进，瞬时读取失败即永久漏捕该次复制                               | capture          |
| F-32 | DF-06             | Html 条目写回写的是 HTML 源码纯文本（粘贴得到字面标签）                         | app              |
| F-33 | UI-P1-1           | 子窗口 Context 不 apply theme，滚动条/tooltip 观感断裂                | ui               |
| F-34 | UI-P1-3           | 主题切换瞬时硬切无过渡                                               | ui               |
| F-35 | UI-P1-4 + UI-P2-3 | 选中描边 `2.0 * scale` 违反逻辑点约定（死分支同病）                         | ui               |
| F-36 | AR-06             | Vulkan 管线组装两份相同实现（app.rs 内联 + SharedGfx::create）          | ui/gfx           |
| F-37 | AR-05             | 配置写入全字符串键，4 处平行 match，字段改名静默失效                            | ui/app           |
| F-38 | AR-04             | gfx→platform 误入 [dependencies]，渲染库拖入 arboard 链            | gfx              |
| F-39 | AR-13             | rust-version=1.85 低于 let-chains 实际要求（1.88）                | workspace        |
| F-40 | PF-9              | elide_text 超宽逐字符测量（每字符一次分配）                               | ui               |
| F-41 | PF-10             | 每帧 3-4 次 env var 查询                                       | ui               |
| F-42 | PF-12             | 捕获文本路径 bytes+text 双拷贝                                     | capture          |
| F-43 | CQ-2/CQ-9         | StoreError::LimitReached 消息误导 + 死代码 allow                 | store            |

### P2（一般质量 / 可维护性）

| #    | 来源                        | 问题                                                    | 模块               |
| ---- | ------------------------- | ----------------------------------------------------- | ---------------- |
| F-44 | CQ-4                      | capture repr/derive 重复声明                              | capture          |
| F-45 | CQ-5                      | bypass 参数与文档矛盾                                        | capture          |
| F-46 | CQ-6                      | let-chain + if let else None 嵌套可读性                    | capture          |
| F-47 | CQ-8                      | 三个配置死开关（capture_image/mcp.enabled/hide_on_focus_lost） | core             |
| F-48 | CQ-10                     | `as_text()` expect 依赖调用方纪律                            | core             |
| F-49 | CQ-12                     | `with_conn` 持锁执行回调，公开 API 死锁路径                        | store            |
| F-50 | CQ-13                     | merge_groups 两条语句非原子                                  | store            |
| F-51 | DF-07                     | 托盘 v4 回调解码内部模型与真实 wire format 不符（当前行为碰巧正确，测试固化错误格式）   | platform         |
| F-52 | DF-08 + SEC-009c          | 托盘菜单宿主窗口/类不回收，重复 start 泄漏                             | platform         |
| F-53 | DF-09                     | reader 级抑制死代码 + 800ms 与文档 900ms 不一致                   | capture          |
| F-54 | DF-10                     | store 模块文档说不开 WAL，实际开启（文档漂移）                          | store            |
| F-55 | UI-P2-1                   | 描边宽度硬编码绕过令牌                                           | ui               |
| F-56 | UI-P2-2                   | 魔法数字集中区（开关/分段/MCP 面板/徽标）                              | ui               |
| F-57 | UI-P2-4                   | 搜索展开/覆盖层/分段选中硬切换                                      | ui（随动效批）         |
| F-58 | UI-P3-1                   | 键盘可达性缺失（方向键/Enter/Esc）                                | ui               |
| F-59 | UI-P3-3                   | 光标图标映射不全                                              | ui               |
| F-60 | UI-P3-2                   | thumbnail 陈旧引用 + GPU 缩略图显示未接线                         | ui               |
| F-61 | AR-16                     | run_with_options 854 行巨块，主循环不可单测                      | ui（重构，见裁决）       |
| F-62 | AR-03                     | paint.rs 6369 行重新成为神模块                                | ui（重构，见裁决）       |
| F-63 | AR-01                     | 配置路径推导三处并存 + core 直接做 fs IO                           | core/app（重构，见裁决） |
| F-64 | AR-02                     | 平台 API 三 crate 并存（gfx/platform/mcp 各一份 Win32）         | 全局（文档化，见裁决）      |
| F-65 | AR-09                     | 卡片子窗口生产不可达（detach 无生产调用方），文档仍承诺「可拖出」                  | ui               |
| F-66 | AR-10                     | LayoutConfig 旧架构遗留滞留 core 配置面                         | core             |
| F-67 | AR-12                     | mcp 不在架构图 + /health 契约字符串耦合                           | mcp/ui           |
| F-68 | AR-11                     | 三份架构文档与代码大面积不一致                                       | docs             |
| F-69 | AR-14                     | opt-level="z" 取舍未验证                                   | workspace        |
| F-70 | AR-17/18/19/20 + CQ-14~25 | 测试卫生、陈旧引用、P3 质量项（约 17 项小问题）                           | 多处               |
| F-71 | PF-11/13/14/15/16         | P2/P3 性能小项                                            | gfx/ui/store     |
| F-72 | DF-13                     | 序列号 32 位回绕理论漏捕（登记不修）                                  | capture          |

### 明确不修（附理由）

| 项                          | 理由                                     |
| -------------------------- | -------------------------------------- |
| AR-08（Service 非 trait 化重构） | 当前耦合深度浅，trait 化收益低于回归风险；记录在案           |
| AR-07（先提交再开发）              | 属流程建议，非代码缺陷；本次修复完成后统一提交                |
| 载荷加密（已知缺口）                 | 产品级决策，超出本次审查修复范围；维持 ARCHITECTURE.md 记录 |
| CopyText 回环                | 旧审查已裁决「宁可功能缺失」，维持                      |
| DF-13 序列号回绕                | 理论不可达（约 136 年），登记在案                    |

## 二、分批修复计划与执行状态

| 批次 | 内容                 | 状态                                                                      |
| -- | ------------------ | ----------------------------------------------------------------------- |
| B0 | clippy 机械修复（~60 处） | ✅ 完成：124 警告行 → 0；修复重复 `#[test]`（双跑）、AR-15 死测试复活、pixels_probe.rs 预存语法错误  |
| B1 | P0 数据完整性 + 命令注入链   | ✅ 完成：F-01~F-08、F-18、F-19、F-26 全部落地（见下）                                  |
| B2 | P0/P1 性能           | ✅ 完成：PF-1/2/3/4/5/6/7/9/10/14 落地；F-29/F-42 延期（理由见下）                     |
| B3 | P0/P1 UI 功能缺陷      | ✅ 完成：F-15/F-16/F-17/F-33/F-35 落地（三个 UI P0 全部修复）                         |
| B4 | P1 安全加固 + 质量       | ⚠️ 部分：F-24/F-25（SEC-011/012）未做；SEC-003/007/008/009、CQ 质量项未做（见「未修项」）     |
| B5 | 文档收敛               | ⚠️ 部分：本清单即汇总文档；三份架构文档重写（AR-11）未做                                        |
| B6 | UI 优化 + 动效         | ✅ 动效体系落地：anim.rs（4 单测）+ hover 补间 + 主题切换 220ms 补间 + 闸门联动；F-55/F-56 令牌化未做 |

### B1 修复明细（全部有回归测试）

- **DF-01**（P0 数据丢失）：`delete`/`delete_many` 删文件前引用计数检查——同哈希共享 `{hash}.bin` 时，只剩最后一个引用才删文件。回归测试 ×2（单条删/批量删）。
- **SEC-001/002 + DF-03**（P0 命令注入）：`open_url` 弃 `cmd /C start` 改 `ShellExecuteW`；`run_command` 弃 shell 改 **argv 直传**；内置 URL 模板改 `{selection_urlencoded}`（自实现 percent-encode）。
- **新发现并修复**（修复过程中）：用户规则的 `arg` 模板**只写不读**（规则永远跑内置模板，联动功能实际不可用）；两个内置 CommandAction 共用 id `"run_command"` 导致 DeepL 动作从未注册。
- **CQ-3**：载荷原子写（.tmp+rename）；**DF-12**：事务失败清理孤儿载荷文件。
- **SEC-005**：`%TEMP%` 载荷目录加随机分量 + reparse point 检测；**SEC-006**：`safe_blob_path` 路径规范化（+单测）。
- **SEC-010**：互斥体创建失败 unwrap → 友好错误；**DF-04**：自启读改 `RRF_RT_ANY`；**DF-05**：序列号窥探/提交分离 + 剪贴板被占用时下轮重试；**DF-02**：trigram <3 字符查询闸断到 LIKE（+单测）；**DF-11a**：三处排序补 `id DESC` 并列兜底。
- CQ-1/CQ-2/CQ-9 **无法复现**（描述的宏/错误枚举在当前代码不存在），标记跳过。

### B2 修复明细

- **PF-5**：preview 截断 512 字符（`PREVIEW_MAX_CHARS`；已知取舍：FTS 随 preview 截断，长文档仅前 512 字符可检索）。
- **PF-1**：列表改**索引迭代**（消除 2000 条/帧 clone）+ **行高缓存**（`AppState::list_version` 失效判据）+ 只克隆可见行；`draw_rail_body` 同改。
- **PF-6**：新增 `(pinned, created_at)` 复合索引。**PF-7**：`delete_many` 单事务批量删（数量/容量淘汰两处）。
- **PF-10**：`env_flag` OnceLock 缓存。**PF-9**：`elide_text` 二分前缀。**PF-4**：空闲渲染闸门（无事件+无 pending repaint+配置未变+**无补间在飞** → 跳过 run_ui/tessellate/present；`svc.pump` 提到闸门前保证捕获不断）。**PF-3**：`focus_window` 移到全部初始化之后。**PF-2**：指纹移入捕获线程。**PF-14**：消除 35MB 字体二次拷贝。
- **PF-15 评估后不修**：egui `Painter::text` 内部必然 `to_string()`，galley 缓存已消除昂贵部分。

### B3 修复明细

- **UI-P0-1**（F-15）：`McpProbe::start()` 接入 App 构造 + 每帧同步 `mcp_conn`。
- **UI-P0-2**（F-16）：新增 notice toast（右上角、3s 过期即消费、自报热区 + 回归测试）。
- **UI-P0-3**（F-17）：子窗口上报拖动区（卡片标题头，draw_child clear+push，childwin render 每帧 `set_drag_region_multi` + 回归测试）。
- **UI-P1-1**（F-33）：子窗口 Context 每帧 `theme::apply`。**UI-P1-4/UI-P2-3**（F-35）：两处 `* scale` 描边改逻辑点。

### B6 动效明细（用户明确要求项）

- 新增 `ui/src/anim.rs`（约 300 行，零外部依赖）：`Ease`（Linear/OutCubic/OutExpo/InOutCubic，单调性+端点单测）、`tween_f32`（retarget 从当前显示值续接、完成精确吸附、状态存 `ctx.data`）、`tween_color`/`tween_rect`、`clear`、**`busy`**（闸门联动）。
- 落地两处：**行 hover 背景补间**（110ms OutCubic，id 带 item.id）；**主题切换 Palette 补间**（220ms InOutCubic，`Palette::lerp` 只插颜色、几何直取目标值，egui Visuals 同步重灌）。
- 空闲闸门已接入 `anim::busy`——否则动画只在事件帧推进，鼠标不动时卡半路。

## 三、未修项与理由（诚实清单）

| 项                                               | 级别    | 理由                                                                                     |
| ----------------------------------------------- | ----- | -------------------------------------------------------------------------------------- |
| SEC-003（MCP 默认无鉴权）                              | P1    | 需要 token 生成 + config 迁移 + http.rs 鉴权 enforcing 三处联动，改动面大；本次会话剩余预算不足以保证质量，**建议下一批第一项做** |
| SEC-007（MCP HTTP 解析上限）                          | P1    | 同 MCP 子系统，宜与 SEC-003 同批                                                                |
| SEC-008（图片解码上限）                                 | P2    | 需联核实测 image 0.25 默认 Limits 行为后再定阈值                                                     |
| SEC-009（GDI 泄漏三处）                               | P2    | backdrop Drop 顺序 / mcp capture 错误路径 / tray 收尾，需逐处实机验证                                  |
| SEC-011/012（UNC/NTLM、搜索词入日志）                    | P2    | 改动小但未及做                                                                                |
| F-29（copy_item 图片解码异步化）                         | P1    | 需跨线程通道回写剪贴板；帧级大头已修，此项收益比下降                                                             |
| F-42（捕获 bytes/text 双拷贝）                         | P2    | 需改核心类型 + 10 处构造点                                                                       |
| AR-03/16（paint.rs 6369 行拆分 / 帧循环抽取）             | P1/P2 | 数千行重构，UI-REWRITE.md 已规划分步实施；本轮只做低风险收敛                                                  |
| AR-01/02/12（配置路径三处 / 平台 API 三 crate / mcp 入架构图） | P1/P2 | 大搬迁类，记录在案待架构迭代                                                                         |
| AR-11（三份架构文档重写）                                 | P2    | 文档工程，需以实物依赖边表为基准专文重写                                                                   |
| F-55/F-56（描边令牌化 / 魔法数字）                         | P2    | 与 AR-03 的拆分协同做更合理                                                                      |
| CQ-14~25 等 P3 质量项                               | P3    | 多为顺手项，未及                                                                               |

## 四、最终状态继续执行

> 项
>
> 修复前
>
> 修复后

> clippy（--all-targets）
>
> 124 警告行
>
> **0 警告、0 错误**

> 测试
>
> 603 passed（含 2 个重复双跑）
>
> **617 passed**（601 唯一 + 16 条新回归测试，11 个二进制全绿）

> 预存缺陷
>
> pixels_probe.rs 语法错误（工作区）
>
> 已修

> 新回归测试
>
> —
>
> DF-01 ×2、DF-02、原子写、批量删共享载荷、SEC-006 路径、子窗拖动区、notice toast、anim ×4、palette lerp ×1、命令注入 ×3 等共 16 条

---

## 第三轮：2026-10-23（3 Agent + 主线手动 UI 审查）

> 安全面 8 条 + 数据链路 9 条 + gfx 渲染与资源 6 条。UI 逻辑一致性因 Agent 通道持续 429 改为主线手动审查，结论并入本章。

> 本文件为多人合稿。每位审查者追加自己的章节，编号独立。

## 审查：安全面（platform/ipc/mcp）

### 威胁模型（先说清楚再谈发现）

本地单用户 Windows 机器。假定的攻击者：**与用户同会话、同完整性级别（medium IL）的任意进程**——用户自己安装/运行的第三方程序、浏览器 exploit 落地的代码、恶意安装包。不假设本地提权（提权后的攻击面不归本条边界管）。

在此模型下，本项目的信任边界只有两道：

1. **命名管道** `\\.\pipe\ModularClipboard.Daemon.v1`：常驻进程 ⇄ 界面进程；
2. **MCP HTTP** `http://host:port/mcp`：MCP 客户端（AI 工具）⇄ 自动化服务。

以下所有发现都围绕这两道边界展开。凡判定为「理论风险」的，均在文末「排除项」中说明排除了什么。

---

### [SEC-01] 命名管道零客户端身份校验：任意本地进程可驱动常驻进程全部命令
- **文件:行号**：`crates/modular-clipboard-ipc/src/transport.rs:261`（`CreateNamedPipeW` 的 `lpSecurityAttributes` 传 `None`）、`crates/modular-clipboard-app/src/daemon.rs:443`（`conn.recv()` 后无任何身份判断）、`crates/modular-clipboard-app/src/daemon.rs:482-604`（`handle_cmd` 全量执行）
- **现象**：管道名是固定串（`transport.rs:33`），创建时不传安全描述符（`transport.rs:261`），`ipc_loop` 接受连接后直接进请求循环（`daemon.rs:443-457`），`handle_cmd` 对**任何**连上来的客户端执行全部 19 条 `Command`——包括：
  - `Command::LoadPayload(id)`（`daemon.rs:579-591`）：取回任意历史条目的**原始载荷字节**。密码管理器复制过的口令、Token、私钥图片，只要进过历史，任意本地进程都能连管道读走；
  - `Command::ClearAll` / `DeleteItem` / `TogglePin`：销毁用户数据；
  - `Command::SaveConfig(cfg)`（`daemon.rs:565-578`）：整体替换配置，随后 `rebind_hotkeys` 立即按攻击者配置注册。攻击者可把「粘贴上一条」绑到 `Ctrl+C`（`MOD_CONTROL|VK_C` 可被 `hotkey::from_key` 接受，`hotkey.rs:103-111`）——此后用户在任何程序里按 Ctrl+C，常驻进程就把上一条历史写回剪贴板并补发 Ctrl+V，**用户的复制被静默替换**；
  - `Command::Ingest(payload)` / `UpdateContent`：向历史注入伪造条目（含 `files` 路径，可诱导后续「打开」动作，见 SEC-07）；
  - `Command::WriteClipboardText`：直接劫持剪贴板（替换刚复制的收款地址一类攻击）；
  - `Command::Quit`：随时杀死常驻进程。
- **根因**：双进程架构把「界面进程」默认为可信客户端，但管道对象本身没有把「只有我们的 ui.exe 能连」这条约束落下去。`FILE_FLAG_FIRST_PIPE_INSTANCE`（`transport.rs:251`）只防第二个 daemon 抢名，不防恶意客户端。
- **影响**：同会话任意进程（含浏览器 exploit 载荷）可外泄全部剪贴板历史、篡改配置与热键、注入伪造条目、DoS。**这是真实可利用的攻击面，不是理论风险**——管道名写在二进制里，连接零门槛。
- **建议修法**（任选其一，推荐第 1 种）：
  1. daemon 每次启动生成随机管道名（如 `ModularClipboard.Daemon.<随机 128bit>`），`spawn_ui` 拉起界面进程时通过命令行参数传给它（`daemon.rs:121-125` 已有传 `--data-dir` 的先例，加一个 `--pipe-name` 即可）；轮询式连接的其他入口（托盘唤起路径）改走共享内存/环境变量发现。
  2. 保留固定管道名，但在 `accept` 后用 `GetNamedPipeClientProcessId` 取客户端 PID，校验进程映像路径为本程序 `--ui` 实例，不匹配直接断连。
  3. 最低成本：`CreateNamedPipeW` 传显式 SD，只放行当前用户 SID + 校验握手后第一条消息必须来自本会话——注意仅限同用户仍挡不住同户恶意进程，不能替代 1/2。
- **严重度**：P1（应该修；泄露面含口令类剪贴板内容，建议排在所有修复的第一位）

### [SEC-02] MCP HTTP 服务默认零鉴权、默认全能力开启，可被浏览器与同网段利用
- **文件:行号**：`crates/modular-clipboard-core/src/config/mcp.rs:76-90`（`McpConfig::default()`：`enabled: true`、`token: String::new()`、10 个能力全部 `true`）、`crates/modular-clipboard-mcp/src/http.rs:99`（`token` 为空时**跳过鉴权`）、`crates/modular-clipboard-mcp/src/http.rs:22-29`（host 可配任意地址，`0.0.0.0` 即监听全部网卡）、`crates/modular-clipboard-ui/src/paint/settings_items.rs:343-347,357-363`（配对提示词把「认证：无」直接印给用户）
- **现象**：用户按设置页提示执行 `modular-clipboard.exe --mcp-http`（`settings_items.rs:361-363`）后：
  - 监听 `127.0.0.1:9123`（默认），**token 为空 ⇒ 鉴权分支被整段跳过**（`http.rs:99` 的条件判断）；
  - 10 个工具全部可用（`tools.rs:216-233` 的 `cap_enabled` 在默认配置下全部放行），包括 `screen_capture`（任意窗口截图）、`click_element`/`type_text`/`key_press`（合成键鼠）、`clipboard` 的 `read`（读取当前剪贴板）；
  - HTTP 处理函数不校验 `Host` 头（`http.rs:59-81` 只取 method/path/content-length/authorization），DNS rebinding 可直接读取响应；
  - 恶意网页可用 `fetch("http://127.0.0.1:9123/mcp", {method:"POST", body: JSON-RPC, headers:{"Content-Type":"text/plain"}})` —— `text/plain` 属于 CORS「简单请求」，**不发预检、必定送达**，副作用（打字、点击、截图）真实发生，只是响应读不到；配合 DNS rebinding 可读到 `clipboard read` 与截图结果。
  - 若用户在设置里把 host 改成 `0.0.0.0`（UI 不拦不警告，`config_write.rs:81` 只做 trim），上述能力对同网段所有主机开放，且 token 仍默认为空。
- **根因**：鉴权是「用户可选」而非「默认强制」：空 token 被当作「无需认证」而不是「未配置」；能力开关默认全开；host 无合法性提示。
- **影响**：浏览器沙箱 → 桌面控制（键鼠输入、任意窗口截图、剪贴板读取）的一键跳板。恶意网页、同网段主机（配 0.0.0.0 时）均可利用。前置条件仅为「用户启动过 --mcp-http」——而这正是设置页引导的操作。
- **建议修法**：
  1. 首次启动 HTTP 模式且 token 为空时**自动生成随机 token** 并写入配置，UI 明示「已自动生成，请复制到客户端」；
  2. `http.rs:99` 改语义：token 为空时拒绝所有非 loopback 来源（对 `peer_addr` 判 `127.0.0.0/8`/`::1`），loopback 也在日志里显著警告；
  3. 校验 `Host` 头（只接受 `127.0.0.1:port`/`localhost:port`）防 DNS rebinding；
  4. host 配成非 loopback 时 UI 弹确认，说明「本服务可截屏、可读写剪贴板，无鉴权将暴露给全网」。
- **严重度**：P0（必须修：默认配置即无鉴权，且是从浏览器沙箱逸出到桌面能力的完整链路）

### [SEC-03] MCP 传输层没有任何大小/连接/超时限制，可被本地请求 OOM 崩溃
- **文件:行号**：`crates/modular-clipboard-mcp/src/http.rs:107`（`let mut body = vec![0u8; content_length];`，`content_length` 完全由请求头决定）、`crates/modular-clipboard-mcp/src/http.rs:76`（`v.parse().unwrap_or(0)`，无上限校验）、`crates/modular-clipboard-mcp/src/http.rs:53,66`（请求行/头 `read_line` 无长度上限）、`crates/modular-clipboard-mcp/src/http.rs:31-43`（每连接一线程，无连接数/读超时上限）、`crates/modular-clipboard-mcp/src/stdio.rs:24`（stdio 侧 `read_line` 同样无上限）
- **现象**：`Content-Length: 4294967296` 一个头即可让服务端尝试分配 4GB 零页缓冲；不送 body 随后断开，分配失败时 Rust 直接 abort。多发几个连接即可稳定拖死 MCP 进程。请求行不发 `\n` 也能让 `read_line` 把内存吃光。对比：IPC 侧同类问题有 `MAX_FRAME = 64MB` 闸门（`transport.rs:27,197-199`），MCP HTTP 侧没有对应物。
- **根因**：手写 HTTP 解析时只实现了「能跑通」的最小路径，把 `Content-Length` 当可信输入。
- **影响**：本地任意进程（或配了 `0.0.0.0` 时的同网段主机）可反复让 MCP 进程崩溃/耗尽内存；连接不关则线程只增不减。
- **建议修法**：body 上限（建议 ≤ 8MB，与 IPC 的 MAX_FRAME 对齐）、请求行/头行长度上限、`set_read_timeout`、单连接线程数上限或改有界线程池。
- **严重度**：P1（应该修：DoS 可靠且修复成本低）

### [SEC-04] 托盘窗口的内部消息可被同权进程伪造（Quit / 清空 / 注销热键）
- **文件:行号**：`crates/modular-clipboard-platform/src/tray/imp.rs:422-427`（`WM_COMMAND` → `command_to_event`）、`crates/modular-clipboard-platform/src/tray/imp.rs:401-421`（`WM_TRAY_SUSPEND_HOTKEYS` 注销全部热键）、`crates/modular-clipboard-platform/src/tray/imp.rs:377-379`（`WM_TRAY_SET_HOTKEY` 改绑）、`crates/modular-clipboard-platform/src/tray/imp.rs:34,50`（类名 `ModularClipboardTrayWindow` / `ModularClipboardTrayMenuHost` 是编译期固定串）
- **现象**：托盘窗口与菜单宿主都是普通顶层/message-only 窗口，类名硬编码在二进制里。同完整性进程可 `FindWindowW("ModularClipboardTrayWindow", NULL)` 拿到句柄后 `PostMessageW`：
  - `WM_COMMAND, 0x1003` → 常驻进程退出；`0x1002` → 清空历史；`0x1001` → 弹窗口；
  - `WM_TRAY_SUSPEND_HOTKEYS, 1` → 注销用户全部全局热键，且恢复依赖客户端再发一次，用户热键静默死亡；
  - `WM_TRAY_SET_HOTKEY` → 把槽位改绑到任意组合（含抢占失败后旧绑定已注销的状态）。
  `WM_COMMAND`（0x0111）低于 `WM_USER`，UIPI 不过滤——低完整性进程也能发。
- **根因**：内部控制消息用固定消息 id + 固定类名，等于把「控制平面」暴露在桌面全局命名空间里。
- **影响**：可用性攻击（杀常驻、清历史、废热键）。与 SEC-01 有重叠但**不依赖管道**，是独立入口。
- **建议修法**：内部消息 id 每次启动加随机盐（进程内共享常量即可，外部无法预测）；或窗口类名带随机后缀；`WM_COMMAND` 仅接受来自本进程菜单（`TPM_RETURNCMD` 返回值路径）的 id，丢弃所有跨进程投递的 `WM_COMMAND`（托盘窗口本身不应收到外部 `WM_COMMAND`——右键菜单走的是 `TPM_RETURNCMD`，正常路径根本不需要处理外部 `WM_COMMAND`，直接删掉该分支即可消除大半攻击面）。
- **严重度**：P2（可选：仅可用性影响，但修复几乎零成本——删 `WM_COMMAND` 分支 + 消息 id 加盐）

### [SEC-05] 界面实例互斥体名固定，可被同名抢占导致界面永远打不开
- **文件:行号**：`crates/modular-clipboard-ipc/src/instance.rs:12`（`Local\ModularClipboard.UIInstance` 固定名）、`crates/modular-clipboard-ipc/src/instance.rs:43-53,71-75`
- **现象**：`Local\` 命名空间任何同户进程都能创建。恶意进程先建 `Local\ModularClipboard.UiInstance` 并持有不放 ⇒ `UiInstanceGuard::acquire()` 永远失败 ⇒ 界面进程启动即退（`main.rs:250-253` 路径），同时 `ui_is_running()` 永远为 true ⇒ 常驻进程认为界面在跑、不再拉起 ⇒ 热键/托盘点击全部失灵，且互斥体随攻击者进程退出才恢复。
- **根因**：互斥体名是公开常量，注释（`instance.rs:36`）只对比了 pid 文件的缺点，没考虑「名字本身可被抢占」。
- **影响**：本地 DoS，恢复依赖攻击者进程退出。仅可用性，无数据面影响。
- **建议修法**：改用「窗口存在性 + 窗口属主进程校验」双判据（`FindWindowW` 后 `GetWindowThreadProcessId` 比对），或把互斥体名改为「数据目录哈希 + 随机盐存共享内存」；至少 `ui_is_running()` 命中已存在时，再校验一次标题窗口的属主进程是否真的是本程序。
- **严重度**：P2

### [SEC-06] 按窗口标题 FindWindow 可被同名伪装窗口利用（热键呼出假界面 / toggle 判错）
- **文件:行号**：`crates/modular-clipboard-platform/src/lib.rs:87-101`（`activate_window_by_title`）、`crates/modular-clipboard-ipc/src/instance.rs:104-110,125-134,140-152,158-170`（`post_to_main` / `ui_window_exists` / `ui_window_hwnd` / `ui_window_visible`）
- **现象**：主窗口标题 `ModularClipboard` 是公开常量（`instance.rs:19`）。任意进程可先建一个同名标题的窗口：热键按下时 `activate_window_by_title` 会把前台授权用于激活**攻击者的窗口**（可画成高仿剪贴板界面的钓鱼窗口，承接用户后续点击）；`ui_window_visible()` 判到假窗口可见 ⇒ 热键 toggle 走隐藏方向 ⇒ `hide_ui_window()` 把 `WM_HIDE_MAIN` 发给假窗口，真界面状态错乱。
- **根因**：跨进程窗口寻址只用「标题」这一不可信标识。
- **影响**：本地钓鱼/界面错乱。需要攻击者持续运行一个窗口，且与 SEC-01 不叠加也有独立效果。
- **建议修法**：找到窗口后校验属主进程（`GetWindowThreadProcessId` 与已知 ui 进程 id 比对；daemon 持有它 spawn 的 ui PID）；或改用窗口类名 + `GetClassName` 校验，类名同样应带随机盐。
- **严重度**：P2

### [SEC-07] 剪贴板条目里的 UNC 路径经「打开/定位」动作触发对外 SMB 认证
- **文件:行号**：`crates/modular-clipboard-app/src/action.rs:273-282`（`extract_path` 显式接受 `\\` 开头的 UNC 路径）、`crates/modular-clipboard-app/src/action.rs:84-92,135-139,381-388`（`OpenExternal`/`RevealInExplorer` 取条目首行直接进 `platform::open_external` / `reveal_in_explorer`）、`crates/modular-clipboard-platform/src/lib.rs:284-300,341-356`
- **现象**：一条文本 `\\attacker.example.com\share\x.txt`（或经 SEC-01 的 `Ingest` 注入的 Files 条目）会通过 `looks_like_path`/UNC 分支被判定为路径，用户点「打开」或「在资源管理器中显示」后，explorer 向攻击者主机发起 SMB 连接与 NTLM 认证——用户哈希可被捕获/中继。参数是 argv 直传（不经 shell），所以没有命令注入；问题在 UNC 语义本身。
- **根因**：剪贴板内容（不可信输入）被当作可信路径直接交给文件系统外壳，UNC 分支没有额外确认。
- **影响**：需要用户点击一次动作；但「复制到粘贴板的一段文本 → 点打开」是本品类的常规操作流。与 SEC-01 叠加时攻击者可预先播种条目、等用户手滑。
- **建议修法**：UNC 路径打开前弹确认（「将连接到外部服务器 \\xxx，继续？」）；或对非本机盘符的路径默认拒绝打开。
- **严重度**：P2

### [SEC-08] 自启动存在两套实现且状态检测只查「值存在」，可被仿冒
- **文件:行号**：`crates/modular-clipboard-platform/src/autostart.rs:32,116-155`（UI 实际使用的版本：值名 `ModularClipboard`，`is_enabled` 只查存在与长度）、`crates/modular-clipboard-platform/src/lib.rs:494-546,555-590`（另一套：值名 `modular-clipboard`，写 `REG_EXPAND_SZ`；全仓无调用方，见 `crates/modular-clipboard-ui/src/app.rs:191,729` 用的都是前者）
- **现象**：
  1. `is_enabled()` 只验证「注册表值存在且非空」，不校验内容指向本程序。任何程序（或用户手动）写一个同名 Run 值指向木马，UI 仍显示「开机自启：已开启」——状态显示被仿冒；
  2. 两套自启动实现值名/类型不一致（`ModularClipboard`/REG_SZ vs `modular-clipboard`/REG_EXPAND_SZ），lib.rs 版目前是死代码。将来若有人调用 lib.rs 版，会出现「写了但 UI 读不到、显示未开启」的静默分叉。
- **根因**：状态检测用存在性代替内容校验；重复实现未收敛。
- **影响**：设置项状态可信度下降；潜在误导用户以为自启正常。无直接代码执行（Run 值内容由写者控制，本程序只写自己的路径）。
- **建议修法**：`is_enabled` 读出值内容，校验包含本进程 exe 路径才算开启；删掉 lib.rs 里的重复实现（或反过来以 lib.rs 版为唯一实现，统一值名与类型）。
- **严重度**：P2

---

### 排除项（核实后不成立的疑点）

1. **IPC 帧长度前缀 OOM**：不成立。`transport.rs:197-199` 在分配前校验 `MAX_FRAME = 64MB`，超限直接 `bail`；且该校验在收发两侧都生效。这是本项目做对了的地方——MCP HTTP 侧（SEC-03）反而没有对应闸门。
2. **日志泄露剪贴板明文**：未发现。全仓检索 `tracing` 宏中携带 `preview/text/content/payload/selection` 的输出，仅有三处且都只记 **kind / 字节数 / hash / 来源程序名**（`crates/modular-clipboard-app/src/lib.rs:814,889`、`crates/modular-clipboard-store/src/write.rs:73`），无内容明文。`app.log`/`ui.log`/`mcp.log` 按进程分文件（`main.rs:300,337,230`），无互相截断。
3. **`run_command` / `open_url` 的命令注入**：不成立。剪贴板文本进命令行前有 percent-encode（`action.rs:189,340`），且全部 argv 直传不经 shell（`lib.rs:471-490,308-338`）；URL 走 `ShellExecuteW` 单参数。SEC-001/DF-03 的既有修复仍然完整。
4. **热键注册抢占处理**：设计稳健。`RegisterHotKey` 失败不 panic、记日志、失败清单经快照回传界面（`tray/imp.rs:841-856`、`daemon.rs:198`）；录制期注销/恢复用同步 `SendMessageW` 保证有序（`tray.rs:448-450`、`imp.rs:946-951`）；`MOD_NOREPEAT` 防长按连发（`hotkey.rs:63-64`）。未发现安全问题。
5. **MCP 截取 elevated 窗口/凭据对话框**：不可行。`capture.rs:58-80` 的 GDI BitBlt 受 UIPI 约束，medium IL 进程截不到高完整性窗口；UAC 同意对话框另跑在安全桌面。`is_password_field` 黑名单是捕获侧的另一层（非本边界）。
6. **托盘 `command_to_event` 的菜单 id 冲突**：不成立。迷你面板虽用条目 id 当菜单 id（`imp.rs:253`），但两条菜单路径都用 `TPM_RETURNCMD` 直接取返回值、不走 `WM_COMMAND`，与 `CMD_SHOW/CLEAR/QUIT`（0x1001-0x1003）不存在路由交叉。跨进程伪造 `WM_COMMAND` 的问题单列在 SEC-04。
7. **`Global\ModularClipboard.SingleInstance` 被低权进程抢占**：不成立。`Global\` 命名对象创建需要 `SeCreateGlobalPrivilege`，标准用户进程拿不到——提权攻击者不在本威胁模型内。`Local\` 的 UI 互斥体才是同权可抢的，已单列为 SEC-05。
8. **SEC-005 / SEC-006 闸门（临时目录 junction / DB 行任意文件读写）在新拆分结构下失效**：未发现失效。`safe_blob_path` 闸门仍覆盖 store 侧全部入口（`write.rs:217-219,343,430`、`query.rs:208`），`open_in_memory` 的载荷目录带 pid+唯一序号（`open.rs:3`、`lib.rs:75`）。从 IPC 边界确认：`Ingest`/`UpdateContent`/`LoadPayload` 三条命令全部经由 store，没有绕过闸门的第二条写入路径。store 内部细节归数据链路审查，此处仅作边界确认。

---

## 小结

按严重度统计：**P0 × 1，P1 × 2，P2 × 5**（共 8 条）。

最该先修的 3 条：

1. **SEC-02**（P0）：MCP HTTP 默认零鉴权 + 全能力默认开启 + 无 Host 校验。这是唯一一条「默认配置下即可被浏览器网页利用」的完整链路，且修复成本低（自动生成 token + 非 loopback 拒绝 + Host 白名单）。
2. **SEC-01**（P1）：命名管道无客户端身份校验。`LoadPayload` 让任意本地进程读走全部剪贴板历史（含口令），`SaveConfig` 可把「粘贴上一条」绑到 Ctrl+C 静默替换用户复制。推荐随机管道名 + 启动参数传递，半天内可完成。
3. **SEC-03**（P1）：MCP HTTP 无请求大小/连接/超时限制。`Content-Length` 一个头即可稳定 OOM 崩溃 MCP 进程，与 IPC 侧已有的 `MAX_FRAME` 形成鲜明对照，属于同一团队在两条传输上标准不一致。

P2 的 5 条（SEC-04/05/06/07/08）均为可用性或需要用户配合点击的问题，可随下一轮迭代顺手处理；其中 SEC-04 的「删掉托盘窗口的 `WM_COMMAND` 分支」是零风险改动，建议优先于其余 P2。

---

## 审查：数据链路（store/core/app）

> 审查范围：`modular-clipboard-store`（lib/open/query/write/retention/groups/blobs）、`modular-clipboard-core`（item/config 家族）、`modular-clipboard-app`（lib/ingest/paths + daemon 接线）。
> 行号以**当前工作区**为准：审查期间有并发编辑对 store 四个文件做了纯空行/分隔注释清理（`git diff --stat`：35 deletions / 0 insertions，无语义变化），下文行号已按清理后的磁盘状态重新校准。
> 基线（925 测试全绿、clippy 零警告）下的深挖结论如下，共 9 条。

### [DL-01] 全局去重命中后永不补写缺失的载荷，条目沦为「看得到、点永远失败」的僵尸
- **文件:行号**：`crates/modular-clipboard-app/src/lib.rs:907-919`（dedup 分支只调 `bump_to_top`）、`crates/modular-clipboard-app/src/lib.rs:981-985`（`copy_item` 强依赖 `load_payload`）、`crates/modular-clipboard-store/src/write.rs:59-74`（超限只存元数据、`blob_file` 为 NULL）
- **现象**：载荷超过 `max_payload_bytes` 时只存元数据。之后用户把上限调大（设置 UI 范围 1..4096 MB，`crates/modular-clipboard-ui/src/paint/settings_items.rs:91-98`），**再次复制同一内容**：`find_by_hash` 命中旧条目 → 只 `bump_to_top` 刷新时间 → 载荷依旧不落盘。用户从历史里点这条 → `load_payload` 报「该条目无载荷（可能已被淘汰）」→ 复制/粘贴失败。条目在列表里可见、可置顶、可编辑，就是复制不出去。
- **根因**：去重分支把「重复内容」当作纯时间戳刷新，没有检查现有条目的载荷完整性。载荷缺失有两条来源（入库时限限、文件丢失/被删），都不会自愈——dedup 命中不重写文件，`gc_orphan_blobs` 只收「无引用」文件也不管它。
- **影响**：永久性功能失效的僵尸条目；图片/大文本（最可能超限的类型）首当其冲。**不需要任何异常条件**：默认 1 MB 下限复制一张大图 → 设置里调大 → 再复制一次，即可稳定复现。
- **建议修法**：dedup 命中时检查现有条目 `payload.is_none()`，且新载荷 `<= current_payload_limit()`，则先经 store 新方法（如 `backfill_payload(id, bytes, dims)`：原子写 blob + 事务内 UPDATE `blob_file/blob_len/img_w/img_h`）补写，再 `bump_to_top`；补写失败只记日志、不阻断 bump。
- **严重度**：P1（应该修）

### [DL-02] insert 失败回滚删除共享载荷文件，可静默摧毁其他条目的载荷（DF-01 纪律在插入侧被违反）
- **文件:行号**：`crates/modular-clipboard-store/src/write.rs:127-135`（`remove_file(self.blob_dir.join(f))`，无引用计数检查），对照 `write.rs:215-226`（delete 的计数删除）与 `write.rs:338-346`（update_content 的计数删除）
- **现象**：`write_blob_atomic` 成功之后 DB 事务失败（磁盘满 `SQLITE_FULL`、DB 文件 IO 错误、FTS 插入失败），回滚逻辑**无条件**删掉刚写的 `{hash}.bin`。而同哈希条目共享同一文件：置顶条目不参与去重（`query.rs:37` `AND pinned = 0`）、`dedup=false` 时每次复制都插一条（`core/src/config/storage.rs:52-53`）。删文件 = 仍在引用它的其他条目载荷被静默摧毁，且不会自愈（dedup 命中只 bump 不重写、gc 只收孤儿）。
- **根因**：insert 的失败路径没有复用 delete / update_content 已经确立的引用计数纪律——同一个 `{hash}.bin` 在删除侧「最后一个引用消失才删」、在插入失败侧「说删就删」，两条路径对同一文件持有相反的生杀大权。DF-01 回归测试只覆盖了删除侧，插入侧回滚是缺口。
- **影响**：触发需要一次 DB 写失败（磁盘满最现实：blob 写成功、DB 随后写满），后果是**无关**历史条目的载荷无声消失、无任何报错，用户下次点那条才以「无载荷」的形式暴露。
- **建议修法**：二选一——(a) 回滚前按 delete 同款查一次 `SELECT COUNT(*) FROM items WHERE blob_file = ?1`，为 0 才删；(b) 失败时干脆不删，留给下次启动的 `gc_orphan_blobs` 回收（它本就为此存在，且 (b) 更简单）。
- **严重度**：P1（应该修）

### [DL-03] update_content 事务失败留下孤儿载荷文件，与 insert 的失败语义不一致
- **文件:行号**：`crates/modular-clipboard-store/src/write.rs:275-284`（事务外先写盘，:280 `write_blob_atomic`）、`write.rs:290-328`（DB 事务）
- **现象**：新载荷在事务**之前**落盘；事务失败时 `?` 直接上抛，没有任何清理——新哈希的文件成为孤儿 `.bin`。insert 路径有对应的失败清理（`write.rs:127-135`，虽然有 DL-02 的缺陷但至少有），update_content 完全没有。
- **影响**：孤儿文件直到下次启动 `gc_orphan_blobs` 才回收；无行引用故不影响容量统计，属空间泄漏而非数据损坏。反复编辑反复失败会累积。
- **建议修法**：把 update_content 的 DB 段包成与 insert 同款的闭包结构，失败时按引用计数清理新文件；至少补一条 `tracing::warn!` 说明可能留下孤儿。
- **严重度**：P2（可选）

### [DL-04] max_items 无任何校验，≥2^63 的配置值经 as i64 变负 → 下次插入删光全部未置顶条目
- **文件:行号**：`crates/modular-clipboard-store/src/retention.rs:49`（`let unpinned_keep = (max_items as i64 - pinned_count).max(0);`）、`crates/modular-clipboard-core/src/config.rs:176-193`（`load` 无范围校验）、`crates/modular-clipboard-core/src/config/storage.rs:14`（`max_items: Option<usize>`）
- **现象**：`max_items` 是 64 位 usize。配置里出现 ≥ 2^63 的值（手改/第三方工具写坏 config.json；serde 对 `[2^63, 2^64)` 的整数照常接受，不会触发损坏回退）时，`as i64` 得负数 → `unpinned_keep = 0` → `OFFSET 0` 选中**全部未置顶条目** → `delete_many` 一次删光。触发点仅仅是「下一次复制」。
- **根因**：配置加载只做格式迁移（`migrate` 仅管热键，`config.rs:208-228`），容量字段无 clamp；store 侧把 usize 直接当 i64 算术。UI 路径上限 1,000,000（`ui/src/paint/settings_rows.rs:78`）不会产生该值，但 config.json 是用户可编辑的纯文本，且项目自己把「配置鲁棒性」当作设计目标（损坏备份、逐字段 default）。
- **影响**：用户未置顶历史一次性全灭（置顶条目保留），无任何提示。小改大害的典型。
- **建议修法**：`Config::load` 对 storage 字段 clamp（如 `max_items.map(|n| n.min(1_000_000))`）；store 侧改 u64 运算或 `i64::try_from(max_items).unwrap_or(i64::MAX)`。
- **严重度**：P1（应该修）

### [DL-05] like_search 的 ORDER BY 缺 `id DESC` 并列兜底，与 list/FTS 分支自相矛盾
- **文件:行号**：`crates/modular-clipboard-store/src/query.rs:165`（`ORDER BY pinned DESC, created_at DESC LIMIT ?2`）对比 `query.rs:105-107`（list）与 `query.rs:139`（FTS 分支）
- **现象**：`created_at` 只到毫秒，同毫秒多条并列时 SQLite 返回序**未定义**。`list()` 与 FTS 分支都有 `id DESC` 兜底且注释阐明了理由（`query.rs:101-104`、`:26-32`），唯独 like_search 漏掉。而 like_search 正是 1-2 字符查询——全部两字中文词——的必经路径（`query.rs:130-132`）。
- **影响**：两字搜索时同毫秒插入的多条相对顺序不稳定。同毫秒插入在生产中可达：`pump` 批量排空事件队列后连续入库（`app/src/lib.rs:845-862`）。当前 UI 只用 LIMIT 不用 OFFSET（`app/src/lib.rs:965`）暂未暴露分页重复/漏行，但这是与 `find_by_hash_finds_latest_unpinned` 当年修掉的同一类偶发失败，只差一个分页需求来点燃。
- **建议修法**：SQL 补 `, id DESC`，与 `list()` 对齐。
- **严重度**：P2（可选）

### [DL-06] 模块文档声称「不开 WAL」，实现却开了 WAL——文档与实现直接矛盾
- **文件:行号**：`crates/modular-clipboard-store/src/lib.rs:7-8`（「连接本身不开WAL，因为剪贴板写入本身是低频操作，WAL 的收益不足以抵消复杂度」）对比 `crates/modular-clipboard-store/src/open.rs:97`（`pragma journal_mode = WAL`）、`:98`（`synchronous = NORMAL`）
- **现象**：lib.rs 设计要点第 3 条与 open.rs 的实际 pragma 正面冲突。gfx 探针注释（`crates/modular-clipboard-gfx/examples/tray_interact_probe.rs:462`「`Store::open` 开了 WAL」）独立证实 WAL 确实开着。
- **影响**：a) 后来者按文档推理（「没有 -wal 边文件」「单连接无需考虑快照」）会得出错误结论，这类错误注释比没有注释更坏；b) WAL + `synchronous=NORMAL` 意味着掉电/蓝屏时**最近提交可能丢失**（不损坏库，但用户可能发现重启后少了几条）——取舍本身合理，但文档宣称没开 WAL，让人以为根本没做过这个取舍；c) WAL 模式持久化在库文件上，将来想改回 rollback journal 需要显式操作。
- **建议修法**：改文档，如实写明「开 WAL + synchronous=NORMAL，掉电可能丢最近提交，剪贴板历史场景可接受」；顺带把「单连接串行由 daemon 的 mpsc 保证」这一现状写进同一节（目前它只是隐含事实）。
- **严重度**：P2（可选）

### [DL-07] gc_orphan_blobs 遇错即中断、且永不回收 .tmp 残留
- **文件:行号**：`crates/modular-clipboard-store/src/retention.rs:122-124`（只匹配 `.bin`；`remove_file(&p)?` 直接上抛）
- **现象**：a) 任一条目删除失败（文件被其他进程占用、权限）→ `?` 返回 Err，GC 半途而废，剩余孤儿要等下次启动；b) 只收 `.bin`：`write_blob_atomic`（`blobs.rs:33-45`）的 `.tmp` 若进程在 `fs::write` 与 `rename` 之间被杀（断电/强杀）会永远留下——`store/src/lib.rs:588-603` 的测试注释自认「gc 只收无引用的正式文件」，即 `.tmp` 残留是已知但未闭环的缺口。
- **影响**：长期运行的 blobs 目录可能累积 `.tmp` 垃圾；单次清理不完整。
- **建议修法**：`remove_file` 失败改为计数 + 日志、不中断循环；把 `*.tmp` 一并纳入回收（启动时未被引用的 `.tmp` 删之无风险）。
- **严重度**：P2（可选）

### [DL-08] 载荷写入路径不过 safe_blob_path：hash 直接拼进文件路径，SEC-006 只守了读/删侧
- **文件:行号**：`crates/modular-clipboard-store/src/write.rs:64-65`（`format!("{}.bin", item.hash)` 后直接 `write_blob_atomic`）、`crates/modular-clipboard-store/src/blobs.rs:16-25`（闸门仅用于读/删）、`write.rs:211,334,420`、`query.rs:200`（读/删侧闸门齐全）
- **现象**：写入侧的文件名来自 `ClipItem.hash`——pub 字段、调用方给什么就是什么。`insert_with_dims` 是 pub API，传入含 `/`、`\` 或 `..` 的 hash 会逃逸 blobs 目录（`write_blob_atomic` 内部无任何校验），构成任意文件写原语；update_content 的 hash 为内部计算（`write.rs:273`）不受影响。
- **影响**：当前全部调用方都传 `fingerprint()` 输出（hex 串，`app/src/lib.rs:487,812`），**不可触发**；属纵深防御缺口——未来任何新调用方（IPC 新命令、导入/恢复功能）传不可信 hash 即踩中。附注：Windows 设备名（`NUL`/`CON`/`AUX`）与 ADS（`a.bin:stream`）也不在 `safe_blob_path` 拦截范围内，只影响被篡改 DB 行的读/删，写入侧同样无感。
- **建议修法**：生成文件名后统一过一道与 `safe_blob_path` 同源的校验（或让 `write_blob_atomic` 内部拒绝含目录分量/`..`/空名的 name）；`safe_blob_path` 顺带拒绝 Windows 设备名与 `:`。
- **严重度**：P2（可选）

### [DL-09] delete_many 返回 ids.len() 而非实际删除数，enforce_retention 的计数口径不可信
- **文件:行号**：`crates/modular-clipboard-store/src/write.rs:437`（`Ok(ids.len())`）、`crates/modular-clipboard-store/src/retention.rs:61,99`（`removed +=`）
- **现象**：`delete_many` 的文档语义是「删除的条目数」，但对传入而不存在的 id 也计数（`DELETE ... WHERE id IN (...)` 影响 0 行不上报）。retention 传入的 id 均来自当次查询，当前不会虚高；但 `enforce_retention` 的 `Result<usize>` 对任何未来调用方都是不可信的契约。
- **影响**：目前仅统计口径问题（返回值无人消费，`app/src/lib.rs:947` 只判 Err）；属 API 契约缺陷，非数据缺陷。
- **建议修法**：事务内累计 `tx.changes()`，或删前删后各 COUNT 一次取差值，返回真实删除数。
- **严重度**：P2（可选）

---

### 排除项（数据链路：核实后不成立或已覆盖的疑点）

1. **拆分破坏不变量（任务重点 1）**：不成立。跨模块项全部是 `pub(crate)`（`write.rs:362`、`query.rs:160,182`、`blobs.rs:16,33,48`），没有提权到 `pub`、无跨 crate 泄漏；六个 `impl Store` 块的字段（`conn/blob_dir/db_path/payload_limit/owns_blob_dir`）只在 `lib.rs:58-69` 定义一次，其余文件一律经 `self.` 访问，无第二处定义、无语义分歧；`SELECT_COLS`（`query.rs:74-76`）14 列与 `row_to_item` 下标 0..13 已逐列核对一致。
2. **FTS5 'delete' 原文失配导致索引残留**：不成立。insert 时索引文本就是 preview（`write.rs:76-81,111-122`）；update_content 删旧索用库内当前 preview、插新索用新 preview（`write.rs:256-267,293-299,317-326`）；delete/delete_many 同样取库内 preview（`write.rs:184-192,372-392`）。全链路索引文本与 preview 恒等，'delete' 不会因原文不匹配而静默失败。
3. **单连接串行假设被绕过（任务重点 6）**：未发现。生产路径只有一个 Store 实例：daemon 受全局单实例互斥保护（`crates/modular-clipboard/src/main.rs:294`），IPC 命令经 mpsc 串行化到主线程（`daemon.rs:443-457`）；UI 进程走 Remote 后端、自己不开库（`app/src/lib.rs:282-312`）；gfx 探针只读文件元数据不开连接。这一点是干净的。
4. **保留策略死循环（任务重点 3）**：不成立。容量循环每轮要么 break（无受害者/零释放），要么删掉一批 `blob_len>0` 的未置顶条目使 SUM 严格下降且可淘汰集合严格收缩（`retention.rs:96-98`），终止性可证；置顶/无载荷条目不参选即退出。「置顶永不淘汰」成立：数量与容量两条路径的 SQL 都带 `pinned = 0`。
5. **SEC-005 的 junction 检查被绕过**：实测不成立。本机（rustc 1.99.1 / Windows 11）用 `mklink /J` 建立 junction 后验证：`symlink_metadata().file_type().is_symlink()` 对其返回 `true`，`open.rs:69-75` 的检查确实覆盖 junction。
6. **`stats()` 的 SUM(blob_len) 容量会计失真**：不构成当前问题。同哈希共享文件会被多条各计一次 → 结果是**高估**（淘汰方向保守，不会少删）；且 `stats()` 全仓无调用方（仅定义于 `query.rs:206`）。如实记录，备将来接入设置页时注意。
7. **fingerprint 截断 16 字节（64 位）碰撞**：可接受。万条规模生日碰撞概率 ~10^-12 量级；且碰撞后果（两内容互覆同一文件）与既有的「同哈希共享文件」语义同类，非新问题。
8. **配置下调后不立即执行保留策略**：设计取舍而非缺陷。`enforce_retention` 只在插入后触发（`app/src/lib.rs:947`），用户调小上限后要等下一次复制才生效——只会延迟删，不会多删或错删。

---

## 小结：数据链路（DL）

按严重度统计：**P1 × 3，P2 × 6**（共 9 条：DL-01/02/04 为 P1，DL-03/05/06/07/08/09 为 P2）。无 P0。

最该先修的 3 条：

1. **DL-04**（P1）：`max_items ≥ 2^63` 时 `as i64` 变负、`unpinned_keep` 归零，下一次复制即删光全部未置顶历史。修法是一处 clamp（load 时或 store 侧改 u64 运算），成本一行、收益是消除唯一一条「一个坏配置值 = 全库未置顶数据灭绝」的路径。
2. **DL-01**（P1）：去重命中不补写载荷，调大一次「单条载荷上限」再复制同一内容，即可稳定造出「列表可见、复制必败」的僵尸条目。影响面是图片/大文本这类最可能超限的内容，且无任何自愈机制。
3. **DL-02**（P1）：insert 失败回滚无引用计数地删共享载荷文件，违反 DF-01 自己确立的纪律（删除侧守、插入失败侧不守）。触发需一次 DB 写失败（磁盘满最现实），后果是无关条目载荷静默消失。改为「不删、留给 gc」即可闭环。

P2 的 6 条中，DL-05（补一个 `id DESC`）与 DL-09（返回真实删除数）是零风险小改动，建议随下一轮顺手处理；DL-06 是文档与实现矛盾的修正，应在有人继续按错误文档做决策之前完成。

---

## 审查：渲染与资源管理（gfx）

审查范围：`crates/modular-clipboard-gfx/src/` 全部 15 个源文件（`chrome.rs` + `chrome/{hit,corners}.rs`、`frame.rs` + `frame/renderer.rs`、`window.rs` + `window/{event_loop,keys}.rs`、`lib.rs`、`staging.rs`、`texture.rs`、`buffer.rs`、`blur.rs`、`backdrop.rs`、`pipeline.rs`、`shader.rs`）。附属窗口的 RedrawGate / 重建风暴 / 关窗同步在 ui  crate 的调用点上，已一并核对作为跨仓证据。未修改任何源码。

### [GFX-01] `abandon_frame` 清 `in_flight` 但不复位栅栏：之后同槽位 `queue_submit` 会收到已 signal 的 fence
- **文件:行号**：`crates/modular-clipboard-gfx/src/frame/renderer.rs:1216-1226`（`abandon_frame` → `wait_own_inflight`）、`crates/modular-clipboard-gfx/src/frame/renderer.rs:1361-1383`（`wait_own_inflight`：1379-1381 行把**所有**槽位 `in_flight = false`，函数全程没有 `reset_fences`）、`crates/modular-clipboard-gfx/src/frame/renderer.rs:750-774`（`acquire` 的 wait + `reset_fences` 整段被 `in_flight` 门控，773 行 reset）、`crates/modular-clipboard-gfx/src/frame/renderer.rs:1109-1116`（`present` 把 `submit_fence` 交给 `queue_submit`）
- **现象**：paint 出错（纹理上传失败 / staging 2s 超时 / 描述符池耗尽）→ 调用方执行 `abandon_frame()`（`childwin.rs:482`、`settingswin.rs:343`，popup/imgedit 同款）→ `wait_own_inflight` 等完在途槽位的 fence 并把全部槽位 `in_flight` 清假，**fence 停留在已 signal 状态**。随后帧循环继续，cursor 轮转回这些槽位时，`acquire` 因 `in_flight == false` 跳过 wait/reset 整段，`present` 直接把已 signal 的 fence 传给 `queue_submit`。该状态每帧持续，直到交换链重建（`allocate_frame_resources` 销毁旧 fence，`renderer.rs:1622-1629`）才自愈。
- **根因**：`in_flight`（有无未完成提交）与「fence 是否已 reset」是两个独立状态，`wait_own_inflight` 只维护前者；而 `acquire` 里的 `reset_fences` 被 `in_flight` 门控，清标志即绕过复位。
- **影响**：违反本项目自己记录的规则——`renderer.rs:1568-1579` 的注释明确写着「规范禁止把已 signal 的 fence 传给 `queue_submit`（`VUID-vkQueueSubmit-fence-00063`）」，当年正是为此把 fence 初值从 SIGNALED 改成 unsignaled。桌面驱动多数能容忍（内部重置后重新 signal），实机不必然崩，但验证层必报、且属规范 UB。**此条只能靠实机 + 验证层复现**：构造一次 paint 失败后保持窗口渲染，观察 `VUID-vkQueueSubmit-fence-00063`。
- **建议修法**：`wait_own_inflight` 等待成功后，对等到的这一批 fence 调 `reset_fences`（它们必然已 signal，reset 合法；`rebuild_swapchain` 路径随后销毁旧 fence，reset 无害）。
- **严重度**：P1（应该修：触发条件是错误路径，但触发后每帧违规且无自愈，修法是一行）

### [GFX-02] 设置窗口与卡片子窗口没有 RedrawGate：空闲态持续 ~125Hz 全量 GPU 提交
- **文件:行号**：`crates/modular-clipboard-ui/src/app/frame_loop.rs:728-753`（主窗口 `skip_render` 空闲分支：`poll_for(8ms)` 后仍调 `drive_attach_windows`）、`crates/modular-clipboard-ui/src/app/frame_loop.rs:1411-1418`（子窗口循环无条件 `child.render()`）、`crates/modular-clipboard-ui/src/app/frame_loop.rs:1466-1487`（设置窗口无条件 `w.sync_and_render(...)`）；对照有闸门的：`crates/modular-clipboard-ui/src/popup.rs:493`、`crates/modular-clipboard-ui/src/imgedit.rs:345`（`gate.needs_redraw(q)` 不通过即 return）
- **现象**：主窗口空闲时循环以 ~125Hz（8ms 轮询）迭代，每一轮驱动全部附属窗口。设置窗口（`settingswin.rs:248-359`）与卡片子窗口（`childwin.rs:393-488`）**没有 RedrawGate**：画面完全静止时仍走完整 `run_ui + tessellate + rebuild_if_needed + acquire + paint + present`。开一个设置窗或拖出一个卡片子窗，就多一条 ~125Hz 的持续 GPU 提交链（磨砂窗口还连带 30fps 桌面捕获，`backdrop.rs:79`）。
- **根因**：`RedrawGate` 只给 popup.rs 与 imgedit.rs 接了线；settingswin / childwin 是更早的窗口类型，PF-4 闸门治理时漏接。
- **影响**：空闲态 GPU/CPU 持续占用与耗电——正是主窗口 PF-4 闸门要消灭的那类浪费，只是转移到了附属窗口。每多一个无闸门窗口线性叠加。**占用幅度需实机测量**（开关设置窗前后的 GPU 占用 / 功耗对比）。
- **建议修法**：把 `popup.rs:173-253` 的 `RedrawGate` 复用给 settingswin 与 childwin；`imgedit.rs:345-366` 就是现成接线样例（入口 `if !self.gate.needs_redraw(q) { return Ok(...) }`，出口 `note_drawn`）。
- **严重度**：P1

### [GFX-03] 交换链重建风暴兜底只覆盖 `rebuild_if_needed`：主窗口与 acquire 失败路径无兜底
- **文件:行号**：`crates/modular-clipboard-ui/src/multiwindow.rs:183,259-297`（`REBUILD_STREAK_LIMIT = 4` 与连续重建计数，只挂在 `WindowGfx::rebuild_if_needed` 上）；绕过计数器的路径：`crates/modular-clipboard-ui/src/app/frame_loop.rs:1579-1596`（主窗口 `rebuild()`，被 422/534/560/624/645/957/968 共 7 个点调用，主窗口从不经过 `rebuild_if_needed`）、`crates/modular-clipboard-ui/src/childwin.rs:461-467` 与 `crates/modular-clipboard-ui/src/settingswin.rs:306-313`（acquire 返回 None 后直接 `fr.rebuild_swapchain(...)`）
- **现象**：「同一目标尺寸连续重建 ≥4 次即停手告警」的兜底只保护四个附属窗口的正常尺寸路径。主窗口的尺寸兜底在 `frame_loop.rs:954` 直接调 `rebuild()`；acquire 失败后的重建也直接调 `rebuild_swapchain`。这些路径每次都过 `wait_own_inflight`（`renderer.rs:1242`）并整体销毁/重建 framebuffer、命令缓冲、描述符池、staging（`renderer.rs:1299-1335`）——驱动若持续给出与请求不符的 extent，就是每帧一次全套重建。
- **根因**：风暴记账放在了 ui 的 `WindowGfx` 层，而 `FrameRenderer::rebuild_swapchain` 才是所有调用路径的公共入口。
- **影响**：2026-10-22 把 `device_wait_idle` 换成 `wait_own_inflight` 后，风暴的爆炸半径已从「全体窗口卡顿」缩到「本窗口每帧自废」，故列 P2 而非 P0/P1；但主窗口完全无兜底，极端驱动下每帧全套重建（可观测：`painter.stats.rebuilds`，`frame_loop.rs:1109`）。
- **建议修法**：把 streak 记账下沉到 `FrameRenderer::rebuild_swapchain`（按 `desired_extent` 连续未收敛计数，超限停手并告警），主窗口与附属窗口自动全覆盖；或在主窗口 `rebuild()` 里复刻同样的 `last_want/streak` 簿记。
- **严重度**：P2

### [GFX-04] 关窗路径仍是设备级 `device_wait_idle`：关一个附属窗口排空主窗口在途工作三次
- **文件:行号**：`crates/modular-clipboard-gfx/src/frame/renderer.rs:1662-1673`（`FrameRenderer::drop` 调 `gpu.wait_idle()`）、`crates/modular-clipboard-gfx/src/lib.rs:524-532`（`destroy_surface` 内部 `wait_idle`，被 `popup.rs:572` / `childwin.rs:310` / `settingswin.rs:384` / `imgedit.rs:951` 四个 Drop 调用）、`crates/modular-clipboard-ui/src/renderer/painter.rs:1560-1562`（`Painter::drop`）
- **现象**：帧路径（rebuild / abandon / acquire）已只等自己的 fence，但**关窗路径**没有：关一个附属窗口 = ui 侧 Drop 一次 `wait_idle` + `FrameRenderer::drop` 一次 + `Painter::drop` 一次，共 3 次 `vkDeviceWaitIdle`，每次都把主窗口的在途 GPU 工作一并排空。
- **根因**：销毁 surface / staging / 纹理的前置条件是「无在途引用」，历史上用设备级等待一把梭；多窗口共享设备后这把梭会波及他人。
- **影响**：关弹窗/设置窗/子窗的瞬间主窗口帧循环被排空一至三次（一次性停顿/掉帧，非持续问题）。频繁开关设置窗时可感知。
- **建议修法**：这些资源只可能被**本窗口**提交的命令引用，可用该窗口自己的 `submit_fence` 等待替代（把 `FrameRenderer::wait_own_inflight` 提为公开入口，surface 销毁前先等本窗口在途）。进程级 `Gpu::drop` 保留 `device_wait_idle` 合理。
- **严重度**：P2

### [GFX-05] painter 的内容事件型 `device_wait_idle`：附属窗口换一张新尺寸缩略图就全局停顿一次
- **文件:行号**：`crates/modular-clipboard-ui/src/renderer/painter.rs:1259`（字体图集扩容）、`painter.rs:1368`（用户纹理换尺寸）、`painter.rs:1499`（缩略图超 64 张 LRU 淘汰）、`painter.rs:1420`（描述符集合分配失败清理）、`painter.rs:1524`（`drop_user_texture`）、`painter.rs:426`（关闭磨砂）、`painter.rs:757` / `painter.rs:1046`（离屏链 / 磨砂资源按新尺寸重建）
- **现象**：以上均为**内容事件驱动**的设备级同步：第一次出现某种新尺寸的缩略图（1368）、字体图集扩容（1259）、缩略图超过 64 张触发 LRU（1499）等，每一次都排空全设备在途工作。附属窗口（设置窗里看缩略图、编辑窗）产生的这类事件会直接停顿主窗口管线。
- **根因**：同 GFX-04——销毁「仅被本窗口命令引用」的资源时用了设备级等待。
- **影响**：事件频率远高于关窗（滚动图片列表时每种尺寸各一次），每次停顿跨越所有窗口。**停顿幅度需实机测量**（连续滚动含图列表时主窗口帧率抖动）。
- **建议修法**：同 GFX-04 换成本窗口槽位 fence 等待；优先处理 1368 / 1499 两条高频路径。
- **严重度**：P2

### [GFX-06] `on_swapchain_rebuilt` 的安全注释与实现不符（声称已 `device_wait_idle`，实际只等了本窗口）
- **文件:行号**：`crates/modular-clipboard-ui/src/renderer/painter.rs:1109-1110` 与 `painter.rs:1129`（注释「`rebuild_swapchain` 已 `device_wait_idle`，销毁安全」）；实际实现见 `crates/modular-clipboard-gfx/src/frame/renderer.rs:1228-1242`（2026-10-22 起只调 `wait_own_inflight`）
- **现象**：两处注释以「已 `device_wait_idle`」作为销毁离屏链 / 磨砂资源 / 重配用户纹理集合的安全依据；实际保证已弱化为「本窗口在途提交已完成」。
- **根因**：注释未随 2026-10-22 的修复同步更新。
- **影响**：安全性仍然成立（这些资源只被本窗口命令引用，`wait_own_inflight` 充分），但注释与实现不符会误导后续维护者据它放宽别的约束（例如误以为可以销毁跨窗口共享对象）。
- **建议修法**：改注释为「`rebuild_swapchain` 已等完本窗口全部在途提交（`wait_own_inflight`）」。
- **严重度**：P3

---

### 排除项（核实后不成立的疑点）

1. **`rebuild_if_needed` 传零尺寸 → `FALLBACK_EXTENT` 无限重建**：当前代码不成立。四个附属窗口调用点全部传真实客户区尺寸（`popup.rs:514`、`imgedit.rs:369`、`childwin.rs:459`、`settingswin.rs:304`），主窗口 `rebuild()` 也传（`frame_loop.rs:1591-1594`）；`WindowGfx::new_for` 创建时的一次零尺寸 FALLBACK 建链（`multiwindow.rs:208`）会在首次 acquire 前被 `rebuild_if_needed` 纠正（各窗口 create 后第一帧都是先 rebuild_if_needed 再 acquire）。已核实的残留问题只有兜底覆盖面，单列 GFX-03。
2. **staging arena 池耗尽杀死界面**：不成立。三级兜底完整——同帧追加 / 真正空闲块（`staging.rs:329-333`）→ 等最老在途块栅栏 2s 超时（`staging.rs:439-454`，`oldest_inflight_chunk` 纯函数选块）→ `push_chunk` 新块（`staging.rs:469-480`）；`retire_frame` 按帧号精确退休且位于 `reset_fences` 之前（`renderer.rs:763-773`）。2s 超时仍会上抛错误，但调用方全部按「放弃本帧 + 告警」处理（`childwin.rs:482`、`settingswin.rs:343`），不会杀界面。
3. **透明与圆角互斥规则被 chrome 拆分破坏**：不成立。`corners.rs` 两条 DWMWCP 开关经 `chrome.rs:111-112` 原样再导出；唯一使用逐像素 alpha 的窗口（弹窗）在创建时同时做对三件事：`disable_rounded_corners`（`popup.rs:281`）、`set_panel_shape` 登记投影区点击穿透（`popup.rs:284-287`）、清屏 alpha=0（`popup.rs:296`）；主窗口在 backdrop≠None 时也设清屏 alpha=0（`frame_loop.rs:1045-1051`）；设置窗/子窗/编辑窗是不透明 + DWM 圆角（`WindowOpts::rounded = true`），与自绘投影无须互斥，口径一致。
4. **chrome 拆分破坏命中判定（含投影区 HTTRANSPARENT）**：不成立。`hit_test` / `popup_zone` / `HitZone` 迁至 `hit.rs` 后语义未变（`chrome.rs:107-110` 再导出），`WM_NCHITTEST` 仍走 `hit_zone_at_screen`，弹窗穿透判定仍在 `hit_test` **之前**（`chrome.rs:541-547`），`popup_panel_outside_is_click_through` 单测在位（`chrome.rs:1028-1060`）。唯一遗留观察：`set_drag_regions` 对超过 `MAX_DRAG_SEGS = 8` 的段静默截断（`chrome.rs:153,246`），当前 UI 预算 6 段，暂不触发。
5. **纹理上传两趟与交换链重建交互**：不成立。先字体图集、后用户纹理的顺序有实现保证（`painter.rs:1209-1214,1230-1231`）；`on_swapchain_rebuilt` 在描述符池被销毁后为每张用户纹理重配集合且**保留图像**（`painter.rs:1131-1160`），不会出现悬空集合或丢纹理。
6. **帧路径残留 `device_wait_idle` / 混入未提交 fence**：不成立。gfx/src 内 `device_wait_idle` 调用点只剩 `FrameRenderer::drop`（`renderer.rs:1665`）与 `Gpu::destroy_surface`（`lib.rs:529`）；`rebuild_swapchain`（1242）、`abandon_frame`（1225）、`acquire`（750-752）均只等本窗口/本槽位 fence，且只等 `in_flight` 槽位（从未提交的 unsignaled fence 不会混进 `wait_all = true`，`renderer.rs:1362-1367` 的 filter 保证）。遗留的设备级等待全在关窗/事件路径，已单列 GFX-04/05。

---

## 小结

按严重度统计：**P1 × 2，P2 × 3，P3 × 1**（共 6 条）。

最该先修的 3 条：

1. **GFX-01**（P1）：`abandon_frame` 的 fence 复位缺口——`wait_own_inflight` 等待后补一次 `reset_fences` 即可，消除一条每帧持续、只能靠交换链重建自愈的规范 UB 路径（`VUID-vkQueueSubmit-fence-00063`）。
2. **GFX-02**（P1）：给设置窗与卡片子窗补 `RedrawGate`——消灭空闲态 ~125Hz 的持续 GPU 提交（popup/imgedit 已有现成接线样例），直接省电降温。
3. **GFX-03**（P2）：把重建风暴记账下沉到 `FrameRenderer::rebuild_swapchain`——主窗口当前没有任何风暴兜底，且该入口一改即覆盖全部调用路径。

需实机验证的结论：GFX-01（验证层复现）、GFX-02（空闲 GPU 占用/功耗对比）、GFX-04/05（关窗与缩略图事件的停顿时长）。其余结论均有静态行号证据支撑。

---
