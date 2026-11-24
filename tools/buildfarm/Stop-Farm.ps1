# Stops farm VMs now (each also stops itself after its idle minutes) and
# waits until GCE reports them stopped. A build running there fails; the
# PC's cargo-remote then retries it on another VM or runs it locally.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Stop-Farm.ps1 [-Name <vm>,...]
#
# Without -Name: every VM of the configuration that is not stopped.

param([string[]] $Name = @())

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$c = Get-FarmConfig
$inst = @(Get-FarmInstances -Config $c -Refresh)
$todo = @($inst | Where-Object { -not (Test-FarmStopped $_) -and (-not $Name -or $Name -contains $_.name) })
foreach ($i in $todo) {
    Write-FarmMessage "stopping $($i.name) ($($i.status))"
    Stop-FarmVm $c $i.name -Async
}
$deadline = (Get-Date).AddMinutes(10)
while ($todo -and (Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 10
    $inst = @(Get-FarmInstances -Config $c -Refresh)
    $todo = @($inst | Where-Object { -not (Test-FarmStopped $_) -and (-not $Name -or $Name -contains $_.name) })
}
foreach ($i in $inst) { Write-FarmMessage "$($i.name): $($i.status)" }
if ($todo) { exit 1 }
