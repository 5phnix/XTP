# Tests of the build farm tools.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Test-Farm.ps1 [-Live]
#
# Without -Live: offline checks of the PC side's helpers (run them under
# Windows PowerShell 5.1, the shell the farm's callers use), the time limits
# included: a program over its time, or one that stops taking stdin, is
# stopped with its whole tree.
# With -Live (XTP_FARM_CONFIG set, from a worktree):
# - a diagnostics build of `xtp` on the farm, through Invoke-FarmCargo.ps1 as
#   cargo-remote calls it, must bring target\diag\xtp.exe and its .pdb back
#   into the worktree, and a second fetch must find nothing that differs;
# - a job over XTP_FARM_JOB_MINUTES=1 must stop with 124, said by the VM;
# - a silent remote command must be cut off at its timeout, its ssh gone;
# - an ssh to the farm whose caller died must be stopped as an orphan.
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

# Timeouts: nothing waits for ever on a stuck program or connection.
$ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$marker = "farm-test-$PID"
$t = Get-Date
$r = Invoke-FarmProcess -FilePath $ps -ArgumentList @('-NoProfile', '-Command', "Start-Sleep 300 # $marker-a") -NoStdin -Capture -TimeoutSeconds 4
$secs = ((Get-Date) - $t).TotalSeconds
Assert-Farm ($r.TimedOut -and $r.ExitCode -eq 255 -and $secs -lt 20) ('a program over its time is stopped (TimedOut after {0:N1} s)' -f $secs)
$left = @(Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" | Where-Object { "$($_.CommandLine)" -like "*$marker-a*" })
Assert-Farm ($left.Count -eq 0) 'the stopped program is gone'
$big = Join-Path $env:TEMP "$marker.bin"
[IO.File]::WriteAllBytes($big, (New-Object byte[] (8MB)))
try {
    $t = Get-Date
    $r = Invoke-FarmProcess -FilePath $ps -ArgumentList @('-NoProfile', '-Command', "Start-Sleep 300 # $marker-b") -StdinFile $big -Capture -StallSeconds 3
    $secs = ((Get-Date) - $t).TotalSeconds
    Assert-Farm ($r.TimedOut -and $secs -lt 20) ('a program that stops taking stdin is stopped (TimedOut after {0:N1} s)' -f $secs)
} finally {
    Remove-Item -Force -LiteralPath $big -ErrorAction SilentlyContinue
}
$left = @(Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" | Where-Object { "$($_.CommandLine)" -like "*$marker-b*" })
Assert-Farm ($left.Count -eq 0) 'the program that stopped taking stdin is gone'

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

    # The job's deadline: a job of over a minute with XTP_FARM_JOB_MINUTES=1
    # is stopped by the VM (124), with nothing left running.
    # (In a child process: farm messages go straight to stderr, which only a
    # child's captured stderr shows.)
    $s = 'test --workspace --locked --exclude xtp-hle'
    $t = Get-Date
    $r = Invoke-FarmProcess -FilePath $ps -WorkingDirectory $wt.Top -NoStdin -Capture -Environment @{ XTP_FARM_JOB_MINUTES = '1' } `
        -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $cargo, '--farm-steps', $s, $s, $s, $s, $s, $s)
    $secs = ((Get-Date) - $t).TotalSeconds
    Assert-Farm ($r.ExitCode -eq 124 -and $secs -lt 150) ("a job over its time stops with 124 (got $($r.ExitCode) after {0:N0} s)" -f $secs)
    Assert-Farm ($r.Err -match 'ran over its time') 'the VM says the job ran over its time'

    # A command that does not answer in time: ssh and its IAP proxy are stopped.
    $vm = (@(Get-FarmInstances -Config $c -Refresh) | Where-Object { Test-FarmRunning $_ } | Select-Object -First 1).name
    $t = Get-Date
    $r = Invoke-FarmSsh -Config $c -Vm $vm -Command "powershell -NoProfile -Command Start-Sleep 120; '$marker-c'" -Capture -TimeoutSeconds 15
    $secs = ((Get-Date) - $t).TotalSeconds
    Assert-Farm ($r.TimedOut -and $secs -lt 40) ('a silent remote command is cut off on time ({0:N0} s)' -f $secs)
    $left = @(Get-CimInstance Win32_Process -Filter "Name='ssh.exe'" | Where-Object { "$($_.CommandLine)" -like "*$marker-c*" })
    Assert-Farm ($left.Count -eq 0) 'its ssh is gone'

    # An ssh whose caller died is an orphan, stopped by the next job.
    $paths = Get-FarmSshPaths $c
    $callerScript = Join-Path $c.tempDir "$marker-caller.ps1"
    [IO.File]::WriteAllText($callerScript, ("& '{0}' -F '{1}' -T -n {2} `"powershell -NoProfile -Command Start-Sleep 120; '{3}-d'`"" -f (Get-FarmSshExe), $paths.Config, $vm, $marker))
    $caller = Start-Process -FilePath $ps -PassThru -WindowStyle Hidden -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $callerScript)
    $ssh = $null
    for ($i = 0; $i -lt 30 -and -not $ssh; $i++) {
        Start-Sleep -Seconds 1
        $ssh = Get-CimInstance Win32_Process -Filter "Name='ssh.exe'" | Where-Object { "$($_.CommandLine)" -like "*$marker-d*" } | Select-Object -First 1
    }
    Assert-Farm ([bool] $ssh) 'an ssh to the farm started'
    if ($ssh) {
        Stop-Process -Id $caller.Id -Force
        $killed = @(Remove-FarmOrphans $c -Force)
        Assert-Farm ($killed -contains [int] $ssh.ProcessId) 'the orphaned ssh is stopped'
        Assert-Farm (-not (Get-Process -Id $ssh.ProcessId -ErrorAction SilentlyContinue)) 'and is gone'
    } else {
        Stop-Process -Id $caller.Id -Force -ErrorAction SilentlyContinue
    }
    Remove-Item -Force -LiteralPath $callerScript -ErrorAction SilentlyContinue
}

if ($script:Failed) { Write-Host "$script:Failed check(s) failed" } else { Write-Host 'all checks passed' }
exit $script:Failed
