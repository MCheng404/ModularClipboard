# 搜索面板关闭行为实机验证（2026-10-21 用户反馈修复）：驱动端。
#
# 三个场景（用户原话「搜索框打开后无法自动关闭和手动关闭」）：
#   1. 自动关闭：展开后指针移到**主窗口之外**（桌面/别的应用），
#      等 1.5s（展开宽限 0.6 + 折叠宽限 0.35 + 余量）应自行收起；
#   2. Esc 手动关闭：展开后按 Esc 应收起；
#   3. 图标开合切换：点击展开（悬停展开后的首点是确认）→ 再点收起。
#
# 判据：窗口「ModularClipboard 搜索」的存在性（精确标题）。
$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class VS {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern void SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, UIntPtr e);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  public const byte VK_ESCAPE = 0x1B;
  public static IntPtr FindTitle(string want) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h,l) => { var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256); if (sb.ToString() == want && IsWindowVisible(h)) { found = h; return false; } return true; }, IntPtr.Zero);
    return found;
  }
  public static void GetRect(IntPtr h, out int l, out int t, out int r, out int b) {
    var rc = new RECT(); GetWindowRect(h, out rc);
    l = rc.L; t = rc.T; r = rc.R; b = rc.B;
  }
  public static uint Dpi(IntPtr h) { uint d = GetDpiForWindow(h); return d == 0 ? 96 : d; }
  public static void Click(int x, int y) {
    SetCursorPos(x, y); System.Threading.Thread.Sleep(120);
    mouse_event(0x02, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(50);
    mouse_event(0x04, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(250);
  }
  public static void Esc() {
    keybd_event(VK_ESCAPE, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(50);
    keybd_event(VK_ESCAPE, 0, 2, UIntPtr.Zero); System.Threading.Thread.Sleep(250);
  }
}
'@
[void][VS]::SetProcessDPIAware()

$exe = 'D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

$dataDir = Join-Path $env:TEMP ("mc-vs-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory $dataDir | Out-Null
$json = '{"ui":{"dark_mode":true}}'
[System.IO.File]::WriteAllText((Join-Path $dataDir 'config.json'), $json, (New-Object System.Text.UTF8Encoding($false)))

Start-Process -FilePath $exe -ArgumentList "--data-dir",$dataDir | Out-Null
Start-Sleep -Seconds 4.5

$main = [VS]::FindTitle('ModularClipboard')
if ($main -eq [IntPtr]::Zero) { Write-Host "FAIL: 找不到主窗口"; exit 1 }
[void][VS]::SetForegroundWindow($main)
$ml = 0; $mt = 0; $mr = 0; $mb = 0
[VS]::GetRect($main, [ref]$ml, [ref]$mt, [ref]$mr, [ref]$mb)
$ppp = [double][VS]::Dpi($main) / 96.0
# 逻辑布局 480x533（物理 720x800 / ppp）。搜索图标中心（draw_topbar）：
#   inner.max.x = 480-12 = 468；btn = 44*0.62 = 27.28；gap = 16；
#   btn_area.min.x = 468 - 27.28*4 - 16*3 = 310.88；
#   搜索图标中心 x = 310.88 - space_md(12) - sq(27.28*0.62)/2 = 290.4，
#   y = 顶栏中心 = 22。
$ix = [int]($ml + 290 * $ppp)
$iy = [int]($mt + 22 * $ppp)

$results = @()
function Check($name, $expect) {
  $h = [VS]::FindTitle('ModularClipboard 搜索')
  $present = ($h -ne [IntPtr]::Zero)
  $ok = if ($expect) { $present } else { -not $present }
  $script:results += [PSCustomObject]@{ scene = $name; expect = $expect; present = $present; ok = $ok }
  Write-Host ("  [{0}] {1}（期望窗口{2}，实际{3}）" -f $(if ($ok) { "PASS" } else { "FAIL" }), $name, $(if ($expect) { "在" } else { "不在" }), $(if ($present) { "在" } else { "不在" }))
}

# ---- 场景 1：自动关闭 ----
# 悬停图标 0.3s 触发展开。
[VS]::Click($ix, $iy)   # 点击 = 展开（折叠态点开）
Start-Sleep -Milliseconds 400
Check "点击展开" $true

# 指针移到**主窗口与弹窗之外**（屏幕左上角桌面区）——关键：不碰主窗口，
# 否则主窗口收到鼠标事件会跑状态机，掩盖「闸门饿死」的根因。
[VS]::SetCursorPos(30, 900)
# 轮询观察折叠时刻（诊断「迟到」与「永不」的区别）。
$t0 = Get-Date
$collapsedAt = $null
for ($i = 0; $i -lt 25; $i++) {
  Start-Sleep -Milliseconds 200
  if ([VS]::FindTitle('ModularClipboard 搜索') -eq [IntPtr]::Zero) {
    $collapsedAt = ((Get-Date) - $t0).TotalSeconds
    break
  }
}
if ($null -ne $collapsedAt) {
  Write-Host ("  [诊断] 指针离开后 {0:N2}s 收起" -f $collapsedAt)
} else {
  Write-Host "  [诊断] 指针离开 5s 内未收起"
}
Start-Sleep -Milliseconds 300
Check "指针离开后自动关闭" $false

# ---- 场景 2：Esc 手动关闭 ----
[VS]::Click($ix, $iy)
Start-Sleep -Milliseconds 400
Check "再次点击展开（供 Esc 测试）" $true
[VS]::Esc()
Start-Sleep -Milliseconds 400
Check "Esc 手动关闭" $false

# ---- 场景 3：图标开合切换 ----
# 点击 #1 = 展开（若悬停已展开则是「确认」，语义都落成 opened_by=Click）；
# 点击 #2 = 收起。悬停展开后的「确认语义」由单测
# `search_icon_click_confirm_then_close` 覆盖（实机的悬停/点击时序
# 互作太脆，不适合做确定性断言）。
[VS]::Click($ix, $iy)
Start-Sleep -Milliseconds 350
Check "点击 #1 后展开" $true
[VS]::Click($ix, $iy)
Start-Sleep -Milliseconds 350
Check "点击 #2 收起（开合切换）" $false

$pass = -not ($results | Where-Object { -not $_.ok })
Write-Host ("SEARCH CLOSE VERIFY: " + $(if ($pass) { "PASS" } else { "FAIL" }))
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-Item $dataDir -Recurse -Force -ErrorAction SilentlyContinue
if (-not $pass) { exit 1 }
