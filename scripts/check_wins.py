import ctypes, ctypes.wintypes as wt
u=ctypes.windll.user32
u.FindWindowW.restype=wt.HWND
u.FindWindowW.argtypes=[wt.LPCWSTR,wt.LPCWSTR]
u.IsWindowVisible.argtypes=[wt.HWND]
u.IsWindowVisible.restype=wt.BOOL
for t in ["ModularClipboard","剪贴板设置"]:
    h=u.FindWindowW(None,t)
    print(t, "hwnd=",h, "visible=", bool(u.IsWindowVisible(h)) if h else "N/A")
