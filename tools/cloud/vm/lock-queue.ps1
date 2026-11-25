# VM-side admission to the shared clone and desktop. Also run in memory by
# run-remote -Status so inspection works before the next deployment.
$script:XtpJobs = 'C:\xtp\jobs'

# Serialize short metadata operations across SSH sessions. Windows releases
# the mutex if its owner dies. Never hold it while sleeping or running a job.
function Invoke-XtpQueueTransaction([string]$Name, [scriptblock]$Action) {
    $mutex = [Threading.Mutex]::new($false, "Global\XtpRemoteQueue-$Name")
    try {
        try { $null = $mutex.WaitOne() } catch [Threading.AbandonedMutexException] {}
        try { & $Action } finally { $mutex.ReleaseMutex() }
    } finally { $mutex.Dispose() }
}

function Get-XtpQueueRecord([IO.FileInfo]$File) {
    try { $info = [IO.File]::ReadAllLines($File.FullName) }
    catch [IO.FileNotFoundException] { return }
    catch [IO.IOException] {
        # An older caller may still have the new lock open for writing.
        if (($_.Exception.GetBaseException().HResult -band 0xffff) -ne 32) { throw }
        $info = @()
    }
    # A writer killed during registration can leave a partial ticket. The PID
    # in its filename still identifies it. Preserve an unidentifiable old lock.
    [int]$ownerId = 0
    $since = $File.LastWriteTimeUtc
    if ($File.Extension -eq '.ticket') {
        $stamp, $ownerId = $File.BaseName.Split('-')
        $since = [datetime]::ParseExact($stamp, 'yyyyMMddHHmmssfffffff', [cultureinfo]::InvariantCulture,
            [Globalization.DateTimeStyles]'AssumeUniversal, AdjustToUniversal')
    } else {
        $null = [int]::TryParse($info[0], [ref]$ownerId)
    }
    $alive = $null
    if ($ownerId -gt 0) {
        try {
            $process = [Diagnostics.Process]::GetProcessById($ownerId)
            try {
                $start = $process.StartTime.ToUniversalTime()
                # Older locks have just PID, acquisition time and owner.
                $alive = if ($info[3]) { $start.Ticks -eq $info[3] } else { $start -le $File.LastWriteTimeUtc }
            } finally { $process.Dispose() }
        } catch [ArgumentException] { $alive = $false }
        catch [InvalidOperationException] { $alive = $false } # exited during lookup
        catch [ComponentModel.Win32Exception] {} # cannot inspect: keep it
    }
    [pscustomobject]@{ File = $File; Pid = $ownerId; Owner = $info[2]; Alive = $alive
        Since = $since; DesktopActive = $false }
}

function Test-XtpDesktopJobActive {
    $desktop = Get-ScheduledTask -TaskPath '\' | Where-Object TaskName -eq 'xtp-session-job'
    $desktop.State -in 'Running', 'Queued'
}

# Called under the resource mutex. Status observes orphan records, while
# admission reaps them. A desktop job can outlive its SSH controller.
function Get-XtpQueueState([string]$Name, [switch]$Clean) {
    $lock = Join-Path $script:XtpJobs "$Name.lock"
    $dir = Join-Path $script:XtpJobs "queue\$Name"
    $holder = if ([IO.File]::Exists($lock)) { Get-XtpQueueRecord ([IO.FileInfo]$lock) }
    if ($holder -and $false -eq $holder.Alive) {
        $holder.DesktopActive = Test-XtpDesktopJobActive
        if ($Clean -and -not $holder.DesktopActive) {
            # A pre-queue client can replace a stale lock during the scheduler
            # query. Recheck the file before reclaiming the observed owner.
            $current = Get-XtpQueueRecord ([IO.FileInfo]$lock)
            if ($current -and $false -eq $current.Alive -and
                $current.Pid -eq $holder.Pid -and $current.Since -eq $holder.Since) {
                [IO.File]::Delete($lock)
                $current = $null
            }
            $holder = $current
        }
    }
    $tickets = @(if ([IO.Directory]::Exists($dir)) {
        foreach ($file in (Get-ChildItem -LiteralPath $dir -File -Filter '*.ticket' | Sort-Object Name)) {
            $ticket = Get-XtpQueueRecord $file
            if ($Clean -and $false -eq $ticket.Alive) { [IO.File]::Delete($file.FullName) }
            elseif ($ticket) { $ticket }
        }
    })
    [pscustomobject]@{ Holder = $holder; Tickets = $tickets }
}

function Format-XtpQueueRecord($Record) {
    $age = [DateTime]::UtcNow - $Record.Since
    $state = if ($Record.DesktopActive) { ' (controller exited, desktop job active)' }
        elseif ($false -eq $Record.Alive) { ' (orphan)' } elseif ($null -eq $Record.Alive) { ' (unverified)' }
    "$($Record.Owner) (VM PID $($Record.Pid)), $([int]$age.TotalSeconds) s$state"
}

# The SSH controller spends most of a healthy build asleep. Include its live
# descendants and the separate scheduled desktop process tree in the CPU sum.
function Write-XtpLockWarning([string]$Name, $Holder) {
    if (-not $Holder -or ([DateTime]::UtcNow - $Holder.Since).TotalMinutes -lt 60) { return }
    try {
        $processes = @(Get-CimInstance Win32_Process -Property ProcessId,ParentProcessId,CommandLine,UserModeTime,KernelModeTime -ErrorAction Stop)
    } catch {
        [Console]::Out.WriteLine("WARNING: cannot inspect CPU use for the long-held $Name lock: $_")
        return
    }
    $roots = @($processes | Where-Object {
        ($_.ProcessId -eq $Holder.Pid -and $Holder.Alive) -or
        ($_.CommandLine -like '*-File C:\xtp\tools\vm\session-job.ps1*')
    })
    if (-not $roots) { return }
    $seen = [Collections.Generic.HashSet[int]]::new()
    $ticks = 0L
    while ($roots.Count) {
        $parents = foreach ($p in $roots) {
            if ($seen.Add($p.ProcessId)) {
                $ticks += [long]$p.UserModeTime + [long]$p.KernelModeTime
                $p.ProcessId
            }
        }
        $roots = @($processes | Where-Object { $_.ParentProcessId -in $parents -and -not $seen.Contains($_.ProcessId) })
    }
    $cpu = [TimeSpan]::FromTicks($ticks).TotalSeconds
    if ($cpu -lt ([DateTime]::UtcNow - $Holder.Since).TotalSeconds * 0.01) {
        [Console]::Out.WriteLine(("WARNING: possible stalled {0} lock: {1}; live processes used {2:N1} CPU seconds. Inspect the VM." -f $Name, (Format-XtpQueueRecord $Holder), $cpu))
    }
}

function Show-XtpLockStatus {
    foreach ($name in 'src', 'session') {
        $state = Invoke-XtpQueueTransaction $name { Get-XtpQueueState $name }
        if ($state.Holder) {
            "$name held by $(Format-XtpQueueRecord $state.Holder)"
            Write-XtpLockWarning $name $state.Holder
        } else { "$name free" }
        if (-not $state.Tickets.Count) { '  Queue empty' }
        $position = 0
        foreach ($ticket in $state.Tickets) {
            if ($false -eq $ticket.Alive) { "  Stale ticket: $(Format-XtpQueueRecord $ticket)"; continue }
            $position++
            "  #$position waiting: $(Format-XtpQueueRecord $ticket)"
        }
    }
}

function Enter-XtpLock([string]$Name, [switch]$NoWait) {
    $dir = Join-Path $script:XtpJobs "queue\$Name"
    $lock = Join-Path $script:XtpJobs "$Name.lock"
    $startTicks = (Get-Process -Id $PID).StartTime.ToUniversalTime().Ticks
    $ticket = $null
    try {
        $ticket = Invoke-XtpQueueTransaction $Name {
            $null = [IO.Directory]::CreateDirectory($dir)
            $last = Get-ChildItem -LiteralPath $dir -File -Filter '*.ticket' | Sort-Object Name | Select-Object -Last 1
            $now = [DateTime]::UtcNow
            $format = 'yyyyMMddHHmmssfffffff'
            if ($last) {
                $tail = [datetime]::ParseExact($last.BaseName.Split('-')[0], $format, [cultureinfo]::InvariantCulture)
                if ($now.Ticks -le $tail.Ticks) { $now = $tail.AddTicks(1) }
            }
            $path = Join-Path $dir ("{0}-$PID.ticket" -f $now.ToString($format, [cultureinfo]::InvariantCulture))
            [IO.File]::WriteAllLines($path, [string[]]@($PID, $now.ToString('o'), $script:XtpOwner, $startTicks))
            $path
        }
        $nextReport = [DateTime]::MinValue
        while ($true) {
            $waiting = Invoke-XtpQueueTransaction $Name {
                $state = Get-XtpQueueState $Name -Clean
                if (-not $state.Holder -and $state.Tickets[0].File.FullName -eq $ticket) {
                    # Publish a complete legacy-compatible lock atomically. Move
                    # refuses to overwrite even a lock taken by an older caller.
                    $now = [DateTime]::UtcNow
                    [IO.File]::WriteAllLines($ticket, [string[]]@($PID, $now.ToString('o'), $script:XtpOwner, $startTicks))
                    try { [IO.File]::Move($ticket, $lock); return }
                    catch [IO.IOException] {
                        if (($_.Exception.GetBaseException().HResult -band 0xffff) -notin 80, 183) { throw }
                    }
                }
                $state
            }
            if (-not $waiting) { return }
            $who = if ($waiting.Holder) { "held by $(Format-XtpQueueRecord $waiting.Holder)" }
                else { "first in queue: $(Format-XtpQueueRecord $waiting.Tickets[0])" }
            if ($NoWait -or [DateTime]::UtcNow -ge $nextReport) {
                Write-XtpLockWarning $Name $waiting.Holder
                if ($NoWait) { throw "The VM is busy: $Name, $who." }
                [Console]::Out.WriteLine("waiting for $Name, $who")
                $nextReport = [DateTime]::UtcNow.AddMinutes(1)
            }
            Start-Sleep -Seconds 10
        }
    } finally {
        if ($ticket) { [IO.File]::Delete($ticket) }
    }
}

function Exit-XtpLock([string]$Name) {
    Invoke-XtpQueueTransaction $Name {
        $lock = Join-Path $script:XtpJobs "$Name.lock"
        if ([IO.File]::Exists($lock)) {
            $record = Get-XtpQueueRecord ([IO.FileInfo]$lock)
            if ($record.Pid -eq $PID -and $record.Alive) {
                if ($script:XtpSessionOutstanding -and (Test-XtpDesktopJobActive)) { return }
                [IO.File]::Delete($lock)
            }
        }
    }
}
