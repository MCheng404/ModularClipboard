# -*- coding: utf-8 -*-
"""用 llvm-symbolizer 解析 modular-clipboard.exe 的 RVA 偏移 -> 函数/行号。
用法: python sym.py <dump文件> [线程数...]   # 直接从 dump 栈扫描
或:   python sym.py 0x1a7b383 0x...           # 解析给定 RVA
"""
import sys, subprocess, struct, pefile
from minidump.minidumpfile import MinidumpFile

SYM = r"D:\Program Files\Microsoft Visual Studio\18\Insiders\VC\Tools\Llvm\x64\bin\llvm-symbolizer.exe"
EXE = r"D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe"

def image_base():
    pe = pefile.PE(EXE, fast_load=True)
    return pe.OPTIONAL_HEADER.ImageBase

def sym_addrs(rvas):
    base = image_base()
    inp = "".join(hex(base + r) + "\n" for r in rvas)
    p = subprocess.run([SYM, "-e", EXE, "--inlines", "-f"],
                       input=inp, capture_output=True, text=True,
                       encoding="utf-8", errors="replace")
    return p.stdout

def dump_thread_rvas(path, tid):
    mf = MinidumpFile.parse(path)
    mods = [(m.baseaddress, m.baseaddress + m.size,
             m.name.split("\\")[-1].lower()) for m in mf.modules.modules]
    mods.sort()
    r = mf.get_reader()
    def loc(a):
        for lo, hi, n in mods:
            if lo <= a < hi:
                return n, a - lo
        return None
    out = []
    for t in mf.threads.threads:
        if t.ThreadId != tid:
            continue
        rsp = t.ContextObject.Rsp
        ss = t.Stack.StartOfMemoryRange
        data = r.read(max(rsp - 0x800, ss),
                      min(rsp + 0x60000, ss + t.Stack.DataSize) - max(rsp - 0x800, ss))
        base = max(rsp - 0x800, ss)
        prev = None
        for off in range(0, len(data) - 8, 8):
            val = struct.unpack_from("<Q", data, off)[0]
            l = loc(val)
            if not l or not l[0].startswith("modular-clipboard"):
                continue
            rva = l[1]
            if prev is not None and abs(rva - prev) < 16:
                continue
            prev = rva
            out.append((base + off, rva))
    return out

if __name__ == "__main__":
    args = sys.argv[1:]
    rvas = []
    if args and args[0].endswith(".dmp"):
        path = args[0]
        for tid in [int(x) for x in args[1:]]:
            for _, rva in dump_thread_rvas(path, tid):
                rvas.append(rva)
    else:
        rvas = [int(x, 16) for x in args]
    # 去重保序
    seen = set(); uniq = []
    for x in rvas:
        if x not in seen:
            seen.add(x); uniq.append(x)
    print(sym_addrs(uniq))
