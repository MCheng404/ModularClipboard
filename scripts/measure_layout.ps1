<#
.SYNOPSIS
量出真实布局数字：三段宽度之和 vs 可用宽度。

.DESCRIPTION
启动真实 exe，读取客户区尺寸与 DPI，然后**按 layout.rs 里的实际公式**
复算一遍降级与分配，检查「三段之和 <= 可用宽度」这条不变式。

公式与 crates/modular-clipboard-ui/src/layout.rs 一一对应：
  MIN_HISTORY_WIDTH / MIN_PINNED_WIDTH / MIN_DETAIL_WIDTH / RAIL_COLLAPSED_WIDTH
  COLLAPSED_HANDLE_WIDTH / SPLITTER_WIDTH
  Tier::min_required_width -> tier_for -> allocate

脚本里的常量是从 layout.rs 手抄的，**改了 layout.rs 必须同步改这里**。
`layout.rs` 里的 cargo 测试做同样的事且不会漂移，
本脚本的价值在于读真实窗口尺寸 + 不依赖 cargo。

.PARAMETER Widths
要测的**物理**像素宽度列表。默认覆盖 420/600/900 三个代表值。

.EXAMPLE
& '$PSScriptRoot\measure_layout.ps1'
& '$PSScriptRoot\measure_layout.ps1' -Widths 420,600,900,1200
#>
param(
  [int[]]$Widths = @(420, 600, 900),
  # 保持窗口高度，只改宽度。
  [int]$Height = 560,
  [switch]$SkipMove
)

# ⚠️ 由 scripts/fix_hardcoded_paths.py 插入：按**脚本自身位置**推导仓库根，
# 不再硬编码绝对路径。项目目录改名 / 搬走后，验证脚本依然能用。
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path


$ErrorActionPreference = 'Continue'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class M {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  // ⚠️ 量测进程必须自己声明 DPI 感知，否则 GetClientRect / MoveWindow
  // 拿到的是**虚拟化坐标**（被系统按 1/scale 缩小）。
  // 实测踩坑：exe 客户区真实是 420x560 物理像素、scale=1.5，
  // 而未声明感知的 PowerShell 读出来是 280x373 —— 正好差一个 1.5 倍，
  // 于是「窗口默认多窄」这个结论整个是错的。
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int ht, bool repaint);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
  public static readonly IntPtr PER_MONITOR_AWARE_V2 = new IntPtr(-4);
  public static void MakeDpiAware() {
    if (!SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)) {
      SetProcessDPIAware();   // Win7/8 回退；Win10 上一般不会走到
    }
  }
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  public static List<IntPtr> TopLevel(uint want) {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Cls(IntPtr h) { var sb = new StringBuilder(256); GetClassNameW(h, sb, 256); return sb.ToString(); }
}
'@
# 必须在任何窗口 API 调用之前声明感知——放在这里而不是后面，
# 因为 host 已加载时重复设置会失败（返回 false），但那时坐标已经虚拟化了。
[void][M]::MakeDpiAware()

# ---- 与 layout.rs 对应的常量（改动必须两边同步）----
$SPL           = 6.0    # SPLITTER_WIDTH
$HISTORY_MIN   = 160.0  # MIN_HISTORY_WIDTH
$PINNED_MIN    = 120.0  # MIN_PINNED_WIDTH
$DETAIL_MIN    = 180.0  # MIN_DETAIL_WIDTH
$RAIL_EXP_MIN  = 88.0   # MIN_RAIL_EXPANDED_WIDTH
$RAIL_COLLAPSED= 28.0   # RAIL_COLLAPSED_WIDTH
$HANDLE        = 24.0   # COLLAPSED_HANDLE_WIDTH
$LEFT_FRAC     = 0.28   # LEFT_FRACTION
$RIGHT_FRAC    = 0.28   # RIGHT_FRACTION

# Tier 顺序（与 layout.rs 的 Tier::ALL 一致）
$tierName = @('三栏全展开','详情转浮动','置顶折叠','侧栏隐藏','仅历史列表')

# 默认布局（layout.rs::default_placement）：
#   Pinned -> Left  (展开)
#   History-> Center(展开)
#   Detail -> Right (折叠)
#   Rail   -> Right (折叠)
# 每项：名称 / 槽位(0=Left,1=Center,2=Right) / 保底宽 min / 舒适宽 comfy / 折叠宽 coll
# ⚠️ comfy = Panel::default_width。侧栏是 96（展开态舒适宽），
#    **不是**折叠窄条 28——它的默认放置才是折叠的。
$panels = @(
  @{ n='Pinned';  slot=0; min=$PINNED_MIN;   comfy=200.0; coll=$HANDLE         },
  @{ n='History'; slot=1; min=$HISTORY_MIN;  comfy=320.0; coll=$HANDLE         },
  @{ n='Detail';  slot=2; min=$DETAIL_MIN;   comfy=240.0; coll=$HANDLE         },
  @{ n='Rail';    slot=2; min=$RAIL_EXP_MIN; comfy=96.0;  coll=$RAIL_COLLAPSED }
)

<#
 复现 Tier::visibility。返回 'Docked' / 'Collapsed' / 'Hidden'。
 - 折叠默认态：Detail 与 Rail 本来就是 collapsed。
 - 降级优先于用户折叠：降级动作不受用户折叠阻挡（见 layout.rs 注释）。
#>
function Get-Visibility([int]$tier, $p) {
  if ($tier -eq 0) {
    if ($p.n -eq 'Detail') { return 'Collapsed' }   # 默认折叠
    if ($p.n -eq 'Rail')   { return 'Collapsed' }   # 默认折叠
    return 'Docked'
  }
  if ($p.n -eq 'History') { return 'Docked' }      # 主区，永不降级
  if ($p.n -eq 'Detail')  { return 'Hidden'  }      # >=1 转浮层
  if ($p.n -eq 'Pinned') {
    if ($tier -le 1) { return 'Docked' }
    if ($tier -le 3) { return 'Collapsed' }
    return 'Hidden'
  }
  if ($p.n -eq 'Rail') {
    if ($tier -le 2) { return 'Collapsed' }         # 默认就是折叠
    return 'Hidden'
  }
  return 'Docked'
}

<# Tier::min_required_width：各段保底 + 段内分隔条。#>
function Get-MinRequired([int]$tier) {
  $total = 0.0
  $spl = 0
  for ($s = 0; $s -lt 3; $s++) {
    $seg = 0.0
    $occ = 0
    foreach ($p in $panels) {
      if ($p.slot -ne $s) { continue }
      $v = Get-Visibility $tier $p
      if ($v -eq 'Docked') { $seg += $p.min; $occ++ }
      elseif ($v -eq 'Collapsed') { $seg += $p.coll; if ($p.coll -gt 0) { $occ++ } }
    }
    if ($occ -gt 1) { $spl += ($occ - 1) }
    $total += $seg
  }
  return $total + $spl * $SPL
}

<# tier_for：从宽到窄找第一个装得下的档。#>
function Get-Tier([double]$bodyW) {
  for ($t = 0; $t -lt $tierName.Count; $t++) {
    if ((Get-MinRequired $t) -le $bodyW) { return $t }
  }
  return ($tierName.Count - 1)
}

<#
 split_within：段内各成员占位宽度（与 layout.rs 同一算法）。
 折叠成员占 collapsed_width；展开成员先拿 min_width 保底，
 富余按「相对缺口最大的优先」注水（water-filling）。
 返回 @成员名 -> 宽度。
#>
function Get-SplitWithin([double]$total, $members, [int]$tier) {
  $result = @{}
  if ($members.Count -eq 0) { return $result }

  # 段内分隔条：占位成员数 - 1
  $occ = 0
  $open = @()
  $fixedSum = 0.0
  foreach ($m in $members) {
    $v = Get-Visibility $tier $m
    if ($v -eq 'Docked') {
      $open += $m.n
      $result[$m.n] = 0.0
      $occ++
    } else {
      $w = if ($v -eq 'Collapsed') { $m.coll } else { 0.0 }
      $result[$m.n] = $w
      $fixedSum += $w
      if ($w -gt 0) { $occ++ }
    }
  }
  $innerSpl = if ($occ -gt 1) { ($occ - 1) * $SPL } else { 0.0 }
  $budget = [Math]::Max($total - $innerSpl, 0.0)

  if ($open.Count -eq 0) { return $result }

  $minOf = @{}
  $comfyOf = @{}
  foreach ($n in $open) {
    $m = $members | Where-Object { $_.n -eq $n }
    $minOf[$n] = $m.min
    $comfyOf[$n] = $m.comfy
  }
  # 先给保底
  foreach ($n in $open) { $result[$n] = [Math]::Min($minOf[$n], $budget) }
  $sum = 0.0
  foreach ($k in $result.Keys) { $sum += $result[$k] }
  if ($sum -ge $budget) {
    if ($sum -gt 0) {
      $f = $budget / $sum
      foreach ($k in @($result.Keys)) { $result[$k] = $result[$k] * $f }
    }
    return $result
  }
  # 注水：每轮给「相对缺口 / 舒适宽」最大的成员
  $surplus = $budget - $sum
  for ($iter = 0; $iter -lt 64 -and $surplus -gt 0; $iter++) {
    $best = $null; $bestGap = 0.0
    foreach ($n in $open) {
      $comfy = $comfyOf[$n]
      if ($comfy -le 0) { continue }
      $gap = ($comfy - $result[$n]) / $comfy
      if ($gap -le 0) { continue }
      if ($gap -gt $bestGap) { $bestGap = $gap; $best = $n }
    }
    if ($null -eq $best) { break }
    $room = $comfyOf[$best] - $result[$best]
    $step = [Math]::Min($room, $surplus)
    if ($step -le 0) { break }
    $result[$best] += $step
    $surplus -= $step
  }
  # 富余按舒适宽比例分
  if ($surplus -gt 0) {
    $totalComfy = 0.0
    foreach ($n in $open) { $totalComfy += $comfyOf[$n] }
    if ($totalComfy -gt 0) {
      foreach ($n in $open) { $result[$n] += $surplus * ($comfyOf[$n] / $totalComfy) }
    }
  }
  return $result
}

<#
 allocate + split_within：算出三段与每个模块的实际矩形宽度。
 返回 @{ left; center; right; widths=@{name=width}; splitters }
#>
function Get-Layout([double]$bodyW) {
  $tier = Get-Tier $bodyW

  # --- 分隔条总数：段内相邻对 + 段间（左|中、中|右）---
  $occOf = {
    param($s)
    $c = 0
    foreach ($pp in $panels) {
      if ($pp.slot -ne $s) { continue }
      $v = Get-Visibility $tier $pp
      if ($v -eq 'Docked') { $c++ }
      elseif ($v -eq 'Collapsed' -and $pp.coll -gt 0) { $c++ }
    }
    return $c
  }
  $nSpl = 0
  for ($s = 0; $s -lt 3; $s++) {
    $occ = & $occOf $s
    if ($occ -gt 1) { $nSpl += ($occ - 1) }
  }
  $occL = & $occOf 0; $occC = & $occOf 1; $occR = & $occOf 2
  # 段间分隔条：相邻两段都占位时才有
  if ($occL -gt 0 -and $occC -gt 0) { $nSpl++ }
  if ($occC -gt 0 -and $occR -gt 0) { $nSpl++ }
  $usable = $bodyW - $nSpl * $SPL

  # --- segment_floor：成员保底之和 + 段内分隔条（与 layout.rs 同口径）---
  # ⚠️ 分隔条必须算进保底：否则「这一段至少要58」的判断少算6，
  #    分配时`split_within` 再扣一次就把折叠把手压窄了。
  $floor = @(0.0, 0.0, 0.0)
  for ($s = 0; $s -lt 3; $s++) {
    $f = 0.0; $occ = 0
    foreach ($p in $panels) {
      if ($p.slot -ne $s) { continue }
      $v = Get-Visibility $tier $p
      if ($v -eq 'Docked') { $f += $p.min; $occ++ }
      elseif ($v -eq 'Collapsed') { $f += $p.coll; if ($p.coll -gt 0) { $occ++ } }
    }
    if ($occ -gt 1) { $f += ($occ - 1) * $SPL }
    $floor[$s] = $f
  }
  # --- slot_desired：floor + 各展开成员从保底到默认宽的差额 ---
  $desired = @(0.0, 0.0, 0.0)
  for ($s = 0; $s -lt 3; $s++) {
    $d = $floor[$s]
    foreach ($p in $panels) {
      if ($p.slot -ne $s) { continue }
      if ((Get-Visibility $tier $p) -eq 'Docked') { $d += ($p.comfy - $p.min) }
    }
    $desired[$s] = $d
  }

  # --- allocate：先给三段保底，剩余额度按注水分给左右两段 ---
  # ⚠️ 必须与 layout.rs 的 allocate 同一套顺序：先预留保底、再分富余。
  #    早前版本「左段先按期望宽取、取完给右段剩下的」会让左段吃掉右段的保底。
  $sideBudget = [Math]::Max($usable - $floor[1], 0.0)
  $left = 0.0; $right = 0.0
  if (($floor[0] + $floor[2]) -le $sideBudget) {
    $l = $floor[0]; $r = $floor[2]
    $surplus = $sideBudget - $floor[0] - $floor[2]
    $lSlack = [Math]::Max($desired[0] - $floor[0], 0.0)
    $rSlack = [Math]::Max($desired[2] - $floor[2], 0.0)
    for ($iter = 0; $iter -lt 64 -and $surplus -gt 0; $iter++) {
      if ($lSlack -le 0 -and $rSlack -le 0) { break }
      if ($lSlack -ge $rSlack) {
        $step = [Math]::Min($lSlack, $surplus); $l += $step; $lSlack -= $step; $surplus -= $step
      } else {
        $step = [Math]::Min($rSlack, $surplus); $r += $step; $rSlack -= $step; $surplus -= $step
      }
    }
    # 富余剩余全留给中央段（中央段吃 usable 余额），不再塞给两侧
    $left = $l; $right = $r
  } else {
    $sum = $floor[0] + $floor[2]
    $k = if ($sum -gt 0) { $sideBudget / $sum } else { 0.0 }
    $left = $floor[0] * $k; $right = $floor[2] * $k
  }
  $center = [Math]::Max($usable - $left - $right, 0.0)

  # --- 每段内 split_within（与 layout.rs 同一算法）---
  $widths = @{}
  $segTotal = @($left, $center, $right)
  for ($s = 0; $s -lt 3; $s++) {
    $members = @($panels | Where-Object { $_.slot -eq $s })
    if ($members.Count -eq 0) { continue }
    $ws = Get-SplitWithin $segTotal[$s] $members $tier
    foreach ($k in $ws.Keys) { $widths[$k] = $ws[$k] }
  }

  return @{
    tier = $tier; usable = $usable
    left = $left; center = $center; right = $right
    splitters = $nSpl * $SPL
    widths = $widths
  }
}

$exe = '$RepoRoot\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
if (-not (Test-Path $exe)) {
  Write-Host "找不到 exe：$exe" -ForegroundColor Red
  Write-Host "请先运行 ./scripts/build.sh" -ForegroundColor Red
  exit 1
}

Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

$err = '$RepoRoot\measure-layout.err'
Remove-Item $err -ErrorAction SilentlyContinue
$env:RUST_LOG = 'warn'
$p = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru `
  -RedirectStandardOutput '$RepoRoot\measure-layout.log' -RedirectStandardError $err
Start-Sleep -Seconds 3
$p.Refresh()

$main = [IntPtr]::Zero
foreach ($h in [M]::TopLevel([uint32]$p.Id)) {
  if ([M]::Cls($h) -eq 'ModularClipboardWindow' -and [M]::IsWindowVisible($h)) { $main = $h; break }
}
if ($main -eq [IntPtr]::Zero) {
  Write-Host "找不到主窗口句柄（进程 id=$($p.Id)）" -ForegroundColor Red
  Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
  exit 1
}

$rc = New-Object M+RECT
[void][M]::GetClientRect($main, [ref]$rc)
$dpi = [M]::GetDpiForWindow($main)
$cw = $rc.R - $rc.L
$ch = $rc.B - $rc.T
$scale = $dpi / 96.0

Write-Host "=========================================================="
Write-Host "客户区(物理)     : ${cw} x ${ch}"
Write-Host "DPI /缩放: $dpi / $scale"
Write-Host ("客户区(逻辑点)   : {0:N1} x {1:N1}" -f ($cw / $scale), ($ch / $scale))
Write-Host "=========================================================="
Write-Host ""
Write-Host "--- 各降级档位的最小宽度需求（阈值由 layout.rs 公式算出）---"
for ($t = 0; $t -lt $tierName.Count; $t++) {
  Write-Host ("  宽 >= {0,7:N1}  {1}" -f (Get-MinRequired $t), $tierName[$t])
}
Write-Host ""

$fail = 0
foreach ($w in $Widths) {
  if (-not $SkipMove) {
    [void][M]::MoveWindow($main, 120, 120, $w, $Height, $true)
    Start-Sleep -Milliseconds 500
  }
  [void][M]::GetClientRect($main, [ref]$rc)
  $pw = $rc.R - $rc.L
  $lw = $pw / $scale

  Write-Host "=========================================================="
  Write-Host ("窗口物理宽度 {0}px  ->  客户区 {1}px  ->  逻辑宽 {2:N1}pt(缩放 {3})" -f $w, $pw, $lw, $scale)
  $L = Get-Layout $lw
  Write-Host ("降级档位: {0}（第 {1} 档）" -f $tierName[$L.tier], $L.tier)
  Write-Host ("usable（扣 {0:N1} 宽分隔条后）: {1:N1}" -f $L.splitters, $L.usable)
  Write-Host "三段宽度:"
  Write-Host ("  left(置顶)     : {0:N1}" -f $L.left)
  Write-Host ("  center(历史)   : {0:N1}" -f $L.center)
  Write-Host ("  right(详情+栏): {0:N1}" -f $L.right)
  Write-Host "各模块实际矩形宽度:"
  foreach ($pn in $panels) {
    $v = Get-Visibility $L.tier $pn
    $wd = if ($L.widths.ContainsKey($pn.n)) { $L.widths[$pn.n] } else { 0.0 }
    Write-Host ("  {0,-8} {1,-9} {2,7:N1}" -f $pn.n, $v, $wd)
  }
  $sum = $L.left + $L.center + $L.right + $L.splitters
  Write-Host ("三段+分隔条 合计  : {0:N1}" -f $sum)
  $over = $sum - $lw
  Write-Host ("超出可用宽度      : {0:N1}" -f $over)
  if ($over -gt 0.01) {
    Write-Host ">>> 溢出：三段之和超出可用宽度，矩形会互相重叠" -ForegroundColor Red
    $fail++
  } else {
    Write-Host ">>> 通过：不重叠、不溢出" -ForegroundColor Green
  }
  Write-Host ""
}

Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300
Remove-Item $err, '$RepoRoot\measure-layout.log' -ErrorAction SilentlyContinue

if ($fail -gt 0) {
  Write-Host "结果: $fail 个宽度仍溢出" -ForegroundColor Red
  exit 1
}
Write-Host "结果: 所有测试宽度均不重叠、不溢出" -ForegroundColor Green
exit 0