<#
.SYNOPSIS
  Starts the VM and waits until it answers over SSH.

.DESCRIPTION
  Every other script starts the VM when it needs it; this one is for working
  on the VM by hand (invoke-vm.ps1 -Shell, rdp-vm.ps1). The VM stops by
  itself after idleMinutes without activity, and every day at stopSchedule.
  A stopped VM keeps its zone, which may have no GPU free at that moment
  (ZONE_RESOURCE_POOL_EXHAUSTED): the start is tried again every 2 minutes
  for -RetryMinutes (default: startRetryMinutes in the configuration, 20).

.EXAMPLE
  tools\cloud\start-vm.ps1
  tools\cloud\start-vm.ps1 -RetryMinutes 120
#>
[CmdletBinding()]
param([string]$Config, [int]$RetryMinutes = -1)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$zone = Start-XtpVm $cfg $RetryMinutes
Write-Host "$($cfg.vm) is running in $zone (billed by the second until it stops)."
