<#
.SYNOPSIS
  Sets the VM up for XTP: GPU driver, build tools, Rust, the XTP clone, the
  data disk, the desktop session for game runs and the idle shutdown. Safe
  to run again (each step checks first); it also updates the VM's copy of
  tools\cloud\vm.

.DESCRIPTION
  Starts the VM if it is stopped, copies tools\cloud\vm to C:\xtp\tools\vm,
  then runs on the VM, over SSH through IAP:
    1. vm\setup-toolchain.ps1 (D: data disk, NVIDIA driver, Git, Visual
       Studio Build Tools, rustup, C:\xtp\src and the pinned toolchain);
       reboots when the driver or the build tools ask for it;
    2. vm\setup-session.ps1 (the automatic-logon account and the scheduled
       tasks), then reboots so that the account's desktop session starts;
    3. vm\d3d12-check.ps1 (the GPU as D3D12 sees it).
  The first run takes 30 to 45 minutes, mostly the build tools.
  The VM keeps running afterwards: sync-game.ps1 is usually next.

.EXAMPLE
  tools\cloud\setup-vm.ps1
#>
[CmdletBinding()]
param(
    [string]$Config,
    # Give the session account a new random password (when its automatic logon fails).
    [switch]$ResetSessionPassword
)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$zone = Start-XtpVm $cfg

function Restart-XtpVmAndWait([string]$Why) {
    Write-Step "Rebooting the VM ($Why)"
    $null = Invoke-XtpVm -Cfg $cfg -Zone $zone -Script "& shutdown.exe /r /t 5 /d p:4:1 /c 'XTP setup: $Why'"
    Start-Sleep -Seconds 60
    Wait-XtpVmSsh $cfg $zone
}

Write-Step "Copying tools\cloud\vm to C:\xtp\tools\vm"
Send-XtpVmToolFolder $cfg $zone

Write-Step 'Toolchain (driver, Git, Build Tools, Rust, the clone)'
$out = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'setup-toolchain.ps1' -Arguments @{ RepoUrl = $cfg.repoUrl; DataDiskGb = [int]$cfg.dataDiskGb }
if ($out -match 'REBOOT NEEDED') {
    Restart-XtpVmAndWait 'driver or build tools'
    # Once more after the reboot: confirms every step and finishes any left.
    $out = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'setup-toolchain.ps1' -Arguments @{ RepoUrl = $cfg.repoUrl; DataDiskGb = [int]$cfg.dataDiskGb }
}

Write-Step "Desktop session ($($cfg.sessionUser), automatic logon) and idle shutdown"
$null = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'setup-session.ps1' -Arguments @{ User = $cfg.sessionUser; ResetPassword = $ResetSessionPassword.IsPresent }
$sessionPattern = "^\s*>?$([regex]::Escape($cfg.sessionUser))\s+console\s+\d+\s+Active"
$sessions = Invoke-XtpVm -Cfg $cfg -Zone $zone -Script '& query.exe user 2>&1 | ForEach-Object { "$_" }' -AllowFail
if ($ResetSessionPassword -or -not ($sessions -match $sessionPattern)) {
    Restart-XtpVmAndWait 'automatic logon'
    $sessions = Invoke-XtpVm -Cfg $cfg -Zone $zone -Script '& query.exe user 2>&1 | ForEach-Object { "$_" }' -AllowFail
}
$sessions | ForEach-Object { Write-Host "    $_" }
if (-not ($sessions -match $sessionPattern)) {
    Write-Warning "$($cfg.sessionUser) is not logged on to the console: game runs need it. Try -ResetSessionPassword."
}

Write-Step 'D3D12 device check'
$null = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'd3d12-check.ps1' -AllowFail
if ($LASTEXITCODE -ne 0) { Write-Warning 'The D3D12 check failed: see above (driver, adapter order).' }

Write-Host ''
Write-Host "Set up. Next: tools\cloud\sync-game.ps1, then tools\cloud\run-remote.ps1 -Check. The VM stops by itself after $($cfg.idleMinutes) idle minutes (or tools\cloud\stop-vm.ps1)."
