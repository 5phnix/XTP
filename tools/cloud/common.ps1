# Shared helpers for the tools/cloud scripts. Dot-source it:  . "$PSScriptRoot\common.ps1"
#
# Works in Windows PowerShell 5.1 and PowerShell 7. Every script reads the
# same configuration file (see cloud.example.json and docs/dev/cloud-setup.md):
#   -Config <path>, else $env:XTP_CLOUD_CONFIG, else %LOCALAPPDATA%\xtp\cloud.json
# The file names your project, bucket and paths: it must stay outside the
# repository, and the scripts refuse one inside it.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$script:XtpCloudRoot = $PSScriptRoot
$script:XtpRepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

# Defaults for every key the scripts read; the file overrides them.
$script:XtpCloudDefaults = [ordered]@{
    project           = $null          # required
    region            = $null          # required: the bucket's region, and the VM's
    zones             = @()            # zones tried in order when the VM is created
    bucket            = $null          # required: globally unique name, no gs://
    vm                = 'xtp-l4'
    machineType       = 'g2-standard-16'
    accelerator       = 'nvidia-l4-vws' # '' for a machine without a GPU
    acceleratorCount  = 1
    imageProject      = 'windows-cloud'
    imageFamily       = 'windows-2025'
    bootDiskGb        = 100
    dataDiskGb        = 300
    diskType          = 'pd-balanced'   # the boot disk's type
    dataDiskType      = $null          # default: diskType; hyperdisk-ml where SSD_TOTAL_GB is short
    dataDiskThroughput = 400           # MiB/s, for a hyperdisk-ml data disk (billed per MiB/s)
    serviceAccount    = 'xtp-vm'       # account id; created in the project if missing
    gameDir           = $null          # your dump of the title, on your PC (read only)
    goldenDir         = $null          # optional: an XTP data folder that runs start from
    gamePrefix        = 'game'         # bucket folder for the title
    goldenPrefix      = 'golden'       # bucket folder for the golden data folder
    titleExe          = $null          # the title's executable, relative to the game folder
    repoUrl           = 'https://github.com/MiamiGame/XTP.git'
    idleMinutes       = 30             # the VM shuts itself down after this long idle
    startRetryMinutes = 20             # how long a start retries while the zone has no GPU free
    stopSchedule      = '0 3 * * *'    # backstop: an instance schedule stops the VM daily
    timeZone          = 'Europe/Paris'
    sessionUser       = 'xtp'          # local account logged on to the desktop for game runs
    sshUser           = $null          # your account on the VM (default: your Windows user name, lowercase)
    localLogDir       = $null          # where fetched logs go (default %LOCALAPPDATA%\xtp\cloud-runs)
}

function Get-XtpCloudConfig {
    param([string]$Path)
    if (-not $Path) { $Path = $env:XTP_CLOUD_CONFIG }
    if (-not $Path) { $Path = Join-Path $env:LOCALAPPDATA 'xtp\cloud.json' }
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "No configuration at $Path. Copy tools\cloud\cloud.example.json there (outside the repository) and fill it in, or pass -Config."
    }
    $full = (Resolve-Path -LiteralPath $Path).Path
    if ($full.StartsWith($script:XtpRepoRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw "The configuration $full is inside the repository; keep it outside (it names your project and paths)."
    }
    $file = Get-Content -LiteralPath $full -Raw | ConvertFrom-Json
    $cfg = [ordered]@{}
    foreach ($k in $script:XtpCloudDefaults.Keys) { $cfg[$k] = $script:XtpCloudDefaults[$k] }
    foreach ($p in $file.PSObject.Properties) {
        if ($p.Name -like '_*') { continue }   # comments
        if (-not $cfg.Contains($p.Name)) { throw "Unknown key '$($p.Name)' in $full" }
        $cfg[$p.Name] = $p.Value
    }
    foreach ($k in 'project', 'region', 'bucket') {
        if (-not $cfg[$k] -or "$($cfg[$k])" -match '^<') { throw "Set '$k' in $full" }
    }
    if (-not $cfg.zones -or @($cfg.zones).Count -eq 0) { $cfg.zones = @("$($cfg.region)-a", "$($cfg.region)-b", "$($cfg.region)-c") }
    # Windows' sshd matches the metadata key's user name only in lowercase.
    if (-not $cfg.sshUser) { $cfg.sshUser = ($env:USERNAME.ToLowerInvariant() -replace '[^a-z0-9_-]', '') }
    $cfg.sshUser = $cfg.sshUser.ToLowerInvariant()
    if (-not $cfg.localLogDir) { $cfg.localLogDir = Join-Path $env:LOCALAPPDATA 'xtp\cloud-runs' }
    $cfg['path'] = $full
    $cfg['serviceAccountEmail'] = "$($cfg.serviceAccount)@$($cfg.project).iam.gserviceaccount.com"
    $cfg['dataDisk'] = "$($cfg.vm)-data"
    if (-not $cfg.dataDiskType) { $cfg.dataDiskType = $cfg.diskType }
    return [pscustomobject]$cfg
}

function Write-Step([string]$Text) { Write-Host "==> $Text" -ForegroundColor Cyan }

# Runs gcloud with an argument array (never a joined string: PowerShell would
# split `type=x,count=1` at the comma). Returns stdout lines; throws on a
# non-zero exit unless -AllowFail, in which case $LASTEXITCODE tells.
function Invoke-Gcloud {
    param(
        [Parameter(Mandatory)] [string[]]$Arguments,
        [switch]$AllowFail,
        [switch]$Quiet
    )
    # Windows PowerShell 5.1 turns a native program's stderr into a
    # terminating error under 'Stop'; gcloud reports progress on stderr.
    $ErrorActionPreference = 'Continue'
    $errFile = [System.IO.Path]::GetTempFileName()
    try {
        $out = & gcloud @Arguments 2> $errFile
        $code = $LASTEXITCODE
        $err = Get-Content -LiteralPath $errFile -Raw
    } finally { Remove-Item -LiteralPath $errFile -ErrorAction SilentlyContinue }
    $global:XtpGcloudError = $err
    if ($code -ne 0 -and -not $AllowFail) {
        throw "gcloud $($Arguments[0..([Math]::Min(3, $Arguments.Count - 1))] -join ' ') failed ($code):`n$err"
    }
    if ($err -and -not $Quiet -and $code -eq 0) {
        $err -split "`r?`n" | Where-Object { $_ -match '^(WARNING|Created|Updated)' } | ForEach-Object { Write-Host "    $_" }
    }
    $global:LASTEXITCODE = $code
    return $out
}

function Get-XtpBaseArgList($Cfg) { return @("--project=$($Cfg.project)", '--quiet') }

# The zones of the instances named exactly $Name (gcloud's name= filter is
# a pattern match, which is changing: compare here).
function Get-XtpInstanceZone($Cfg, [string]$Name) {
    $rows = Invoke-Gcloud -Arguments (@('compute', 'instances', 'list', "--filter=name~^$([regex]::Escape($Name))", '--format=value(name,zone.basename())') + (Get-XtpBaseArgList $Cfg))
    return @($rows | Where-Object { $_ } | ForEach-Object { $n, $z = $_ -split '\s+'; if ($n -eq $Name) { $z } })
}

# The zone the VM lives in (it may be any of the configured zones), or $null.
function Get-XtpVmZone($Cfg) {
    $z = @(Get-XtpInstanceZone $Cfg $Cfg.vm)
    if ($z.Count -gt 1) { throw "Several VMs named $($Cfg.vm): $($z -join ', ')" }
    if ($z.Count -eq 0) { return $null }
    return $z[0]
}

function Get-XtpVmStatus($Cfg, [string]$Zone) {
    return (Invoke-Gcloud -Arguments (@('compute', 'instances', 'describe', $Cfg.vm, "--zone=$Zone", '--format=value(status)') + (Get-XtpBaseArgList $Cfg)) | Select-Object -First 1)
}

function Get-XtpSshArgList($Cfg, [string]$Zone) {
    return @("--zone=$Zone", "--project=$($Cfg.project)", '--tunnel-through-iap', '--quiet', '--strict-host-key-checking=no')
}

# Runs a PowerShell script block's text on the VM, over SSH through IAP.
# Windows' sshd starts cmd.exe; the script travels as -EncodedCommand, so it
# must fit a command line (compressed if needed). Larger tools are copied
# under C:\xtp\tools (see Send-XtpVmToolFolder). Returns the output
# lines; throws when the remote script fails, unless -AllowFail.
function Invoke-XtpVm {
    param(
        [Parameter(Mandatory)] $Cfg,
        [Parameter(Mandatory)] [string]$Zone,
        [Parameter(Mandatory)] [string]$Script,
        [switch]$AllowFail,
        [switch]$PassThru   # stream the output to the console as it comes
    )
    $wrapped = "`$ProgressPreference='SilentlyContinue'; `$ErrorActionPreference='Stop'; try {`n$Script`n} catch { [Console]::Out.WriteLine('REMOTE ERROR: ' + `$_.Exception.Message + ' ' + `$_.InvocationInfo.PositionMessage); exit 1 }; exit `$LASTEXITCODE"
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($wrapped))
    $command = "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $encoded"
    if ($command.Length -gt 8000) {
        # Keep read-only queries such as -Status in memory on the VM. Base64
        # fits in a cmd environment variable without shell quoting.
        $bytes = [Text.Encoding]::UTF8.GetBytes($wrapped)
        $buffer = [IO.MemoryStream]::new()
        try {
            $gzip = [IO.Compression.GZipStream]::new($buffer, [IO.Compression.CompressionMode]::Compress, $true)
            try { $gzip.Write($bytes, 0, $bytes.Length) } finally { $gzip.Dispose() }
            $payload = [Convert]::ToBase64String($buffer.ToArray())
        } finally { $buffer.Dispose() }
        $unpack = {
            $ErrorActionPreference = 'Stop'
            $m = [IO.MemoryStream]::new([Convert]::FromBase64String($env:XtpRemotePayload))
            $z = [IO.Compression.GZipStream]::new($m, [IO.Compression.CompressionMode]::Decompress)
            & ([scriptblock]::Create([IO.StreamReader]::new($z).ReadToEnd()))
            exit $LASTEXITCODE
        }.ToString()
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($unpack))
        $command = "set XtpRemotePayload=$payload&& powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $encoded"
    }
    if ($command.Length -gt 8000) { throw "Remote script too long ($($command.Length) characters); put it in a file under tools\cloud\vm." }
    $sshArgs = @('compute', 'ssh', "$($Cfg.sshUser)@$($Cfg.vm)") + (Get-XtpSshArgList $Cfg $Zone) + @("--command=$command", '--', '-batch')
    if ($PassThru) {
        $ErrorActionPreference = 'Continue'
        $lines = & gcloud @sshArgs 2>&1 | ForEach-Object { $line = "$_"; Write-Host $line; $line }
        $code = $LASTEXITCODE
        $global:LASTEXITCODE = $code
        if ($code -ne 0 -and -not $AllowFail) { throw "Remote command failed ($code)" }
        return $lines
    }
    $out = Invoke-Gcloud -Arguments $sshArgs -AllowFail
    $code = $LASTEXITCODE
    $global:LASTEXITCODE = $code
    if ($code -ne 0 -and -not $AllowFail) {
        throw "Remote command failed ($code):`n$($out -join "`n")`n$global:XtpGcloudError"
    }
    return $out
}

# Waits until the VM answers over SSH (after a start or a reboot).
function Wait-XtpVmSsh($Cfg, [string]$Zone, [int]$TimeoutSeconds = 900) {
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ($true) {
        $out = Invoke-XtpVm -Cfg $Cfg -Zone $Zone -Script 'Write-Output "ssh-ok $env:COMPUTERNAME"' -AllowFail
        if ($LASTEXITCODE -eq 0 -and ($out -match 'ssh-ok')) { return }
        # A Spot VM can be taken back by Google at any time, a reboot included.
        $status = Get-XtpVmStatus $Cfg $Zone
        if ($status -in 'TERMINATED', 'STOPPED', 'STOPPING') {
            throw "The VM stopped ($status) while waiting for SSH: a Spot VM taken back by Google? Start it again (start-vm.ps1) and rerun."
        }
        if ((Get-Date) -gt $deadline) { throw "The VM did not answer over SSH within $TimeoutSeconds s:`n$global:XtpGcloudError" }
        Start-Sleep -Seconds 15
    }
}

# Copies local files to the VM (scp through IAP). Destination is a Windows
# path on the VM, e.g. C:\xtp\tools.
function Send-XtpVmFile {
    param([Parameter(Mandatory)] $Cfg, [Parameter(Mandatory)] [string]$Zone,
        [Parameter(Mandatory)] [string[]]$Path, [Parameter(Mandatory)] [string]$Destination, [switch]$Recurse)
    $dest = '/' + ($Destination -replace '\\', '/')
    $a = @('compute', 'scp') + $Path + @("$($Cfg.sshUser)@$($Cfg.vm):$dest") + (Get-XtpSshArgList $Cfg $Zone)
    if ($Recurse) { $a += '--recurse' }
    $null = Invoke-Gcloud -Arguments $a
}

function Receive-XtpVmFile {
    param([Parameter(Mandatory)] $Cfg, [Parameter(Mandatory)] [string]$Zone,
        [Parameter(Mandatory)] [string]$Path, [Parameter(Mandatory)] [string]$Destination, [switch]$Recurse)
    $src = '/' + ($Path -replace '\\', '/')
    $a = @('compute', 'scp', "$($Cfg.sshUser)@$($Cfg.vm):$src", $Destination) + (Get-XtpSshArgList $Cfg $Zone)
    if ($Recurse) { $a += '--recurse' }
    $null = Invoke-Gcloud -Arguments $a
}

# Copies tools\cloud\vm\* to C:\xtp\tools\vm on the VM (always the local
# version, so a change is tested before it is committed).
function Send-XtpVmToolFolder($Cfg, [string]$Zone) {
    $null = Invoke-XtpVm -Cfg $Cfg -Zone $Zone -Script 'New-Item -ItemType Directory -Force -Path C:\xtp\tools\vm | Out-Null'
    $files = @(Get-ChildItem -LiteralPath (Join-Path $script:XtpCloudRoot 'vm') -File | ForEach-Object { $_.FullName })
    Send-XtpVmFile -Cfg $Cfg -Zone $Zone -Path $files -Destination 'C:\xtp\tools\vm'
}

# The VM, started if it is stopped; returns its zone. A stopped VM keeps its
# zone, and the zone may have no GPU free at that moment
# (ZONE_RESOURCE_POOL_EXHAUSTED): the start is tried again every 2 minutes
# for RetryMinutes (default: the configuration's startRetryMinutes).
function Start-XtpVm($Cfg, [int]$RetryMinutes = -1) {
    if ($RetryMinutes -lt 0) { $RetryMinutes = [int]$Cfg.startRetryMinutes }
    $zone = Get-XtpVmZone $Cfg
    if (-not $zone) { throw "No VM named $($Cfg.vm) in $($Cfg.project); create it with new-vm.ps1." }
    $status = Get-XtpVmStatus $Cfg $zone
    if ($status -ne 'RUNNING') {
        Write-Step "Starting $($Cfg.vm) in $zone (was $status)"
        while ((Get-XtpVmStatus $Cfg $zone) -in 'STOPPING', 'SUSPENDING') { Start-Sleep -Seconds 10 }
        $deadline = (Get-Date).AddMinutes($RetryMinutes)
        while ($true) {
            $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'start', $Cfg.vm, "--zone=$zone") + (Get-XtpBaseArgList $Cfg)) -AllowFail -Quiet
            if ($LASTEXITCODE -eq 0) { break }
            $err = $global:XtpGcloudError
            if ($err -notmatch 'RESOURCE_POOL_EXHAUSTED|STOCKOUT|does not have enough resources') { throw "Starting $($Cfg.vm) failed:`n$err" }
            if ((Get-Date) -ge $deadline) {
                throw "No GPU free in $zone to start $($Cfg.vm) (ZONE_RESOURCE_POOL_EXHAUSTED); tried for $RetryMinutes min. Try again later (start-vm.ps1 -RetryMinutes 120)."
            }
            Write-Host ("    {0:HH:mm:ss} no capacity in {1}, trying again in 2 minutes" -f (Get-Date), $zone)
            Start-Sleep -Seconds 120
        }
    }
    Wait-XtpVmSsh $Cfg $zone
    return $zone
}

# Runs a script of tools\cloud\vm on the VM with named arguments, streaming
# its output. Arguments are passed as a hashtable (strings, numbers, switches).
function Invoke-XtpVmTool {
    param([Parameter(Mandatory)] $Cfg, [Parameter(Mandatory)] [string]$Zone,
        [Parameter(Mandatory)] [string]$Name, [hashtable]$Arguments = @{}, [switch]$AllowFail)
    # GetEnumerator, not .Keys: a hashtable with a 'Keys' entry would return it.
    $parts = foreach ($entry in $Arguments.GetEnumerator()) {
        $k = $entry.Key
        $v = $entry.Value
        if ($v -is [bool] -or $v -is [System.Management.Automation.SwitchParameter]) {
            if ($v) { "-$k" }
        } elseif ($v -is [int] -or $v -is [long] -or $v -is [double]) {
            "-$k $v"
        } else {
            "-$k '" + ("$v" -replace "'", "''") + "'"
        }
    }
    $script = "& 'C:\xtp\tools\vm\$Name' $($parts -join ' ')"
    Invoke-XtpVm -Cfg $Cfg -Zone $Zone -Script $script -PassThru -AllowFail:$AllowFail
}
