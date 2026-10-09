# 诊断：前台窗口状态 + 鼠标事件是否到达主窗口（点击齿轮→设置窗口、拖动）。
param([int]$TargetPid)

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class DG {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  public static List<IntPtr> OfPid(uint want) {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Txt(IntPtr h) { if (h == IntPtr.Zero) return "(null)"; var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256); return sb.ToString(); }
  public static int[] Rect(IntPtr h) { RECT r; GetWindowRect(h, out r); return new int[] { r.L, r.T, r.R, r.B }; }
  public static void Click(int x, int y) {
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(300);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(100);
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(300);
  }
  public static void Drag(int x0, int y0, int x1, int y1) {
    SetCursorPos(x0, y0);
    System.Threading.Thread.Sleep(400);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(250);
    for (int i = 1; i <= 12; i++) {
      SetCursorPos(x0 + (x1 - x0) * i / 12, y0 + (y1 - y0) * i / 12);
      System.Threading.Thread.Sleep(30);
    }
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
  }
}
'@

$main = [IntPtr]::Zero; $settings = [IntPtr]::Zero
foreach ($h in [DG]::OfPid([uint32]$TargetPid)) {
  $t = [DG]::Txt($h)
  if ($t -like 'ModularClipboard*' -and $main -eq [IntPtr]::Zero) { $main = $h }
  elseif ($t -eq '剪贴板设置') { $settings = $h }
}
if ($main -eq [IntPtr]::Zero) { Write-Output "NO_MAIN"; exit 1 }

Write-Output ("FG_BEFORE=" + [DG]::Txt([DG]::GetForegroundWindow()))
[DG]::SetForegroundWindow($main) | Out-Null
Start-Sleep -Milliseconds 500
Write-Output ("FG_AFTER=" + [DG]::Txt([DG]::GetForegroundWindow()))

$m0 = [DG]::Rect($main)
Write-Output ("MAIN_INIT=" + ($m0 -join ','))
if ($settings -ne [IntPtr]::Zero) { Write-Output ("SETT_VISIBLE_INIT=" + [DG]::IsWindowVisible($settings)) }

# 点击顶栏左半空白（激活 + 非拖动响应）：窗口内 (60,25) 物理。
[DG]::Click(($m0[0] + 60), ($m0[1] + 25))
Start-Sleep -Milliseconds 300

# 点齿轮（顶栏右侧第3按钮）：窗口内物理 (310,25) —— 逻辑 (206,16)?? 用估算中心 (295.6*1.5, 22*1.5) → 物理 (444,33)?? 修正：btn_area 右端逻辑 312 → 齿轮中心约 295 → 物理 443+80=523, 33+80=113。
[DG]::Click(523, 113)
Start-Sleep -Seconds 2

# 设置窗口是否出现
$found = $false
foreach ($h in [DG]::OfPid([uint32]$TargetPid)) {
  if ([DG]::Txt($h) -eq '剪贴板设置') { $found = $true; break }
}
Write-Output ("SETT_AFTER_GEAR_CLICK=" + $found)

# 拖动顶栏左半：窗口内 (60,25) → +160,+60
$m = [DG]::Rect($main)
[DG]::Drag(($m[0] + 60), ($m[1] + 25), ($m[0] + 220), ($m[1] + 85))
Start-Sleep -Milliseconds 600
$m1 = [DG]::Rect($main)
Write-Output ("MAIN_AFTER=" + ($m1 -join ','))
Write-Output ("MOVED=" + ($m1[0] - $m0[0]) + "," + ($m1[1] - $m0[1]))
