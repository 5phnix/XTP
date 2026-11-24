<#
.SYNOPSIS
  Moves the VM, with its disks as they are (Windows, tools, game files), to
  another zone: when its zone has had no GPU free for too long. Then, once
  the new VM is checked, -DeleteOld removes the old one.

.DESCRIPTION
  A stopped VM can only start in its own zone. This script:
  1. stops the VM, snapshots its boot and data disks (<vm>-boot-move,
     <vm>-data-move; existing snapshots are reused unless -NewSnapshots);
  2. renames the old VM <vm>-old (it stays, stopped, with its disks);
  3. creates <vm> in the first of -Zones with a GPU free, its boot disk made
     from the snapshot (the zone is held from that moment);
  4. makes the data disk from its snapshot in that zone and attaches it:
     dataDiskType, or hyperdisk-ml when the region's SSD_TOTAL_GB quota
     cannot hold both disks as pd-balanced (Hyperdisk ML has its own quota,
     HDML_TOTAL_GB, and bills its throughput, dataDiskThroughput MiB/s);
  5. runs new-vm.ps1 (metadata, firewall check, the region's daily stop).
  Nothing is asked of Google: the GPU and CPU quotas are the same in every
  region, and the old VM, stopped, uses none.

  Check the new VM (run-remote.ps1 -DeviceCheck, a game run), then:
    tools\cloud\move-vm.ps1 -DeleteOld
  deletes <vm>-old with its disks, the old region's schedule if nothing uses
  it, and the move snapshots.

.EXAMPLE
  tools\cloud\move-vm.ps1 -Zones europe-west4-a,europe-west4-b,us-east4-a
  tools\cloud\move-vm.ps1 -DeleteOld
#>
[CmdletBinding()]
param(
    [string]$Config,
    [string[]]$Zones = @(),
    [switch]$NewSnapshots,
    [switch]$DeleteOld,
    [int]$RetryMinutes = 0
)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$base = Get-XtpBaseArgList $cfg
$old = "$($cfg.vm)-old"
$bootSnap = "$($cfg.vm)-boot-move"
$dataSnap = "$($cfg.vm)-data-move"

function Get-Zone([string]$Name) {
    $z = @(Invoke-Gcloud -Arguments (@('compute', 'instances', 'list', "--filter=name=$Name", '--format=value(zone.basename())') + $base) | Where-Object { $_ })
    if ($z.Count) { return $z[0] } else { return $null }
}

if ($DeleteOld) {
    $oz = Get-Zone $old
    if (-not $oz) { Write-Host "No $old."; } else {
        Write-Step "Deleting $old in $oz with its disks"
        $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'delete', $old, "--zone=$oz", '--delete-disks=all') + $base)
        $oregion = $oz -replace '-[a-z]$', ''
        $policy = "$($cfg.vm)-daily-stop"
        $users = Invoke-Gcloud -Arguments (@('compute', 'instances', 'list', "--filter=resourcePolicies~$policy AND zone~$oregion", '--format=value(name)') + $base)
        if (-not ($users | Where-Object { $_ })) {
            $null = Invoke-Gcloud -Arguments (@('compute', 'resource-policies', 'delete', $policy, "--region=$oregion") + $base) -AllowFail
        }
    }
    foreach ($s in $bootSnap, $dataSnap) {
        $null = Invoke-Gcloud -Arguments (@('compute', 'snapshots', 'delete', $s) + $base) -AllowFail -Quiet
        if ($LASTEXITCODE -eq 0) { Write-Host "    deleted snapshot $s" }
    }
    return
}

if (-not $Zones.Count) { throw 'Pass -Zones: the zones to try, in order (another region than the VM''s own is fine).' }
$zone = Get-XtpVmZone $cfg
if (-not $zone) { throw "No VM named $($cfg.vm)." }
if (Get-Zone $old) { throw "$old exists already: finish or undo the previous move first (move-vm.ps1 -DeleteOld)." }

# --- 1. Stop and snapshot ------------------------------------------------------
if ((Get-XtpVmStatus $cfg $zone) -ne 'TERMINATED') {
    Write-Step "Stopping $($cfg.vm)"
    $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'stop', $cfg.vm, "--zone=$zone") + $base)
}
foreach ($pair in @(@($bootSnap, $cfg.vm), @($dataSnap, $cfg.dataDisk))) {
    $snap, $disk = $pair
    $have = Invoke-Gcloud -Arguments (@('compute', 'snapshots', 'describe', $snap, '--format=value(status)') + $base) -AllowFail -Quiet
    if ($LASTEXITCODE -eq 0 -and $NewSnapshots) {
        $null = Invoke-Gcloud -Arguments (@('compute', 'snapshots', 'delete', $snap) + $base)
        $have = $null
    }
    if (-not $have) {
        Write-Step "Snapshot $snap of $disk"
        $null = Invoke-Gcloud -Arguments (@('compute', 'snapshots', 'create', $snap, "--source-disk=$disk", "--source-disk-zone=$zone", '--labels=app=xtp', '--async') + $base)
    }
}
foreach ($snap in $bootSnap, $dataSnap) {
    while ((Invoke-Gcloud -Arguments (@('compute', 'snapshots', 'describe', $snap, '--format=value(status)') + $base)) -ne 'READY') { Start-Sleep -Seconds 15 }
    Write-Host "    $snap ready"
}

# --- 2. The old VM steps aside ---------------------------------------------------
Write-Step "Renaming $($cfg.vm) to $old (kept, stopped, until -DeleteOld)"
$null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'set-name', $cfg.vm, "--zone=$zone", "--new-name=$old") + $base)

# --- 3. The new VM, from the boot snapshot -----------------------------------------
$create = @('compute', 'instances', 'create', $cfg.vm, "--machine-type=$($cfg.machineType)",
    "--create-disk=name=$($cfg.vm),device-name=$($cfg.vm),boot=yes,auto-delete=yes,source-snapshot=$bootSnap,size=$($cfg.bootDiskGb)GB,type=$($cfg.diskType)",
    "--service-account=$($cfg.serviceAccountEmail)", '--scopes=cloud-platform',
    "--metadata=enable-windows-ssh=TRUE,block-project-ssh-keys=TRUE,xtp-idle-minutes=$($cfg.idleMinutes)",
    '--labels=app=xtp', '--tags=xtp', '--shielded-vtpm', '--shielded-integrity-monitoring')
if ($cfg.accelerator) { $create += @("--accelerator=type=$($cfg.accelerator),count=$($cfg.acceleratorCount)", '--maintenance-policy=TERMINATE') }
$deadline = (Get-Date).AddMinutes($RetryMinutes)
$newZone = $null
while (-not $newZone) {
    foreach ($z in $Zones) {
        Write-Host ("    {0:HH:mm:ss} {1} ..." -f (Get-Date), $z) -NoNewline
        $null = Invoke-Gcloud -Arguments ($create + @("--zone=$z") + $base) -AllowFail -Quiet
        if ($LASTEXITCODE -eq 0) { Write-Host ' created'; $newZone = $z; break }
        $err = $global:XtpGcloudError
        if ($err -match 'RESOURCE_POOL_EXHAUSTED|STOCKOUT|does not have enough resources') { Write-Host ' no capacity'; continue }
        if ($err -match 'QUOTA|[Qq]uota') { Write-Host ' quota'; Write-Host "      $(($err -split "`n" | Where-Object { $_ -match '[Qq]uota' } | Select-Object -First 1))"; continue }
        Write-Host ' failed'
        throw "Creating the VM in $z failed (the old VM is $old in $zone; rename it back with gcloud compute instances set-name):`n$err"
    }
    if ($newZone) { break }
    if ((Get-Date) -ge $deadline) {
        $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'set-name', $old, "--zone=$zone", "--new-name=$($cfg.vm)") + $base)
        throw "No zone of $($Zones -join ', ') could take the VM now; the old VM has its name back. Try later, or other zones (-RetryMinutes)."
    }
    Start-Sleep -Seconds 120
}
$region = $newZone -replace '-[a-z]$', ''

# --- 4. The data disk, from its snapshot -------------------------------------------------
$type = $cfg.dataDiskType
if ($type -notlike 'hyperdisk*') {
    $ssd = (Invoke-Gcloud -Arguments (@('compute', 'regions', 'describe', $region, '--format=json') + $base)) -join "`n" | ConvertFrom-Json
    $q = $ssd.quotas | Where-Object { $_.metric -eq 'SSD_TOTAL_GB' }
    if ($q -and ($q.limit - $q.usage) -lt $cfg.dataDiskGb) {
        Write-Host "    SSD_TOTAL_GB in $region leaves $($q.limit - $q.usage) GB: the data disk will be hyperdisk-ml ($($cfg.dataDiskThroughput) MiB/s)"
        $type = 'hyperdisk-ml'
    }
}
Write-Step "Data disk $($cfg.dataDisk) ($type) in $newZone from $dataSnap"
$disk = @('compute', 'disks', 'create', $cfg.dataDisk, "--zone=$newZone", "--source-snapshot=$dataSnap", "--size=$($cfg.dataDiskGb)GB", "--type=$type", '--labels=app=xtp')
if ($type -eq 'hyperdisk-ml') { $disk += "--provisioned-throughput=$($cfg.dataDiskThroughput)" }
$null = Invoke-Gcloud -Arguments ($disk + $base)
$null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'attach-disk', $cfg.vm, "--zone=$newZone", "--disk=$($cfg.dataDisk)", "--device-name=$($cfg.dataDisk)") + $base)

# --- 5. The rest, as for a new VM ---------------------------------------------------------
& "$PSScriptRoot\new-vm.ps1" -Config $cfg.path
Write-Host ''
Write-Host "Moved: $($cfg.vm) runs in $newZone; $old waits, stopped, in $zone. Check the new VM (run-remote.ps1 -DeviceCheck), then: move-vm.ps1 -DeleteOld"
if ($type -ne $cfg.dataDiskType) { Write-Host "Set `"dataDiskType`": `"$type`" in $($cfg.path) (a disk made later in $region needs it)." }
