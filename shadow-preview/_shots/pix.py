# -*- coding: utf-8 -*-
from PIL import Image
src = r"D:\modular-clipboard\shadow-preview\_shots\卡片阴影效果预览_desktop.jpg"
im = Image.open(src).convert("RGB")
w, h = im.size
# 浅色面板：x 范围约 0.08w~0.42w；卡片第一张 y 约 0.26h~0.40h
# 取 x=0.25w 一列，从 y=0.25h 到 0.50h，每 4px 打印一次
x = int(w * 0.25)
ys = range(int(h*0.25), int(h*0.50), 4)
print("== light column x=%d ==" % x)
prev = None
for y in ys:
    p = im.getpixel((x, y))
    print(y, p)
# 深色面板 x=0.75w
x = int(w * 0.75)
print("== dark column x=%d ==" % x)
for y in ys:
    p = im.getpixel((x, y))
    print(y, p)
