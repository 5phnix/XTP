<#
  Shared by the scripts that run ON THE VM (dot-source it). Windows PowerShell 5.1.

  - Enter-XtpLock / Exit-XtpLock: FIFO tickets for each lock in C:\xtp\jobs.
  - Set-XtpSource: C:\xtp\src on a commit, from a git bundle or from origin.
  - Invoke-XtpSessionJob: runs a program in the desktop session of the
    automatic-logon account (through the scheduled task xtp-session-job), its
    output to a log that is printed while it runs; returns its exit code.
    Builds and tests run there too: some need a desktop (D3D12 swapchains,
    display power requests), which an SSH session does not have.
#>

. "$PSScriptRoot\lock-queue.ps1"
# Who holds the locks this script takes (set by the caller: -Owner).
$script:XtpOwner = 'unknown'
$script:XtpSessionOutstanding = $false
$script:XtpGit = 'C:\Program Files\Git\cmd\git.exe'

# A git command on C:\xtp\src, its output indented; throws on failure.
function Invoke-XtpGit([string[]]$ArgList) {
    $ErrorActionPreference = 'Continue'
    & $script:XtpGit -C 'C:\xtp\src' @ArgList 2>&1 | ForEach-Object { "    $_" }
    if ($LASTEXITCODE -ne 0) { throw "git $($ArgList -join ' ') failed ($LASTEXITCODE)" }
}

# Puts C:\xtp\src on -Ref (default: the bundle's head, or origin/main). With
# -Commit, fails unless that is the commit checked out: callers wait for the
# lock in turn, and each must build what it sent, not another caller's code.
# The bundle is removed once fetched (run-remote.ps1 names each one apart).
function Set-XtpSource([string]$Bundle, [string]$Ref, [string]$Commit = '') {
    if ($Bundle) {
        # The bundle's first ref (HEAD, or the branch run-remote.ps1 -LocalRev named).
        $ErrorActionPreference = 'Continue'
        $heads = @(& $script:XtpGit -C 'C:\xtp\src' bundle list-heads $Bundle 2>$null)
        $ErrorActionPreference = 'Stop'
        if (-not $heads.Count) { throw "No ref in the bundle $Bundle." }
        $name = ("$($heads[0])" -split '\s+')[1]
        Invoke-XtpGit @('fetch', '--quiet', '--force', $Bundle, "+$($name):refs/xtp/local")
        if ((Split-Path -Leaf $Bundle) -ne 'src.bundle') { Remove-Item -LiteralPath $Bundle -Force -ErrorAction SilentlyContinue }
        if (-not $Ref) { $Ref = 'refs/xtp/local' }
    } else {
        Invoke-XtpGit @('fetch', '--quiet', '--prune', 'origin')
        if (-not $Ref) { $Ref = 'origin/main' }
    }
    Invoke-XtpGit @('checkout', '--quiet', '--force', '--detach', $Ref)
    Invoke-XtpGit @('clean', '-fdq', '-e', 'target/')
    Invoke-XtpGit @('log', '-1', '--format=commit %h %s (%cr)')
    if ($Commit) {
        $head = (& $script:XtpGit -C 'C:\xtp\src' rev-parse HEAD) | Select-Object -First 1
        if (-not "$head".StartsWith($Commit)) { throw "C:\xtp\src is on $head, not on $Commit, the commit sent." }
    }
}

function Test-XtpSession([string]$User) {
    $ErrorActionPreference = 'Continue'
    $s = & query.exe user 2>&1 | Where-Object { "$_" -match "^\s*>?$([regex]::Escape($User))\s+console\s+\d+\s+Active" }
    return [bool]$s
}

# Runs Exe with Arguments in the desktop session; stdout and stderr go to Log,
# printed here as they come. Seconds: stop it after that long (0: no limit).
# One session job at a time (the lock 'session'). Returns the exit code, or
# throws when the job could not run.
function Invoke-XtpSessionJob {
    param([string]$Exe, [string]$Arguments, [string]$Log, [string]$WorkDir = 'C:\xtp\src',
        [int]$Seconds = 0, $Environment = @{}, [string]$User = 'xtp', [switch]$Quiet, [switch]$NoWait)
    if (-not (Test-XtpSession $User)) {
        throw "$User is not logged on to the console (no desktop session): restart the VM, or run setup-vm.ps1."
    }
    Enter-XtpLock 'session' -NoWait:$NoWait
    try {
        $id = (Get-Date -Format 'yyyyMMdd-HHmmss') + "-$PID"
        $done = Join-Path $script:XtpJobs "done-$id.json"
        $job = [ordered]@{ exe = $Exe; arguments = $Arguments; workDir = $WorkDir; log = $Log; seconds = $Seconds; env = $Environment; done = $done }
        $job | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $script:XtpJobs 'job.json') -Encoding ASCII
        $script:XtpSessionOutstanding = $true
        Start-ScheduledTask -TaskName 'xtp-session-job'
        $deadline = if ($Seconds -gt 0) { (Get-Date).AddSeconds($Seconds + 180) } else { (Get-Date).AddHours(12) }
        $pos = 0L
        $t0 = Get-Date
        $last = Get-Date
        $nextLockCheck = $t0.AddHours(1)
        while ($true) {
            $finished = Test-Path -LiteralPath $done
            if (Test-Path -LiteralPath $Log) {
                try {
                    $fs = [System.IO.File]::Open($Log, 'Open', 'Read', 'ReadWrite, Delete')
                    $null = $fs.Seek($pos, 'Begin')
                    $text = (New-Object System.IO.StreamReader($fs)).ReadToEnd()
                    $pos = $fs.Position
                    $fs.Close()
                    if ($text -and -not $Quiet) { [Console]::Out.Write($text); $last = Get-Date }
                } catch { Write-Verbose "log not readable yet: $_" }
            }
            if ($finished) { break }
            if ((Get-Date) -ge $nextLockCheck) {
                $holder = Get-XtpQueueRecord ([IO.FileInfo](Join-Path $script:XtpJobs 'session.lock'))
                Write-XtpLockWarning 'session' $holder
                $nextLockCheck = (Get-Date).AddMinutes(5)
            }
            if ((Get-Date) -gt $deadline) { throw "The session job did not finish (no $done): see Task Scheduler, xtp-session-job." }
            if (((Get-Date) - $last).TotalSeconds -ge 60) { [Console]::Out.WriteLine(("    ... {0:N0} s" -f ((Get-Date) - $t0).TotalSeconds)); $last = Get-Date }
            Start-Sleep -Seconds 3
        }
        $script:XtpSessionOutstanding = $false
        $result = Get-Content -LiteralPath $done -Raw | ConvertFrom-Json
        Remove-Item -LiteralPath $done -Force
        if ($result.error) { throw "The session job failed to start: $($result.error)" }
        $script:XtpLastJob = $result
        return [int]$result.exitCode
    } finally {
        Exit-XtpLock 'session'
    }
}
