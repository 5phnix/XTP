# Sync and run: cargo for the current worktree, on the build farm.
#
#   Invoke-FarmCargo.ps1 <cargo arguments>                 e.g. test -p xtp-gpu --locked
#   Invoke-FarmCargo.ps1 --farm-steps "<args>" "<args>" ... several cargo commands, one sync
#                                                          (each a space-separated argument list)
#   Invoke-FarmCargo.ps1 --farm-fetch <path> ...           files of this worktree's last farm
#                                                          build, e.g. target/diag/xtp.exe
#
# The worktree's commits the VM lacks and its uncommitted changes go over as
# one git bundle; cargo runs in a slot of the VM with a persistent target
# directory; its output streams here and its exit code is this script's.
# `build` also brings back the executables it built (and their .pdb) when
# they differ from the local ones; XTP_FARM_FETCH="a;b" brings back more
# files after any successful command.
#
# Only commands that read the worktree go to the farm: build, check, clippy,
# test, doc. The others (fmt, run, xtask, bench, update, clippy --fix, ...)
# change local files or must run here: exit 76 without doing anything.
# Exit 75: the farm cannot take the command now (not configured, no VM can
# start, over the daily budget): the caller runs it locally.
#
# No param block on purpose: every argument reaches cargo untouched.
# Configuration: XTP_FARM_CONFIG (docs/dev/build-farm.md).

$CargoArgs = @($args)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')

function Get-Route([string[]] $A) {
    $i = 0
    while ($i -lt $A.Count) {
        $x = $A[$i]
        if ($x.StartsWith('+')) { $i++; continue }
        if (@('--color', '-Z', '--config', '-C') -contains $x) { $i += 2; continue }
        if ($x.StartsWith('-')) { $i++; continue }
        break
    }
    if ($i -ge $A.Count) { return 'local' }
    $sub = $A[$i]
    $rest = @()
    if ($i + 1 -lt $A.Count) { $rest = @($A[($i + 1)..($A.Count - 1)]) }
    $before = @($rest | ForEach-Object -Begin { $stop = $false } -Process { if ($_ -eq '--') { $stop = $true }; if (-not $stop) { $_ } })
    switch ($sub) {
        { @('build', 'b') -contains $_ } { return 'build' }
        { @('check', 'c', 'test', 't') -contains $_ } { return 'remote' }
        'clippy' { if ($before -contains '--fix') { return 'local' } else { return 'remote' } }
        { @('doc', 'd') -contains $_ } { if ($before -contains '--open') { return 'local' } else { return 'remote' } }
        default { return 'local' }
    }
}

if ($env:XTP_FARM -eq 'off') { Write-FarmMessage 'off (XTP_FARM=off)'; exit $ExitUnavailable }

$steps = @()
$labels = @()
$fetch = @("$env:XTP_FARM_FETCH" -split ';' | Where-Object { $_ })
$collect = $false
$fetchOnly = $false
if ($CargoArgs.Count -and $CargoArgs[0] -eq '--farm-steps') {
    $list = @($CargoArgs | Select-Object -Skip 1)
    $n = 0
    foreach ($s in $list) {
        $n++
        $a = @("$s" -split '\s+' | Where-Object { $_ })
        if (-not $a) { continue }
        if ((Get-Route $a) -eq 'local' -and $a[0] -ne 'fmt') {
            Write-FarmMessage "not a farm command: cargo $s"
            exit $ExitLocalOnly
        }
        $steps += , $a
        $labels += "farm step $n/$($list.Count): cargo $s"
    }
} elseif ($CargoArgs.Count -and $CargoArgs[0] -eq '--farm-fetch') {
    $fetch = @($CargoArgs | Select-Object -Skip 1)
    $fetchOnly = $true
} else {
    $route = Get-Route $CargoArgs
    if ($route -eq 'local') { exit $ExitLocalOnly }
    $collect = ($route -eq 'build') -and $env:XTP_FARM_FETCH_BINS -ne '0'
    $steps = , $CargoArgs
}

try {
    $c = Get-FarmConfig
    $cfg = (Get-FarmSshPaths $c).Config
    if (-not (Test-Path -LiteralPath $cfg) -or (Get-Item -LiteralPath $c.configPath).LastWriteTime -gt (Get-Item -LiteralPath $cfg).LastWriteTime) {
        Initialize-FarmSsh $c | Out-Null
    }
    $code = @(Invoke-FarmJob -Config $c -WorkDir (Get-Location).Path -Steps $steps -Labels $labels -CollectBinaries:$collect -Fetch $fetch -FetchOnly:$fetchOnly)[-1]
    exit ([int] $code)
} catch {
    Write-FarmMessage "unavailable: $($_.Exception.Message)"
    exit $ExitUnavailable
}
