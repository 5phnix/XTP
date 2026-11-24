# Reads today's list prices of the farm's machine types, disks and Cloud NAT
# from the Cloud Billing Catalog API into prices.json (the ledger and the
# budget cap use them). Spot prices change up to once a month: run it again
# now and then (New-FarmImage.ps1 does when its copy is a week old).
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Update-FarmPrices.ps1

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$c = Get-FarmConfig
$p = Update-FarmPrices $c
foreach ($k in $p.machines.Keys) {
    $m = $p.machines[$k]
    Write-Host ('{0}: {1:N4} USD/h = {2} vCPU x ({3:N5} Spot + {4:N3} Windows licence) + {5:N1} GiB x {6:N5} + NAT {7:N4}' -f `
            $k, $m.usdPerHour, $m.vcpus, $m.spotCoreHour, $m.licenseCoreHour, $m.memoryGiB, $m.spotRamGiBHour, $m.natVmHour)
}
foreach ($k in $p.diskGbMonth.Keys) { Write-Host ('{0} disk: {1:N3} USD per GB-month' -f $k, $p.diskGbMonth[$k]) }
Write-Host ('image storage {0:N3} USD per GB-month, Cloud NAT address {1:N4} USD/h' -f $p.imageGbMonth, $p.natIpHour)
