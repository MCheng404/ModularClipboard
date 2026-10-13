# -*- coding: utf-8 -*-
"""通过向 daemon 托盘 message-only 窗口投递 WM_HOTKEY(id=3054)，
精确模拟用户按 Win+V 的**完整真实链路**：
托盘线程收 WM_HOTKEY -> emit Hotkey -> daemon 主循环 t.poll()
-> ui_window_visible + hide_ui_window(PostMessage) -> UI 隐藏/显示。

比 SendInput 可靠（不受前台/系统保留组合限制），可高频重复，
正好能压「daemon 阻塞 ShowWindow 撞上 UI 在 ReadFile」的竞态。
判据：每次热键后窗口可见性必须在时限内翻转，且两进程持续响应。"""
import ctypes
import ctypes.wintypes as wt
import time
import sys
import os
import datetime

user32 = ctypes.windll.user32

TITLE = "ModularClipboard"
TRAY_CLASS = "ModularClipboardTrayWindow"
WM_HOTKEY = 0x0312
WM_NULL = 0x0000
HOTKEY_TOGGLE_ID = 0x0BEE
SMTO_ABORTIFHUNG = 0x0002

user32.FindWindowW.restype = wt.HWND
user32.FindWindowW.argtypes = [wt.LPCWSTR, wt.LPCWSTR]
user32.FindWindowExW.restype = wt.HWND
user32.FindWindowExW.argtypes = [wt.HWND, wt.HWND, wt.LPCWSTR, wt.LPCWSTR]
user32.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
user32.PostMessageW.restype = wt.BOOL
user32.IsWindowVisible.argtypes = [wt.HWND]
user32.IsWindowVisible.restype = wt.BOOL
user32.SendMessageTimeoutW.argtypes = [
    wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM, wt.UINT, wt.UINT,
    ctypes.POINTER(ctypes.c_ulong)]
user32.SendMessageTimeoutW.restype = wt.BOOL


def heartbeat_age(log_path):
    if not os.path.exists(log_path):
        return None
    with open(log_path, "rb") as f:
        f.seek(0, os.SEEK_END)
        size = f.tell()
        f.seek(max(0, size - 8192))
        tail = f.read().decode("utf-8", errors="replace")
    last = None
    for line in tail.splitlines():
        if "常驻主循环心跳" in line:
            last = line
    if not last:
        return None
    ts = last.split()[0]
    try:
        dt = datetime.datetime.fromisoformat(ts.replace("Z", "+00:00"))
        return (datetime.datetime.now(datetime.timezone.utc) - dt).total_seconds()
    except Exception:
        return None


def main():
    app_log = sys.argv[1] if len(sys.argv) > 1 else r"D:\modular-clipboard\verify-data\app.log"
    main_hwnd = user32.FindWindowW(None, TITLE)
    if not main_hwnd:
        print("FAIL: 没找到主窗口")
        return 1
    # message-only 窗口挂在 HWND_MESSAGE(-3) 下，FindWindowW 找不到，要用 FindWindowExW。
    tray_hwnd = user32.FindWindowExW(ctypes.c_void_p(0xFFFFFFFFFFFFFFFD), None,
                                     TRAY_CLASS, None)
    if not tray_hwnd:
        print("FAIL: 没找到托盘 message-only 窗口")
        return 1
    print(f"主窗口={main_hwnd} 托盘窗口={tray_hwnd}")

    def visible():
        return bool(user32.IsWindowVisible(main_hwnd))

    def respond(timeout_ms=1500):
        res = ctypes.c_ulong(0)
        return bool(user32.SendMessageTimeoutW(
            main_hwnd, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, timeout_ms, ctypes.byref(res)))

    def wait_flip(before, deadline_s=2.5):
        t0 = time.time()
        while time.time() - t0 < deadline_s:
            if visible() != before:
                return True, int((time.time() - t0) * 1000)
            time.sleep(0.005)
        return False, int((time.time() - t0) * 1000)

    failures = []
    N = 12
    for i in range(N):
        before = visible()
        if not user32.PostMessageW(tray_hwnd, WM_HOTKEY, HOTKEY_TOGGLE_ID, 0):
            failures.append(f"#{i} PostMessage WM_HOTKEY 失败")
            break
        flipped, ms = wait_flip(before)
        if not flipped:
            failures.append(f"#{i} 热键后 {ms}ms 可见性未翻转（{'隐藏' if before else '显示'}卡死）")
            break
        if not respond():
            failures.append(f"#{i} 热键后 UI 线程无响应")
            break
        age = heartbeat_age(app_log)
        print(f"#{i:2d} {'可见->隐藏' if before else '隐藏->可见'} 翻转耗时={ms}ms "
              f"UI响应=是 心跳年龄={age:.2f}s")

    # 高频轰炸：连续 20 次热键，间隔极短，制造最大竞态压力。
    print("\n开始 20 次高频热键轰炸...")
    for _ in range(20):
        user32.PostMessageW(tray_hwnd, WM_HOTKEY, HOTKEY_TOGGLE_ID, 0)
        time.sleep(0.05)
    time.sleep(1.5)
    ui_ok = respond(2000)
    age = heartbeat_age(app_log)
    print(f"轰炸后：UI响应={ui_ok} 心跳年龄={age}")

    print("\n==== 结果 ====")
    if failures:
        for f in failures:
            print("FAIL:", f)
        return 1
    if not ui_ok:
        print("FAIL：轰炸后 UI 无响应")
        return 1
    if age is not None and age > 3.0:
        print(f"FAIL：daemon 主循环心跳停滞（{age:.1f}s）")
        return 1
    print(f"PASS：{N} 次真实热键链路翻转 + 20 次高频轰炸，窗口翻转正常、UI 响应、daemon 心跳推进")
    return 0


if __name__ == "__main__":
    sys.exit(main())
