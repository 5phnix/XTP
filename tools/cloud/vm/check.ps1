<#
  Runs ON THE VM, over SSH (run-remote.ps1 calls it). Windows PowerShell 5.1.

  Puts C:\xtp\src on a commit, then runs cargo there:
  - the source: -Bundle <git bundle on the VM> (your local commits, sent by
    run-remote.ps1 -Local) or the clone's origin; -Ref names the commit
    (default origin/main, or the bundle's head);
  - -Mode check: the workspace check (cargo fmt --check, clippy twice, test);
    -Mode build: the diagnostics build of xtp that game runs use;
    -Mode cargo: cargo with -CargoArgs.
  One job at a time (C:\xtp\jobs\src.lock). The output goes to the console
  and to C:\xtp\logs\<date-time>-<mode>.log. Exits with cargo's exit code.
#>
param(
    [ValidateSet('check', 'build', 'cargo')] [string]$Mode = 'check',
    [string]$Ref = '',
    [string]$Bundle = '',
    [string]$CargoArgs = '',
    [string]$Src = 'C:\xtp\src'
)
$ErrorActionPreference = 'Stop'
$git = 'C:\Program Files\Git\cmd\git.exe'
$env:RUSTUP_HOME = [Environment]::GetEnvironmentVariable('RUSTUP_HOME', 'Machine')
$env:CARGO_HOME = [Environment]::GetEnvironmentVariable('CARGO_HOME', 'Machine')
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine')
$env:CARGO_TERM_COLOR = 'never'
$lock = 'C:\xtp\jobs\src.lock'
$log = Join-Path 'C:\xtp\logs' ("{0}-{1}.log" -f (Get-Date -Format 'yyyyMMdd-HHmmss'), $Mode)

# Runs a program, its output to the console and the log; returns its exit code.
function Invoke-Logged([string]$Exe, [string[]]$ArgList) {
    "> $Exe $($ArgList -join ' ')" | Tee-Object -FilePath $log -Append | Out-Host
    $ErrorActionPreference = 'Continue'
    & $Exe @ArgList 2>&1 | ForEach-Object { "$_" } | Tee-Object -FilePath $log -Append | Out-Host
    return $LASTEXITCODE
}

New-Item -ItemType Directory -Force -Path 'C:\xtp\logs', 'C:\xtp\jobs' | Out-Null
$waited = $false
while ($true) {
    try { $fs = [System.IO.File]::Open($lock, 'CreateNew', 'Write', 'Read'); $w = New-Object System.IO.StreamWriter($fs); $w.WriteLine($PID); $w.Close(); break }
    catch [System.IO.IOException] {
        $holder = Get-Content -LiteralPath $lock -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($holder -match '^\d+$' -and -not (Get-Process -Id ([int]$holder) -ErrorAction SilentlyContinue)) { Remove-Item -LiteralPath $lock -Force; continue }
        if (-not $waited) { Write-Output "waiting for another job on $Src (pid $holder)"; $waited = $true }
        Start-Sleep -Seconds 10
    }
}
try {
    if ($Bundle) {
        if ((Invoke-Logged $git @('-C', $Src, 'fetch', '--quiet', '--force', $Bundle, '+HEAD:refs/xtp/local')) -ne 0) { throw 'git fetch from the bundle failed' }
        if (-not $Ref) { $Ref = 'refs/xtp/local' }
    } else {
        if ((Invoke-Logged $git @('-C', $Src, 'fetch', '--quiet', '--prune', 'origin')) -ne 0) { throw 'git fetch failed' }
        if (-not $Ref) { $Ref = 'origin/main' }
    }
    if ((Invoke-Logged $git @('-C', $Src, 'checkout', '--quiet', '--force', '--detach', $Ref)) -ne 0) { throw "git checkout $Ref failed" }
    $null = Invoke-Logged $git @('-C', $Src, 'clean', '-fdq', '-e', 'target/')
    $null = Invoke-Logged $git @('-C', $Src, 'log', '-1', '--format=commit %h %s (%cr)')
    Push-Location $Src
    $code = 0
    $t0 = Get-Date
    try {
        switch ($Mode) {
            'check' {
                $steps = @(
                    @('fmt', '--all', '--check'),
                    @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings'),
                    @('clippy', '--workspace', '--all-targets', '--all-features', '--locked', '--', '-D', 'warnings'),
                    @('test', '--workspace', '--locked')
                )
                foreach ($s in $steps) { $code = Invoke-Logged 'cargo' $s; if ($code -ne 0) { break } }
            }
            'build' { $code = Invoke-Logged 'cargo' @('build', '--profile', 'diag', '-p', 'xtp', '--features', 'diagnostics', '--locked') }
            'cargo' { $code = Invoke-Logged 'cargo' ($CargoArgs -split '\s+' | Where-Object { $_ }) }
        }
    } finally { Pop-Location }
    ("{0} {1} in {2:N0} s (log {3})" -f $Mode, $(if ($code -eq 0) { 'PASSED' } else { "FAILED ($code)" }), ((Get-Date) - $t0).TotalSeconds, $log) | Tee-Object -FilePath $log -Append
    exit $code
} finally {
    Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue
}
