#!/usr/bin/env python3
"""给截图验证灌入测试条目：混合类型 + 长短文本 + 置顶 + 图片/文件。

在**隔离数据目录**里操作（.shotdata/），不碰真实数据库。
用法：先让 app 用该 --data-dir 跑过一次（建好 schema），再跑本脚本。
"""
import os
import sqlite3
import struct
import sys
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA = os.path.join(ROOT, ".shotdata")
# CAUTION: `--data-dir` 模式下库与载荷**直接放在目录下**（不是 data/ 子目录）——
# 默认数据目录才是 %APPDATA%/modular-clipboard/data/history.db。
DB = os.path.join(DATA, "history.db")
BLOBS = os.path.join(DATA, "blobs")

NOW_MS = 1760000000000  # 固定基准，避免依赖墙钟


def png_bytes(w: int, h: int, rgb) -> bytes:
    """最小 PNG 编码器（无依赖）：纯色图。"""
    raw = b"".join(
        b"\x00" + bytes(rgb) * w for _ in range(h)
    )

    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


TEXTS = [
    "https://github.com/MCheng404/ModularClipboard",
    "Cargo.toml 里 rusqlite 的 bundled feature 需要 C 编译器，编译 sqlite3.c 会占掉整个构建的大头",
    "短文本",
    "登录服务器：ssh -p 2222 deploy@10.0.0.8 -i ~/.ssh/id_ed25519",
    "fn main() {\n    println!(\"hello\");\n}",
    "会议纪要 2026-10-17：\n1. 滚动条与卡片的间距问题已定位——floating 滚动条热区与行矩形右缘之间零呼吸空间；\n2. 动效方案：行悬停已有 110ms 补间，选中态、按下反馈、Toast 出入场待补；\n3. 下一步：测量与绘制宽度同源纪律要守住。",
    "SELECT id, kind, preview FROM items ORDER BY pinned DESC, created_at DESC LIMIT 50;",
    "咖啡因摄入过量提示：今天已是第四杯",
    "D:\\modular-clipboard\\crates\\modular-clipboard-ui\\src\\paint.rs",
    "宽度/尺寸口径的同源要求：测量宽度必须与绘制宽度逐位相同，否则折行数偏少、行高偏小、文字被裁",
    "TODO: 清理死代码",
    "egui 0.36 的 ScrollArea 没有 style() 方法，滚动条样式只能改全局 Style.spacing",
    "一句很短的话",
    "The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs.",
    "窗口标题单一数据源：ipc::UI_WINDOW_TITLE",
    "1",
    "联系人：张三 13800138000；李四 13900139000；王五 13700137000",
    "密码不要放在这里",
    "测试中文检索：剪贴板历史记录",
    "设置 → 通信 → MCP 服务总开关",
]

MARKDOWNS = [
    "# 标题\n\n**加粗** 与 *斜体*，还有 `行内代码`。\n\n- 列表项一\n- 列表项二\n\n> 引用：测量与绘制必须同源。",
    "## 二级标题\n\n```rust\nlet x = 42;\n```\n\n[链接](https://example.com)",
]

FILES = [
    "设计稿-主界面-v3.png",
    "剪贴板需求文档.docx",
]


def seed_text(cur, i, text, kind, pinned=False, extra=None):
    cur.execute(
        "INSERT INTO items (kind, hash, preview, blob_file, blob_len, img_w, img_h,"
        " source_app, created_at, pinned, use_count, last_used_at)"
        " VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        (
            kind,
            "seed-%s-%d" % (kind, i),
            text,
            None,
            0,
            None,
            None,
            extra or ("Code.exe" if kind == "text" else "msedge.exe"),
            NOW_MS - i * 370_000,
            1 if pinned else 0,
            0,
            None,
        ),
    )
    rid = cur.lastrowid
    if kind in ("text", "html", "markdown"):
        cur.execute("INSERT INTO items_fts(rowid, text) VALUES (?,?)", (rid, text))
        cur.execute("INSERT INTO items_fts_map(rowid, item_id) VALUES (?,?)", (rid, rid))


def main():
    if not os.path.exists(DB):
        sys.exit("找不到 %s——先让 app 用该 --data-dir 启动一次建 schema" % DB)
    os.makedirs(BLOBS, exist_ok=True)
    conn = sqlite3.connect(DB)
    cur = conn.cursor()
    cur.execute("DELETE FROM items")
    cur.execute("DELETE FROM items_fts_map")
    cur.execute("DELETE FROM sqlite_sequence WHERE name='items'")
    cur.execute("INSERT INTO items_fts(items_fts) VALUES('delete-all')")

    n = 0
    # 置顶条目（排最前）
    seed_text(cur, 0, "置顶条目：这是钉在列表最上面的一条，取消置顶点右上角图钉", "text", pinned=True, extra="explorer.exe")
    n += 1
    # 长短文本混合
    for i, t in enumerate(TEXTS, start=1):
        seed_text(cur, i, t, "text")
        n += 1
    # Markdown
    for j, m in enumerate(MARKDOWNS):
        seed_text(cur, 100 + j, m, "markdown", extra="Obsidian.exe")
        n += 1
    # 文件条目（单文件：preview 即文件名，按扩展名判图）
    for j, f in enumerate(FILES):
        seed_text(cur, 200 + j, f, "files", extra="explorer.exe")
        n += 1
    # 图片条目：真实 PNG blob + dims（走 ClipKind::Image 分支，load_payload_bytes）
    for j, (w, h, rgb) in enumerate([(512, 300, (70, 110, 200)), (300, 300, (200, 90, 90))]):
        name = "seed-img-%d.png" % j
        with open(os.path.join(BLOBS, name), "wb") as fp:
            fp.write(png_bytes(w, h, rgb))
        blob_len = os.path.getsize(os.path.join(BLOBS, name))
        cur.execute(
            "INSERT INTO items (kind, hash, preview, blob_file, blob_len, img_w, img_h,"
            " source_app, created_at, pinned, use_count, last_used_at)"
            " VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            (
                "image",
                "seed-img-%d" % j,
                "%dx%d 图片" % (w, h),
                name,
                blob_len,
                w,
                h,
                "mspaint.exe",
                NOW_MS - (300 + j) * 370_000,
                0,
                0,
                None,
            ),
        )
        n += 1
    conn.commit()
    conn.close()
    print("seeded %d items into %s" % (n, DB))


if __name__ == "__main__":
    main()
