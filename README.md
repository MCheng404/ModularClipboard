# ModularClipboard

纯 Rust 实现的模块化剪贴板管理器。图形后端为**原生 Vulkan**（用 `ash` 直驱，
不经 wgpu 等抽象层）。

> **项目状态：开发中。** 剪贴板捕获、存储、搜索、联动引擎已完成并通过测试；
> 渲染管线正在迁移到自写 Vulkan 实现，界面暂由 eframe 承载。

## 实测指标

| 指标 | glow/eframe | wgpu | ash（原生 Vulkan） |
|---|---|---|---|
| 二进制体积 | 7.96 MB | 11.73 MB | 迁移中 |
| 内存（含中文字体） | 107.8 MB | 383.7 MB | 迁移中 |
| 空闲 CPU | 0.75% | 3.12% | 迁移中 |

**一个必须说清的事实**：裸 egui（仅画一个 label，无字体无数据库）就占
**128.65 MB**。也就是说上面 ~85MB 的基线来自 OpenGL 驱动初始化，
**不是业务代码**。真正由项目控制、值得优化的只有中文字体的 35MB。

选择 ash 而非 wgpu 的实测依据：wgpu 会在初始化时同时编译并枚举 DX12 与
Vulkan 两条路径，且为每个枚举到的 Adapter 加载对应厂商驱动栈。
本机是双显卡（RTX 5070 + Radeon 610M），因此 wgpu 版内存涨到 383MB、CPU 涨到 3.12%。
自写 ash 只初始化 Vulkan 一条路径。

## 渲染后端

`crates/tiez-gfx` 用 `ash` 直接驱动 Vulkan：

- 实例、表面、物理设备与逻辑设备创建
- 设备选择（支持 `MODULARCLIPBOARD_GPU=integrated|discrete` 限定显卡）
- 交换链、呈现模式（优先 FIFO，功耗最低）
- 图像布局屏障

已通过实机探针验证：成功加载 Vulkan 1.4.341 运行时，枚举到 2 个设备。

```bash
cargo run -p tiez-gfx --example probe
```

## 构建

本机 Visual Studio 位于 `D:\Program Files`（非常规路径），且 `PATH` 中无 `cl.exe`。
`scripts/build.sh` 负责注入编译环境：

```bash
./scripts/build.sh              # debug
./scripts/build.sh --release    # release
./scripts/build.sh --test       # 全部测试
./scripts/build.sh --run        # 运行
```

若你的 MSVC 在标准位置且已在 `PATH` 中，可直接用 `cargo build --release`。

## 运行

```bash
tiez                        # 正常启动
tiez --no-capture           # 不监听剪贴板（调试）
tiez --data-dir <路径>      # 指定数据目录
tiez --help
```

数据存于 `%APPDATA%/modular-clipboard/`：`history.db`（SQLite + FTS5）与 `blobs/`（图片等载荷文件）。

## 已实现功能

**捕获**
- 文本 / HTML / 图片 / 文件列表四类内容
- Windows 序列号轮询（`GetClipboardSequenceNumber`），而非反序列化整个剪贴板
- 内容指纹去重，窗口期内重复内容仅刷新时间
- 来源进程名记录与黑名单过滤（默认含 1Password / Bitwarden / KeePass / KeePassXC）
- 密码框启发式检测（前台窗口类名）
- 自写回抑制，避免把「粘贴历史」误记为新复制

**存储**
- SQLite + FTS5 全文索引，**中文子串检索可用**（分词器实测选型见 ARCHITECTURE.md）
- 载荷文件化，数据库不存大 BLOB
- 数量与容量双重保留策略，置顶条目永不淘汰
- 启动时清理孤儿载荷文件
- FTS 语法错误自动回退 LIKE 查询

**界面**
- 虚拟化列表，万级条目不卡顿
- 搜索（`Ctrl+F` 聚焦）
- 分组筛选与创建
- 多格式详情预览
- 明暗主题、字体缩放
- 运行时加载系统中文字体

**软件联动**
- 按内容类型自动推断动作：URL → 浏览器打开，路径 → 打开 / 在资源管理器中定位
- 内置动作：浏览器搜索、DeepL 翻译
- 用户可配置 `ActionRule`（关键词 + 类型 → 动作），**优先级高于内置推断**
- 外部命令联动，`{selection}` 占位符替换
- 一键粘贴到前台窗口（写回剪贴板 + SendInput Ctrl+V）

## 扩展新动作

实现 `Action` trait 并注册，**不需要改动捕获、存储或界面的任何代码**：

```rust
use tiez_app::action::{Action, ActionId, ActionPlan, ActionRegistry};
use tiez_core::ClipItem;

struct SendToPhoneAction;

impl Action for SendToPhoneAction {
    fn id(&self) -> ActionId { "send_to_phone" }
    fn label(&self) -> &'static str { "发送到手机" }
    fn accepts(&self, item: &ClipItem) -> bool {
        item.kind == tiez_core::ClipKind::Text
    }
    fn plan(&self, item: &ClipItem) -> Option<ActionPlan> {
        Some(ActionPlan::RunCommand {
            program: "scrcpy".into(),
            arg: format!("--text {}", item.preview.trim()),
        })
    }
}

let mut reg = ActionRegistry::with_builtins();
reg.register(Box::new(SendToPhoneAction))?;
```

## 架构

单向依赖，`tiez-core` 零 IO 依赖：

```
tiez-bin → tiez-ui → tiez-app → {tiez-capture, tiez-platform, tiez-gfx} → tiez-store → tiez-core
```

两个线程 + 一个无锁环形队列：UI 线程负责绘制与全部 SQLite 读写，捕获线程只做剪贴板轮询。解耦的原因是读取剪贴板可能被其他进程锁住数秒，同步写库会连带卡住界面。

详见 [ARCHITECTURE.md](ARCHITECTURE.md)。

## 未实现

**渲染层**：Vulkan 渲染管线（着色器、图形管线、帧图）、egui 图元上传绘制、
Win32 窗口与事件循环、`tiez-ui` 从 eframe 迁移到自写渲染器。

**功能层**：托盘常驻、全局快捷键唤出、图片缩略图预览、分页加载、
载荷加密、跨平台验证。
