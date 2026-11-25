# Builds tools\pix\xtp-pix.cpp into E:\xtp-data\pix\bin\xtp-pix.exe (never in
# the repository) with the Visual Studio C++ tools and the PIX API headers.
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\pix\build.ps1 [-Out <dir>] [-Pix <PIX folder>]
param([string]$Out = 'E:\xtp-data\pix\bin', [string]$Pix = '')
$ErrorActionPreference = 'Stop'
if (-not $Pix) {
    $Pix = Get-ChildItem 'C:\Program Files\Microsoft PIX' -Directory |
        Where-Object { Test-Path (Join-Path $_.FullName 'include\PixApi.h') } |
        Sort-Object Name | Select-Object -Last 1 -ExpandProperty FullName
}
if (-not $Pix) { throw 'no PIX folder with include\PixApi.h (PIX 2611 or newer)' }
$vswhere = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'no Visual Studio with the C++ tools' }
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
New-Item -ItemType Directory -Force $Out | Out-Null
$src = Join-Path $PSScriptRoot 'xtp-pix.cpp'
$obj = Join-Path $Out 'obj'
New-Item -ItemType Directory -Force $obj | Out-Null
$cl = "cl /nologo /std:c++20 /EHsc /O2 /W3 /utf-8 /I`"$Pix\include`" /I`"$PSScriptRoot`" /Fo`"$obj\\`" /Fe`"$Out\xtp-pix.exe`" `"$src`" ole32.lib shlwapi.lib"
cmd /c "`"$vcvars`" >nul && $cl"
if ($LASTEXITCODE) { throw "cl failed ($LASTEXITCODE)" }
"built $Out\xtp-pix.exe against $Pix"
