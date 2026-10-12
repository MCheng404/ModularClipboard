# 临时验证：白色最大化背景 + 启动应用 + 截图（单次进程内完成，便于对照）。
param([string]$OutPath = "D:\modular-clipboard\target\ab_acrylic_white.png")

$ErrorActionPreference = 'Continue'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

# ---- 1. 白色背景 ----
$form = New-Object System.Windows.Forms.Form
$form.BackColor = [System.Drawing.Color]::White
$form.FormBorderStyle = 'None'
# 用字符串赋值而不是枚举类型：本会话里 `[System.Windows.Forms.WindowState]`
# 这类嵌套枚举在 Add-Type 之后仍可能解析不到（实测报 TypeNotFound）。
$form.WindowState = 'Maximized'
$form.Bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$form.TopMost = $false
$form.Show()
Start-Sleep -Milliseconds 400

# ---- 2. DPI 感知 + 启动应用 ----
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CapDpi3 {
  [DllImport("user32.dll")]
  public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
}
'@ | Out-Null
[void][CapDpi3]::SetProcessDpiAwarenessContext([IntPtr](-4))

$exe = 'D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
$p = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru
Start-Sleep -Seconds 5

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class WFind3 {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  public static IntPtr Find(uint want) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p == want) {
        var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256);
        if (sb.ToString() == "ModularClipboard") { found = h; return false; }
      }
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
'@ | Out-Null

$h = [WFind3]::Find([uint32]$p.Id)
if ($h -ne [IntPtr]::Zero) {
  $r = New-Object WFind3+RECT
  [void][WFind3]::GetWindowRect($h, [ref]$r)
  $w = $r.R - $r.L; $ht = $r.B - $r.T
  Write-Output "主窗口 rect=($($r.L),$($r.T)) ${w}x${ht}"
  $bmp = New-Object System.Drawing.Bitmap $w, $ht
  $gfx = [System.Drawing.Graphics]::FromImage($bmp)
  $gfx.CopyFromScreen($r.L, $r.T, 0, 0, (New-Object System.Drawing.Size($w, $ht)))
  $bmp.Save($OutPath, [System.Drawing.Imaging.ImageFormat]::Png)
  $gfx.Dispose(); $bmp.Dispose()
  Write-Output "截图已存: $OutPath"
} else {
  Write-Output "未找到主窗口"
}
Start-Sleep -Seconds 3
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
$form.Close(); $form.Dispose()
