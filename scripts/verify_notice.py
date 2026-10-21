# -*- coding: utf-8 -*-
"""通知呈现实机验证（2026-10-20 轮次）：校验端。

配合 verify_notice.ps1 使用（它负责驱动应用并截图 + 记 manifest）。

判据**模型无关**——不猜「文字长什么样」，只查几何事实：
  1. 通知框存在：盒色（surface_raised）的**最长连续行 run** ≥ 60px
     （行卡片与盒同色，但盒是实心矩形，行内有文字打断；取最长 run
     不会误命中——实测行底色的 run 被文字切成碎片）；
  2. 框的几何合理：宽 100~300pt、高 24~130pt、水平居中；
  3. **文字装在框内**：差分出的「新增亮像素」（toast 文字）包围盒
     必须落在框包围盒内（2pt 容差）。旧 bug（no-wrap 重排）会让文字
     行宽超过框宽 ⇒ 文字包围盒超出框 ⇒ 这条断言当场红。

# 两个实机踩出来的坑（2026-10-21）

1. **亮度阈值是 0..255 不是 0..1**：第一版阈值写 `lum > 0.45`，
   解码出来是 0..255 ⇒ 全屏皆「亮」，差分 6.7 万像素全是文字。
2. **差分必须配 before 图**：盒底与行卡片同为 surface_raised，纯色
   检测分不出「盒」和「行」；而 toast 文字是**新增**的亮像素，
   before/after 差分 + 亮度阈值能把它从行文字里择出来。
"""
import json
import os
import struct
import sys
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# surface_raised（深色基底重调后的值，#2F3540）。
BOX_RGB = (47, 53, 64)
BOX_TOL = 10
# toast 文字用 pal.text（#E3E6E8，lum≈228）；行的次要文字 text_dim
# （#BCC2CB，lum≈197）在这个阈值之下，不会混进来。
TEXT_LUM = 210
# 差分阈值（三通道绝对差之和）。
DIFF_T = 24
# 盒色实心行的最短连续 run（px）：toast 盒宽 ≥100pt=150px，行内被
# 文字打断的 run 远小于此。
MIN_RUN = 60


# ---------------------------------------------------------------- PNG 解码
def decode_png(path):
    """返回 (width, height, channels, rows[bytes])，行优先 top-down。"""
    with open(path, "rb") as f:
        data = f.read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", f"{path} 不是 PNG"
    pos = 8
    width = height = bitd = ctype = None
    idat = bytearray()
    while pos < len(data):
        (ln,) = struct.unpack(">I", data[pos:pos + 4])
        typ = data[pos + 4:pos + 8]
        chunk = data[pos + 8:pos + 8 + ln]
        if typ == b"IHDR":
            width, height, bitd, ctype = struct.unpack(">IIBB", chunk[:10])
        elif typ == b"IDAT":
            idat += chunk
        elif typ == b"IEND":
            break
        pos += 12 + ln
    assert bitd == 8 and ctype in (2, 6), f"仅支持 8bit RGB/RGBA：bitd={bitd} ctype={ctype}"
    ch = 3 if ctype == 2 else 4
    raw = zlib.decompress(bytes(idat))
    stride = width * ch
    rows = []
    prev = bytearray(stride)
    p = 0
    for _ in range(height):
        f = raw[p]
        line = bytearray(raw[p + 1:p + 1 + stride])
        p += 1 + stride
        if f == 1:
            for i in range(ch, stride):
                line[i] = (line[i] + line[i - ch]) & 0xFF
        elif f == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif f == 3:
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xFF
        elif f == 4:
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                b = prev[i]
                c = prev[i - ch] if i >= ch else 0
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 0xFF
        rows.append(bytes(line))
        prev = line
    return width, height, ch, rows


def lum(r, g, b):
    """WCAG 相对亮度的 0..255 版本（阈值都按这个量纲写）。"""
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


class Shot:
    def __init__(self, name):
        self.name = name
        self.w, self.h, self.ch, self.rows = decode_png(os.path.join(ROOT, name))

    def px(self, x, y):
        row = self.rows[y]
        i = x * self.ch
        return row[i], row[i + 1], row[i + 2]

    def lumi(self, x, y):
        r, g, b = self.px(x, y)
        return lum(r, g, b)

    def is_box(self, x, y):
        r, g, b = self.px(x, y)
        return (abs(r - BOX_RGB[0]) < BOX_TOL
                and abs(g - BOX_RGB[1]) < BOX_TOL
                and abs(b - BOX_RGB[2]) < BOX_TOL)


# ---------------------------------------------------------------- 判据
def find_notice_box(shot, y_lo, y_hi):
    """盒色实心矩形的最长连续行 run 集合 → 盒 bbox（物理像素）。

    行卡片与盒同为 surface_raised，但行内有文字/徽标把盒色切成碎片；
    toast 盒是实心的（文字只在中间一小条），逐行取最长 run 不会误命中。
    """
    runs = []  # (y, x0, x1)
    for y in range(y_lo, y_hi):
        best_start = cur_start = 0
        best = cur = 0
        for x in range(shot.w):
            if shot.is_box(x, y):
                if cur == 0:
                    cur_start = x
                cur += 1
                if cur > best:
                    best, best_start = cur, cur_start
            else:
                cur = 0
        if best >= MIN_RUN:
            runs.append((y, best_start, best_start + best))
    if not runs:
        return None
    x0 = min(r[1] for r in runs)
    x1 = max(r[2] for r in runs)
    y0 = min(r[0] for r in runs)
    y1 = max(r[0] for r in runs)
    return x0, y0, x1, y1


def diff_bright_text(before, after, y_lo, y_hi):
    """before/after 差分中「变成亮像素」的点 = 本帧新增的文字。

    toast 文字是新增的（行文字在 before 里就有）；盒是同色覆盖不产生
    亮差分；选中/悬停的底色变化亮度都低于阈值，混不进来。
    """
    pts = []
    for y in range(y_lo, y_hi):
        for x in range(after.w):
            br, bg, bb = before.px(x, y)
            ar, ag, ab = after.px(x, y)
            if abs(ar - br) + abs(ag - bg) + abs(ab - bb) < DIFF_T:
                continue
            if lum(ar, ag, ab) > TEXT_LUM:
                pts.append((x, y))
    return pts


def check_notice(before_name, after_name, scale, label):
    """toast 的三条判据：存在 + 几何合理 + 文字装在框内。"""
    before = Shot(before_name)
    after = Shot(after_name)
    assert before.w == after.w and before.h == after.h, "两张截图尺寸不同"
    s = scale
    # toast 栖息带（逻辑 y 40..140 → 物理）。
    y_lo, y_hi = int(40 * s), min(after.h, int(140 * s))
    box = find_notice_box(after, y_lo, y_hi)
    assert box, f"{label}: 上部带状区没有出现通知框——通知没显示？"
    x0, y0, x1, y1 = box
    bw, bh = (x1 - x0) / s, (y1 - y0) / s
    cx = (x0 + x1) / 2 / s
    print(f"  [{label}] 框 {bw:.0f}x{bh:.0f}pt 中心 x={cx:.0f}")
    assert 100.0 <= bw <= 300.0, f"{label}: 框宽 {bw:.0f}pt 不在内容自适应区间 100~300"
    assert 24.0 <= bh <= 130.0, f"{label}: 框高 {bh:.0f}pt 不合理（应随行数自适应）"
    assert abs(cx - 240.0) < 24.0, (
        f"{label}: 框中心 x={cx:.0f} 未居中于窗口中线 240（480 逻辑宽布局）"
    )
    # 文字（差分亮像素）必须装在框内。
    text = diff_bright_text(before, after, y_lo, y_hi)
    assert text, f"{label}: 框内没有新增文字像素"
    tx0 = min(p[0] for p in text)
    tx1 = max(p[0] for p in text)
    ty0 = min(p[1] for p in text)
    ty1 = max(p[1] for p in text)
    tol = 2 * s
    assert tx0 >= x0 - tol and tx1 <= x1 + tol, (
        f"{label}: 文字横向越出通知框——文字 x[{tx0},{tx1}] 框 x[{x0},{x1}]"
        "（2026-10-20 修的 no-wrap 溢出 bug 正是这个症状）"
    )
    assert ty0 >= y0 - tol and ty1 <= y1 + tol, (
        f"{label}: 文字纵向越出通知框——文字 y[{ty0},{ty1}] 框 y[{y0},{y1}]"
    )
    print(f"  [{label}] 文字 {len(text)}px 装在框内 OK")


def check_notice_popup(name, scale, label):
    """弹窗通知：整窗 = 投影边距 + 面板。盒色定位面板，亮像素是文字。"""
    shot = Shot(name)
    s = scale
    box = find_notice_box(shot, 0, shot.h)
    assert box, f"{label}: 通知弹窗里没有面板"
    x0, y0, x1, y1 = box
    # 文字：盒内的亮像素（弹窗画布透明，盒外没有别的内容）。
    text = [(x, y) for y in range(y0, y1) for x in range(x0, x1)
            if shot.lumi(x, y) > TEXT_LUM]
    assert text, f"{label}: 通知弹窗里没有文字"
    tx0 = min(p[0] for p in text)
    tx1 = max(p[0] for p in text)
    ty0 = min(p[1] for p in text)
    ty1 = max(p[1] for p in text)
    tol = 2 * s
    assert tx0 >= x0 - tol and tx1 <= x1 + tol, (
        f"{label}: 弹窗文字横向越出面板 x[{tx0},{tx1}] vs [{x0},{x1}]"
    )
    assert ty0 >= y0 - tol and ty1 <= y1 + tol, (
        f"{label}: 弹窗文字纵向越出面板 y[{ty0},{ty1}] vs [{y0},{y1}]"
    )
    print(f"  [{label}] 弹窗面板 {((x1-x0)/s):.0f}x{((y1-y0)/s):.0f}pt，文字 {len(text)}px 装在面板内 OK")


def notice_absent(after_name, before_name, scale, label):
    """通知应已消失：既无盒（最长 run 判据），也无新增亮文字。"""
    after = Shot(after_name)
    before = Shot(before_name)
    s = scale
    y_lo, y_hi = int(40 * s), min(after.h, int(140 * s))
    box = find_notice_box(after, y_lo, y_hi)
    text = diff_bright_text(before, after, y_lo, y_hi)
    assert box is None and not text, (
        f"{label}: 通知仍在（盒={box} 新增文字={len(text)}px）——TTL 没生效？"
    )
    print(f"  [{label}] 通知已消失 OK")


def main():
    # utf-8-sig：容忍驱动端万一写了 BOM（PowerShell 的 UTF8 编码坑，
    # 项目修复记录 2026-10-14 记载过）。
    with open(os.path.join(ROOT, "vn_manifest.json"), encoding="utf-8-sig") as f:
        m = json.load(f)
    mode = m["mode"]
    scale = m["scale"]
    print(f"mode={mode} scale={scale}")
    ok = True

    def attempt(fn, *args):
        nonlocal ok
        try:
            fn(*args)
        except AssertionError as e:
            print("FAIL:", e)
            ok = False

    # ---- 链路 1：双击行复制 ⇒ 通知出现 ----
    # 判据随模式分叉：toast 模式查窗内 toast；弹窗模式查独立窗口
    # （主窗口**本就不画** toast，对它跑窗内判据必然红——第一版
    # 就在这里误报过）。
    if mode == "toast":
        attempt(check_notice, "vn_before.png", "vn_after.png", scale, "双击复制")

    # ---- 链路 2：模式分叉 ----
    if mode == "popup":
        if m.get("notice_window") != "present":
            print("FAIL: 弹窗模式下未出现「ModularClipboard 通知」窗口")
            ok = False
        else:
            print("  [双击复制] 通知弹窗已出现 OK")
            attempt(check_notice_popup, "vn_notice.png", scale, "通知弹窗")
        if m.get("notice_window_after_ttl") != "absent":
            print("FAIL: TTL 后通知弹窗仍在（应自行消失）")
            ok = False
        else:
            print("  [TTL] 通知弹窗已自行消失 OK")
    else:
        if m.get("notice_window") == "present":
            print("FAIL: toast 模式不应创建独立弹窗窗口")
            ok = False
        else:
            print("  [toast 模式] 未创建独立弹窗 OK")

    # ---- 链路 3：Enter 复制 ⇒ 通知出现 ----
    if mode == "toast":
        attempt(check_notice, "vn_before.png", "vn_enter.png", scale, "Enter 复制")
        # TTL：vn_gone 相对 vn_enter 不应再有新通知。
        attempt(notice_absent, "vn_gone.png", "vn_enter.png", scale, "TTL 后 toast 应消失")
    else:
        if m.get("notice_window_after_enter") != "present":
            print("FAIL: 弹窗模式下 Enter 复制后未出现通知弹窗")
            ok = False
        else:
            print("  [Enter] 通知弹窗已出现 OK")

    print("VERIFY NOTICE:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
