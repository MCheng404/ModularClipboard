"""生成图标观感预览（一次性，不入库脚本）。"""
from PIL import Image, ImageDraw
from pathlib import Path

ROOT = Path("D:/modular-clipboard")
app = Image.open(ROOT / "assets/icon-256.png").convert("RGBA")
tray = Image.open(ROOT / "crates/modular-clipboard-platform/assets/tray.png").convert("RGBA")
tray16 = Image.open(ROOT / "crates/modular-clipboard-platform/assets/tray-16.png").convert("RGBA")

W, H = 900, 470
canvas = Image.new("RGB", (W, H), (245, 246, 248))
d = ImageDraw.Draw(canvas)


def zoom(im, k):
    return im.resize((im.width * k, im.height * k), Image.NEAREST)


# --- 场景 1：深色任务栏（Windows 11 深色主题）---
d.rectangle([0, 0, W, 96], fill=(32, 32, 32))
d.text((16, 10), "深色任务栏 · 实际大小(16px) → 放大 6x", fill=(190, 190, 190))
canvas.paste(tray16, (16, 40), tray16)
canvas.paste(tray16, (60, 40), tray16)
canvas.paste(tray16, (104, 40), tray16)
z = zoom(tray16, 6)
canvas.paste(z, (170, 20), z)
canvas.paste(app, (320, 32), app)  # 256 缩不缩都是 32? 用 32 尺寸
big32 = app.resize((32, 32), Image.LANCZOS)
canvas.paste(big32, (300, 32), big32)
z32 = zoom(tray, 3)
canvas.paste(z32, (400, 10), z32)
d.text((300, 72), "任务栏 32px（应用图标）", fill=(190, 190, 190))
d.text((610, 40), "托盘 32px 放大 3x", fill=(190, 190, 190))

# --- 场景 2：浅色背景（设置面板卡片）---
d.rectangle([0, 110, W, 250], fill=(255, 255, 255))
d.text((16, 120), "浅色背景 · 各尺寸阶梯", fill=(110, 110, 110))
x = 16
for s in [16, 20, 24, 32, 40, 48, 64, 128]:
    ic = app.resize((s, s), Image.LANCZOS)
    canvas.paste(ic, (x, 150 + (128 - s) // 2), ic)
    x += s + 16

# --- 场景 3：深色背景对比 ---
d.rectangle([0, 260, W, 400], fill=(30, 30, 30))
d.text((16, 270), "深色背景 · 各尺寸阶梯", fill=(190, 190, 190))
x = 16
for s in [16, 20, 24, 32, 40, 48, 64, 128]:
    ic = app.resize((s, s), Image.LANCZOS)
    canvas.paste(ic, (x, 300 + (128 - s) // 2), ic)
    x += s + 16

d.text((16, 415), "说明：16/20/24 为托盘与列表小图标，48 为任务栏，128/256 为资源管理器大图标。", fill=(110, 110, 110))
canvas.save(ROOT / "assets/preview-scenes.png")
print("ok -> assets/preview-scenes.png", canvas.size)
