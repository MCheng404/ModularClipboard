# 构建/测试 modular-clipboard（注入 MSVC + Windows SDK 环境）。
# 用法：powershell -ExecutionPolicy Bypass -File scripts\build.ps1 [build|test-ui|test-all]
param([string]$Mode = "build")

$ErrorActionPreference = 'Stop'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

$MSVC = 'D:\Program Files\Microsoft Visual Studio\18\Insiders\VC\Tools\MSVC\14.52.36629'
$SDK = 'C:\Program Files (x86)\Windows Kits\10'
$SDK_VER = '10.0.28000.0'

if (-not (Test-Path (Join-Path $MSVC 'bin\Hostx64\x64\cl.exe'))) {
  Write-Host "✗ MSVC 路径不存在: $MSVC"
  exit 1
}

$env:INCLUDE = "$MSVC\include;$SDK\Include\$SDK_VER\ucrt;$SDK\Include\$SDK_VER\um;$SDK\Include\$SDK_VER\shared"
$env:LIB = "$MSVC\lib\x64;$SDK\Lib\$SDK_VER\ucrt\x64;$SDK\Lib\$SDK_VER\um\x64"
$env:PATH = "$MSVC\bin\Hostx64\x64;$env:PATH"
$env:CC = "$MSVC\bin\Hostx64\x64\cl.exe"
$env:CXX = "$MSVC\bin\Hostx64\x64\cl.exe"
$env:AR = "$MSVC\bin\Hostx64\x64\lib.exe"

Push-Location $RepoRoot
try {
  switch ($Mode) {
    'build'   { cargo build -p modular-clipboard }
    'test-ui' { cargo test -p modular-clipboard-ui }
    'test-all' { cargo test -p modular-clipboard-ui -p modular-clipboard-gfx }
    default   { cargo build -p modular-clipboard }
  }
  if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
  Pop-Location
}
Write-Host "✓ $Mode 完成"
