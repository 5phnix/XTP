<#
  Runs ON THE VM, over SSH (run-remote.ps1 -Game calls it). Windows PowerShell 5.1.

  Runs the title under XTP in the desktop session of the automatic-logon
  account (D3D12 swapchains need one; an SSH session has none):
  1. a run folder D:\xtp-data\runs\<date-time>[-name] with a copy of the
     golden data folder D:\golden\data when there is one (never written
     itself) and an xtp.toml (game D:\game, cache D:\xtp-data\cache, data the
     copy);
  2. XTP_CONFIG, XTP_KEYS (by default Enter, the pad's A, every 2.5 s from
     5 s to 50 s, which takes the title past its start screens), XTP_CAPTURE
     (frames as PNG in <run>\capture) and any -Env values;
  3. the job for the scheduled task xtp-session-job, which starts the title
     in the desktop session and stops it after -Seconds;
  4. the log without colour codes in <run>\run.log, and a summary.
  Needs the diagnostics build (target\diag\xtp.exe) for XTP_KEYS and XTP_CAPTURE.
#>
param(
    [Parameter(Mandatory)] [string]$TitleExe,
    [int]$Seconds = 180,
    [string]$Exe = 'C:\xtp\src\target\diag\xtp.exe',
    # XTP_KEYS syntax; '-' for none. Default: Enter every 2.5 s from 5 s to 50 s.
    [string]$Keys = '',
    # Extra environment, 'NAME=value;NAME2=value2'.
    [string]$Env = '',
    [string]$Name = '',
    [string]$XtpArgs = '',
    [switch]$NoGolden,
    [switch]$NoCapture,
    [string]$SessionUser = 'xtp'
)
$ErrorActionPreference = 'Stop'
$jobs = 'C:\xtp\jobs'
$lock = Join-Path $jobs 'run.lock'

$title = Join-Path 'D:\game' $TitleExe
if (-not (Test-Path -LiteralPath $title)) { throw "No title at $title (sync-game.ps1 copies the game files)." }
if (-not (Test-Path -LiteralPath $Exe)) { throw "No $Exe (run-remote.ps1 -Game builds it)." }
$ErrorActionPreference = 'Continue'
$session = & query.exe user 2>&1 | Where-Object { "$_" -match "^\s*>?$([regex]::Escape($SessionUser))\s+console\s+\d+\s+Active" }
$ErrorActionPreference = 'Stop'
if (-not $session) { throw "$SessionUser is not logged on to the console (no desktop session): reboot the VM, or run setup-vm.ps1." }

# One game run at a time on this VM.
try {
    $fs = [System.IO.File]::Open($lock, 'CreateNew', 'Write', 'Read')
    $w = New-Object System.IO.StreamWriter($fs); $w.WriteLine($PID); $w.WriteLine((Get-Date).ToString('o')); $w.Close()
} catch [System.IO.IOException] {
    $holder = Get-Content -LiteralPath $lock -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($holder -match '^\d+$' -and -not (Get-Process -Id ([int]$holder) -ErrorAction SilentlyContinue)) {
        Remove-Item -LiteralPath $lock -Force
        throw 'A stale run lock was removed; run again.'
    }
    throw "Another game run holds $lock (pid $holder)."
}
try {
    if (Get-Process -Name xtp -ErrorAction SilentlyContinue) { throw 'An xtp.exe is already running on the VM.' }
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $run = Join-Path 'D:\xtp-data\runs' ($stamp + $(if ($Name) { "-$Name" } else { '' }))
    $data = Join-Path $run 'data'
    New-Item -ItemType Directory -Force -Path $run | Out-Null
    $golden = 'D:\golden\data'
    if (-not $NoGolden -and (Test-Path -LiteralPath $golden) -and (Get-ChildItem -LiteralPath $golden -Force | Select-Object -First 1)) {
        Copy-Item -LiteralPath $golden -Destination $data -Recurse
        $from = 'the golden data folder'
    } else {
        New-Item -ItemType Directory -Force -Path $data | Out-Null
        $from = 'an empty data folder (first boot)'
    }
    @(
        '[paths]'
        "game_dir = 'D:\game'"
        "cache_dir = 'D:\xtp-data\cache'"
        "data_dir = '$data'"
        'test_titles = []'
    ) | Set-Content -LiteralPath (Join-Path $run 'xtp.toml') -Encoding ASCII

    if (-not $Keys) { $Keys = (0..18 | ForEach-Object { '{0}:Enter' -f (5 + 2.5 * $_) }) -join ',' }
    $envs = [ordered]@{ XTP_CONFIG = (Join-Path $run 'xtp.toml') }
    if ($Keys -ne '-') { $envs['XTP_KEYS'] = $Keys }
    if (-not $NoCapture) { $envs['XTP_CAPTURE'] = (Join-Path $run 'capture'); New-Item -ItemType Directory -Force -Path $envs['XTP_CAPTURE'] | Out-Null }
    foreach ($pair in ($Env -split ';' | Where-Object { $_ -match '=' })) {
        $k, $v = $pair -split '=', 2
        $envs[$k.Trim()] = $v
    }
    $envs.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" } | Set-Content -LiteralPath (Join-Path $run 'env.txt') -Encoding ASCII

    $raw = Join-Path $run 'run.raw.log'
    $done = Join-Path $jobs "done-$stamp.json"
    $job = [ordered]@{
        exe = $Exe; arguments = "run `"$title`" --execute $XtpArgs"; workDir = 'C:\xtp\src'
        log = $raw; seconds = $Seconds; env = $envs; done = $done
    }
    $job | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $jobs 'job.json') -Encoding ASCII
    Write-Output "run folder: $run (from $from, $Seconds s)"
    Start-ScheduledTask -TaskName 'xtp-session-job'
    $deadline = (Get-Date).AddSeconds([Math]::Max($Seconds, 60) + 180)
    $t0 = Get-Date
    while (-not (Test-Path -LiteralPath $done)) {
        if ((Get-Date) -gt $deadline) { throw "The session job did not finish (no $done); see Task Scheduler, xtp-session-job." }
        Start-Sleep -Seconds 5
        if (((Get-Date) - $t0).TotalSeconds % 60 -lt 5) { Write-Output ("    running ({0:N0} s)" -f ((Get-Date) - $t0).TotalSeconds) }
    }
    $result = Get-Content -LiteralPath $done -Raw | ConvertFrom-Json
    Move-Item -LiteralPath $done -Destination (Join-Path $run 'result.json') -Force
    if ($result.error) { throw "The session job failed: $($result.error)" }

    $text = if (Test-Path -LiteralPath $raw) { Get-Content -LiteralPath $raw -Raw } else { '' }
    $esc = [char]27
    $text -replace "$esc\[[0-9;]*m", '' | Set-Content -LiteralPath (Join-Path $run 'run.log') -Encoding UTF8
    Remove-Item -LiteralPath $raw -ErrorAction SilentlyContinue
    $lines = Get-Content -LiteralPath (Join-Path $run 'run.log')

    Write-Output ("exit code {0}, {1}, {2:N0} s" -f $result.exitCode, $(if ($result.timedOut) { "stopped after $Seconds s" } else { 'ended by itself' }),
        ([datetime]$result.ended - [datetime]$result.started).TotalSeconds)
    Write-Output "log: $($lines.Count) lines"
    $lines | Where-Object { $_ -match 'host D3D12 device|mesh shader support|adapter|software adapter' } | Select-Object -First 5 | ForEach-Object { "  $_" }
    $fps = @($lines | Where-Object { $_ -match '\d+(\.\d+)? fps' })
    if ($fps.Count) { Write-Output "frame-rate lines: $($fps.Count), the last ones:"; $fps | Select-Object -Last 6 | ForEach-Object { "  $_" } }
    $fail = $lines | Where-Object { $_ -match 'host GPU failure|device removed|unhandled exception|panicked|ERROR' } | Select-Object -First 5
    if ($fail) { Write-Output 'errors:'; $fail | ForEach-Object { "  $($_.Substring(0, [Math]::Min(300, $_.Length)))" } }
    $crash = Get-ChildItem -LiteralPath $data -Recurse -Filter 'crashes.log' -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($crash) { Write-Output "crashes.log ($($crash.FullName)):"; Get-Content -LiteralPath $crash.FullName -Tail 15 | ForEach-Object { "  $_" } }
    $frames = @(Get-ChildItem -LiteralPath (Join-Path $run 'capture') -Filter '*.png' -ErrorAction SilentlyContinue)
    if ($frames.Count) { Write-Output "captured frames: $($frames.Count) (last: $($frames | Sort-Object LastWriteTime | Select-Object -Last 1 -ExpandProperty Name))" }
    Write-Output "RUN DIR $run"
} finally {
    $holder = Get-Content -LiteralPath $lock -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($holder -eq "$PID") { Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue }
}
exit 0
