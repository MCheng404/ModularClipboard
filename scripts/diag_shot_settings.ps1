# 抓设置窗口内容（PrintWindow + PW_RENDERFULLCONTENT），保存 PNG。
#
# 与 diag_shot_main.ps1 同源，只把目标窗口从「ModularClipboard」换成
# 「剪贴板设置」。用于验证设置窗口自身的绘制（含自研 GPU 模糊覆盖层）。
param([int]$TargetPid, [string]$OutPath)

# CAUTION: 必须显式加载 System.Drawing —— PowerShell 7 在部分宿主下
# 不会自动加载它，缺这行的症状是 `New-Object System.Drawing.Bitmap`
# 报「找不到类型」，与脚本本身无关，很容易误判成脚本坏了。
Add-Type -AssemblyName System.Drawing

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class PWS {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  public static List<IntPtr> OfPid(uint want) { var r = new List<IntPtr>(); EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero); return r; }
  public static string Txt(IntPtr h) { var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256); return sb.ToString(); }
}
'@

$ofPid = [PWS]::OfPid([uint32]$TargetPid)
$settings = [IntPtr]::Zero
foreach ($h in $ofPid) { if ([PWS]::Txt($h) -eq '剪贴板设置') { $settings = $h } }
Write-Output "设置窗口=$settings"
if ($settings -eq [IntPtr]::Zero) { Write-Output "未找到设置窗口"; exit 1 }

$r = New-Object PWS+RECT; [void][PWS]::GetWindowRect($settings, [ref]$r)
$w = $r.R - $r.L; $h = $r.B - $r.T

# CAUTION: 本脚本进程不是 per-monitor DPI aware，GetWindowRect / PrintWindow
# 返回逻辑点；客户区是物理像素。必须按 GetDpiForWindow 换算，否则只画进
# 左上 1/scale 的内容。
$dpi = [PWS]::GetDpiForWindow($settings)
$scale = [double]$dpi / 96.0
$pw = [int][Math]::Round($w * $scale); $ph = [int][Math]::Round($h * $scale)
Write-Output "窗口尺寸 ${w}x${h} (dpi=$dpi scale=$scale => bitmap ${pw}x${ph}"

$bmp = New-Object System.Drawing.Bitmap($pw, $ph)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$dc = $g.GetHdc()
$ok = [PWS]::PrintWindow($settings, $dc, 2)  # PW_RENDERFULLCONTENT
$g.ReleaseHdc($dc)
$g.Dispose()
if (-not $ok) { Write-Output "PrintWindow 失败"; exit 1 }
$bmp.Save($OutPath, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "已保存 $OutPath"
