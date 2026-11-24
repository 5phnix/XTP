<#
.SYNOPSIS
  Takes the first GPU free anywhere: tries every zone that offers the
  accelerator, in a loop and in parallel, standard and Spot, then smaller
  machine types, until one VM runs. Logs every attempt.

.DESCRIPTION
  For when the VM's zone has had no GPU free for a while (its disks are
  snapshotted: move-vm.ps1 did it, or gcloud compute snapshots create).
  Each round:
    1. if <vm>-old exists (a move under way), tries to start it in its zone;
    2. tries to create <vm> with -MachineTypes[0], standard then Spot, in
       every region that offers the accelerator (a regional bulk insert takes
       the first zone of the region with a GPU free), one attempt at a time:
       an attempt in flight holds the GPU quota, so parallel ones would only
       be refused by the quota;
    3. if none got a GPU, the same with the other machine types;
  then waits -PauseSeconds and starts again, until -Minutes have passed.
  An attempt boots the public Windows image (fast to create and to refuse),
  so the GPU is held within seconds of being free. The GPU quota (one GPU)
  lets at most one attempt win.

  Once a VM runs, the game disk <vm>-data is made from -DataSnapshot in its
  zone (pd-balanced when the region's SSD_TOTAL_GB allows it, otherwise
  hyperdisk-ml) and attached. Then: new-vm.ps1 (metadata, schedule),
  setup-vm.ps1 (the new boot disk is a fresh Windows: driver, tools,
  session), and move-vm.ps1 -DeleteOld once it is checked.

.EXAMPLE
  tools\cloud\grab-vm.ps1 -Minutes 240
#>
[CmdletBinding()]
param(
    [string]$Config,
    [int]$Minutes = 240,
    [string[]]$MachineTypes = @('g2-standard-16', 'g2-standard-12', 'g2-standard-8'),
    [switch]$NoSpot,
    [int]$PauseSeconds = 45,
    [string]$DataSnapshot = '',
    [string]$Log = ''
)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$base = Get-XtpBaseArgList $cfg
if (-not $DataSnapshot) { $DataSnapshot = "$($cfg.vm)-data-move" }
if (-not $Log) { $Log = Join-Path $env:LOCALAPPDATA 'xtp\cloud-grab.log' }
$work = Join-Path ([System.IO.Path]::GetTempPath()) "xtp-grab-$PID"
New-Item -ItemType Directory -Force -Path $work | Out-Null
$gcloud = (Get-Command gcloud.cmd -ErrorAction SilentlyContinue).Source
if (-not $gcloud) { $gcloud = (Get-Command gcloud).Source }
# Many gcloud processes at once: below-normal priority keeps the PC usable.
try { (Get-Process -Id $PID).PriorityClass = 'BelowNormal' } catch { Write-Verbose "priority unchanged: $_" }

function Write-Attempt([string]$Text) {
    $line = "{0:yyyy-MM-dd HH:mm:ss} {1}" -f (Get-Date), $Text
    Add-Content -LiteralPath $Log -Value $line
    Write-Host $line
}

if (Get-XtpVmZone $cfg) { throw "$($cfg.vm) exists already; nothing to grab." }
$null = Invoke-Gcloud -Arguments (@('compute', 'snapshots', 'describe', $DataSnapshot, '--format=value(status)') + $base)
$zones = @(Invoke-Gcloud -Arguments (@('compute', 'accelerator-types', 'list', "--filter=name~^$([regex]::Escape($cfg.accelerator))", '--format=value(name,zone.basename())') + $base) |
        Where-Object { $_ } | ForEach-Object { $n, $z = $_ -split '\s+'; if ($n -eq $cfg.accelerator) { $z } } | Sort-Object -Unique)
$oldName = "$($cfg.vm)-old"
$oldZone = @(Get-XtpInstanceZone $cfg $oldName) | Select-Object -First 1
# The old VM's zone keeps its disk names: a new VM there would collide; the
# old VM itself is tried instead.
if ($oldZone) { $zones = @($zones | Where-Object { $_ -ne $oldZone }) }
Write-Attempt "start: $($zones.Count) zones, types $($MachineTypes -join ','), spot $(-not $NoSpot), old VM $(if ($oldZone) { "$oldName in $oldZone" } else { 'none' })"

$metadata = "enable-windows-ssh=TRUE,block-project-ssh-keys=TRUE,xtp-idle-minutes=$($cfg.idleMinutes),sysprep-specialize-script-cmd=googet -noconfirm=true install google-compute-engine-ssh"
$regions = @($zones | ForEach-Object { $_ -replace '-[a-z]$', '' } | Sort-Object -Unique)

# One attempt: a regional bulk insert of one VM, which takes the first zone
# of the region with a GPU free. One at a time: an attempt in flight holds the
# GPU quota (one GPU), so parallel attempts only get quota errors.
function Invoke-Attempt([string]$Region, [string]$Type, [bool]$Spot) {
    $what = "$Type $(if ($Spot) { 'spot' } else { 'standard' }) in $Region"
    $a = @('compute', 'instances', 'bulk', 'create', "--predefined-names=$($cfg.vm)", '--count=1', "--region=$Region",
        "--machine-type=$Type", "--accelerator=type=$($cfg.accelerator),count=$($cfg.acceleratorCount)", '--maintenance-policy=TERMINATE',
        "--image-project=$($cfg.imageProject)", "--image-family=$($cfg.imageFamily)", "--boot-disk-size=$($cfg.bootDiskGb)GB",
        "--boot-disk-type=$($cfg.diskType)", "--boot-disk-device-name=$($cfg.vm)",
        "--service-account=$($cfg.serviceAccountEmail)", '--scopes=cloud-platform', "--metadata=$metadata",
        '--labels=app=xtp', '--tags=xtp')
    if ($Spot) { $a += @('--provisioning-model=SPOT', '--instance-termination-action=STOP') }
    if ($oldZone -and $oldZone -like "$Region-*") { $a += "--location-policy=$oldZone=deny" }
    $t0 = Get-Date
    $null = Invoke-Gcloud -Arguments ($a + $base) -AllowFail -Quiet
    $text = "$global:XtpGcloudError"
    $zone = @(Get-XtpInstanceZone $cfg $cfg.vm) | Select-Object -First 1
    $secs = ((Get-Date) - $t0).TotalSeconds
    if ($zone) {
        Write-Attempt ("{0}: CREATED in {1} ({2:N0} s)" -f $what, $zone, $secs)
        return [pscustomobject]@{ Zone = $zone; Type = $Type; Spot = $Spot }
    }
    $why = if ($text -match 'RESOURCE_POOL_EXHAUSTED|STOCKOUT|does not have enough resources|resource_availability') { 'no capacity' }
    elseif ($text -match "Quota '([A-Z_0-9]+)'") { "quota $($Matches[1])" }
    elseif ($text -match '[Qq]uota') { 'quota' }
    else { (($text -split "`n" | Where-Object { $_ -match 'ERROR|message|failed' } | Select-Object -First 1) -replace '\s+', ' ').Trim() }
    Write-Attempt ("{0}: {1} ({2:N0} s)" -f $what, $why, $secs)
    return $null
}

$deadline = (Get-Date).AddMinutes($Minutes)
$won = $null
$round = 0
while (-not $won -and (Get-Date) -lt $deadline) {
    $round++
    if ($oldZone) {
        $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'start', $oldName, "--zone=$oldZone", '--async') + $base) -AllowFail -Quiet
        $oldCfg = [pscustomobject]@{ vm = $oldName; project = $cfg.project }
        # Wait for the start to end one way or the other (it holds the GPU quota meanwhile).
        for ($i = 0; $i -lt 36; $i++) {
            Start-Sleep -Seconds 5
            if ((Get-XtpVmStatus $oldCfg $oldZone) -in 'RUNNING', 'TERMINATED', 'STOPPED') { break }
        }
        if ((Get-XtpVmStatus $oldCfg $oldZone) -eq 'RUNNING') {
            Write-Attempt "round ${round}: start $oldName in ${oldZone}: STARTED"
            $won = [pscustomobject]@{ Zone = $oldZone; Old = $true }
            break
        }
        Write-Attempt "round ${round}: start $oldName in ${oldZone}: not running"
    }
    foreach ($type in $MachineTypes) {
        foreach ($r in $regions) {
            $won = Invoke-Attempt $r $type $false
            if ($won) { break }
            if (-not $NoSpot) { $won = Invoke-Attempt $r $type $true; if ($won) { break } }
        }
        if ($won) { break }
    }
    if (-not $won) { Start-Sleep -Seconds $PauseSeconds }
}
Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
if (-not $won) { Write-Attempt "gave up after $Minutes min ($round rounds)"; exit 1 }
if ($won.Old) { Write-Attempt "the old VM $oldName runs again in $($won.Zone), with all its disks."; exit 0 }

# Another attempt may have won too, in the instant before the quota saw the first.
$extra = @(Get-XtpInstanceZone $cfg $cfg.vm | Where-Object { $_ -ne $won.Zone })
foreach ($z in $extra) {
    Write-Attempt "deleting a second VM that also won, in $z"
    $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'delete', $cfg.vm, "--zone=$z") + $base) -AllowFail
}

# The game disk, from its snapshot, in the winning zone.
$region = $won.Zone -replace '-[a-z]$', ''
$type = $cfg.dataDiskType
if ($type -notlike 'hyperdisk*') {
    $q = ((Invoke-Gcloud -Arguments (@('compute', 'regions', 'describe', $region, '--format=json') + $base)) -join "`n" | ConvertFrom-Json).quotas |
        Where-Object { $_.metric -eq 'SSD_TOTAL_GB' }
    if ($q -and ($q.limit - $q.usage) -lt $cfg.dataDiskGb) { $type = 'hyperdisk-ml' }
}
Write-Attempt "data disk $($cfg.dataDisk) ($type) from $DataSnapshot in $($won.Zone)"
$disk = @('compute', 'disks', 'create', $cfg.dataDisk, "--zone=$($won.Zone)", "--source-snapshot=$DataSnapshot", "--size=$($cfg.dataDiskGb)GB", "--type=$type", '--labels=app=xtp')
if ($type -eq 'hyperdisk-ml') { $disk += "--provisioned-throughput=$($cfg.dataDiskThroughput)" }
$null = Invoke-Gcloud -Arguments ($disk + $base)
$null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'attach-disk', $cfg.vm, "--zone=$($won.Zone)", "--disk=$($cfg.dataDisk)", "--device-name=$($cfg.dataDisk)") + $base)
Write-Attempt "READY: $($cfg.vm) $($won.Type) $(if ($won.Spot) { 'spot' } else { 'standard' }) in $($won.Zone), game disk attached. Next: new-vm.ps1, setup-vm.ps1."
exit 0
