# -*- coding: utf-8 -*-
"""验证热键隐藏主窗时连带收起设置窗（不再孤立留屏）。
前置：应用以 MC_SETTINGS=1 启动，主窗 + 设置窗均可见。
触发真实热键链路后，主窗应隐藏、设置窗应被销毁（FindWindow 找不到）。"""
import ctypes
import ctypes.wintypes as wt
import time
import os
import datetime

user32 = ctypes.windll.user32
MAIN = "ModularClipboard"
SETTINGS = "剪贴板设置"
TRAY_CLASS = "ModularClipboardTrayWindow"
WM_HOTKEY = 0x0312
WM_NULL = 0x0000
TOGGLE_ID = 0x0BEE
SMTO_ABORTIFHUNG = 0x0002

user32.FindWindowW.restype = wt.HWND
user32.FindWindowW.argtypes = [wt.LPCWSTR, wt.LPCWSTR]
user32.FindWindowExW.restype = wt.HWND
user32.FindWindowExW.argtypes = [wt.HWND, wt.HWND, wt.LPCWSTR, wt.LPCWSTR]
user32.PostMessageW.argtypes = [wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM]
user32.PostMessageW.restype = wt.BOOL
user32.IsWindowVisible.argtypes = [wt.HWND]
user32.IsWindowVisible.restype = wt.BOOL
user32.IsWindow.argtypes = [wt.HWND]
user32.IsWindow.restype = wt.BOOL
user32.SendMessageTimeoutW.argtypes = [
    wt.HWND, wt.UINT, wt.WPARAM, wt.LPARAM, wt.UINT, wt.UINT,
    ctypes.POINTER(ctypes.c_ulong)]
user32.SendMessageTimeoutW.restype = wt.BOOL


def find(t):
    return user32.FindWindowW(None, t)


def vis(h):
    return bool(h) and bool(user32.IsWindowVisible(h))


def main():
    app_log = r"D:\modular-clipboard\verify-data2\app.log"
    main_h = find(MAIN)
    set_h = find(SETTINGS)
    tray = user32.FindWindowExW(ctypes.c_void_p(0xFFFFFFFFFFFFFFFD), None, TRAY_CLASS, None)
    print(f"隐藏前：主窗 hwnd={main_h} visible={vis(main_h)}；"
          f"设置窗 hwnd={set_h} visible={vis(set_h) if set_h else '不存在'}")
    if not (main_h and vis(main_h)):
        print("FAIL: 前置不满足，主窗应可见")
        return 1
    if not (set_h and vis(set_h)):
        print("FAIL: 前置不满足，设置窗应可见（MC_SETTINGS=1）")
        return 1

    # 触发真实热键 toggle：主窗可见 -> 隐藏。
    user32.PostMessageW(tray, WM_HOTKEY, TOGGLE_ID, 0)

    # 等主窗隐藏。
    t0 = time.time()
    while time.time() - t0 < 2.5:
        if not vis(main_h):
            break
        time.sleep(0.01)
    main_hidden = not vis(main_h)
    print(f"主窗隐藏：{main_hidden}（{int((time.time()-t0)*1000)}ms）")

    # 设置窗：给它一点时间被 drive_attach_windows 销毁。
    time.sleep(0.8)
    set_h2 = find(SETTINGS)
    set_gone = not bool(set_h2) or not bool(user32.IsWindow(set_h2))
    set_vis = vis(set_h2) if set_h2 else False
    print(f"设置窗：hwnd={set_h2} 是否仍存在={bool(set_h2)} 是否仍可见={set_vis}")

    # 再唤起主窗，确认主窗能回来（设置窗保持收起，符合预期）。
    user32.PostMessageW(tray, WM_HOTKEY, TOGGLE_ID, 0)
    t0 = time.time()
    while time.time() - t0 < 2.5:
        if vis(main_h):
            break
        time.sleep(0.01)
    main_back = vis(main_h)
    print(f"再按热键后主窗恢复：{main_back}（{int((time.time()-t0)*1000)}ms）")

    # UI 响应。
    res = ctypes.c_ulong(0)
    ui_resp = bool(user32.SendMessageTimeoutW(
        main_h, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, 1500, ctypes.byref(res)))

    # 心跳。
    age = None
    if os.path.exists(app_log):
        with open(app_log, "rb") as f:
            f.seek(0, os.SEEK_END)
            s = f.tell(); f.seek(max(0, s - 8192))
            tail = f.read().decode("utf-8", "replace")
        last = [l for l in tail.splitlines() if "常驻主循环心跳" in l]
        if last:
            dt = datetime.datetime.fromisoformat(last[-1].split()[0].replace("Z", "+00:00"))
            age = (datetime.datetime.now(datetime.timezone.utc) - dt).total_seconds()
    print(f"UI 响应={ui_resp} 心跳年龄={age}")

    print("\n==== 结果 ====")
    ok = main_hidden and set_gone and main_back and ui_resp and (age is None or age < 3)
    print("PASS：热键隐藏后主窗隐藏、设置窗连带销毁、再唤回主窗正常、UI 响应、心跳推进"
          if ok else "FAIL")
    if not main_hidden:
        print(" - 主窗未隐藏")
    if not set_gone:
        print(" - 设置窗未连带收起")
    if not main_back:
        print(" - 主窗未能再唤回")
    if not ui_resp:
        print(" - UI 无响应")
    return 0 if ok else 1


if __name__ == "__main__":
    import sys
    sys.exit(main())
