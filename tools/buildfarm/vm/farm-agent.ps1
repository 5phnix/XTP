# The build farm agent: a scheduled task (SYSTEM, at startup) on every farm VM.
#
# At boot: on a VM made from the image (a new instance id), new SSH host
# keys; then the ed25519 host key goes to the guest attributes
# (xtp-farm/hostkey), where the client pins it.
#
# Then the idle watcher: the VM shuts itself down (GCE stops it, billing
# stops) after `xtp-farm-idle-minutes` (metadata, default 15) minutes without
# a farm run, cargo, rustc, a linker or a test binary. Build processes left
# without a run (a client gone) are stopped after twice that, 30 minutes at
# least, so nothing keeps a VM up for ever.
#
# Windows PowerShell 5.1. ASCII only.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Root = 'C:\farm'
$Log = Join-Path $Root 'log\agent.log'
$Meta = 'http://metadata.google.internal/computeMetadata/v1/instance'
$Hdr = @{ 'Metadata-Flavor' = 'Google' }
foreach ($d in 'log', 'run', 'state', 'tmp', 'locks', 'w') { New-Item -ItemType Directory -Force -Path (Join-Path $Root $d) | Out-Null }

function Write-Agent([string] $Message) {
    try {
        if ((Test-Path $Log) -and (Get-Item $Log).Length -gt 5MB) { Move-Item -Force $Log "$Log.old" }
        Add-Content -Path $Log -Value ('{0:u} {1}' -f (Get-Date).ToUniversalTime(), $Message)
    } catch { }
}

function Get-Meta([string] $Path, [string] $Default = '') {
    try {
        $v = Invoke-RestMethod -UseBasicParsing -TimeoutSec 5 -Headers $Hdr -Uri "$Meta/$Path"
        if ($null -eq $v -or "$v" -eq '') { return $Default }
        return "$v".Trim()
    } catch {
        return $Default
    }
}

function Get-SshDir {
    $svc = Get-CimInstance Win32_Service -Filter "Name='sshd'" -ErrorAction SilentlyContinue
    if ($svc -and $svc.PathName) {
        $pn = $svc.PathName.Trim()
        if ($pn.StartsWith('"')) { $exe = $pn.Substring(1, $pn.IndexOf('"', 1) - 1) } else { $exe = ($pn -split ' ')[0] }
        return (Split-Path -Parent $exe)
    }
    'C:\Windows\System32\OpenSSH'
}

# --- boot -------------------------------------------------------------------

try {
    Write-Agent "agent start, version $((Get-Content -Raw (Join-Path $Root 'bin\version.txt')).Trim())"
    # A VM made from the image keeps the builder's computer name: messages
    # use the instance's name instead.
    $vmName = Get-Meta 'name'
    if ($vmName) { Set-Content -Encoding ascii -Path (Join-Path $Root 'state\instance-name') -Value $vmName }
    $id = Get-Meta 'id'
    $idFile = Join-Path $Root 'state\instance-id'
    $old = ''
    if (Test-Path $idFile) { $old = (Get-Content -Raw $idFile).Trim() }
    $pub = 'C:\ProgramData\ssh\ssh_host_ed25519_key.pub'
    if ($id -and $id -ne $old) {
        Write-Agent "new instance ($old -> $id): new SSH host keys, clean state"
        Stop-Service sshd -Force -ErrorAction SilentlyContinue
        Remove-Item -Force 'C:\ProgramData\ssh\ssh_host_*' -ErrorAction SilentlyContinue
        & (Join-Path (Get-SshDir) 'ssh-keygen.exe') -A 2>&1 | Out-Null
        Start-Service sshd
        Remove-Item -Force (Join-Path $Root 'run\*') -ErrorAction SilentlyContinue
        Remove-Item -Force (Join-Path $Root 'locks\*') -ErrorAction SilentlyContinue
        Set-Content -Encoding ascii -Path $idFile -Value $id
    }
    for ($i = 0; $i -lt 60 -and -not (Test-Path $pub); $i++) { Start-Sleep -Seconds 5 }
    if (Test-Path $pub) {
        Invoke-RestMethod -UseBasicParsing -Method Put -Headers $Hdr -Uri "$Meta/guest-attributes/xtp-farm/hostkey" `
            -Body (Get-Content -Raw $pub).Trim() | Out-Null
        Write-Agent 'host key published'
    } else {
        Write-Agent "no host key at $pub"
    }
} catch {
    Write-Agent "boot step failed: $($_.Exception.Message)"
}

# --- idle watcher -------------------------------------------------------------

$names = @('cargo', 'rustc', 'clippy-driver', 'rustdoc', 'rustfmt', 'cargo-clippy', 'cargo-fmt', 'link', 'lld-link', 'cl', 'lib', 'rustup')
$work = (Join-Path $Root 'w') + '\'
$last = Get-Date
$orphanSince = $null
$idle = 15
$checked = [datetime]::MinValue
while ($true) {
    try {
        if (((Get-Date) - $checked).TotalMinutes -ge 5) {
            $v = Get-Meta 'attributes/xtp-farm-idle-minutes' '15'
            if ($v -match '^\d+$' -and [int] $v -ge 5) { $idle = [int] $v }
            $checked = Get-Date
        }
        $runs = @()
        foreach ($f in @(Get-ChildItem (Join-Path $Root 'run') -Filter '*.json' -ErrorAction SilentlyContinue)) {
            $runPid = 0
            if ([int]::TryParse($f.BaseName, [ref] $runPid) -and (Get-Process -Id $runPid -ErrorAction SilentlyContinue)) {
                # A run's own deadline ends it long before this: a run that old
                # is stuck, and would keep the VM up for ever.
                if (((Get-Date) - $f.CreationTime).TotalHours -ge 6) {
                    Write-Agent "stopping run $runPid, started $($f.CreationTime.ToUniversalTime().ToString('u')): stuck"
                    & taskkill.exe /PID $runPid /T /F 2>&1 | Out-Null
                    Remove-Item -Force $f.FullName -ErrorAction SilentlyContinue
                    continue
                }
                $runs += $runPid
            } else {
                Remove-Item -Force $f.FullName -ErrorAction SilentlyContinue
            }
        }
        $procs = @(Get-Process -Name $names -ErrorAction SilentlyContinue)
        $procs += @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
                try { $_.Path -and $_.Path.StartsWith($work, [StringComparison]::OrdinalIgnoreCase) } catch { $false }
            })
        if ($runs) {
            $last = Get-Date
            $orphanSince = $null
        } elseif ($procs) {
            $last = Get-Date
            if (-not $orphanSince) {
                $orphanSince = Get-Date
            } elseif (((Get-Date) - $orphanSince).TotalMinutes -ge [Math]::Max(30, 2 * $idle)) {
                Write-Agent ('stopping build processes left without a run: ' + (($procs | ForEach-Object { "$($_.ProcessName)($($_.Id))" }) -join ' '))
                $procs | Stop-Process -Force -ErrorAction SilentlyContinue
                $orphanSince = $null
            }
        } else {
            $orphanSince = $null
        }
        if (((Get-Date) - $last).TotalMinutes -ge $idle) {
            Write-Agent "idle for $idle minutes: shutting down"
            & shutdown.exe /s /t 0 /f /d p:0:0 /c 'xtp-farm: idle' 2>&1 | Out-Null
            Start-Sleep -Seconds 600
        }
    } catch {
        Write-Agent "watcher error: $($_.Exception.Message)"
    }
    Start-Sleep -Seconds 30
}
