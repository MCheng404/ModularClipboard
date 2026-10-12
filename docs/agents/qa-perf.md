# qa-perf：性能审查报告（2026-10-12）

> 只读审查，未改任何文件、未运行 cargo。范围：全 workspace。行号基于当前工作区。
> 已对 egui 0.36.2 依赖源码核实缓存语义（见 PF-1 说明）。

## P0（可感知卡顿/明显浪费）

- [PF-1] P0 | ui/src/paint.rs:3390-3442 | 列表每帧全量 clone items + 逐条重建 LayoutJob 做行高测量，帧成本随剪贴板总字节线性膨胀
  热点路径：`draw_history_body` 每帧执行（app.rs:575 每轮循环无条件 `ctx.run_ui`；空闲时节流为 8ms/轮 ⇒ 约 100Hz，app.rs:929）；滚动/交互时每帧必付。
  浪费：① `items` 每帧全量 `.cloned()`：最多 PAGE_LIMIT=2000 条 ×（hash/preview/source_app 三个 String + PayloadRef）；② 每帧为每条 item 新建 `LayoutJob` 并 `job.append(&item.preview, …)`（paint.rs:3767）——`LayoutJob::text += &str` 在空 String 上触发一次精确分配 + 全量 memcpy；③ 该 job 的 cache key 是对整篇 preview 的 ahash 全量哈希（epaint fonts.rs:1103 `hash((&job, ppp))`）。已核实 egui galley cache 的 `flush_cache` 按代淘汰、本帧用过的下帧保留（epaint-0.36.2/src/text/fonts.rs:742、1285-1291），且本处 job 的 max_width/ppp/字号跨帧稳定 ⇒ shaping/折行确实命中缓存、不重排；但「分配 + memcpy + 全量哈希」三项每帧对 2000 条全量重复支付。Text/Html 条目的 preview 是**全文**（见 PF-5），估算：2000 条 × 平均 4KB ≈ 8MB/帧的分配+拷贝+哈希，100Hz 空闲 ⇒ 约 1GB/s 持续内存带宽、数 ms/帧；重度文本用户（平均 40KB）≈ 80MB/帧、帧时间数十 ms，滚动明显掉帧。`draw_rail_body`（paint.rs:3858）同病，独立置顶窗再各付一份。
  建议：heights/offsets 按 (items 版本号, text_w, scale) 缓存，仅在 reload_list/字号/宽度变化时重算；`items` 改为索引迭代不 clone（`draw_item_row` 只按行号取用）；配合 PF-5 截断 preview。预估收益：空闲帧时间从数 ms～数十 ms 降至 <1ms，滚动 60fps 稳定。

- [PF-2] P0 | app/src/lib.rs:339-413 | 捕获入库全链路在 UI 线程执行（SHA-256 + ≤32MB blob 写盘 + SQLite insert/FTS + 保留策略 + 2000 行全量 reload）
  热点路径：捕获线程 rtrb 投递 → UI 线程 `pump()` 排空 → `ingest()`：`fingerprint`（368，对 payload.bytes 做 SHA-256）、`store.insert`（396，含 fs::write 整个载荷文件）、`enforce_retention`（401）、`reload_list`（404）。
  浪费：复制一张 32MB 图片时，UI 线程同步做 32MB 哈希 + 32MB 文件写 + SQLite 事务 + FTS 插入 + 保留策略删除 + 全量列表重读（含 2000 行 preview 全文物化，见 PF-6），累计数百 ms 级 UI 冻结，直接表现为「复制大图时窗口卡住、掉帧、拖动无响应」。频率：每次剪贴板变化。
  建议：`ingest` 的重活（fingerprint/insert/retention）移到工作线程，UI 线程只接收「已入库」结果做 `reload_list`；或至少把 blob 写盘与哈希异步化、retention 延迟到空闲帧。预估收益：复制大载荷的 UI 冻结从数百 ms 降到接近 0。

- [PF-3] P0 | ui/src/app.rs:124-250 + 171 | 启动路径先显示窗口再初始化，期间无消息泵；35MB 字体双拷贝 + gc_orphan_blobs 同步扫目录全部压在首帧前
  热点路径：`Window::new`(136) → `focus_window` 显示窗口(171) → `Gpu::new`(182) → 管线/FrameRenderer/Painter(202-233) → `SharedGfx::create`(246) → `App::new`(250，含 DB 打开、`install_cjk_font` 35MB 读入+`bytes.to_vec()` 二次拷贝 theme.rs:692-726、`gc_orphan_blobs` 同步遍历 blobs 目录 store lib.rs:638-661) → 托盘(277)。窗口已可见但主循环未启动，用户看到「无响应」窗壳。
  浪费：首帧时间 = 上述全部同步工作之和（字体 70MB 拷贝 + DB + 目录扫描，冷启动可达数百 ms～秒级），且期间零消息处理（拖动/关闭都无响应，可能触发 Windows 假死转圈）。
  建议：先完成 Gpu/字体/DB 初始化再显示窗口；或保留早显示但插入「加载中」提示帧并保持 `poll_for` 泵消息；`gc_orphan_blobs` 放启动后首帧之后的空闲时机。预估收益：消除启动假死观感，首帧时间减半以上。

- [PF-4] P0 | ui/src/app.rs:575-577、908-932 | 无变化也以约 100Hz 全速重跑完整 UI + tessellate + GPU 提交
  热点路径：主循环每轮无条件 `ctx.run_ui` → tessellate → acquire/paint/present；节流只在循环末尾 `poll_for(8ms)`（app.rs:929 `None => 8ms`）。鼠标微动、光标闪烁等持续消息会使实际帧率逼近 120Hz；完全静止时约 100Hz。
  浪费：静态界面每 8-10ms 重绘一遍全部内容（含 PF-1 的每帧列表成本），空闲期单核持续占用、笔记本耗电发热；present 到合成器亦产生无变化带宽。
  建议：增加「无 input 事件且无 dirty（pump 无事件、无交互状态变化）则跳过 run_ui+render，仅 poll」的短路；把空闲轮询降到 30-60Hz 或事件驱动（egui 的 `repaint_delay` 已有，app.rs:606，可作为是否需要立即重绘的依据）。预估收益：空闲 CPU 占用降一个量级；与 PF-1 叠加后整体空闲开销趋近于 0。

## P1（显著浪费）

- [PF-5] P1 | app/src/lib.rs:683-718 | preview 入库存全文，同时放大帧级 clone、DB 读取、帧级哈希三处成本
  热点路径：`build_preview` 对 `ClipKind::Text/Html` 直接 `payload.text.clone()` 全文入库；`one_line_preview()`（core/item.rs:85-93）全仓仅测试引用，生产未用。
  浪费：一条几百 KB 的文本复制 ⇒ DB 里存几百 KB preview；此后每次 `reload_list`（每次捕获/搜索/置顶/分组切换都触发，lib.rs:378/404/437）把 2000 行 preview 全文读入并物化成 String；UI 每帧再全量 clone + 哈希一遍（PF-1）。同一份全文在三层被反复搬运。
  建议：入库时按展示需要截断（如 512 字符，行高测量只需「是否超过 N 行」的信息量），完整内容已文件化在 blob 里、详情面板可读全量；`list` 的 SELECT 只取截断后的 preview 列。预估收益：DB 读取量、帧级分配/拷贝/哈希同时下降 1-2 个数量级（长文本场景）。

- [PF-6] P1 | store/src/lib.rs:409、195-197 | 列表查询 `ORDER BY pinned DESC, created_at DESC` 无复合索引，且每次事件全量重载 2000 行
  热点路径：`list()` 的 SQL 固定 `ORDER BY pinned DESC, created_at DESC LIMIT ? OFFSET ?`；现有索引只有 `idx_items_created(created_at DESC)`、`idx_items_hash`、`idx_items_group`（195-197），无 `(pinned, created_at)` ⇒ 每次查询对全表（max_items 默认 10_000）做 temp B-tree 排序；`reload_list` 每次捕获事件全量重读 + `list_groups()` 再来一次查询。
  浪费：1 万条时每次重载 = 全表排序 + 2000 行 × 3 String 物化（preview 还是全文，见 PF-5）；高频复制时每秒数次。
  建议：建 `CREATE INDEX idx_items_pinned_created ON items(pinned DESC, created_at DESC)`；reload_list 增量维护或合并短时间内的多次重载；`list_groups` 随分组变更才刷。预估收益：每次重载从数 ms + 数 MB 分配降到亚 ms 级。

- [PF-7] P1 | store/src/lib.rs:597-600、618 | 保留策略逐条 delete，每条一次事务 + FTS delete + fs remove
  热点路径：`ingest` 后每次调用 `enforce_retention`；数量超限时 `for id in ids { self.delete(id) }`（597-600），容量淘汰同样循环（604-623）。`delete`（512-547）= 2 次查询 + 事务内 3 次执行（含 FTS 'delete' 命令）+ fs::remove_file。
  浪费：一次淘汰 N 条 = N 个独立事务（各带 WAL 提交）+ N 次 FTS 删除 + N 次文件删除；批量复制触发超限时成串同步 IO，全部压在 UI 线程（PF-2 同链路）。
  建议：收集待删 id 后单事务批量 `DELETE ... WHERE id IN (…)` + 一次性 FTS `'delete'` 命令，blob 文件循环外统一删；retention 与 ingest 一并异步化。预估收益：淘汰 100 条从 100 次事务降到 1 次。

- [PF-8] P1 | app/src/lib.rs:447-490（483 行 decode_to_rgba） | 点击复制时 UI 线程整文件读载荷 + 全图解码
  热点路径：`copy_item` → `store.load_payload`（store lib.rs:489-495，`std::fs::read` 整个 blob）→ 图片分支 `decode_to_rgba`。
  浪费：复制一条 32MB 图片记录时，点击后 UI 线程同步读 32MB + 全图解码（数十至上百 ms），表现为「点了复制按钮卡一下」。
  建议：解码与读盘放工作线程，UI 线程先反馈按压态；或对图片复制直接走原始字节写剪贴板（若平台 API 支持 CF_DIB 可直接构造，免全量 RGBA 解码）。预估收益：点击响应从上百 ms 到一帧内。

## P2（可优化）

- [PF-9] P2 | ui/src/text.rs:37-63 | `elide_text` 超宽时逐字符测量，每字符一次 String 分配 + 一次全量 layout 缓存查找
  热点路径：每可见行 2 处调用（paint.rs:4115 meta 行、4137 类型/大小行）；超宽时对每字符 `ch.to_string()` + `layout_no_wrap`（内部仍走 galley cache 查找，key 为该字符的哈希）。
  浪费：~10 个可见行 × 2 处 × 20-40 字符 ≈ 400-800 次小分配 + 缓存查找/帧；缓存命中省了 shaping，但分配与哈希全付。
  建议：一次 layout 整串后按 Galley 内 glyph/段的 x advance 累加找截断点（O(log n) 二分），消除逐字符循环；或按 (text, font, max_w) 缓存截断结果。预估收益：可见区绘制每帧数百次分配归零。

- [PF-10] P2 | ui/src/app.rs:567、858、1267、1291 | 每帧 3-4 次 `std::env::var_os` 查询（MC_DIAG ×3、MC_SETTINGS ×1）
  热点路径：每帧 `draw_frame`/帧尾诊断/设置自开路径都查环境变量；Windows 上 env 查询需遍历进程环境块。
  浪费：每帧 3-4 次系统调用级查询，纯重复。
  建议：`OnceLock<bool>` 缓存（诊断开关本就无需热切换）。预估收益：每帧省 3-4 次 env 查询，微小但零成本修复。

- [PF-11] P2 | gfx/src/frame.rs:1300-1305 | `record_to` 每次调用新分配 `Vec<DrawBatch>` 收集可绘批次
  热点路径：一帧内 blur/主通道各趟 `record_to` 均执行一次 `iter().copied().filter().collect()`。
  浪费：每帧 2-3 次小 Vec 分配（批次数通常 <20，绝对量小，但每帧必付）。
  建议：复用帧级缓冲（`&mut Vec<DrawBatch>` 传入后 clear）或直接迭代 filter 不求积。预估收益：每帧省 2-3 次分配，微小。

- [PF-12] P2 | capture/src/lib.rs:90-97 | 捕获文本路径同一份内容存两份（bytes + text 双拷贝）
  热点路径：`read()` 文本分支 `bytes: t.as_bytes().to_vec()` 与 `text: Some(t)` 并存，随后 rtrb 传递、UI 线程 `fingerprint(&payload.bytes)` 与 `build_preview` 又各读一份。
  浪费：每次文本复制多一次全量分配+拷贝；payload 过队列时双份内存。
  建议：`CapturedPayload` 对文本用「bytes 或 text 二选一 + 惰性访问器」，下游按需取。预估收益：每次捕获省一次全量拷贝。

- [PF-13] P2 | store/src/lib.rs:428-436 | FTS 查询排序键 `pinned DESC, bm25(...), created_at DESC` 混合无法走索引
  热点路径：每次搜索输入（防抖后）`search()` 执行该 SQL，对全部匹配行排序。
  浪费：匹配数大时全结果集排序；FTS 参数化本身已做对（?1 绑定，无注入）。
  建议：量级（单页 2000）下影响有限；可先 `ORDER BY rank` 取 Top-N 再内存置顶优先，或接受现状。预估收益：大结果集搜索从数 ms 到亚 ms。

## P3（微观）

- [PF-14] P3 | ui/src/theme.rs:692-726 | 35MB 中文字体启动时双拷贝（fs::read Vec + `from_owned(bytes.to_vec())`），且常驻 35MB
  热点路径：每次启动一次。ARCHITECTURE.md 已记录为已知取舍。
  浪费：一次性 70MB 拷贝 + 常驻内存 35MB。
  建议：`include_bytes!`/`from_static` 或 memmap 免二次拷贝（静态嵌入则零拷贝）。预估收益：启动少 70MB 拷贝（<50ms），常驻不变。

- [PF-15] P3 | ui/src/icons.rs:284-297 | `Icon::paint` 每图标一次 `codepoint().to_string()` 分配
  热点路径：每可见行 3 个操作图标 + 其他图标位，每帧每图标一次 String 分配。
  浪费：~30-50 次小分配/帧。
  建议：码位转 `char` 直接传给 painter 的 text API（或缓存静态 str）。预估收益：每帧数十次分配归零，微小。

- [PF-16] P3 | ui/src/{solver.rs,app.rs:draw_frame} | 每帧布局求解产生 3 个小 Vec（solver）等一次性集合
  热点路径：每帧 `solve_scaled` 等。
  浪费：每帧 2-3 次小分配。
  建议：可留待与 PF-1 缓存化时一并处理；单独优化收益过低。

---

## 总体评价

性能健康度：**中上**。架构层面已有不少正确决策：egui galley cache 因稳定的 (job, ppp) 跨帧命中（省掉了最贵的 shaping）、tessellate 顶点缓冲复用（renderer.rs:193-262）、uniform/binding 脏标记（765-775）、rtrb 无锁解耦捕获与 UI、序列号轮询替代窗口钩子、自写回抑制、磨砂桌面捕获 33ms 节流 + 1/2 降采样、staging arena 稳态零分配、空闲 8ms 节流（已堵住忙等）、ThumbnailCache LRU 32MiB 已备（尚未接详情面板，属功能缺口）。

主要问题集中在**「数据层按全量/全文设计，帧层按每帧全量消费」**的放大链上：preview 存全文（PF-5）→ 每次事件全量重读（PF-6）→ 每帧全量 clone + 全量哈希（PF-1），三层叠加使帧成本与列表总字节成正比；其次是**重 IO 挂在 UI 线程**（PF-2/PF-8）和**启动顺序**（PF-3）。

最值得优化的 3 个点（按收益排序）：
1. **PF-1 + PF-5（列表帧级成本 + preview 截断）**：改动集中在 paint.rs 的 heights 缓存与索引迭代、build_preview 截断两处，不触碰架构。预估收益：空闲/滚动帧时间从「数 ms ～数十 ms（随剪贴板字节数线性增长）」降到 <1ms 且与数据量脱钩；同时 DB 读取量下降 1-2 个数量级。投入产出比最高。
2. **PF-2（ingest 异步化）**：把 fingerprint/blob 写/insert/retention 移到工作线程。预估收益：消除「复制大图/大文本时 UI 冻结数百 ms」这一最重的可感知卡顿；配合 PF-7 批量删除后，高频复制场景也不再出现成串同步 IO。
3. **PF-4（空闲跳过重绘）**：利用已有的 `repaint_delay`/dirty 判断加短路。预估收益：空闲 CPU 占用降一个量级（风扇/耗电视角直接可感），且与前两项叠加后静态界面开销趋近于零。

修复顺序建议：PF-2 → PF-1+PF-5 → PF-4 → PF-3（启动）→ PF-6/PF-7（存储索引与批量删）→ 其余 P2/P3 顺手清理。
