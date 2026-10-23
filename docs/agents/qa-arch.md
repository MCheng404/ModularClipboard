# qa-arch：架构合理性审查报告（2026-10-12）

> 只读审查，未改任何文件。范围：9 crate / 50242 行。方法：逐个 Cargo.toml + 实际 use 语句核对依赖边、大文件结构抽样、trait 与配置面通读、文档与实物比对、git status 与新增代码走查。

## 一、依赖方向验证结论

**实际依赖边（自 Cargo.toml + use 语句逐条确认）：**

| crate | 依赖的 workspace crate | 外部依赖要点 |
|---|---|---|
| core | 无 | serde/serde_json/thiserror/anyhow/tracing |
| store | core | rusqlite/image/directories/sha2 |
| capture | core | arboard/windows |
| platform | core, **capture** | arboard/windows |
| app | core, store, capture, platform | rtrb/directories/image |
| gfx | core, **platform**, egui | ash/bytemuck/windows/naga(build) |
| ui | core, **store**, app, gfx, platform | egui/image/**ash**/bytemuck/windows |
| mcp | core | arboard/image/base64/windows |
| bin | core, app, **mcp**, ui | tracing/directories/windows |

- **无环、无反向依赖、core 未被下游污染 crate 级依赖**：依赖图整体健康，单向向下的主旨成立。
- 但**实物图与文档图有 4 条边不一致 + 2 个 crate 缺席**（详见 AR-02/AR-04/AR-12）：
  1. `ui → store`（实物存在，文档明令禁止，见 ui/src/lib.rs:74）；
  2. `gfx → platform`（实物存在且错放 [dependencies]，仅为 example 服务）；
  3. `bin → mcp`（违反 bin 自述的「只依赖 core/app/ui 三层」约定）；
  4. `platform → capture`（良性，代码内有说明，但 ARCHITECTURE.md 未画）；
  5. gfx 在 README 分层图中的位置与实际职责不符；**mcp 完全不在任何架构图中**。
- core 的 crate 级依赖确实零 IO 库，但 **core 自身在 config.rs 里直接做 std::fs 读写**，「零 IO」基石失实（AR-01）。

## 二、发现清单（按严重度）

### P0：无

依赖图无环、无反向边、core 领域模型（item/event）干净，未发现会直接导致系统性 bug 的结构缺陷。最严重的问题集中在「文档声明与实物背离」与「单点重新集中」两类，列 P1。

### P1

- [AR-01] P1 | crates/modular-clipboard-core/src/config.rs:445-460、crates/modular-clipboard-app/src/lib.rs:668-680、crates/modular-clipboard/src/main.rs:121-130 | 「core 零 IO」基石失实，配置路径推导三处并存
  问题：ARCHITECTURE.md/README 把「core 零 IO 依赖」作为整个分层合理性的前提，但 core/src/config.rs 直接用 std::fs 做 read_to_string/write/rename/create_dir_all/remove_file（配置原子写 + 损坏备份全在 core）。同时配置路径推导存在三份并行实现：main.rs::config_path、app::default_config_path、store::default_data_dir。
  证据：app/lib.rs:100-119 的长注释记录过恰好由此类分叉产生的「读 A 写 B」历史 bug。三处推导只要再漂移一次，同类 bug 会复现。
  建议：把 Config::load/save 及原子写/备份上移到 infrastructure 侧（app 或新建 config-io 职责），core 只留纯数据结构与序列化；配置路径收敛为单一函数。

- [AR-02] P1 | platform/src/lib.rs:1-3、gfx/src/window.rs、gfx/src/chrome.rs、gfx/src/backdrop.rs、mcp/src/win.rs、mcp/src/input.rs | 「平台 API 收敛在一处」声明失实：Win32 集成三 crate 并存
  问题：ARCHITECTURE.md 称 capture+platform 把平台 API 收敛在一处。实际 Win32 集成分布在三个 crate：platform（tray/hotkey/autostart/auto_paste/窗口探测）、gfx（window.rs 2546 行的窗口+DPI+IME+事件循环、chrome.rs 拖动区/圆角、backdrop.rs GDI 捕获）、mcp（win.rs 窗口枚举/resize、input.rs SendInput 注入、capture.rs GDI 截图）。
  证据：mcp/src/capture.rs 与 gfx/src/backdrop.rs 各自实现一遍 GDI BitBlt 捕获，取舍还不同（CAPTUREBLT vs WDA_EXCLUDEFROMCAPTURE）。
  建议：明确 gfx/window 与 mcp/win 的定位（渲染宿主 / 自动化后端）并写进 ARCHITECTURE.md；至少把「屏幕捕获」与「窗口查找」抽到 platform 作为唯一实现，gfx/mcp 调用。

- [AR-03] P1 | ui/src/paint.rs（6369 行 / 137 函数） | paint.rs 重新成为神模块，UI-REWRITE 要消灭的模式换文件名复现
  问题：UI-REWRITE.md 计划新代码控制在 2000 行以内。实物：paint.rs 单文件 6369 行，混合了顶栏、搜索框、整个设置对话框（含 MCP overlay，约 1700 行）、卡片框、历史行渲染、详情/侧栏、图标绘制助手，还定义了应用级命令词汇 Op 与设置项静态表。
  证据：theme.rs:487-491 的注释自认「SCROLLBAR_WIDTH 必须与 paint.rs 的 SCROLLBAR_RESERVE 一致——两处各写一份」——正是 UI-REWRITE 判词「同一件事算两遍」的同类模式在小尺度复现。
  建议：按 UI-REWRITE 的分层精神把 paint.rs 拆为 topbar/search/settings/cards/rows 若干模块；把 SCROLLBAR_RESERVE 改为引用 theme::SCROLLBAR_WIDTH 单一来源。

- [AR-04] P1 | gfx/Cargo.toml:26-29 | gfx→platform 误入 [dependencies]，渲染库把 arboard+capture 拖进每个消费者
  问题：gfx 的 Cargo.toml 注释自认「platform 只被 example 使用，不进渲染库本身」，但它声明在 [dependencies] 而非 [dev-dependencies]（全 workspace 无任何 [dev-dependencies] 节）。实际 src/ 中零处使用 platform（仅 examples/tray_probe.rs 用）。后果：ui 的依赖图被强行加上 platform→capture→arboard→windows 整条链。
  证据：grep modular_clipboard_platform 在 gfx/src 下 0 命中。
  建议：移到 [dev-dependencies]。
  > **处置（2026-10-23）**：已直接**移除**该依赖，比降到 [dev-dependencies] 更彻底——
  > 同日仓库瘦身删掉了 `crates/*/examples/` 下的全部实机探针（`platform` 仅被
  > `tray_probe` 使用），删除后 `gfx/src` 与 `build.rs` 对其零使用。

- [AR-05] P1 | ui/src/paint.rs:1785-2060、ui/src/app.rs:1729-1797 | 配置写入通道全字符串键，四处并行 match，字段改名静默失效
  问题：设置项用 ItemKind::Bool(ConfigGroup::Ui, "always_on_top") 声明，经 Op::SetBool{field} 到 app.rs set_bool_field(cfg, g, field, v) 落地；读写两侧各有一份 get_*/set_* 字符串 match（共 4 处平行 match，覆盖 21 个字段）。core::Config 字段一旦改名，编译期毫无提示，运行期落入 warn 静默失效。
  证据：app.rs:1750/1769/1779/1795 四处 warn 兜底分支；paint.rs:1886-2021 全部字段名字符串量。
  建议：改为类型化通道，或至少加一条遍历全部 SETTINGS_CARDS 的测试，对每个字段名断言「get 与 set 都不落 warn 分支」。

- [AR-06] P1 | ui/src/app.rs:202-228、ui/src/multiwindow.rs:90-118 | Vulkan 管线组装在 ui 层内联完成且重复一份；ui 直接依赖 ash，gfx 封装边界泄漏
  问题：app.rs run_with_options 内联创建 RenderPass/DescriptorLayout/PipelineLayout/ShaderModule/GraphicsPipeline（202-228 行），随后 SharedGfx::create（multiwindow.rs:90-118）又创建了完全相同的一套。启动时实际建了两份相同管线。两份逻辑只要改一处忘一处，就出现「主窗口与子窗口行为不一致」且驱动层才报错。
  证据：app.rs:198-201 注释自述「pick_format 是 pub(crate) 外部拿不到，只能按同样偏好预选」——为绕开封装边界而复制逻辑。
  建议：gfx 导出 PipelineBundle 级工厂（或把 pick_format 提 pub），ui 只调用一次；主窗口与 SharedGfx 共用同一份 PipelineBundle。

- [AR-07] P1 | git status（47 项未提交）+ mcp/（整个 crate 未跟踪）+ gfx blur/backdrop | 演进风险：审查对象是移动靶，核心新 crate 无任何版本历史
  问题：47 个文件未提交；mcp crate（9 文件约 1700 行）整个未被 git 跟踪；gfx blur/backdrop 与 ui mcp_status 为未跟踪新文件。backdrop 还引入了第二条帧路径（painter.paint_backdrop vs paint）。
  建议：先按逻辑提交切片（mcp crate 单独一个 commit、blur/backdrop 一个、ui 接线一个）再继续功能开发；paint_backdrop 与 paint 的差异点应抽成文档化的单一差异表。

### P2

- [AR-08] P2 | app/src/lib.rs:68-88 | Service 是宽 pub 字段的上帝结构而非 trait，「服务接口」实为具体类型
  问题：Service 持有 store+state+registry+捕获线程+配置路径，pub store/state/registry 全部暴露；ui 直接依赖具体 Service。更换 app 实现不可能。
  建议：至少把 store/registry 收为私有 + 窄方法；中期可提炼 ClipboardService trait。当前耦合深度尚浅，优先级低于 AR-01~07。

- [AR-09] P2 | ui/src/childwin.rs（446 行）、ui/src/paint.rs:1905、ui/src/card.rs:122 | 卡片子窗口基础设施生产不可达，文档与设置项仍承诺「可拖出」
  问题：workspace.rs:280 detach() 的唯一生产调用方不存在——grep 确认 ws.detach( 只出现在测试里。childwin.rs 整套在生产中永远不执行，只被单测覆盖。但 card.rs:122 文档仍写「可以拖出去成为子窗口」，设置项 desc 仍写「独立分栏可拖出」。
  建议：二选一——(a) 恢复拖出口；(b) 删除 childwin.rs 并把 PinnedMode 文档/设置文案改为实情。

- [AR-10] P2 | core/src/config.rs:143-179、app/src/lib.rs:1764 | LayoutConfig 旧架构遗留滞留 core 配置面；dedup_window_secs 死字段仍有写入臂
  问题：LayoutConfig（modules 浮动/折叠/槽位 7 元组）是旧四面板架构的持久化格式，其文档引用的 From<&LayoutState> 转换目标类型已随 layout.rs 删除；app.rs:1235-1238 明确不再恢复它，但字段仍在 core 公开配置面里随 config.json 序列化。dedup_window_secs 被文档标注「不再参与去重逻辑」，set_number_field 仍为它留写入臂。
  建议：下个配置格式版本移除 LayoutConfig；删 dedup_window_secs 写入臂。

- [AR-11] P2 | README.md:6-7,138-144；ARCHITECTURE.md:33-42,149-158；UI-REWRITE.md:1-3,105-115 | 三份架构文档与代码大面积不一致
  问题：(1) README 状态行仍写「界面暂由 eframe 承载」——eframe 已整体移除；(2) README「未实现」清单全部已完成；(3) 已知限制称无托盘常驻、无全局快捷键——均已实现，仅「配置里改绑定」未接线；(4) ARCHITECTURE 线程模型表称「两个线程」——实际有 UI、捕获、托盘、mcp-probe 四个线程；(5) ARCHITECTURE 分层图把 store 画在 capture/platform 之下且完全缺失 gfx/mcp；(6) UI-REWRITE.md 第 4 步入口已删，文档未更新。
  建议：以本次审查的实物依赖边表为基准重写三份文档的图与状态段。

- [AR-12] P2 | mcp/（整体）、ui/src/mcp_status.rs:238-259 | mcp 不在任何架构图；/health 探测契约靠字符串跨 crate 耦合
  问题：mcp 是唯一完全缺席架构文档的 crate；ui 不依赖 mcp crate，改用裸 TcpStream 手搓 GET /health 探活，与 mcp/http.rs 之间靠 "modular-clipboard-mcp" 字符串和「host 空→127.0.0.1」约定耦合。
  建议：ARCHITECTURE.md 补 mcp 的分层位置；把 /health 路径与服务标识提为 mcp crate 的 pub 常量供 ui 引用。

- [AR-13] P2 | Cargo.toml:12（rust-version = "1.85"） | 声明 MSRV 低于实际语言特性要求
  问题：代码使用 let-chains（5 处），该特性 1.88 才稳定。声明 1.85 的消费者/CI 按此构建会直接失败。
  建议：rust-version 改为 "1.88"（或删掉该字段）。

- [AR-14] P2 | Cargo.toml:58-63（profile.release） | opt-level="z" 的运行时取舍未经验证，链接成本取最高档
  问题：opt-level="z" + lto="fat" + codegen-units=1 是「最小产物 + 最慢构建」的极端组合。egui tessellate、FTS5 搜索、图片解码三条交互热路径都在吃 "z" 的亏，仓库里没有实测数据支撑。lto=fat+cgu=1 让每次 release 验证都付全量链接费。
  建议：保留 "z" 可以，但补一条「搜索/首帧/缩略图解码延迟」的实机对比；或在注释里写明取舍依据。迭代期可考虑 release 验证暂用 lto="thin"。

- [AR-15] P2 | ui/src/solver.rs:551 | 一条不变式守卫测试缺少 #[test]，永不执行
  问题：layout_demand_excludes_detached_and_embedded_pinned（内含 3 组断言）没有 #[test] 属性——写在 mod tests 里但永远不会被运行。该文件其余部分的不变式都有测试覆盖，唯独这条「早前 Card::layout_width 漏掉内嵌置顶」的回归守卫静默丢失。同文件还有测试卫生问题：333 行游离的 #[cfg(test)]、497 与 509 行同一函数上两个重复 #[test]。
  建议：补上 #[test]；清理重复/游离属性。

- [AR-16] P2 | ui/src/app.rs:124-978 | run_with_options 单函数约 854 行：窗口/GPU/管线/托盘/帧循环全内联
  问题：入口函数内联了窗口创建、Vulkan 设备、管线组装、托盘启动、以及整个帧循环。drive_attach_windows 已抽出的好模式没有继续——主循环本身仍是不可单测的巨块。ui/tests/event_delivery.rs 被迫用「读 app.rs 源码文本断言」来守接线正确性。
  建议：把帧循环抽为 FrameLoop 结构体（一个 step() 方法），run_with_options 只做装配。这是让主循环变得可测试的前提。

### P3

- [AR-17] P3 | ui/tests/event_delivery.rs（全文） | 源码文本断言测试：重构即假失败。随 AR-16 的结构化改造逐步退役。
- [AR-18] P3 | mcp/src/lib.rs:33、ui/src/theme.rs:484-486、mcp/src/tools.rs:6,83 | 陈旧引用与文档小错：winit 已不存在、工具数 8 vs 10、crate::automation 死链接。随手修正；rustdoc 死链接可用 cargo doc 警告兜住。
- [AR-19] P3 | gfx/src/window.rs（2546 行）、chrome.rs（783）、backdrop.rs（369） | gfx 实质是「Win32 窗口宿主 + Vulkan 渲染器」混合体，与 platform crate 的边界模糊。建议在 ARCHITECTURE.md 用一句话划定边界。
- [AR-20] P3 | gfx/src/backdrop.rs:61-91、gfx/src/blur.rs、ui/src/renderer.rs:450-465 | 新 blur/backdrop 代码的小重复与第二帧路径。建议合并 capture_size/capture_extent；给 backdrop 路径补差异表文档。

## 三、trait 设计评估（专项）

- **Action**：设计良好。accepts/plan 分离使引擎可纯函数测试；registry 有重复 id 防护；用户规则优先于内置推断的顺序明确。可改进点：(a) ActionPlan 是封闭枚举，新动作种类需改 dispatch（可接受的取舍）；(b) label() -> &'static str 限制动态命名；(c) 无优先级/能力元数据。扩展点整体稳定，无需结构性调整。
- **Service**：不是 trait，是本项目最宽的单体抽象（见 AR-08）。
- **Store / ClipboardReader**：具体类型、API 窄而清晰，无需 trait 化。

## 四、workspace 配置评估（专项）

- workspace.dependencies 统一性良好；windows crate 的 feature 按 crate 显式声明并注释了「feature 交集」陷阱，是正确的。
- 无任何 [features] 节：平台差异全用 cfg(windows)——对 Windows-only 产品是合理简化。
- edition 2024 全 workspace 一致；rust-version 声明失实（AR-13）。
- profile.release 取舍未验证（AR-14）；.cargo/config.toml 的 crt-static/incremental/line-tables-only 是仓库里配置做得最好的部分。
- gfx build.rs 用 naga 编译期编译 WGSL→SPIR-V，保持纯 Rust 构建链，设计正确。

## 五、总体评价

**架构健康度：中上。** 依赖方向的主干是干净的（无环、单向、core 领域模型纯粹），新 UI 三件套（card/workspace/solver）是这次重构最成功的部分——单一数据源、不变式有跨宽度扫描测试守护，旧架构「同一件事算两遍」的病在设计层已根除；gfx 的 Vulkan 代码展现出罕见的正确性纪律；603 项测试 + 根因注释的回归文化是本仓最宝贵的资产。

**最需要在意的 3 个结构问题：**

1. **架构文档已不能作为审查依据（AR-01/02/04/11/12）**：「core 零 IO」「平台 API 收敛一处」「ui 不碰数据库」「两个线程」「无托盘无热键」五条支柱声明全部与实物不符。建议以本报告的实物边表为基准，一次性重写三份文档的图与状态段。
2. **「单点重新集中」模式复发（AR-03/06/16）**：UI-REWRITE 消灭了 6700 行的旧布局集中，却在 paint.rs（6369 行）+ app.rs（854 行入口函数 + 内联管线组装）重新集中了同等量级的混合职责，且管线组装出现了两份相同实现。应在 blur/backdrop 新代码落地前完成拆分。
3. **配置与平台契约的三处「单一事实源」被打破（AR-01/02/05/12）**：配置路径推导三处、平台 API 三 crate、配置字段名 4 处字符串匹配、/health 契约字符串耦合——每一处都已有真实事故记录或正在造成隐性成本。建议按「一个事实一处实现」的原则批量收敛。

另：演进风险（AR-07）建议立即处理——mcp crate 整个未跟踪，先提交再继续开发。
