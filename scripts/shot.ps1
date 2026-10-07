$ErrorActionPreference = 'Stop'
# ⚠️ 必须先让本进程Per-Monitor DPI 感知，否则 GetClientRect 返回的是
# **DPI 虚拟化后的逻辑坐标**，而 CopyFromScreen 按物理像素抓取。
# 两者不一致时会截到窗口的一个角（实测 150% 缩放下拿到 280x373，
# 真实客户区是 420x560），据此做的像素判据全是错的。
# early-bound（Add-Type 编译成类型而非字符串）保证失败时抛异常，
# 不会静默退化成虚拟化坐标。-4 == PER_MONITOR_AWARE_V2。
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CapDpi {
  [DllImport("user32.dll")]
  public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
}
'@
$dpiOk = [CapDpi]::SetProcessDpiAwarenessContext([IntPtr](-4))
Write-Host "DPI 感知设置: $dpiOk（False 通常表示进程早已设置过，不影响后续坐标）"

# 启动真实 exe，截取客户区，把像素读回来做布局分析
# 目的：验证「布局计算结果」与「实际渲染位置」是否一致

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class Cap {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }

  public static List<IntPtr> TopLevel(uint want) {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Cls(IntPtr h) { var sb = new StringBuilder(256); GetClassNameW(h, sb, 256); return sb.ToString(); }
}
'@

$exe = 'D:\WorkBuddy\Tiez\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 600

$env:RUST_LOG = 'info,modular_clipboard=debug'
$err = 'D:\WorkBuddy\Tiez\shot.err'
Remove-Item $err -ErrorAction SilentlyContinue
$p = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru `
  -RedirectStandardOutput 'D:\WorkBuddy\Tiez\shot.log' -RedirectStandardError $err
Start-Sleep -Seconds 4
$p.Refresh()

$main = [IntPtr]::Zero
foreach ($h in [Cap]::TopLevel([uint32]$p.Id)) { if ([Cap]::Cls($h) -eq 'ModularClipboardWindow') { $main = $h; break } }
if ($main -eq [IntPtr]::Zero) { Write-Host "找不到主窗口"; Stop-Process -Id $p.Id -Force; exit 1 }

$rc = New-Object Cap+RECT
[void][Cap]::GetClientRect($main, [ref]$rc)
$w = $rc.R - $rc.L; $h = $rc.B - $rc.T
$dpi = [Cap]::GetDpiForWindow($main)
$pt = New-Object Cap+POINT
[void][Cap]::ClientToScreen($main, [ref]$pt)
Write-Host ("客户区(物理) : {0} x {1}  DPI={2}缩放={3}" -f $w, $h, $dpi, ($dpi/96.0))
Write-Host ("客户区(逻辑) : {0:N1} x {1:N1}" -f ($w/($dpi/96.0)), ($h/($dpi/96.0)))
Write-Host ("屏幕原点     : {0},{1}" -f $pt.X, $pt.Y)

# 屏幕截图整个客户区
Add-Type -AssemblyName System.Drawing
$bmp = New-Object System.Drawing.Bitmap $w, $h
$gfx = [System.Drawing.Graphics]::FromImage($bmp)
$gfx.CopyFromScreen($pt.X, $pt.Y, 0, 0, (New-Object System.Drawing.Size($w, $h)))
$png = 'D:\WorkBuddy\Tiez\shot-client.png'
$bmp.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
$gfx.Dispose(); $bmp.Dispose()
Write-Host "客户区截图已存: $png"

# 全屏截图（看窗口在屏幕上的实际位置与大小）
# ⚠️ 必须先加载程序集再用 [System.Windows.Forms.SystemInformation]：
# 未加载时该类型不存在，PowerShell 会在此抛「无法找到类型」而不是返回 $null，
# 于是 `if (-not $sb)` 这类兜底判断根本没机会执行（原脚本的 bug）。
Add-Type -AssemblyName System.Windows.Forms
$sb = [System.Windows.Forms.SystemInformation]::VirtualScreen
$fb = New-Object System.Drawing.Bitmap $sb.Width, $sb.Height
$fg = [System.Drawing.Graphics]::FromImage($fb)
$fg.CopyFromScreen($sb.Left, $sb.Top, 0, 0, (New-Object System.Drawing.Size($sb.Width, $sb.Height)))
$fpng = 'D:\WorkBuddy\Tiez\shot-fullscreen.png'
$fb.Save($fpng, [System.Drawing.Imaging.ImageFormat]::Png)
$fg.Dispose(); $fb.Dispose()
Write-Host "全屏截图已存: $fpng ($($sb.Width)x$($sb.Height))"

Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300
Write-Host ""
Write-Host "=== 运行日志（尾部）==="
Get-Content $err -ErrorAction SilentlyContinue | Select-Object -Last 12