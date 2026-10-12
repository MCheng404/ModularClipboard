# 临时验证脚本（本轮 backdrop 改造用，验证完可删）：
# 启动 debug exe → 等待渲染 → CopyFromScreen 抓客户区 → 存 PNG。
# CopyFromScreen 走 DWM 合成结果，能真实反映逐像素半透明的透出效果。
param([string]$OutPath = "D:\modular-clipboard\target\verify_translucent.png")

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class CapDpi {
  [DllImport("user32.dll")]
  public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
}
'@ | Out-Null
[void][CapDpi]::SetProcessDpiAwarenessContext([IntPtr](-4))

$exe = 'D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
if ($env:MC_NO_EXCLUDE) { Remove-Item Env:MC_NO_EXCLUDE }
if ($env:MC_TEST_NO_EXCLUDE) { $env:MC_NO_EXCLUDE = '1' }
$p = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru
Start-Sleep -Seconds 5

# 按标题找主窗口（此时进程应已建窗）
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class WFind {
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

$h = [WFind]::Find([uint32]$p.Id)
if ($h -eq [IntPtr]::Zero) { Write-Output "未找到主窗口"; Stop-Process -Id $p.Id -Force; exit 1 }
$r = New-Object WFind+RECT
[void][WFind]::GetWindowRect($h, [ref]$r)
$w = $r.R - $r.L; $ht = $r.B - $r.T
Write-Output "主窗口 $h rect=($($r.L),$($r.T)) ${w}x${ht}"

$bmp = New-Object System.Drawing.Bitmap $w, $ht
$gfx = [System.Drawing.Graphics]::FromImage($bmp)
$gfx.CopyFromScreen($r.L, $r.T, 0, 0, (New-Object System.Drawing.Size($w, $ht)))
$bmp.Save($OutPath, [System.Drawing.Imaging.ImageFormat]::Png)
$gfx.Dispose(); $bmp.Dispose()
Write-Output "截图已存: $OutPath"

# 让窗口多跑几秒再退出（观察稳定性）
Start-Sleep -Seconds 4
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
