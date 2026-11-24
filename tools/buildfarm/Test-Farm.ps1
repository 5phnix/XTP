# Tests of the build farm tools.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Test-Farm.ps1 [-Live]
#
# Without -Live: offline checks of the PC side's helpers (run them under
# Windows PowerShell 5.1, the shell the farm's callers use).
# With -Live (XTP_FARM_CONFIG set, from a worktree): a diagnostics build of
# `xtp` on the farm, through Invoke-FarmCargo.ps1 as cargo-remote calls it,
# must bring target\diag\xtp.exe and its .pdb back into the worktree, and a
# second fetch must find nothing that differs.
#
# Exit code: the number of failed checks.

param([switch] $Live)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$script:Failed = 0

function Assert-Farm([bool] $Condition, [string] $What) {
    if ($Condition) {
        Write-Host "PASS $What"
    } else {
        Write-Host "FAIL $What"
        $script:Failed++
    }
}

# --- offline ----------------------------------------------------------------

Write-Host "PowerShell $($PSVersionTable.PSVersion)"
$none = @(ConvertFrom-FarmJsonArray '[]')
Assert-Farm ($none.Count -eq 0) 'JSON array: [] has no element'
$one = @(ConvertFrom-FarmJsonArray '[{"path":"target/diag/xtp.exe","size":1}]')
Assert-Farm ($one.Count -eq 1 -and $one[0].path -is [string]) 'JSON array: one object, its path a string'
$two = @(ConvertFrom-FarmJsonArray '[{"path":"target/diag/xtp.exe"},{"path":"target/diag/xtp.pdb"}]')
Assert-Farm ($two.Count -eq 2 -and $two[1].path -eq 'target/diag/xtp.pdb') 'JSON array: two objects, each on its own'
$strings = @(ConvertFrom-FarmJsonArray '["a","b","c"]')
Assert-Farm ($strings.Count -eq 3 -and $strings[2] -eq 'c') 'JSON array: strings'
# What the bug was: Windows PowerShell wraps a whole array as one element.
$wrapped = @('[{"path":"a"},{"path":"b"}]' | ConvertFrom-Json)
if ($PSVersionTable.PSVersion.Major -lt 6) {
    Assert-Farm ($wrapped.Count -eq 1) 'Windows PowerShell: @(... | ConvertFrom-Json) is one element (the trap the helper avoids)'
}
Assert-Farm ((ConvertTo-FarmArg 'a b\') -eq '"a b\\"') 'argument quoting: a trailing backslash inside quotes'
Assert-Farm ((ConvertTo-FarmArg '') -eq '""') 'argument quoting: an empty argument'
Assert-Farm ((ConvertTo-FarmArg 'say "hi"') -eq '"say \"hi\""') 'argument quoting: inner quotes'
$top = 'E:\XTP-worktrees\some-worktree'
$mapped = @(ConvertTo-FarmCargoArgs @('--manifest-path', 'E:/XTP-worktrees/some-worktree/crates/a/Cargo.toml', '-p', 'x') $top)
Assert-Farm ($mapped[1] -eq '@@FARM_SLOT@@/crates/a/Cargo.toml' -and $mapped[3] -eq 'x') 'worktree paths in arguments point to the slot'

# --- live -------------------------------------------------------------------

if ($Live) {
    $c = Get-FarmConfig
    $wt = Get-FarmWorktree (Get-Location).Path
    $exe = Join-Path $wt.Top 'target\diag\xtp.exe'
    $pdb = Join-Path $wt.Top 'target\diag\xtp.pdb'
    foreach ($f in $exe, $pdb) { Remove-Item -Force -LiteralPath $f -ErrorAction SilentlyContinue }
    $cargo = Join-Path $PSScriptRoot 'Invoke-FarmCargo.ps1'
    # As play.ps1 builds it; XTP_FARM_FETCH names the .exe again, which the
    # build brings back anyway: each file must come once.
    $env:XTP_FARM_FETCH = 'target/diag/xtp.exe'
    try {
        & $cargo build --profile diag -p xtp --features diagnostics --locked
        $code = $LASTEXITCODE
    } finally {
        Remove-Item Env:XTP_FARM_FETCH -ErrorAction SilentlyContinue
    }
    Assert-Farm ($code -eq 0) "farm build --profile diag -p xtp exits 0 (got $code)"
    Assert-Farm ((Test-Path -LiteralPath $exe) -and (Get-Item -LiteralPath $exe).Length -gt 0) 'target\diag\xtp.exe came back'
    Assert-Farm ((Test-Path -LiteralPath $pdb) -and (Get-Item -LiteralPath $pdb).Length -gt 0) 'target\diag\xtp.pdb came back'
    # The local copies are the slot's: a second fetch has nothing to bring.
    $before = (Get-Item -LiteralPath $exe).LastWriteTimeUtc
    $out = & $cargo --farm-fetch target/diag/xtp.exe target/diag/xtp.pdb 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    Assert-Farm ($code -eq 0) "a second fetch exits 0 (got $code)"
    Assert-Farm (-not ($out -match 'fetched ')) 'a second fetch finds the local copies identical'
    Assert-Farm ((Get-Item -LiteralPath $exe).LastWriteTimeUtc -eq $before) 'xtp.exe untouched by the second fetch'
}

if ($script:Failed) { Write-Host "$script:Failed check(s) failed" } else { Write-Host 'all checks passed' }
exit $script:Failed
