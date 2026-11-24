<#
  Runs ON THE VM every 5 minutes and at startup, as SYSTEM (the scheduled
  task xtp-idle-shutdown, made by setup-session.ps1). Windows PowerShell 5.1.

  Shuts the VM down once it has been idle for the instance metadata value
  xtp-idle-minutes (default 30; 0 turns this off). A VM shut down from inside
  is STOPPED: its CPU, GPU and Windows licence are no longer billed.

  Busy means any of:
  - an SSH connection (gcloud compute ssh, scp, the tools/cloud scripts);
  - a Remote Desktop session that is connected;
  - a build, test, game run, sync or Claude Code process (see $busyNames);
  - C:\xtp\keep-awake-until holding a time that has not passed yet
    (write one with: (Get-Date).AddHours(2).ToString('o') | Set-Content C:\xtp\keep-awake-until).
#>
$ErrorActionPreference = 'Stop'
$state = 'C:\xtp\logs\idle-since.txt'
$log = 'C:\xtp\logs\idle.log'
New-Item -ItemType Directory -Force -Path 'C:\xtp\logs' | Out-Null

function Write-Log([string]$t) {
    Add-Content -LiteralPath $log -Value "$(Get-Date -Format s) $t"
    $lines = Get-Content -LiteralPath $log
    if ($lines.Count -gt 2000) { $lines | Select-Object -Last 1000 | Set-Content -LiteralPath $log }
}

$minutes = 30
try {
    $v = Invoke-RestMethod -Uri 'http://metadata.google.internal/computeMetadata/v1/instance/attributes/xtp-idle-minutes' `
        -Headers @{ 'Metadata-Flavor' = 'Google' } -TimeoutSec 5 -UseBasicParsing
    $minutes = [int]"$v"
} catch { Write-Verbose "no xtp-idle-minutes metadata: $_" }
if ($minutes -le 0) { exit 0 }

$busyNames = 'cargo', 'rustc', 'rustup', 'link', 'cl', 'clippy-driver', 'rustfmt', 'git', 'xtp', 'python', 'gcloud',
    'vs_installer', 'vs_buildtools', 'setup', 'msiexec', 'claude', 'node'
$reasons = @()
$sshd = @(Get-Process -Name 'sshd', 'sshd-session' -ErrorAction SilentlyContinue).Count
if ($sshd -gt 1) { $reasons += "ssh ($($sshd - 1))" }
$ErrorActionPreference = 'Continue'
$rdp = @(& query.exe session 2>$null | Where-Object { $_ -match 'rdp-tcp#\d+' -and $_ -match '\sActive\s' }).Count
$ErrorActionPreference = 'Stop'
if ($rdp -gt 0) { $reasons += "remote desktop ($rdp)" }
$procs = @(Get-Process -Name $busyNames -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name -Unique)
if ($procs.Count) { $reasons += ($procs -join ',') }
if (Test-Path 'C:\xtp\keep-awake-until') {
    try { if ([datetime](Get-Content 'C:\xtp\keep-awake-until' -Raw).Trim() -gt (Get-Date)) { $reasons += 'keep-awake' } } catch { Write-Verbose "unreadable keep-awake-until: $_" }
}

$boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime
$since = $null
if (Test-Path $state) { try { $since = [datetime](Get-Content $state -Raw).Trim() } catch { $since = $null } }
if ($reasons.Count -or -not $since -or $since -lt $boot) {
    (Get-Date).ToString('o') | Set-Content -LiteralPath $state
    if ($reasons.Count) { Write-Log "busy: $($reasons -join '; ')" }
    exit 0
}
$idle = ((Get-Date) - $since).TotalMinutes
if ($idle -ge $minutes) {
    Write-Log ("idle for {0:N0} min (limit {1}): shutting down" -f $idle, $minutes)
    & shutdown.exe /s /t 60 /d p:0:0 /c "XTP: idle for $minutes minutes, the VM stops (tools\cloud\start-vm.ps1 starts it)."
} else {
    Write-Log ("idle for {0:N0} min (limit {1})" -f $idle, $minutes)
}
