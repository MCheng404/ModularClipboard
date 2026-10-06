$ErrorActionPreference = 'Stop'
$exe = 'D:\WorkBuddy\Tiez\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class U {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", SetLastError=true, EntryPoint="PostMessageW")] public static extern bool PM(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  public static List<IntPtr> TopLevel(uint want) {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Cls(IntPtr h) { var sb = new StringBuilder(256); GetClassNameW(h, sb, 256); return sb.ToString(); }
}
'@

# MAKELPARAM(low, high)
function LP([int]$lo, [int]$hi) { return [IntPtr](($hi -shl 16) -bor ($lo -band 0xFFFF)) }

Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

$err = 'D:\WorkBuddy\Tiez\ui-input.err'
$out = 'D:\WorkBuddy\Tiez\ui-input.log'
Remove-Item $err,$out -ErrorAction SilentlyContinue

$env:RUST_LOG = 'info,modular_clipboard=debug'
$p = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
Start-Sleep -Seconds 3
$p.Refresh()
$main = [IntPtr]::Zero
foreach ($h in [U]::TopLevel([uint32]$p.Id)) { if ([U]::Cls($h) -eq 'ModularClipboardWindow') { $main = $h; break } }
Write-Host "main=$main"

# 客户区尺寸（用于构造合法的鼠标坐标）
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class R {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
}
'@
$rc = New-Object R+RECT
[void][R]::GetClientRect($main, [ref]$rc)
$w = $rc.R - $rc.L; $ht = $rc.B - $rc.T
Write-Host "client=${w}x${ht}"

Write-Host "`n--- 投递 5 条鼠标移动到客户区中部 ---"
for ($i = 0; $i -lt 5; $i++) {
  $x = [int]($w/2 + $i*3); $y = [int]($ht/2 + $i*2)
  [void][U]::PM($main, 0x0200, [IntPtr]0, (LP $x $y))   # WM_MOUSEMOVE, wParam=0
  Start-Sleep -Milliseconds 60
}
Write-Host "已发 5条 WM_MOUSEMOVE"

Write-Host "`n--- 投递 1 次左键按下+抬起（客户区中部）---"
$x = [int]($w/2); $y = [int]($ht/2)
[void][U]::PM($main, 0x0201, [IntPtr]1, (LP $x $y))   # WM_LBUTTONDOWN, wParam=MK_LBUTTON
Start-Sleep -Milliseconds 80
[void][U]::PM($main, 0x0202, [IntPtr]0, (LP $x $y))   # WM_LBUTTONUP
Start-Sleep -Milliseconds 600

$p.Refresh()
Write-Host "alive=$(-not $p.HasExited) visible=$([U]::IsWindowVisible($main))"

Write-Host "`n--- 帧统计（确认循环活着且在响应输入）---"
$fs = Get-Content $err | Select-String '帧统计'
Write-Host "帧统计条数=$($fs.Count)"
$fs | Select-Object -Last 2

Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
Write-Host "`n=== panic /错误检查 ==="
$bad = Get-Content $err | Select-String 'panic|ERROR|error:' 
if ($bad) { $bad | Select-Object -Last 5 } else { Write-Host "  (无)" }
