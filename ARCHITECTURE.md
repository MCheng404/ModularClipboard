# ModularClipboard 架构说明

## 分层与依赖方向

依赖严格单向向下，无环：

```
modular-clipboard  (二进制入口、命令行、启动顺序)
   │
   ▼
modular-clipboard-ui   (egui 渲染；只依赖 modular-clipboard-app 的服务接口)
   │
   ▼
modular-clipboard-app  (服务编排、联动引擎)
   │        │
   │        ▼
   │     modular-clipboard-capture (剪贴板读取)
   │     modular-clipboard-platform (系统集成)
   │        │
   ▼        ▼
modular-clipboard-store (SQLite 持久化)
   │
   ▼
modular-clipboard-core  (领域模型、事件、配置；零 IO 依赖)
```

**为什么这样切**

- `modular-clipboard-core` 不依赖任何 IO/GUI/平台 crate，因此可被全部下游引用，不会引入循环依赖。领域模型的定义只有一处。
- `modular-clipboard-ui` 不接触数据库与剪贴板，全部经 `modular-clipboard-app` 的 `Service`。换掉整个界面不影响任何业务逻辑。
- `modular-clipboard-capture` 与 `modular-clipboard-platform` 把平台 API 收敛在一处。移植到 macOS/Linux 只需重写这两个 crate。

## 进程模型（2026-10-12 起）

界面与后台**分成两个进程**，之间走 Windows 命名管道：

```
常驻进程（默认双击启动）             界面进程（按需拉起，关闭即退出）
  捕获线程                            窗口 + Vulkan + egui
  SQLite 读写          ⇄ 命名管道 ⇄   每帧一次增量同步
  托盘 / 全局热键                     不碰数据库、不监听剪贴板
  IPC 服务端                          不建托盘
```

| 角色 | 启动方式 | 说明 |
|---|---|---|
| 常驻进程 | 默认（双击）/ `--daemon` | 只跑业务，**不初始化 Vulkan**，实测 15–16 MB |
| 界面进程 | 常驻进程拉起 / `--ui` | 关闭即退出，内存由系统整体回收 |
| 单进程 | `--single` | 旧行为，排查问题时更好调 |

**为什么拆**：窗口那套东西（Vulkan 设备 + 交换链 + 字体图集）在销毁后
会被驱动收进自己的池子、**不还给系统**——同进程时「关掉窗口」换不回内存。
拆开后界面进程一退，那几十 MB 是整块消失的。
详见 `docs/内存分析-2026-10-12.md` 第七节。

**协议**：请求-响应式，`u32` 长度前缀 + JSON。界面每帧问一次
「数据版本变了吗」，没变时回答只有几十字节，因此这个「每帧一问」
是廉价的。写命令（增删置顶清空）会顺带回传新列表，省一次往返。

**分层约定不变**：界面侧仍然只调 `app::Service`。`Service` 内部有
本地/远端两种后端，界面代码不需要知道自己跑在哪个进程。

## 线程模型

**常驻进程**：IPC 线程（只收发）+ 主线程（执行请求、轮询托盘、排空捕获队列）。
请求转回主线程执行是因为 `Service` 握着 SQLite 连接、不是 `Send`；
副作用是请求天然串行，界面看到的状态永远一致。

**单进程模式**（`--single`）仍是两个线程，一个无锁队列：

| 线程 | 职责 | 禁止做的事 |
|---|---|---|
| UI 线程（egui 事件循环） | 绘制、所有 SQLite 读写、响应用户操作 | 不阻塞等待剪贴板 |
| 捕获线程 `modular-clipboard-capture` | 轮询序列号、读取剪贴板内容、投递给 UI | 不碰数据库 |

通信用 `rtrb` 无锁环形队列（容量 128，溢出丢弃而非阻塞）。

**为什么必须解耦**：读取剪贴板可能被其他进程短暂持有锁而阻塞数秒。若在读取时同步写库，整个界面会跟着卡住。解耦后即使读取失败也只影响后台线程。

## 低资源占用的具体做法

### 实测数据（release 构建，本机）

| 场景 | 工作集内存 |
|---|---|
| 裸 egui（仅一个 label，无字体无数据库） | **128.65 MB** |
| ModularClipboard（未加载中文字体） | 85.19 MB |
| ModularClipboard（加载微软雅黑） | **120.16 MB** |
| ModularClipboard 空闲 CPU（5 秒采样） | 0.75%（占整机） |

**必须说明的结论**：128MB 这个数字说明 85MB 的基线主要来自 **glow/OpenGL 在 Windows 上的驱动初始化开销**，而非本项目的代码。与其猜测，不如给出实测：应用自身的业务逻辑（SQLite + 捕获 + 存储）只占几 MB 量级。

真正由本项目控制、且值得优化的只有一项：**中文字体的 35MB 成本**。

### 中文字体的 35MB

egui 内置字体不含 CJK 字形，必须加载系统中文字体。代价来自两处：

1. 字体文件本身常驻内存（`msyh.ttc` = 18.79 MB，`Deng.ttf` = 15.57 MB，`NotoSansSC-VF.ttf` = 16.95 MB）；
2. egui 将字形按需栅格化进纹理图集（`max_texture_side` 默认 2048）。

**未做的优化**：字体子集化。剪贴板内容是任意文本，若只保留界面固定文案用到的字形，用户粘贴的中文会显示为方块——这是错误的取舍。要做正确的子集化，需覆盖 GB2312 全集（约 6763 字），体积约 2-3MB，收益约 30MB。当前选择「完整覆盖 + 35MB」以保证任意内容都能正确显示。

若要进一步优化，正确方向是启动时按需延迟加载字体（先起窗口，用户真正需要显示中文时再载入），而非子集化。尚未实现。

### 其它优化手段

1. **序列号轮询而非内容轮询**。Windows 的 `GetClipboardSequenceNumber` 每次剪贴板变更递增，读它是 4 字节内存读。若改为每 300ms 反序列化整个剪贴板（包括图片解码），CPU 占用会高出一到两个数量级。

2. **载荷不进数据库**。图片等大对象以文件形式存在 `blobs/`，库中只留 `blob_file` / `blob_len` 引用。SQLite 始终保持在几十 MB 量级，查询不会因大 BLOB 变慢。

3. **列表虚拟化**。界面用 `ScrollArea::show_rows`，只为可见行构建控件。2000 条与 20 条的渲染开销基本相同。

4. **自写回抑制**。把历史条目放回剪贴板时，记录时间戳（`last_self_write`）并在 900ms 内忽略捕获。用时间戳而非布尔标志：UI 每帧都调 `pump`，布尔标志会在 16ms 后被清除，起不到抑制作用。

5. **图片只读文件头**。`compute_dims` 用 `image::ImageReader::into_dimensions()` 解析尺寸，不解码像素。

## 中文全文检索

SQLite FTS5 的分词器选择对中文是决定性的，已实测：

| 分词器 | 搜「剪贴板」在「剪贴板历史记录」中 | 结论 |
|---|---|---|
| `unicode61` | 0 条 | 无中文分词，整句成为单个 token，检索完全失效 |
| `trigram` | 1 条 | 3 字符滑窗切分，子串检索有效，无需词典 |

因此 `items_fts` 使用 `tokenize='trigram'`，代价是索引体积约为两倍，对剪贴板规模可接受。

`search()` 对用户输入做健壮性处理：FTS5 语法错误（如未闭合引号）不会导致失败，而是回退到 `LIKE` 查询。

FTS 表声明为 `content=''`（正文已在 `items.preview`），这带来一个约束：**不能用 SQL `DELETE`**，必须用 `INSERT INTO items_fts(items_fts, rowid, text) VALUES('delete', ?1, ?2)` 并提供原文。为简化删除逻辑，写入时显式指定 `rowid = items.id`，使两表键完全一致。

## 扩展性：软件联动

新增一个动作**不需要改动捕获、存储或界面的任何代码**：

```rust
struct SendToPhoneAction;

impl Action for SendToPhoneAction {
    fn id(&self) -> ActionId { "send_to_phone" }
    fn label(&self) -> &'static str { "发送到手机" }
    fn accepts(&self, item: &ClipItem) -> bool { item.kind == ClipKind::Text }
    fn plan(&self, item: &ClipItem) -> Option<ActionPlan> {
        Some(ActionPlan::RunCommand {
            program: "scrcpy".into(),
            arg: format!("--text {}", item.preview.trim()),
        })
    }
}

// 注册
registry.register(Box::new(SendToPhoneAction))?;
```

`Action` trait 把「判断能否执行」与「执行副作用」分离：引擎只产出 `ActionPlan`，由 `dispatch` 统一执行。因此联动逻辑可以脱离文件系统与网络做纯函数测试。

`ActionRegistry::with_builtins()` 已注册：浏览器打开 URL、打开文件、在资源管理器中定位、浏览器搜索、DeepL 翻译。用户配置的 `ActionRule` 优先于内置推断（按 `sort_order` 排序），因此可以覆盖默认行为。

## 保留策略

数量与容量双重上限，置顶条目永不淘汰：

- 数量淘汰时先统计置顶条目数，未置顶的保留 `max_items - pinned_count` 条。直接用 `max_items` 作 OFFSET 会把置顶条目算进窗口，导致多留N 条。
- 容量淘汰按 `created_at ASC` 依次删除最旧且有载荷的条目。
- 每次插入后立即执行，避免高频复制时库无限增长。
- 启动时 `gc_orphan_blobs` 清理主库中无对应记录的孤儿载荷文件。

## 安全与隐私

两道信任边界，都要显式设防（2026-10-23 审查后加固，详见 `docs/reviews.md` 第三轮）：

1. **命名管道** `\\.\pipe\ModularClipboard.Daemon.v1`（常驻 ⇄ 界面）。
   现状：`FILE_FLAG_FIRST_PIPE_INSTANCE` 只防第二个 daemon 抢名，**不校验客户端身份**——
   同会话任意进程连上即可驱动全部 `Command`（含 `LoadPayload` 读走全部历史载荷）。
   这是已知缺口，推荐修法是随机管道名 + 启动参数传递。
2. **MCP HTTP** `http://{host}:{port}/mcp`（AI 客户端 ⇄ 自动化服务）。
   该服务能合成键鼠、截屏、读写剪贴板，因此：
   - **token 为空 ⇒ 仅 loopback**。空 token 曾被当作「无需认证」，恶意网页一个
     `text/plain` 简单请求（免 CORS 预检）即可驱动；
   - **`Host` 头白名单**（`127.0.0.1` / `localhost` / `::1`）防 DNS rebinding；
   - **绑非 loopback 且无 token 直接拒绝启动**；
   - 传输层上限：请求体 8MB、单行 8KB、头行 64、读超时 30s。

其余措施：

- **密码管理器屏蔽**：`blocked_apps` 默认含 1Password、Bitwarden、KeePass、KeePassXC，按前缀不区分大小写匹配。
- **密码框启发式**：检查前台窗口类名是否含 `Credential` / `Password` / `Logon` / `Security` / `Consent`。这是**尽力而为**，Windows 无公开 API 直接查询，因此默认还需配合进程黑名单。
- **载荷文件路径闸门**（SEC-006）：DB 行内容不可信，`safe_blob_path` 把 `blob_file` 收敛为「blobs 目录下的纯文件名」，挡掉任意文件读/删原语；覆盖 store 侧全部读删入口。
- **临时目录防 junction**（SEC-005）：`open_in_memory` 的载荷目录名带 pid + 原子序号 + 每进程随机分量，不可预测。
- **加密存储**：未实现。载荷以明文存于 `%APPDATA%/modular-clipboard/blobs/`。这是已知缺口。

## 文档布局

| 文件 | 职责 |
|---|---|
| `README.md` | 项目是什么、怎么跑、文档地图 |
| `ARCHITECTURE.md` | 本文件：架构、进程模型、模块地图、安全边界 |
| `BUILD.md` | 构建环境、MSVC 注入、测试规范、注释标记规范 |
| `CHANGELOG.md` | 逐版本变更 |
| `docs/dpi-conventions.md` | DPI 三层量纲约定（代码注释大量引用） |
| `docs/reviews.md` | 历三轮代码审查发现与处置 |
| `docs/history.md` | 开发史存档（并行开发黑板、UI 重构设计稿、项目报告、旧 README） |
| `docs/修复记录-2026-10-14.md` | 该日修复记录 |
| `docs/内存分析-2026-10-12.md` | 双进程拆分的内存依据 |
| `docs/渲染管线审查与GPU模糊.md` | 渲染管线与磨砂实现 |
| `docs/编译卡死排查.md` | 构建卡死的排查记录 |
| `docs/agents/*.md` | 第二轮 6 路 Agent 审查的原始报告 |

`main` 分支只保留构建环境、文档、源代码、协议四类内容；
设计原型与实机探针脚本已清除，完整历史在 `old` 分支。

## 构建环境说明

本机Visual Studio 安装在 `D:\Program Files\Microsoft Visual Studio`（非常规路径），且系统 `PATH` 中没有 `cl.exe`。`scripts/build.sh` 负责注入 `INCLUDE` / `LIB` / `CC` 等环境变量，并设置 `MSYS_NO_PATHCONV=1` 阻止 Git Bash 的路径自动转换——否则 cl.exe 无法定位头文件。

`D:\Qt` 与本项目无关，其中的 MinGW 曾被用作备选链接器，最终改用 MSVC（Windows 原生 ABI）。

依赖选型的硬约束：**必须有一个 C 编译器**，因为 `rusqlite` 的 `bundled` feature 要编译 `sqlite3.c`。这排除了 Slint 等需要 C++ 工具链的 GUI 方案。

## 已知限制

1. **单页加载上限 2000 条**（`app::paths::PAGE_LIMIT`），未实现分页加载。
2. **载荷未加密**，明文存于 `%APPDATA%/modular-clipboard/blobs/`。这是已知缺口，也是安全章节里唯一未做的一项。
3. **中文字体常驻 35MB**，见上文实测与取舍说明。
4. **仅在 Windows 上验证过**。其他平台的 `#[cfg(not(windows))]` 分支为最小实现，未实测。
5. **图片编辑器是「简单」变换**：90° 旋转、水平/垂直翻转、亮度、对比度、饱和度、色温、伽马、画笔标注。没有曲线、选区、图层。

> 早期此节列的「无托盘常驻」「无全局快捷键」「图片无缩略图预览」三项**均已实现**：
> 托盘在 `platform/src/tray.rs` + `tray/imp.rs`（Shell_NotifyIcon），
> 热键在 `platform` 的 `hotkey`（双槽位 RegisterHotKey），
> 缩略图在 `ui/src/thumbnail.rs`。此处保留说明以免文档再次漂移。

## 源码模块地图（2026-10-23 重构后）

大文件已按「**删一个功能 = 删一个文件 + 一行 `mod`**」的口径拆开。
下表是各 crate 的实际布局，找不到某个功能时先查这里。

### `modular-clipboard-store`（SQLite 持久化）

| 文件 | 职责 |
|---|---|
| `lib.rs` | 句柄定义 + `Drop`、路径常量、指纹、`GroupFilter` 词汇、配置注入 |
| `open.rs` | 打开、建表迁移、位置访问（`data_dir` / `path`） |
| `query.rs` | 分页列表、全文搜索与 LIKE 回退、行反序列化 |
| `write.rs` | 插入、就地更新、置顶、删除（含引用计数删载荷） |
| `retention.rs` | 保留策略淘汰、全量清空、孤儿载荷回收 |
| `groups.rs` | 分组定义的增删改查 |
| `blobs.rs` | 载荷文件层：路径规范化（SEC-006）、原子写、尺寸嗅探 |

### `modular-clipboard-platform`（系统集成）

| 文件 | 职责 |
|---|---|
| `lib.rs` | 公共 API 门面：托盘事件、热键槽位、自启动 |
| `tray.rs` | `TrayEvent` / `Tray` 句柄 / 图标数据 / 跨平台桩 |
| `tray/imp.rs` | Win32 实现（`#[cfg(windows)]`）：图标、消息循环、热键、右键菜单、mini 面板 |

### `modular-clipboard-ui`（界面）

| 文件 | 职责 |
|---|---|
| `paint.rs` + `paint/` | 绘制主入口 + 16 个绘制子模块（list/row/cards/detail/settings_*/topbar/…） |
| `app.rs` + `app/` | 启动、配置写入、帧循环、录制、尺寸协商 |
| `imgedit.rs` + `imgedit/` | 图片编辑器；`crop` 裁剪、`adjust` 影调、`stroke` 画笔 |
| `icons.rs` + `icons/` | `font` 字体数据与度量、`draw` 矢量绘制、`md` 行内类型图标 |
| `theme.rs` + `theme/` | `secondary` 强调色、`accent`、`system` 系统色、`visuals`、`fonts` |
| `chrome.rs` + `chrome/` | `hit` 命中判定（纯函数）、`corners` DWM 圆角开关 |
| `renderer.rs` + `renderer/painter.rs` | Vulkan 绘制记录 |
| `thumbnail.rs` | 图片缩略图解码与缓存 |

### `modular-clipboard-gfx`（Vulkan 基建）

| 文件 | 职责 |
|---|---|
| `frame.rs` + `frame/renderer.rs` | 帧渲染、交换链重建 |
| `window.rs` + `window/` | `event_loop` 事件循环、`keys` 键位翻译 |
| `chrome.rs` + `chrome/` | 命中判定与圆角（见上） |

### `modular-clipboard-core`（领域模型）

`config.rs` 拆为 `config/` 11 个子模块（hotkey/storage/layout/ui/panels/card_row/window/secondary/theme_params/accent/mcp），根文件只留类型与再导出。

### `modular-clipboard-app`（服务编排）

`lib.rs` 拆出 `ingest.rs`（捕获入库管线）与 `paths.rs`（路径推导与分页/队列容量）。
