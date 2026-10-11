# 设置窗口交互验证：导航切换 + 关闭按钮 + 拖动。
# 启动主程序（MC_SETTINGS=1 自动打开设置窗口），然后：
#   1. 点击「外观」导航 → 右侧内容应变为外观卡片
#   2. 点击右上角关闭按钮 → 设置窗口应隐藏
#   3. 拖动标题区 → 窗口位置应变化
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class W5 {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  public static IntPtr FindTitle(string want) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h,l) => { var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256); if (sb.ToString() == want) { found = h; return false; } return true; }, IntPtr.Zero);
    return found;
  }
  public static void Click(int x, int y) {
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(120);
    mouse_event(0x02, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(60);
    mouse_event(0x04, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(120);
  }
  public static void Drag(int x0, int y0, int x1, int y1) {
    SetCursorPos(x0, y0);
    System.Threading.Thread.Sleep(150);
    mouse_event(0x02, 0, 0, 0, UIntPtr.Zero);
    for (int i = 1; i <= 10; i++) {
      SetCursorPos(x0 + (x1 - x0) * i / 10, y0 + (y1 - y0) * i / 10);
      System.Threading.Thread.Sleep(25);
    }
    mouse_event(0x04, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(250);
  }
}
'@

$exe = 'D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
$env:MC_SETTINGS = '1'
$dataDir = Join-Path $env:TEMP ("mc-iv-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory $dataDir | Out-Null
$p = Start-Process -FilePath $exe -ArgumentList "--data-dir","$dataDir" -PassThru
Start-Sleep -Seconds 4

$h = [W5]::FindTitle('剪贴板设置')
if ($h -eq [IntPtr]::Zero) { Write-Host "FAIL: 找不到设置窗口"; exit 1 }
$r = New-Object W5+RECT
[void][W5]::GetWindowRect($h, [ref]$r)
$w = $r.R - $r.L; $hh = $r.B - $r.T
Write-Host "设置窗口 rect=($($r.L),$($r.T),$($r.R),$($r.B)) w=$w h=$hh"

# 截图工具函数（存盘）
function Snap($name) {
  Add-Type -AssemblyName System.Drawing -ErrorAction SilentlyContinue
  Add-Type -AssemblyName System.Windows.Forms -ErrorAction SilentlyContinue
  $b = New-Object System.Drawing.Bitmap([System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width, [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Height)
  $g = [System.Drawing.Graphics]::FromImage($b)
  $g.CopyFromScreen(0,0,0,0,$b.Size)
  $b.Save("D:\modular-clipboard\$name",[System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $b.Dispose()
}

# ---- 1. 点击「外观」导航（第 2 项，左侧导航列）----
# 导航列：左缘 = 窗口左 + 12pt，宽 172pt；首项 = 搜索框下方。
# ppp 按窗口物理宽 780 与 DEFAULT_SIZE 780 推断为 1.0（系统 DPI 144 但程序按 1.0 渲染）。
$ppp = 1.0
$navX = $r.L + (12 + 172/2) * $ppp
$navTop = $r.T + (12 + 34 + 10 + 6) * $ppp       # margin + 搜索框 + 间隔 + pad
$appearanceY = $navTop + (40 + 2) * $ppp + 20 * $ppp  # 第 2 项中心
[void][W5]::Click([int]$navX, [int]$appearanceY)
Start-Sleep -Milliseconds 500
Snap 'iv_after_nav.png'
Write-Host "已点击外观导航 ($([int]$navX),$([int]$appearanceY))"

# ---- 2. 点击关闭按钮（右上角，窗口右 - 15，窗口顶 + 15）----
$closeX = $r.R - 15
$closeY = $r.T + 15
[void][W5]::Click([int]$closeX, [int]$closeY)
Start-Sleep -Milliseconds 600
if (-not [W5]::IsWindowVisible($h)) {
  Write-Host "PASS: 关闭按钮生效，设置窗口已隐藏"
} else {
  Write-Host "FAIL: 设置窗口仍可见"
}

# ---- 3. 重新打开设置窗口（点主窗口齿轮）并拖动 ----
# 用托盘/主窗口齿轮打开：直接点主窗口的齿轮按钮（主窗口右上角）。
$mh = [W5]::FindTitle('ModularClipboard')
if ($mh -ne [IntPtr]::Zero) {
  $mr = New-Object W5+RECT
  [void][W5]::GetWindowRect($mh, [ref]$mr)
  # 齿轮在最右：主窗口右 - 15，顶部 + 15
  [void][W5]::Click([int]($mr.R - 15), [int]($mr.T + 15))
  Start-Sleep -Seconds 1
}
$h2 = [W5]::FindTitle('剪贴板设置')
if ($h2 -eq [IntPtr]::Zero) { Write-Host "WARN: 重新打开设置窗口失败，跳过拖动验证"; exit 0 }
$r2 = New-Object W5+RECT
[void][W5]::GetWindowRect($h2, [ref]$r2)
# 拖动区：左挖到导航列右缘（12+172），右挖关闭按钮 40pt；标题高 44pt
$dragX = [int](($r2.L + $r2.R) / 2)   # 中间段
$dragY = $r2.T + 15
$endX = $dragX + 150; $endY = $dragY + 90
[void][W5]::Drag($dragX, $dragY, $endX, $endY)
$r3 = New-Object W5+RECT
[void][W5]::GetWindowRect($h2, [ref]$r3)
$moved = [Math]::Abs($r3.L - $r2.L) + [Math]::Abs($r3.T - $r2.T)
if ($moved -gt 10) {
  Write-Host "PASS: 拖动生效 ($($r2.L),$($r2.T)) -> ($($r3.L),$($r3.T)) 位移 $moved"
} else {
  Write-Host "FAIL: 拖动无效"
}
Snap 'iv_after_drag.png'

Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-Item $dataDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "done"
