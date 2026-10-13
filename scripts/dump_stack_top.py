# -*- coding: utf-8 -*-
"""打印每个线程栈顶（RSP 起 ~6KB）内的返回地址，严格按栈顺序，还原内层调用链。"""
import sys, struct
from minidump.minidumpfile import MinidumpFile

WANT = ("modular-clipboard", "user32", "win32u", "kernelbase", "kernel32",
        "ntdll", "dwmapi", "uxtheme", "vulkan")

def main(path, only_tids=None):
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
        if only_tids and t.ThreadId not in only_tids:
            continue
        ctx = t.ContextObject
        rsp = ctx.Rsp
        span = 0x80000
        lo = max(rsp - 0x800, t.Stack.StartOfMemoryRange)
        hi = min(rsp + span, t.Stack.StartOfMemoryRange + t.Stack.DataSize)
        try:
            data = r.read(lo, hi - lo)
        except Exception as e:
            print(f"tid={t.ThreadId} 读栈失败 {e}"); continue
        chain = []
        prev = None
        for off in range(0, len(data) - 8, 8):
            val = struct.unpack_from("<Q", data, off)[0]
            loc = locate(val)
            if not loc:
                continue
            name, delta = loc
            if not any(w in name for w in WANT):
                continue
            if prev == (name, delta >> 4):
                continue
            prev = (name, delta >> 4)
            chain.append((name, delta))
        if chain:
            print(f"\n--- tid={t.ThreadId} RIP附近链（{len(chain)} 帧）")
            for name, delta in chain[:120]:
                print(f"   {name}+0x{delta:x}")

if __name__ == "__main__":
    path = sys.argv[1]
    tids = [int(x) for x in sys.argv[2:]] if len(sys.argv) > 2 else None
    main(path, tids)
