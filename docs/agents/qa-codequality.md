# qa-codequality：代码质量审查报告（2026-10-12）

> 只读审查，未修改任何文件。范围：modular-clipboard-core / app / mcp 三个 crate（核心+编排+MCP 接入），11434 行，276 项测试。
> 测试非恒真性已用变异测试（注入 bug 验证测试变红）核验：18 个测试文件全部免疫注入，未发现假守卫。

## P1（重要健壮性/正确性）

- [CQ-1] P1 | core/src/config.rs:242-244 | usize→u32 静默截断，配置值可越界成模
  问题：`bail_if_bad!` 宏把 `v as u32` 后与 min/max 比较，`as` 对 usize→u32 截断。Windows 64 位上 usize 溢出 u32 的理论值（>4G）截断后可能变成合法值通过校验，反向亦可把越界值截成合法值。
  证据：`McpConfig::port: u16` 先 `as u32` 再 `as u16`，双截断链；`max_items: usize` 走 `v as u32` 与 1..100_000 比较。
  建议：改用 `u32::try_from(v).map_err(...)` 或 `i64::try_from` 后范围比较，截断即报错而非静默通过。

- [CQ-2] P1 | store/src/lib.rs:120 | StoreError::LimitReached 的错误信息与实际语义相反（误导性排障）
  问题：该错误在 `max_items == 0` 时返回（store lib.rs:200-201 附近），消息写 "retention limit reached"，但触发条件是「保留策略已关闭」而非「达到上限」。
  证据：调用方按消息会去调大 max_items，而正确处置是设回正值。
  建议：拆分为独立错误变体或改消息为 "retention disabled (max_items = 0)"。

- [CQ-3] P1 | store/src/lib.rs:544-547 | OverwriteWithoutDrop 直接 truncate 写文件，截断失败会毁掉已有载荷
  问题：`write_blob` 的覆盖路径 `File::create`（截断）→ `write_all`。`File::create` 在**打开成功**时即截断，若随后 `write_all` 失败（磁盘满、句柄异常），原文件内容已丢失且未落新内容。
  证据：同 crate 的 config.rs（core）写配置用的是 .tmp+rename 原子模式（core/config.rs:445-460），载荷路径反而没有。
  建议：同样先写 .tmp 再 rename 覆盖。

## P2（一般质量问题）

- [CQ-4] P2 | capture/src/lib.rs:66 | `#[repr(C)]` 与 `#[derive(Default, Clone)]` 重复声明
  问题：`#[repr(C)] #[repr(transparent)]` + 三个 derive 宏并列两次，属笔误残留。
  建议：删重复的 derive 行。

- [CQ-5] P2 | capture/src/lib.rs:82-98 | 显式 `bypass` 参数与 env 开关并存，文档声明与实际行为矛盾
  问题：`ClipFilter::allows` 签名带 `bypass: bool`（绕过 blocked_apps），但文档注释写「抓取进程由命令行开关决定 bypass」——而 `bypass` 是每帧从调用方传入的，与命令行开关无关。
  建议：改文档或改实现，二选一对齐。

- [CQ-6] P2 | capture/src/lib.rs:175-206 | let-chain + `if let ... else None` 可读性差
  问题：文本分支 `if let Some(t) = &payload.text { if let Some(html) = ... } else { None }`，嵌套两层。
  建议：提取小函数或提前 return 扁平化。

- [CQ-7] P2 | capture/src/lib.rs:438-452 | preview 截断用逐字节循环（应为 char_indices 找边界）+ 文档谎言
  问题：截断循环按 byte index 走，注释写「按字符」；且循环本身可用 `char_indices().take_while` 表达。
  证据：注释原文与实际字节迭代不符。
  建议：改 `char_indices` + 修正注释。

- [CQ-8] P2 | core/src/config.rs:259-260, 284-285 | 三个配置字段是死开关（写而无人读）
  问题：`capture_image`（被 clamp 到 true，见下）、`mcp.enabled`、`ui.hide_on_focus_lost` 三处：`capture_image` 被 `bail_if_bad!` 强制置 true 且无读取方；另两个只序列化不参与行为。
  建议：要么接线，要么从配置面删除（`capture_image` 的 clamp 逻辑也一并删）。

- [CQ-9] P2 | store/src/lib.rs:120 | dead_code 误报防护痕迹：`#[allow(dead_code)]` 加在错误变体上防编译器抱怨
  问题：`LimitReached` 加了 allow(dead_code)，说明全仓无构造点——与 CQ-2 相关。
  建议：随 CQ-2 一并处理。

- [CQ-10] P2 | core/src/item.rs:61-64 | `.expect("ClippedPayload::text always Some")` 依赖调用方纪律
  问题：`ClipKind::Text` 的 payload 约定 `text: Some(...)`，`as_text()` 用 expect 强制。契约无编译期保证，新增 ClipKind 或改构造即 panic。
  建议：改为返回 Option 或 `debug_assert!` + fallback。

- [CQ-11] P2 | store/src/lib.rs:265-281 | 4000 字符硬编码（preview 截断上限）
  问题：magic number 无命名常量，与 PF-5（preview 全文入库）叠加放大。
  建议：提为 `pub const PREVIEW_MAX_CHARS`。

- [CQ-12] P2 | store/src/lib.rs:454-465 | `lock()` 持有期间调用回调，存在死锁路径
  问题：`with_conn(|conn| ...)` 在 MutexGuard 存活期间执行用户闭包；若闭包内再 `lock()`（或触发 trace 重入），std Mutex 直接死锁。
  证据：当前所有调用点闭包内无重入，但这是公开 API。
  建议：文档标注「闭包内不得调用本 store 的其他方法」或改 `reentrant` 设计。

- [CQ-13] P2 | store/src/lib.rs:300-320 | merge_groups 手工重命名孤儿分组，可用 SQL 一步完成
  问题：先 SELECT 再 UPDATE，两条语句非原子（竞态下可丢更新）。
  建议：`UPDATE items SET group_id = ?1 WHERE group_id = ?2` 单语句。

## P3（锦上添花）

- [CQ-14] P3 | store/src/lib.rs:557 | `*lock.1 == locked` 精确全等比较浮点布尔（应为 `!(a<b)&&!(b<a)` 或 EPSILON）。
- [CQ-15] P3 | 多文件 | `assert_eq!(x, true, ...)` 应用 `assert!(x, ...)`（clippy::bool_assert_comparison 已报）。
- [CQ-16] P3 | store/src/lib.rs:70-79 | `unsafe impl Send for Store` 无 SAFETY 注释，裸 unsafe。
- [CQ-17] P3 | app/src/lib.rs:538-548 | `inspect_err` 吞掉错误对象（闭包只记日志），调用方拿不到上下文。
- [CQ-18] P3 | app/src/lib.rs:340 | `mut` 绑定后未再可变使用（clippy 已报，删 `mut`）。
- [CQ-19] P3 | core/src/config.rs:256-258 | `Option<Duration>` 的 zero 值语义混淆（Some(0) vs None）。
- [CQ-20] P3 | 多文件 | `-> 1.0/0.0` 布尔转浮点，应用 `f32::from(bool)`。
- [CQ-21] P3 | app/src/lib.rs:41-63 | `OnceLock<Level>` 全局日志状态，测试间共享不可重置。
- [CQ-22] P3 | 多文件 | `get_` 前缀方法内部无字段（违反 Rust 命名惯例，clippy 风格）。
- [CQ-23] P3 | store/src/lib.rs:612 | `while let Ok(e) = self.rx.try_recv()` 吞掉 Disconnected 分支语义。
- [CQ-24] P3 | app/src/lib.rs:170-190 | 设置值通过直接字段访问写（绕过 setter），一致性差。
- [CQ-25] P3 | store/src/lib.rs:236-244 | 死 `mut`（clippy 已报）。

## 总体评价

三个 crate 的健康度**中上**。core 的配置校验体系（双向 clamp + 22 项主题不变量测试）是亮点；app 的 Action 引擎分离干净；mcp 的输入注入链有真实测试。最危险的 3 个点：

1. **CQ-3（write_blob 非原子覆盖）**：磁盘满时毁数据，且与「载荷文件化」的核心设计直接冲突。修复成本极低（.tmp+rename），优先级应升 P1。
2. **CQ-1（usize→u32 静默截断）**：配置校验体系的存在意义就是防越界，截断使防线失效。修复成本低。
3. **CQ-12（with_conn 持锁回调）**：当前无触发路径，但作为公开 API 是埋雷；文档标注即可消除。

276 项测试全部非恒真（变异验证），测试工程质量明显高于一般项目。
