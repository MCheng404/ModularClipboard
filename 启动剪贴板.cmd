@echo off
rem模块化剪贴板 —— 启动脚本（**带 cmd 窗口**，仅用于排查问题）
rem
rem ============================================================
rem [!] 日常启动请双击「启动剪贴板.vbs」——它**不弹任何窗口**。
rem
rem 本 .cmd 双击必然附带一个 cmd 窗口：.cmd 就是批处理，
rem Windows 一定会给它一个控制台。这跟exe 本身无关——
rem exe 已经是 GUI 子系统（PE Subsystem = 2），直接双击它
rem 也不会有窗口。
rem
rem 这个 .cmd 保留下来是因为它能「把stderr 落盘 + 检查进程是否
rem 立即退出」，这两件事在排查「双击没反应」时很有用。
rem ============================================================

setlocal


set "REPO=D:\WorkBuddy\Tiez"
set "EXE=%REPO%\target\x86_64-pc-windows-msvc\release\modular-clipboard.exe"

if not exist "%EXE%" (
    echo [错误] 找不到可执行文件：
    echo        %EXE%
    echo.
    echo 请先构建：在 %REPO% 下执行
    echo        ./scripts/build.sh --release
    echo.
    pause
    exit /b 1
)

rem 后台启动，日志写到仓库的 logs 目录便于排查崩溃。
if not exist "%REPO%\logs" mkdir "%REPO%\logs"

start "" /b "%EXE%" 2>"%REPO%\logs\last-run.err"

rem 只在启动失败时提示，正常起来了窗口就在任务栏/托盘。
rem 等1 秒看进程是否还活着——很多失败（比如 Vulkan 初始化失败）
rem 会在启动后几百毫秒内就退出。
timeout /t 2 /nobreak >nul
tasklist /fi "IMAGENAME eq modular-clipboard.exe" | find /i "modular-clipboard.exe" >nul
if errorlevel 1 (
    echo [警告] 程序启动后立即退出，日志尾部：
    powershell -NoProfile -Command "if (Test-Path '%REPO%\logs\last-run.err') { Get-Content '%REPO%\logs\last-run.err' -Tail 10 }"
    echo.
    pause
)

endlocal
