# -*- coding: utf-8 -*-
"""把三份审查发现清单合并成 docs/reviews.md。

原文件：
- docs/审查发现清单.md            （2026-10-08/09，4 Agent，46 条，已闭环）
- docs/审查发现清单-2026-10-12.md （2026-10-12，6 Agent + 基线）
- docs/审查发现清单-2026-10-23.md （2026-10-23，3 Agent + 主线手动 UI 审查）

合并策略：保留各自正文，去掉重复的 H1，外面包一层带日期的 H2，
顶部加总索引。原文件删除。
"""
import io
import os

SOURCES = [
    ("docs/审查发现清单.md", "第一轮：2026-10-08/09（4 Agent 并行）",
     "46 条发现，全部闭环。含窗口/渲染、测试质量自省、明确不做的项。"),
    ("docs/审查发现清单-2026-10-12.md", "第二轮：2026-10-12（6 Agent 并行）",
     "代码质量 CQ / 缺陷 DF / 架构 AR / 性能 PF / 安全 SEC / UI 六路，"
     "外加 clippy 基线与 603 项测试基线。原始报告全文曾见 docs/agents/*.md。"),
    ("docs/审查发现清单-2026-10-23.md", "第三轮：2026-10-23（3 Agent + 主线手动 UI 审查）",
     "安全面 8 条 + 数据链路 9 条 + gfx 渲染与资源 6 条。UI 逻辑一致性因 Agent "
     "通道持续 429 改为主线手动审查，结论并入本章。"),
]

HEADER = """# 代码审查发现清单（历轮合并）

> 本文档合并了项目全部三轮系统性代码审查。每轮独立编号、互不影响，
> 查阅时按日期定位。状态含义：`✅ 已修` / `⬜ 待修` / `⚠️ 待确认`。
>
> **不要**把不同轮次的编号混用——`DF-01`（第二轮）与 `DL-01`（第三轮）
> 是不同的东西。

## 索引

| 轮次 | 日期 | 规模 | 状态 |
|---|---|---|---|
"""

INDEX_ROWS = [
    "| 第一轮 | 2026-10-08/09 | 4 Agent，46 条 | 全部闭环 |\n",
    "| 第二轮 | 2026-10-12 | 6 Agent，分批修复 | 大部分闭环，未修项附理由 |\n",
    "| 第三轮 | 2026-10-23 | 3 Agent + 手动 UI 审查，23 条 | P0/P1 已修 8 条，其余列后续 |\n",
]

parts = [HEADER]
parts.extend(INDEX_ROWS)
parts.append("\n---\n")

for path, title, blurb in SOURCES:
    if not os.path.exists(path):
        continue
    text = io.open(path, encoding="utf-8", newline="").read()
    lines = text.split("\n")
    # 去掉首个 H1（若有）
    if lines and lines[0].startswith("# "):
        lines = lines[1:]
    body = "\n".join(lines).strip("\n")
    parts.append("\n## %s\n\n" % title)
    parts.append("> %s\n\n" % blurb)
    parts.append(body)
    parts.append("\n\n---\n")
    os.remove(path)
    print("merged + removed: %s" % path)

out = "".join(parts).rstrip("\n") + "\n"
io.open("docs/reviews.md", "w", encoding="utf-8", newline="").write(out)
print("wrote docs/reviews.md: %d lines" % out.count("\n"))
