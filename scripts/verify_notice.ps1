# 通知呈现实机验证（2026-10-20 轮次）：驱动端。
#
# 覆盖三条链路：
#   1. toast 模式：双击行复制 => 窗内顶部出现「已复制」toast；
#   2. 弹窗模式：同一操作 => 独立窗口「ModularClipboard 通知」出现，
#      TTL 后自行消失；
#   3. Enter 复制：单击选中行 + Enter => 同样触发复制反馈。
#
# # 为什么不用 MCP screen_capture（2026-10-20 换方法的原因）
#
# MCP 的 `find_window` 是「标题**包含**」匹配：类型栏弹窗标题
# 「ModularClipboard 类型栏」包含「ModularClipboard」，Z 序里排在
# 主窗口前面 ⇒ screen_capture 抓到的是 108x108 的类型栏（实测），
# 主窗口反而抓不到。本脚本走项目既有的 **PrintWindow + 精确标题**
# 路线（`diag_shot_main.ps1` 同款），DPI 感知声明后 GetWindowRect
# 直接给物理像素，点击坐标 = 窗口原点 + 逻辑点 x scale。
#
# 产物：vn_before / vn_after / vn_notice / vn_enter / vn_gone 五张
# PNG + vn_manifest.json（窗口存在性事实，供 verify_notice.py 断言）。
param([string]$Mode = "toast")  # toast | popup

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class VN {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern void SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, UIntPtr e);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
  public const byte VK_RETURN = 0x0D;
  // 精确标题匹配（不用「包含」——弹窗标题都带 ModularClipboard 前缀）。
  public static IntPtr FindTitle(string want) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((h,l) => { var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256); if (sb.ToString() == want && IsWindowVisible(h)) { found = h; return false; } return true; }, IntPtr.Zero);
    return found;
  }
  public static void Click(int x, int y) {
    SetCursorPos(x, y); System.Threading.Thread.Sleep(120);
    mouse_event(0x02, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(60);
    mouse_event(0x04, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(120);
  }
  // CAUTION: 两次点击的间隔必须明显小于 egui 的双击判定窗口
  // （GetDoubleClickTime，Windows 默认 500ms，egui 自己也按 ~300ms
  // 收口）。第一版 Click 每次首尾各睡 120ms，两次 down 间隔踩到
  // 300ms 边界 ⇒ 实机录到「只选中、不复制」。这里压到 ~110ms。
  public static void DoubleClick(int x, int y) {
    SetCursorPos(x, y); System.Threading.Thread.Sleep(100);
    mouse_event(0x02, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(30);
    mouse_event(0x04, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(50);
    mouse_event(0x02, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(30);
    mouse_event(0x04, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(150);
  }
  public static void Enter() {
    keybd_event(VK_RETURN, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(50);
    keybd_event(VK_RETURN, 0, 2, UIntPtr.Zero); System.Threading.Thread.Sleep(150);
  }
  public static void GetRect(IntPtr h, out int l, out int t, out int r, out int b) {
    var rc = new RECT(); GetWindowRect(h, out rc);
    l = rc.L; t = rc.T; r = rc.R; b = rc.B;
  }
  public static uint Dpi(IntPtr h) { uint d = GetDpiForWindow(h); return d == 0 ? 96 : d; }
}
'@
Add-Type -AssemblyName System.Drawing

[void][VN]::SetProcessDPIAware()

$exe = 'D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
$root = 'D:\modular-clipboard'
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

$dataDir = Join-Path $env:TEMP ("mc-vn-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory $dataDir | Out-Null
$popup = if ($Mode -eq "popup") { "true" } else { "false" }
# 无 BOM：带 BOM 的 JSON 会让 Config::load 静默回落默认值。
$json = "{`"ui`":{`"dark_mode`":true,`"notice_popup`":$popup}}"
[System.IO.File]::WriteAllText((Join-Path $dataDir 'config.json'), $json, (New-Object System.Text.UTF8Encoding($false)))

$p = Start-Process -FilePath $exe -ArgumentList "--data-dir","$dataDir" -PassThru
Start-Sleep -Seconds 4.5

# 造一条记录：写系统剪贴板，常驻进程捕获后入库（poll 300ms）。
Set-Clipboard -Value "实机验证：复制反馈与通知显示链路测试文本"
Start-Sleep -Milliseconds 1200

$main = [VN]::FindTitle('ModularClipboard')
if ($main -eq [IntPtr]::Zero) { Write-Host "FAIL: 找不到主窗口"; exit 1 }
[void][VN]::SetForegroundWindow($main)
$r = New-Object VN+RECT; [void][VN]::GetWindowRect($main, [ref]$r)
$scale = ($r.R - $r.L) / 720.0
Write-Host "主窗口 rect=($($r.L),$($r.T),$($r.R),$($r.B)) scale=$scale"

# PrintWindow 抓窗存 PNG。CAUTION: bitmap 尺寸必须按**物理像素**建--
# 本进程已 SetProcessDPIAware，GetWindowRect 直接给物理像素；早前
# diag 脚本没声明 DPI aware（拿到逻辑点），bitmap 要乘 scale，两处
# 口径不同，别互相抄。
function Snap($hwnd, $name) {
  $l = 0; $t = 0; $r = 0; $b = 0
  [VN]::GetRect($hwnd, [ref]$l, [ref]$t, [ref]$r, [ref]$b)
  # CAUTION: GetWindowRect 已是**物理像素**（本进程 DPI aware），
  # bitmap 按 rect 原样建——再乘 Dpi/96 会得到 1.5 倍拉伸图。
  $pw = $r - $l
  $ph = $b - $t
  $bmp = New-Object System.Drawing.Bitmap($pw, $ph)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $dc = $g.GetHdc()
  $ok = [VN]::PrintWindow($hwnd, $dc, 2)  # PW_RENDERFULLCONTENT
  $g.ReleaseHdc($dc); $g.Dispose()
  if (-not $ok) { Write-Host "FAIL: PrintWindow failed"; exit 1 }
  $path = Join-Path $root $name
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  Write-Host "  saved $name (${pw}x${ph})"
}
# 逻辑点 -> 物理像素。CAUTION: 应用窗口物理尺寸 720x800（配置语义
# 「物理像素」，见 docs/dpi-conventions.md），ppp=1.5 ⇒ egui 逻辑
# 布局只有 480x533。点击目标必须按**逻辑布局**算再乘 ppp。
$ppp = [double][VN]::Dpi($main) / 96.0
Write-Host "ppp=$ppp 逻辑布局=$([int](($r.R-$r.L)/$ppp))x$([int](($r.B-$r.T)/$ppp))"
function PX([double]$lx) { [int]($r.L + $lx * $ppp) }
function PY([double]$ly) { [int]($r.T + $ly * $ppp) }

# 行 0 中心（逻辑点，480x533 布局）：顶栏 44 + 半缝 3 + 半行高约 35
# （480 窄栏下一行预览会折行，行高比 700 宽夹具里的 52.8 更高）。
$rowX = PX 240.0; $rowY = PY 82.0

Snap $main 'vn_before.png'

# ---- 链路 1/2：双击行复制 ----
[VN]::DoubleClick($rowX, $rowY)
Start-Sleep -Milliseconds 450   # toast 入场 140ms + 余量
Snap $main 'vn_after.png'

$manifest = @{ mode = $Mode; scale = $ppp; window = @($r.L, $r.T, $r.R, $r.B) }
$noticeHwnd = [VN]::FindTitle('ModularClipboard 通知')
$manifest.notice_window = if ($noticeHwnd -ne [IntPtr]::Zero) { "present" } else { "absent" }
if ($noticeHwnd -ne [IntPtr]::Zero) {
  Snap $noticeHwnd 'vn_notice.png'
  Write-Host "  通知弹窗存在，已截图"
}
Write-Host "通知弹窗: $($manifest.notice_window)"

# ---- 链路 3：Enter 复制（先单击选中，再回车）----
# 必须等上一个通知过期（TTL 3s + 余量）：否则 vn_enter 里的 toast
# 归因不清（可能是双击那次的残留），Enter 链路等于没测。
Start-Sleep -Milliseconds 3300
[VN]::Click($rowX, $rowY)
Start-Sleep -Milliseconds 250
[VN]::Enter()
Start-Sleep -Milliseconds 450
Snap $main 'vn_enter.png'
$manifest.notice_window_after_enter = if ([VN]::FindTitle('ModularClipboard 通知') -ne [IntPtr]::Zero) { "present" } else { "absent" }
Write-Host "Enter 后通知弹窗: $($manifest.notice_window_after_enter)"

# ---- TTL 后应消失（ Enter 触发的通知 3.3s 后过期）----
Start-Sleep -Milliseconds 3300
$manifest.notice_window_after_ttl = if ([VN]::FindTitle('ModularClipboard 通知') -ne [IntPtr]::Zero) { "present" } else { "absent" }
Snap $main 'vn_gone.png'
Write-Host "TTL 后通知弹窗: $($manifest.notice_window_after_ttl)"

# CAUTION: `Set-Content -Encoding UTF8` 会写 BOM，读取端（Python json）
# 拿到 BOM 直接解析失败——项目修复记录里记过这个坑（2026-10-14：
# 配置种子文件带 BOM ⇒ Config::load 静默回落默认值）。显式写无 BOM。
[System.IO.File]::WriteAllText(
  (Join-Path $root 'vn_manifest.json'),
  ($manifest | ConvertTo-Json),
  (New-Object System.Text.UTF8Encoding($false)))

Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-Item $dataDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "done mode=$Mode"
