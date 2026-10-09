# 折叠式搜索实测：折叠态大片拖动 → 点图标展开 → 展开态拖动
param([int]$TargetPid)

Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class DragWin3 {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
  public static IntPtr MainOf(uint want) {
    IntPtr m = IntPtr.Zero;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p == want) {
        var sb = new StringBuilder(256); GetWindowTextW(h, sb, 256);
        if (sb.ToString().StartsWith("ModularClipboard")) m = h;
      }
      return true;
    }, IntPtr.Zero);
    return m;
  }
  public static int[] Rect(IntPtr h) { RECT r; GetWindowRect(h, out r); return new int[] { r.L, r.T, r.R, r.B }; }
  public static int[] Drag(IntPtr h, int x0, int y0, int x1, int y1) {
    int[] b = Rect(h);
    SetCursorPos(x0, y0);
    System.Threading.Thread.Sleep(400);
    mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(250);
    for (int i = 1; i <= 12; i++) {
      SetCursorPos(x0 + (x1 - x0) * i / 12, y0 + (y1 - y0) * i / 12);
      System.Threading.Thread.Sleep(30);
    }
    mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
    System.Threading.Thread.Sleep(600);
    int[] a = Rect(h);
    return new int[] { a[0] - b[0], a[1] - b[1] };
  }
}
'@

$main = [DragWin3]::MainOf([uint32]$TargetPid)
if ($main -eq [IntPtr]::Zero) { Write-Output "NO_MAIN"; exit 1 }
$r0 = [DragWin3]::Rect($main)
Write-Output ("WIN=" + ($r0 -join ',') + " W=" + ($r0[2]-$r0[0]) + " H=" + ($r0[3]-$r0[1]))

# 1) 折叠态：顶栏左大片，RelX 100 物理（远离按钮/图标）→ 必须 MOVED
$m1 = [DragWin3]::Drag($main, ($r0[0]+100), ($r0[1]+25), ($r0[0]+100+70), ($r0[1]+25))
Write-Output ("FOLD_X100=" + ($m1 -join ','))

# 2) 折叠态：更靠右（RelX 250），仍在图标左侧大片区 → 必须 MOVED
$r0 = [DragWin3]::Rect($main)
$m2 = [DragWin3]::Drag($main, ($r0[0]+250), ($r0[1]+25), ($r0[0]+250-60), ($r0[1]+25))
Write-Output ("FOLD_X250=" + ($m2 -join ','))

# 3) 点搜索图标（RelX 285 物理，按钮左侧）→ 展开搜索框（原地点击，不拖动）
$r0 = [DragWin3]::Rect($main)
$m3 = [DragWin3]::Drag($main, ($r0[0]+285), ($r0[1]+22), ($r0[0]+285), ($r0[1]+22))
Write-Output ("CLICK_ICON=" + ($m3 -join ',') + " (0,0=未拖动，即点击)")
