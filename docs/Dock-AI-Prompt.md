# 给 Dock 侧 AI 的适配提示词（可直接复制）

> 用途：让 MiniDock-rs 的 AI 主动实现一个通用的 Host Link v1 适配层。
> 协议权威文档：`docs/HostLink-Protocol.md`；实现清单：`docs/Dock-Integration-Guide.md`。

---

你是 MiniDock-rs 的开发者 AI。请为 Dock **主动适配**一套通用的外部应用联动协议（Host Link v1），目标程序是 ModularClipboard，但**实现必须通用**：不得硬编码应用名、安装目录、版本号、窗口标题或管道名，任何发布 `mc.host-link/v1` 清单的程序都应能接入。

## 背景（先理解，不要跳过）

- Dock 现在的 `launchpad::scan()` 只递归「开始菜单 Programs」里的 `.lnk`。ModularClipboard 是便携部署 + HKCU 自启动，天然不在那两个目录里，所以 Dock 读不到它。
- 该程序是 GUI 子系统（`windows_subsystem = "windows"`）：从资源管理器启动时没有 stdout；但通过 `Command::output()` 拉起时 JSON 仍可写入管道，且每次结果都会镜像到文件。
- `\\.\pipe\ModularClipboard.Daemon.v1` 是「界面进程 ↔ 常驻进程」的**私有**通道，带单界面保护和 `Sync{known}` 状态机。**禁止外部连接**，清单里已标注 `external_use: "not-supported"`。
- 剪贴板侧已经实现并发布了接口：`host-link.json` 自描述清单（多点发布、原子写、exe 指纹），以及 `--host-link / --host-probe / --host-publish / --host-doctor / --host-activate` 五个 CLI 子命令。

## 必读文档

优先读（任选其一，第一个失败就换下一个，不要因为读不到就放弃）：

1. `https://raw.githubusercontent.com/MCheng404/ModularClipboard/main/docs/HostLink-Protocol.md`
2. `https://github.com/MCheng404/ModularClipboard/blob/main/docs/HostLink-Protocol.md`
3. 本地文件 `D:\modular-clipboard\docs\HostLink-Protocol.md`（同一台机器，最稳）
4. 同样方式读 `docs/Dock-Integration-Guide.md`

若 Wiki 已初始化，也可用 `https://github.com/MCheng404/ModularClipboard/wiki/HostLink-Protocol`。

## 实现要求

1. 新增一个**通用** `HostLinkProvider`（不要叫 ModularClipboardProvider）：按协议第 2 节的 6 条通道顺序发现清单，任一通道缺失都继续下一条，**单个路径不存在不等于程序未安装**。
2. 校验 `schema == "mc.host-link/v1"`、`executable.path` 存在、至少校验 size/mtime 指纹；`app.id` 作为唯一去重键。
3. 所有动作只从清单的 `commands.*` 取 `exe` + `args` 原样 spawn，**不要自己拼命令行**。点击图标调用 `activate`；健康检查调用 `probe`。
4. 实现**文件兜底通道**：`%APPDATA%\modular-clipboard\host-last.json` 与 `host-probe.json`，用于无法 spawn / 拿不到 stdout 的场景。
5. 失败时按 `probe` 返回的 `error.code` 与 `repair` 执行恢复（协议第 5 节表格），不要在本地另写一套猜测式修复逻辑。
6. 缓存最近一次成功的清单，但遇到 `EXE_CHANGED` / `LINK_STALE` / `EXE_MISSING` 必须重读。
7. `ui.running: false` 是正常状态（界面按需拉起），**不是错误**。
8. 保持 MiniDock 现有规模与风格：新代码独立模块、纯英文或项目现有语言、跑 `cargo fmt` / `clippy` / `cargo test`（若项目有 gate 脚本则跑 gate）。

## 硬性禁止

- 禁止连接 `\\.\pipe\ModularClipboard.Daemon.v1`。
- 禁止把「开始菜单 `.lnk` 扫描」当作唯一发现来源。
- 禁止硬编码 `%APPDATA%\modular-clipboard`、版本、exe 文件名、窗口标题。
- 禁止用显示名 / 本地化字符串识别应用。
- 禁止假设窗口、托盘或 UI 进程常驻。
- 禁止把 `--host-*` 与其它 CLI 参数混用（这些命令要求单独执行）。
- 禁止因为一次探测失败就把条目从 Dock 永久移除；应降级为「未就绪 / 可重试」。

## 验收（逐条做故障注入，见 Guide 第 6 节）

1. 删除主清单 → 仍能通过 exe 同目录 / 开始菜单副本发现。
2. 把所有清单写成坏 JSON → `LINK_UNREADABLE`，`doctor` 修复后重新可见。
3. 把整个程序目录搬走 → 从新 exe `publish` 后 `executable.path` 自动更新、指纹通过。
4. 杀掉常驻进程 → `DAEMON_NOT_RUNNING`，`activate` 能拉起，复检变绿。
5. 替换/升级 exe → `EXE_CHANGED`，`publish` 后指纹更新。
6. 写入 `schema: mc.host-link/v99` → 优雅降级，不 panic、不猜测字段。
7. 无控制台 / 无 stdout 场景 → 走文件兜底通道仍能拿到结果。
8. 点击开始菜单 `.lnk` → 主窗口出现（参数 `--host-activate` 未被改写）。
9. 隐藏窗口让 UI 进程退出 → 不报错、条目仍在。

## 交付报告

请说明：新增/修改的文件、发现顺序的实现、错误码到行为的映射、9 条验收的实测结果；若有未通过项，明确列出原因，不要静默跳过。

---

*补充：剪贴板侧的协议实现在 `crates/modular-clipboard/src/hostlink.rs`，如需核对字段或错误码可直接读源码。Wiki 尚未初始化首页时，用 `scripts/sync-hostlink-wiki.sh` 同步文档。*
