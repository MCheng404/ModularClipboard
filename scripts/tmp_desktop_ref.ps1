# 临时验证脚本：抓取**指定屏幕矩形**（不启动应用），用于取得「窗口身后桌面」的亮度基准。
param([int]$X = 1800, [int]$Y = 225, [int]$W = 520, [int]$H = 800,
      [string]$OutPath = "D:\modular-clipboard\target\desktop_ref.png")

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CapDpi2 {
  [DllImport("user32.dll")]
  public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
}
'@ | Out-Null
[void][CapDpi2]::SetProcessDpiAwarenessContext([IntPtr](-4))

$bmp = New-Object System.Drawing.Bitmap $W, $H
$gfx = [System.Drawing.Graphics]::FromImage($bmp)
$gfx.CopyFromScreen($X, $Y, 0, 0, (New-Object System.Drawing.Size($W, $H)))
$bmp.Save($OutPath, [System.Drawing.Imaging.ImageFormat]::Png)
$gfx.Dispose(); $bmp.Dispose()
Write-Output "桌面基准已存: $OutPath ($X,$Y ${W}x${H})"
