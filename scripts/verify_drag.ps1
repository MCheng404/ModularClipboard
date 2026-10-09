# 临时验证：真实鼠标拖动窗口（参数指定相对窗口左/顶的拖动点与目标偏移）。
param([int]$TargetPid, [int]$RelX, [int]$RelY, [int]$Dx, [int]$Dy)

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class DragWin2 {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  public static IntPtr MainOf(uint want) {
    IntPtr m = IntPtr.Zero;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p == want) {
        var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256);
        if (sb.ToString().StartsWith("ModularClipboard")) m = h;
      }
      return true;
    }, IntPtr.Zero);
    return m;
  }
  public static int[] Rect(IntPtr h) { RECT r; GetWindowRect(h, out r); return new int[] { r.L, r.T, r.R, r.B }; }
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

$main = [DragWin2]::MainOf([uint32]$TargetPid)
if ($main -eq [IntPtr]::Zero) { Write-Output "NO_MAIN"; exit 1 }
$b = [DragWin2]::Rect($main)
[DragWin2]::Drag(($b[0] + $RelX), ($b[1] + $RelY), ($b[0] + $RelX + $Dx), ($b[1] + $RelY + $Dy))
Start-Sleep -Milliseconds 600
$a = [DragWin2]::Rect($main)
Write-Output ("BEFORE=" + ($b -join ','))
Write-Output ("AFTER=" + ($a -join ','))
Write-Output ("MOVED=" + ($a[0] - $b[0]) + "," + ($a[1] - $b[1]))
