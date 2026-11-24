<#
  Runs ON THE VM, over SSH (run-remote.ps1 calls it). Windows PowerShell 5.1.

  Puts C:\xtp\src on a commit, then runs cargo there in the desktop session
  (some tests need a desktop: an SSH session has none):
  - the source: -Bundle <git bundle on the VM> (your local commits, sent by
    run-remote.ps1 -Local) or the clone's origin; -Ref names the commit
    (default origin/main, or the bundle's head);
  - -Mode check: the workspace check (cargo fmt --check, clippy twice, test);
    -Mode build: the diagnostics build of xtp that game runs use;
    -Mode cargo: cargo with -CargoArgs.
  One job at a time on the clone (lock 'src'). The output is printed as it
  comes and kept in C:\xtp\logs\<date-time>-<mode>.log. Exits with cargo's
  exit code.
#>
param(
    [ValidateSet('check', 'build', 'cargo')] [string]$Mode = 'check',
    [string]$Ref = '',
    [string]$Bundle = '',
    [string]$CargoArgs = '',
    [string]$SessionUser = 'xtp',
    [string]$Owner = '',
    [switch]$NoWait
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\lib.ps1"
if ($Owner) { $script:XtpOwner = $Owner }
if ($CargoArgs -match '"') { throw 'CargoArgs may not contain double quotes.' }
New-Item -ItemType Directory -Force -Path 'C:\xtp\logs' | Out-Null
$log = Join-Path 'C:\xtp\logs' ("{0}-{1}.log" -f (Get-Date -Format 'yyyyMMdd-HHmmss'), $Mode)

Enter-XtpLock 'src' -NoWait:$NoWait
try {
    Set-XtpSource $Bundle $Ref
    $t0 = Get-Date
    $arguments = "-NoProfile -ExecutionPolicy Bypass -File C:\xtp\tools\vm\cargo-steps.ps1 -Mode $Mode"
    if ($CargoArgs) { $arguments += " -CargoArgs `"$CargoArgs`"" }
    $code = Invoke-XtpSessionJob -Exe "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Arguments $arguments -Log $log -User $SessionUser
    Write-Output ("{0} {1} in {2:N0} s (log {3})" -f $Mode, $(if ($code -eq 0) { 'PASSED' } else { "FAILED ($code)" }), ((Get-Date) - $t0).TotalSeconds, $log)
    exit $code
} finally {
    Exit-XtpLock 'src'
}
