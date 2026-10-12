# 临时验证：高频棋盘背景 + 桌面基准 + 应用截图（单次进程内完成）。
# 判据：棋盘基准相邻像素差很大；若磨砂生效，窗口区域的高频应被抹平。
param([string]$OutPath = "D:\modular-clipboard\target\check_app.png",
      [string]$RefPath = "D:\modular-clipboard\target\check_ref.png")

$ErrorActionPreference = 'Continue'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CapDpi5 {
  [DllImport("user32.dll")]
  public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
}
'@ | Out-Null
[void][CapDpi5]::SetProcessDpiAwarenessContext([IntPtr](-4))

function Rect-Capture([int]$X, [int]$Y, [int]$W, [int]$H, [string]$Path) {
  $bmp = New-Object System.Drawing.Bitmap $W, $H
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($X, $Y, 0, 0, (New-Object System.Drawing.Size($W, $H)))
  $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
  Write-Output "已存: $Path"
}

function Pump([int]$ms) {
  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  while ($sw.Elapsed.TotalMilliseconds -lt $ms) {
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 50
  }
}

# ---- 1. 高频棋盘背景（自绘，不依赖壁纸）----
$form = New-Object System.Windows.Forms.Form
$form.BackColor = [System.Drawing.Color]::White
$form.FormBorderStyle = 'None'
$form.Bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$form.TopMost = $false
$form.Add_Paint({
  param($s, $e)
  $g = $e.Graphics
  for ($y = 0; $y -lt $s.Height; $y += 16) {
    for ($x = 0; $x -lt $s.Width; $x += 16) {
      $c = if ((($x / 16) + ($y / 16)) % 2 -eq 0) { [System.Drawing.Color]::White } else { [System.Drawing.Color]::Black }
      $b = New-Object System.Drawing.SolidBrush($c)
      $g.FillRectangle($b, $x, $y, 16, 16)
      $b.Dispose()
    }
  }
})
$form.Show()
Pump 800
Write-Output "棋盘背景已显示"

# ---- 2. 桌面基准 ----
Rect-Capture -X 1738 -Y 207 -W 520 -H 800 -Path $RefPath

# ---- 3. 启动应用 ----
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class WFind5 {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  public static IntPtr Find(uint want) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p == want) {
        var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256);
        if (sb.ToString() == "ModularClipboard") { found = h; return false; }
      }
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
'@ | Out-Null

$exe = 'D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
if ($env:MC_NO_EXCLUDE) { $env:MC_NO_EXCLUDE = '1' }
$p = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru
Pump 5000

$h = [WFind5]::Find([uint32]$p.Id)
if ($h -ne [IntPtr]::Zero) {
  $r = New-Object WFind5+RECT
  [void][WFind5]::GetWindowRect($h, [ref]$r)
  $w = $r.R - $r.L; $ht = $r.B - $r.T
  Write-Output "主窗口 rect=($($r.L),$($r.T)) ${w}x${ht}"
  Rect-Capture -X $r.L -Y $r.T -W $w -H $ht -Path $OutPath
} else {
  Write-Output "未找到主窗口"
}
Pump 2500
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
$form.Close(); $form.Dispose()
