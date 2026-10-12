# 临时验证脚本：显示一个**最大化白色窗口**当背景，供磨砂捕获做亮度对照。
# 应用是 always_on_top，会盖在它上面。
param([int]$Seconds = 25)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$form = New-Object System.Windows.Forms.Form
$form.BackColor = [System.Drawing.Color]::White
$form.FormBorderStyle = [System.Windows.Forms.FormBorderStyle]::None
$form.WindowState = [System.Windows.Forms.WindowState]::Maximized
$form.StartPosition = [System.Windows.Forms.FormStartPosition]::Manual
$form.Location = New-Object System.Drawing.Point(0, 0)
$form.TopMost = $false
$form.Show()

$sw = [System.Diagnostics.Stopwatch]::StartNew()
while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 100
}
$form.Close()
$form.Dispose()
