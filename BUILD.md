# 构建、产物与测试规范

本文件是本仓库**唯一的**构建/产物/测试约定。写在代码里的说明只解释
「为什么」，具体「怎么做」一律以本文件为准。

- 目标平台：Windows，`x86_64-pc-windows-msvc`
- 工具链：rustc 1.97.1 / cargo 1.97.1，Visual Studio 18 Insiders
- 相关文件：`scripts/env.sh`、`scripts/build.sh`、`scripts/package.sh`、
  `.cargo/config.toml`

---

## 一、快速命令

```bash
./scripts/build.sh                    # debug 构建
./scripts/build.sh --release          # release 构建（不落到 dist/）
./scripts/build.sh --run              # 构建并运行
./scripts/build.sh --test             # 跑测试（默认跳过 doctest）
./scripts/build.sh --doctest          # 只跑 doctest
./scripts/build.sh --package          # 生成规范化产物（推荐用这个交付）
```

临时需要原生工具链环境时（例如 `cargo install`）：

```bash
source scripts/env.sh                 # 导出 MSVC / Windows SDK 环境变量
source scripts/env.sh --print         # 只打印关键变量，便于排查
```

**不要**在 shell 里直接 `cargo build`：`cl.exe` 不在系统 PATH 里，
cc-rs 编译 `sqlite3.c` 会失败（`failed to find tool "cl.exe"`）。

### 运行时的三个角色

程序拆成了两个进程（原因见 `docs/内存分析-2026-10-12.md` 第七节）：

```bash
./modular-clipboard.exe                  # 默认：起常驻进程 + 拉起界面
./modular-clipboard.exe --daemon         # 只常驻（后台记录 + 托盘），不弹界面
./modular-clipboard.exe --ui             # 只起界面（一般由常驻进程拉起）
./modular-clipboard.exe --single         # 单进程模式（旧行为，排查用）
```

排查问题时先看日志：常驻进程写 `{数据目录}/app.log`，
界面进程写 `{数据目录}/ui.log`（分开写，否则两边各自 truncate 会互相截断）。

---

## 二、产物规范

### 2.1 唯一产物位置

所有交付用的产物**只**允许出现在仓库根的 `dist/`。其它任何目录
（`target/`、工作区根、临时目录）里出现的 exe 都**不是**交付物。

`dist/` 布局固定为三个文件：

| 文件 | 说明 |
|---|---|
| `modular-clipboard.exe` | 主程序。单文件、静态链 CRT，双击即可运行 |
| `SHA256SUMS.txt` | 校验和，可在 `dist/` 内直接 `sha256sum -c SHA256SUMS.txt` |
| `BUILD_INFO.txt` | 版本、提交（含 dirty 标记）、构建时间、profile、target、rustc/cargo 版本、字节数 |

### 2.2 不允许残留上一次构建的产物

`package.sh` 在写入前**清空** `dist/` 的内容（保留目录本身，避免目录被占用
时删除失败）。理由：只覆盖同名文件是不够的 —— 一旦产物改名、或者上次留下了
校验和/截图/临时文件，它们会一直躺在 `dist/` 里，最终被当成"本次产物"分发出去。

已验证：手工往 `dist/` 塞入 `OLD_STALE_ARTIFACT.txt` 与 `old_dir/junk.txt`
后重新打包，两者均消失，`dist/` 只剩约定的三个文件。

### 2.3 生成方式

```bash
./scripts/package.sh              # release（默认）
./scripts/package.sh --debug      # debug 产物，仅排查用
./scripts/package.sh --no-clean   # 不清空 dist（一般不要用）
```

`package.sh` 做的事，按顺序：

1. `cargo build --release`（复用 `build.sh` 的环境引导）
2. 校验产物确实存在，不存在直接报错退出
3. **清空** `dist/` 内容
4. 拷贝 exe
5. 写 `BUILD_INFO.txt`（提交号来自 `git rev-parse`，非 git 环境降级为 `unknown`）
6. 写 `SHA256SUMS.txt`（相对路径，便于在 `dist/` 内直接校验）

### 2.4 不入库

`dist/` 已在 `.gitignore` 中。理由：8MB 二进制每次构建都是新 blob，
历史会持续膨胀且无法合并。需要分发时走 GitHub Release。

### 2.5 其它需要忽略的东西

| 路径 | 原因 |
|---|---|
| `target*/` | 各 Agent 的隔离构建目录；动态产生 |
| `scripts/__pycache__/`、`*.pyc` | Python 解释器产物 |
| `verify-render.log` / `verify-render.err` | 验证脚本重定向输出 |
| `shot-*.png` | 验证脚本截图 |
| `.commit-msg.txt` | `git commit -F` 的临时提交信息文件 |

---

## 三、测试规范

### 3.1 怎么跑

```bash
./scripts/build.sh --test                 # 全量（934 条）
./scripts/build.sh --test paint::         # 按名字过滤，透传给 cargo
./scripts/build.sh --test -- --nocapture  # 看断言的诊断输出
./scripts/build.sh --doctest              # 单独跑 doctest
```

### 3.2 性能：默认跳过 doctest

`--test` 等价于 `cargo test --workspace --lib --tests`，**不含** doctest。

原因：doctest 是 7 个独立的"编译 + 运行"目标，而本项目总共只有 1 条
（且标记为 ignored）。实测（热缓存、无其它进程争用）：

| 方式 | 耗时 |
|---|---|
| 含 doctest | 3.7 s |
| 跳过 doctest | 1.6 s |

每次跑测试多付约 2 秒，换回 0 条有意义的用例。需要跑 doctest 时用
`--doctest` 显式触发，不要把 doctest 塞回默认路径。

### 3.3 性能：测试构建只保留行表

`.cargo/config.toml` 中：

```toml
[profile.test]
debug = "line-tables-only"
```

完整调试信息让每个测试二进制都带上大量 PDB 数据，链接时间与磁盘占用
被它主导；行表已足够给出"文件 + 行号"的回溯，排查断言失败够用。
实测改动全部源码后的重编：5.9 s → 5.4 s。

只作用于 `cargo test`，不影响 `cargo build` 出来的 debug 产物。

### 3.4 不要临时切 `CARGO_INCREMENTAL`

`.cargo/config.toml` 里显式写了 `[profile.dev] incremental = true`。

增量开/关会使用**两套不同的构建缓存**，交替使用等于每次都让另一套失效，
于是每次都是全量重编 —— 看起来像"cargo 变慢了"，其实是自己在两套缓存之间
反复横跳。要固定就在 config 里改，不要在命令行覆盖。

### 3.5 测试分布（934 条 / 23 个测试二进制）

| crate / 目标 | 条数 |
|---|---|
| `modular_clipboard_ui`（lib） | 442 |
| `modular_clipboard_gfx`（lib） | 229 |
| `modular_clipboard_core`（lib） | 52 |
| `modular_clipboard_app`（lib） | 42 |
| `modular_clipboard_platform`（lib） | 40 |
| `modular_clipboard_store`（lib） | 33 |
| `modular_clipboard_mcp`（lib） | 23 |
| `modular_clipboard_capture`（lib） | 18 |
| `modular_clipboard_ipc`（lib） | 12 |
| `modular_clipboard`（main） | 8 |
| `app/tests/integration.rs` | 25 |
| `ui/tests/event_delivery.rs` | 6 |
| `ipc/tests/pipe_roundtrip.rs` | 2 |
| `gfx/tests/icon_resource.rs` | 1 |
| Doc-tests | 1 |

`ui` 与 `gfx` 两个 lib 占了七成以上——渲染与绘制是本项目测试最密的地方，
改这两处时务必连跑。

### 3.6 测试环境里必须知道的三个坑

1. **不要在同一个进程里假设测试之间互不可见。**
   `gfx::chrome::CHROME` 是**进程级**全局表（per-HWND 只是"按 HWND 索引"）。
   cargo 默认并行跑测试，两条测试共用同一批假句柄就会互相看见对方的写入。
   每条测试必须用**自己专属的**假 HWND。

2. **不要假设时间戳能区分两次写入。**
   `created_at` 只到毫秒，同一毫秒内插入两条时 `ORDER BY created_at DESC`
   是未定义的。SQL 排序必须带 `id DESC` 之类的确定性兜底。

3. **写"外部可观察"的断言之前必须先读实现。**
   曾为一个**不成立**的前提（"`required_width` 会多算内嵌置顶那一栏"）写了
   测试，注入旧实现后照样绿 —— 因为 `Workspace::docked()` 早就排除了它。
   恒绿的测试比没有测试更糟：它给人虚假的安全感。

### 3.7 关于 cargo-nextest

试过安装，**在本机装不上**：`aws-lc-sys` 的 C 代码在 MSVC 下
`lib.exe` 退出码 1181 失败。因此 `build.sh --test` 用的是原生
`cargo test`，不依赖 nextest。

（nextest 的进程级隔离确实能规避 3.6-1 那类问题，但既然装不上，
就把防线落在"各测试各用专属句柄"上，而不是留一个永不生效的分支。）

---

## 四、代码注释标记规范

### 4.1 不用图标

代码注释里**不使用**图标标记（对勾、叉、警告三角、方框、扳手等），
改用纯 ASCII 的专业术语。理由：

1. 这些字符**不在 GBK 里**。本仓库的启动脚本（`.cmd` / `.vbs`）与部分验证
   脚本是 GBK 编码，注释一旦被搬进去、或文件被工具存成 GBK，就会变成乱码
   甚至语法错误 —— 而这类问题只在"换台机器 / 换个编辑器"时才暴露。
2. 图标在 diff、终端、日志里宽度不一致，对齐会错乱。
3. 旧写法只有一个语义层级，"必须这样做"和"千万别这样做"长得一样。

### 4.2 术语表

| 标记 | 用于 |
|---|---|
| `NOTE:` | 说明、背景、为什么这么写（最常用） |
| `IMPORTANT:` | 必须满足的约束，不这么做就不对 |
| `CAUTION:` | 禁止 / 有坑，这么做会出问题 |
| `ERROR:` | 错误写法（两段对照时的前一段） |
| `CORRECT:` | 正确写法（两段对照时的后一段） |
| `TODO:` | 待办 |
| `DONE:` | 已完成 |

当前全仓分布：`IMPORTANT:` 92 处、`CAUTION:` 53 处、`NOTE:` 110 处。

### 4.3 批量工具

```bash
python scripts/strip_comment_icons.py --dry-run   # 只报告
python scripts/strip_comment_icons.py             # 实际改写
```

它按注释正文自动分级（含"不能/不要/否则"等词 → `CAUTION:`，
含"必须/务必/关键"等词 → `IMPORTANT:`，其余 → `NOTE:`），
并只处理 `crates/` 下的 `.rs` / `.py` / `.wgsl`。
该脚本自身用码位（`\u26a0` 形式）书写，不引入任何图标字符。

### 4.4 写注释时补充的两点经验

- 注释要写**为什么**，不写做了什么 —— 代码本身已经说了做了什么。
- 记下"这个坑是怎么被发现的"，比记下"这里要小心"有用得多。

---

## 五、故障速查

| 症状 | 原因 | 处理 |
|---|---|---|
| `failed to find tool "cl.exe"` | 绕过了环境引导直接跑 cargo | 走 `scripts/build.sh`，或先 `source scripts/env.sh` |
| `unable to find core` / `头文件找不到` | Git Bash 把 POSIX 路径转成了 Windows 路径 | `env.sh` 里已设 `MSYS_NO_PATHCONV=1`；不要在脚本里用 `/d/Program Files/...` 传给子进程 |
| 测试结果每次条数不同 | 跑到了旧测试二进制 | `cargo clean -p <crate>` 后重建；确认没有并行 cargo 争用 target 锁 |
| 跑测试特别慢 | 在两套增量缓存之间横跳 | 去掉命令行上的 `CARGO_INCREMENTAL`，用 config 里的固定值 |
| PowerShell 脚本乱码 / 报错 | 文件被存成错误编码 | 改后用 `Parser::ParseFile` 校验语法；必要时用 `scripts/fix_script_encoding.py` |
| `git commit -F` 被安全拦截 | 提交信息里出现 `.cmd` / `.vbs` 文件名 | 把提交信息写到文件再 `-F`，并在文字里避开这些文件名 |

---

## 六、图标资源

程序图标与托盘图标都由脚本生成，**不要手工改二进制**。

```bash
python scripts/make_icons.py       # 生成 assets/icon.ico 与托盘 PNG
python scripts/preview_icons.py    # 生成各尺寸/明暗背景的观感对照图
```

| 文件 | 用途 | 谁消费 |
|---|---|---|
| `assets/icon.ico` | 应用程序图标（任务栏 / Alt+Tab / 资源管理器） | `crates/modular-clipboard/build.rs` 用 `rc.exe` 缝进 exe |
| `assets/icon-256.png` | 大尺寸预览 | 只用来看 |
| `crates/modular-clipboard-platform/assets/tray.png` | 托盘图标（32×32） | `platform::tray` 用 `include_bytes!` 内嵌 |
| `assets/source/icon-ai-source.png` | AI 生成的设计原图 | 改设计时对着它改几何参数 |

### 为什么要脚本，不直接放一张 PNG

图标要在三个尺寸段各自成立：**16px（托盘）**要减元素加粗笔画、
**32~48px（任务栏）**要能看出是剪贴板、**128~256px（资源管理器）**
可以带细节。同一张图缩放做不到——大图缩到 16px 会把文本线糊成灰块。
所以脚本按同一套几何为不同尺寸分别绘制。

### 换图标时

1. 改 `scripts/make_icons.py` 里的几何/配色（或换 AI 原图后重新描一遍几何）；
2. 重跑两个脚本；
3. `cargo test -p modular-clipboard` —— `tests/icon_resource.rs` 会用
   Win32 的 `ExtractIconExW` 数一遍 exe 里的图标资源，防止
   `build.rs` 静默失效（那时资源管理器里会是一张白纸）。

### 图标没了怎么查

`build.rs` 找不到 `rc.exe` 时**只警告不报错**（构建机可能没装 SDK）。
症状是资源管理器/任务栏显示白纸。先看构建日志里有没有
`cargo:warning=... rc.exe ...`，再确认 `assets/icon.ico` 存在。
