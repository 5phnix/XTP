<#
  Runs ON THE VM, over SSH (run-remote.ps1 calls it). Windows PowerShell 5.1.

  Puts C:\xtp\src on a commit, then runs cargo there in the desktop session
  (some tests need a desktop: an SSH session has none):
  - the source: -Bundle <git bundle on the VM> (your local commits, sent by
    run-remote.ps1 -Local) or the clone's origin; -Ref names the commit
    (default origin/main, or the bundle's head);
  - -Mode check: the workspace check (cargo fmt --check, clippy twice, test);
    -Mode build: the diagnostics build of xtp that game runs use;
    -Mode cargo: cargo with -CargoArgs, and the variables of -Env
    ('NAME=value;NAME=value');
    -Mode tests: the GPU test set of -Packages, each test alone with a time
    limit, on -Adapter (vm\test-each.ps1); its log, results.csv and the
    output of the tests that did not pass go to C:\xtp\logs\<date-time>-tests\.
  One job at a time on the clone (lock 'src'). The output is printed as it
  comes and kept in C:\xtp\logs\<date-time>-<mode>.log. Exits with cargo's
  exit code.
#>
param(
    [ValidateSet('check', 'build', 'cargo', 'tests')] [string]$Mode = 'check',
    [string]$Ref = '',
    [string]$Bundle = '',
    # The commit sent (run-remote.ps1 -Local): fails unless it is the one checked out.
    [string]$Commit = '',
    [string]$CargoArgs = '',
    [string]$Env = '',
    [string]$Packages = 'xtp-shader,xtp-gpu',
    [ValidateSet('hardware', 'warp')] [string]$Adapter = 'hardware',
    [int]$TestTimeout = 300,
    [int]$Parallel = 8,
    [string]$Filter = '',
    [string]$SessionUser = 'xtp',
    [string]$Owner = '',
    [switch]$NoWait
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\lib.ps1"
if ($Owner) { $script:XtpOwner = $Owner }
if ($CargoArgs -match '"' -or $Filter -match '"' -or $Packages -match '"') { throw 'CargoArgs, Filter and Packages may not contain double quotes.' }
New-Item -ItemType Directory -Force -Path 'C:\xtp\logs' | Out-Null
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$log = Join-Path 'C:\xtp\logs' ("{0}-{1}.log" -f $stamp, $Mode)
$envs = @{}
foreach ($pair in ($Env -split ';' | Where-Object { $_ -match '=' })) {
    $k, $v = $pair -split '=', 2
    $envs[$k.Trim()] = $v
}

Enter-XtpLock 'src' -NoWait:$NoWait
try {
    Set-XtpSource $Bundle $Ref $Commit
    $t0 = Get-Date
    if ($Mode -eq 'tests') {
        $dir = Join-Path 'C:\xtp\logs' "$stamp-tests"
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        $log = Join-Path $dir 'run.log'
        $arguments = "-NoProfile -ExecutionPolicy Bypass -File C:\xtp\tools\vm\test-each.ps1 -Packages `"$Packages`" -Adapter $Adapter -TestTimeout $TestTimeout -Parallel $Parallel -Out `"$dir`""
        if ($Filter) { $arguments += " -Filter `"$Filter`"" }
    } else {
        $arguments = "-NoProfile -ExecutionPolicy Bypass -File C:\xtp\tools\vm\cargo-steps.ps1 -Mode $Mode"
        if ($CargoArgs) { $arguments += " -CargoArgs `"$CargoArgs`"" }
    }
    $code = Invoke-XtpSessionJob -Exe "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Arguments $arguments -Log $log -Environment $envs -User $SessionUser
    Write-Output ("{0} {1} in {2:N0} s (log {3})" -f $Mode, $(if ($code -eq 0) { 'PASSED' } else { "FAILED ($code)" }), ((Get-Date) - $t0).TotalSeconds, $log)
    exit $code
} finally {
    Exit-XtpLock 'src'
}
