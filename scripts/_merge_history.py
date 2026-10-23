# -*- coding: utf-8 -*-
"""把 4 份历史类根级 md 合并成 docs/history.md。

原文件（均为「怎么走到今天」的历史记录，不再是现行规范）：
- PROGRESS.md        并行开发进度黑板（多 Agent 协作期的工作簿）
- 项目报告.md        项目报告（与 ARCHITECTURE.md 有重叠）
- README-归档说明.md 旧版 README（其文件名即说明已归档）
- UI-REWRITE.md      新 UI 架构设计稿（重构已完成，属设计决策记录）

现行规范在 README.md / ARCHITECTURE.md / BUILD.md / CHANGELOG.md，
本文件只作历史存档，不承担规范职责。
"""
import io
import os

SOURCES = [
    ("PROGRESS.md", "一、并行开发进度黑板（原 PROGRESS.md）",
     "多 Agent 并行开发期的工作簿：共享接口契约、文件所有权划分、"
     "实机验证基线。开发完成后它只是历史，但「接口契约为何是现在这样」"
     "仍能从这里查到。"),
    ("UI-REWRITE.md", "二、新 UI 架构设计稿（原 UI-REWRITE.md）",
     "「卡片是一等实体」这次重构的设计决策记录：为什么旧架构必然反复"
     "出问题、分配算法为何取代 5 档降级阶梯、卡片如何分离为子窗口。"),
    ("项目报告.md", "三、项目报告（原 项目报告.md）",
     "早期项目报告。技术栈/代码结构/运行环境/构建环境五节中，前两节"
     "已被 ARCHITECTURE.md 取代并保持更新，此处保留原始快照。"),
    ("README-归档说明.md", "四、旧版 README（原 README-归档说明.md）",
     "重构前的 README 全文存档。文件名自带「归档说明」，内容已被现行 "
     "README.md 取代。"),
]

HEADER = """# 开发史存档

> 本文件合并了 4 份历史类文档。它们记录「这个项目怎么走到今天」，
> **不承担现行规范职责**——现行规范见：
>
> | 想找什么 | 去这里 |
> |---|---|
> | 项目是什么、怎么构建运行 | `README.md` |
> | 架构与模块地图 | `ARCHITECTURE.md` |
> | 构建细节与环境要求 | `BUILD.md` |
> | 逐版本变更 | `CHANGELOG.md` |
> | DPI 三层量纲约定 | `docs/dpi-conventions.md` |
> | 历轮审查发现 | `docs/reviews.md` |
>
> 各节按「与现行的距离」从近到远排：并行开发黑板 → UI 重构设计稿 →
> 项目报告 → 旧 README。

"""

parts = [HEADER]
for path, title, blurb in SOURCES:
    if not os.path.exists(path):
        continue
    text = io.open(path, encoding="utf-8", newline="").read()
    lines = text.split("\n")
    if lines and lines[0].startswith("# "):
        lines = lines[1:]
    body = "\n".join(lines).strip("\n")
    parts.append("\n## %s\n\n" % title)
    parts.append("> %s\n\n" % blurb)
    parts.append(body)
    parts.append("\n")
    os.remove(path)
    print("merged + removed: %s" % path)

out = "".join(parts).rstrip("\n") + "\n"
io.open("docs/history.md", "w", encoding="utf-8", newline="").write(out)
print("wrote docs/history.md: %d lines" % out.count("\n"))
