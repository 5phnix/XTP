<#
  Runs ON THE VM, over SSH (run-remote.ps1 -Game calls it). Windows PowerShell 5.1.

  Runs the title under XTP in the desktop session of the automatic-logon
  account (D3D12 swapchains need one; an SSH session has none):
  1. with -Build, puts C:\xtp\src on the commit (-Bundle / -Ref, as check.ps1)
     and builds the diagnostics build, in the desktop session; the clone stays
     locked until the run ends, so nobody else's build replaces the program;
  2. a run folder D:\xtp-data\runs\<date-time>[-name] with a copy of the
     golden data folder D:\golden\data when there is one (never written
     itself) and an xtp.toml (game D:\game, cache D:\xtp-data\cache, data the
     copy);
  3. XTP_CONFIG, XTP_KEYS (by default Enter, the pad's A, every 2.5 s from
     5 s to 50 s, which takes the title past its start screens), XTP_CAPTURE
     (frames as PNG in <run>\capture) and any -Env values;
  4. the title in the desktop session, stopped after -Seconds;
  5. the log without colour codes in <run>\run.log, and a summary. The last
     line names the run folder ("RUN DIR ...").
  The diagnostics build (target\diag\xtp.exe) is needed for XTP_KEYS and XTP_CAPTURE.
#>
param(
    [Parameter(Mandatory)] [string]$TitleExe,
    [int]$Seconds = 180,
    [switch]$Build,
    [string]$Bundle = '',
    [string]$Ref = '',
    # The commit sent (run-remote.ps1 -Local): the build fails unless it is the one checked out.
    [string]$Commit = '',
    [string]$Exe = 'C:\xtp\src\target\diag\xtp.exe',
    # XTP_KEYS syntax; '-' for none. Default: Enter every 2.5 s from 5 s to 50 s.
    [string]$Keys = '',
    # Extra environment, 'NAME=value;NAME2=value2'.
    [string]$Env = '',
    [string]$Name = '',
    [string]$XtpArgs = '',
    [switch]$NoGolden,
    [switch]$NoCapture,
    [string]$SessionUser = 'xtp',
    # Who runs it (shown to whoever waits for the VM), and fail at once when busy.
    [string]$Owner = '',
    [switch]$NoWait
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\lib.ps1"
if ($Owner) { $script:XtpOwner = $Owner }

$title = Join-Path 'D:\game' $TitleExe
if (-not (Test-Path -LiteralPath $title)) { throw "No title at $title (sync-game.ps1 copies the game files)." }
if (-not (Test-XtpSession $SessionUser)) { throw "$SessionUser is not logged on to the console (no desktop session): restart the VM, or run setup-vm.ps1." }

# One session at a time on this VM: the clone stays locked from the build to
# the end of the run ('src'), and the desktop session runs one job ('session').
Enter-XtpLock 'src' -NoWait:$NoWait
try {
    if ($Build) {
        Set-XtpSource $Bundle $Ref $Commit
        $log = Join-Path 'C:\xtp\logs' ("{0}-build.log" -f (Get-Date -Format 'yyyyMMdd-HHmmss'))
        $code = Invoke-XtpSessionJob -Exe "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" `
            -Arguments '-NoProfile -ExecutionPolicy Bypass -File C:\xtp\tools\vm\cargo-steps.ps1 -Mode build' -Log $log -User $SessionUser
        if ($code -ne 0) { Write-Output "build FAILED ($code) (log $log)"; exit $code }
        Write-Output "build PASSED (log $log)"
    }
    if (-not (Test-Path -LiteralPath $Exe)) { throw "No $Exe (run-remote.ps1 -Game builds it)." }
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
    Write-Output "run folder: $run (from $from, $Seconds s)"
    # The GPU's memory and use every 2 s, beside the run.
    $smi = @('C:\Windows\System32\nvidia-smi.exe', 'C:\Program Files\NVIDIA Corporation\NVSMI\nvidia-smi.exe') | Where-Object { Test-Path $_ } | Select-Object -First 1
    $gpuCsv = Join-Path $run 'gpu.csv'
    $sampler = $null
    if ($smi) {
        $sampler = Start-Process -FilePath $smi -PassThru -WindowStyle Hidden -ArgumentList @(
            '--query-gpu=timestamp,memory.used,memory.total,utilization.gpu', '--format=csv', '-lms', '2000', '-f', $gpuCsv)
    }
    # Windows' own view of the adapters' memory (dxgkrnl's counters, every 2 s).
    $vidmmCsv = Join-Path $run 'vidmm.csv'
    $typeperf = Start-Process -FilePath "$env:SystemRoot\System32\typeperf.exe" -PassThru -WindowStyle Hidden -ArgumentList @(
        '"\GPU Adapter Memory(*)\Dedicated Usage"', '"\GPU Adapter Memory(*)\Shared Usage"', '-si', '2', '-f', 'CSV', '-o', "`"$vidmmCsv`"", '-y')
    try {
        $code = Invoke-XtpSessionJob -Exe $Exe -Arguments "run `"$title`" --execute $XtpArgs" -Log $raw -Seconds $Seconds -Environment $envs -User $SessionUser -Quiet
    } finally {
        foreach ($p in $sampler, $typeperf) { if ($p -and -not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue } }
    }
    $result = $script:XtpLastJob
    $result | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $run 'result.json') -Encoding ASCII

    $text = if (Test-Path -LiteralPath $raw) { Get-Content -LiteralPath $raw -Raw } else { '' }
    $esc = [char]27
    $text -replace "$esc\[[0-9;]*m", '' | Set-Content -LiteralPath (Join-Path $run 'run.log') -Encoding UTF8
    Remove-Item -LiteralPath $raw -ErrorAction SilentlyContinue
    $lines = @(Get-Content -LiteralPath (Join-Path $run 'run.log'))

    Write-Output ("exit code {0}, {1}, {2:N0} s" -f $code, $(if ($result.timedOut) { "stopped after $Seconds s" } else { 'ended by itself' }),
        ([datetime]$result.ended - [datetime]$result.started).TotalSeconds)
    Write-Output "log: $($lines.Count) lines"
    $lines | Where-Object { $_ -match 'host D3D12 device|mesh shader support|adapter' } | Select-Object -First 5 | ForEach-Object { "  $_" }
    $fps = @($lines | Where-Object { $_ -match '\d+(\.\d+)? fps' })
    if ($fps.Count) { Write-Output "frame-rate lines: $($fps.Count), the last ones:"; $fps | Select-Object -Last 6 | ForEach-Object { "  $_" } }
    $fail = @($lines | Where-Object { $_ -match 'host GPU failure|device removed|unhandled exception|panicked| ERROR ' } | Select-Object -First 5)
    if ($fail.Count) { Write-Output 'errors:'; $fail | ForEach-Object { "  $($_.Substring(0, [Math]::Min(300, $_.Length)))" } }
    $crash = Get-ChildItem -LiteralPath $data -Recurse -Filter 'crashes.log' -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($crash) { Write-Output "crashes.log ($($crash.FullName)):"; Get-Content -LiteralPath $crash.FullName -Tail 15 | ForEach-Object { "  $_" } }
    $auto = @($lines | Where-Object { $_ -match 'autopilot' } | Select-Object -Last 8)
    if ($auto.Count) { Write-Output 'autopilot:'; $auto | ForEach-Object { "  $($_.Substring(0, [Math]::Min(300, $_.Length)))" } }
    if (Test-Path -LiteralPath $gpuCsv) {
        $rows = @(Import-Csv -LiteralPath $gpuCsv)
        if ($rows.Count) {
            # Columns: timestamp, memory.used, memory.total, utilization.gpu.
            $used = @($rows | ForEach-Object { [int]((@($_.PSObject.Properties.Value)[1]) -replace '[^\d]', '') })
            $total = @($rows[0].PSObject.Properties.Value)[2]
            $peak = ($used | Measure-Object -Maximum).Maximum
            Write-Output ("GPU memory used (every 2 s, {0} samples): peak {1:N0} MiB of {2}, last {3:N0} MiB" -f $rows.Count, $peak, "$total".Trim(), $used[-1])
        }
    }
    $frames = @(Get-ChildItem -LiteralPath (Join-Path $run 'capture') -Filter '*.png' -ErrorAction SilentlyContinue)
    if ($frames.Count) { Write-Output "captured frames: $($frames.Count) (last: $($frames | Sort-Object LastWriteTime | Select-Object -Last 1 -ExpandProperty Name))" }
    Write-Output "RUN DIR $run"
} finally {
    Exit-XtpLock 'src'
}
exit 0
