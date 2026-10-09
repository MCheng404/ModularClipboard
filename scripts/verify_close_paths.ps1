
# ⚠️ 由 scripts/fix_hardcoded_paths.py 插入：按**脚本自身位置**推导仓库根，
# 不再硬编码绝对路径。项目目录改名 / 搬走后，验证脚本依然能用。
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

$ErrorActionPreference = 'Stop'
$exe = '$RepoRoot\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class K {
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

# 标签, 消息号, wParam
$cases = @(
  @{ Name='WM_CLOSE          (0x0010)'; Msg=0x0010; Wp=0x0000 },
  @{ Name='WM_SYSCOMMAND     (0x0112)'; Msg=0x0112; Wp=0xF060 },
  @{ Name='WM_SYSCOMMAND+Alt (0xF061)'; Msg=0x0112; Wp=0xF061 },
  @{ Name='WM_SYSCOMMAND Min (0xF020)'; Msg=0x0112; Wp=0xF020 }
)

foreach ($c in $cases) {
  Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 500
  $err = "$RepoRoot\sc-$($c.Msg.ToString('x'))-$($c.Wp.ToString('x')).err"
  $out = "$RepoRoot\sc.log"
  Remove-Item $err,$out -ErrorAction SilentlyContinue

  $env:RUST_LOG = 'info,modular_clipboard=debug'
  $p = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
  Start-Sleep -Seconds 3
  $p.Refresh()
  $main = [IntPtr]::Zero
  foreach ($h in [K]::TopLevel([uint32]$p.Id)) { if ([K]::Cls($h) -eq 'ModularClipboardWindow') { $main = $h; break } }
  $before = [K]::IsWindowVisible($main)

  $ok = [K]::PM($main, $c.Msg, [IntPtr]$c.Wp, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 1200
  $p.Refresh()
  $after = [K]::IsWindowVisible($main)
  $alive = -not $p.HasExited

  # 期望：收到关闭请求 -> 隐藏（SC_MINIMIZE 不该触发关闭）
  $expectHide = ($c.Wp -eq 0xF020) -eq $false
  $pass = if ($expectHide) { $ok -and $alive -and ($before -eq $true) -and ($after -eq $false) } else { $ok -and $alive -and ($after -eq $true) }
  $verdict = if ($pass) { 'PASS' } else { 'FAIL' }
  Write-Host "[$($c.Name)] $verdict  post=$ok visible: $before -> $after alive=$alive"
  $h2 = Get-Content $err -ErrorAction SilentlyContinue | Select-String '关闭请求'
  if ($h2) { Write-Host ("        日志: " + $h2[-1]) }

  Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 400
}
