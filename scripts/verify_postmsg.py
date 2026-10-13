# -*- coding: utf-8 -*-
"""验证 WM_HIDE_MAIN/WM_SHOW_MAIN 消息链路：高频 toggle 往返，
每步确认窗口可见性按预期变化、UI 线程持续响应（SendMessageTimeout）、
daemon 主循环心跳持续推进。任何一步在时限内未完成即 FAIL。"""
import ctypes
import ctypes.wintypes as wt
import time
import os
import sys

user32 = ctypes.windll.user32
kernel32 = ctypes.windll.kernel32

TITLE = "ModularClipboard"
WM_SHOW_MAIN = 0x8000 + 1
WM_HIDE_MAIN = 0x8000 + 2
WM_NULL = 0x0000
SMTO_ABORTIFHUNG = 0x0002

user32.FindWindowW.restype = wt.HWND
user32.FindWindowW.argtypes = [wt.LPCWSTR, wt.LPCWSTR]
user32.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
user32.PostMessageW.restype = wt.BOOL
user32.IsWindowVisible.argtypes = [wt.HWND]
user32.IsWindowVisible.restype = wt.BOOL
user32.GetWindowThreadProcessId.argtypes = [wt.HWND, ctypes.POINTER(wt.DWORD)]
user32.GetWindowThreadProcessId.restype = wt.DWORD
user32.SendMessageTimeoutW.argtypes = [
    wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM, wt.UINT, wt.UINT,
    ctypes.POINTER(ctypes.c_ulong)]
user32.SendMessageTimeoutW.restype = wt.BOOL


def find_window():
    return user32.FindWindowW(None, TITLE)


def respond(hwnd, timeout_ms=1500):
    """对 UI 线程发 WM_NULL 并等待；False=UI 线程不泵消息（卡死）。"""
    res = ctypes.c_ulong(0)
    ok = user32.SendMessageTimeoutW(
        hwnd, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, timeout_ms, ctypes.byref(res))
    return bool(ok)


def wait_visible(hwnd, want, deadline_s=2.0):
    """轮询直到可见性==want，返回 (是否达成, 耗时ms)。"""
    t0 = time.time()
    while time.time() - t0 < deadline_s:
        if bool(user32.IsWindowVisible(hwnd)) == want:
            return True, int((time.time() - t0) * 1000)
        time.sleep(0.01)
    return False, int((time.time() - t0) * 1000)


def heartbeat_age(log_path):
    """daemon 最近一条心跳距现在的秒数（主循环是否推进）。"""
    if not os.path.exists(log_path):
        return None
    age = None
    with open(log_path, "rb") as f:
        try:
            f.seek(0, os.SEEK_END)
            size = f.tell()
            f.seek(max(0, size - 8192))
        except Exception:
            pass
        tail = f.read().decode("utf-8", errors="replace")
    last = None
    for line in tail.splitlines():
        if "常驻主循环心跳" in line:
            last = line
    if last:
        ts = last.split()[0]
        import datetime
        try:
            dt = datetime.datetime.fromisoformat(ts.replace("Z", "+00:00"))
            now = datetime.datetime.now(datetime.timezone.utc)
            age = (now - dt).total_seconds()
        except Exception:
            pass
    return age


def main():
    log = sys.argv[1] if len(sys.argv) > 1 else r"D:\modular-clipboard\verify-data\app.log"
    hwnd = find_window()
    if not hwnd:
        print("FAIL: 没找到主窗口")
        return 1
    pid = wt.DWORD(0)
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    print(f"主窗口 hwnd={hwnd} UI_PID={pid.value}")

    failures = []
    N = 15
    for i in range(N):
        # 隐藏
        if not user32.PostMessageW(hwnd, WM_HIDE_MAIN, 0, 0):
            failures.append(f"#{i} PostMessage WM_HIDE 失败")
            break
        ok, ms = wait_visible(hwnd, False)
        if not ok:
            failures.append(f"#{i} 隐藏后 {ms}ms 仍可见（卡死？）")
            break
        if not respond(hwnd):
            failures.append(f"#{i} 隐藏后 UI 线程无响应（卡死）")
            break
        # 显示
        if not user32.PostMessageW(hwnd, WM_SHOW_MAIN, 0, 0):
            failures.append(f"#{i} PostMessage WM_SHOW 失败")
            break
        ok, ms = wait_visible(hwnd, True)
        if not ok:
            failures.append(f"#{i} 显示后 {ms}ms 仍不可见（卡死？）")
            break
        if not respond(hwnd):
            failures.append(f"#{i} 显示后 UI 线程无响应（卡死）")
            break
        age = heartbeat_age(log)
        hb = f"{age:.2f}s" if age is not None else "?"
        print(f"#{i:2d} hide->show OK  显示耗时={ms}ms  UI响应=是  daemon心跳年龄={hb}")

    # 最终：连续快速轰炸（不等待，压消息队列）
    for _ in range(30):
        user32.PostMessageW(hwnd, WM_HIDE_MAIN, 0, 0)
        time.sleep(0.02)
        user32.PostMessageW(hwnd, WM_SHOW_MAIN, 0, 0)
        time.sleep(0.02)
    time.sleep(1.0)
    final_ok, _ = wait_visible(hwnd, True)
    final_respond = respond(hwnd)
    age = heartbeat_age(log)
    print(f"\n轰炸后：可见={final_ok} UI响应={final_respond} 心跳年龄={age}")

    print("\n==== 结果 ====")
    if failures:
        for f in failures:
            print("FAIL:", f)
        print(f"FAIL：{len(failures)} 项")
        return 1
    if not final_ok or not final_respond:
        print("FAIL：轰炸后窗口未恢复/无响应")
        return 1
    if age is not None and age > 3.0:
        print(f"FAIL：daemon 心跳停滞（{age:.1f}s）")
        return 1
    print(f"PASS：{N} 轮 hide/show 往返 + 30 次快速轰炸，窗口、UI 线程、daemon 心跳均正常")
    return 0


if __name__ == "__main__":
    sys.exit(main())
