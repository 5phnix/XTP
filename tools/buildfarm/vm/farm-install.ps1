# Installs or updates the build farm agent on this VM. Publish-FarmAgent (on
# the PC) has just unpacked the scripts of tools\buildfarm\vm into
# C:\farm\bin; this registers farm-agent.ps1 as a startup task running as
# SYSTEM and restarts it.
#
# Windows PowerShell 5.1. ASCII only.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Root = 'C:\farm'
foreach ($d in 'bin', 'w', 'tmp', 'log', 'locks', 'run', 'state') { New-Item -ItemType Directory -Force -Path (Join-Path $Root $d) | Out-Null }
if (-not (Test-Path (Join-Path $Root 'farm-env.json'))) { throw 'C:\farm\farm-env.json is missing: the toolchain setup has not run here' }

$name = 'XtpFarmAgent'
$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File $Root\bin\farm-agent.ps1"
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $name -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
Stop-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" | Where-Object { $_.CommandLine -match 'farm-agent\.ps1' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Start-ScheduledTask -TaskName $name
Write-Output ("farm agent {0} installed on {1}" -f (Get-Content -Raw (Join-Path $Root 'bin\version.txt')).Trim(), $env:COMPUTERNAME)
