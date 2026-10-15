# MCP 覆盖层实机取证（DPI 感知 + 实测几何）。
# 设置窗逻辑 780x560；外观/通信卡片首行中心 y≈102（实测），
# 导航第 6 项「通信」中心 y = 55+6+5*42+20 = 291。
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class WB {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
  public static IntPtr FindTitle(string want) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h,l) => { var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256); if (sb.ToString() == want) { found = h; return false; } return true; }, IntPtr.Zero);
    return found;
  }
  public static void Click(int x, int y) {
    SetCursorPos(x, y); System.Threading.Thread.Sleep(150);
    mouse_event(0x02, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(80);
    mouse_event(0x04, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(180);
  }
  public static void Park(int x, int y) { SetCursorPos(x, y); System.Threading.Thread.Sleep(250); }
}
'@
[void][WB]::SetProcessDPIAware()

$exe = 'D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
$env:MC_SETTINGS = '1'
$dataDir = Join-Path $env:TEMP ("mc-mcp3-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory $dataDir | Out-Null
[System.IO.File]::WriteAllText((Join-Path $dataDir 'config.json'),
  '{"ui":{"dark_mode":true,"font_scale":1.0}}', (New-Object System.Text.UTF8Encoding($false)))
$p = Start-Process -FilePath $exe -ArgumentList "--data-dir","$dataDir" -PassThru
Start-Sleep -Seconds 4

$h = [WB]::FindTitle('剪贴板设置')
$r = New-Object WB+RECT; [void][WB]::GetWindowRect($h, [ref]$r)
$scale = ($r.R - $r.L) / 780.0
Write-Host "设置窗物理 rect=($($r.L),$($r.T),$($r.R),$($r.B)) scale=$scale"
$L = $r.L; $T = $r.T
function P([double]$v) { [int]($v * $scale) }
function Snap($name) {
  Add-Type -AssemblyName System.Drawing -ErrorAction SilentlyContinue
  Add-Type -AssemblyName System.Windows.Forms -ErrorAction SilentlyContinue
  $b = New-Object System.Drawing.Bitmap([System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width, [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Height)
  $g = [System.Drawing.Graphics]::FromImage($b); $g.CopyFromScreen(0,0,0,0,$b.Size)
  $b.Save("D:\modular-clipboard\$name",[System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $b.Dispose()
}
# 中性停靠点：窗口右下角留白（面板之外、无控件）
$parkX = $L + (P 762); $parkY = $T + (P 552)

# 点「通信」导航（第 6 项）
[void][WB]::Click(($L + (P 98)), ($T + (P 291)))
Start-Sleep -Milliseconds 600
[void][WB]::Park($parkX, $parkY); Snap 'vmcp3_closed.png'

# MCP 总开关行的 ⓘ：实测（通信卡片无「重置」行 ⇒ 首行中心 y≈77）
# ⓘ 圆心逻辑 (701, 77)，开关在它右侧（accent 实底）。
[void][WB]::Click(($L + (P 701)), ($T + (P 77)))
Start-Sleep -Milliseconds 700
[void][WB]::Park($parkX, $parkY); Snap 'vmcp3_open.png'
Write-Host "已点 ⓘ 逻辑(701,77)"

Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-Item $dataDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "done"
