# Copies the shader pack (shaders.rpf, and nothing else) from the game bucket
# to the farm VMs, for shader surveys (Invoke-FarmSurvey.ps1). The copy runs
# inside Google's network: each VM downloads the one object itself, through
# a signed URL valid for 15 minutes; the PC only sends the command. A VM that
# already holds that object's generation (size and CRC32C checked) is left
# alone.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Copy-FarmPack.ps1 [-Name <vm>,...] [-Force]
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Copy-FarmPack.ps1 -Status
#
# The VMs keep no service account. The URL is signed by a service account of
# its own (packSigner, default <prefix>-pack), created here if missing, which
# can read that one object and nothing else (an IAM condition on the bucket
# binding); the gcloud account running this script gets the Token Creator
# role on that service account only, to sign with it.
#
# The source is packSource in the farm configuration, else
# gs://<bucket>/<gamePrefix>/shaders.rpf from the cloud configuration
# (tools/cloud, XTP_CLOUD_CONFIG). Stopped VMs are started if the daily
# budget allows it; -Status only reports (and starts nothing).
#
# Windows PowerShell 5.1 and PowerShell 7. ASCII only.

param([string[]] $Name = @(), [switch] $Status, [switch] $Force)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$c = Get-FarmConfig
Initialize-FarmSsh $c | Out-Null

# --- The source: one object named shaders.rpf --------------------------------
$src = $c.packSource
if (-not $src) {
    $cloudPath = $(if ($env:XTP_CLOUD_CONFIG) { $env:XTP_CLOUD_CONFIG } else { Join-Path $env:LOCALAPPDATA 'xtp\cloud.json' })
    if (-not (Test-Path -LiteralPath $cloudPath)) { throw "no packSource in $($c.configPath) and no cloud configuration at $cloudPath" }
    $cloud = [IO.File]::ReadAllText($cloudPath) | ConvertFrom-Json
    $prefix = $(if ($cloud.gamePrefix) { "$($cloud.gamePrefix)".Trim('/') } else { 'game' })
    $src = "gs://$($cloud.bucket)/$prefix/shaders.rpf"
}
if ($src -notmatch '^gs://([a-z0-9][a-z0-9._-]+)/((?:[^/]+/)*shaders\.rpf)$') { throw "packSource must be one gs:// object named shaders.rpf, not $src" }
$bucket = $Matches[1]
$object = $Matches[2]

# -Name a,b arrives as one string through powershell -File.
$Name = @($Name | ForEach-Object { "$_" -split '[,\s]+' } | Where-Object { $_ })
$vms = $(if ($Name) { $Name } else { @($c.vms | ForEach-Object { $_.name }) })
$inst = @(Get-FarmInstances -Config $c -Refresh)

function Invoke-PackScript([string] $Vm, $Request, [int] $TimeoutSeconds = 180) {
    $text = "`$Request = '$(ConvertTo-FarmBase64 $Request)'`n" + [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'pack\vm-pack.ps1'))
    $tmp = Join-Path $c.tempDir "pack-$PID-$Vm.ps1"
    [IO.File]::WriteAllText($tmp, $text, (New-Object Text.ASCIIEncoding))
    # The VM reads exactly the script's bytes (after the stdin preamble, see
    # Get-FarmStdinSkip): the end of stdin does not always reach it.
    $boot = @(
        '$ErrorActionPreference = ''Stop'''
        "`$skip = $(Get-FarmStdinSkip); `$size = $((Get-Item -LiteralPath $tmp).Length)"
        '$in = [Console]::OpenStandardInput(); $b = New-Object byte[] ($skip + $size); $got = 0'
        'while ($got -lt $b.Length) { $n = $in.Read($b, $got, $b.Length - $got); if ($n -le 0) { break }; $got += $n }'
        'if ($got -lt $b.Length) { [Console]::Error.WriteLine("farm: script incomplete ($got of $($b.Length) bytes)"); exit 1 }'
        '$t = [Text.Encoding]::ASCII.GetString($b, $skip, $size)'
        'try { & ([scriptblock]::Create($t)) } catch { [Console]::Error.WriteLine("farm: $($_.Exception.Message)"); exit 1 }'
    ) -join "`n"
    $cmd = 'powershell -NoProfile -NonInteractive -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand ' + [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($boot))
    try {
        $r = Invoke-FarmSsh -Config $c -Vm $Vm -Command $cmd -StdinFile $tmp -Capture -TimeoutSeconds $TimeoutSeconds
    } finally {
        Remove-Item -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
    }
    if ($r.ExitCode -ne 0) { throw "$Vm ($($r.ExitCode)): $($r.Err.Trim())" }
    $r.Out | ConvertFrom-Json
}

function Write-PackState($s) {
    $have = $(if ($s.size -ge 0 -and $s.stamp) { '{0:N0} bytes, generation {1}, CRC32C {2}' -f $s.size, $s.stamp.generation, $s.stamp.crc32c } else { 'no pack' })
    Write-FarmMessage ("{0}: {1}{2}; {3} GB free" -f $s.vm, $have, $(if ($s.copied) { " (copied in $($s.seconds) s)" } else { '' }), $s.freeGb)
    $newest = @($s.dxil) | Select-Object -Last 1
    if ($newest) {
        Write-FarmMessage ("{0}: dxil.dll {1} ({2} found; the newest is the one xtp loads: {3})" -f $s.vm, $newest.fileVersion, @($s.dxil).Count, $newest.path)
    } else {
        Write-FarmMessage "$($s.vm): no dxil.dll in the Windows SDK: --validate cannot work there"
    }
}

if ($Status) {
    $failed = 0
    foreach ($vm in $vms) {
        $i = $inst | Where-Object { $_.name -eq $vm } | Select-Object -First 1
        if (-not $i -or -not (Test-FarmRunning $i)) { Write-FarmMessage "${vm}: $(if ($i) { $i.status } else { 'does not exist' })"; continue }
        try { Write-PackState (Invoke-PackScript $vm @{ mode = 'status' }) } catch { Write-FarmMessage "$($_.Exception.Message)"; $failed++ }
    }
    exit [int] [bool] $failed
}

# --- The object and the signer ------------------------------------------------
$desc = (Invoke-FarmGcloud -Config $c -Json -Arguments @('storage', 'objects', 'describe', $src)).Data
$size = [int64] $desc.size
$crc = "$($desc.crc32c_hash)"
$gen = "$($desc.generation)"
if (-not $crc -or -not $gen) { throw "$src has no CRC32C or generation: $($desc | ConvertTo-Json -Compress)" }
Write-FarmMessage ('{0}: {1:N0} bytes, generation {2}, CRC32C {3}' -f $src, $size, $gen, $crc)

$signer = "$($c.packSigner)@$($c.project).iam.gserviceaccount.com"
$account = (Invoke-FarmGcloud -Arguments @('config', 'get-value', 'account')).Out.Trim()
if (-not $account) { throw 'no active gcloud account (gcloud auth login)' }
$me = $(if ($account -match '\.gserviceaccount\.com$') { "serviceAccount:$account" } else { "user:$account" })

$on = @((Invoke-FarmGcloud -Config $c -Json -Arguments @('services', 'list', '--enabled', '--filter=config.name=iamcredentials.googleapis.com')).Data | ForEach-Object { $_.config.name })
if ($on -notcontains 'iamcredentials.googleapis.com') {
    Write-FarmMessage 'enabling iamcredentials.googleapis.com (URL signing)'
    Invoke-FarmGcloud -Config $c -Arguments @('services', 'enable', 'iamcredentials.googleapis.com') | Out-Null
}
$sa = Invoke-FarmGcloud -Config $c -AllowFailure -Arguments @('iam', 'service-accounts', 'describe', $signer, '--format=value(email)')
if ($sa.ExitCode -ne 0) {
    Write-FarmMessage "creating service account $signer (signs the pack's download URL; no project role)"
    Invoke-FarmGcloud -Config $c -Arguments @('iam', 'service-accounts', 'create', $c.packSigner, '--display-name=XTP build farm: reads shaders.rpf only (signed URLs)') | Out-Null
}
# A new service account takes a little while to be known everywhere.
function Invoke-IamChange([string[]] $Arguments) {
    $deadline = (Get-Date).AddMinutes(3)
    while ($true) {
        $r = Invoke-FarmGcloud -Config $c -AllowFailure -Arguments $Arguments
        if ($r.ExitCode -eq 0) { return }
        if ((Get-Date) -gt $deadline -or $r.Err -notmatch 'does not exist|not found') { throw "gcloud $($Arguments -join ' ') failed ($($r.ExitCode)): $($r.Err.Trim())" }
        Start-Sleep -Seconds 10
    }
}
$expr = "resource.name ==`"projects/_/buckets/$bucket/objects/$object`""
$policy = (Invoke-FarmGcloud -Config $c -Json -Arguments @('storage', 'buckets', 'get-iam-policy', "gs://$bucket")).Data
$has = @($policy.bindings) | Where-Object {
    $_.role -eq 'roles/storage.objectViewer' -and @($_.members) -contains "serviceAccount:$signer" -and $_.condition -and $_.condition.expression -eq $expr
}
if (-not $has) {
    Write-FarmMessage "granting $signer read on $object only (conditional roles/storage.objectViewer on gs://$bucket)"
    Invoke-IamChange @('storage', 'buckets', 'add-iam-policy-binding', "gs://$bucket", "--member=serviceAccount:$signer",
        '--role=roles/storage.objectViewer', "--condition=expression=$expr,title=xtp-farm-pack-only,description=shaders.rpf only for farm shader surveys")
}
$saPolicy = (Invoke-FarmGcloud -Config $c -Json -Arguments @('iam', 'service-accounts', 'get-iam-policy', $signer)).Data
$canSign = @($saPolicy.bindings) | Where-Object { $_.role -eq 'roles/iam.serviceAccountTokenCreator' -and @($_.members) -contains $me }
if (-not $canSign) {
    Write-FarmMessage "granting $account Token Creator on $signer only (to sign the URL)"
    Invoke-IamChange @('iam', 'service-accounts', 'add-iam-policy-binding', $signer, "--member=$me", '--role=roles/iam.serviceAccountTokenCreator')
}

# A URL for each copy, so that none outlives its copy by much. New IAM
# bindings take a minute or more to apply: retry for up to 5 minutes.
# The signer cannot read the bucket's metadata: its region comes from here.
$region = (Invoke-FarmGcloud -Config $c -Arguments @('storage', 'buckets', 'describe', "gs://$bucket", '--format=value(location)')).Out.Trim().ToLowerInvariant()
function New-PackUrl {
    $deadline = (Get-Date).AddMinutes(5)
    while ($true) {
        $r = Invoke-FarmGcloud -Config $c -Json -AllowFailure -Arguments @('storage', 'sign-url', $src, '--duration=15m', "--region=$region", "--impersonate-service-account=$signer")
        $u = $(if ($r.ExitCode -eq 0) { "$(@($r.Data)[0].signed_url)" } else { '' })
        if ($u -match '^https://') { return $u }
        if ((Get-Date) -gt $deadline -or $r.Err -notmatch 'PERMISSION_DENIED|getAccessToken|signBlob') { throw "cannot sign a URL for $src as ${signer}: $($r.Err.Trim())" }
        Write-FarmMessage 'signing refused (IAM changes still applying?): retrying in 20 s'
        Start-Sleep -Seconds 20
    }
}

# --- Each VM --------------------------------------------------------------------
$failed = 0
foreach ($vm in $vms) {
    try {
        $i = $inst | Where-Object { $_.name -eq $vm } | Select-Object -First 1
        if (-not $i) { throw "$vm does not exist (New-FarmVm.ps1)" }
        if (Test-FarmStopped $i) {
            $b = Test-FarmBudget -Config $c -Instances $inst -StartVm $vm
            if (-not $b.Ok) { throw "not starting ${vm}: $($b.Message)" }
            Start-FarmVm $c $vm
        }
        Wait-FarmVmReady $c $vm
        $s = Invoke-PackScript $vm @{ mode = 'status' }
        if (-not $Force -and $s.size -eq $size -and $s.stamp -and $s.stamp.crc32c -eq $crc -and "$($s.stamp.generation)" -eq $gen) {
            Write-PackState $s
            continue
        }
        if ($Force -and $s.stamp) {
            # A forced copy downloads again even when the stamp matches.
            $null = Invoke-FarmSsh -Config $c -Vm $vm -Command 'del /q C:\farm\pack\shaders.rpf.json' -Capture
        }
        Write-FarmMessage "${vm}: downloading $src"
        $s = Invoke-PackScript $vm ([ordered]@{ mode = 'copy'; url = (New-PackUrl); source = $src; size = $size; crc32c = $crc; generation = $gen }) -TimeoutSeconds 1500
        Write-PackState $s
        Write-FarmLog $c "pack $src ($gen) on $vm"
    } catch {
        Write-FarmMessage "$($_.Exception.Message)"
        $failed++
    }
}
if ($failed) { exit 1 }
