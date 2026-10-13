# -*- coding: utf-8 -*-
"""高频采样窗口/进程状态，抓手动热键卡死瞬间。跑 90 秒。"""
import ctypes, time, subprocess, sys
user32 = ctypes.windll.user32
WM_NULL=0; SMTO_ABORTIFHUNG=0x0002

def find(t):
    return user32.FindWindowW(None, ctypes.create_unicode_buffer(t))
def state(t):
    h = find(t)
    if not h: return "不存在"
    vis = bool(user32.IsWindowVisible(h))
    res = ctypes.c_size_t(0)
    ok = user32.SendMessageTimeoutW(h, WM_NULL,0,0,SMTO_ABORTIFHUNG,1000,ctypes.byref(res))
    return "hwnd=%s 可见=%s 响应=%s" % (h, vis, bool(ok))

def procstat():
    p = subprocess.run(['powershell','-NoProfile','-Command',
        "Get-Process -Name 'modular-clipboard' -ErrorAction SilentlyContinue | ForEach-Object { '{0}:cpu={1:0.0} respond={2}' -f $_.Id,$_.CPU,$_.Responding }"],
        capture_output=True, text=True)
    return ' | '.join(p.stdout.split())

LOG = open(r'D:\modular-clipboard\monitor_out.txt','w',encoding='utf-8')
def log(*a):
    s = time.strftime('%H:%M:%S') + ' ' + ' '.join(str(x) for x in a)
    LOG.write(s+'\n'); LOG.flush()

end = time.time() + 90
log("监控开始")
while time.time() < end:
    try:
        log("主窗[%s] 设置窗[%s] 进程{%s}" % (
            state("ModularClipboard"), state("剪贴板设置"), procstat()))
    except Exception as e:
        log("采样异常", e)
    time.sleep(0.25)
log("监控结束")
