"""生成应用程序图标（.ico）与托盘图标（.png）。

# 为什么要有这个脚本

图标不是「画一次就完了」的东西：它要同时满足三个互相冲突的场景——

1. **任务栏 / Alt+Tab（32~48px）**：有细节、能看出是「剪贴板」；
2. **资源管理器大图标（128~256px）**：可以带上更精致的造型；
3. **托盘（16px，实际显示像素就这么大）**：多条细线会糊成一坨，
   必须**减少元素、加粗笔画**。

同一张图缩放是做不到的：大图缩到 16px，内部的文本线会糊成灰块。
所以所有尺寸**都按同一套几何重绘**（`draw_app_master` / `draw_tray_master`）。

AI 出图在本项目里的角色是**设计定型**（形状、配色、视觉重量），
成品由本脚本按该几何精确重绘。原因有两条，都很硬：

1. **白夹子与白底直接相连**。AI 出图是白底不透明的，而图标里的
   夹子也是白的，两者在像素上连成一片、没有任何边界——
   任何自动抠图都无法既保留夹子又抠掉背景（实测：按蓝色板身
   膨胀会把夹子一起吞掉，闭运算又会在夹子上方留一圈白）。
2. **小尺寸本来就要重画**。16px 下板身只有十几像素高，
   直接缩放会把三条线糊成灰块，必须减少元素、加粗笔画。

AI 原图存档在 `assets/source/icon-ai-source.png`，改设计时对着它改
`draw_app_master` 的几何参数即可。

# 用法

    python scripts/make_icons.py

产出：
    assets/icon.ico                                  多尺寸应用程序图标
    assets/icon-256.png                              大尺寸预览（透明）
    crates/modular-clipboard-platform/assets/tray.png  托盘图标（32×32）
    assets/preview.png                               各尺寸对照，人工确认用

# 抠图原理（AI 原图 → 透明）

AI 出图是**白底不透明**的。直接按「接近白色 → 透明」处理会连
板身内部的白色文本条一起抠掉。所以：

1. 用蓝色（板身）通道判定主体，做一次形态学闭运算把顶部夹子的
   凹口封住（否则夹子会被当成背景抠掉）；
2. 从四边 flood fill，得到「真正的外部」；
3. 外部用「与白底的差异」算 alpha（保留边缘抗锯齿），
   主体内部强制 alpha=255（保住白色文本条）；
4. 边缘像素做去白（unmatte），否则半透明边缘会泛白边。
"""

from __future__ import annotations

import struct
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
TRAY_ASSETS = ROOT / "crates" / "modular-clipboard-platform" / "assets"

# 主题蓝。与 platform/src/tray.rs 里原先手绘图标用的 #1F6FEB 一致，
# 也是 Windows 11 强调色系里最常见的那个蓝。
BLUE = (31, 111, 235, 255)
WHITE = (255, 255, 255, 255)

# 超采样倍数：所有形状先在 N 倍画布上画，再 LANCZOS 缩小。
# Pillow 的 rounded_rectangle 不做抗锯齿，靠这一步拿到干净的边缘。
SS = 4


def draw_app_master(size: int) -> Image.Image:
    """画「应用程序图标」的简化版（与 AI 主图同一套几何）。

    `size` 是超采样后的画布边长，几何按 1024 坐标系等比放大。
    """
    k = size / 1024.0
    im = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)

    def rr(x0, y0, x1, y1, r, color):
        d.rounded_rectangle(
            [x0 * k, y0 * k, x1 * k, y1 * k], radius=r * k, fill=color
        )

    # 板身
    rr(176, 216, 848, 880, 104, BLUE)
    # 顶部夹子：白色横条压在板身上缘；中间的蓝孔让它读起来像「夹子」
    rr(352, 152, 672, 268, 58, WHITE)
    rr(476, 186, 548, 240, 26, BLUE)
    # 三条文本线（第三条短，符合「一行没写完」的直觉）
    rr(336, 432, 800, 500, 34, WHITE)
    rr(336, 592, 800, 660, 34, WHITE)
    rr(336, 752, 688, 820, 34, WHITE)
    return im


def draw_tray_master(size: int) -> Image.Image:
    """画「托盘图标」：同一设计，但笔画更粗、线只有两条。

    16px 下板身只有十几像素高，三条线会挤在一起变成灰块。
    两条线 + 更粗的板身留白，是这个尺寸下的最优解。
    """
    k = size / 1024.0
    im = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)

    def rr(x0, y0, x1, y1, r, color):
        d.rounded_rectangle(
            [x0 * k, y0 * k, x1 * k, y1 * k], radius=r * k, fill=color
        )

    rr(112, 176, 912, 912, 140, BLUE)
    rr(368, 96, 656, 232, 64, WHITE)
    rr(476, 136, 548, 200, 28, BLUE)
    rr(256, 392, 768, 480, 44, WHITE)
    rr(256, 608, 768, 696, 44, WHITE)
    return im


def downscale(master: Image.Image, size: int) -> Image.Image:
    return master.resize((size, size), Image.LANCZOS)


# ---------------------------------------------------------------------------
# AI 原图抠白底
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# ICO 封装
# ---------------------------------------------------------------------------


def write_ico(path: Path, images: list[tuple[int, Image.Image]]) -> None:
    """手写 ICO 容器，每个条目直接放 PNG（Vista 起支持）。

    不用 `Image.save(..., sizes=...)`：那个只能从**同一张图**缩放，
    而本脚本的大尺寸来自 AI 原图、小尺寸来自重绘，必须逐条塞。
    """
    entries = []
    blobs = []
    offset = 6 + 16 * len(images)
    for size, im in images:
        buf = Path(f"/tmp/_ico_{size}.png")
        im.save(buf, format="PNG", optimize=True)
        data = buf.read_bytes()
        blobs.append(data)
        entries.append((size, len(data), offset))
        offset += len(data)

    out = bytearray()
    out += struct.pack("<HHH", 0, 1, len(images))
    for (size, length, off) in entries:
        dim = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, length, off)
    for data in blobs:
        out += data
    path.write_bytes(bytes(out))


def main() -> None:
    ASSETS.mkdir(exist_ok=True)
    TRAY_ASSETS.mkdir(parents=True, exist_ok=True)

    app_master = draw_app_master(2048)
    tray_master = draw_tray_master(2048)

    # 大尺寸也来自重绘：AI 原图的白夹子抠不出来（见模块文档），
    # 混用会让 48px 有夹子、64px 没夹子——同一程序两个图标，很怪。
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    entries: list[tuple[int, Image.Image]] = [
        (s, downscale(app_master, s)) for s in sizes
    ]
    write_ico(ASSETS / "icon.ico", entries)
    downscale(app_master, 256).save(ASSETS / "icon-256.png")
    print(f"assets/icon.ico 已生成：{len(entries)} 个尺寸")

    # ---- 托盘 ----
    tray32 = downscale(tray_master, 32)
    tray32.save(TRAY_ASSETS / "tray.png")
    downscale(tray_master, 16).save(TRAY_ASSETS / "tray-16.png")
    print("托盘图标已生成：tray.png(32) / tray-16.png(预览)")

    # ---- 对照预览 ----
    preview_sizes = [16, 24, 32, 48, 64, 128, 256]
    pad = 12
    width = sum(preview_sizes) + pad * (len(preview_sizes) + 1)
    height = max(preview_sizes) + pad * 2
    prev = Image.new("RGB", (width, height), (245, 246, 248))
    x = pad
    for s in preview_sizes:
        icon = downscale(app_master, s)
        prev.paste(icon, (x, pad + (max(preview_sizes) - s) // 2), icon)
        x += s + pad
    prev.save(ASSETS / "preview.png")
    print("对照预览已生成：assets/preview.png")


if __name__ == "__main__":
    main()
