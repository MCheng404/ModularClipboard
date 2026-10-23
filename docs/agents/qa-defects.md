# qa-defects：缺陷审查报告（store / capture / platform）（2026-10-12）

> 审查范围：modular-clipboard-store / capture / platform 全部源码+测试（3,423 行，只读，未运行 cargo）。
> 关键语义疑点已用 MSDN 官方文档、微软官方 SDK 样例、SQLite FTS5 官方文档交叉验证。工作区未做任何修改。

## P0（丢数据）

- [DF-01] P0 | store/src/lib.rs:271, 540-545（关联 :251, 597-600） | 载荷文件按 `{hash}.bin` 命名且 delete 无条件删文件：同哈希多条目共享一个文件，删任意一条即永久摧毁其余条目的载荷

  问题：`insert` 用 `let name = format!("{}.bin", &item.hash);` 命名载荷（:271），而 `delete` 在事务提交后无条件 `remove_file`（:540-545）。两条记录可以合法地拥有同一哈希：① `find_by_hash` 带 `AND pinned = 0`（:251）——置顶条目不参与去重，因此「置顶 A → 再复制同一内容」会插入同哈希的 B，两者共享 `{H}.bin`；② app 层 `if cfg.capture.dedup`（modular-clipboard-app/src/lib.rs:374）——用户关掉去重后每次复制同一内容都插入一条，全部共享同一文件。
  触发条件：默认配置下：置顶某条目 → 再次复制相同内容（列表顶部出现重复项 B）→ 删除 B（或被保留策略淘汰，:597-600 与容量淘汰 :604-623 都会走到同一个 delete）→ 再点击置顶项 A 复制 → `load_payload` 读文件失败，载荷不可恢复（启动时 `gc_orphan_blobs` 也不会重建文件）。`dedup=false` 时更简单：连复制两次同一内容 → 删任意一条 → 另一条载荷即失。
  建议：最小修复——`delete` 删文件前加引用计数检查：`SELECT COUNT(*) FROM items WHERE blob_file = ?1`，为 0 才 `remove_file`（注意要在删行前计数，或计数时排除自身）。彻底修复——载荷名改用行 ID（`{id}.bin`，insert 拿到 `last_insert_rowid` 后在事务内写文件）。

## P1（特定场景功能错误）

- [DF-02] P1 | store/src/lib.rs:422-451 | FTS5 trigram 分词器对 <3 字符查询静默返回空结果，LIKE 兜底不会触发，1-2 字搜索（含最常见的两字中文词）永远搜不到

  问题：`search()` 只在 FTS 查询**报错**时回退 LIKE（:438-450）。而 SQLite 官方文档明确：「Substrings consisting of fewer than 3 unicode characters do not match any rows when used with a full-text query」——trigram 以 3 字符为最小匹配单位，1-2 字符查询是**无错误**地返回 0 行。`like_search` 存在但永远等不到它该出场的时机。
  触发条件：搜索框输入任意 1-2 个字符（如 "ab"、中文两字词 "剪贴"）→ 列表为空，尽管库里有多条匹配项；输到第 3 个字符才突然出现结果。
  建议：`search()` 在 prepare FTS 前加长度闸门：`if q.chars().count() < 3 { return self.like_search(q, limit); }`（注意按字符数而非字节数；当前数据量 2000 条全表 LIKE 可接受）。

- [DF-03] P1 | platform/src/lib.rs:131-146 | `open_url` 经 `cmd /C start "" <url>` 打开链接，URL 中的 `&` 被 cmd 当命令分隔符、`%XX` 被变量展开，带查询串的 URL 被截断/损坏

  问题：`Command::new("cmd").args(["/C", "start", "", url])` 把整行交给 cmd.exe 解析。`&` 在 cmd 行内是命令分隔符：`start "" https://x.com/?a=1&b=2` 只会打开 `https://x.com/?a=1`，剩下 `b=2` 当成另一条命令；`%` 同理会被 cmd 做变量展开后吞掉（百分号编码的 URL 必坏）。
  触发条件：对任意带 `&` 或 `%` 的 URL 执行「浏览器打开 URL」动作（Google/YouTube/淘宝搜索结果链接是重灾区）。
  建议：改走本文件已有的 `open_external`（`explorer.exe <url>`，argv 直传无 shell 解析），或用 `ShellExecuteW(None, "open", url, ...)`。附带提醒：`run_command`（:169-189）同款 `cmd /C program arg` 形状，用户配置的 `{selection}` 含 `&|>%` 同样会断句/注入，若「联动规则」本意是 shell 语义建议至少在文档中写明，否则给 arg 加引号或改用直传 argv。

## P2（边缘/隐患，当前多为潜在触发）

- [DF-04] P2 | platform/src/lib.rs:273-283（对 :226） | `is_autostart_enabled` 用 `RRF_RT_REG_SZ` 查询，与 `set_autostart` 写入的 `REG_EXPAND_SZ` 类型不匹配，读函数永远看不到写函数写的值

  问题：MSDN 明确 RegGetValue 的 dwFlags 限制类型，「如果值的数据类型不符合此条件，函数将失败」。写侧用 `REG_EXPAND_SZ`（:226），读侧用 `RRF_RT_REG_SZ`（只匹配 REG_SZ，:277）→ 自启已开启时查询返回 ERROR_UNSUPPORTED_TYPE → 函数恒返回 false。
  触发条件：当前无生产调用方（ARCHITECTURE.md 记录「UI 未接入」），属潜伏缺陷；一旦设置界面接入，表现为「开了自启，复选框永远显示未开」，用户反复点、反复写。
  建议：dwFlags 改 `RRF_RT_ANY`（或 `RRF_RT_REG_SZ | RRF_RT_EXPAND_SZ | RRF_NOEXPAND`，后者兼容 Win7）。建议补一条写-读往返单测。

- [DF-05] P2 | capture/src/lib.rs:111-119（配合 app/src/lib.rs:646-654） | 序列号基线无条件先推进，剪贴板读取一旦瞬时失败（被其他进程占用），该次复制被永久漏捕且无重试

  问题：`capture_should_read` 先调 `reader.changed()`（内部无条件 `last_sequence = seq`，:113-114），再调 `read()`。若此刻另一进程持有剪贴板锁（arboard 各 `get_*` 返回 Err）或内容读取失败，`read()` 返回 None，而序列号已被消费——后续轮次 `changed()` 恒为 false，这次复制再也回不来。
  触发条件：复制瞬间剪贴板被其他程序短暂占用（Office OLE、远程桌面、VMware 同步等场景）→ 该条历史静默缺失。正是架构文档「读取可能被其他进程短暂持有锁而阻塞数秒」描述的场景，但只防了卡死、没防丢弃。
  建议：拆分「窥探」与「提交」：`changed()` 返回 `Option<u32>` 不落基线；仅当 (a) 抑制期（吸收进基线，现有语义）或 (b) `read()` 成功/确认为空 时才提交新序列号；非抑制期 `read()` 返回 None 时回滚基线，下一轮重试。可在 capture 加 `rollback_sequence()` 或在 app 侧保存旧值恢复。

- [DF-06] P2 | app/src/lib.rs:469-472（根因在 capture/src/lib.rs:98-105） | Html 条目写回剪贴板写入的是 HTML 源码纯文本，既没保留格式、也不是可读的剥离文本

  问题：capture 对 Html 类存 `bytes = html 源码`、`text = strip_html 后的纯文本`（:98-105），注释声称「粘贴到编辑器能保留格式」；但 `copy_item` 对 Html 走 `write_text(&String::from_utf8_lossy(&bytes))`——`write_text` 只能设置 CF_UNICODETEXT，格式无从保留，用户粘到编辑器里得到的是字面 `<html>...` 标签。
  触发条件：从网页/Word 复制富文本 → 历史中出现 Html 条目 → 点击该条目复制 → 粘贴 → 满屏标签。
  建议：二选一——Html 条目写回 `strip_html` 后的 `payload.text`（需要 app 侧把 `text` 带下来或从 store 重算）；或用 arboard 的 HTML 格式接口同时设置 CF_HTML+文本，真正兑现注释里的承诺。

## P3（隐患/死代码/文档漂移）

- [DF-07] P3 | platform/src/tray.rs:198-292, 1032-1135 | 托盘 v4 回调解码的内部模型与真实 wire format 不符（注释和测试固化了错误格式）；当前行为碰巧正确，高位匹配臂是死代码

  问题：代码假定 v4 的 `lParam` 高 16 位是鼠标消息码、低 16 位是通知码（:227-241 及大段注释）。经 MSDN（NOTIFYICONDATAW：「LOWORD(lParam) contains notification events…HIWORD(lParam) contains the icon ID」）与微软官方 SDK 样例 NotificationIcon.cpp（`switch (LOWORD(lParam)) { case NIN_SELECT … case WM_CONTEXTMENU: pt = {LOWORD(wParam), HIWORD(wParam)} }`）核实：真实格式是**低 16 位=通知码（NIN_SELECT/NIN_KEYSELECT/WM_CONTEXTMENU/NIN_POPUP*），高 16 位=图标 ID（本进程恒为 1）**，坐标在 wParam。因此 `cb.mouse` 恒等于 1，`WM_LBUTTONUP/DBLCLK` 的高位臂永不命中（死代码，只在按错误格式构造的单测里绿）。
  实际行为仍然正确（这是只列 P3 的原因）：左键靠 `cb.notify == NIN_SELECT` 接住、右键靠 `cb.notify == WM_CONTEXTMENU` 接住、悬停 NIN_POPUPOPEN(0x406) 落入 `_` 臂被忽略、NIM_SETVERSION 失败后的 legacy 路径（lParam 即裸鼠标码）也由低字臂接住——v4 不投递 WM_MOUSEMOVE，不存在悬停弹窗问题。
  触发条件：无直接触发；风险在于后续维护者按注释里的错误模型「修」容易引入真 bug，且测试锁定了错误格式。
  建议：按真实格式重写 `Callback`（mouse/notify 语义对调，并校验 `HIWORD == ICON_ID`），菜单坐标可继续用 GetCursorPos（右键场景等价）；测试改用真实 lParam 构造（`(ICON_ID<<16)|NIN_SELECT` 等）。

- [DF-08] P3 | platform/src/tray.rs:456-470（对 :674-720） | 托盘停止时菜单宿主窗口与窗口类不回收，重复 start 会累积泄漏

  问题：`Resources::release()` 只销毁托盘窗口、注销托盘类；`create_menu_host` 创建的 1x1 `WS_EX_TOOLWINDOW` 宿主窗口与 `MENU_HOST_CLASS` 类从未销毁/注销，`MENU_HOST` 全局也不复位。进程退出时由 OS 回收，实际无损；但 Tray drop 后再次 `start()` 会再泄漏一个窗口并覆盖全局。
  建议：`release()` 里补 `DestroyWindow(host)` + `UnregisterClassW(MENU_HOST_CLASS)`，`MENU_HOST.store(0, ...)`。

- [DF-09] P3 | capture/src/lib.rs:129-156（配合 app/src/lib.rs:452, 597-598） | `write_*` 的 reader 级 800ms 抑制在生产路径上是死代码；且与 ARCHITECTURE.md 宣称的 900ms 不一致

  问题：`copy_item`/`write_clipboard_text` 每次 `ClipboardReader::new()` 新建临时实例，`suppress_for`/`reset_baseline` 作用在随即 drop 的对象上；真正的抑制靠 app 层 `last_self_write` 原子量 + `capture_should_read`。方法注释（:126-128）声称它们防自写回，实际不参与生产抑制，具有误导性。
  建议：要么在注释中注明「生产抑制由 app 层时间戳完成，本方法仅测试/独立使用」，要么把抑制状态提升到跨实例共享；顺手把 800ms 与文档 900ms 对齐。

- [DF-10] P3 | store/src/lib.rs:156（对 :6-8） | 模块文档说「连接本身不开 WAL」，`open()` 实际开启了 WAL，文档与实现相反

  问题：文件头注释 :6-8 称不开 WAL「收益不足以抵消复杂度」，:156 却 `pragma_update(None, "journal_mode", "WAL")`。实现本身合理（WAL+NORMAL 对常驻工具是正确取舍），是文档漂移。
  建议：改注释说明实际开启 WAL 及理由（或评估后关掉）；注意 WAL 模式下 `-wal/-shm` 文件需随主库一起备份。

- [DF-11] P3 | store/src/lib.rs:589-596, 597-600, 409 | 保留策略同毫秒并列时淘汰目标不确定（缺 `id DESC` 兜底），且逐条 delete 形成 N+1 事务；`list` 分页排序同样无兜底

  问题：`find_by_hash` 专门用 `ORDER BY created_at DESC, id DESC` 兜底并列（:251-252，注释详细论证过），但 `enforce_retention` 的数量淘汰（:591-592）和 `list`（:409）都没有——同一毫秒插入的多条未置顶条目，淘汰/分页取到哪一条是未定义的。另外数量淘汰对每个 id 调一次 `self.delete(id)`，每条一个事务+一次文件 IO，一次性淘汰上千条即上千次事务。
  建议：两处排序都补 `id DESC` 兜底；淘汰改为单条 SQL 批量删除 + 循环清理 FTS/文件（或至少把 delete 的文件删除延后到循环外统一做）。

- [DF-12] P3 | store/src/lib.rs:269-288 | `insert` 先写载荷文件后开事务，事务失败留下孤儿载荷文件，要等下次启动才清理

  问题：`std::fs::write`（:272）在 `transaction()`（:288）之前。FTS 插入失败或进程崩溃 → blob 文件成为孤儿；`gc_orphan_blobs` 仅在启动时运行，期间重复失败会累积。
  建议：事务提交后再写文件；或事务失败路径里补删该文件。

- [DF-13] P3 | capture/src/lib.rs:111-119 | `GetClipboardSequenceNumber` 为 32 位，理论上回绕到 `last_sequence` 时漏捕一次

  问题：`seq != self.last_sequence` 比较，序列号绕回（约 43 亿次变更，按每秒 1 次计约 136 年）恰好撞上旧值时漏一次。实际几乎不可达，仅登记。
  建议：无需处理；如顺手可改为「不等即视为变化」的窗口比较（`seq.wrapping_sub(last) > 0 && seq.wrapping_sub(last) < 2^31`）。

## 总体评价

这三个 crate 的工程质量明显高于平均：SQL 全部参数化、`escape_like` 顺序正确、contentless FTS5 的 `'delete' 命令带原文语义正确、迁移幂等、`find_by_hash` 的 `id DESC` 兜底有注释论证、`source_process_name` 句柄关闭无泄漏、自启的 UTF-16 手工缓冲区正确、线程模型与架构文档一致（捕获线程只碰 ClipboardReader+rtrb，Store 仅 UI 线程访问，托盘线程只 emit 事件，未发现违反）、rtrb 溢出丢弃策略与文档一致、饱和减法防时钟回拨想得很细。已逐条验证无问题的点还包括：FTS trigram 中文子串检索主体路径、`set_autostart`/`is_autostart_enabled` 的句柄管理、hotkey 常量与 Win32 一致、托盘左键/右键/悬停在 v4 与 legacy 两条路径下的实际行为（见 DF-07）。

真正需要动手的是 **DF-01**：它是唯一会造成不可恢复数据丢失的缺陷，且默认配置下的触发路径（置顶 → 重复制 → 删重复项）完全自然，保留策略还会自动触发。DF-02/DF-03 是用户立即可见的功能错误（两字搜索搜不到、带参链接被截断），修复量都在几行。DF-04/DF-05 是「接口已写好但调用方还没接」类的潜伏缺陷，建议在 UI 接入前一并修掉。DF-07 建议重排优先级时注意：它当前**不是**功能 bug，修的是模型和测试，别被大段注释吓着去改行为。
