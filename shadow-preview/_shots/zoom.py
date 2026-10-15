# -*- coding: utf-8 -*-
from PIL import Image
src = r"D:\modular-clipboard\shadow-preview\_shots\卡片阴影效果预览_desktop.jpg"
im = Image.open(src)
w, h = im.size
# 浅色面板（左半）：x 0~0.5w, y 0.25h~0.75h
im.crop((0, int(h*0.25), int(w*0.5), int(h*0.75))).resize(
    (int(w*0.5*2), int(h*0.5*2)), Image.LANCZOS
).save(r"D:\modular-clipboard\shadow-preview\_shots\light_zoom.jpg")
# 深色面板（右半）
im.crop((int(w*0.5), int(h*0.25), w, int(h*0.75))).resize(
    (int(w*0.5*2), int(h*0.5*2)), Image.LANCZOS
).save(r"D:\modular-clipboard\shadow-preview\_shots\dark_zoom.jpg")
print("ok")
