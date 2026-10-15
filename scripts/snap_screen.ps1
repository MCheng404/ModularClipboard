# 实机验证：抓整屏截图（用于核对弹窗位置 / Alpha 圆角 / 投影）
# 用法：powershell -File scripts/snap_screen.ps1 <输出路径>
param([string]$Out = "$PSScriptRoot\..\target\screen.png")
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bmp = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($vs.Left, $vs.Top, 0, 0, $bmp.Size)
$g.Dispose()
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Host "saved $Out ($($vs.Width)x$($vs.Height))"