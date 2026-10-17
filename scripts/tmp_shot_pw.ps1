# NOTE: 按脚本自身位置推导仓库根，不硬编码绝对路径。
param(
  [int]$WaitSec = 5,
  [string]$Out = ''
)
$ErrorActionPreference = 'Continue'
if (-not $Out) { $Out = (Join-Path (Split-Path $PSScriptRoot -Parent) 'shot-client.png') }
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 600

$exe = "$RepoRoot\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe"
$ShotData = "$RepoRoot\.shotdata"
$daemon = Start-Process -FilePath $exe -ArgumentList '--data-dir', $ShotData, '--no-capture' -PassThru `
  -RedirectStandardOutput "$RepoRoot\shot.log" -RedirectStandardError "$RepoRoot\shot.err"
Start-Sleep -Seconds 2
$ui = Start-Process -FilePath $exe -ArgumentList '--data-dir', $ShotData, '--ui' -PassThru `
  -RedirectStandardOutput "$RepoRoot\shot-ui.log" -RedirectStandardError "$RepoRoot\shot-ui.err"
Start-Sleep -Seconds $WaitSec

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class PW {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  public static List<IntPtr> All() {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Cls(IntPtr h) { var sb = new StringBuilder(256); GetClassNameW(h, sb, 256); return sb.ToString(); }
}
'@
Add-Type -AssemblyName System.Drawing

$main = [IntPtr]::Zero
$bestArea = 0
foreach ($h in [PW]::All()) {
  if ([PW]::Cls($h) -eq 'ModularClipboardWindow') {
    $rr = New-Object PW+RECT
    [void][PW]::GetWindowRect($h, [ref]$rr)
    $area = ($rr.R - $rr.L) * ($rr.B - $rr.T)
    Write-Output ("候选窗口 h={0} rect={1},{2},{3},{4} area={5}" -f $h, $rr.L, $rr.T, $rr.R, $rr.B, $area)
    if ($area -gt $bestArea) { $bestArea = $area; $main = $h }
  }
}
if ($main -eq [IntPtr]::Zero) {
  Write-Output "找不到主窗口"
  Get-Content "$RepoRoot\shot-ui.err" -Tail 10 -ErrorAction SilentlyContinue
  Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
  Stop-Process -Id $ui.Id -Force -ErrorAction SilentlyContinue
  exit 1
}

$r = New-Object PW+RECT
[void][PW]::GetWindowRect($main, [ref]$r)
$w = $r.R - $r.L; $hgt = $r.B - $r.T
$dpi = [PW]::GetDpiForWindow($main)
$scale = [double]$dpi / 96.0
$pw = [int][Math]::Round($w * $scale); $ph = [int][Math]::Round($hgt * $scale)
Write-Output "窗口 ${w}x${hgt} dpi=$dpi scale=$scale => bitmap ${pw}x${ph}"

$bmp = New-Object System.Drawing.Bitmap($pw, $ph)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$dc = $g.GetHdc()
$ok = [PW]::PrintWindow($main, $dc, 2)  # PW_RENDERFULLCONTENT
$g.ReleaseHdc($dc)
$g.Dispose()
if (-not $ok) { Write-Output "PrintWindow 失败" }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "已保存 $Out"

Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $ui.Id -Force -ErrorAction SilentlyContinue
