"""纯 Python PNG 解码 + 像素统计（无第三方依赖）。

只支持 System.Drawing 导出的 8-bit RGB/RGBA、无隔行 PNG——够用即可。
用途：对比「无背景效果 / 半透明 / 磨砂」三张截图的像素分布，验证
逐像素半透明确实让桌面透出（而不是整窗淡出或完全不透明）。
"""
import sys
import zlib
import struct


def decode_png(path):
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "不是 PNG"
    pos = 8
    idat = b""
    w = h = bd = ct = None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        ctype = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        if ctype == b"IHDR":
            w, h, bd, ct, _, _, interlace = struct.unpack(">IIBBBBB", body)
            assert bd == 8 and interlace == 0, "只支持 8bit 无隔行"
        elif ctype == b"IDAT":
            idat += body
        elif ctype == b"IEND":
            break
        pos += 12 + length
    channels = {0: 1, 2: 3, 4: 2, 6: 4}[ct]
    raw = zlib.decompress(idat)
    stride = w * channels
    out = bytearray(w * h * channels)
    prev = bytearray(stride)
    p = 0
    for y in range(h):
        f = raw[p]
        p += 1
        line = bytearray(raw[p:p + stride])
        p += stride
        if f == 1:
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 0xFF
        elif f == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif f == 3:
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xFF
        elif f == 4:
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                b = prev[i]
                c = prev[i - channels] if i >= channels else 0
                pp = a + b - c
                pa, pb, pc = abs(pp - a), abs(pp - b), abs(pp - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 0xFF
        out[y * stride:(y + 1) * stride] = line
        prev = line
    return w, h, channels, out


def stats(path):
    w, h, ch, px = decode_png(path)
    hist = {}
    n = 0
    rsum = gsum = bsum = 0
    for y in range(0, h, 4):
        row = y * w * ch
        for x in range(0, w, 4):
            i = row + x * ch
            r, g, b = px[i], px[i + 1], px[i + 2]
            hist[(r, g, b)] = hist.get((r, g, b), 0) + 1
            rsum += r
            gsum += g
            bsum += b
            n += 1
    top = sorted(hist.items(), key=lambda kv: -kv[1])[:6]
    print(f"{path}")
    print(f"  尺寸 {w}x{h} 采样 {n} 点，均值 RGB=({rsum//n},{gsum//n},{bsum//n})")
    print(f"  不同颜色数 {len(hist)}")
    print("  高频色（前6）：")
    for (c, k) in top:
        print(f"    #{c[0]:02X}{c[1]:02X}{c[2]:02X}  {100.0*k/n:5.1f}%")
    return w, h, ch, px


def diff(a, b, path_a, path_b):
    w1, h1, c1, pa = stats(path_a)
    w2, h2, c2, pb = stats(path_b)
    n = 0
    tot = 0
    changed = 0
    for y in range(0, min(h1, h2), 4):
        ra = y * w1 * c1
        rb = y * w2 * c2
        for x in range(0, min(w1, w2), 4):
            ia = ra + x * c1
            ib = rb + x * c2
            d = abs(pa[ia] - pb[ib]) + abs(pa[ia + 1] - pb[ib + 1]) + abs(pa[ia + 2] - pb[ib + 2])
            tot += d
            if d > 24:
                changed += 1
            n += 1
    print(f"\n[{a}] vs [{b}]：平均通道差 {tot/n/3:.2f}，显著不同像素占比 {100.0*changed/n:.1f}%")


if __name__ == "__main__":
    args = sys.argv[1:]
    if len(args) == 4 and args[0] == "diff":
        diff(args[1], args[2], args[3], args[4])
    else:
        for p in args:
            stats(p)
