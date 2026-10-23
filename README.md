# ModularClipboard

纯 Rust 实现的模块化剪贴板管理器：原生 Vulkan 图形后端（`ash` 直驱）、
**常驻进程与界面进程分离**、全局热键、开机自启、迷你快捷复制面板、
**自研磨砂背景**，并内置 **MCP 服务**，让 AI 客户端能看屏幕、点界面、读写剪贴板。

> **项目状态：Pre-Release。** 核心功能（捕获、存储、搜索、联动、托盘常驻、
> 自定义热键、自启动、自研磨砂、内容编辑、MCP 服务、中英双语界面）已完成
> 并通过 900+ 项测试；滚动测试版由 GitHub Actions 自动构建发布。

## 功能特性

**剪贴板捕获**
- 文本 / HTML / 图片 / 文件列表四类内容
- Windows 序列号轮询（`GetClipboardSequenceNumber`），不反序列化整个剪贴板
- 内容指纹去重、来源进程名记录、密码管理器黑名单（1Password / Bitwarden / KeePass / KeePassXC）
- 密码框启发式检测；自写回抑制（不会把「粘贴历史」误记成新复制）

**存储**
- SQLite + FTS5 全文索引，**中文子串检索可用**
- 载荷文件化，数据库不存大 BLOB；数量/容量双重保留，置顶永不淘汰

**界面**
- 自写 Vulkan 渲染器（ash 直驱，不经 wgpu）
- 虚拟化列表，万级条目不卡顿；搜索（`Ctrl+F` 聚焦）、分组筛选
- 明暗主题、字体缩放、**自研磨砂 / 半透明背景**（GPU 卷积实时高斯模糊）
- 自研磨砂每帧抓取窗口身后的桌面再模糊：**拖动窗口时磨砂实时跟随**（标题栏命中
  后改走非模态自定义拖动，不进入系统模态移动循环，帧循环不停）；并通过
  `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` 把自身排除在录屏/截图外，
  磨砂不会糊到自己。可在设置里调不透明度
- 设置界面：固定窗口、左右导航 + 独立圆角卡片，即改即生效（无「应用」按钮、无底栏），
  每项偏离默认值时在卡片右上角出现「重置」
- 记忆窗口位置；卡片间缝隙、列表边距等空白区域均可拖动窗口
- **内容编辑**：行内编辑文本条目（Enter 保存 / Esc 取消 / 切行自动提交），
  图片条目另开编辑器（旋转、翻转、裁剪、亮度 / 对比 / 饱和度 / 色温 / 伽马、
  画笔），修改后左下角一键撤销
- **通知方式可选**：窗内 toast 或独立弹窗；操作反馈（复制 / 删除 / 保存）不打断
  当前操作
- **中英双语界面**：默认跟随系统语言，可在设置里固定中文 / English；
  设置项名称与说明按「名词短语 + 一句话说行为」规范撰写
- 多格式详情预览

**常驻与热键**
- 托盘常驻：关闭窗口只是隐藏到托盘，后台继续记录
- 两个全局热键槽位，均可**文本输入**或**按键录制**修改：

  | 槽位 | 默认 | 说明 |
  |---|---|---|
  | 显示/隐藏主窗口 | `Win+V` | 与系统剪贴板历史同键位，接管无学习成本 |
  | 粘贴上一个条目 | `Ctrl+Alt+V` | 不弹界面，直接粘贴最近复制的内容 |

  注册失败（组合被其它软件占用）会在设置行内红字 + 状态栏提示，不会静默失灵。
- 迷你快捷复制面板（默认关闭）：按住修饰键点鼠标右键，在光标处弹出最近几条
  （默认 3 条，1~10 可配），点选即写入剪贴板；修饰键四项可勾选。
- 开机自启动：只启动常驻进程（不弹窗口），守在托盘；写在 HKCU Run，
  不需要管理员权限，可在「任务管理器 → 启动」里直接关闭。

**软件联动**
- 按内容类型自动推断动作：URL → 浏览器打开，路径 → 打开 / 定位
- 内置动作：浏览器搜索、DeepL 翻译
- 用户可配置 `ActionRule`（关键词 + 类型 → 动作），优先级高于内置推断
- 一键粘贴到前台窗口（写回剪贴板 + SendInput Ctrl+V）

**MCP 服务（AI 调用）**
- 内置 Model Context Protocol（JSON-RPC 2.0）服务，让 AI 客户端（Claude Desktop、
  Cursor 等）能够操作本机界面
- 两种传输：
  - **stdio**（本地推荐）：`--mcp-server`，由 MCP 客户端 spawn，每行一个 JSON
  - **Streamable HTTP**（远程 / 自定义）：`--mcp-http`，监听 `http://{host}:{port}/mcp`，
    支持 Bearer token 鉴权
- 安全边界（2026-10-23 加固）：这个服务能**合成键鼠、截屏、读写剪贴板**，
  因此 **token 为空时仅允许本机（loopback）访问**，且 `Host` 头不在白名单内
  （`127.0.0.1` / `localhost` / `::1`）一律拒绝——防 DNS rebinding。
  绑到非 loopback 地址（如 `0.0.0.0`）却未配 token 时**直接拒绝启动**。
  请求体上限 8MB、单行 8KB、读超时 30s
- 10 个可单独开关的界面自动化工具：屏幕截图、读取元素布局、鼠标点击 / 滚动 / 拖拽、
  文本键入、组合键、调整窗口大小、切换窗口焦点、剪贴板读写
- 设置 →「通信」：总开关 + 「ⓘ」覆盖层逐项授权，服务地址 / 端口 / 密钥可自定义；
  「一键 AI 配对提示词」按当前配置生成可直接粘贴的接入 JSON

## 快速开始

### 下载运行

从 [Releases](https://github.com/MCheng404/ModularClipboard/releases) 下载
Pre-Release，解压后运行 `modular-clipboard.exe`。首次启动会弹出主窗口，
关闭后驻留托盘，右键托盘图标可退出。

### 从源码构建

本机 Visual Studio 位于非常规路径且 `PATH` 中无 `cl.exe`，`scripts/build.sh`
负责注入编译环境：

```bash
./scripts/build.sh              # debug 构建
./scripts/build.sh --package    # 打开发包（dist/）
./scripts/build.sh --test       # 全部测试
```

若你的 MSVC 在标准位置且已在 `PATH` 中，可直接 `cargo build --release`。

运行参数：

```bash
modular-clipboard              # 正常启动（常驻进程 + 界面）
modular-clipboard --daemon     # 只启动常驻进程（开机自启用的就是它）
modular-clipboard --ui         # 只启动界面（连接已运行的常驻进程）
modular-clipboard --single     # 单进程模式（排障用）
modular-clipboard --no-capture # 不监听剪贴板（调试）
modular-clipboard --mcp-server # 以 stdio 方式运行 MCP 服务（供 MCP 客户端 spawn）
modular-clipboard --mcp-http   # 以 Streamable HTTP 方式运行 MCP 服务
modular-clipboard --data-dir <路径>
```

数据位于 `%APPDATA%/modular-clipboard/`：`config/config.json`（配置）、
`data/history.db`（SQLite + FTS5）、`data/blobs/`（载荷文件）。

## 使用说明

**改快捷键**：主窗口右上角齿轮 → 设置 →「快捷键」分类。每个槽位有两种改法：
- 直接在输入框打字（如 `Ctrl+Alt+C`），回车应用；
- 点「录制」，然后按下想要的组合（录制期间会自动暂停旧热键，
  所以**当前正绑着的组合也能录**——按下去不会再触发旧功能）。

**开机自启动**：设置 →「常规」→「开机自启动」。写入的是常驻进程
（`--daemon`），开机不弹窗口；想直接看到界面就从托盘点它。

**迷你面板**：设置 →「迷你快捷复制面板」→ 打开「启用」，勾选修饰键
（至少留一个），在任意程序里按住修饰键点右键即可。

## 架构

```
                ┌────────────────────────────┐
   双击/自启动 → │  常驻进程（--daemon）        │
                │  捕获线程 · SQLite · 托盘     │
                │  全局热键 · 迷你面板钩子       │
                └────────────┬───────────────┘
                             │ 命名管道 IPC
                             │ （u32 长度前缀 + JSON）
                ┌────────────▼───────────────┐
   唤起/热键 →   │  界面进程（--ui）            │
                │  窗口 · Vulkan 渲染 · 读列表  │
                └────────────────────────────┘
```

拆成两个进程的原因：窗口的 GPU 内存在销毁后会进驱动池、不还给系统，
关窗即回收整块；实测常驻内存从 105MB 降到 15MB。界面每帧只发一次
`Sync{known}`，版本未变回 `{"Unchanged"}`（几十字节），不传全量列表。

```
modular-clipboard (bin)
 ├─ modular-clipboard-ui → modular-clipboard-app → {capture, platform, gfx} → store → core
 └─ modular-clipboard-mcp → core        # MCP 服务：JSON-RPC + UI 自动化（stdio / HTTP）
```

详见 [ARCHITECTURE.md](ARCHITECTURE.md)；构建与图标资源见 [BUILD.md](BUILD.md)。

## 实测指标

| 指标 | 数值 |
|---|---|
| 常驻进程内存（界面关闭后） | ~15 MB |
| 界面进程内存（含中文字体 35MB） | ~105 MB |
| 空闲 CPU | < 1% |
| 万级条目列表 | 虚拟化，滚动不掉帧 |

对比参考：裸 egui（仅画一个 label，无字体无数据库）即占 128 MB——
图形栈的基线成本远高于业务代码，这也是选择 ash 直驱 Vulkan 的原因
（wgpu 会同时初始化 DX12 与 Vulkan 两条路径，双显卡机器上内存翻数倍）。

## 测试

```bash
cargo test --workspace      # 969 项（23 个测试二进制）
cargo clippy --workspace --all-targets   # 零警告
```

> 实机探针（`crates/*/examples/` 下的 `*_probe.rs`、`full_app.rs`）已于
> 2026-10-23 仓库瘦身时移除——它们只在开发期用于验证「只有真实 shell /
> 真实消息循环才能暴露的问题」，不属于交付内容。源码在 `old` 分支可查。

## 文档地图

| 想知道什么 | 读这个 |
|---|---|
| 项目是什么、怎么跑 | 本文件 |
| 架构分层、进程模型、**模块地图** | [ARCHITECTURE.md](ARCHITECTURE.md) |
| 构建环境、MSVC 注入、图标资源 | [BUILD.md](BUILD.md) |
| 逐版本变更 | [CHANGELOG.md](CHANGELOG.md) |
| DPI 三层量纲（逻辑点/物理像素/归一化） | [docs/dpi-conventions.md](docs/dpi-conventions.md) |
| 历轮代码审查发现与处置 | [docs/reviews.md](docs/reviews.md) |
| 开发史存档（并行开发黑板、UI 重构设计稿） | [docs/history.md](docs/history.md) |
| 内存分析 / 渲染管线 / 编译卡死排查 | [docs/内存分析-2026-10-12.md](docs/内存分析-2026-10-12.md) 等 |
| 第三方许可 | [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) |

## 仓库布局

`main` 只保留四类内容：**构建环境**（`Cargo.*`、`scripts/`、`.github/`）、
**文档**（根级 4 份 md + `docs/`）、**源代码**（`crates/`）、**协议**（`LICENSE` 等）。
设计原型、截图、实机探针脚本等中间产物已清除。

完整开发历史（含被清除的文件与全部提交记录）在 **`old`** 分支。

## 路线图

- 跨平台验证（目前仅 Windows）
- 载荷加密
- 动作规则编辑界面化

## 链接

- 项目主页：<https://github.com/MCheng404/ModularClipboard>
- 作者主页：<https://github.com/MCheng404>
- 协议：[GNU GPL v3.0 or later](LICENSE)（`GPL-3.0-or-later`）
