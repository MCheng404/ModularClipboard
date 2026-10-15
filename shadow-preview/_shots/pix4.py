# -*- coding: utf-8 -*-
from PIL import Image
src = r"D:\modular-clipboard\shadow-preview\_shots\卡片阴影效果预览_desktop.jpg"
im = Image.open(src).convert("RGB")
w, h = im.size
x = int(w * 0.75)
print("== dark card1 top edge x=%d ==" % x)
for y in range(int(h*0.36), int(h*0.43), 1):
    p = im.getpixel((x, y))
    print(y, p)
