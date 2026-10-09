# 设置窗口实机验证：拖动 + 点击。
# 启动主程序（MC_SETTINGS=1 自动打开设置窗口），然后：
#   1. 真实鼠标拖动设置窗口标题栏 → 窗口位置应变化（拖动有效）
#   2. PostMessage 点击关闭按钮 → 设置窗口应隐藏（点击有效）
# 用法：powershell -ExecutionPolicy Bypass -File scripts\verify_settings_window.ps1

$ErrorActionPreference = 'Stop'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$exe = Join-Path $RepoRoot 'target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
if (-not (Test-Path $exe)) { Write-Host "找不到 exe: $exe"; exit 1 }

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class W {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  [DllImport("user32.dll", SetLastError=true, EntryPoint="PostMessageW")] public static extern bool PM(IntPtr h, uint m, IntPtr w, IntPtr l);
  public static List<IntPtr> OfPid(uint want) {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Cls(IntPtr h) { var sb = new StringBuilder(256); GetClassNameW(h, sb, 256); return sb.ToString(); }
  public static string Txt(IntPtr h) { var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256); return sb.ToString(); }
}
'@

$env:MC_SETTINGS = '1'
$dataDir = Join-Path $env:TEMP ("mc-settings-verify-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory $dataDir | Out-Null
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

$p = Start-Process -FilePath $exe -ArgumentList "--data-dir","$dataDir" -PassThru
Start-Sleep -Seconds 4
$p.Refresh()

# 找设置窗口：类名统一是 ModularClipboardWindow，按**窗口标题**「剪贴板设置」匹配。
$settings = [IntPtr]::Zero
foreach ($h in [W]::OfPid([uint32]$p.Id)) {
  $txt = [W]::Txt($h)
  if ($txt -eq '剪贴板设置') { $settings = $h; break }
}
if ($settings -eq [IntPtr]::Zero) {
  Write-Host "✗ 找不到设置窗口（标题=剪贴板设置）"
  Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force
  exit 1
}
Write-Host "✓ 设置窗口已创建 hwnd=$settings"

# ---- 1. 拖动验证：真实鼠标拖动标题栏 ----
$r0 = New-Object W+RECT
[void][W]::GetWindowRect($settings, [ref]$r0)
$startX = ($r0.L + $r0.R) / 2
$startY = $r0.T + 15   # 标题栏内
$endX = $startX + 120
$endY = $startY + 80
[void][W]::SetCursorPos([int]$startX, [int]$startY)
Start-Sleep -Milliseconds 200
# MOUSEEVENTF_LEFTDOWN=0x02, MOVE=0x0001, LEFTUP=0x04
[void][W]::mouse_event(0x02, 0, 0, 0, [UIntPtr]::Zero)
for ($i = 1; $i -le 12; $i++) {
  [void][W]::SetCursorPos([int]($startX + ($endX - $startX) * $i / 12), [int]($startY + ($endY - $startY) * $i / 12))
  Start-Sleep -Milliseconds 30
}
[void][W]::mouse_event(0x04, 0, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 300
$r1 = New-Object W+RECT
[void][W]::GetWindowRect($settings, [ref]$r1)
$moved = [Math]::Abs($r1.L - $r0.L) + [Math]::Abs($r1.T - $r0.T)
if ($moved -gt 10) {
  Write-Host "✓ 拖动生效：窗口从 ($($r0.L),$($r0.T)) 移到 ($($r1.L),$($r1.T))，位移 $moved px"
} else {
  Write-Host "✗ 拖动无效：位置未变化（$($r0.L),$($r0.T) -> $($r1.L),$($r1.T)）"
}

# ---- 2. 点击验证：真实鼠标点击关闭按钮 ----
# 关闭按钮（×）中心：PrintWindow 截图为 (653, 35) 客户区物理像素
# （460x560 逻辑点 @ 1.5x → 690x840 物理；按钮中心 (460-12-8*1.5, 12+8*1.5)*1.5）。
# 屏幕坐标 = 窗口原点 + 客户区坐标（无边框窗口，客户区≈窗口区）。
$cr = New-Object W+RECT
[void][W]::GetClientRect($settings, [ref]$cr)
$btnClientX = 653
$btnClientY = 35
# 用窗口原点 + 客户区偏移换算屏幕坐标：窗口阴影 1px，用 GetWindowRect 与
# GetClientRect 的差对齐。
$wr = New-Object W+RECT
[void][W]::GetWindowRect($settings, [ref]$wr)
$borderX = ($wr.L) - ($cr.L)  # 屏幕坐标 = 窗口原点 + 客户区坐标
$borderY = ($wr.T) - ($cr.T)
$screenX = $wr.L + $btnClientX
$screenY = $wr.T + $btnClientY
[void][W]::SetCursorPos($screenX, $screenY)
Start-Sleep -Milliseconds 250
[void][W]::mouse_event(0x02, 0, 0, 0, [UIntPtr]::Zero)  # LEFTDOWN
Start-Sleep -Milliseconds 80
[void][W]::mouse_event(0x04, 0, 0, 0, [UIntPtr]::Zero)  # LEFTUP
Start-Sleep -Milliseconds 800
if (-not [W]::IsWindowVisible($settings)) {
  Write-Host "✓ 点击生效：关闭按钮触发，设置窗口已隐藏"
} else {
  Write-Host "✗ 点击无效：设置窗口仍可见（屏幕点 $($screenX),$($screenY)，客户区 $($btnClientX),$($btnClientY)）"
}

Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-Item $dataDir -Recurse -Force -ErrorAction SilentlyContinue
