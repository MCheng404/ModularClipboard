# 第三方资源归因

本文件记录项目内所有**非自研**的第三方资源及其许可。
新增依赖字体 / 图标 / 数据集时**必须**在此登记——这是许可要求，不是可选项。

---

## 图标字体

### Bootstrap Icons

| 项目 | 内容 |
|---|---|
| **上游项目** | [Bootstrap Icons](https://github.com/twbs/icons) |
| **版本** | 1.13.1 |
| **许可** | MIT |
| **上游版权** | The Bootstrap Authors |
| **本仓库文件** | `crates/modular-clipboard-ui/assets/modular-clipboard-icons.ttf` |
| **是否子集化** | **是**（见下） |

**上游许可原文**（MIT，节选自`LICENSE` in twbs/icons）：

```
The MIT License (MIT)

Copyright (c) 2019-2024 The Bootstrap Authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

#### 子集化说明（本仓库做过的加工）

本仓库**没有**直接分发上游的完整字体文件，而是自行生成了一个子集。

- **生成方式**：fontello（`fontello.com`）导出 + 按用到的图标重映射码位。
  字体内部 `name` 表记录了这一点（family = `bootstrap-icons`，
  vendor URL = `http://fontello.com`）。
- **保留的字形**：18 个（对应 `icons.rs` 里的 18 个 `Icon` 变体）。
  上游完整字体约 130KB，本子集 **4144 字节**。
- **码位重映射**：上游码位被打散重排到 PUA`U+E000..=U+E011`。
  映射表见 `crates/modular-clipboard-ui/src/icons.rs` 的
  [`Icon::codepoint`]，每行注释标注了对应的上游图标名
  （如 `U+E001 // pin-angle-fill`）。
- **度量保留**：`head.unitsPerEm = 300`，`OS/2` typo 度量
  ascender=300 / descender=0 / lineGap=27。**注意**：epaint 读OS/2
  而非 `hhea`，`icons.rs` 的 `BASELINE_FACTOR` 正是基于这一点标定的。

#### 本仓库对子集字体做的一处修复（`hmtx.lsb`）

字体ello 导出的这份子集里，`hmtx` 表的 `leftSideBearing` **全部写成了 0**
（个别 −4），而 `glyf` 里字形轮廓的真实 `xMin` 是 37/ 94/ 75 /……
两者矛盾。后果是渲染器读到的 bearing为 0，`Drag`(grip-vertical，
真实 bearing 19.65px) 被贴到笔尖上，在 64px 框里**横向偏左 21px（33%）**；
`Close` 偏 7.7px、`Collapse`/`Expand` 各偏 15.7px。受影响的共 13 个字形。

已按 `lsb == glyf.xMin` 修正 `hmtx`（13 个条目，字节数不变、文件仍4144
字节；`glyf`/`cmap`/轮廓等一概未动）。修复后 `Drag` 的横向偏差降到
-1.88px —— 那不再是错误，而是 `grip-vertical` 这个图标**自身**的设计
不对称（竖排三列点阵本身就偏左 9 个字体单位）。

⚠️ **重新生成字体时必须重做这件事**：fontello 的导出结果不可直接用，
否则上面那13 个图标会重新横向错位。
回归判据见 `crates/modular-clipboard-ui/src/icons.rs` 的
`glyph_ink_is_centred_horizontally`（及配套的变异测试
`horizontal_centering_would_catch_a_broken_lsb`）。

#### 为什么不用其他图标集

调研阶段评估过Feather / Lucide / Phosphor，最终选 Bootstrap Icons：

- **许可简单**：三者都是 MIT / ISC，但 Bootstrap Icons 的
  「MIT + 保留声明」条款最直接，无附加署名要求。
- **字形完整性**：Bootstrap Icons 同时提供描边（`-fill` 后缀缺失时）与
  实心（`-fill`）两套变体。本项目 `Icon::Pinned` / `Unpinned`、
  `Icon::Lock` / `Icon::Unlock` 需要「同语义、实心/描边两态」的区分，
  Bootstrap Icons 原生就成对提供，无需自己拼装。
- ⚠️ **明确排除 Remix Icon**：自 2026-01-25 起 Remix Icon 采用自定义的
  "Remix Icon License v1.0"，**禁止用作竞品图标库**。本项目不使用，
  且后续引入图标集时也不得引入。

---

## Rust 依赖

第三方 crate 的许可随 `Cargo.lock` 锁定，不逐一在此誊抄。
审计入口：

```bash
cargo install cargo-about   # 或 cargo-deny
cargo about generate about.hbs
```

当前依赖中与本文件相关的间接依赖字体为 `ab_glyph`（MIT），
由 `egui` / `epaint` 引入。