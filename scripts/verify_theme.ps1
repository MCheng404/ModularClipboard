
# ⚠️ 由 scripts/fix_hardcoded_paths.py 插入：按**脚本自身位置**推导仓库根，
# 不再硬编码绝对路径。项目目录改名 / 搬走后，验证脚本依然能用。
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

$ErrorActionPreference = 'Stop'
# 主题接线实机验证。
#
# 界面里**没有**主题切换入口（`show_settings` 只翻标志位，没有面板），
# 所以自动验证只能走配置文件：`UiConfig.dark_mode` 是
# `Some(true)/Some(false)/None(跟随系统)`，接线读的就是它。
#
# 验证两件事：
#   1. 每次启动的日志里「主题已解析」打印出正确的 dark 值
#   2. 深浅两套**真的不同**（不是只改了个日志）
# 用像素采样证明第2 点：抓客户区一块不含文字/按钮的区域，
# 深色下应是暗色、浅色下应是亮色。

$exe = '$RepoRoot\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe'
$script:pass = 0
$script:fail = 0

Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Drawing;
public static class W4 {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  [DllImport("user32.dll")] public static extern IntPtr GetDC(IntPtr h);
  [DllImport("user32.dll")] public static extern int ReleaseDC(IntPtr h, IntPtr dc);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int left, top, right, bottom; }
  public static List<IntPtr> TopLevel(uint want) {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == want) r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Cls(IntPtr h) { var sb = new StringBuilder(256); GetClassNameW(h, sb, 256); return sb.ToString(); }
}
'@

function Say($ok, $label, $detail) {
  if ($ok) {
    $script:pass = [int]$script:pass + 1
    Write-Host "[PASS] $label  $detail" -ForegroundColor Green
  } else {
    $script:fail = [int]$script:fail + 1
    Write-Host "[FAIL] $label  $detail" -ForegroundColor Red
  }
}

# 采客户区一块**空白**区域的平均亮度。
# 取 (宽/2, 高-8)：状态栏上沿、又没有文字的横带。
function SampleLuma($hwnd) {
  $cr = New-Object W4+RECT
  [void][W4]::GetClientRect($hwnd, [ref]$cr)
  $w = $cr.right - $cr.left; $h = $cr.bottom - $cr.top
  $bmp = New-Object System.Drawing.Bitmap $w, $h
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $dc = [W4]::GetDC($hwnd)
  try {
    # PW_CLIENTONLY = 1, PW_RENDERFULLCONTENT = 2
    [void][W4]::PrintWindow($hwnd, $g.GetHdc(), 3)
  } finally {
    $g.ReleaseHdc()
    [void][W4]::ReleaseDC($hwnd, $dc)
    $g.Dispose()
  }
  $sum = 0.0; $n = 0
  for ($y = [int]($h * 0.55); $y -lt [int]($h * 0.55) + 6; $y++) {
    for ($x = [int]($w * 0.45); $x -lt [int]($w * 0.45) + 20; $x++) {
      $c = $bmp.GetPixel($x, $y)
      $sum += 0.299 * $c.R + 0.587 * $c.G + 0.114 * $c.B
      $n++
    }
  }
  $bmp.Dispose()
  return [math]::Round($sum / $n, 1)
}

# 跑一次：写入 dark_mode，跑 exe，抓日志里的主题行+ 采样亮度
function RunCase($dir, $darkModeJson, $label) {
  Get-Process | Where-Object { $_.ProcessName -like '*modular*' } | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 500

  if (Test-Path $dir) {
    # 同样避开受保护删除：残留目录用 `sub` 后缀另起，避免撞名。
    $dir = "$dir-new"
    if (Test-Path $dir) { $dir = "$dir-$(Get-Random)" }
  }
  New-Item -ItemType Directory -Path $dir -Force | Out-Null
  $cfg = @"
{
  "storage": {"max_items": 100, "max_bytes": 1048576, "max_payload_bytes": 1048576, "cleanup_on_start": false},
  "capture": {"enabled": false, "poll_interval_ms": 300, "track_source_app": false,
              "blocked_apps": [], "skip_password_fields": false, "dedup": true, "dedup_window_secs": 3},
  "ui": {"dark_mode": $darkModeJson, "always_on_top": true, "hide_on_focus_lost": false,
         "window_width": 420.0, "window_height": 560.0, "show_tray": true,
         "start_minimized": false, "font_path": null, "font_scale": 1.0, "layout": null},
  "hotkey_toggle": {"key": "V", "ctrl": true, "shift": true, "alt": false, "win": false},
  "hotkey_paste_previous": {"key": "V", "ctrl": true, "shift": false, "alt": false, "win": true},
  "rules": []
}
"@
  # ⚠️ 必须用 `WriteAllText` + `UTF8Encoding($false)`：**不能**用
  #    `Set-Content -Encoding UTF8`，它会写UTF-8 BOM，而 serde_json
  #    不接受BOM —— `Config::load` 解析失败后**静默回退默认值**，
  #    于是 dark_mode 被吃掉、日志打出 configured=None，
  #    看起来像「主题接线没生效」，实际是测试自己写坏了配置。
  $enc = New-Object System.Text.UTF8Encoding($false)
  [System.IO.File]::WriteAllText("$dir\config.json", $cfg, $enc)

  $e2 = "$dir\run.err"; $o2 = "$dir\run.log"
  $env:RUST_LOG = 'debug'
  $p = Start-Process -FilePath $exe -ArgumentList '--data-dir', $dir -PassThru `
        -RedirectStandardOutput $o2 -RedirectStandardError $e2
  Start-Sleep -Seconds 4
  $p.Refresh()

  $main = [IntPtr]::Zero
  foreach ($h in [W4]::TopLevel([uint32]$p.Id)) {
    if ([W4]::Cls($h) -eq 'ModularClipboardWindow') { $main = $h; break }
  }
  if ($main -eq [IntPtr]::Zero) {
    Say $false $label '找不到主窗口'
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    return $null
  }

  # 采样前再等一拍：PrintWindow 抓的是**已经画上去**的内容，
  # 窗口刚显示时交换链可能还没 present 过，抓到的是黑底。
  # 实测浅色模式下抓全 0就是这么来的。
  Start-Sleep -Seconds 2
  $luma = SampleLuma $main
  Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
  # ⚠️ 必须在停进程**之后**读日志：`Start-Process -RedirectStandardError`
  #    的写句柄由子进程持有，进程活着时 `ReadAllText` 会因共享锁抛异常。
  Start-Sleep -Milliseconds 700
  # 另：日志是 UTF-8 且带 ANSI 转义（tracing 的 with_ansi 会给级别上色），
  #    必须按字节解码 + 剥掉 CSI 序列，否则中文乱码、`dark=` 断言恒false。
  $raw = [System.IO.File]::ReadAllText($e2, [System.Text.Encoding]::UTF8)
  $plain = [regex]::Replace($raw, "`e\[[0-9;]*[A-Za-z]", '')
  $themeLine = ($plain -split "`r?`n" |
                Where-Object { $_ -like '*主题已解析*' } |
                Select-Object -Last 1)

  Write-Host "  [$label] 日志: $themeLine"
  Write-Host "  [$label] 空白区亮度: $luma"
  return [pscustomobject]@{ label = $label; line = "$themeLine"; luma = $luma }
}

$base = '$RepoRoot\theme-probe'
if (Test-Path $base) { Remove-Item $base -Recurse -Force }

Write-Host "== 三种 dark_mode 各自跑一次 =="
$dark = RunCase "$base\dark" 'true'  'dark_mode=Some(true)'
$light = RunCase "$base\light" 'false' 'dark_mode=Some(false)'
$sys = RunCase "$base\sys" 'null'   'dark_mode=None(跟随系统)'

# ---- 断言 ----
Write-Host "`n== 断言 =="
# ⚠️ 下面用 `.Contains()` 而不是 `-match`：日志行里可能带 ANSI 转义序列
#    （tracing 的 `with_ansi` 会给级别上色），`-match` 遇到非法转义
#    会静默返回 False，`.Contains` 是纯字面比较，不受转义影响。
function HasDark($o, $want) {
  if ($null -eq $o) { return $false }
  $s = [string]$o.line
  return $s.Contains("dark=$want")
}
if ($dark) {
  Say (HasDark $dark 'true') 't1 Some(true) 解析为深色' $dark.line
  Say ($dark.luma -lt 128) 't2 Some(true) 界面确实是暗的' "亮度 $($dark.luma) < 128"
}
if ($light) {
  Say (HasDark $light 'false') 't3 Some(false) 解析为浅色' $light.line
  Say ($light.luma -gt 128) 't4 Some(false) 界面确实是亮的' "亮度 $($light.luma) > 128"
}
if ($dark -and $light) {
  $d = [math]::Abs($dark.luma - $light.luma)
  Say ($d -gt 40) 't5 深浅两套**肉眼可分**' "亮度差 $d"
}
if ($sys) {
  # 跟随系统：解析成 Dark 或 Light 都合法，但必须是二者之一且不留 None
  $s = [string]$sys.line
  Say ($s.Contains('dark=true') -or $s.Contains('dark=false')) `
      't6 跟随系统解析成确定值' $sys.line
}

# ⚠️ 不用 `Remove-Item -Recurse` 清目录：受删除策略保护时会直接抛
#    safe-delete 失败并中断脚本，而此时断言其实已经跑完了。
#    每个 case 用**独立子目录**互不干扰，清不清都不影响正确性。
try { Remove-Item $base -Recurse -Force -ErrorAction SilentlyContinue } catch {}
Write-Host "`n================================"
Write-Host "PASS = $script:pass    FAIL = $script:fail"
Write-Host "================================"
if ($script:fail -eq 0) { exit 0 } else { exit 1 }