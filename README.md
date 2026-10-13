# ModularClipboard

纯 Rust 实现的模块化剪贴板管理器：原生 Vulkan 图形后端（`ash` 直驱）、
**常驻进程与界面进程分离**、全局热键、开机自启、迷你快捷复制面板。

> **项目状态：Pre-Release。** 核心功能（捕获、存储、搜索、联动、托盘常驻、
> 自定义热键、自启动）已完成并通过 657 项测试；首批 Pre-Release 已发布，
> 欢迎实测反馈。

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
- 明暗主题、字体缩放、**自研磨砂 / 半透明背景**（GPU 卷积实时模糊）
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
modular-clipboard → modular-clipboard-ui → modular-clipboard-app → {capture, platform, gfx} → store → core
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
cargo test --workspace      # 657 项
cargo clippy --workspace --all-targets   # 零警告
```

实机探针（验证「只有真实 shell / 真实消息循环才能暴露的问题」）：

```bash
cargo run --example tray_probe          # 托盘图标 + 事件链路 + 热键注册/重注册/录制暂停
cargo run --example tray_interact_probe # 真实子进程：关窗口≠退出 的四条链路
```

## 路线图

- 跨平台验证（目前仅 Windows）
- 载荷加密、图片缩略图预览、分页加载
- 动作规则编辑界面化
