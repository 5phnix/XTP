# Runs one job of the build farm on this VM (called over SSH by the PC's
# Invoke-FarmJob): reads the git bundle the client sends on stdin, imports it
# into C:\farm\repo.git, checks the job's slot (C:\farm\w\<slot>, a worktree
# with its own persistent target directory) out at the requested commit and
# runs the cargo commands there, their output going straight to the client.
#
# The request is base64 JSON: v (agent version), slot, target (commit), ref
# and bundle (bundle ref and size, 0 for none), cwd, steps [{args, label}],
# env, collect, maxMinutes, waitMinutes, worktree.
#
# Exit code: the last cargo command's, or 95 this VM is going away (Spot
# reclaim or stop: run the job elsewhere), 96 agent outdated, 97 commits
# missing (send a full bundle), 98 farm error, 99 slot busy too long,
# 124 run over time, 125 client gone.
#
# Windows PowerShell 5.1. ASCII only.

param([Parameter(Mandatory = $true)] [string] $Request)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
. (Join-Path $PSScriptRoot 'FarmVm.ps1')
$Root = Get-FarmRoot
$req = ConvertFrom-FarmBase64 $Request
$t0 = Get-Date
# The job's deadline, all its commands included (the client gives up a
# little later).
$deadline = $t0.AddMinutes([Math]::Max(1, [int] $req.maxMinutes))

$version = (Get-Content -Raw (Join-Path $PSScriptRoot 'version.txt')).Trim()
if ($req.v -and $req.v -ne $version) {
    Write-FarmErr "agent $version here, the client has $($req.v)"
    exit 96
}
Test-FarmSlotName $req.slot

# 1. The bundle, read first: the client is sending it now.
$bundle = ''
if ([int64] $req.bundle -gt 0) {
    $bundle = Join-Path $Root "tmp\$PID.bundle"
    $in = [Console]::OpenStandardInput()
    $buf = New-Object byte[] 1048576
    # The client's .NET may put its console encoding's preamble (a UTF-8 BOM)
    # before the bundle: it says how many bytes that is.
    # Nothing for 60 s: the connection is stuck. Give up (98, the client
    # retries elsewhere) rather than wait for ever, which would also keep
    # this VM from stopping when idle.
    $skip = [int] $req.skip
    $stalled = $false
    while ($skip -gt 0) {
        $n = Read-FarmStream $in $buf $skip 60
        if ($n -lt 0) { $stalled = $true; break }
        if ($n -eq 0) { break }
        $skip -= $n
    }
    $fs = [IO.File]::Create($bundle)
    $left = [int64] $req.bundle
    try {
        while (-not $stalled -and $left -gt 0) {
            $n = Read-FarmStream $in $buf ([int] [Math]::Min([int64] $buf.Length, $left)) 60
            if ($n -lt 0) { $stalled = $true; break }
            if ($n -eq 0) { break }
            $fs.Write($buf, 0, $n)
            $left -= $n
        }
    } finally {
        $fs.Dispose()
    }
    if ($left -gt 0) {
        $why = $(if ($stalled) { 'nothing came for 60 s' } else { 'the stream ended' })
        Write-FarmErr "the bundle arrived incomplete ($left of $($req.bundle) bytes missing: $why)"
        Remove-Item -Force $bundle -ErrorAction SilentlyContinue
        # A read may still be pending on stdin: leave without waiting for it.
        [Environment]::Exit(98)
    }
}

# 2. This run counts as activity for the idle watcher while it lasts.
$runFile = Join-Path $Root "run\$PID.json"
@{ pid = $PID; slot = $req.slot; worktree = $req.worktree; started = (Get-Date).ToUniversalTime().ToString('o') } |
    ConvertTo-Json | Set-Content -Encoding ascii -Path $runFile
$lock = $null
$code = 98
try {
    # 3. The slot, one job at a time.
    $lockPath = Join-Path $Root "locks\$($req.slot).lock"
    # Its own limit: $deadline stays the whole job's.
    $lockDeadline = (Get-Date).AddMinutes([int] $req.waitMinutes)
    $said = $false
    while (-not $lock) {
        try {
            $lock = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        } catch {
            if ((Get-Date) -gt $lockDeadline) { Write-FarmErr "slot $($req.slot) stayed busy for $($req.waitMinutes) minutes"; exit 99 }
            if (-not $said) { Write-FarmErr "slot $($req.slot) is busy: waiting"; $said = $true }
            Start-Sleep -Seconds 2
        }
    }
    Set-FarmSlotUsed $Root $req.slot

    # 4. Commits into the mirror, the slot at the target commit.
    $git = Get-FarmGit
    $repo = Join-Path $Root 'repo.git'
    $m = New-Object System.Threading.Mutex($false, 'Global\xtp-farm-repo')
    try {
        try { [void] $m.WaitOne() } catch [System.Threading.AbandonedMutexException] { }
        if (-not (Test-Path (Join-Path $repo 'HEAD'))) {
            $r = Invoke-FarmCapture $git @('init', '--bare', '--quiet', $repo)
            if ($r.ExitCode -ne 0) { throw "git init: $($r.Err)" }
        }
    } finally {
        $m.ReleaseMutex()
        $m.Dispose()
    }
    $slotRef = "refs/farm/$($req.slot)"
    if ($bundle) {
        $r = Invoke-FarmCapture $git @('-C', $repo, 'fetch', '--quiet', '--no-tags', '--no-write-fetch-head', $bundle, "+$($req.ref):$slotRef")
        if ($r.ExitCode -ne 0) {
            if ($r.Err -match 'prerequisite') { Write-FarmErr 'commits missing here: the client resends them all'; exit 97 }
            throw "importing the bundle failed: $($r.Err.Trim())"
        }
    } else {
        $r = Invoke-FarmCapture $git @('-C', $repo, 'cat-file', '-e', "$($req.target)^{commit}")
        if ($r.ExitCode -ne 0) { Write-FarmErr 'commit missing here: the client resends it'; exit 97 }
        $r = Invoke-FarmCapture $git @('-C', $repo, 'update-ref', $slotRef, $req.target)
        if ($r.ExitCode -ne 0) { throw "update-ref failed: $($r.Err.Trim())" }
    }
    $slotDir = Join-Path $Root "w\$($req.slot)"
    if (-not (Test-Path (Join-Path $slotDir '.git'))) {
        if (Test-Path $slotDir) { Remove-Item -Recurse -Force $slotDir }
        [void] (Invoke-FarmCapture $git @('-C', $repo, 'worktree', 'prune'))
        $r = Invoke-FarmCapture $git @('-C', $repo, 'worktree', 'add', '--detach', '--force', $slotDir, $req.target)
        if ($r.ExitCode -ne 0) { throw "worktree add failed: $($r.Err.Trim())" }
    } else {
        # Only the files that differ are rewritten: cargo rebuilds what changed.
        $r = Invoke-FarmCapture $git @('-C', $slotDir, 'checkout', '--detach', '--force', '--quiet', $req.target)
        if ($r.ExitCode -ne 0) { throw "checkout failed: $($r.Err.Trim())" }
        $r = Invoke-FarmCapture $git @('-C', $slotDir, 'clean', '-fdq')
        if ($r.ExitCode -ne 0) { throw "clean failed: $($r.Err.Trim())" }
    }
    $sync = ((Get-Date) - $t0).TotalSeconds

    # 4b. Disk room. Six slots' incremental caches filled the 110 GB disk in
    # a few hours of busy deliveries (2026-11-24: "os error 112" on every
    # build). Builds now run without incremental compilation: the caches
    # left go, in this slot and in the slots no job holds (their lock taken
    # meanwhile), never under a running build. Under 20 GB free after that,
    # the target folders of free slots without a job for 30 minutes go too.
    try {
        foreach ($gone in @(Clear-FarmSlotCaches -Root $Root -Current $req.slot | Where-Object { $_ -notmatch '\\incremental$' })) {
            Write-FarmErr "under 20 GB free: removed $gone (no job there for 30 minutes)"
        }
    } catch { Write-FarmErr "disk room: $($_.Exception.Message)" }

    # 5. The cargo commands.
    $envs = Get-FarmBuildEnv $req.env
    $cwd = $slotDir
    if ($req.cwd) { $cwd = Join-Path $slotDir ($req.cwd -replace '/', '\') }
    if (-not (Test-Path $cwd)) { $cwd = $slotDir }
    $cargo = Get-FarmCargo
    $watch = @(Get-FarmSessionPids)
    $token = '@@FARM_SLOT@@'
    $vmName = $env:COMPUTERNAME
    $nameFile = Join-Path $Root 'state\instance-name'
    if (Test-Path $nameFile) { $vmName = (Get-Content -Raw $nameFile).Trim() }
    Write-FarmErr ('{0} slot {1} at {2} (ready in {3:N1} s)' -f $vmName, $req.slot, $req.target.Substring(0, 8), $sync)
    $last = $null
    $code = 0
    foreach ($step in @($req.steps)) {
        $a = @(@($step.args) | ForEach-Object { "$_".Replace($token, $slotDir) })
        $last = $a
        if ($step.label) { [Console]::Out.WriteLine($step.label); [Console]::Out.Flush() }
        $code = Invoke-FarmWatched -FilePath $cargo -ArgumentList $a -Environment $envs -WorkingDirectory $cwd -Deadline $deadline -Watch $watch
        if ($code -ne 0 -and (Test-FarmGoingAway)) {
            Write-FarmErr "$vmName is being stopped (Spot reclaim or stop): the client runs the job elsewhere"
            $code = 95
        }
        if ($code -ne 0) { break }
    }

    # 6. The executables a build made, for the client to fetch.
    $state = Join-Path $Root "state\$($req.slot).artifacts.json"
    if ($code -eq 0 -and $req.collect -and $last -and (($last -join ' ') -notmatch '--message-format')) {
        $r = Invoke-FarmCapture $cargo (@($last) + '--message-format=json') $envs $cwd
        $exes = @()
        foreach ($line in ($r.Out -split "`r?`n")) {
            if (-not $line.StartsWith('{')) { continue }
            $msg = $line | ConvertFrom-Json
            if ($msg.reason -eq 'compiler-artifact' -and $msg.executable -and -not $msg.profile.test) {
                $full = [IO.Path]::GetFullPath($msg.executable)
                if ($full.StartsWith($slotDir + '\', [StringComparison]::OrdinalIgnoreCase)) {
                    $exes += $full.Substring($slotDir.Length + 1).Replace('\', '/')
                }
            }
        }
        ConvertTo-Json -InputObject @($exes | Sort-Object -Unique) | Set-Content -Encoding ascii -Path $state
    }
} catch {
    Write-FarmErr "error: $($_.Exception.Message)"
    $code = 98
} finally {
    if ($lock) {
        Set-FarmSlotUsed $Root $req.slot
        $lock.Dispose()
    }
    Remove-Item -Force $runFile -ErrorAction SilentlyContinue
    if ($bundle) { Remove-Item -Force $bundle -ErrorAction SilentlyContinue }
}
exit $code
