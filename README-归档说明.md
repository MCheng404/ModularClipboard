# 模块化剪贴板（ModularClipboard）

Windows 原生剪贴板管理器。自写 Vulkan 渲染后端，卡片化 UI，常驻托盘。

📄 **完整项目报告见 [`项目报告.md`](项目报告.md)** —— 进展、环境、路径、已知陷阱全在里面。

---

## 快速开始

```bash
# 构建（必须用引导脚本，MSVC 在非默认路径且不在 PATH）
./scripts/build.sh --release

# 跑测试（628 个）
./scripts/build.sh --test
```

## 目录

| 路径 | 内容 |
|---|---|
| `crates/` | 源码，8 个 crate，45,314 行 |
| `dist/modular-clipboard.exe` | 可直接双击运行（7.6 MB，无 cmd 窗口） |
| `启动剪贴板.vbs` | 静默启动器 |
| `启动剪贴板.cmd` | 排查用（**会弹 cmd 窗口**） |
| `docs/编译卡死排查.md` | ⚠️ 编译前建议先读|
| `项目报告.md` | 项目完整报告 |

## 构建注意事项

1. **务必用 `./scripts/build.sh`**，不要直接 `cargo build` —— MSVC 装在
   `D:\Program Files\...`（非默认路径）且不在 `PATH`，`rusqlite` 的 bundled SQLite
   需要编译 `sqlite3.c`。

2. **建议限制并行度** `cargo build -j 12`：cargo 默认会开满 32 核导致系统卡死。

3. **把 `target/` 加入 Defender 排除项**，否则会遇到随机的
   「拒绝访问」与 rustc ICE。

详见 [`docs/编译卡死排查.md`](docs/编译卡死排查.md)。

## 运行时路径

| 用途 | 位置 |
|---|---|
| 配置 | `%APPDATA%\modular-clipboard\config\config.json` |
| 数据库 | `%APPDATA%\modular-clipboard\data\` |
| 日志 | `%APPDATA%\modular-clipboard\data\app.log` |
