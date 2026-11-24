# The build farm at a glance: each VM's state, price and current jobs, the
# slots this PC uses there, and today's spend against the daily cap.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Get-FarmStatus.ps1

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$c = Get-FarmConfig
$inst = @(Get-FarmInstances -Config $c -Refresh)
$prices = Get-FarmPrices $c
$leases = @(Get-FarmLeases -Config $c)
$slots = @(Read-FarmJson (Join-Path $c.stateDir 'slots.json') @())
$bad = Get-FarmUnhealthy $c

$rows = foreach ($vm in @($c.vms)) {
    $i = $inst | Where-Object { $_.name -eq $vm.name } | Select-Object -First 1
    [pscustomobject]@{
        VM       = $vm.name
        Zone     = $vm.zone
        Type     = $(if ($i) { $i.machineType } else { $vm.machineType })
        Status   = $(if ($i) { $i.status } else { 'not created' })
        'USD/h'  = $(try { '{0:N3}' -f (Get-FarmRate $c $prices $vm.machineType $vm.zone) } catch { '?' })
        Jobs     = @($leases | Where-Object { $_.vm -eq $vm.name }).Count
        Slots    = @($slots | Where-Object { $_.vm -eq $vm.name }).Count
        Since    = $(if ($i -and $i.status -eq 'RUNNING' -and $i.lastStart) { (ConvertTo-FarmUtc $i.lastStart).ToLocalTime().ToString('HH:mm') } else { '' })
        Note     = $(if ($bad.ContainsKey($vm.name)) { $bad[$vm.name] } else { '' })
    }
}
$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host

foreach ($l in $leases) {
    Write-Host ('job: {0} on {1} slot {2} since {3}' -f (Split-Path -Leaf $l.worktree), $l.vm, $l.slot, (ConvertTo-FarmUtc $l.started).ToLocalTime().ToString('HH:mm:ss'))
}
$s = Get-FarmSpend -Config $c -Instances $inst
Write-Host ('today: {0:N2} USD of {1:N2} USD cap ({2:N2} h of VMs = {3:N2} USD, fixed costs {4:N2} USD)' -f `
        $s.TotalUsd, $s.CapUsd, $s.MachineHours, $s.MachineUsd, $s.FixedUsd)
if ($prices) { Write-Host "prices: $($prices.source), $($prices.fetched)" } else { Write-Host 'prices: none yet (Update-FarmPrices.ps1)' }
$img = Read-FarmJson (Join-Path $c.stateDir 'image.json')
if ($img) { Write-Host "image: $($img.name) ($($img.archiveGb) GB stored)" }
