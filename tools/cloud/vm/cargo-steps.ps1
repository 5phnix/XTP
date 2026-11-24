<#
  Runs ON THE VM in the desktop session (check.ps1 and run-game.ps1 start it
  through Invoke-XtpSessionJob), in C:\xtp\src. Windows PowerShell 5.1.
    -Mode check   the workspace check: cargo fmt --all --check, clippy
                  (default and all features, -D warnings), cargo test
    -Mode build   the diagnostics build of xtp that game runs use
    -Mode cargo   cargo -CargoArgs
  Stops at the first failing step; exits with its exit code.
#>
param(
    [ValidateSet('check', 'build', 'cargo')] [string]$Mode = 'check',
    [string]$CargoArgs = ''
)
$ErrorActionPreference = 'Continue'
foreach ($v in 'RUSTUP_HOME', 'CARGO_HOME', 'Path') {
    [Environment]::SetEnvironmentVariable($v, [Environment]::GetEnvironmentVariable($v, 'Machine'), 'Process')
}
$env:CARGO_TERM_COLOR = 'never'
Set-Location -LiteralPath 'C:\xtp\src'
$steps = New-Object System.Collections.ArrayList
switch ($Mode) {
    'check' {
        [void]$steps.Add(@('fmt', '--all', '--check'))
        [void]$steps.Add(@('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings'))
        [void]$steps.Add(@('clippy', '--workspace', '--all-targets', '--all-features', '--locked', '--', '-D', 'warnings'))
        [void]$steps.Add(@('test', '--workspace', '--locked'))
    }
    'build' { [void]$steps.Add(@('build', '--profile', 'diag', '-p', 'xtp', '--features', 'diagnostics', '--locked')) }
    'cargo' { [void]$steps.Add(@($CargoArgs -split '\s+' | Where-Object { $_ })) }
}
$t0 = Get-Date
foreach ($s in $steps) {
    $t = Get-Date
    Write-Output "> cargo $($s -join ' ')"
    & cargo @s 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    Write-Output ("< exit {0} after {1:N0} s" -f $code, ((Get-Date) - $t).TotalSeconds)
    if ($code -ne 0) { exit $code }
}
Write-Output ("all steps passed in {0:N0} s" -f ((Get-Date) - $t0).TotalSeconds)
exit 0
