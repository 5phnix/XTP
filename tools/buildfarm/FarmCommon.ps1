# FarmCommon.ps1: shared code of the XTP build farm tools, on the PC's side.
#
# The farm is a few Windows Spot VMs on Google Cloud that run cargo for the
# worktrees of this PC (docs/dev/build-farm.md). Everything specific to one
# person (project, zones, names, paths) is in the farm configuration file
# (farm.example.json shows it), never in the repository.
#
# Dot-sourced by the scripts next to it (`. FarmCommon.ps1`): the repository's
# asset policy allows .ps1, not .psm1.
#
# Windows PowerShell 5.1 and PowerShell 7. ASCII only.

Set-StrictMode -Off
$ErrorActionPreference = 'Stop'

$script:ToolsDir = $PSScriptRoot
$script:Gcloud = $null
$script:SlotToken = '@@FARM_SLOT@@'

# Exit codes of the farm's own failures (cargo's codes pass through).
$script:ExitUnavailable = 75   # the farm cannot take the command: run it locally
$script:ExitLocalOnly = 76     # the command must run on this PC

# .NET Framework writes Console.InputEncoding's preamble (a UTF-8 BOM when
# the console code page is 65001) at the start of a child's redirected
# stdin. Same code page without the BOM where the console allows it; the
# receiver is told how many bytes to skip in any case (Get-FarmStdinSkip).
try {
    if ([Console]::InputEncoding.GetPreamble().Length) { [Console]::InputEncoding = New-Object Text.UTF8Encoding $false }
} catch { }

function Get-FarmStdinSkip { [Console]::InputEncoding.GetPreamble().Length }

# ---------------------------------------------------------------------------
# Messages, configuration, JSON state
# ---------------------------------------------------------------------------

# Farm messages go to stderr: cargo's stdout (JSON messages, test output)
# stays exactly cargo's.
function Write-FarmMessage([string] $Message) {
    [Console]::Error.WriteLine("farm: $Message")
}

function Get-FarmConfigPath {
    if ($env:XTP_FARM_CONFIG) { return $env:XTP_FARM_CONFIG }
    Join-Path $env:LOCALAPPDATA 'xtp\buildfarm\farm.json'
}

function Test-FarmProperty($Object, [string] $Name) {
    $null -ne $Object -and ($Object.PSObject.Properties.Name -contains $Name)
}

function Get-FarmConfig {
    param([string] $Path = (Get-FarmConfigPath))
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "no build farm configuration at $Path (copy tools\buildfarm\farm.example.json there and fill it, or set XTP_FARM_CONFIG)"
    }
    $c = [IO.File]::ReadAllText((Resolve-Path -LiteralPath $Path).Path) | ConvertFrom-Json
    $defaults = [ordered]@{
        prefix           = 'xtp-farm'
        machineType      = 'c2d-highcpu-16'
        diskGb           = 110
        diskType         = 'pd-balanced'
        baseImageFamily  = 'windows-2025'
        baseImageProject = 'windows-cloud'
        imageFamily      = 'xtp-farm'
        network          = 'default'
        sshUser          = 'xtpfarm'
        idleMinutes      = 15
        dailyCapUsd      = 50
        maxBuildsPerVm   = 2
        slotsPerVm       = 6
        maxRunMinutes    = 30
        maxSendFileMB    = 20
        stateDir         = $null
        tempDir          = $null
        # Shader surveys (Copy-FarmPack.ps1, Invoke-FarmSurvey.ps1).
        packSource       = $null   # gs://.../shaders.rpf; default: the cloud configuration's bucket
        packSigner       = $null   # service account that signs the pack's download URL; default <prefix>-pack
        surveyDir        = $null   # where surveys come back; default farm-surveys next to stateDir
        surveyMinutes    = 90      # longest survey job, its build included
    }
    foreach ($k in $defaults.Keys) {
        if (-not (Test-FarmProperty $c $k) -or $null -eq $c.$k) {
            $c | Add-Member -Force -NotePropertyName $k -NotePropertyValue $defaults[$k]
        }
    }
    if (-not $c.project -or "$($c.project)" -match '^<') { throw "farm configuration ${Path}: set 'project'" }
    if (-not $c.vms) { throw "farm configuration ${Path}: list the VMs in 'vms'" }
    $full = (Resolve-Path -LiteralPath $Path).Path
    if (-not $c.stateDir) { $c.stateDir = Split-Path -Parent $full }
    if (-not $c.tempDir) { $c.tempDir = Join-Path $c.stateDir 'tmp' }
    if (-not $c.packSigner) { $c.packSigner = "$($c.prefix)-pack" }
    if (-not $c.surveyDir) { $c.surveyDir = Join-Path (Split-Path -Parent $c.stateDir) 'farm-surveys' }
    foreach ($vm in @($c.vms)) {
        if (-not (Test-FarmProperty $vm 'machineType') -or -not $vm.machineType) {
            $vm | Add-Member -Force -NotePropertyName machineType -NotePropertyValue $c.machineType
        }
        $vm | Add-Member -Force -NotePropertyName region -NotePropertyValue ($vm.zone -replace '-[a-z]$', '')
    }
    foreach ($d in $c.stateDir, $c.tempDir, (Join-Path $c.stateDir 'leases'), (Join-Path $c.stateDir 'vms'), (Join-Path $c.stateDir 'ssh')) {
        New-Item -ItemType Directory -Force -Path $d | Out-Null
    }
    $c | Add-Member -Force -NotePropertyName configPath -NotePropertyValue $full
    $c
}

function Get-FarmVmSpec([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Name) {
    $vm = @($Config.vms) | Where-Object { $_.name -eq $Name } | Select-Object -First 1
    if (-not $vm) { throw "build farm: no VM named $Name in $($Config.configPath)" }
    $vm
}

function Get-FarmRegions([Parameter(Mandatory)] $Config) {
    @(@($Config.vms) | ForEach-Object { $_.region } | Sort-Object -Unique)
}

function Read-FarmJson([string] $Path, $Default = $null) {
    if (-not (Test-Path -LiteralPath $Path)) { return $Default }
    for ($i = 0; $i -lt 20; $i++) {
        try {
            $t = [IO.File]::ReadAllText($Path)
            if (-not $t.Trim()) { return $Default }
            return ($t | ConvertFrom-Json)
        } catch [IO.IOException] {
            Start-Sleep -Milliseconds 100
        } catch {
            return $Default
        }
    }
    $Default
}

function Write-FarmJson([string] $Path, $Value, [int] $Depth = 8) {
    $tmp = "$Path.$PID.tmp"
    [IO.File]::WriteAllText($tmp, (ConvertTo-Json -InputObject $Value -Depth $Depth), (New-Object Text.UTF8Encoding $false))
    for ($i = 0; $i -lt 50; $i++) {
        try {
            Move-Item -Force -LiteralPath $tmp -Destination $Path
            return
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    throw "build farm: cannot write $Path"
}

function Get-FarmHash([string] $Text, [int] $Length = 12) {
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $h = $sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($Text))
    } finally {
        $sha.Dispose()
    }
    ((@($h) | ForEach-Object { $_.ToString('x2') }) -join '').Substring(0, $Length)
}

function Get-FarmFileHash([string] $Path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

# Timestamps: GCE's RFC 3339 strings, ledger 'o' strings, or DateTime (which
# PowerShell 7's ConvertFrom-Json makes of them).
function ConvertTo-FarmUtc($Value) {
    if ($null -eq $Value -or "$Value" -eq '') { return $null }
    if ($Value -is [datetime]) { return $Value.ToUniversalTime() }
    if ($Value -is [DateTimeOffset]) { return $Value.UtcDateTime }
    [DateTimeOffset]::Parse("$Value", [Globalization.CultureInfo]::InvariantCulture).UtcDateTime
}

function ConvertTo-FarmStamp([datetime] $Utc) {
    $Utc.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ', [Globalization.CultureInfo]::InvariantCulture)
}

# One lock for the farm's state on this PC (all worktrees share it).
function Use-FarmLock {
    param([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [scriptblock] $Script, [string] $Name = 'state')
    $m = New-Object System.Threading.Mutex($false, "Global\xtp-farm-$Name-$(Get-FarmHash $Config.stateDir)")
    $got = $false
    try {
        try {
            $got = $m.WaitOne([TimeSpan]::FromMinutes(5))
        } catch [System.Threading.AbandonedMutexException] {
            $got = $true
        }
        if (-not $got) { throw "build farm: the '$Name' lock stayed busy for 5 minutes" }
        & $Script
    } finally {
        if ($got) { $m.ReleaseMutex() }
        $m.Dispose()
    }
}

function Write-FarmLog([Parameter(Mandatory)] $Config, [string] $Message) {
    $line = '{0} [{1}] {2}' -f (ConvertTo-FarmStamp (Get-Date).ToUniversalTime()), $PID, $Message
    for ($i = 0; $i -lt 10; $i++) {
        try {
            [IO.File]::AppendAllText((Join-Path $Config.stateDir 'farm.log'), "$line`r`n")
            return
        } catch {
            Start-Sleep -Milliseconds 50
        }
    }
}

# ---------------------------------------------------------------------------
# Processes
# ---------------------------------------------------------------------------

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

# Stops a process and every process it started (ssh and its IAP proxy).
function Stop-FarmProcessTree([int] $ProcessId) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = Join-Path $env:SystemRoot 'System32\taskkill.exe'
    $psi.Arguments = "/PID $ProcessId /T /F"
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $k = [System.Diagnostics.Process]::Start($psi)
    [void] $k.StandardOutput.ReadToEnd()
    [void] $k.StandardError.ReadToEnd()
    $k.WaitForExit()
    $k.Dispose()
}

# Runs a program. Without -Capture or -StdoutFile its stdout and stderr are
# this process's own (output streams straight through); -StderrFile sends
# its stderr to a file too.
# -TimeoutSeconds: stop its whole tree when it runs longer. -StallSeconds:
# stop it when it takes no stdin for that long (a stuck connection: ssh's own
# keepalives cannot fire while it is blocked on its proxy). Either way the
# result says TimedOut, with exit code 255 (a lost connection).
function Invoke-FarmProcess {
    param(
        [Parameter(Mandatory)] [string] $FilePath,
        [string[]] $ArgumentList = @(),
        [hashtable] $Environment = @{},
        [string] $WorkingDirectory = '',
        [string] $StdinFile = '',
        [string] $StdoutFile = '',
        [string] $StderrFile = '',
        [switch] $NoStdin,
        [switch] $Capture,
        [int] $TimeoutSeconds = 0,
        [int] $StallSeconds = 0
    )
    if ($Capture) { $StderrFile = '' }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $FilePath
    $psi.Arguments = (@($ArgumentList | ForEach-Object { ConvertTo-FarmArg $_ }) -join ' ')
    $psi.UseShellExecute = $false
    if ($WorkingDirectory) { $psi.WorkingDirectory = $WorkingDirectory }
    foreach ($k in $Environment.Keys) {
        if ($null -eq $Environment[$k]) {
            $psi.EnvironmentVariables.Remove($k)
        } else {
            $psi.EnvironmentVariables[$k] = [string] $Environment[$k]
        }
    }
    $psi.RedirectStandardInput = [bool] $StdinFile -or $NoStdin
    $psi.RedirectStandardOutput = $Capture -or [bool] $StdoutFile
    $psi.RedirectStandardError = $Capture -or [bool] $StderrFile
    if ($Capture) {
        $psi.StandardOutputEncoding = New-Object Text.UTF8Encoding $false
        $psi.StandardErrorEncoding = New-Object Text.UTF8Encoding $false
    }
    $deadline = $(if ($TimeoutSeconds -gt 0) { (Get-Date).AddSeconds($TimeoutSeconds) } else { [datetime]::MaxValue })
    $p = [System.Diagnostics.Process]::Start($psi)
    $fs = $null
    $efs = $null
    $timedOut = $false
    try {
        $outTask = $null
        $errTask = $null
        $copyTask = $null
        $errCopyTask = $null
        if ($Capture) {
            $outTask = $p.StandardOutput.ReadToEndAsync()
            $errTask = $p.StandardError.ReadToEndAsync()
        } elseif ($StdoutFile) {
            $fs = [IO.File]::Create($StdoutFile)
            $copyTask = $p.StandardOutput.BaseStream.CopyToAsync($fs)
        }
        if ($StderrFile) {
            $efs = [IO.File]::Create($StderrFile)
            $errCopyTask = $p.StandardError.BaseStream.CopyToAsync($efs)
        }
        if ($psi.RedirectStandardInput) {
            if ($StdinFile) {
                $src = [IO.File]::OpenRead($StdinFile)
                try {
                    $in = $p.StandardInput.BaseStream
                    $buf = New-Object byte[] 65536
                    while (($n = $src.Read($buf, 0, $buf.Length)) -gt 0) {
                        # Each chunk must go within StallSeconds (and the deadline).
                        [double] $wait = [Math]::Min([double] [int]::MaxValue, [double] ($deadline - (Get-Date)).TotalMilliseconds)
                        if ($StallSeconds -gt 0) { $wait = [Math]::Min($wait, [double] $StallSeconds * 1000) }
                        $t = $in.WriteAsync($buf, 0, $n)
                        $ok = $false
                        try { $ok = $t.Wait([int] [Math]::Max(0.0, $wait)) } catch { $ok = $true }  # the program exited early
                        if (-not $ok) { $timedOut = $true; break }
                        if ($t.IsFaulted) { break }
                    }
                    if (-not $timedOut) { try { $in.Flush() } catch { } }
                } finally {
                    $src.Dispose()
                }
            }
            if ($timedOut) {
                Stop-FarmProcessTree $p.Id
            } else {
                try { $p.StandardInput.Close() } catch { }
            }
        }
        if (-not $timedOut) {
            $left = ($deadline - (Get-Date)).TotalMilliseconds
            if ($left -lt [int]::MaxValue) {
                if (-not $p.WaitForExit([int] [Math]::Max(0.0, [double] $left))) {
                    $timedOut = $true
                    Stop-FarmProcessTree $p.Id
                }
            }
        }
        $p.WaitForExit()
        if ($copyTask) { try { $copyTask.Wait() } catch { }; $fs.Dispose(); $fs = $null }
        if ($errCopyTask) { try { $errCopyTask.Wait() } catch { }; $efs.Dispose(); $efs = $null }
        $code = $(if ($timedOut) { 255 } else { $p.ExitCode })
        $r = [pscustomobject]@{ ExitCode = $code; Out = ''; Err = ''; TimedOut = $timedOut }
        if ($Capture) {
            try { $r.Out = $outTask.Result } catch { }
            try { $r.Err = $errTask.Result } catch { }
        }
        $r
    } finally {
        if ($fs) { $fs.Dispose() }
        if ($efs) { $efs.Dispose() }
        $p.Dispose()
    }
}

function Get-FarmGitExe {
    $c = Get-Command git.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $c) { throw 'build farm: git is not on PATH' }
    $c.Source
}

function Invoke-FarmGit {
    param([Parameter(Mandatory)] [string] $Dir, [Parameter(Mandatory)] [string[]] $Arguments, [hashtable] $Environment = @{}, [switch] $AllowFailure)
    $r = Invoke-FarmProcess -FilePath (Get-FarmGitExe) -ArgumentList (@('-C', $Dir) + $Arguments) -Environment $Environment -Capture -NoStdin
    if ($r.ExitCode -ne 0 -and -not $AllowFailure) {
        throw ("git {0} failed ({1}): {2}" -f ($Arguments -join ' '), $r.ExitCode, $r.Err.Trim())
    }
    $r
}

# ---------------------------------------------------------------------------
# gcloud
# ---------------------------------------------------------------------------

# gcloud runs through the SDK's Python directly: no cmd.exe in between, so
# arguments and binary streams pass untouched (and it starts faster).
function Get-FarmGcloud {
    if ($script:Gcloud) { return $script:Gcloud }
    $cmd = Get-Command gcloud.cmd, gcloud.ps1, gcloud -ErrorAction SilentlyContinue | Where-Object { $_.Source } | Select-Object -First 1
    if (-not $cmd) { throw 'build farm: gcloud is not on PATH (install the Google Cloud SDK)' }
    $sdk = Split-Path -Parent (Split-Path -Parent $cmd.Source)
    $py = $env:CLOUDSDK_PYTHON
    if (-not $py) { $py = Join-Path $sdk 'platform\bundledpython\python.exe' }
    $main = Join-Path $sdk 'lib\gcloud.py'
    if (-not (Test-Path -LiteralPath $py) -or -not (Test-Path -LiteralPath $main)) {
        throw "build farm: cannot find the Google Cloud SDK's Python or gcloud.py under $sdk (set CLOUDSDK_PYTHON)"
    }
    $script:Gcloud = [pscustomobject]@{ Python = $py; Main = $main; Sdk = $sdk }
    $script:Gcloud
}

# No prompts, and no log file per call in gcloud's configuration folder:
# every SSH connection runs one gcloud.
function Get-FarmGcloudEnv {
    @{ CLOUDSDK_CORE_DISABLE_PROMPTS = '1'; CLOUDSDK_CORE_DISABLE_FILE_LOGGING = 'true'; PYTHONIOENCODING = 'utf-8'; PYTHONUTF8 = '1' }
}

function Invoke-FarmGcloud {
    param([Parameter(Mandatory)] [string[]] $Arguments, $Config, [switch] $Json, [switch] $AllowFailure)
    $g = Get-FarmGcloud
    $a = @('-S', $g.Main) + $Arguments + @('--quiet', '--verbosity=error')
    if ($Config) { $a += "--project=$($Config.project)" }
    if ($Json) { $a += '--format=json' }
    $r = Invoke-FarmProcess -FilePath $g.Python -ArgumentList $a -Capture -NoStdin -Environment (Get-FarmGcloudEnv)
    if ($r.ExitCode -ne 0 -and -not $AllowFailure) {
        throw ("gcloud {0} failed ({1}): {2}" -f ($Arguments -join ' '), $r.ExitCode, $r.Err.Trim())
    }
    $data = $null
    if ($Json -and $r.ExitCode -eq 0 -and $r.Out.Trim()) { $data = $r.Out | ConvertFrom-Json }
    $r | Add-Member -NotePropertyName Data -NotePropertyValue $data
    $r
}

# ---------------------------------------------------------------------------
# Instances, prices, ledger, budget
# ---------------------------------------------------------------------------

function Test-FarmStopped($Instance) { @('TERMINATED', 'STOPPED', 'SUSPENDED') -contains $Instance.status }
function Test-FarmRunning($Instance) { $Instance.status -eq 'RUNNING' }
function Test-FarmStarting($Instance) { @('PROVISIONING', 'STAGING') -contains $Instance.status }

# The farm's VMs as GCE sees them (cached for a few seconds), and the ledger
# brought up to date from their start and stop times.
function Get-FarmInstances {
    param([Parameter(Mandatory)] $Config, [switch] $Refresh, [int] $MaxAgeSeconds = 20)
    $cache = Join-Path $Config.stateDir 'instances.json'
    if (-not $Refresh -and (Test-Path -LiteralPath $cache)) {
        $age = ((Get-Date) - (Get-Item -LiteralPath $cache).LastWriteTime).TotalSeconds
        if ($age -lt $MaxAgeSeconds) {
            $c = Read-FarmJson $cache
            if ($null -ne $c) { return @($c) }
        }
    }
    $r = Invoke-FarmGcloud -Config $Config -Json -Arguments @('compute', 'instances', 'list', '--filter=labels.xtp-role=buildfarm')
    $names = @(@($Config.vms) | ForEach-Object { $_.name })
    $list = @()
    foreach ($i in @($r.Data)) {
        if ($null -eq $i -or $names -notcontains $i.name) { continue }
        $list += [pscustomobject]@{
            name        = $i.name
            id          = "$($i.id)"
            zone        = ($i.zone -split '/')[-1]
            status      = $i.status
            machineType = ($i.machineType -split '/')[-1]
            lastStart   = $(if ($i.lastStartTimestamp) { ConvertTo-FarmStamp (ConvertTo-FarmUtc $i.lastStartTimestamp) } else { $null })
            lastStop    = $(if ($i.lastStopTimestamp) { ConvertTo-FarmStamp (ConvertTo-FarmUtc $i.lastStopTimestamp) } else { $null })
        }
    }
    Write-FarmJson $cache @($list)
    Update-FarmLedger -Config $Config -Instances $list
    @($list)
}

function Get-FarmPrices([Parameter(Mandatory)] $Config) {
    Read-FarmJson (Join-Path $Config.stateDir 'prices.json')
}

function Get-FarmSkus([string] $Token, [string] $Service, [string] $Pattern) {
    $out = New-Object System.Collections.ArrayList
    $page = ''
    do {
        $uri = "https://cloudbilling.googleapis.com/v1/services/$Service/skus?currencyCode=USD&pageSize=5000"
        if ($page) { $uri += '&pageToken=' + [uri]::EscapeDataString($page) }
        $resp = $null
        for ($try = 1; -not $resp; $try++) {
            try {
                $resp = Invoke-WebRequest -UseBasicParsing -Uri $uri -Headers @{ Authorization = "Bearer $Token" }
            } catch {
                # The catalog allows a few requests a minute per client.
                if ($try -ge 6) { throw }
                Start-Sleep -Seconds (20 * $try)
            }
        }
        $j = $resp.Content | ConvertFrom-Json
        foreach ($s in @($j.skus)) {
            if ($s -and $s.description -match $Pattern) { [void] $out.Add($s) }
        }
        $page = $j.nextPageToken
    } while ($page)
    $out
}

function Get-FarmSkuPrice($Sku) {
    $t = @($Sku.pricingInfo[0].pricingExpression.tieredRates) | Select-Object -Last 1
    [double] $t.unitPrice.units + [double] $t.unitPrice.nanos / 1e9
}

function Select-FarmSku($Skus, [string] $Pattern, [string] $Region, [string] $Exclude = '^$') {
    @($Skus) | Where-Object {
        $_.description -match $Pattern -and $_.description -notmatch $Exclude -and
        (-not $Region -or @($_.serviceRegions) -contains $Region)
    } | Select-Object -First 1
}

# Today's list prices from the Cloud Billing Catalog API (public prices; your
# invoice may differ with discounts or credits). Kept in prices.json.
function Update-FarmPrices {
    param([Parameter(Mandatory)] $Config)
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $token = (Invoke-FarmGcloud -Arguments @('auth', 'print-access-token')).Out.Trim()
    Write-FarmMessage 'reading the Cloud Billing price list (about a minute)'
    $compute = Get-FarmSkus -Token $token -Service '6F81-5844-456A' -Pattern '^(Spot Preemptible |Licensing Fee for Windows Server|Balanced PD Capacity|SSD backed PD Capacity|Storage PD Capacity|Storage Image)'
    $net = Get-FarmSkus -Token $token -Service 'E505-1604-58F8' -Pattern '^Networking Cloud (Nat|NAT) '
    $ver = ([regex]::Match($Config.baseImageFamily, '\d{4}')).Value
    $lic = Select-FarmSku $compute "^Licensing Fee for Windows Server $ver Datacenter Edition on VM$" ''
    if (-not $lic) { throw "price list: no Windows Server $ver Datacenter licence price" }
    $natVm = Select-FarmSku $net '^Networking Cloud Nat Gateway Uptime$' ''
    $natIp = Select-FarmSku $net '^Networking Cloud NAT IP Usage$' ''
    $machines = [ordered]@{}
    $disks = [ordered]@{}
    $diskPattern = @{ 'pd-balanced' = '^Balanced PD Capacity( in .*)?$'; 'pd-ssd' = '^SSD backed PD Capacity( in .*)?$'; 'pd-standard' = '^Storage PD Capacity( in .*)?$' }[$Config.diskType]
    if (-not $diskPattern) { throw "price list: unknown disk type $($Config.diskType)" }
    foreach ($vm in @($Config.vms)) {
        $key = "$($vm.machineType)@$($vm.region)"
        if (-not $machines.Contains($key)) {
            $mt = (Invoke-FarmGcloud -Config $Config -Json -Arguments @('compute', 'machine-types', 'describe', $vm.machineType, "--zone=$($vm.zone)")).Data
            $cpus = [double] $mt.guestCpus
            $gib = [double] $mt.memoryMb / 1024
            $family = ($vm.machineType -split '-')[0].ToUpper()
            $core = Select-FarmSku $compute "^Spot Preemptible $family\b.*\bInstance Core running in" $vm.region 'Custom|Sole Tenancy|Extended'
            $ram = Select-FarmSku $compute "^Spot Preemptible $family\b.*\bInstance Ram running in" $vm.region 'Custom|Sole Tenancy|Extended'
            if (-not $core -or -not $ram) { throw "price list: no Spot $family price in $($vm.region)" }
            $m = [ordered]@{
                machineType     = $vm.machineType
                region          = $vm.region
                vcpus           = $cpus
                memoryGiB       = $gib
                spotCoreHour    = (Get-FarmSkuPrice $core)
                spotRamGiBHour  = (Get-FarmSkuPrice $ram)
                licenseCoreHour = (Get-FarmSkuPrice $lic)
                natVmHour       = $(if ($natVm) { Get-FarmSkuPrice $natVm } else { 0.0 })
            }
            $m.usdPerHour = [Math]::Round($cpus * $m.spotCoreHour + $gib * $m.spotRamGiBHour + $cpus * $m.licenseCoreHour + $m.natVmHour, 4)
            $machines[$key] = $m
        }
        if (-not $disks.Contains($vm.region)) {
            $d = Select-FarmSku $compute $diskPattern $vm.region
            if (-not $d) { throw "price list: no $($Config.diskType) price in $($vm.region)" }
            $disks[$vm.region] = Get-FarmSkuPrice $d
        }
    }
    $img = Select-FarmSku $compute '^Storage Image( in .*)?$' (@($Config.vms)[0].region)
    $prices = [ordered]@{
        fetched      = (ConvertTo-FarmStamp (Get-Date).ToUniversalTime())
        source       = 'Cloud Billing Catalog API, list prices in USD'
        machines     = $machines
        diskGbMonth  = $disks
        imageGbMonth = $(if ($img) { Get-FarmSkuPrice $img } else { 0.05 })
        natIpHour    = $(if ($natIp) { Get-FarmSkuPrice $natIp } else { 0.005 })
    }
    Write-FarmJson (Join-Path $Config.stateDir 'prices.json') $prices
    [pscustomobject] $prices
}

# USD per hour of one running VM: Spot vCPUs and memory, the Windows Server
# licence (billed per vCPU, not discounted by Spot) and Cloud NAT.
function Get-FarmRate([Parameter(Mandatory)] $Config, $Prices, [string] $MachineType, [string] $Zone) {
    $region = $Zone -replace '-[a-z]$', ''
    $key = "$MachineType@$region"
    if ($Prices -and (Test-FarmProperty $Prices.machines $key)) { return [double] $Prices.machines.$key.usdPerHour }
    if ((Test-FarmProperty $Config 'fallbackUsdPerVmHour') -and $Config.fallbackUsdPerVmHour) { return [double] $Config.fallbackUsdPerVmHour }
    throw "build farm: no price for $key (run tools\buildfarm\Update-FarmPrices.ps1)"
}

# What the farm costs per day even when every VM is stopped: disks, image,
# Cloud NAT addresses.
function Get-FarmFixedDailyUsd([Parameter(Mandatory)] $Config, $Prices, $Instances) {
    if (-not $Prices) { return 0.0 }
    $usd = 0.0
    foreach ($i in @($Instances)) {
        if (-not $i) { continue }
        $region = $i.zone -replace '-[a-z]$', ''
        if (Test-FarmProperty $Prices.diskGbMonth $region) {
            $usd += [double] $Config.diskGb * [double] $Prices.diskGbMonth.$region * 12 / 365
        }
    }
    $image = Read-FarmJson (Join-Path $Config.stateDir 'image.json')
    if ($image -and $image.archiveGb) { $usd += [double] $image.archiveGb * [double] $Prices.imageGbMonth * 12 / 365 }
    $usd += (Get-FarmRegions $Config).Count * [double] $Prices.natIpHour * 24
    $usd
}

# The ledger: one interval per VM start, from GCE's own start and stop times.
function Update-FarmLedger {
    param([Parameter(Mandatory)] $Config, $Instances)
    $prices = Get-FarmPrices $Config
    Use-FarmLock -Config $Config -Name 'ledger' -Script {
        $path = Join-Path $Config.stateDir 'ledger.json'
        $ledger = Read-FarmJson $path
        $rows = New-Object System.Collections.ArrayList
        if ($ledger -and $ledger.intervals) {
            foreach ($r in @($ledger.intervals)) { if ($r) { [void] $rows.Add($r) } }
        }
        foreach ($i in @($Instances)) {
            if (-not $i -or -not $i.lastStart) { continue }
            $end = $null
            if ((Test-FarmStopped $i) -and $i.lastStop -and ((ConvertTo-FarmUtc $i.lastStop) -ge (ConvertTo-FarmUtc $i.lastStart))) {
                $end = $i.lastStop
            }
            $row = $rows | Where-Object { $_.vm -eq $i.name -and $_.start -eq $i.lastStart } | Select-Object -First 1
            if (-not $row) {
                # A start not seen before closes this VM's older open interval.
                foreach ($o in @($rows | Where-Object { $_.vm -eq $i.name -and -not $_.end })) {
                    $o.end = $i.lastStart
                    $o | Add-Member -Force -NotePropertyName note -NotePropertyValue 'stop not observed: closed at the next start'
                }
                [void] $rows.Add([pscustomobject]@{
                        vm          = $i.name
                        machineType = $i.machineType
                        zone        = $i.zone
                        start       = $i.lastStart
                        end         = $end
                        usdPerHour  = (Get-FarmRate $Config $prices $i.machineType $i.zone)
                    })
            } elseif ($end) {
                $row.end = $end
            }
        }
        Write-FarmJson $path ([pscustomobject]@{ updated = (ConvertTo-FarmStamp (Get-Date).ToUniversalTime()); intervals = @($rows) })
    }
}

# Spend of one local day (machine-hours priced at the ledger's rates, plus
# the fixed daily cost), counting a running VM up to now.
function Get-FarmSpend {
    param([Parameter(Mandatory)] $Config, [datetime] $Day = (Get-Date).Date, $Instances = $null)
    $from = $Day.Date.ToUniversalTime()
    $to = $Day.Date.AddDays(1).ToUniversalTime()
    $now = (Get-Date).ToUniversalTime()
    $ledger = Read-FarmJson (Join-Path $Config.stateDir 'ledger.json')
    $hours = 0.0
    $machineUsd = 0.0
    $perVm = @{}
    if ($ledger -and $ledger.intervals) {
        foreach ($r in @($ledger.intervals)) {
            if (-not $r) { continue }
            $s = ConvertTo-FarmUtc $r.start
            $e = $(if ($r.end) { ConvertTo-FarmUtc $r.end } else { $now })
            $a = $(if ($s -gt $from) { $s } else { $from })
            $b = $(if ($e -lt $to) { $e } else { $to })
            if ($b -gt $a) {
                $h = ($b - $a).TotalHours
                $hours += $h
                $machineUsd += $h * [double] $r.usdPerHour
                if (-not $perVm.ContainsKey($r.vm)) { $perVm[$r.vm] = 0.0 }
                $perVm[$r.vm] += $h
            }
        }
    }
    if ($null -eq $Instances) { $Instances = Read-FarmJson (Join-Path $Config.stateDir 'instances.json') @() }
    $fixed = Get-FarmFixedDailyUsd $Config (Get-FarmPrices $Config) $Instances
    [pscustomobject]@{
        Day          = $Day.Date
        MachineHours = $hours
        MachineUsd   = $machineUsd
        FixedUsd     = $fixed
        TotalUsd     = $machineUsd + $fixed
        CapUsd       = [double] $Config.dailyCapUsd
        HoursPerVm   = $perVm
    }
}

# May the farm start one more VM today? Each running VM is reserved its idle
# window; the VM to start is reserved its boot and idle window.
function Test-FarmBudget {
    param([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] $Instances, [string] $StartVm = '')
    $spend = Get-FarmSpend -Config $Config -Instances $Instances
    $prices = Get-FarmPrices $Config
    $reserve = 0.0
    foreach ($i in @($Instances)) {
        if ($i -and ((Test-FarmRunning $i) -or (Test-FarmStarting $i))) {
            $reserve += (Get-FarmRate $Config $prices $i.machineType $i.zone) * [double] $Config.idleMinutes / 60
        }
    }
    if ($StartVm) {
        $spec = Get-FarmVmSpec $Config $StartVm
        $reserve += (Get-FarmRate $Config $prices $spec.machineType $spec.zone) * ([double] $Config.idleMinutes + 5) / 60
    }
    $ok = ($spend.TotalUsd + $reserve) -le $spend.CapUsd
    $msg = ('daily budget: {0:N2} USD spent today of {1:N2} (machine-hours {2:N1}), {3:N2} reserved for running VMs and the one to start' -f `
            $spend.TotalUsd, $spend.CapUsd, $spend.MachineHours, $reserve)
    [pscustomobject]@{ Ok = $ok; Spend = $spend; Reserve = $reserve; Message = $msg }
}

# ---------------------------------------------------------------------------
# SSH over IAP
# ---------------------------------------------------------------------------

function Get-FarmSshExe {
    $p = Join-Path $env:SystemRoot 'System32\OpenSSH\ssh.exe'
    if (Test-Path -LiteralPath $p) { return $p }
    throw 'build farm: the Windows OpenSSH client is missing (Settings > System > Optional features > OpenSSH Client)'
}

function Get-FarmSshPaths([Parameter(Mandatory)] $Config) {
    $dir = Join-Path $Config.stateDir 'ssh'
    [pscustomobject]@{
        Dir    = $dir
        Key    = Join-Path $dir 'farm_ed25519'
        Pub    = Join-Path $dir 'farm_ed25519.pub'
        Known  = Join-Path $dir 'known_hosts'
        Config = Join-Path $dir 'config'
    }
}

# The farm's own key pair and ssh config: one Host per VM, reached through an
# IAP tunnel on stdin (no listening port, no public address on the VM).
function Initialize-FarmSsh([Parameter(Mandatory)] $Config) {
    $p = Get-FarmSshPaths $Config
    if (-not (Test-Path -LiteralPath $p.Key)) {
        $kg = Join-Path (Split-Path -Parent (Get-FarmSshExe)) 'ssh-keygen.exe'
        $r = Invoke-FarmProcess -FilePath $kg -ArgumentList @('-q', '-t', 'ed25519', '-N', '', '-C', 'xtp-farm', '-f', $p.Key) -Capture -NoStdin
        if ($r.ExitCode -ne 0) { throw "ssh-keygen failed: $($r.Err)" }
        # OpenSSH refuses a private key that other accounts can read.
        $r = Invoke-FarmProcess -FilePath 'icacls.exe' -ArgumentList @($p.Key, '/inheritance:r', '/grant:r', "$($env:USERDOMAIN)\$($env:USERNAME):F") -Capture -NoStdin
        if ($r.ExitCode -ne 0) { throw "icacls on $($p.Key) failed: $($r.Out) $($r.Err)" }
    }
    if (-not (Test-Path -LiteralPath $p.Known)) { [IO.File]::WriteAllText($p.Known, '') }
    $g = Get-FarmGcloud
    $key = $p.Key.Replace('\', '/')
    $known = $p.Known.Replace('\', '/')
    $lines = @()
    foreach ($vm in @($Config.vms)) {
        $lines += "Host $($vm.name)"
        $lines += "  HostName $($vm.name)"
        $lines += "  HostKeyAlias $($vm.name)"
        $lines += "  User $($Config.sshUser)"
        $lines += "  IdentityFile `"$key`""
        $lines += '  IdentitiesOnly yes'
        $lines += "  UserKnownHostsFile `"$known`""
        $lines += '  StrictHostKeyChecking yes'
        $lines += '  HostKeyAlgorithms ssh-ed25519'
        $lines += '  BatchMode yes'
        $lines += '  ServerAliveInterval 15'
        $lines += '  ServerAliveCountMax 4'
        $lines += '  LogLevel ERROR'
        $lines += "  ProxyCommand `"$($g.Python)`" -S `"$($g.Main)`" compute start-iap-tunnel $($vm.name) 22 --listen-on-stdin --project=$($Config.project) --zone=$($vm.zone) --verbosity=error"
    }
    [IO.File]::WriteAllText($p.Config, (($lines -join "`n") + "`n"), (New-Object Text.UTF8Encoding $false))
    $p
}

function Test-FarmKnownHost([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Vm) {
    $p = Get-FarmSshPaths $Config
    if (-not (Test-Path -LiteralPath $p.Known)) { return $false }
    [bool] (@(Get-Content -LiteralPath $p.Known) | Where-Object { $_.StartsWith("$Vm ") })
}

# Pins a VM's SSH host key from what its agent published in the guest
# attributes (xtp-farm/hostkey): no trust on first use.
function Update-FarmKnownHost {
    param([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Vm, [int] $WaitSeconds = 0)
    $spec = Get-FarmVmSpec $Config $Vm
    $deadline = (Get-Date).AddSeconds($WaitSeconds)
    $key = $null
    while ($true) {
        $r = Invoke-FarmGcloud -Config $Config -Json -AllowFailure -Arguments @(
            'compute', 'instances', 'get-guest-attributes', $Vm, "--zone=$($spec.zone)", '--query-path=xtp-farm/hostkey')
        if ($r.ExitCode -eq 0 -and $r.Data) {
            $key = (@($r.Data) | Where-Object { $_.key -eq 'hostkey' } | Select-Object -First 1).value
        }
        if ($key) { break }
        if ((Get-Date) -ge $deadline) { throw "build farm: $Vm has not published its SSH host key (guest attribute xtp-farm/hostkey)" }
        Start-Sleep -Seconds 10
    }
    $parts = "$key".Trim() -split '\s+'
    if ($parts.Count -lt 2 -or $parts[0] -ne 'ssh-ed25519') { throw "build farm: $Vm published an unexpected host key: $key" }
    $p = Get-FarmSshPaths $Config
    $lines = @(Get-Content -LiteralPath $p.Known -ErrorAction SilentlyContinue | Where-Object { $_ -and -not $_.StartsWith("$Vm ") })
    $lines += "$Vm $($parts[0]) $($parts[1])"
    [IO.File]::WriteAllText($p.Known, (($lines -join "`n") + "`n"))
}

function Remove-FarmKnownHost([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Vm) {
    $p = Get-FarmSshPaths $Config
    if (-not (Test-Path -LiteralPath $p.Known)) { return }
    $lines = @(Get-Content -LiteralPath $p.Known | Where-Object { $_ -and -not $_.StartsWith("$Vm ") })
    [IO.File]::WriteAllText($p.Known, (($lines -join "`n") + "`n"))
}

# One command on a VM through cmd.exe. Without -Capture or -StdoutFile its
# output streams to this process's stdout and stderr.
function Invoke-FarmSsh {
    param(
        [Parameter(Mandatory)] $Config,
        [Parameter(Mandatory)] [string] $Vm,
        [Parameter(Mandatory)] [string] $Command,
        [string] $StdinFile = '',
        [string] $StdoutFile = '',
        [string] $StderrFile = '',
        [switch] $Capture,
        [int] $ConnectTimeout = 60,
        # Whole command; short commands get 3 minutes. 0: no limit.
        [int] $TimeoutSeconds = 180,
        # Longest wait for the VM to take more of stdin.
        [int] $StallSeconds = 60
    )
    $p = Get-FarmSshPaths $Config
    if (-not (Test-Path -LiteralPath $p.Config)) { $p = Initialize-FarmSsh $Config }
    if (-not (Test-FarmKnownHost $Config $Vm)) { Update-FarmKnownHost -Config $Config -Vm $Vm -WaitSeconds 300 }
    $a = @('-F', $p.Config, '-T', '-o', "ConnectTimeout=$ConnectTimeout", '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=4')
    if (-not $StdinFile) { $a += '-n' }
    $a += @($Vm, $Command)
    $r = Invoke-FarmProcess -FilePath (Get-FarmSshExe) -ArgumentList $a -StdinFile $StdinFile -StdoutFile $StdoutFile -StderrFile $StderrFile -Capture:$Capture `
        -NoStdin:(-not $StdinFile) -Environment (Get-FarmGcloudEnv) -TimeoutSeconds $TimeoutSeconds -StallSeconds $StallSeconds
    if ($r.TimedOut) {
        $what = $(if ($Command.Length -gt 60) { $Command.Substring(0, 60) + '...' } else { $Command })
        Write-FarmLog $Config "ssh to $Vm stopped: no answer in time ($what)"
    }
    $r
}

# The farm's ssh processes (and their IAP proxies) whose caller is gone: a
# cargo-remote stopped in the middle of a job leaves them behind, holding a
# connection. Those whose caller still runs are never touched. At most once a
# minute (all callers share the stamp).
function Remove-FarmOrphans([Parameter(Mandatory)] $Config, [switch] $Force) {
    $stamp = Join-Path $Config.stateDir 'orphans.stamp'
    if (-not $Force -and (Test-Path -LiteralPath $stamp) -and ((Get-Date) - (Get-Item -LiteralPath $stamp).LastWriteTime).TotalSeconds -lt 60) { return @() }
    [IO.File]::WriteAllText($stamp, '')
    $cfg = (Get-FarmSshPaths $Config).Config
    $byId = @{}
    foreach ($p in @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue)) { $byId[[int] $p.ProcessId] = $p }
    $killed = @()
    foreach ($p in @($byId.Values)) {
        $cl = "$($p.CommandLine)"
        $ours = ($p.Name -eq 'ssh.exe' -and $cl.IndexOf($cfg, [StringComparison]::OrdinalIgnoreCase) -ge 0) -or
        ($p.Name -eq 'python.exe' -and $cl -match 'start-iap-tunnel' -and $cl.Contains("--project=$($Config.project)") -and $cl.Contains(" $($Config.prefix)-"))
        if (-not $ours) { continue }
        $parent = $byId[[int] $p.ParentProcessId]
        # A parent created after its child is another process with a reused id.
        if ($parent -and $parent.CreationDate -le $p.CreationDate) { continue }
        Write-FarmLog $Config ('stopping orphaned {0} {1} (started {2:u})' -f $p.Name, $p.ProcessId, $p.CreationDate)
        Stop-FarmProcessTree ([int] $p.ProcessId)
        $killed += [int] $p.ProcessId
    }
    $killed
}

# After a failed job: is the VM still up and staying? When it shuts down
# (a Spot reclaim or a stop), Windows kills the job's processes, the runner
# included, so the exit code alone cannot tell that from a failed build.
function Test-FarmVmStaying([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Vm) {
    $r = Invoke-FarmSsh -Config $Config -Vm $Vm -Capture -ConnectTimeout 20 `
        -Command 'powershell -NoProfile -NonInteractive -OutputFormat Text -ExecutionPolicy Bypass -File C:\farm\bin\farm-probe.ps1'
    $r.ExitCode -eq 0 -and $r.Out.Trim() -eq 'ok'
}

function Wait-FarmVmReady {
    param([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Vm, [int] $TimeoutSeconds = 480)
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    $refreshed = $false
    while ($true) {
        $r = Invoke-FarmSsh -Config $Config -Vm $Vm -Command 'echo farm-ready' -Capture -ConnectTimeout 30
        if ($r.ExitCode -eq 0 -and $r.Out -match 'farm-ready') { return }
        if (-not $refreshed -and $r.Err -match 'IDENTIFICATION HAS CHANGED|Host key verification failed') {
            Update-FarmKnownHost -Config $Config -Vm $Vm -WaitSeconds 180
            $refreshed = $true
            continue
        }
        if ((Get-Date) -ge $deadline) { throw "build farm: $Vm does not answer over SSH: $($r.Err.Trim())" }
        Start-Sleep -Seconds 10
    }
}

# ---------------------------------------------------------------------------
# VMs and the agent installed on them
# ---------------------------------------------------------------------------

function Set-FarmHealth([Parameter(Mandatory)] $Config, [string] $Vm, [int] $Minutes, [string] $Reason) {
    Use-FarmLock -Config $Config -Name 'health' -Script {
        $path = Join-Path $Config.stateDir 'health.json'
        $h = @{}
        $old = Read-FarmJson $path
        if ($old) { foreach ($p in $old.PSObject.Properties) { $h[$p.Name] = $p.Value } }
        $h[$Vm] = [pscustomobject]@{ until = (ConvertTo-FarmStamp (Get-Date).ToUniversalTime().AddMinutes($Minutes)); reason = $Reason }
        Write-FarmJson $path ([pscustomobject] $h)
    }
}

function Get-FarmUnhealthy([Parameter(Mandatory)] $Config) {
    $h = Read-FarmJson (Join-Path $Config.stateDir 'health.json')
    $now = (Get-Date).ToUniversalTime()
    $out = @{}
    if ($h) {
        foreach ($p in $h.PSObject.Properties) {
            if ((ConvertTo-FarmUtc $p.Value.until) -gt $now) { $out[$p.Name] = $p.Value.reason }
        }
    }
    $out
}

function Start-FarmVm([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Vm) {
    $spec = Get-FarmVmSpec $Config $Vm
    Write-FarmMessage "starting $Vm ($($spec.zone))"
    $r = Invoke-FarmGcloud -Config $Config -AllowFailure -Arguments @('compute', 'instances', 'start', $Vm, "--zone=$($spec.zone)")
    Get-FarmInstances -Config $Config -Refresh | Out-Null
    if ($r.ExitCode -ne 0) {
        $why = $r.Err.Trim()
        Set-FarmHealth $Config $Vm 10 "start failed: $why"
        throw "build farm: cannot start ${Vm}: $why"
    }
    Write-FarmLog $Config "started $Vm"
}

function Stop-FarmVm([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Vm, [switch] $Async) {
    $spec = Get-FarmVmSpec $Config $Vm
    $a = @('compute', 'instances', 'stop', $Vm, "--zone=$($spec.zone)")
    if ($Async) { $a += '--async' }
    Invoke-FarmGcloud -Config $Config -Arguments $a | Out-Null
    Write-FarmLog $Config "stop requested for $Vm"
}

# The farm agent's version: a hash of the scripts in vm\ (line endings aside).
function Get-FarmAgentVersion {
    $sb = New-Object Text.StringBuilder
    foreach ($f in (Get-ChildItem -LiteralPath (Join-Path $script:ToolsDir 'vm') -File | Sort-Object Name)) {
        $t = [IO.File]::ReadAllText($f.FullName) -replace "`r`n", "`n"
        [void] $sb.Append($f.Name).Append("`n").Append($t).Append("`n")
    }
    Get-FarmHash $sb.ToString() 16
}

# Copies the scripts of vm\ to C:\farm\bin on a VM and (re)installs the agent.
function Publish-FarmAgent([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Vm) {
    $version = Get-FarmAgentVersion
    $stage = Join-Path $Config.tempDir "agent-$PID"
    if (Test-Path -LiteralPath $stage) { Remove-Item -Recurse -Force -LiteralPath $stage }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    $tgz = "$stage.tgz"
    try {
        Copy-Item -Path (Join-Path $script:ToolsDir 'vm\*') -Destination $stage
        [IO.File]::WriteAllText((Join-Path $stage 'version.txt'), $version)
        $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
        $r = Invoke-FarmProcess -FilePath $tar -ArgumentList @('-czf', $tgz, '-C', $stage, '.') -Capture -NoStdin
        if ($r.ExitCode -ne 0) { throw "tar failed: $($r.Err)" }
        # The VM saves exactly the archive's bytes (after the stdin preamble,
        # see Get-FarmStdinSkip), unpacks it and installs the agent.
        $size = (Get-Item -LiteralPath $tgz).Length
        $recv = @(
            '$ErrorActionPreference = ''Stop''; $ProgressPreference = ''SilentlyContinue'''
            "`$skip = $(Get-FarmStdinSkip); `$left = [int64] $size"
            'New-Item -ItemType Directory -Force C:\farm\bin, C:\farm\tmp | Out-Null'
            '$in = [Console]::OpenStandardInput(); $buf = New-Object byte[] 65536'
            'while ($skip -gt 0) { $n = $in.Read($buf, 0, $skip); if ($n -le 0) { break }; $skip -= $n }'
            '$fs = [IO.File]::Create(''C:\farm\tmp\agent.tgz'')'
            'while ($left -gt 0) { $n = $in.Read($buf, 0, [int] [Math]::Min(65536, $left)); if ($n -le 0) { break }; $fs.Write($buf, 0, $n); $left -= $n }'
            '$fs.Dispose(); if ($left -gt 0) { throw "archive incomplete: $left bytes missing" }'
            '& tar.exe -xzf C:\farm\tmp\agent.tgz -C C:\farm\bin; if ($LASTEXITCODE) { throw "tar exited with $LASTEXITCODE" }'
            '& powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File C:\farm\bin\farm-install.ps1; exit $LASTEXITCODE'
        ) -join "`n"
        $cmd = 'powershell -NoProfile -NonInteractive -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand ' + [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($recv))
        $r = Invoke-FarmSsh -Config $Config -Vm $Vm -Command $cmd -StdinFile $tgz -Capture
        if ($r.ExitCode -ne 0) { throw "build farm: agent update on $Vm failed ($($r.ExitCode)): $($r.Err.Trim()) $($r.Out.Trim())" }
    } finally {
        Remove-Item -Recurse -Force -LiteralPath $stage -ErrorAction SilentlyContinue
        Remove-Item -Force -LiteralPath $tgz -ErrorAction SilentlyContinue
    }
    Write-FarmLog $Config "agent $version installed on $Vm"
    $version
}

# ---------------------------------------------------------------------------
# Creating the farm
# ---------------------------------------------------------------------------

function Initialize-FarmApis([Parameter(Mandatory)] $Config) {
    $on = @((Invoke-FarmGcloud -Config $Config -Json -Arguments @('services', 'list', '--enabled')).Data | ForEach-Object { $_.config.name })
    foreach ($api in 'compute.googleapis.com', 'iap.googleapis.com') {
        if ($on -notcontains $api) {
            Write-FarmMessage "enabling $api"
            Invoke-FarmGcloud -Config $Config -Arguments @('services', 'enable', $api) | Out-Null
        }
    }
}

# SSH from Identity-Aware Proxy only, and outbound access (crates.io, rustup,
# installers) through Cloud NAT: the VMs have no public address.
function Initialize-FarmNetwork([Parameter(Mandatory)] $Config) {
    $fw = "$($Config.prefix)-allow-iap-ssh"
    $r = Invoke-FarmGcloud -Config $Config -AllowFailure -Arguments @('compute', 'firewall-rules', 'describe', $fw)
    if ($r.ExitCode -ne 0) {
        Write-FarmMessage "creating firewall rule $fw (SSH from IAP's range only)"
        Invoke-FarmGcloud -Config $Config -Arguments @('compute', 'firewall-rules', 'create', $fw, "--network=$($Config.network)",
            '--direction=INGRESS', '--action=ALLOW', '--rules=tcp:22', '--source-ranges=35.235.240.0/20',
            "--target-tags=$($Config.prefix)", '--description=XTP build farm: SSH through Identity-Aware Proxy only') | Out-Null
    }
    $routers = @((Invoke-FarmGcloud -Config $Config -Json -Arguments @('compute', 'routers', 'list')).Data)
    foreach ($region in Get-FarmRegions $Config) {
        $here = @($routers | Where-Object { $_ -and ($_.region -split '/')[-1] -eq $region -and ($_.network -split '/')[-1] -eq $Config.network })
        if (@($here | Where-Object { $_.nats -and @($_.nats).Count -gt 0 }).Count) { continue }
        $name = "$($Config.prefix)-nat-$region"
        if (-not @($here | Where-Object { $_.name -eq $name }).Count) {
            Write-FarmMessage "creating Cloud Router $name"
            Invoke-FarmGcloud -Config $Config -Arguments @('compute', 'routers', 'create', $name, "--network=$($Config.network)", "--region=$region") | Out-Null
        }
        Write-FarmMessage "creating Cloud NAT $name"
        Invoke-FarmGcloud -Config $Config -Arguments @('compute', 'routers', 'nats', 'create', $name, "--router=$name", "--region=$region",
            '--auto-allocate-nat-external-ips', '--nat-all-subnet-ip-ranges') | Out-Null
    }
}

function Get-FarmRustToolchain {
    $file = Join-Path (Split-Path -Parent (Split-Path -Parent $script:ToolsDir)) 'rust-toolchain.toml'
    $m = [regex]::Match([IO.File]::ReadAllText($file), '(?m)^\s*channel\s*=\s*"([^"]+)"')
    if (-not $m.Success) { throw "no channel in $file" }
    $m.Groups[1].Value
}

# Creates one farm VM: Spot (stopped, not deleted, when reclaimed), no public
# address, no service account, the farm's SSH key only, from the farm image
# or (-FromBaseImage, for the image builder) from Google's Windows image.
function New-FarmInstance {
    param([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Name, [switch] $FromBaseImage,
        [string] $StartupScript = '', [string[]] $Metadata = @())
    $spec = Get-FarmVmSpec $Config $Name
    $ssh = Initialize-FarmSsh $Config
    $metaDir = Join-Path $Config.tempDir "meta-$PID"
    New-Item -ItemType Directory -Force -Path $metaDir | Out-Null
    try {
        [IO.File]::WriteAllText((Join-Path $metaDir 'ssh-keys'), "$($Config.sshUser):$((Get-Content -Raw -LiteralPath $ssh.Pub).Trim())")
        $meta = @('enable-windows-ssh=TRUE', 'enable-guest-attributes=TRUE', 'block-project-ssh-keys=TRUE', "xtp-farm-idle-minutes=$($Config.idleMinutes)") + $Metadata
        $fromFile = @("ssh-keys=$(Join-Path $metaDir 'ssh-keys')")
        if ($StartupScript) { $fromFile += "windows-startup-script-ps1=$StartupScript" }
        if ($FromBaseImage) {
            [IO.File]::WriteAllText((Join-Path $metaDir 'specialize'), 'googet -noconfirm=true install google-compute-engine-ssh')
            $fromFile += "sysprep-specialize-script-cmd=$(Join-Path $metaDir 'specialize')"
            $image = @("--image-family=$($Config.baseImageFamily)", "--image-project=$($Config.baseImageProject)")
        } else {
            $image = @("--image-family=$($Config.imageFamily)", "--image-project=$($Config.project)")
        }
        $a = @('compute', 'instances', 'create', $Name, "--zone=$($spec.zone)", "--machine-type=$($spec.machineType)",
            '--provisioning-model=SPOT', '--instance-termination-action=STOP') + $image + @(
            "--boot-disk-size=$($Config.diskGb)GB", "--boot-disk-type=$($Config.diskType)",
            "--network=$($Config.network)", '--no-address', '--no-service-account', '--no-scopes',
            "--tags=$($Config.prefix)", '--labels=xtp-role=buildfarm',
            '--shielded-secure-boot', '--shielded-vtpm', '--shielded-integrity-monitoring',
            "--metadata=$($meta -join ',')", "--metadata-from-file=$($fromFile -join ',')")
        Write-FarmMessage "creating $Name ($($spec.machineType), Spot, $($spec.zone))"
        Invoke-FarmGcloud -Config $Config -Arguments $a | Out-Null
    } finally {
        Remove-Item -Recurse -Force -LiteralPath $metaDir -ErrorAction SilentlyContinue
    }
    Remove-FarmKnownHost $Config $Name
    Write-FarmLog $Config "created $Name"
    Get-FarmInstances -Config $Config -Refresh | Out-Null
}

# Follows the toolchain setup on the serial console until it is done; starts
# the VM again when Spot reclaims it meanwhile (the setup resumes).
function Wait-FarmSetup([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Vm, [int] $TimeoutMinutes = 120) {
    $spec = Get-FarmVmSpec $Config $Vm
    $deadline = (Get-Date).AddMinutes($TimeoutMinutes)
    $seen = @{}
    while ($true) {
        $r = Invoke-FarmGcloud -Config $Config -AllowFailure -Arguments @('compute', 'instances', 'get-serial-port-output', $Vm, "--zone=$($spec.zone)")
        if ($r.ExitCode -eq 0) {
            foreach ($line in ($r.Out -split "`r?`n")) {
                if ($line -notmatch 'xtp-farm-setup: (.*)$' -or $seen.ContainsKey($line)) { continue }
                $seen[$line] = $true
                $msg = $Matches[1].Trim()
                Write-FarmMessage "$Vm setup: $msg"
                if ($msg -eq 'done' -or $msg -eq 'already set up') { return }
                if ($msg -like 'FAILED*') { throw "build farm: the setup failed on ${Vm}: $msg (see C:\farm\log\setup.log there)" }
            }
        }
        $i = @(Get-FarmInstances -Config $Config -Refresh) | Where-Object { $_.name -eq $Vm } | Select-Object -First 1
        if ($i -and (Test-FarmStopped $i)) {
            Write-FarmMessage "$Vm stopped during its setup (Spot reclaim?): starting it again"
            Start-FarmVm $Config $Vm
        }
        if ((Get-Date) -gt $deadline) { throw "build farm: the setup of $Vm did not finish in $TimeoutMinutes minutes" }
        Start-Sleep -Seconds 20
    }
}

# ---------------------------------------------------------------------------
# Choosing a VM and a slot
# ---------------------------------------------------------------------------

function Get-FarmLeases([Parameter(Mandatory)] $Config) {
    $out = @()
    foreach ($f in @(Get-ChildItem -LiteralPath (Join-Path $Config.stateDir 'leases') -Filter '*.json' -ErrorAction SilentlyContinue)) {
        $l = Read-FarmJson $f.FullName
        $alive = $false
        if ($l) {
            try {
                $p = Get-Process -Id ([int] $l.pid) -ErrorAction Stop
                $alive = (ConvertTo-FarmStamp $p.StartTime.ToUniversalTime()) -eq $l.pidStart
            } catch {
                $alive = $false
            }
        }
        if ($alive) { $out += $l } else { Remove-Item -Force -LiteralPath $f.FullName -ErrorAction SilentlyContinue }
    }
    $out
}

function Remove-FarmLease([string] $Path) {
    if ($Path) { Remove-Item -Force -LiteralPath $Path -ErrorAction SilentlyContinue }
}

# Picks the VM and the slot (a checkout with its own target directory on the
# VM) for one job, and takes a lease on them. Prefers a running VM, the one
# holding this worktree's slot, the least loaded and the cheapest; starts a
# stopped VM when every running one is busy and the budget allows it.
# -Spread (shader surveys, which use every core for minutes): an idle running
# VM first, else a stopped one if the budget allows it, else the least loaded.
function Select-FarmTarget {
    param([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Worktree, [string[]] $Exclude = @(), [switch] $Spread)
    Use-FarmLock -Config $Config -Script {
        $inst = @(Get-FarmInstances -Config $Config)
        $leases = @(Get-FarmLeases -Config $Config)
        $slotsPath = Join-Path $Config.stateDir 'slots.json'
        $slots = New-Object System.Collections.ArrayList
        foreach ($s in @(Read-FarmJson $slotsPath @())) { if ($s) { [void] $slots.Add($s) } }
        $bad = Get-FarmUnhealthy $Config
        $prices = Get-FarmPrices $Config
        $key = $Worktree.ToLowerInvariant()
        $cands = @()
        $why = @()
        foreach ($vm in @($Config.vms)) {
            $i = $inst | Where-Object { $_.name -eq $vm.name } | Select-Object -First 1
            if (-not $i) { $why += "$($vm.name) does not exist"; continue }
            if ($Exclude -contains $vm.name) { $why += "$($vm.name) failed this job"; continue }
            if ($bad.ContainsKey($vm.name)) { $why += "$($vm.name): $($bad[$vm.name])"; continue }
            $cands += [pscustomobject]@{
                vm       = $vm.name
                zone     = $vm.zone
                status   = $i.status
                running  = (Test-FarmRunning $i) -or (Test-FarmStarting $i)
                starting = (Test-FarmStarting $i)
                stopped  = (Test-FarmStopped $i)
                load     = @($leases | Where-Object { $_.vm -eq $vm.name }).Count
                mine     = [bool] (@($slots) | Where-Object { $_.vm -eq $vm.name -and $_.worktree -eq $key })
                rate     = (Get-FarmRate $Config $prices $i.machineType $i.zone)
                id       = $i.id
            }
        }
        $order = @(@{ Expression = 'mine'; Descending = $true }, @{ Expression = 'load'; Ascending = $true }, @{ Expression = 'rate'; Ascending = $true })
        $maxLoad = [int] $Config.maxBuildsPerVm
        if ($Spread) {
            $order = @(@{ Expression = 'load'; Ascending = $true }, @{ Expression = 'mine'; Descending = $true }, @{ Expression = 'rate'; Ascending = $true })
            $maxLoad = 1
        }
        $running = @($cands | Where-Object { $_.running })
        $pick = $running | Where-Object { $_.load -lt $maxLoad } | Sort-Object -Property $order | Select-Object -First 1
        $start = $false
        $budget = $null
        if (-not $pick) {
            foreach ($c in @($cands | Where-Object { $_.stopped } | Sort-Object -Property @(@{ Expression = 'mine'; Descending = $true }, @{ Expression = 'rate'; Ascending = $true }))) {
                $budget = Test-FarmBudget -Config $Config -Instances $inst -StartVm $c.vm
                if ($budget.Ok) { $pick = $c; $start = $true; break }
            }
        }
        if (-not $pick -and $running) {
            $pick = $running | Sort-Object -Property $order | Select-Object -First 1
        }
        if (-not $pick) {
            $reason = $(if ($budget -and -not $budget.Ok) { "$($budget.Message): not starting another VM" }
                elseif ($cands) { 'no build VM can start now (' + (($cands | ForEach-Object { "$($_.vm) $($_.status)" }) -join ', ') + ')' }
                else { 'no build VM available (' + ($why -join '; ') + ')' })
            return [pscustomobject]@{ ok = $false; reason = $reason }
        }
        if (-not $start) {
            $spend = Get-FarmSpend -Config $Config -Instances $inst
            if ($spend.TotalUsd -ge $spend.CapUsd) {
                return [pscustomobject]@{ ok = $false; reason = ('daily budget reached: {0:N2} USD spent today of {1:N2}' -f $spend.TotalUsd, $spend.CapUsd) }
            }
        }
        # The slot: this worktree's own, else a new one, else the least
        # recently used free one (any checkout of a recent main builds
        # incrementally from another one's target directory).
        $busy = @($leases | Where-Object { $_.vm -eq $pick.vm } | ForEach-Object { $_.slot })
        $vmSlots = @($slots | Where-Object { $_.vm -eq $pick.vm })
        $slot = $vmSlots | Where-Object { $_.worktree -eq $key -and $busy -notcontains $_.slot } | Select-Object -First 1
        if (-not $slot -and $vmSlots.Count -lt [int] $Config.slotsPerVm) {
            $n = 1
            while (@($vmSlots | Where-Object { $_.slot -eq "s$n" }).Count) { $n++ }
            $slot = [pscustomobject]@{ vm = $pick.vm; slot = "s$n"; worktree = $key; used = '' }
            [void] $slots.Add($slot)
        }
        if (-not $slot) {
            $slot = $vmSlots | Where-Object { $busy -notcontains $_.slot } | Sort-Object used | Select-Object -First 1
        }
        if (-not $slot) { $slot = $vmSlots | Sort-Object used | Select-Object -First 1 }
        $slot.worktree = $key
        $slot.used = ConvertTo-FarmStamp (Get-Date).ToUniversalTime()
        Write-FarmJson $slotsPath @($slots)
        $me = Get-Process -Id $PID
        $lease = Join-Path $Config.stateDir "leases\$PID-$($pick.vm)-$($slot.slot).json"
        Write-FarmJson $lease ([pscustomobject]@{
                pid      = $PID
                pidStart = (ConvertTo-FarmStamp $me.StartTime.ToUniversalTime())
                vm       = $pick.vm
                slot     = $slot.slot
                worktree = $key
                started  = (ConvertTo-FarmStamp (Get-Date).ToUniversalTime())
            })
        [pscustomobject]@{
            ok       = $true
            vm       = $pick.vm
            zone     = $pick.zone
            slot     = $slot.slot
            start    = $start
            starting = $pick.starting
            id       = $pick.id
            rate     = $pick.rate
            lease    = $lease
        }
    }
}

# ---------------------------------------------------------------------------
# The worktree: snapshot, bundle, known commits
# ---------------------------------------------------------------------------

function Get-FarmWorktree([Parameter(Mandatory)] [string] $Dir) {
    $r = Invoke-FarmGit -Dir $Dir -Arguments @('rev-parse', '--show-toplevel', '--show-prefix', '--git-path', 'index')
    $lines = $r.Out -split "`r?`n"
    $top = [IO.Path]::GetFullPath($lines[0].Trim())
    $prefix = $lines[1].Trim().TrimEnd('/')
    $index = $lines[2].Trim()
    if (-not [IO.Path]::IsPathRooted($index)) { $index = Join-Path $Dir $index }
    [pscustomobject]@{ Top = $top; Prefix = $prefix; Index = [IO.Path]::GetFullPath($index); Name = Split-Path -Leaf $top }
}

# The commit to build: HEAD, or a temporary commit on top of it holding the
# uncommitted changes (tracked and untracked files git does not ignore), made
# from a copy of the index so the worktree's own index is never touched.
function New-FarmSnapshot([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] $Worktree) {
    $top = $Worktree.Top
    $head = (Invoke-FarmGit -Dir $top -Arguments @('rev-parse', 'HEAD')).Out.Trim()
    $headTree = (Invoke-FarmGit -Dir $top -Arguments @('rev-parse', 'HEAD^{tree}')).Out.Trim()
    $tmpIndex = Join-Path $Config.tempDir "index-$PID"
    try {
        if (Test-Path -LiteralPath $Worktree.Index) { Copy-Item -Force -LiteralPath $Worktree.Index -Destination $tmpIndex }
        $envs = @{ GIT_INDEX_FILE = $tmpIndex }
        Invoke-FarmGit -Dir $top -Arguments @('add', '-A') -Environment $envs | Out-Null
        $tree = (Invoke-FarmGit -Dir $top -Arguments @('write-tree') -Environment $envs).Out.Trim()
    } finally {
        Remove-Item -Force -LiteralPath $tmpIndex -ErrorAction SilentlyContinue
    }
    if ($tree -eq $headTree) {
        return [pscustomobject]@{ Target = $head; Head = $head; Dirty = $false; Changed = @() }
    }
    $who = @{
        GIT_AUTHOR_NAME = 'xtp-farm'; GIT_AUTHOR_EMAIL = 'farm@localhost'
        GIT_COMMITTER_NAME = 'xtp-farm'; GIT_COMMITTER_EMAIL = 'farm@localhost'
    }
    $target = (Invoke-FarmGit -Dir $top -Arguments @('commit-tree', $tree, '-p', $head, '-m', 'farm: uncommitted changes') -Environment $who).Out.Trim()
    $diff = (Invoke-FarmGit -Dir $top -Arguments @('diff-tree', '-r', '--no-renames', '--name-only', '--diff-filter=AM', '-z', $head, $target)).Out
    $changed = @($diff -split "`0" | Where-Object { $_ })
    # Source code only: refuse big files nobody committed.
    $limit = [double] $Config.maxSendFileMB * 1MB
    $big = @()
    foreach ($f in $changed) {
        $full = Join-Path $top $f
        if ((Test-Path -LiteralPath $full) -and (Get-Item -LiteralPath $full).Length -gt $limit) { $big += $f }
    }
    if ($big) {
        throw ("build farm: uncommitted files over {0} MB would be sent: {1} (commit or ignore them, or raise maxSendFileMB)" -f $Config.maxSendFileMB, ($big -join ', '))
    }
    [pscustomobject]@{ Target = $target; Head = $head; Dirty = $true; Changed = $changed }
}

function Get-FarmKnown([Parameter(Mandatory)] $Config, [string] $Vm, [string] $InstanceId) {
    $k = Read-FarmJson (Join-Path $Config.stateDir "vms\$Vm.json")
    if (-not $k -or $k.instanceId -ne $InstanceId) { return @() }
    @($k.known | Where-Object { $_ })
}

function Add-FarmKnown([Parameter(Mandatory)] $Config, [string] $Vm, [string] $InstanceId, [string[]] $Commits, [switch] $Reset) {
    Use-FarmLock -Config $Config -Name 'known' -Script {
        $old = $(if ($Reset) { @() } else { @(Get-FarmKnown $Config $Vm $InstanceId) })
        $new = @($Commits) + @($old | Where-Object { $Commits -notcontains $_ }) | Select-Object -First 40
        Write-FarmJson (Join-Path $Config.stateDir "vms\$Vm.json") ([pscustomobject]@{ instanceId = $InstanceId; known = @($new) })
    }
}

# A git bundle of the target and its history, minus what the VM is known to
# have. Returns Size 0 when the VM has it all.
function New-FarmBundle([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] $Worktree, [string] $Target, [string[]] $Known) {
    $top = $Worktree.Top
    if (@($Known) -contains $Target) { return [pscustomobject]@{ Path = ''; Size = 0; Ref = '' } }
    $have = @()
    foreach ($k in @($Known)) {
        $r = Invoke-FarmGit -Dir $top -Arguments @('cat-file', '-e', "$k^{commit}") -AllowFailure
        if ($r.ExitCode -eq 0) { $have += "^$k" }
    }
    $ref = "refs/worktree/farm/$PID"
    $path = Join-Path $Config.tempDir "farm-$PID.bundle"
    Invoke-FarmGit -Dir $top -Arguments @('update-ref', $ref, $Target) | Out-Null
    try {
        $r = Invoke-FarmGit -Dir $top -Arguments (@('bundle', 'create', '--quiet', $path, $ref) + $have) -AllowFailure
        if ($r.ExitCode -ne 0) {
            if ($r.Err -match 'empty bundle') { return [pscustomobject]@{ Path = ''; Size = 0; Ref = '' } }
            throw "git bundle create failed: $($r.Err.Trim())"
        }
    } finally {
        Invoke-FarmGit -Dir $top -Arguments @('update-ref', '-d', $ref) -AllowFailure | Out-Null
    }
    [pscustomobject]@{ Path = $path; Size = (Get-Item -LiteralPath $path).Length; Ref = $ref }
}

# Environment variables a build may depend on. Paths of this PC (the game
# folder, caches) never go to the VMs.
function Get-FarmForwardEnv {
    $allow = '^(RUSTFLAGS|RUSTDOCFLAGS|RUST_BACKTRACE|RUST_LIB_BACKTRACE|RUST_LOG|RUST_TEST_THREADS|RUST_TEST_NOCAPTURE|RUST_MIN_STACK|CARGO_INCREMENTAL|CARGO_TERM_COLOR|CARGO_TERM_VERBOSE|CARGO_PROFILE_.+|CARGO_BUILD_RUSTFLAGS|CARGO_ENCODED_RUSTFLAGS|XTP_.+)$'
    $deny = '^(XTP_FARM.*|XTP_SLOT_JOBS|XTP_GAME_DIR|XTP_CACHE_DIR|XTP_DATA_DIR|XTP_TEST_TITLES|XTP_CONFIG)$'
    $extra = @("$env:XTP_FARM_ENV" -split '[,; ]+' | Where-Object { $_ })
    $out = [ordered]@{}
    foreach ($e in @(Get-ChildItem Env: | Sort-Object Name)) {
        $n = $e.Name.ToUpperInvariant()
        if (($n -match $allow -or $extra -contains $e.Name) -and $n -notmatch $deny -and "$($e.Value)" -notmatch '^[A-Za-z]:[\\/]|^\\\\') {
            $out[$e.Name] = "$($e.Value)"
        }
    }
    $out
}

# Arguments naming files of the worktree point to the slot on the VM.
function ConvertTo-FarmCargoArgs([string[]] $CargoArgs, [string] $Top) {
    $pat = '(?i)' + [regex]::Escape($Top.TrimEnd('\')).Replace('\\', '[\\/]')
    @($CargoArgs | ForEach-Object { [regex]::Replace("$_", $pat, $script:SlotToken) })
}

# ---------------------------------------------------------------------------
# Jobs
# ---------------------------------------------------------------------------

function ConvertTo-FarmBase64($Object) {
    [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes((ConvertTo-Json -InputObject $Object -Depth 8 -Compress)))
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

# Copies files of a slot (paths relative to the worktree) into the worktree,
# only those that differ. With -Artifacts, the executables the slot's last
# build produced (and their .pdb unless XTP_FARM_FETCH_PDB=0).
function Receive-FarmFiles {
    param([Parameter(Mandatory)] $Config, [Parameter(Mandatory)] [string] $Vm, [Parameter(Mandatory)] [string] $Slot,
        [Parameter(Mandatory)] $Worktree, [string[]] $Paths = @(), [switch] $Artifacts)
    $q = [ordered]@{ mode = 'list'; slot = $Slot; paths = @($Paths); artifacts = [bool] $Artifacts; pdb = ($env:XTP_FARM_FETCH_PDB -ne '0') }
    $cmd = 'powershell -NoProfile -NonInteractive -OutputFormat Text -ExecutionPolicy Bypass -File C:\farm\bin\farm-fetch.ps1 ' + (ConvertTo-FarmBase64 $q)
    $r = Invoke-FarmSsh -Config $Config -Vm $Vm -Command $cmd -Capture
    if ($r.ExitCode -ne 0) { Write-FarmMessage "cannot list files to fetch on ${Vm}: $($r.Err.Trim())"; return $r.ExitCode }
    $items = @(ConvertFrom-FarmJsonArray $r.Out)
    $need = @()
    foreach ($it in $items) {
        if (-not $it -or -not ($it.path -is [string])) { throw "build farm: unexpected file list from ${Vm}: $($r.Out)" }
        $local = Join-Path $Worktree.Top $it.path
        if ((Test-Path -LiteralPath $local) -and (Get-Item -LiteralPath $local).Length -eq [int64] $it.size -and (Get-FarmFileHash $local) -eq $it.sha256) { continue }
        $need += $it
    }
    if (-not $need) { return 0 }
    $q.mode = 'tar'
    $q.paths = @($need | ForEach-Object { $_.path })
    $q.artifacts = $false
    $tgz = Join-Path $Config.tempDir "fetch-$PID.tgz"
    try {
        $cmd = 'powershell -NoProfile -NonInteractive -OutputFormat Text -ExecutionPolicy Bypass -File C:\farm\bin\farm-fetch.ps1 ' + (ConvertTo-FarmBase64 $q)
        $r = Invoke-FarmSsh -Config $Config -Vm $Vm -Command $cmd -StdoutFile $tgz -TimeoutSeconds 900
        if ($r.ExitCode -ne 0) { Write-FarmMessage "fetching from $Vm failed ($($r.ExitCode))"; return $r.ExitCode }
        $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
        $x = Invoke-FarmProcess -FilePath $tar -ArgumentList @('-xzf', $tgz, '-C', $Worktree.Top) -Capture -NoStdin
        if ($x.ExitCode -ne 0) { Write-FarmMessage "unpacking fetched files failed: $($x.Err.Trim())"; return $x.ExitCode }
    } finally {
        Remove-Item -Force -LiteralPath $tgz -ErrorAction SilentlyContinue
    }
    foreach ($it in $need) { Write-FarmMessage ('fetched {0} ({1:N1} MB)' -f $it.path, ([double] $it.size / 1MB)) }
    0
}

# Runs cargo commands (steps) of one worktree on the farm: picks a VM and a
# slot, starts the VM if needed, sends the commits it lacks and the
# uncommitted changes as one git bundle on the SSH connection that runs the
# commands, streams their output and returns cargo's exit code. Retries on
# another VM when one is reclaimed or unreachable. 75: the farm could not
# take the job.
# For shader surveys (Invoke-FarmSurvey.ps1): -Commit builds that commit
# instead of the worktree's snapshot; -SlotKey takes the slots of that key
# instead of the worktree's; -Spread prefers an idle VM; -StdoutFile and
# -StderrFile take the job's output; -Info (a hashtable) gets the VM and
# slot used.
function Invoke-FarmJob {
    param(
        [Parameter(Mandatory)] $Config,
        [string] $WorkDir = (Get-Location).Path,
        [object[]] $Steps = @(),
        [string[]] $Labels = @(),
        [switch] $CollectBinaries,
        [string[]] $Fetch = @(),
        [switch] $FetchOnly,
        [string] $Commit = '',
        [string] $SlotKey = '',
        [switch] $Spread,
        [string] $StdoutFile = '',
        [string] $StderrFile = '',
        [hashtable] $Info = @{}
    )
    $t0 = Get-Date
    $wt = Get-FarmWorktree $WorkDir
    $key = $(if ($SlotKey) { $SlotKey } else { $wt.Top })
    if (-not (Test-Path -LiteralPath (Get-FarmSshPaths $Config).Config) -or -not (Test-Path -LiteralPath (Get-FarmSshPaths $Config).Key)) {
        Initialize-FarmSsh $Config | Out-Null
    }
    try { Remove-FarmOrphans $Config | Out-Null } catch { Write-FarmLog $Config "orphan cleanup failed: $($_.Exception.Message)" }
    if ($FetchOnly) {
        $slots = @(Read-FarmJson (Join-Path $Config.stateDir 'slots.json') @())
        $s = $slots | Where-Object { $_.worktree -eq $wt.Top.ToLowerInvariant() } | Sort-Object used -Descending | Select-Object -First 1
        if (-not $s) { Write-FarmMessage "no farm build of $($wt.Name) yet"; return 1 }
        $inst = @(Get-FarmInstances -Config $Config) | Where-Object { $_.name -eq $s.vm } | Select-Object -First 1
        if (-not $inst -or -not (Test-FarmRunning $inst)) { Write-FarmMessage "$($s.vm), which holds $($wt.Name)'s last build, is $($inst.status): build again to fetch"; return 1 }
        return (Receive-FarmFiles -Config $Config -Vm $s.vm -Slot $s.slot -Worktree $wt -Paths $Fetch)
    }
    if ($Commit) {
        $snap = [pscustomobject]@{ Target = $Commit; Head = $Commit; Dirty = $false; Changed = @() }
    } else {
        $snap = New-FarmSnapshot $Config $wt
    }
    $agent = Get-FarmAgentVersion
    $envs = Get-FarmForwardEnv
    $stepList = @()
    for ($n = 0; $n -lt $Steps.Count; $n++) {
        $label = $(if ($Labels.Count -gt $n) { $Labels[$n] } else { '' })
        $stepList += [ordered]@{ args = @(ConvertTo-FarmCargoArgs @($Steps[$n]) $wt.Top); label = $label }
    }
    $what = $(if ($snap.Dirty) { "$($snap.Head.Substring(0, 8)) + uncommitted changes" } else { $snap.Head.Substring(0, 8) })
    # The job's time, all attempts included: the VM stops cargo at its own
    # deadline, a little before this client gives up on a VM that stopped
    # answering (then another VM, or the PC).
    $jobMinutes = [int] $Config.maxRunMinutes
    if ("$env:XTP_FARM_JOB_MINUTES" -match '^\d+$' -and [int] $env:XTP_FARM_JOB_MINUTES -gt 0) { $jobMinutes = [int] $env:XTP_FARM_JOB_MINUTES }
    $giveUp = $t0.AddMinutes($jobMinutes + 3)
    $exclude = @()
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        if ($attempt -gt 1 -and ($giveUp - (Get-Date)).TotalMinutes -lt 2) {
            Write-FarmMessage "unavailable: no farm VM could finish the job within $jobMinutes minutes"
            Write-FarmLog $Config "gave up on a job of $($wt.Name) after $jobMinutes minutes"
            return $script:ExitUnavailable
        }
        $sel = Select-FarmTarget -Config $Config -Worktree $key -Exclude $exclude -Spread:$Spread
        if (-not $sel.ok) {
            Write-FarmMessage "unavailable: $($sel.reason)"
            Write-FarmLog $Config "unavailable for $($wt.Name): $($sel.reason)"
            return $script:ExitUnavailable
        }
        $bundle = $null
        try {
            if ($sel.start) {
                Start-FarmVm $Config $sel.vm
                Wait-FarmVmReady $Config $sel.vm
                $sel.id = (@(Get-FarmInstances -Config $Config) | Where-Object { $_.name -eq $sel.vm } | Select-Object -First 1).id
            } elseif ($sel.starting) {
                Wait-FarmVmReady $Config $sel.vm
            }
            $known = @(Get-FarmKnown $Config $sel.vm $sel.id)
            $lost = $false
            $stuck = $false
            for ($try = 0; $try -lt 4; $try++) {
                $bundle = New-FarmBundle $Config $wt $snap.Target $known
                $req = [ordered]@{
                    v           = $agent
                    slot        = $sel.slot
                    target      = $snap.Target
                    ref         = $bundle.Ref
                    bundle      = $bundle.Size
                    skip        = $(if ($bundle.Size) { Get-FarmStdinSkip } else { 0 })
                    cwd         = $wt.Prefix
                    steps       = @($stepList)
                    env         = $envs
                    collect     = [bool] $CollectBinaries
                    maxMinutes  = [int] [Math]::Max(1, [Math]::Floor(($giveUp - (Get-Date)).TotalMinutes - 2))
                    waitMinutes = [int] [Math]::Max(1, [Math]::Min(10, [Math]::Floor(($giveUp - (Get-Date)).TotalMinutes - 3)))
                    worktree    = $wt.Name
                }
                Write-FarmMessage ('{0} slot {1}: {2}, sent {3:0} KB' -f $sel.vm, $sel.slot, $what, ([double] $bundle.Size / 1KB))
                $Info['vm'] = $sel.vm
                $Info['slot'] = $sel.slot
                $cmd = 'powershell -NoProfile -NonInteractive -OutputFormat Text -ExecutionPolicy Bypass -File C:\farm\bin\farm-run.ps1 ' + (ConvertTo-FarmBase64 $req)
                $tRun = Get-Date
                $run = Invoke-FarmSsh -Config $Config -Vm $sel.vm -Command $cmd -StdinFile $bundle.Path -StdoutFile $StdoutFile -StderrFile $StderrFile `
                    -TimeoutSeconds ([int] [Math]::Max(60, ($giveUp - (Get-Date)).TotalSeconds)) -StallSeconds 60
                $code = $run.ExitCode
                if ($bundle.Path) { Remove-Item -Force -LiteralPath $bundle.Path -ErrorAction SilentlyContinue }
                if ($run.TimedOut) {
                    $lost = $true
                    $stuck = $true
                    break
                }
                if ($code -eq 96) {
                    Write-FarmMessage "updating the farm agent on $($sel.vm)"
                    Publish-FarmAgent $Config $sel.vm | Out-Null
                    continue
                }
                if ($code -eq 97) {
                    if (-not $known) { throw "build farm: $($sel.vm) rejected a full bundle" }
                    $known = @()
                    continue
                }
                if ($code -eq 255 -or $code -eq 95) {
                    # 255: the connection dropped; 95: the VM is being reclaimed or stopped.
                    $lost = $true
                    break
                }
                if ($code -eq 98 -or $code -eq 99) {
                    # The VM could not prepare the job (its message says why).
                    $lost = $true
                    Set-FarmHealth $Config $sel.vm 5 "could not prepare a job (exit $code)"
                    break
                }
                if ($code -ne 0 -and $code -ne 124 -and -not (Test-FarmVmStaying $Config $sel.vm)) {
                    # Killed by a reclaim or a stop, not a failed build.
                    $code = 95
                    $lost = $true
                    break
                }
                Add-FarmKnown $Config $sel.vm $sel.id @($snap.Target, $snap.Head)
                $secs = ((Get-Date) - $t0).TotalSeconds
                Write-FarmLog $Config ('{0} {1}/{2} [{3}] exit {4} in {5:N0} s (run {6:N0} s, sent {7} B): {8}' -f `
                        $wt.Name, $sel.vm, $sel.slot, $what, $code, $secs, ((Get-Date) - $tRun).TotalSeconds, $bundle.Size,
                    (($stepList | ForEach-Object { 'cargo ' + ($_.args -join ' ') }) -join ' ; '))
                if ($code -eq 0 -and ($CollectBinaries -or $Fetch)) {
                    # The build is done: a failed fetch is reported, never a
                    # reason to build again elsewhere or on the PC.
                    try {
                        $f = @(Receive-FarmFiles -Config $Config -Vm $sel.vm -Slot $sel.slot -Worktree $wt -Paths $Fetch -Artifacts:$CollectBinaries)[-1]
                    } catch {
                        Write-FarmMessage "the build succeeded on $($sel.vm), but fetching its files failed: $($_.Exception.Message)"
                        Write-FarmLog $Config "fetch from $($sel.vm)/$($sel.slot) failed for $($wt.Name): $($_.Exception.Message)"
                        $f = 1
                    }
                    if ($f -ne 0) {
                        Write-FarmMessage "files not brought back; retry with: cargo-remote.ps1 --farm-fetch <path> (from $($sel.vm) slot $($sel.slot))"
                        $code = $f
                    }
                }
                Write-FarmMessage ('{0} finished in {1:N0} s, exit {2}' -f $sel.vm, ((Get-Date) - $t0).TotalSeconds, $code)
                return $code
            }
            if (-not $lost) { throw "build farm: $($sel.vm) kept refusing the job" }
            if ($stuck) {
                # No answer for too long: ssh and its IAP proxy were stopped.
                Write-FarmMessage "$($sel.vm) stopped answering (stuck connection): running the job elsewhere"
                Write-FarmLog $Config "job of $($wt.Name) on $($sel.vm) stopped: no answer in time"
                Set-FarmHealth $Config $sel.vm 5 'stuck connection'
                $exclude += $sel.vm
                continue
            }
            if ($code -eq 95) {
                Write-FarmMessage "$($sel.vm) is being reclaimed or stopped: running the job elsewhere"
                Write-FarmLog $Config "$($sel.vm) went away during a job of $($wt.Name)"
                Set-FarmHealth $Config $sel.vm 5 'reclaimed or stopped'
                $exclude += $sel.vm
                continue
            }
            if ($code -eq 98 -or $code -eq 99) {
                $exclude += $sel.vm
                continue
            }
            # The connection dropped. A VM that GCE still shows running may be
            # shutting down (a reclaim takes it down in about a minute): retry
            # it only if it answers at once.
            $now = @(Get-FarmInstances -Config $Config -Refresh) | Where-Object { $_.name -eq $sel.vm } | Select-Object -First 1
            $status = $(if ($now) { $now.status } else { 'gone' })
            $answers = $false
            if ($now -and (Test-FarmRunning $now) -and $attempt -lt 2) {
                try { Wait-FarmVmReady $Config $sel.vm -TimeoutSeconds 20; $answers = $true } catch { $answers = $false }
            }
            Write-FarmMessage "lost the connection to $($sel.vm) ($status$(if (-not $answers) { ', not answering' })): retrying"
            Write-FarmLog $Config "lost $($sel.vm) ($status, answers: $answers) during a job of $($wt.Name)"
            if (-not $answers) {
                Set-FarmHealth $Config $sel.vm 5 "connection lost ($status)"
                $exclude += $sel.vm
            }
        } catch {
            Write-FarmMessage "$($_.Exception.Message)"
            Write-FarmLog $Config "error with $($sel.vm) for $($wt.Name): $($_.Exception.Message)"
            $exclude += $sel.vm
        } finally {
            Remove-FarmLease $sel.lease
            if ($bundle -and $bundle.Path) { Remove-Item -Force -LiteralPath $bundle.Path -ErrorAction SilentlyContinue }
        }
    }
    Write-FarmMessage 'unavailable: no VM could run the job'
    $script:ExitUnavailable
}

