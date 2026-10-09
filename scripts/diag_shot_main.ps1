# 临时诊断：PrintWindow 抓主窗口内容（PW_RENDERFULLCONTENT=2），保存 PNG。
param([int]$TargetPid, [string]$OutPath)

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class PWC {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  public static List<IntPtr> OfPid(uint want) { var r = new List<IntPtr>(); EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero); return r; }
  public static string Txt(IntPtr h) { var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256); return sb.ToString(); }
  public static IntPtr Fg() { return GetForegroundWindow(); }
}
'@
$ofPid = [PWC]::OfPid([uint32]$TargetPid)
$main = [IntPtr]::Zero; $settings = [IntPtr]::Zero
foreach ($h in $ofPid) { $t = [PWC]::Txt($h); if ($t -eq 'ModularClipboard') { $main = $h }; if ($t -eq '剪贴板设置') { $settings = $h } }
Write-Output "主窗口=$main 设置=$settings"
if ($main -eq [IntPtr]::Zero) { exit 1 }
$r = New-Object PWC+RECT; [void][PWC]::GetWindowRect($main, [ref]$r)
$w = $r.R - $r.L; $h = $r.B - $r.T
Write-Output "窗口尺寸 ${w}x${h}"
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$dc = $g.GetHdc()
$ok = [PWC]::PrintWindow($main, $dc, 2)  # PW_RENDERFULLCONTENT
$g.ReleaseHdc($dc)
$g.Dispose()
if (-not $ok) { Write-Output "PrintWindow 失败"; exit 1 }
$bmp.Save($OutPath, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "已保存 $OutPath"
