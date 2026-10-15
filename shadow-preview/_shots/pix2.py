# -*- coding: utf-8 -*-
from PIL import Image
src = r"D:\modular-clipboard\shadow-preview\_shots\卡片阴影效果预览_desktop.jpg"
im = Image.open(src).convert("RGB")
w, h = im.size
# 深色面板 x=0.75w：检查第一张卡片顶部（高光）与卡片/背景过渡
x = int(w * 0.75)
print("== dark x=%d : card1 top area ==" % x)
for y in range(int(h*0.30), int(h*0.38), 2):
    p = im.getpixel((x, y))
    print(y, p)
# 浅色卡片顶部
x = int(w * 0.25)
print("== light x=%d : card1 top area ==" % x)
for y in range(int(h*0.30), int(h*0.38), 2):
    p = im.getpixel((x, y))
    print(y, p)
