# 主题切换 + 字体缩放实机验证（DPI 感知版）。
#
# 教训：PowerShell 进程若不声明 DPI 感知，Screen.Bounds 返回的是**虚拟化
# 逻辑尺寸**（1707x1067），而 CopyFromScreen 抓到的是物理像素的左上角
# 一小块 —— 设置窗口（物理 858..2028）会被截掉右半，导致「点击位置对、
# 但截图上量不到变化」的误判。这里先 SetProcessDPIAware，全程用物理像素。
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class WA {
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

[void][WA]::SetProcessDPIAware()

$exe = 'D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
$env:MC_SETTINGS = '1'
$env:RUST_LOG = 'info'
$dataDir = Join-Path $env:TEMP ("mc-tf2-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory $dataDir | Out-Null
# 无 BOM 写入：带 BOM 的 JSON 会让 Config::load 解析失败并静默回落默认值
# （实测过一次：配置写进去了但日志里 configured=None）。
$json = '{"ui":{"dark_mode":true,"font_scale":1.0}}'
[System.IO.File]::WriteAllText((Join-Path $dataDir 'config.json'), $json, (New-Object System.Text.UTF8Encoding($false)))

$p = Start-Process -FilePath $exe -ArgumentList "--data-dir","$dataDir" -PassThru
Start-Sleep -Seconds 4

$h = [WA]::FindTitle('剪贴板设置')
if ($h -eq [IntPtr]::Zero) { Write-Host "FAIL: 找不到设置窗口"; exit 1 }
$r = New-Object WA+RECT; [void][WA]::GetWindowRect($h, [ref]$r)
# 设置窗逻辑尺寸 780x560 ⇒ scale 由物理宽推出。
$scale = ($r.R - $r.L) / 780.0
Write-Host "设置窗物理 rect=($($r.L),$($r.T),$($r.R),$($r.B)) scale=$scale"
$L = $r.L; $T = $r.T

function Snap($name) {
  Add-Type -AssemblyName System.Drawing -ErrorAction SilentlyContinue
  Add-Type -AssemblyName System.Windows.Forms -ErrorAction SilentlyContinue
  $b = New-Object System.Drawing.Bitmap([System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width, [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Height)
  $g = [System.Drawing.Graphics]::FromImage($b)
  $g.CopyFromScreen(0,0,0,0,$b.Size)
  $b.Save("D:\modular-clipboard\$name",[System.Drawing.Imaging.ImageFormat]::Png)
  $w = $b.Width; $hh = $b.Height
  $g.Dispose(); $b.Dispose()
  Write-Host "  $name : $w x $hh"
}
# 逻辑点 → 物理像素
function P([double]$v) { [int]($v * $scale) }

$parkX = $L + (P 30); $parkY = $T + (P 520)

[void][WA]::Park($parkX, $parkY); Snap 'vtf2_dark_100.png'

# ---- 1. 切浅色 ----
# 导航第 2 项（外观）：y = 12+34+10+6 + 1*42 + 20 = 124
[void][WA]::Click(($L + (P 98)), ($T + (P 124)))
Start-Sleep -Milliseconds 500
# 主题分段（实测几何：轨道逻辑 y[88,117] x[516,755]，3 段各 80）
# ⇒ 第 3 段「浅色」中心 = (716, 102)。
[void][WA]::Click(($L + (P 716)), ($T + (P 102)))
Start-Sleep -Milliseconds 900
[void][WA]::Park($parkX, $parkY); Snap 'vtf2_light_100.png'
Write-Host "已点「浅色」逻辑(716,102)"

# ---- 2. 字体缩放「超大」(150%) ----
# 字体缩放分段（实测：轨道逻辑 y[142,171] x[464,713]，5 段各 50）
# ⇒ 第 5 段「超大」中心 = (464 + 4.5*50, 156) = (689, 156)。
[void][WA]::Click(($L + (P 689)), ($T + (P 156)))
Start-Sleep -Milliseconds 800
[void][WA]::Park($parkX, $parkY); Snap 'vtf2_light_150.png'
Write-Host "已点「超大」逻辑(689,156)"

Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300
$log = Join-Path $dataDir 'ui.log'
if (Test-Path $log) {
  Write-Host "---- ui.log 关键行 ----"
  Select-String -Path $log -Pattern '主题已解析|字体缩放已应用|设置窗口定位' | ForEach-Object { Write-Host $_.Line }
  Copy-Item $log 'D:\modular-clipboard\vtf2_ui.log' -Force
}
Remove-Item $dataDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "done"
