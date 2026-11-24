<#
  Shared by the scripts that run ON THE VM (dot-source it). Windows PowerShell 5.1.

  - Enter-XtpLock / Exit-XtpLock: a lock file in C:\xtp\jobs holding the pid of
    its owner; a lock whose owner is gone is stale and taken over.
  - Set-XtpSource: C:\xtp\src on a commit, from a git bundle or from origin.
  - Invoke-XtpSessionJob: runs a program in the desktop session of the
    automatic-logon account (through the scheduled task xtp-session-job), its
    output to a log that is printed while it runs; returns its exit code.
    Builds and tests run there too: some need a desktop (D3D12 swapchains,
    display power requests), which an SSH session does not have.
#>

$script:XtpJobs = 'C:\xtp\jobs'
# Who holds the locks this script takes (set by the caller: -Owner).
$script:XtpOwner = 'unknown'
$script:XtpGit = 'C:\Program Files\Git\cmd\git.exe'

function Enter-XtpLock([string]$Name, [switch]$NoWait) {
    $lock = Join-Path $script:XtpJobs "$Name.lock"
    New-Item -ItemType Directory -Force -Path $script:XtpJobs | Out-Null
    $said = $false
    while ($true) {
        try {
            $fs = [System.IO.File]::Open($lock, 'CreateNew', 'Write', 'Read')
            $w = New-Object System.IO.StreamWriter($fs)
            # pid, since, who (run-remote.ps1 passes the Hangar pane or user, and the worktree).
            $w.WriteLine($PID); $w.WriteLine((Get-Date).ToString('o')); $w.WriteLine($script:XtpOwner); $w.Close()
            return
        } catch [System.IO.IOException] {
            $info = @(Get-Content -LiteralPath $lock -ErrorAction SilentlyContinue)
            $holder = $info | Select-Object -First 1
            if ($holder -match '^\d+$' -and -not (Get-Process -Id ([int]$holder) -ErrorAction SilentlyContinue)) {
                Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue
                continue
            }
            $who = "pid $holder, $($info[2]), since $($info[1])"
            if ($NoWait) { throw "The VM is busy: $Name is held by $who." }
            if (-not $said) { [Console]::Out.WriteLine("waiting for $Name, held by $who"); $said = $true }
            Start-Sleep -Seconds 10
        }
    }
}

function Exit-XtpLock([string]$Name) {
    $lock = Join-Path $script:XtpJobs "$Name.lock"
    $holder = Get-Content -LiteralPath $lock -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($holder -eq "$PID") { Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue }
}

# A git command on C:\xtp\src, its output indented; throws on failure.
function Invoke-XtpGit([string[]]$ArgList) {
    $ErrorActionPreference = 'Continue'
    & $script:XtpGit -C 'C:\xtp\src' @ArgList 2>&1 | ForEach-Object { "    $_" }
    if ($LASTEXITCODE -ne 0) { throw "git $($ArgList -join ' ') failed ($LASTEXITCODE)" }
}

# Puts C:\xtp\src on -Ref (default: the bundle's head, or origin/main).
function Set-XtpSource([string]$Bundle, [string]$Ref) {
    if ($Bundle) {
        # The bundle's first ref (HEAD, or the branch run-remote.ps1 -LocalRev named).
        $ErrorActionPreference = 'Continue'
        $heads = @(& $script:XtpGit -C 'C:\xtp\src' bundle list-heads $Bundle 2>$null)
        $ErrorActionPreference = 'Stop'
        if (-not $heads.Count) { throw "No ref in the bundle $Bundle." }
        $name = ("$($heads[0])" -split '\s+')[1]
        Invoke-XtpGit @('fetch', '--quiet', '--force', $Bundle, "+$($name):refs/xtp/local")
        if (-not $Ref) { $Ref = 'refs/xtp/local' }
    } else {
        Invoke-XtpGit @('fetch', '--quiet', '--prune', 'origin')
        if (-not $Ref) { $Ref = 'origin/main' }
    }
    Invoke-XtpGit @('checkout', '--quiet', '--force', '--detach', $Ref)
    Invoke-XtpGit @('clean', '-fdq', '-e', 'target/')
    Invoke-XtpGit @('log', '-1', '--format=commit %h %s (%cr)')
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
        [int]$Seconds = 0, $Environment = @{}, [string]$User = 'xtp', [switch]$Quiet)
    if (-not (Test-XtpSession $User)) {
        throw "$User is not logged on to the console (no desktop session): restart the VM, or run setup-vm.ps1."
    }
    Enter-XtpLock 'session'
    try {
        $id = (Get-Date -Format 'yyyyMMdd-HHmmss') + "-$PID"
        $done = Join-Path $script:XtpJobs "done-$id.json"
        $job = [ordered]@{ exe = $Exe; arguments = $Arguments; workDir = $WorkDir; log = $Log; seconds = $Seconds; env = $Environment; done = $done }
        $job | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $script:XtpJobs 'job.json') -Encoding ASCII
        Start-ScheduledTask -TaskName 'xtp-session-job'
        $deadline = if ($Seconds -gt 0) { (Get-Date).AddSeconds($Seconds + 180) } else { (Get-Date).AddHours(12) }
        $pos = 0L
        $t0 = Get-Date
        $last = Get-Date
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
            if ((Get-Date) -gt $deadline) { throw "The session job did not finish (no $done): see Task Scheduler, xtp-session-job." }
            if (((Get-Date) - $last).TotalSeconds -ge 60) { [Console]::Out.WriteLine(("    ... {0:N0} s" -f ((Get-Date) - $t0).TotalSeconds)); $last = Get-Date }
            Start-Sleep -Seconds 3
        }
        $result = Get-Content -LiteralPath $done -Raw | ConvertFrom-Json
        Remove-Item -LiteralPath $done -Force
        if ($result.error) { throw "The session job failed to start: $($result.error)" }
        $script:XtpLastJob = $result
        return [int]$result.exitCode
    } finally {
        Exit-XtpLock 'session'
    }
}
