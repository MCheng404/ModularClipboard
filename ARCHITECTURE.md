# ModularClipboard 架构说明

## 分层与依赖方向

依赖严格单向向下，无环：

```
tiez-bin  (二进制入口、命令行、启动顺序)
   │
   ▼
tiez-ui   (egui 渲染；只依赖 tiez-app 的服务接口)
   │
   ▼
tiez-app  (服务编排、联动引擎)
   │        │
   │        ▼
   │     tiez-capture (剪贴板读取)
   │     tiez-platform (系统集成)
   │        │
   ▼        ▼
tiez-store (SQLite 持久化)
   │
   ▼
tiez-core  (领域模型、事件、配置；零 IO 依赖)
```

**为什么这样切**

- `tiez-core` 不依赖任何 IO/GUI/平台 crate，因此可被全部下游引用，不会引入循环依赖。领域模型的定义只有一处。
- `tiez-ui` 不接触数据库与剪贴板，全部经 `tiez-app` 的 `Service`。换掉整个界面不影响任何业务逻辑。
- `tiez-capture` 与 `tiez-platform` 把平台 API 收敛在一处。移植到 macOS/Linux 只需重写这两个 crate。

## 线程模型

两个线程，一个无锁队列：

| 线程 | 职责 | 禁止做的事 |
|---|---|---|
| UI 线程（egui 事件循环） | 绘制、所有 SQLite 读写、响应用户操作 | 不阻塞等待剪贴板 |
| 捕获线程 `tiez-capture` | 轮询序列号、读取剪贴板内容、投递给 UI | 不碰数据库 |

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

- **密码管理器屏蔽**：`blocked_apps` 默认含 1Password、Bitwarden、KeePass、KeePassXC，按前缀不区分大小写匹配。
- **密码框启发式**：检查前台窗口类名是否含 `Credential` / `Password` / `Logon` / `Security` / `Consent`。这是**尽力而为**，Windows 无公开 API 直接查询，因此默认还需配合进程黑名单。
- **加密存储**：未实现。载荷以明文存于 `%APPDATA%/tiez/blobs/`。这是已知缺口。

## 构建环境说明

本机Visual Studio 安装在 `D:\Program Files\Microsoft Visual Studio`（非常规路径），且系统 `PATH` 中没有 `cl.exe`。`scripts/build.sh` 负责注入 `INCLUDE` / `LIB` / `CC` 等环境变量，并设置 `MSYS_NO_PATHCONV=1` 阻止 Git Bash 的路径自动转换——否则 cl.exe 无法定位头文件。

`D:\Qt` 与本项目无关，其中的 MinGW 曾被用作备选链接器，最终改用 MSVC（Windows 原生 ABI）。

依赖选型的硬约束：**必须有一个 C 编译器**，因为 `rusqlite` 的 `bundled` feature 要编译 `sqlite3.c`。这排除了 Slint 等需要 C++ 工具链的 GUI 方案。

## 已知限制

1. **无托盘常驻**。目前是普通窗口，关闭窗口即退出进程，尚无系统托盘图标。`tiez-platform` 已实现注册表自启读写，但 UI 未接入。
2. **无全局快捷键**。`tiez-core` 已定义 `Hotkey` 配置项，但 `global-hotkey` 尚未接入服务层——因此当前没有 `Ctrl+Shift+V` 唤出。
3. **图片无缩略图预览**。详情面板只显示尺寸描述，未渲染实际图片。
4. **单页加载上限 2000 条**，未实现分页加载。
5. **载荷未加密**，明文存于 `%APPDATA%/tiez/blobs/`。
6. **中文字体常驻 35MB**，见上文实测与取舍说明。
7. **仅在 Windows 上验证过**。其他平台的 `#[cfg(not(windows))]` 分支为最小实现，未实测。
8. **自动粘贴未做焦点校验**。`auto_paste` 依赖 250ms 固定延迟，在个别卡顿场景下可能丢失按键。
