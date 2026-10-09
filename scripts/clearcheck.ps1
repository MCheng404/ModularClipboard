param(
    [string]$Png = '$RepoRoot\shot-client.png'
)

# ⚠️ 由 scripts/fix_hardcoded_paths.py 插入：按**脚本自身位置**推导仓库根，
# 不再硬编码绝对路径。项目目录改名 / 搬走后，验证脚本依然能用。
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path


# 分析客户区截图：统计 clear color 像素占比，判断 UI 是否铺满客户区。
#
# ⚠️ 判据必须与视觉**同源**：直接读shot-client.png 的像素，
# 而不是复算布局数值——「数字判据与视觉判据不同源」是本项目吃过的亏
# （三段宽度之和 <= 可用宽度 数值通过，用户仍看到重影）。
#
# clear color = (25,25,31)，即渲染通道的 CLEAR 值。
# UI 若铺满客户区，则**最右一列与最下一行**都不应是纯 clear color。

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

if (-not (Test-Path $Png)) { throw "截图不存在: $Png" }
$bmp = New-Object System.Drawing.Bitmap $Png
$w = $bmp.Width; $h = $bmp.Height
Write-Host ("客户区截图: {0} x {1}px" -f $w, $h)

# clear color 判据：允许 ±2 的容差，避免 PNG 转换/alpha 带来的1~2 级误差
# 把真实背景误判成 clear color（那会让判据偏严，方向是安全的）。
$CLEAR_R = 25; $CLEAR_G = 25; $CLEAR_B = 31; $TOL = 2

function Test-Clear([System.Drawing.Color]$c) {
    [Math]::Abs([int]$c.R - $CLEAR_R) -le $TOL -and
    [Math]::Abs([int]$c.G - $CLEAR_G) -le $TOL -and
    [Math]::Abs([int]$c.B - $CLEAR_B) -le $TOL
}

# ---- 全图 clear 占比 ----
$clearCount = 0
for ($y = 0; $y -lt $h; $y++) {
    for ($x = 0; $x -lt $w; $x++) {
        if (Test-Clear $bmp.GetPixel($x, $y)) { $clearCount++ }
    }
}
$total = $w * $h
Write-Host ("全图 clear color 占比 : {0:N2}%  ({1}/{2})" -f (100.0*$clearCount/$total), $clearCount, $total)

# ---- 最右一列 / 最下一行（判据本体）----
# UI 铺满 ⇒ 这些像素**不应**是纯 clear color。
# 改前：右列与下行100% clear（实测应为 100%）。
$rightClear = 0
for ($y = 0; $y -lt $h; $y++) { if (Test-Clear $bmp.GetPixel($w-1, $y)) { $rightClear++ } }
$bottomClear = 0
for ($x = 0; $x -lt $w; $x++) { if (Test-Clear $bmp.GetPixel($x, $h-1)) { $bottomClear++ } }
Write-Host ("最右一列 clear 占比   : {0:N2}%  ({1}/{2})" -f (100.0*$rightClear/$h), $rightClear, $h)
Write-Host ("最下一行 clear 占比   : {0:N2}%  ({1}/{2})" -f (100.0*$bottomClear/$w), $bottomClear, $w)

# ---- 找出内容实际覆盖到的边界（诊断用）----
# 逐列找**最后一个**非 clear 像素（= 内容右边缘），逐行同理。
# ⚠️ 必须扫到「最后一个」而不是「第一个」：从左往右遇到的第一个非 clear
# 像素恒为x=0（最左列一定有内容），用它当右边缘会得出「空白 419px」的
# 荒谬结论——这正是判据失效的一种形态。
$maxX = -1
for ($x = $w - 1; $x -ge 0; $x--) {
    $hit = $false
    for ($y = 0; $y -lt $h; $y++) {
        if (-not (Test-Clear $bmp.GetPixel($x, $y))) { $hit = $true; break }
    }
    if ($hit) { $maxX = $x; break }
}
$maxY = -1
for ($y = $h - 1; $y -ge 0; $y--) {
    $hit = $false
    for ($x = 0; $x -lt $w; $x++) {
        if (-not (Test-Clear $bmp.GetPixel($x, $y))) { $hit = $true; break }
    }
    if ($hit) { $maxY = $y; break }
}
# 最左/最上边缘同理（取第一个非 clear）
$minX = -1
for ($x = 0; $x -lt $w; $x++) {
    for ($y = 0; $y -lt $h; $y++) {
        if (-not (Test-Clear $bmp.GetPixel($x, $y))) { $minX = $x; break }
    }
    if ($minX -ge 0) { break }
}
if ($maxX -ge 0 -and $maxY -ge 0) {
    Write-Host ("内容包围盒: 左x={0} 右 x={1}/{2}  下 y={3}/{4}" -f $minX, $maxX, ($w-1), $maxY, ($h-1))
    Write-Host ("右侧空白   : {0}px                底部空白   : {1}px" -f ($w-1-$maxX), ($h-1-$maxY))
} else {
    Write-Host "整张图都是 clear color —— UI 完全没画出来"
}

# ---- 底部一行的非clear 像素落在哪（判断残留空白是圆角还是真未绘制）----
if ($maxY -ge 0 -and $maxY -lt $h-1) {
    $bottomHits = @()
    for ($x = 0; $x -lt $w; $x++) {
        if (-not (Test-Clear $bmp.GetPixel($x, $h-1))) { $bottomHits += $x }
    }
    if ($bottomHits.Count -gt 0) {
        Write-Host ("最下一行非clear 像素 x 范围: {0}..{1}（共{2} 个，集中在两端=>圆角）" -f `
            $bottomHits[0], $bottomHits[-1], $bottomHits.Count)
    }
}

# ---- 结论（阈值判据，不能是「有没有非 clear 像素」）----
#
# ⚠️ 这里踩过一次判据失效：早先的判据是「最右列/最下一行**存在**非 clear
# 像素就算铺满」。但截图边缘总有零星非 clear 像素（窗口边框、resize 边框、
# 抗锯齿），于是**修前的 95.89% 也被判成 ✅** —— 与「数字过了但用户
# 看到重影」是同一种错误。判据必须是**占比**低于阈值。
Write-Host ""
Write-Host "=== 结论 ==="
$EDGE_MAX = 10.0   # 边缘 clear 占比超过 10% 即视为未铺满
$TOTAL_MAX = 20.0  # 全图 clear 占比超过 20% 即视为未铺满

$rightPct = 100.0 * $rightClear / $h
$bottomPct = 100.0 * $bottomClear / $w
$totalPct = 100.0 * $clearCount / $total

if ($rightPct -gt $EDGE_MAX -or $bottomPct -gt $EDGE_MAX -or $totalPct -gt $TOTAL_MAX) {
    Write-Host ("❌ 未铺满客户区：全图 clear {0:N2}%（阈值 {1}%）/ 最右列 {2:N2}%（阈值 {3}%）/ 最下行 {4:N2}%" -f `
        $totalPct, $TOTAL_MAX, $rightPct, $EDGE_MAX, $bottomPct) -ForegroundColor Red
    Write-Host "   典型成因：set_pixels_per_point 被漏设 ⇒ ppp=1.0，布局与换算差一个 scale 因子"
} else {
    Write-Host ("✅ 已铺满客户区：全图 clear {0:N2}%，最右列 {1:N2}%，最下行 {2:N2}%（均低于阈值）" -f `
        $totalPct, $rightPct, $bottomPct) -ForegroundColor Green
}

$bmp.Dispose()