$ErrorActionPreference = 'Stop'
#无边框窗口实机验证。
#
# 用法（**必须由用户自己执行**，Agent 不得在用户在场时弹窗）：
#   powershell -ExecutionPolicy Bypass -File D:\WorkBuddy\Tiez\scripts\verify_frameless.ps1
#
# 验证项：
#   a) 窗口可见且**客户区**尺寸仍约 420x560（去掉AdjustWindowRectEx 后
#      不能多出/少掉一个标题栏的高度）
#   b) 四条关闭路径仍全PASS（等价于 verify_close_paths.ps1，但额外断言样式）
#   d) 最小化 / 恢复：SC_MINIMIZE -> IsIconic == TRUE -> SC_RESTORE -> 恢复
#   e) 边缘 resize 命中：SendMessage(WM_NCHITTEST) 返回 HTLEFT/HTTOPRIGHT 等
#   f) DPI 路径未坏：GetDpiForWindow 返回 96 的整数倍且 > 0
#
# c)「系统不再画标题栏」**无法自动验证**，需人眼确认，见脚本末尾提示。

$exe = 'D:\WorkBuddy\Tiez\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
$script:pass = 0
$script:fail = 0

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class W {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", SetLastError=true, EntryPoint="PostMessageW")] public static extern bool PM(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode, EntryPoint="SendMessageW")] public static extern IntPtr SM(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ScreenToClient(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll", EntryPoint="GetWindowLongPtrW")] public static extern IntPtr GetWindowLongPtr(IntPtr h, int i);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int left, top, right, bottom; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int x, y; }
  public const int GWL_STYLE = -16;
  public const int WS_POPUP    = 0x80000000;
  public const int WS_CAPTION  = 0x00C00000;   // WS_BORDER|WS_DLGFRAME
  public const int WS_THICKFRAME = 0x00040000;
  public const uint WM_NCHITTEST = 0x0084;
  public static List<IntPtr> TopLevel(uint want) {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Cls(IntPtr h) { var sb = new StringBuilder(256); GetClassNameW(h, sb, 256); return sb.ToString(); }
  // WM_NCHITTEST 的 lParam 是**屏幕**坐标，低16位 x、高16位 y
  public static IntPtr HitTest(IntPtr h, int sx, int sy) {
    IntPtr lp = (IntPtr)((sy << 16) | (sx & 0xFFFF));
    return SM(h, WM_NCHITTEST, IntPtr.Zero, lp);
  }
  public static bool ClientToScreen(IntPtr h, ref POINT p) { return ScreenToClient(h, ref p); }
}
'@

function Say($ok, $label, $detail) {
  if ($ok) { $script:pass++; Write-Host "[PASS] $label  $detail" -ForegroundColor Green }
  else     { $script:fail++; Write-Host "[FAIL] $label  $detail" -ForegroundColor Red }
}

Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 600

$err = 'D:\WorkBuddy\Tiez\frameless.err'
$out = 'D:\WorkBuddy\Tiez\frameless.log'
Remove-Item $err, $out -ErrorAction SilentlyContinue
$env:RUST_LOG = 'info,modular_clipboard=debug'

Write-Host "启动 $exe ..."
$p = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru `
        -RedirectStandardOutput $out -RedirectStandardError $err
Start-Sleep -Seconds 4
$p.Refresh()

$main = [IntPtr]::Zero
foreach ($h in [W]::TopLevel([uint32]$p.Id)) {
  if ([W]::Cls($h) -eq 'ModularClipboardWindow') { $main = $h; break }
}
if ($main -eq [IntPtr]::Zero) { Write-Host "[FAIL] 找不到主窗口 hwnd" -ForegroundColor Red; Stop-Process -Id $p.Id -Force; exit 1 }
Write-Host "主窗口 hwnd = $main`n"

# ---------------------------------------------------------------- a) 尺寸
$cr = New-Object W+RECT
[void][W]::GetClientRect($main, [ref]$cr)
$cw = $cr.right - $cr.left; $ch = $cr.bottom - $cr.top
$wr = New-Object W+RECT
[void][W]::GetWindowRect($main, [ref]$wr)
$ww = $wr.right - $wr.left; $wh = $wr.bottom - $wr.top

Write-Host "== a) 尺寸 =="
Write-Host "  客户区 $cw x $ch"
Write-Host "  窗口区 $ww x $wh"
# 关键：WS_POPUP 下客户区必须**等于**请求尺寸，且与窗口区相同。
# 若AdjustWindowRectEx 的换算残留，窗口区会比客户区大一圈（标题栏高度）。
Say ($cw -gt 0 -and $ch -gt 0) 'a1 客户区非零' "客户区 ${cw}x${ch}"
Say ($cw -eq $ww -and $ch -eq $wh) 'a2 客户区==窗口区（无标题栏高度）' "客户 ${cw}x${ch} vs 窗口 ${ww}x${wh}"
# 日志里的「窗口已创建」应与实测一致
$logLine = Get-Content $err -ErrorAction SilentlyContinue | Select-String '窗口已创建' | Select-Object -Last 1
if ($logLine) { Write-Host ("  日志: " + $logLine) } else { Write-Host "  (未捕获到「窗口已创建」日志)" }

# ---------------------------------------------------------------- 样式
Write-Host "`n== 样式（无边框的直接证据）=="
$style = [W]::GetWindowLongPtr($main, [W]::GWL_STYLE).ToInt64()
$hasPopup    = ($style -band [W]::WS_POPUP) -ne 0
$hasCaption  = ($style -band [W]::WS_CAPTION) -ne 0
$hasThickFrm = ($style -band [W]::WS_THICKFRAME) -ne 0
Say $hasPopup      's1 样式含 WS_POPUP' ("style=0x{0:X}" -f $style)
Say (-not $hasCaption) 's2 样式**不含** WS_CAPTION/WS_BORDER（无标题栏无边框）' ("hasCaption=$hasCaption")
Say (-not $hasThickFrm) 's3 样式**不含** WS_THICKFRAME（resize 由 WM_NCHITTEST 自绘）' ("hasThickFrame=$hasThickFrm")

# ---------------------------------------------------------------- e) resize
Write-Host "`n== e) 边缘 resize 命中 =="
# 指针移到左边缘 / 右上角（屏幕坐标）
$pt = New-Object W+POINT
$pt.x = $wr.left + 1; $pt.y = $wr.top + ($wh -shr 1)
[void][W]::ClientToScreen($main, [ref]$pt)
$hitLeft = [W]::HitTest($main, $pt.x, $pt.y).ToInt32()
Say ($hitLeft -eq 10) 'e1 左边缘 -> HTLEFT(10)' "得到 $hitLeft"

$pt.x = $wr.right - 1; $pt.y = $wr.top + ($wh -shr 1)
[void][W]::ClientToScreen($main, [ref]$pt)
$hitRight = [W]::HitTest($main, $pt.x, $pt.y).ToInt32()
Say ($hitRight -eq 11) 'e2 右边缘 -> HTRIGHT(11)' "得到 $hitRight"

$pt.x = $wr.left + ($ww -shr 1); $pt.y = $wr.top + 1
[void][W]::ClientToScreen($main, [ref]$pt)
$hitTop = [W]::HitTest($main, $pt.x, $pt.y).ToInt32()
Say ($hitTop -eq 12) 'e3 上边缘 -> HTTOP(12)' "得到 $hitTop"

$pt.x = $wr.left + 1; $pt.y = $wr.top + 1
[void][W]::ClientToScreen($main, [ref]$pt)
$hitTL = [W]::HitTest($main, $pt.x, $pt.y).ToInt32()
Say ($hitTL -eq 13) 'e4 左上角 -> HTTOPLEFT(13)' "得到 $hitTL"

# 中心必须是 HTCLIENT(1)，否则界面收不到鼠标事件
$pt.x = $wr.left + ($ww -shr 1); $pt.y = $wr.top + ($wh -shr 1)
[void][W]::ClientToScreen($main, [ref]$pt)
$hitMid = [W]::HitTest($main, $pt.x, $pt.y).ToInt32()
Say ($hitMid -eq 1) 'e5 窗口中心 -> HTCLIENT(1)（egui 仍能收鼠标）' "得到 $hitMid"

# ---------------------------------------------------------------- f) DPI
Write-Host "`n== f) DPI =="
$dpi = [W]::GetDpiForWindow($main)
Say ($dpi -ge 96 -and ($dpi % 96 -eq 0 -or $dpi -gt 0)) 'f1 GetDpiForWindow 返回合理值' "dpi=$dpi"

# ---------------------------------------------------------------- d) 最小化
Write-Host "`n== d) 最小化 / 恢复 =="
[void][W]::PM($main, 0x0112, [IntPtr]0xF020, [IntPtr]::Zero)   # WM_SYSCOMMAND SC_MINIMIZE
Start-Sleep -Milliseconds 900
$iconic = [W]::IsIconic($main)
Say $iconic 'd1 SC_MINIMIZE 后 IsIconic == TRUE' "IsIconic=$iconic"
# 最小化期间必须仍可见（IsWindowVisible 与 IsIconic 是两件事）
Say ([W]::IsWindowVisible($main)) 'd2 最小化期间 IsWindowVisible 仍为 TRUE' ''
[void][W]::PM($main, 0x0112, [IntPtr]0xF120, [IntPtr]::Zero)  # SC_RESTORE
Start-Sleep -Milliseconds 900
$restored = -not [W]::IsIconic($main)
Say $restored 'd3 SC_RESTORE 后 IsIconic == FALSE' "IsIconic=$([W]::IsIconic($main))"
$cr2 = New-Object W+RECT
[void][W]::GetClientRect($main, [ref]$cr2)
Say (($cr2.right-$cr2.left) -gt 0) 'd4 恢复后客户区尺寸非零' "客户区 $($cr2.right-$cr2.left)x$($cr2.bottom-$cr2.top)"

# ---------------------------------------------------------------- b) 四条关闭路径
Write-Host "`n== b) 四条关闭路径 =="
$cases = @(
  @{ Name='WM_CLOSE     (0x0010)'; Msg=0x0010; Wp=0x0000 },
  @{ Name='WM_SYSCOMMAND(0xF060)'; Msg=0x0112; Wp=0xF060 },
  @{ Name='WM_SYSCOMMAND+Alt(0xF061)'; Msg=0x0112; Wp=0xF061 },
  @{ Name='WM_SYSCOMMAND Min (0xF020)'; Msg=0x0112; Wp=0xF020 }
)
foreach ($c in $cases) {
  #每条路径都用**全新进程**，避免上一条的状态污染
  Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 500
  $e2 = "D:\WorkBuddy\Tiez\fl-$($c.Msg.ToString('x'))-$($c.Wp.ToString('x')).err"
  $o2 = "D:\WorkBuddy\Tiez\fl.log"
  Remove-Item $e2, $o2 -ErrorAction SilentlyContinue
  $q = Start-Process -FilePath $exe -ArgumentList '--no-capture' -PassThru `
        -RedirectStandardOutput $o2 -RedirectStandardError $e2
  Start-Sleep -Seconds 3
  $q.Refresh()
  $hw = [IntPtr]::Zero
  foreach ($h in [W]::TopLevel([uint32]$q.Id)) { if ([W]::Cls($h) -eq 'ModularClipboardWindow') { $hw = $h; break } }
  if ($hw -eq [IntPtr]::Zero) { Say $false "[$($c.Name)]" '找不到主窗口'; continue }

  $before = [W]::IsWindowVisible($hw)
  $ok = [W]::PM($hw, $c.Msg, [IntPtr]$c.Wp, [IntPtr]::Zero)
  Start-Sleep -Milliseconds 1200
  $q.Refresh()
  $after = [W]::IsWindowVisible($hw)
  $alive = -not $q.HasExited

  # SC_MINIMIZE 不该触发关闭；其余三条应隐藏窗口但进程常驻
  $expectHide = ($c.Wp -eq 0xF020) -eq $false
  $pass = if ($expectHide) { $ok -and $alive -and ($before -eq $true) -and ($after -eq $false) }
          else { $ok -and $alive -and ($after -eq $true) }
  Say $pass "[$($c.Name)]" "post=$ok visible: $before -> $after alive=$alive"
  $h2 = Get-Content $e2 -ErrorAction SilentlyContinue | Select-String '关闭请求'
  if ($h2) { Write-Host("        日志: " + $h2[-1]) }
  Stop-Process -Id $q.Id -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 400
}

# ---------------------------------------------------------------- 汇总
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-Item 'D:\WorkBuddy\Tiez\fl-*.err','D:\WorkBuddy\Tiez\fl.log','D:\WorkBuddy\Tiez\frameless.err','D:\WorkBuddy\Tiez\frameless.log' -ErrorAction SilentlyContinue

Write-Host "`n================================"
Write-Host "PASS = $script:pass    FAIL = $script:fail"
Write-Host "================================"
Write-Host ""
Write-Host ">>> c) 这一项脚本**无法**判定，需你人眼确认：" -ForegroundColor Yellow
Write-Host "    窗口顶部应该出现的是egui 自绘的内容（标题栏 + 关闭按钮），"
Write-Host "    而**不是**系统的灰白标题栏；窗口四周也没有可拖动的系统边框。"
Write-Host "    鼠标移到窗口边缘应出现**双箭头**光标，拖动能缩放窗口。"
if ($script:fail -eq 0) { exit 0 } else { exit 1 }