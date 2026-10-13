# -*- coding: utf-8 -*-
"""对卡死进程生成 full memory minidump。"""
import ctypes, sys
from ctypes import wintypes

k32 = ctypes.windll.kernel32
dbg = ctypes.windll.dbghelp

PROCESS_QUERY_INFORMATION = 0x0400
PROCESS_VM_READ = 0x0010
GENERIC_READ = 0x80000000
GENERIC_WRITE = 0x40000000
CREATE_ALWAYS = 2
FILE_ATTRIBUTE_NORMAL = 0x80
# MiniDumpWithFullMemory(0x2) | MiniDumpWithThreadInfo(0x1000) | MiniDumpWithHandleData(0x4)
DUMP_TYPE = 0x1006

targets = [(int(a), b) for a, b in
           zip(sys.argv[1::2], sys.argv[2::2])]

for pid, path in targets:
    h = k32.OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, pid)
    if not h:
        print("OpenProcess 失败", pid, ctypes.get_last_error()); continue
    fh = k32.CreateFileW(path, GENERIC_READ | GENERIC_WRITE, 0, None,
                         CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, None)
    ok = dbg.MiniDumpWriteDump(h, pid, fh, DUMP_TYPE, None, None, None)
    err = ctypes.get_last_error()
    k32.CloseHandle(fh); k32.CloseHandle(h)
    print("pid", pid, "->", path, "ok", ok, "lasterr", err)
