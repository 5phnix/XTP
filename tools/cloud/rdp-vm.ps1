<#
.SYNOPSIS
  Opens Remote Desktop to the VM through an IAP tunnel (no public port).

.DESCRIPTION
  Starts the VM if needed, opens `gcloud compute start-iap-tunnel` from
  localhost:<Port> to the VM's port 3389 in a separate window, and starts
  mstsc on it. Close the tunnel's window when you are done.

  You need a Windows password for your account on the VM: -NewPassword runs
  `gcloud compute reset-windows-password`, which prints a new one in this
  console only. Note it in your password manager; never put it in a file in
  the repository or in a chat with an agent.

  Log on as yourself, not as the automatic-logon account (sessionUser): taking
  over its console session would leave its desktop disconnected afterwards
  and game runs would fail until the VM restarts.

.EXAMPLE
  tools\cloud\rdp-vm.ps1 -NewPassword
  tools\cloud\rdp-vm.ps1
#>
[CmdletBinding()]
param([string]$Config, [int]$Port = 13389, [switch]$NewPassword)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$zone = Start-XtpVm $cfg
if ($NewPassword) {
    # Printed by gcloud to this console only.
    & gcloud compute reset-windows-password $cfg.vm "--zone=$zone" "--project=$($cfg.project)" "--user=$($cfg.sshUser)"
}
$tunnel = Start-Process -FilePath 'cmd.exe' -PassThru -ArgumentList '/c', 'gcloud', 'compute', 'start-iap-tunnel', $cfg.vm, '3389',
    "--local-host-port=localhost:$Port", "--zone=$zone", "--project=$($cfg.project)"
Start-Sleep -Seconds 5
Start-Process -FilePath 'mstsc.exe' -ArgumentList "/v:localhost:$Port"
Write-Host "Remote Desktop on localhost:$Port (tunnel window pid $($tunnel.Id); close it when done). User: $($cfg.sshUser)"
