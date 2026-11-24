<#
.SYNOPSIS
  Fills the VM's data disk from the bucket: the game files to D:\game, the
  golden data folder to D:\golden\data. Safe to run again: only what changed
  is copied.

.DESCRIPTION
  Starts the VM if needed, updates C:\xtp\tools\vm, then runs vm\sync-game.ps1
  on the VM. The copy runs inside Google's network (bucket and VM in the same
  region: fast, and free of transfer charges); your PC only sends the command.
  Run upload-game.ps1 first.

.EXAMPLE
  tools\cloud\sync-game.ps1
#>
[CmdletBinding()]
param(
    [string]$Config,
    # Also delete files on D:\game that the bucket no longer has.
    [switch]$Mirror
)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$zone = Start-XtpVm $cfg
if ($zone -notlike "$($cfg.region)-*") { Write-Warning "The VM ($zone) is outside the bucket's region ($($cfg.region)): this copy is billed as inter-region transfer." }
Send-XtpVmToolFolder $cfg $zone
Write-Step "gs://$($cfg.bucket) -> $($cfg.vm) D:\"
$null = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'sync-game.ps1' -Arguments @{
    Bucket = $cfg.bucket; GamePrefix = $cfg.gamePrefix; GoldenPrefix = $cfg.goldenPrefix; Mirror = $Mirror.IsPresent
}
