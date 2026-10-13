# -*- coding: utf-8 -*-
"""
验证自定义（非模态）窗口拖动：
  1. 探测一个 WM_NCHITTEST 返回 HTCAPTION 的点；
  2. 用真实鼠标输入按住左键拖动约 3 秒；
  3. 判据 A：拖动「过程中」窗口位置持续跟随光标（不是松开才跳、也不是不动）；
  4. 判据 B：拖动期间 Vulkan 帧计数持续增长 —— 证明没有进入系统模态
     移动循环（模态循环会冻结帧循环，旧实现的磨砂就停在旧帧）。

用法：
  python scripts/verify_drag.py [ui.log 路径] [窗口类名]
默认：verify-blur/ui.log，类名 ModularClipboardWindow。
"""
import ctypes
import re
import sys
import time
from ctypes import wintypes

user32 = ctypes.windll.user32
shcore = ctypes.windll.shcore

# DPI 感知，否则坐标被系统虚拟化。
try:
    shcore.SetProcessDpiAwareness(2)
except Exception:
    user32.SetProcessDPIAware()

CLASS_NAME = sys.argv[2] if len(sys.argv) > 2 else "ModularClipboardWindow"
LOG_PATH = sys.argv[1] if len(sys.argv) > 1 else r"D:\modular-clipboard\verify-blur\ui.log"

WM_NCHITTEST = 0x0084
HTCAPTION = 2
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004

SW_RESTORE = 9

user32.FindWindowW.restype = wintypes.HWND
user32.FindWindowW.argtypes = [wintypes.LPCWSTR, wintypes.LPCWSTR]
user32.SendMessageW.restype = ctypes.c_longlong
user32.SendMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
user32.GetWindowRect.restype = wintypes.BOOL
user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.GetClientRect.restype = wintypes.BOOL
user32.GetClientRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.ClientToScreen.restype = wintypes.BOOL
user32.ClientToScreen.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.POINT)]
user32.SetCursorPos.restype = wintypes.BOOL
user32.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
user32.mouse_event.restype = None
user32.mouse_event.argtypes = [wintypes.DWORD, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD, ctypes.POINTER(ctypes.c_ulong)]
user32.SetForegroundWindow.restype = wintypes.BOOL
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.ShowWindow.restype = wintypes.BOOL
user32.ShowWindow.argtypes = [wintypes.HWND, ctypes.c_int]


def fail(msg):
    print(f"[drag] FAIL: {msg}")
    sys.exit(1)


def window_rect(hwnd):
    r = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(r))
    return r.left, r.top, r.right, r.bottom


def client_origin(hwnd):
    cr = wintypes.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(cr))
    p = wintypes.POINT(0, 0)
    user32.ClientToScreen(hwnd, ctypes.byref(p))
    return p.x, p.y


def find_caption_point(hwnd):
    """在窗口顶部栏扫一个 HTCAPTION 点；扫不到再扫左右边距。"""
    l, t, r, b = window_rect(hwnd)
    # 顶部 10~70 物理像素（避开顶部 6px resize 边）。
    for y in range(t + 12, t + 70, 6):
        for x in range(l + 24, r - 24, 12):
            lp = (y << 16) | (x & 0xFFFF)
            hit = user32.SendMessageW(hwnd, WM_NCHITTEST, 0, lp) & 0xFFFFFFFF
            if hit == HTCAPTION:
                return x, y
    # 退而求其次：客户区左右边距（列表留白也是拖动区）。
    cx, cy = client_origin(hwnd)
    _, _, crr, cbb = window_rect(hwnd)
    for x in list(range(cx, cx + 14, 2)) + list(range(crr - 14, crr, 2)):
        for y in range(cy + 80, cbb - 40, 20):
            lp = (y << 16) | (x & 0xFFFF)
            hit = user32.SendMessageW(hwnd, WM_NCHITTEST, 0, lp) & 0xFFFFFFFF
            if hit == HTCAPTION:
                return x, y
    return None


def last_frame():
    """读 ui.log 最后一条『帧统计 frame=N』，返回 (frame, 时间戳epoch)。"""
    frame = None
    ts = None
    pat = re.compile(r"^(\S+)\s+DEBUG\s+帧统计 frame=(\d+)")
    try:
        with open(LOG_PATH, "r", encoding="utf-8", errors="replace") as f:
            for line in f:
                m = pat.match(line)
                if m:
                    ts = time.mktime(time.strptime(m.group(1)[:19], "%Y-%m-%dT%H:%M:%S"))
                    frame = int(m.group(2))
    except FileNotFoundError:
        pass
    return frame, ts


def main():
    hwnd = user32.FindWindowW(CLASS_NAME, None)
    if not hwnd:
        fail(f"找不到窗口类 {CLASS_NAME}")

    user32.ShowWindow(hwnd, SW_RESTORE)
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.4)

    cap = find_caption_point(hwnd)
    if cap is None:
        fail("窗口上找不到任何 HTCAPTION 拖动点")
    px, py = cap
    print(f"[drag] 找到拖动点（屏幕坐标）= ({px},{py})")

    l0, t0, _, _ = window_rect(hwnd)

    # 让日志稳定，记录拖动前帧号。
    time.sleep(0.6)
    f0, _ = last_frame()
    print(f"[drag] 拖动前帧号 frame={f0}")

    # 真实按下。
    user32.SetCursorPos(px, py)
    time.sleep(0.15)
    user32.mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, None)
    time.sleep(0.2)

    # 拖动路径：向右下移动（中途采样），再向左上回到原点附近。
    mid_samples = []
    dx_total, dy_total = 120, 80
    steps = 14
    for i in range(1, steps + 1):
        nx = px + int(dx_total * i / steps)
        ny = py + int(dy_total * i / steps)
        user32.SetCursorPos(nx, ny)
        time.sleep(0.05)
        if i == steps // 2:
            l, t, _, _ = window_rect(hwnd)
            mid_samples.append((l - l0, t - t0))
    # 拖到右下后停住，让帧日志多刷几条（总按住时长 ~3 秒）。
    time.sleep(2.0)
    mid_hold = window_rect(hwnd)[:2]
    # 拖回起点。
    for i in range(steps, -1, -1):
        nx = px + int(dx_total * i / steps)
        ny = py + int(dy_total * i / steps)
        user32.SetCursorPos(nx, ny)
        time.sleep(0.03)
    user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, None)
    time.sleep(0.5)

    f1, _ = last_frame()
    l2, t2, _, _ = window_rect(hwnd)
    print(f"[drag] 拖动后帧号 frame={f1}（增量 {None if f0 is None or f1 is None else f1 - f0}）")
    print(f"[drag] 中途采样窗口位移 = {mid_samples}")
    print(f"[drag] 按住停留时窗口位移 = ({mid_hold[0]-l0},{mid_hold[1]-t0})")
    print(f"[drag] 松开后窗口位移（应回到≈0）= ({l2-l0},{t2-t0})")

    # 判据 A：拖动中途，窗口已跟随移动到大致光标位移的一半位置（不是 0）。
    if not mid_samples:
        fail("没有采到中途样本")
    mx, my = mid_samples[0]
    if abs(mx) < 20 or abs(my) < 12:
        fail(f"拖动中途窗口未跟随（位移 {mx},{my} 过小）——移动可能仍被系统模态循环吞掉")

    # 按住停留时应到达总位移附近（容差 40px，含圆角/边界）。
    hx, hy = mid_hold[0] - l0, mid_hold[1] - t0
    if abs(hx - dx_total) > 40 or abs(hy - dy_total) > 40:
        fail(f"按住停留时窗口未到位（期望 ~{dx_total},{dy_total}，实得 {hx},{hy}）")

    # 判据 B：拖动 ~3 秒期间帧计数必须显著增长。300fps 下应 ~900 帧；
    # 模态循环冻结时增量接近 0。阈值保守取 200。
    if f0 is None or f1 is None:
        fail("读不到帧统计日志")
    if f1 - f0 < 200:
        fail(f"拖动期间帧几乎不增长（{f0}->{f1}，仅 {f1-f0}）——帧循环被模态移动冻结")

    # 松开后窗口应回到原点附近（路径拖回了）。
    if abs(l2 - l0) > 30 or abs(t2 - t0) > 30:
        print(f"[drag] 注意：窗口未精确回到原点（{l2-l0},{t2-t0}），但不影响拖动判据")

    print("[drag] PASS：拖动中窗口实时跟随，且帧循环未被冻结（磨砂可实时更新）")


if __name__ == "__main__":
    main()
