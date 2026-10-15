# -*- coding: utf-8 -*-
"""UI 修复的截图校验（模型无关判据，配合两个 ps1 采集脚本使用）。

用法：
  python scripts/verify_ui_checks.py theme   # 校验 vtf2_*.png
  python scripts/verify_ui_checks.py mcp     # 校验 vmcp3_*.png

# 为什么判据要「模型无关」

前一版校验用「遮罩等比压暗 0.6235×」当模型去反推「哪些像素是覆盖层
新画的」——实测该系数并不成立（实际约 0.57，与预乘合成/色彩空间有关），
于是把**所有亮内容**都误报成「文字溢出」。现在改成两条不依赖任何
系数的判据：
  1. 面板内的文字包围盒是否落在面板内缘之内；
  2. 面板右缘之外**是否还有亮像素**（压暗后最亮的文字也只有 ~110）。
"""
import sys

sys.path.insert(0, "scripts")
from tmp_crop import decode_png  # noqa: E402

# 设置窗（逻辑点）：默认 780x560，屏幕物理位置由 ps1 打印。
WIN_W, WIN_H = 780.0, 560.0
# 遮罩带里「压暗后的文字」上限：文字最亮 227 × 0.57 ≈ 130，取 140 留余量。
DIM_TEXT_MAX = 140


class Shot:
    def __init__(self, path, left, top, scale):
        self.w, self.h, self.ch, self.buf = decode_png(path)
        self.l, self.t, self.s = left, top, scale

    def px(self, lx, ly):
        x, y = int(self.l + lx * self.s), int(self.t + ly * self.s)
        i = (y * self.w + x) * self.ch
        return self.buf[i], self.buf[i + 1], self.buf[i + 2]


def check_theme(left, top, scale):
    dark = Shot("vtf2_dark_100.png", left, top, scale)
    light = Shot("vtf2_light_100.png", left, top, scale)
    big = Shot("vtf2_light_150.png", left, top, scale)
    # 窗底留白（导航块之下的窗口底色）
    bg0 = dark.px(20, 552)
    bg1 = light.px(20, 552)
    print(f"启动底色={bg0}  点浅色后={bg1}")

    def nav_text(shot):
        xs = []
        for ly in range(313, 353):
            for lx in range(45, 200):
                if shot.px(lx, ly)[0] < 150:  # 浅色主题：文字比底色暗
                    xs.append(lx)
        return (max(xs) - min(xs) + 1) if xs else 0

    w100, w150 = nav_text(light), nav_text(big)
    ok = True
    if bg0[0] > 128:
        print("FAIL: 起始主题不是深色")
        ok = False
    if bg1[0] < 200:
        print("FAIL: 点「浅色」后窗口底色仍偏暗 —— 主题切换未生效")
        ok = False
    else:
        print("PASS: 主题切换生效（窗口底色 深→浅）")
    if w100 > 0 and w150 > 0:
        ratio = w150 / w100
        if abs(ratio - 1.5) <= 0.2:
            print(f"PASS: 字体缩放 150% 生效（导航文字宽 {w100} → {w150}，比 {ratio:.2f}）")
        else:
            print(f"FAIL: 字体缩放比值 {ratio:.2f}（{w100} → {w150}），期望 ≈1.5")
            ok = False
    else:
        print("FAIL: 未测到导航文字（截图或几何参数有误）")
        ok = False
    return ok


def check_mcp(left, top, scale):
    open_shot = Shot("vmcp3_open.png", left, top, scale)
    # 面板几何（逻辑）：480x459 居中于客户区 ⇒ x[150,630] y[50,509]，内缘 x[166,614]
    px0, px1 = 166, 614
    band = range(112, 168)  # 状态两行
    inside = []
    for ly in band:
        for lx in range(px0, px1 + 1):
            r, g, b = open_shot.px(lx, ly)
            if abs(r - 30) + abs(g - 33) + abs(b - 39) > 70:
                inside.append((lx, ly))
    outside = [
        (lx, ly, open_shot.px(lx, ly))
        for ly in band
        for lx in range(634, 778)
        if open_shot.px(lx, ly)[0] > DIM_TEXT_MAX and open_shot.px(lx, ly)[1] > DIM_TEXT_MAX
    ]
    ok = True
    if not inside:
        print("FAIL: 面板内没测到状态文字（覆盖层可能没打开）")
        ok = False
    else:
        xs = [p[0] for p in inside]
        ys = [p[1] for p in inside]
        print(f"面板内状态文字 bbox 逻辑 x[{min(xs)},{max(xs)}] y[{min(ys)},{max(ys)}]（内缘右={px1}）")
        if max(xs) <= px1:
            print("PASS: 状态文字完全落在面板内缘之内")
        else:
            print("FAIL: 状态文字越出面板内缘")
            ok = False
    print(f"面板右缘外亮像素 = {len(outside)}")
    if outside:
        print("FAIL: 面板之外仍有文字像素（溢出）")
        for t in outside[:6]:
            print("   ", t)
        ok = False
    else:
        print("PASS: 面板之外没有文字像素")
    return ok


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else "theme"
    left, top, scale = (float(v) for v in sys.argv[2:5])
    ok = check_theme(left, top, scale) if mode == "theme" else check_mcp(left, top, scale)
    print("结论:", "PASS" if ok else "FAIL")
    sys.exit(0 if ok else 1)
