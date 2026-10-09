# 实测：设置窗口存在时拖动主窗口，采样两窗口位置轨迹。
# 先激活主窗口（点击一次），再按住拖动，拖动中采样。
param([int]$TargetPid)

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class DWS2 {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  public static List<IntPtr> OfPid(uint want) {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Txt(IntPtr h) { var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256); return sb.ToString(); }
  public static int[] Rect(IntPtr h) { RECT r; GetWindowRect(h, out r); return new int[] { r.L, r.T, r.R, r.B }; }
  public static void Click(int x, int y) {
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(250);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(80);
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(250);
  }
  public static void DragSteps(int x0, int y0, int x1, int y1, int steps, Action<int[]> sample) {
    SetCursorPos(x0, y0);
    System.Threading.Thread.Sleep(400);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(250);
    for (int i = 1; i <= steps; i++) {
      SetCursorPos(x0 + (x1 - x0) * i / steps, y0 + (y1 - y0) * i / steps);
      System.Threading.Thread.Sleep(60);
      if (sample != null) sample(new int[] { i, x0 + (x1 - x0) * i / steps, y0 + (y1 - y0) * i / steps });
    }
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
  }
}
'@

$main = [IntPtr]::Zero; $settings = [IntPtr]::Zero
foreach ($h in [DWS2]::OfPid([uint32]$TargetPid)) {
  $t = [DWS2]::Txt($h)
  if ($t -like 'ModularClipboard*' -and $main -eq [IntPtr]::Zero) { $main = $h }
  elseif ($t -eq '剪贴板设置') { $settings = $h }
}
if ($main -eq [IntPtr]::Zero) { Write-Output "NO_MAIN"; exit 1 }

$m0 = [DWS2]::Rect($main)
Write-Output ("MAIN_INIT=" + ($m0 -join ','))
if ($settings -eq [IntPtr]::Zero) {
  Write-Output "NO_SETTINGS"
  $s0 = @(0,0,0,0)
} else {
  $s0 = [DWS2]::Rect($settings)
  Write-Output ("SETT_INIT=" + ($s0 -join ','))
}

# 先激活主窗口：点击其左上角空白（避开设置窗口区域）。
[DWS2]::SetForegroundWindow($main) | Out-Null
Start-Sleep -Milliseconds 400
[DWS2]::Click(($m0[0] + 60), ($m0[1] + 60))

# 拖动主窗口：顶栏左段（窗口内 x=22,y=25 物理，逻辑 (14.7,16.7) ∈ 左段 [0,34]）。
$m = [DWS2]::Rect($main)
$grabX = $m[0] + 22
$grabY = $m[1] + 25
$samples = New-Object System.Collections.Generic.List[string]
$st = $settings
[DWS2]::DragSteps([int]$grabX, [int]$grabY, [int]($grabX + 160), [int]($grabY + 60), 8, {
  param($step)
  $mm = [DWS2]::Rect($main); $ss = [DWS2]::Rect($st)
  $samples.Add(("step=$($step[0]) main=(" + $mm[0] + "," + $mm[1] + ") sett=(" + $ss[0] + "," + $ss[1] + ")"))
})
Start-Sleep -Milliseconds 400
$m1 = [DWS2]::Rect($main)
Write-Output ("MAIN_AFTER=" + ($m1 -join ','))
if ($settings -eq [IntPtr]::Zero) {
  Write-Output "SETT_AFTER=N/A"
} else {
  $s1 = [DWS2]::Rect($settings)
  Write-Output ("SETT_AFTER=" + ($s1 -join ','))
}
foreach ($x in $samples) { Write-Output $x }
