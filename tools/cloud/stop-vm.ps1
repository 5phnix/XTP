<#
.SYNOPSIS
  Stops the VM (and says what it still costs while stopped).

.DESCRIPTION
  A stopped VM is not billed for its CPUs, memory, GPU or Windows licence;
  its disks (and the bucket) still are. With -Status, only prints the state.

.EXAMPLE
  tools\cloud\stop-vm.ps1
  tools\cloud\stop-vm.ps1 -Status
#>
[CmdletBinding()]
param([string]$Config, [switch]$Status)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$base = Get-XtpBaseArgList $cfg
$zone = Get-XtpVmZone $cfg
if (-not $zone) { Write-Host "No VM named $($cfg.vm) in $($cfg.project)."; return }
$state = Get-XtpVmStatus $cfg $zone
if (-not $Status -and $state -notin 'TERMINATED', 'STOPPED', 'SUSPENDED') {
    Write-Step "Stopping $($cfg.vm) in $zone (was $state)"
    $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'stop', $cfg.vm, "--zone=$zone") + $base)
    $state = Get-XtpVmStatus $cfg $zone
}
$disks = Invoke-Gcloud -Arguments (@('compute', 'disks', 'list', "--filter=zone:($zone) AND users:($($cfg.vm))", '--format=value(name,sizeGb,type.basename())') + $base)
Write-Host "$($cfg.vm): $state ($zone)"
$disks | ForEach-Object { Write-Host "    disk $_" }
