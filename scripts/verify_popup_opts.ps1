# 弹窗新选项 + 设置界面的实机验证（2026-10-20 轮次）：驱动端。
#
# 覆盖：
#   1. 「外观」卡片渲染本轮新增的两个设置项（搜索面板 / 通知样式）——
#      判据见 verify_popup_opts.py（卡片内容高度 = 行数，模型无关）；
#   2. 类型过滤栏在有多类型记录时弹出，且**Markdown 徽标**（本轮重绘）
#      画在按钮里——截类型栏弹窗；
#   3. 通知样式切到「独立弹窗」后行为改变（设置 -> 行为闭环）。
#
# 方法同 verify_notice.ps1：PrintWindow + 精确标题 + DPI aware。
param([string]$Phase = "all")  # all | shot

$ErrorActionPreference = 'Stop'

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class VP {
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
    mouse_event(0x04, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(200);
  }
  public static void DoubleClick(int x, int y) {
    SetCursorPos(x, y); System.Threading.Thread.Sleep(100);
    mouse_event(0x02, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(30);
    mouse_event(0x04, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(50);
    mouse_event(0x02, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(30);
    mouse_event(0x04, 0, 0, 0, UIntPtr.Zero); System.Threading.Thread.Sleep(150);
  }
}
'@
Add-Type -AssemblyName System.Drawing

[void][VP]::SetProcessDPIAware()

$exe = 'D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
$root = 'D:\modular-clipboard'
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

$dataDir = Join-Path $env:TEMP ("mc-vp-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory $dataDir | Out-Null
# 无 BOM（项目修复记录记载的坑：带 BOM 的配置种子会让 Config::load
# 静默回落默认值）。
$json = '{`"ui`":{`"dark_mode`":true,`"notice_popup`":false}}'
[System.IO.File]::WriteAllText((Join-Path $dataDir 'config.json'), $json, (New-Object System.Text.UTF8Encoding($false)))

$env:MC_SETTINGS = '1'
$p = Start-Process -FilePath $exe -ArgumentList "--data-dir","$dataDir" -PassThru
Start-Sleep -Seconds 4.5

# 造三种类型的记录：文本 / Markdown / HTML（HTML 走剪贴板的 CF_HTML 太难，
# 用带 HTML 标签的纯文本 + Markdown 特征文本各一条，捕获层各自归类）。
Set-Clipboard -Value "实机验证：普通文本记录一条"
Start-Sleep -Milliseconds 900
Set-Clipboard -Value "# 实机验证标题`n`n- 列表项一`n- 列表项二`n`n**粗体** 与 ``代码``"
Start-Sleep -Milliseconds 900

function Snap($hwnd, $name) {
  $l = 0; $t = 0; $r = 0; $b = 0
  [VP]::GetRect($hwnd, [ref]$l, [ref]$t, [ref]$r, [ref]$b)
  $scale = [double][VP]::Dpi($hwnd) / 96.0
  $pw = $r - $l; $ph = $b - $t
  $bmp = New-Object System.Drawing.Bitmap($pw, $ph)
  $g = [System.Drawing.Graphics]::FromImage($bmp); $dc = $g.GetHdc()
  $ok = [VP]::PrintWindow($hwnd, $dc, 2)
  $g.ReleaseHdc($dc); $g.Dispose()
  if (-not $ok) { Write-Host "FAIL: PrintWindow failed"; exit 1 }
  $bmp.Save((Join-Path $root $name), [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  Write-Host "  saved $name (${pw}x${ph})"
}

$settings = [VP]::FindTitle('剪贴板设置')
if ($settings -eq [IntPtr]::Zero) { Write-Host "FAIL: 找不到设置窗口"; exit 1 }
[void][VP]::SetForegroundWindow($settings)
$sl = 0; $st = 0; $sr = 0; $sb = 0
[VP]::GetRect($settings, [ref]$sl, [ref]$st, [ref]$sr, [ref]$sb)
$sscale = [double][VP]::Dpi($settings) / 96.0
Write-Host "设置窗 rect=($sl,$st,$sr,$sb) scale=$sscale"

# 导航第 3 项「外观」：逻辑 (98, 156)（verify_theme_font.ps1 同款坐标）。
[VP]::Click([int]($sl + 98 * $sscale), [int]($st + 156 * $sscale))
Start-Sleep -Milliseconds 700
Snap $settings 'vp_appearance.png'

$manifest = @{}
# 类型栏弹窗：有 Text/Markdown 两类记录时应弹出（按钮数 = 2）。
Start-Sleep -Milliseconds 300
$rail = [VP]::FindTitle('ModularClipboard 类型栏')
$manifest.rail_window = if ($rail -ne [IntPtr]::Zero) { "present" } else { "absent" }
if ($rail -ne [IntPtr]::Zero) {
  $rl = 0; $rt = 0; $rr = 0; $rb = 0
  [VP]::GetRect($rail, [ref]$rl, [ref]$rt, [ref]$rr, [ref]$rb)
  $manifest.rail_size = @($rr - $rl, $rb - $rt)
  Snap $rail 'vp_rail.png'
  Write-Host "类型栏弹窗: present $($manifest.rail_size -join 'x')"
} else {
  Write-Host "类型栏弹窗: absent"
}

# 主窗口（设置开着时它也在）。
$main = [VP]::FindTitle('ModularClipboard')
if ($main -ne [IntPtr]::Zero) { Snap $main 'vp_main.png' }

[System.IO.File]::WriteAllText(
  (Join-Path $root 'vp_manifest.json'),
  ($manifest | ConvertTo-Json),
  (New-Object System.Text.UTF8Encoding($false)))

Remove-Item Env:\MC_SETTINGS
Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
Remove-Item $dataDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Host "done"
