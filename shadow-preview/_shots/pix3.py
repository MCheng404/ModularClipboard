# -*- coding: utf-8 -*-
from PIL import Image
src = r"D:\modular-clipboard\shadow-preview\_shots\卡片阴影效果预览_desktop.jpg"
im = Image.open(src).convert("RGB")
w, h = im.size
# 深色面板卡片1顶边附近：x=0.75w, y=0.37h 上下
x = int(w * 0.75)
print("== dark card1 top edge x=%d ==" % x)
for y in range(int(h*0.36), int(h*0.43), 1):
    p = im.getpixel((x, y))
    print(y, p)
