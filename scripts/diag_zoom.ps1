# 诊断：把截图的指定区域裁剪并按最近邻放大，用于肉眼检查抗锯齿。
#
# 为什么需要它：抗锯齿的差别在 1x 下几乎看不出来（「看着还行」），
# 8x 放大后才会暴露成明显的硬台阶。判断「圆角/字形有没有锯齿」
# 只能靠放大看像素，靠肉眼看整体比例是不可靠的。
#
# 用法：
#   & scripts/diag_zoom.ps1 -Src shot.png -X 645 -Y 92 -W 55 -H 55 -Scale 8 -Out zoom.png
#
# CAUTION: 必须用最近邻（NearestNeighbor）放大。双线性会把台阶自己抹平，
# 那就等于把要观察的现象改掉了。
param(
    [Parameter(Mandatory = $true)][string]$Src,
    [Parameter(Mandatory = $true)][int]$X,
    [Parameter(Mandatory = $true)][int]$Y,
    [Parameter(Mandatory = $true)][int]$W,
    [Parameter(Mandatory = $true)][int]$H,
    [double]$Scale = 4,
    [Parameter(Mandatory = $true)][string]$Out
)
Add-Type -AssemblyName System.Drawing
$img = [System.Drawing.Image]::FromFile($Src)
$crop = New-Object System.Drawing.Bitmap($W, $H)
$g = [System.Drawing.Graphics]::FromImage($crop)
$dstRect = New-Object System.Drawing.Rectangle(0, 0, $W, $H)
$srcRect = New-Object System.Drawing.Rectangle($X, $Y, $W, $H)
$g.DrawImage($img, $dstRect, $srcRect, [System.Drawing.GraphicsUnit]::Pixel)
$g.Dispose()
$big = New-Object System.Drawing.Bitmap([int]($W * $Scale), [int]($H * $Scale))
$g2 = [System.Drawing.Graphics]::FromImage($big)
$g2.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
$g2.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::Half
$g2.DrawImage($crop, 0, 0, [int]($W * $Scale), [int]($H * $Scale))
$g2.Dispose()
$crop.Dispose()
$big.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$big.Dispose()
$img.Dispose()
Write-Output "saved $Out"
