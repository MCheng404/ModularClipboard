# 临时验证：白色最大化背景（含消息泵）+ 桌面基准 + 应用截图（单次进程内完成）。
param([string]$OutPath = "D:\modular-clipboard\target\ab_acrylic_white2.png",
      [string]$RefPath = "D:\modular-clipboard\target\ref_white2.png")

$ErrorActionPreference = 'Continue'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

# ---- DPI 感知 ----
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CapDpi4 {
  [DllImport("user32.dll")]
  public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
}
'@ | Out-Null
[void][CapDpi4]::SetProcessDpiAwarenessContext([IntPtr](-4))

# ---- 1. 白色背景（必须泵消息，否则 WM_PAINT 不处理 → 根本没变白）----
$form = New-Object System.Windows.Forms.Form
$form.BackColor = [System.Drawing.Color]::White
$form.FormBorderStyle = 'None'
$form.Bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$form.TopMost = $false
$form.Show()
for ($i = 0; $i -lt 20; $i++) { [System.Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 50 }
Write-Output "白色背景已显示"

# 抓取指定屏幕矩形（必须在首次调用之前定义）
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

# ---- 2. 桌面基准（应用还没启动，应基本全白）----
Rect-Capture -X 1800 -Y 225 -W 520 -H 800 -Path $RefPath

# ---- 3. 启动应用 ----
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class WFind4 {
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
$p = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru
Pump 5000

$h = [WFind4]::Find([uint32]$p.Id)
if ($h -ne [IntPtr]::Zero) {
  $r = New-Object WFind4+RECT
  [void][WFind4]::GetWindowRect($h, [ref]$r)
  $w = $r.R - $r.L; $ht = $r.B - $r.T
  Write-Output "主窗口 rect=($($r.L),$($r.T)) ${w}x${ht}"
  Rect-Capture -X $r.L -Y $r.T -W $w -H $ht -Path $OutPath
} else {
  Write-Output "未找到主窗口"
}
Pump 3000
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
$form.Close(); $form.Dispose()
