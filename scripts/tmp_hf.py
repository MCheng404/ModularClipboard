"""临时验证：比较两张图的高频（相邻像素差）强度。

判据：棋盘基准的高频极强；若磨砂生效，窗口区域的高频应被显著抹平。
"""
import sys

sys.path.insert(0, "scripts")
from tmp_png_stats import decode_png  # noqa: E402


def high_freq(path):
    w, h, ch, px = decode_png(path)
    tot = 0
    n = 0
    mx = 0
    for y in range(0, h, 2):
        row = y * w * ch
        for x in range(0, w - 2, 2):
            i = row + x * ch
            j = i + 2 * ch
            d = abs(px[i] - px[j]) + abs(px[i + 1] - px[j + 1]) + abs(px[i + 2] - px[j + 2])
            tot += d
            mx = max(mx, d)
            n += 1
    return tot / max(n, 1) / 3.0, mx / 3.0


for p in sys.argv[1:]:
    avg, mx = high_freq(p)
    print(f"{p}\n  相邻像素平均差 = {avg:.2f}/255   最大差 = {mx:.0f}/255")
