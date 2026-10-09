' 模块化剪贴板 —— 静默启动器（双击**不弹任何窗口**）
'
' ============================================================
' 为什么用 .vbs 而不是 .cmd
'
' .cmd 就是批处理，Windows 必定给它一个控制台 —— 双击必然闪一个
' 黑窗口。哪怕 exe 本身已经是 GUI 子系统（PE Subsystem = 2）、
' 直接双击 exe 不会有窗口，**通过 .cmd 启动照样有**。
'
' .vbs 由 wscript.exe 执行，默认没有控制台，所以能做到零窗口。
'
' 为什么不直接双击 exe
'
' - exe 在 target/ 里（构建产物，通常不入库），路径随仓库位置变；
' - 万一启动失败（Vulkan 初始化等），静默退出用户什么都看不到。
'   本脚本在找不到 exe 时会弹窗告知。
'
' [!] 快捷方式（.lnk）创建需要 WScript.Shell COM 对象，部分环境的
'    安全策略会拦，所以这里用 .vbs + 直接路径。
' ============================================================

Option Explicit

' [!] 路径写死为本仓库位置。移动目录后需改这一行，
'    或改为从脚本自身位置推导（见下方注释）。
Dim repo, exe, shell, fso

' 从脚本自身位置推导仓库根目录 —— 这样整个目录搬到别处也能用。
' fso.GetParentFolderName(WScript.ScriptFullName) 得到脚本所在目录。
Set fso = CreateObject("Scripting.FileSystemObject")
repo = fso.GetParentFolderName(WScript.ScriptFullName)
exe = repo & "\target\x86_64-pc-windows-msvc\release\modular-clipboard.exe"

If Not fso.FileExists(exe) Then
    MsgBox "找不到可执行文件：" & vbCrLf & vbCrLf & exe & vbCrLf & vbCrLf & _
           "请先构建（在该目录下执行）：" & vbCrLf & _
           "  ./scripts/build.sh --release" & vbCrLf & vbCrLf & _
           "或直接运行 启动剪贴板.cmd（它会检查并给出提示）。", _
           vbExclamation, "模块化剪贴板"
    WScript.Quit 1
End If

Set shell = CreateObject("WScript.Shell")
' 第二个参数 0 = 最小化隐藏。exe 是 GUI 程序本来就不显示控制台，
' 显式传 0 可避免某些环境下闪一下黑框。
shell.Run """" & exe & """", 0, False

Set shell = Nothing
Set fso = Nothing