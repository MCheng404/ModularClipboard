# -*- coding: utf-8 -*-
"""扫描每个线程栈内的返回地址，映射到模块+偏移，近似还原调用链。"""
import sys, struct
from minidump.minidumpfile import MinidumpFile

WANT = ("modular-clipboard", "user32", "win32u", "kernelbase", "kernel32",
        "ntdll", "dwmapi", "uxtheme")

def main(path):
    mf = MinidumpFile.parse(path)
    mods = []
    for m in mf.modules.modules:
        nm = m.name.split("\\")[-1].lower()
        mods.append((m.baseaddress, m.baseaddress + m.size, nm))
    mods.sort()
    r = mf.get_reader()

    def locate(addr):
        for lo, hi, name in mods:
            if lo <= addr < hi:
                return name, addr - lo
        return None

    print(f"===== {path} =====")
    for t in mf.threads.threads:
        ctx = t.ContextObject
        rsp = ctx.Rsp
        st_start = t.Stack.StartOfMemoryRange
        st_size = t.Stack.DataSize
        st_end = st_start + st_size
        try:
            data = r.read(st_start, st_size)
        except Exception as e:
            print(f"tid={t.ThreadId} 读栈失败 {e}")
            continue
        chain = []
        seen = set()
        for off in range(0, len(data) - 8, 8):
            val = struct.unpack_from("<Q", data, off)[0]
            loc = locate(val)
            if not loc:
                continue
            name, delta = loc
            if not any(w in name for w in WANT):
                continue
            key = (name, delta >> 4)
            if key in seen:
                continue
            seen.add(key)
            addr = st_start + off
            if addr < rsp:
                continue
            chain.append((addr, name, delta))
        # 只打印含主程序帧或 GUI 系统调用的线程
        interesting = any(n.startswith("modular-clipboard") for _, n, _ in chain)
        if interesting:
            print(f"\n--- tid={t.ThreadId} (栈内 {len(chain)} 帧，含主程序) RSP=0x{rsp:x}")
            for addr, name, delta in chain:
                print(f"   {name}+0x{delta:x}")

if __name__ == "__main__":
    for p in sys.argv[1:]:
        main(p)
