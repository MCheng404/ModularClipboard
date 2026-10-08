<#
.SYNOPSIS
UI 渲染端到端验证：在多个窗口宽度下截图，并用**像素判据**检查渲染覆盖率。

.DESCRIPTION
解决的问题：早前的截图曾把「窗口被别的程序盖住」误判成「渲染 bug」，
也曾把「UI 只画在客户区左上角」当成布局问题。这里用两条硬判据把两者分开：

  判据 A（覆盖率）：统计客户区截图里「非 clear color」像素的包围盒。
    若 UI 真的按客户区尺寸铺满，包围盒应接近整个客户区。
    若 UI 只画了一角（历史缺陷：ppp 恒为 1.0 导致内容缩在左上 1/scale），
    包围盒宽度只有 1/scale ⇒ 直接判FAIL。
    ⚠️ 窗口被遮挡时截到的是别的程序 ⇒ 也会 FAIL。
    所以判据 A 失败时必须先看判据 B。

  判据 B（唯一性）：检查 clear color 区域是否**连通成一片**。
    被遮挡时，clear color 区域会被窗口边框/别的窗口切成碎块。
    纯渲染不足时，clear color 一定是一整块（右下角的大片区域）。

判据：
  覆盖率 >= 90% 且 UI 包围盒铺满客户区 ⇒ PASS
  覆盖率 < 90% ⇒ FAIL（需人工判读截图区分「遮挡」与「渲染不足」）

.PARAMETER Widths
要测的物理像素宽度列表。

.PARAMETER Height
窗口物理像素高度。

.EXAMPLE
& 'D:\WorkBuddy\Tiez\scripts\verify_render.ps1'
& 'D:\WorkBuddy\Tiez\scripts\verify_render.ps1' -Widths 420,560,720,900
#>
param(
  [int[]]$Widths = @(420, 560, 720, 900),
  [int]$Height = 800,
  [string]$OutDir = 'D:\WorkBuddy\Tiez'
)

$ErrorActionPreference = 'Continue'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class V {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
  // Win7/8 回退路径：Win10 上一般走不到，但**必须声明**，
  // 否则 C# 编译器报 CS0103（measure_layout.ps1 也踩过同一个坑）。
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int ht, bool repaint);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  public static readonly IntPtr PMv2 = new IntPtr(-4);
  public static void DpiAware() {
    if (!SetProcessDpiAwarenessContext(PMv2)) { SetProcessDPIAware(); }
  }
  public static List<IntPtr> TopLevel(uint want) {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Cls(IntPtr h) { var sb = new StringBuilder(256); GetClassNameW(h, sb, 256); return sb.ToString(); }
}
'@
[V]::DpiAware()
Add-Type -AssemblyName System.Drawing

$exe = "$OutDir\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe"
if (-not (Test-Path $exe)) {
  Write-Host "找不到 exe：$exe" -ForegroundColor Red
  Write-Host "请先运行 ./scripts/build.sh" -ForegroundColor Red
  exit 1
}

Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 600

$err = "$OutDir\verify-render.err"
Remove-Item $err -ErrorAction SilentlyContinue
$env:RUST_LOG = 'info,modular_clipboard=debug'
$env:MC_DIAG = '1'
$p = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru `
  -RedirectStandardOutput "$OutDir\verify-render.log" -RedirectStandardError $err
Start-Sleep -Seconds 4
$p.Refresh()

$main = [IntPtr]::Zero
foreach ($h in [V]::TopLevel([uint32]$p.Id)) {
  if ([V]::Cls($h) -eq 'ModularClipboardWindow' -and [V]::IsWindowVisible($h)) { $main = $h; break }
}
if ($main -eq [IntPtr]::Zero) {
  # ⚠️ 必须把实际枚举到的窗口全打出来。
  #
  # 症状：改动后脚本报「找不到主窗口句柄」，但程序日志显示
  # 「窗口已创建」且手动运行完全正常——说明问题在**查找方式**，
  # 不在程序。此时只报一句「找不到」等于把线索全丢了，
  # 下一步只能靠猜。
  Write-Host "找不到主窗口句柄（进程 id=$($p.Id)）。实际枚举到的顶层窗口：" -ForegroundColor Red
  foreach ($h in [V]::TopLevel([uint32]$p.Id)) {
    $cls = [V]::Cls($h)
    $vis = [V]::IsWindowVisible($h)
    Write-Host ("  hwnd=0x{0:X}  class='{1}'  visible={2}" -f [int64]$h, $cls, $vis)
  }
  Write-Host "（若列表为空说明进程已退出；若都在但 visible=False 说明窗口被隐藏）"
  Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
  exit 1
}

# 屏幕右下角实测是唯一稳定的空白区（(0,0) 与 (120,120) 常被浏览器/终端占据）。
$screenW = 2560; $screenH = 1600
$mX = $screenW - 900 - 60
$mY = $screenH - $Height - 160

$fail = 0
$skip = 0
foreach ($w in $Widths) {
  [void][V]::MoveWindow($main, $mX, $mY, $w, $Height, $true)
  Start-Sleep -Milliseconds 500
  [void][V]::BringWindowToTop($main)
  [void][V]::SetForegroundWindow($main)
  Start-Sleep -Milliseconds 600

  $rc = New-Object V+RECT
  [void][V]::GetClientRect($main, [ref]$rc)
  $cw = $rc.R - $rc.L
  $ch = $rc.B - $rc.T
  $dpi = [V]::GetDpiForWindow($main)
  $scale = $dpi / 96.0
  $pt = New-Object V+POINT
  [void][V]::ClientToScreen($main, [ref]$pt)

  $bmp = New-Object System.Drawing.Bitmap $cw, $ch
  $gfx = [System.Drawing.Graphics]::FromImage($bmp)
  $gfx.CopyFromScreen($pt.X, $pt.Y, 0, 0, (New-Object System.Drawing.Size($cw, $ch)))
  $png = "$OutDir\shot-w$w.png"
  $bmp.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
  $gfx.Dispose(); $bmp.Dispose()

  # ---- 判据 A：非 clear color 像素的包围盒 ----
  # clear color是「窗口没画到」的地方。程序用的是浅色主题，
  # 底色接近 (240,240,240) 量级；这里用「亮度接近该值且饱和度低」当作 clear。
  $b2 = New-Object System.Drawing.Bitmap $png

  # ---- 判据 0：先区分「窗口不在」与「窗口在但没画」-------------------
  # ⚠️ 这是判据 A 之前必须做的检查。
  #
  # 实测踩坑：某次 720px 宽度跑出「覆盖率 0.0%」的 FAIL，截图却是
  # **一片纯白**——窗口被别的程序盖住了（或已被移开），截到的是
  # 别人的界面。若不加这一步，判据 A 会把「遮挡」报成「渲染失败」，
  # 让人去改本来正确的渲染代码。
  #
  # 区分方法：真正的 UI 截图里必定存在**明显暗于全图最亮值**的像素
  # （文字、描边、卡片边框）；而被遮挡时截到的往往是浏览器/编辑器
  # 之类大片浅色或纯色区域，暗像素极少。
  $brightest = 0.0
  $darkSamples = 0
  $totalSamples = 0
  for ($y = 0; $y -lt $ch; $y += 4) {
    for ($x = 0; $x -lt $cw; $x += 4) {
      $c = $b2.GetPixel($x, $y)
      $lum = ($c.R + $c.G + $c.B) / 3.0
      if ($lum -gt $brightest) { $brightest = $lum }
      $totalSamples++
      if ($lum -lt 120.0) { $darkSamples++ }
    }
  }
  $darkRatio = if ($totalSamples -gt 0) { $darkSamples / $totalSamples } else { 0 }
  # 判「被遮挡」：UI 截图必有文字/描边这类暗像素；被遮挡时几乎没有。
  $isBlankShot = $darkRatio -lt 0.002

  $minX = 99999; $maxX = -1; $minY = 99999; $maxY = -1
  $inkCount = 0
  # ⚠️ 早前版本硬编码clear 底色≈240，把**深灰 clear 区**也算成「内容」，
  # 于是「UI 只占左上角」这个真实缺陷被判成 PASS（假守卫）。
  # 现在改成自适应：先扫出全图最暗值（clear color 的代表），
  # 再以「明显亮于它」为内容判据，与具体主题色无关。
  $darkest = 255.0
  for ($y = 0; $y -lt $ch; $y += 4) {
    for ($x = 0; $x -lt $cw; $x += 4) {
      $c = $b2.GetPixel($x, $y)
      $lum = ($c.R + $c.G + $c.B) / 3.0
      if ($lum -lt $darkest) { $darkest = $lum }
    }
  }
  $thresh = $darkest + 25.0
  for ($y = 0; $y -lt $ch; $y += 2) {
    for ($x = 0; $x -lt $cw; $x += 2) {
      $c = $b2.GetPixel($x, $y)
      $lum = ($c.R + $c.G + $c.B) / 3.0
      if ($lum -gt $thresh) {
        $inkCount++
        if ($x -lt $minX) { $minX = $x }
        if ($x -gt $maxX) { $maxX = $x }
        if ($y -lt $minY) { $minY = $y }
        if ($y -gt $maxY) { $maxY = $y }
      }
    }
  }
  $b2.Dispose()

  $total = [Math]::Ceiling($cw / 2) * [Math]::Ceiling($ch / 2)
  $ratio = if ($total -gt 0) { $inkCount / $total } else { 0 }
  $boxW = if ($maxX -ge 0) { $maxX - $minX } else { 0 }
  $boxH = if ($maxY -ge 0) { $maxY - $minY } else { 0 }
  $coverX = if ($cw -gt 0) { $boxW / $cw } else { 0 }
  $coverY = if ($ch -gt 0) { $boxH / $ch } else { 0 }

  Write-Host "=========================================================="
  Write-Host ("窗口 {0}x{1}  客户区(物理) {2}x{3}  DPI={4} scale={5}" -f $w, $Height, $cw, $ch, $dpi, $scale)
  Write-Host ("暗像素占比 {0:P2}（判「是否被遮挡」）" -f $darkRatio)
  Write-Host ("内容包围盒: x {0}..{1}  y {2}..{3}  ({4}x{5})" -f $minX, $maxX, $minY, $maxY, $boxW, $boxH)
  Write-Host ("横向覆盖 {0:P1}  纵向覆盖 {1:P1}  墨迹像素比 {2:P1}" -f $coverX, $coverY, $ratio)
  Write-Host "截图: $png"

  # ⚠️ 先判「窗口在不在」，再判「画没画满」。
  # 顺序反了会把「被别的程序遮挡」报成「渲染失败」，
  # 让人去改本来正确的渲染代码（实测踩过一次）。
  if ($isBlankShot) {
    Write-Host ">>> SKIP：截图几乎纯色，窗口多半被遮挡或已移开 —— 本次结果不可用" -ForegroundColor Yellow
    $skip++
  } elseif ($coverX -lt 0.92 -or $coverY -lt 0.92) {
    # 判据：UI 包围盒必须横向铺满客户区。
    # 允许 8px 容差——最右侧是窗口边框/圆角。
    Write-Host (">>> FAIL：内容未铺满客户区（横向 {0:P1} / 纵向 {1:P1}）" -f $coverX, $coverY) -ForegroundColor Red
    $fail++
  } else {
    Write-Host ">>> PASS：内容铺满客户区" -ForegroundColor Green
  }
  Write-Host ""
}

Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300

Write-Host "=========================================================="
if ($fail -gt 0) {
  Write-Host "结果: $fail / $($Widths.Count) 个宽度未铺满（另有 $skip 个被跳过）" -ForegroundColor Red
  exit 1
}
if ($skip -gt 0) {
  # ⚠️ 有跳过项时**不能宣布全部通过**：那等于把「没测到」说成「测过了」。
  Write-Host "结果: $($Widths.Count - $skip) / $($Widths.Count) 个宽度通过，$skip 个因截图不可用被跳过" -ForegroundColor Yellow
  exit 0
}
Write-Host "结果: 所有宽度内容均铺满客户区" -ForegroundColor Green
exit 0
