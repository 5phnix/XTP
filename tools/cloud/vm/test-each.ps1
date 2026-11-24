<#
  Runs ON THE VM in the desktop session (check.ps1 -Mode tests starts it
  through Invoke-XtpSessionJob), in C:\xtp\src. Windows PowerShell 5.1.

  The GPU test set, each test in a process of its own with a time limit, so
  that a test that hangs (the GPU or the CPU) is stopped and reported
  without taking the others with it:
  - builds the test binaries of -Packages (cargo test --no-run, the test
    profile) and lists their tests; with -Adapter hardware it adds the
    ignored ones whose reason starts with "hardware"
    (#[ignore = "hardware: ..."]: tests that need a hardware adapter);
  - runs them -Parallel at a time, with XTP_TEST_ADAPTER=<-Adapter> and
    XTP_REQUIRE_GPU=1 (a test without its device fails instead of skipping),
    from the package's folder as cargo does, with --nocapture; a test that
    passes after printing "skipped" (xtp-shader's tests return early
    without their device, XTP_REQUIRE_GPU or not) counts as a skip; a test
    still running after -TestTimeout seconds is stopped with its process
    tree (timeout);
  - runs each test that did not pass again, alone: "fails alone" means it
    fails by itself, "passes alone" that it failed only beside others (a GPU
    reset caused by another test, or a race);
  - lists the GPU resets Windows logged meanwhile (System log, event 4101)
    with the tests that were running;
  - writes <-Out>\results.csv (binary, test, outcome, seconds, alone) and
    <-Out>\output\*.txt for each test that did not pass.
  Doc tests are not run (cargo test --doc runs them; none needs a GPU).
  Exits 0 when every test passed the first time, 1 otherwise.
#>
param(
    [string]$Packages = 'xtp-shader,xtp-gpu',
    [ValidateSet('hardware', 'warp')] [string]$Adapter = 'hardware',
    [int]$TestTimeout = 300,
    [int]$Parallel = 8,
    [string]$Filter = '',
    [Parameter(Mandatory)] [string]$Out
)
$ErrorActionPreference = 'Continue'
foreach ($v in 'RUSTUP_HOME', 'CARGO_HOME', 'Path') {
    [Environment]::SetEnvironmentVariable($v, [Environment]::GetEnvironmentVariable($v, 'Machine'), 'Process')
}
$env:CARGO_TERM_COLOR = 'never'
$env:XTP_TEST_ADAPTER = $Adapter
$env:XTP_REQUIRE_GPU = '1'
Set-Location -LiteralPath 'C:\xtp\src'
$outputs = Join-Path $Out 'output'
New-Item -ItemType Directory -Force -Path $outputs | Out-Null
$t0 = Get-Date
Write-Output ("XTP_TEST_ADAPTER={0} XTP_REQUIRE_GPU=1, {1} at a time, {2} s per test" -f $Adapter, $Parallel, $TestTimeout)
Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue | ForEach-Object { Write-Output "display adapter: $($_.Name), driver $($_.DriverVersion)" }

# 1. The test binaries (cargo prints compile errors on stderr, into the log).
$pkgArgs = @($Packages -split '[,\s]+' | Where-Object { $_ } | ForEach-Object { '-p', $_ })
Write-Output "> cargo test $($pkgArgs -join ' ') --locked --no-run"
$messages = & cargo test @pkgArgs --locked --no-run --message-format=json-render-diagnostics
if ($LASTEXITCODE -ne 0) { Write-Output "< build failed (exit $LASTEXITCODE)"; exit $LASTEXITCODE }
$binaries = @(foreach ($m in $messages) {
        if ("$m" -notlike '{"reason":"compiler-artifact"*') { continue }
        $a = "$m" | ConvertFrom-Json
        if (-not ($a.profile.test -and $a.executable)) { continue }
        $dir = Split-Path -Parent $a.manifest_path
        [pscustomobject]@{ Label = "$(Split-Path -Leaf $dir)/$($a.target.name)"; Exe = $a.executable; Dir = $dir }
    })

# 2. Their tests: libtest lists them; run with the ignored names as exact
# filters (and without --ignored), it prints each one's reason and runs none.
function Get-TestName([string[]]$Lines) {
    @($Lines | ForEach-Object { if ("$_" -match '^(.+): test$') { $Matches[1] } })
}
$queue = New-Object System.Collections.ArrayList
foreach ($b in $binaries) {
    $all = Get-TestName (& $b.Exe --list --format terse 2>$null)
    $ignored = Get-TestName (& $b.Exe --list --ignored --format terse 2>$null)
    $hardware = @()
    if ($ignored.Count -and $Adapter -eq 'hardware') {
        $hardware = @(& $b.Exe --exact @ignored 2>$null | ForEach-Object {
                if ("$_" -match '^test (\S+) \.\.\. ignored, hardware') { $Matches[1] }
            })
    }
    $run = @($all | Where-Object { $ignored -notcontains $_ -or $hardware -contains $_ })
    Write-Output ("{0}: {1} tests, {2} ignored, {3} of them for hardware" -f $b.Label, $all.Count, $ignored.Count, $hardware.Count)
    foreach ($t in $run) {
        if ($Filter -and "$($b.Label) $t" -notlike "*$Filter*") { continue }
        [void]$queue.Add([pscustomobject]@{
                Index = $queue.Count; Binary = $b.Label; Exe = $b.Exe; Dir = $b.Dir; Test = $t
                Ignored = ($hardware -contains $t)
                Outcome = ''; Seconds = 0.0; Exit = $null; Alone = ''; Start = $null; End = $null
            })
    }
}
Write-Output ("{0} tests to run" -f $queue.Count)

function Start-Test($Item, [string]$File) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = "$env:SystemRoot\System32\cmd.exe"
    $extra = if ($Item.Ignored) { ' --include-ignored' } else { '' }
    $psi.Arguments = '/d /c ""' + $Item.Exe + '" --exact ' + $Item.Test + $extra + ' --nocapture > "' + $File + '" 2>&1"'
    $psi.WorkingDirectory = $Item.Dir
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.EnvironmentVariables['CARGO_MANIFEST_DIR'] = $Item.Dir
    return [System.Diagnostics.Process]::Start($psi)
}

# What a finished run of one test gave: pass, skip (it passed after saying
# it skipped some or all of its work: xtp-shader's tests return early
# without their device, whatever XTP_REQUIRE_GPU says), fail, crash 0x...
# or "not run".
function Get-Outcome([int]$Code, [string]$Text) {
    if ($Code -eq 0) {
        if ($Text -notmatch 'test result: ok\. 1 passed') { return 'not run' }
        if ($Text -match '(?i)\bskipped\b') { return 'skip' }
        return 'pass'
    }
    if ($Code -eq 101) { return 'fail' }
    return ('crash 0x{0:X8}' -f $Code)
}

# Runs Items, Width at a time; sets each one's outcome (or, with -Alone, its
# Alone field). Prints a line per test, and the end of the output of each one
# that did not pass.
function Invoke-TestPool($Items, [int]$Width, [string]$Tag, [switch]$Alone) {
    $active = New-Object System.Collections.ArrayList
    $i = 0
    while ($i -lt $Items.Count -or $active.Count) {
        while ($i -lt $Items.Count -and $active.Count -lt $Width) {
            $it = $Items[$i]; $i++
            $file = Join-Path $outputs ('{0}-{1:D4}.txt' -f $Tag, $it.Index)
            $start = Get-Date
            if (-not $Alone) { $it.Start = $start }
            [void]$active.Add([pscustomobject]@{ Item = $it; Proc = (Start-Test $it $file); File = $file; Start = $start })
        }
        Start-Sleep -Milliseconds 100
        foreach ($a in @($active)) {
            $seconds = ((Get-Date) - $a.Start).TotalSeconds
            if ($a.Proc.HasExited) {
                $text = Get-Content -LiteralPath $a.File -Raw -ErrorAction SilentlyContinue
                $outcome = Get-Outcome $a.Proc.ExitCode "$text"
                $code = $a.Proc.ExitCode
            } elseif ($seconds -gt $TestTimeout) {
                & taskkill.exe /PID $a.Proc.Id /T /F 2>&1 | Out-Null
                $null = $a.Proc.WaitForExit(30000)
                $outcome = 'timeout'
                $code = $null
            } else { continue }
            $active.Remove($a)
            $it = $a.Item
            if ($Alone) {
                $it.Alone = $outcome
            } else {
                $it.Outcome = $outcome; $it.Seconds = [Math]::Round($seconds, 2); $it.Exit = $code; $it.End = Get-Date
            }
            Write-Output ('{0,-12} {1,7:N1} s  {2}  {3}{4}' -f $outcome, $seconds, $it.Binary, $it.Test, $(if ($Alone) { ' (alone)' } else { '' }))
            if ($outcome -eq 'pass') {
                Remove-Item -LiteralPath $a.File -ErrorAction SilentlyContinue
            } elseif ($outcome -eq 'skip') {
                Get-Content -LiteralPath $a.File -ErrorAction SilentlyContinue | Where-Object { $_ -match '(?i)\bskipped\b' } | ForEach-Object { "    | $_" }
            } else {
                Get-Content -LiteralPath $a.File -Tail 40 -ErrorAction SilentlyContinue | ForEach-Object { "    | $_" }
            }
        }
    }
}

# 3. Every test, then each one that did not pass, alone.
Invoke-TestPool $queue $Parallel 'run'
$bad = @($queue | Where-Object Outcome -notin 'pass', 'skip')
if ($bad.Count) {
    Write-Output ''
    Write-Output "$($bad.Count) tests did not pass: again, one at a time"
    Invoke-TestPool $bad 1 'alone' -Alone
}

# 4. GPU resets (TDR) Windows logged meanwhile, with the tests running then.
$resets = @(Get-WinEvent -FilterHashtable @{ LogName = 'System'; Id = 4101; StartTime = $t0 } -ErrorAction SilentlyContinue)
Write-Output ''
Write-Output ("GPU resets logged (System event 4101): {0}" -f $resets.Count)
foreach ($e in $resets | Sort-Object TimeCreated) {
    $at = $e.TimeCreated
    $during = @($queue | Where-Object { $_.Start -and $_.Start -le $at -and (-not $_.End -or $_.End -ge $at) } | ForEach-Object { "$($_.Binary) $($_.Test)" })
    Write-Output ("  {0:HH:mm:ss.fff}: {1}" -f $at, $(if ($during.Count) { "during $($during -join ', ')" } else { 'during the rerun or between tests' }))
}

# 5. Results.
$queue | Select-Object Binary, Test, Ignored, Outcome, Seconds, Exit, Alone |
    Export-Csv -LiteralPath (Join-Path $Out 'results.csv') -NoTypeInformation -Encoding UTF8
Write-Output ''
Write-Output 'Slowest tests:'
$queue | Sort-Object Seconds -Descending | Select-Object -First 10 | ForEach-Object { '  {0,7:N1} s  {1}  {2}' -f $_.Seconds, $_.Binary, $_.Test }
Write-Output ''
$queue | Group-Object Outcome | Sort-Object Name | ForEach-Object { Write-Output ("{0}: {1}" -f $_.Name, $_.Count) }
foreach ($b in $bad) { Write-Output ("  {0}  {1} {2}, alone: {3}" -f $b.Outcome, $b.Binary, $b.Test, $b.Alone) }
foreach ($s in @($queue | Where-Object Outcome -eq 'skip')) { Write-Output ("  skip  {0} {1}" -f $s.Binary, $s.Test) }
Write-Output ("{0} tests on {1} in {2:N0} s" -f $queue.Count, $Adapter, ((Get-Date) - $t0).TotalSeconds)
if ($bad.Count -or -not $queue.Count) { exit 1 }
exit 0
