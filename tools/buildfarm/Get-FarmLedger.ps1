# The build farm's ledger: machine-hours and spend per day (local time),
# from each VM's start and stop times as GCE reports them, priced at the list
# prices of prices.json, plus the fixed daily cost (disks, image, Cloud NAT).
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Get-FarmLedger.ps1 [-Days 7] [-Intervals]
#
# An estimate from list prices: the Cloud Billing reports of the project are
# the reference.

param([int] $Days = 7, [switch] $Intervals)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$c = Get-FarmConfig
$inst = @(Get-FarmInstances -Config $c -Refresh)

$rows = for ($d = $Days - 1; $d -ge 0; $d--) {
    $day = (Get-Date).Date.AddDays(-$d)
    $s = Get-FarmSpend -Config $c -Day $day -Instances $inst
    [pscustomobject]@{
        Day          = $day.ToString('yyyy-MM-dd')
        'VM hours'   = '{0:N2}' -f $s.MachineHours
        'VMs USD'    = '{0:N2}' -f $s.MachineUsd
        'Fixed USD'  = '{0:N2}' -f $s.FixedUsd
        'Total USD'  = '{0:N2}' -f $s.TotalUsd
        'Cap USD'    = '{0:N2}' -f $s.CapUsd
        'Per VM (h)' = (($s.HoursPerVm.Keys | Sort-Object | ForEach-Object { '{0} {1:N2}' -f $_, $s.HoursPerVm[$_] }) -join ', ')
    }
}
$rows | Format-Table -AutoSize | Out-String -Width 220 | Write-Host

if ($Intervals) {
    $ledger = Read-FarmJson (Join-Path $c.stateDir 'ledger.json')
    $from = (Get-Date).Date.AddDays(1 - $Days).ToUniversalTime()
    $list = foreach ($r in @($ledger.intervals)) {
        if (-not $r) { continue }
        $s = ConvertTo-FarmUtc $r.start
        $e = $(if ($r.end) { ConvertTo-FarmUtc $r.end } else { $null })
        if ($e -and $e -lt $from) { continue }
        $h = ($(if ($e) { $e } else { (Get-Date).ToUniversalTime() }) - $s).TotalHours
        [pscustomobject]@{
            VM      = $r.vm
            Type    = $r.machineType
            Start   = $s.ToLocalTime().ToString('yyyy-MM-dd HH:mm:ss')
            Stop    = $(if ($e) { $e.ToLocalTime().ToString('yyyy-MM-dd HH:mm:ss') } else { 'running' })
            Hours   = '{0:N3}' -f $h
            'USD/h' = '{0:N3}' -f [double] $r.usdPerHour
            USD     = '{0:N2}' -f ($h * [double] $r.usdPerHour)
        }
    }
    $list | Format-Table -AutoSize | Out-String -Width 220 | Write-Host
}
