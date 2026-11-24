# Copies the current farm agent (the scripts of tools\buildfarm\vm) to the
# running farm VMs. Not needed in daily use: a job that reaches a VM with an
# older agent updates it first. Useful after changing vm\farm-agent.ps1.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Update-FarmAgent.ps1 [-Name <vm>,...]

param([string[]] $Name = @())

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$c = Get-FarmConfig
Initialize-FarmSsh $c | Out-Null
foreach ($i in @(Get-FarmInstances -Config $c -Refresh)) {
    if ($Name -and $Name -notcontains $i.name) { continue }
    if (-not (Test-FarmRunning $i)) { Write-FarmMessage "$($i.name) is $($i.status): updated at its next job"; continue }
    $v = Publish-FarmAgent $c $i.name
    Write-FarmMessage "$($i.name): agent $v"
}
