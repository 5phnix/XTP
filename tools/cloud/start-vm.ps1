<#
.SYNOPSIS
  Starts the VM and waits until it answers over SSH.

.DESCRIPTION
  Every other script starts the VM when it needs it; this one is for working
  on the VM by hand (invoke-vm.ps1 -Shell, rdp-vm.ps1). The VM stops by
  itself after idleMinutes without activity, and every day at stopSchedule.
  A start can fail when the zone has no GPU free at that moment
  (ZONE_RESOURCE_POOL_EXHAUSTED): try again a few minutes later.

.EXAMPLE
  tools\cloud\start-vm.ps1
#>
[CmdletBinding()]
param([string]$Config)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$zone = Start-XtpVm $cfg
Write-Host "$($cfg.vm) is running in $zone (billed by the second until it stops)."
