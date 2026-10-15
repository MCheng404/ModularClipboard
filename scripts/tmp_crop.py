# -*- coding: utf-8 -*-
"""裁剪截图（纯 Python PNG 解码 + 重编码，无第三方依赖）。"""
import struct
import sys
import zlib


def decode_png(path):
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "不是 PNG"
    pos = 8
    idat = b""
    w = h = ct = None
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
    return w, h, channels, bytes(out)


def encode_png(path, w, h, channels, buf, scale=1):
    ow, oh = w * scale, h * scale
    raw = bytearray()
    for y in range(oh):
        raw.append(0)  # filter none
        sy = y // scale
        row = buf[sy * w * channels:(sy + 1) * w * channels]
        for x in range(ow):
            sx = x // scale
            raw += row[sx * channels:(sx + 1) * channels]

    def chunk(tag, payload):
        c = struct.pack(">I", len(payload)) + tag + payload
        return c + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", ow, oh, 8, {1: 0, 2: 2, 4: 4}[channels], 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    open(path, "wb").write(png)


if __name__ == "__main__":
    src, dst = sys.argv[1], sys.argv[2]
    x0, y0, x1, y1 = (int(v) for v in sys.argv[3:7])
    scale = int(sys.argv[7]) if len(sys.argv) > 7 else 1
    w, h, ch, buf = decode_png(src)
    x1, y1 = min(x1, w), min(y1, h)
    cw, chh = x1 - x0, y1 - y0
    crop = bytearray(cw * chh * ch)
    for y in range(chh):
        crop[y * cw * ch:(y + 1) * cw * ch] = buf[(y + y0) * w * ch + x0 * ch:(y + y0) * w * ch + x1 * ch]
    encode_png(dst, cw, chh, ch, bytes(crop), scale)
    print(f"{dst}: {cw}x{chh} x{scale}")
