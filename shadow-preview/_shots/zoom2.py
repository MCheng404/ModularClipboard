# -*- coding: utf-8 -*-
from PIL import Image
src = r"D:\modular-clipboard\shadow-preview\_shots\卡片阴影效果预览_desktop.jpg"
im = Image.open(src)
w, h = im.size
# 浅色面板：左半 x 0~0.5w。卡片区在 y 0.25h 起。
# 放大第一张卡片与第二张卡片的行间缝隙区域
x0, x1 = int(w*0.08), int(w*0.42)
y0, y1 = int(h*0.33), int(h*0.48)
im.crop((x0, y0, x1, y1)).resize(((x1-x0)*3, (y1-y0)*3), Image.LANCZOS).save(
    r"D:\modular-clipboard\shadow-preview\_shots\light_gap.jpg")
# 深色面板：右半
x0, x1 = int(w*0.58), int(w*0.92)
im.crop((x0, y0, x1, y1)).resize(((x1-x0)*3, (y1-y0)*3), Image.LANCZOS).save(
    r"D:\modular-clipboard\shadow-preview\_shots\dark_gap.jpg")
# 浅色最后一行底部（完整投影轮廓）
y0, y1 = int(h*0.62), int(h*0.78)
im.crop((int(w*0.08), y0, int(w*0.42), y1)).resize(((int(w*0.42)-int(w*0.08))*3, (y1-y0)*3), Image.LANCZOS).save(
    r"D:\modular-clipboard\shadow-preview\_shots\light_bottom.jpg")
# 深色最后一行底部
im.crop((int(w*0.58), y0, int(w*0.92), y1)).resize(((int(w*0.92)-int(w*0.58))*3, (y1-y0)*3), Image.LANCZOS).save(
    r"D:\modular-clipboard\shadow-preview\_shots\dark_bottom.jpg")
print("ok")
