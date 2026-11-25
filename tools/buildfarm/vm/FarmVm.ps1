# FarmVm.ps1: helpers of the scripts that run on a build farm VM.
# Windows PowerShell 5.1. ASCII only.

Set-StrictMode -Off
$ErrorActionPreference = 'Stop'

$script:Root = 'C:\farm'

function Get-FarmRoot { $script:Root }

function Write-FarmErr([string] $Message) {
    [Console]::Error.WriteLine("farm: $Message")
}

# Quotes one argument the way CommandLineToArgvW and the C runtime read it.
function ConvertTo-FarmArg([string] $Arg) {
    if ($Arg.Length -gt 0 -and $Arg -notmatch '[\s"]') { return $Arg }
    $sb = New-Object System.Text.StringBuilder
    [void] $sb.Append('"')
    $bs = 0
    foreach ($ch in $Arg.ToCharArray()) {
        if ($ch -eq [char] '\') { $bs++; continue }
        if ($ch -eq [char] '"') {
            [void] $sb.Append('\' * (2 * $bs + 1)).Append('"')
            $bs = 0
            continue
        }
        if ($bs) { [void] $sb.Append('\' * $bs); $bs = 0 }
        [void] $sb.Append($ch)
    }
    [void] $sb.Append('\' * (2 * $bs)).Append('"')
    $sb.ToString()
}

function New-FarmStartInfo([string] $FilePath, [string[]] $ArgumentList, [hashtable] $Environment, [string] $WorkingDirectory) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $FilePath
    $psi.Arguments = (@($ArgumentList | ForEach-Object { ConvertTo-FarmArg $_ }) -join ' ')
    $psi.UseShellExecute = $false
    if ($WorkingDirectory) { $psi.WorkingDirectory = $WorkingDirectory }
    if ($Environment) {
        foreach ($k in $Environment.Keys) {
            if ($null -eq $Environment[$k]) { $psi.EnvironmentVariables.Remove($k) } else { $psi.EnvironmentVariables[$k] = [string] $Environment[$k] }
        }
    }
    $psi
}

# Runs a program and returns its exit code and output (stdin closed).
function Invoke-FarmCapture([string] $FilePath, [string[]] $ArgumentList, [hashtable] $Environment = @{}, [string] $WorkingDirectory = '') {
    $psi = New-FarmStartInfo $FilePath $ArgumentList $Environment $WorkingDirectory
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = New-Object Text.UTF8Encoding $false
    $psi.StandardErrorEncoding = New-Object Text.UTF8Encoding $false
    $p = [System.Diagnostics.Process]::Start($psi)
    try {
        $p.StandardInput.Close()
        $o = $p.StandardOutput.ReadToEndAsync()
        $e = $p.StandardError.ReadToEndAsync()
        $p.WaitForExit()
        [pscustomobject]@{ ExitCode = $p.ExitCode; Out = $o.Result; Err = $e.Result }
    } finally {
        $p.Dispose()
    }
}

function Stop-FarmTree([int] $ProcessId) {
    & taskkill.exe /PID $ProcessId /T /F 2>&1 | Out-Null
}

# This process's ssh session: the shell that started it and the sshd process
# of the connection. When one of them is gone the client has left.
function Get-FarmSessionPids {
    $pids = @()
    $cur = $PID
    for ($i = 0; $i -lt 5; $i++) {
        $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$cur" -ErrorAction SilentlyContinue
        if (-not $proc) { break }
        $parent = Get-CimInstance Win32_Process -Filter "ProcessId=$($proc.ParentProcessId)" -ErrorAction SilentlyContinue
        if (-not $parent) { break }
        $name = "$($parent.Name)".ToLowerInvariant()
        if (@('services.exe', 'wininit.exe', 'svchost.exe', 'explorer.exe') -contains $name) { break }
        if ($name -like 'sshd*') {
            $gp = Get-CimInstance Win32_Process -Filter "ProcessId=$($parent.ParentProcessId)" -ErrorAction SilentlyContinue
            if ($gp -and "$($gp.Name)".ToLowerInvariant() -eq 'services.exe') { break }
            $pids += [int] $parent.ProcessId
            break
        }
        $pids += [int] $parent.ProcessId
        $cur = $parent.ProcessId
    }
    $pids
}

# Runs a program with this process's stdout and stderr (output streams to
# the client), stops its whole tree at the job's deadline or when the ssh
# session goes away. 124: over time; 125: client gone.
function Invoke-FarmWatched {
    param([string] $FilePath, [string[]] $ArgumentList, [hashtable] $Environment, [string] $WorkingDirectory,
        [datetime] $Deadline = (Get-Date).AddHours(2), [int[]] $Watch = @())
    if ((Get-Date) -ge $Deadline) {
        Write-FarmErr 'the job is out of time: this command did not start'
        return 124
    }
    $psi = New-FarmStartInfo $FilePath $ArgumentList $Environment $WorkingDirectory
    $psi.RedirectStandardInput = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    try {
        $p.StandardInput.Close()
        $tick = 0
        while (-not $p.WaitForExit(2000)) {
            if ((Get-Date) -gt $Deadline) {
                Write-FarmErr ('the job ran over its time (until {0:HH:mm:ss} UTC): cargo stopped' -f $Deadline.ToUniversalTime())
                Stop-FarmTree $p.Id
                $p.WaitForExit()
                return 124
            }
            $tick++
            if ($tick % 3 -eq 0) {
                foreach ($w in $Watch) {
                    if (-not (Get-Process -Id $w -ErrorAction SilentlyContinue)) {
                        Stop-FarmTree $p.Id
                        $p.WaitForExit()
                        return 125
                    }
                }
            }
        }
        $p.WaitForExit()
        $p.ExitCode
    } finally {
        $p.Dispose()
    }
}

# The environment of a build: the toolchain under C:\farm, the variables the
# client forwards; never the client's target directory or job count, and
# never incremental compilation, whatever the client says: farm builds gain
# little from it, and its caches filled the disks (Clear-FarmSlotCaches).
function Get-FarmBuildEnv($Forward) {
    $fe = Get-Content -Raw (Join-Path $script:Root 'farm-env.json') | ConvertFrom-Json
    $h = @{}
    $h['CARGO_HOME'] = $fe.CARGO_HOME
    $h['RUSTUP_HOME'] = $fe.RUSTUP_HOME
    $h['PATH'] = ((@($fe.PathPrepend) -join ';') + ';' + $env:PATH)
    $h['TMP'] = Join-Path $script:Root 'tmp'
    $h['TEMP'] = Join-Path $script:Root 'tmp'
    $h['RUSTUP_AUTO_INSTALL'] = '1'
    $h['CARGO_TARGET_DIR'] = $null
    $h['CARGO_BUILD_TARGET_DIR'] = $null
    $h['CARGO_BUILD_JOBS'] = $null
    # No GPU here: GPU tests run on WARP, as on the PC (unless the client
    # forwards another XTP_TEST_ADAPTER).
    $h['XTP_TEST_ADAPTER'] = 'warp'
    if ($Forward) {
        foreach ($p in $Forward.PSObject.Properties) { $h[$p.Name] = [string] $p.Value }
    }
    $h['CARGO_INCREMENTAL'] = '0'
    $h
}

# Takes a slot's lock (C:\farm\locks\<slot>.lock, which farm-run.ps1 holds
# for the whole job) when no job holds it: the open handle, to dispose of to
# let it go, or $null while a job holds it.
function Open-FarmSlotLock([string] $Root, [string] $Slot) {
    try {
        [IO.File]::Open((Join-Path $Root "locks\$Slot.lock"), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    } catch {
        $null
    }
}

# When a slot last had a job (C:\farm\state\<slot>.used, written by
# farm-run.ps1 under the slot's lock at the start and the end of each job).
function Set-FarmSlotUsed([string] $Root, [string] $Slot) {
    try { [IO.File]::WriteAllText((Join-Path $Root "state\$Slot.used"), (Get-Date).ToUniversalTime().ToString('o')) } catch { }
}

# Disk room, before a job's build (farm-run.ps1, step 4b). Returns what it
# removed. Builds run without incremental compilation (Get-FarmBuildEnv), so
# the slots' incremental folders are dead weight: they go in the job's own
# slot ($Current, whose lock the caller holds) and in every slot no job
# holds, its lock taken meanwhile so that no job starts there. A slot a job
# holds is never touched: judging a slot idle by its incremental folder's
# date (that date does not change while files deep inside it do) deleted
# caches under running builds, which then failed (2026-11-25). Under
# $MinFreeGb free after that, the whole target folders of free slots that
# had no job for $UnusedMinutes go too, least recently used first; never
# this job's slot, nor a slot with no use recorded.
function Clear-FarmSlotCaches {
    param([Parameter(Mandatory)] [string] $Root, [Parameter(Mandatory)] [string] $Current, [double] $MinFreeGb = 20, [int] $UnusedMinutes = 30)
    $removed = @()
    $slots = @(Get-ChildItem -LiteralPath (Join-Path $Root 'w') -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^s\d{1,3}$' })
    foreach ($s in $slots) {
        $lock = $null
        if ($s.Name -ine $Current) {
            $lock = Open-FarmSlotLock $Root $s.Name
            if (-not $lock) { continue }
        }
        try {
            foreach ($prof in @(Get-ChildItem -LiteralPath (Join-Path $s.FullName 'target') -Directory -ErrorAction SilentlyContinue)) {
                $inc = Join-Path $prof.FullName 'incremental'
                if (Test-Path -LiteralPath $inc) {
                    Remove-Item -LiteralPath $inc -Recurse -Force -ErrorAction SilentlyContinue
                    $removed += $inc
                }
            }
        } finally {
            if ($lock) { $lock.Dispose() }
        }
    }
    $drive = $Root.Substring(0, 1)
    $freeGb = { [double] (Get-PSDrive -Name $drive).Free / 1GB }
    if ((& $freeGb) -ge $MinFreeGb) { return $removed }
    $before = (Get-Date).AddMinutes(-$UnusedMinutes)
    $old = foreach ($s in $slots) {
        $stamp = Join-Path $Root "state\$($s.Name).used"
        if ($s.Name -ieq $Current -or -not (Test-Path -LiteralPath $stamp) -or -not (Test-Path -LiteralPath (Join-Path $s.FullName 'target'))) { continue }
        $used = (Get-Item -LiteralPath $stamp).LastWriteTime
        if ($used -lt $before) { [pscustomobject]@{ slot = $s; stamp = $stamp; used = $used } }
    }
    foreach ($o in @($old | Sort-Object used)) {
        if ((& $freeGb) -ge $MinFreeGb) { break }
        $lock = Open-FarmSlotLock $Root $o.slot.Name
        if (-not $lock) { continue }
        try {
            # The stamp changes only under the lock: still unused.
            if ((Get-Item -LiteralPath $o.stamp).LastWriteTime -lt $before) {
                $target = Join-Path $o.slot.FullName 'target'
                Remove-Item -LiteralPath $target -Recurse -Force -ErrorAction SilentlyContinue
                $removed += $target
            }
        } finally {
            $lock.Dispose()
        }
    }
    $removed
}

function Get-FarmGit {
    $fe = Get-Content -Raw (Join-Path $script:Root 'farm-env.json') | ConvertFrom-Json
    $fe.Git
}

function Get-FarmCargo {
    $fe = Get-Content -Raw (Join-Path $script:Root 'farm-env.json') | ConvertFrom-Json
    Join-Path $fe.CARGO_HOME 'bin\cargo.exe'
}

# Is this VM going away? A Spot reclaim (GCE's metadata says so as soon as
# it starts) or a shutdown in progress (a stop). Windows kills the build's
# processes first, so a failed step may only mean that.
function Test-FarmGoingAway {
    try {
        $p = Invoke-RestMethod -UseBasicParsing -TimeoutSec 2 -Headers @{ 'Metadata-Flavor' = 'Google' } `
            -Uri 'http://metadata.google.internal/computeMetadata/v1/instance/preempted'
        if ("$p".Trim() -eq 'TRUE') { return $true }
    } catch { }
    try {
        if (-not ('FarmNative' -as [type])) {
            Add-Type -Name FarmNative -Namespace '' -MemberDefinition '[DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);'
        }
        # SM_SHUTTINGDOWN
        if ([FarmNative]::GetSystemMetrics(0x2000) -ne 0) { return $true }
    } catch { }
    $false
}

# The elements of a JSON array. Windows PowerShell's ConvertFrom-Json writes
# an array as one object, so `@($json | ConvertFrom-Json)` is a one-element
# array holding the array: callers use `@(ConvertFrom-FarmJsonArray $json)`.
function ConvertFrom-FarmJsonArray([string] $Json) {
    if (-not $Json -or -not $Json.Trim()) { return }
    $value = ConvertFrom-Json -InputObject $Json
    foreach ($e in @($value)) {
        if ($null -ne $e) { $e }
    }
}

# Reads up to Count bytes from a stream into Buffer, waiting at most
# TimeoutSeconds: the number read, 0 at the end of the stream, -1 when
# nothing came in time (the client's connection is stuck).
function Read-FarmStream($Stream, [byte[]] $Buffer, [int] $Count, [int] $TimeoutSeconds = 60) {
    $t = $Stream.ReadAsync($Buffer, 0, $Count)
    if (-not $t.Wait($TimeoutSeconds * 1000)) { return -1 }
    $t.Result
}

function ConvertFrom-FarmBase64([string] $Text) {
    [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Text)) | ConvertFrom-Json
}

function Test-FarmSlotName([string] $Slot) {
    if ($Slot -notmatch '^s\d{1,3}$') { throw "bad slot name '$Slot'" }
}

