# Starts farm VMs ahead of a batch of builds (cargo-remote starts them on
# demand anyway), each only if the daily budget allows it, and waits until
# they answer over SSH.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Start-Farm.ps1 [-Name <vm>,...]
#
# Without -Name: every VM of the configuration.

param([string[]] $Name = @())

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$c = Get-FarmConfig
Initialize-FarmSsh $c | Out-Null
$want = $(if ($Name) { $Name } else { @($c.vms | ForEach-Object { $_.name }) })
$started = @()
$refused = 0
foreach ($n in $want) {
    $inst = @(Get-FarmInstances -Config $c -Refresh)
    $i = $inst | Where-Object { $_.name -eq $n } | Select-Object -First 1
    if (-not $i) { Write-FarmMessage "$n does not exist (New-FarmVm.ps1)"; $refused++; continue }
    if (-not (Test-FarmStopped $i)) { Write-FarmMessage "${n}: $($i.status)"; $started += $n; continue }
    $b = Test-FarmBudget -Config $c -Instances $inst -StartVm $n
    if (-not $b.Ok) { Write-FarmMessage "not starting ${n}: $($b.Message)"; $refused++; continue }
    try {
        Start-FarmVm $c $n
        $started += $n
    } catch {
        Write-FarmMessage "$($_.Exception.Message)"
        $refused++
    }
}
foreach ($n in $started) {
    Wait-FarmVmReady $c $n
    Write-FarmMessage "$n is ready"
}
if ($refused) { exit 1 }
