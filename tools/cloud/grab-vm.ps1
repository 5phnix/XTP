<#
.SYNOPSIS
  Takes the first GPU free anywhere: starts the stopped VM in its zone, or
  creates it in another region, in a loop, until one runs. Logs every
  attempt.

.DESCRIPTION
  For when the VM's zone has had no GPU free for a while. Before each
  attempt in a region, the stopped VM (<vm>, and <vm>-old during a move) is
  started in its own zone; then one regional request (a bulk insert of one
  VM, Google picks a zone of the region with a GPU free) creates <vm> in the
  next region that offers the accelerator: standard, then Spot unless
  -NoSpot; g2-standard-16, then the other -MachineTypes once every region
  refused the first. One attempt at a time: an attempt in flight holds the
  GPU quota (one GPU), so parallel attempts would only be refused by it.

  A new VM boots -Image (a custom image family of the project: an image of
  the set-up boot disk, `gcloud compute images create <vm>-boot
  --source-disk=<vm> --source-disk-zone=<zone> --family=<vm>-boot`, so it is
  ready at once) or else the public Windows image (then run setup-vm.ps1).
  The game disk <vm>-data is made in its zone from -DataSnapshot
  (pd-balanced when the region's SSD_TOTAL_GB allows it, else hyperdisk-ml)
  and attached, and the stopped VM elsewhere is deleted with its disks.
  Then: new-vm.ps1 (metadata, the region's daily stop).

.EXAMPLE
  tools\cloud\grab-vm.ps1 -NoSpot -Image xtp-l4-boot -Minutes 240
#>
[CmdletBinding()]
param(
    [string]$Config,
    [int]$Minutes = 240,
    [string[]]$MachineTypes = @('g2-standard-16', 'g2-standard-12', 'g2-standard-8'),
    [switch]$NoSpot,
    [string]$Image = '',
    [string]$DataSnapshot = '',
    [string]$Log = ''
)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$base = Get-XtpBaseArgList $cfg
if (-not $DataSnapshot) { $DataSnapshot = "$($cfg.vm)-data-move" }
if (-not $Log) { $Log = Join-Path $env:LOCALAPPDATA 'xtp\cloud-grab.log' }
try { (Get-Process -Id $PID).PriorityClass = 'BelowNormal' } catch { Write-Verbose "priority unchanged: $_" }

function Write-Attempt([string]$Text) {
    $line = "{0:yyyy-MM-dd HH:mm:ss} {1}" -f (Get-Date), $Text
    Add-Content -LiteralPath $Log -Value $line
    Write-Host $line
}

$null = Invoke-Gcloud -Arguments (@('compute', 'snapshots', 'describe', $DataSnapshot, '--format=value(status)') + $base)
$zones = @(Invoke-Gcloud -Arguments (@('compute', 'accelerator-types', 'list', "--filter=name~^$([regex]::Escape($cfg.accelerator))", '--format=value(name,zone.basename())') + $base) |
        Where-Object { $_ } | ForEach-Object { $n, $z = $_ -split '\s+'; if ($n -eq $cfg.accelerator) { $z } } | Sort-Object -Unique)
# Stopped VMs to start: <vm> and, during a move, <vm>-old. A new VM is never
# made in their zones (their disk names would collide).
$stopped = @()
foreach ($name in $cfg.vm, "$($cfg.vm)-old") {
    foreach ($z in @(Get-XtpInstanceZone $cfg $name)) {
        $s = Get-XtpVmStatus ([pscustomobject]@{ vm = $name; project = $cfg.project }) $z
        if ($s -eq 'RUNNING') { Write-Attempt "$name runs already in $z"; exit 0 }
        $stopped += [pscustomobject]@{ Name = $name; Zone = $z }
    }
}
$busy = @($stopped | ForEach-Object { $_.Zone })
$regions = @($zones | ForEach-Object { $_ -replace '-[a-z]$', '' } | Sort-Object -Unique)
Write-Attempt "start: $($regions.Count) regions, types $($MachineTypes -join ','), spot $(-not $NoSpot), image $(if ($Image) { $Image } else { 'public' }), stopped VMs: $(($stopped | ForEach-Object { "$($_.Name) in $($_.Zone)" }) -join ', ')"

$metadata = "enable-windows-ssh=TRUE,block-project-ssh-keys=TRUE,xtp-idle-minutes=$($cfg.idleMinutes)"
if (-not $Image) { $metadata += ',sysprep-specialize-script-cmd=googet -noconfirm=true install google-compute-engine-ssh' }

# Starts a stopped VM and waits for the start to end either way (it holds the
# GPU quota meanwhile); returns its zone when it runs.
function Start-Stopped($Vm) {
    $vmCfg = [pscustomobject]@{ vm = $Vm.Name; project = $cfg.project }
    $t0 = Get-Date
    $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'start', $Vm.Name, "--zone=$($Vm.Zone)", '--async') + $base) -AllowFail -Quiet
    for ($i = 0; $i -lt 36; $i++) {
        Start-Sleep -Seconds 5
        if ((Get-XtpVmStatus $vmCfg $Vm.Zone) -in 'RUNNING', 'TERMINATED', 'STOPPED') { break }
    }
    $s = Get-XtpVmStatus $vmCfg $Vm.Zone
    Write-Attempt ("start {0} in {1}: {2} ({3:N0} s)" -f $Vm.Name, $Vm.Zone, $(if ($s -eq 'RUNNING') { 'STARTED' } else { 'no capacity' }), ((Get-Date) - $t0).TotalSeconds)
    return ($s -eq 'RUNNING')
}

# One regional bulk insert of one VM; returns the zone it landed in, or $null.
function New-InRegion([string]$Region, [string]$Type, [bool]$Spot) {
    $what = "create $Type $(if ($Spot) { 'spot' } else { 'standard' }) in $Region"
    $a = @('compute', 'instances', 'bulk', 'create', "--predefined-names=$($cfg.vm)", '--count=1', "--region=$Region",
        "--machine-type=$Type", "--accelerator=type=$($cfg.accelerator),count=$($cfg.acceleratorCount)", '--maintenance-policy=TERMINATE',
        "--boot-disk-size=$($cfg.bootDiskGb)GB", "--boot-disk-type=$($cfg.diskType)", "--boot-disk-device-name=$($cfg.vm)",
        "--service-account=$($cfg.serviceAccountEmail)", '--scopes=cloud-platform', "--metadata=$metadata",
        '--labels=app=xtp', '--tags=xtp')
    if ($Image) { $a += @("--image-family=$Image", "--image-project=$($cfg.project)") }
    else { $a += @("--image-project=$($cfg.imageProject)", "--image-family=$($cfg.imageFamily)") }
    if ($Spot) { $a += @('--provisioning-model=SPOT', '--instance-termination-action=STOP') }
    $deny = @($busy | Where-Object { $_ -like "$Region-*" } | ForEach-Object { "$_=deny" })
    if ($deny.Count) { $a += "--location-policy=$($deny -join ',')" }
    $t0 = Get-Date
    $null = Invoke-Gcloud -Arguments ($a + $base) -AllowFail -Quiet
    $text = "$global:XtpGcloudError"
    $zone = @(Get-XtpInstanceZone $cfg $cfg.vm | Where-Object { $busy -notcontains $_ }) | Select-Object -First 1
    $secs = ((Get-Date) - $t0).TotalSeconds
    if ($zone) { Write-Attempt ("{0}: CREATED in {1} ({2:N0} s)" -f $what, $zone, $secs); return $zone }
    $why = if ($text -match 'RESOURCE_POOL_EXHAUSTED|STOCKOUT|does not have enough resources|resource_availability') { 'no capacity' }
    elseif ($text -match "Quota '([A-Z_0-9]+)'") { "quota $($Matches[1])" }
    elseif ($text -match '[Qq]uota') { 'quota' }
    else { (($text -split "`n" | Where-Object { $_ -match 'ERROR|message|failed' } | Select-Object -First 1) -replace '\s+', ' ').Trim() }
    Write-Attempt ("{0}: {1} ({2:N0} s)" -f $what, $why, $secs)
    return $null
}

$deadline = (Get-Date).AddMinutes($Minutes)
$started = $null
$created = $null
$round = 0
while (-not $started -and -not $created -and (Get-Date) -lt $deadline) {
    $round++
    Write-Attempt "round $round"
    foreach ($type in $MachineTypes) {
        foreach ($r in $regions) {
            foreach ($vm in $stopped) { if (Start-Stopped $vm) { $started = $vm; break } }
            if ($started) { break }
            foreach ($spot in @($false) + $(if ($NoSpot) { @() } else { @($true) })) {
                $z = New-InRegion $r $type $spot
                if ($z) { $created = [pscustomobject]@{ Zone = $z; Type = $type; Spot = $spot }; break }
            }
            if ($created -or (Get-Date) -ge $deadline) { break }
        }
        if ($started -or $created -or (Get-Date) -ge $deadline) { break }
    }
}
if ($started) { Write-Attempt "READY: $($started.Name) runs again in $($started.Zone), with its disks."; exit 0 }
if (-not $created) { Write-Attempt "gave up after $Minutes min ($round rounds)"; exit 1 }

# The stopped VMs elsewhere go, with their disks (the game disk comes back
# from the snapshot, the boot disk from the image or the setup).
foreach ($vm in $stopped) {
    Write-Attempt "deleting the stopped $($vm.Name) in $($vm.Zone) with its disks"
    $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'delete', $vm.Name, "--zone=$($vm.Zone)", '--delete-disks=all') + $base) -AllowFail
}

# The game disk, from its snapshot, in the new VM's zone.
$region = $created.Zone -replace '-[a-z]$', ''
$type = $cfg.dataDiskType
if ($type -notlike 'hyperdisk*') {
    $q = ((Invoke-Gcloud -Arguments (@('compute', 'regions', 'describe', $region, '--format=json') + $base)) -join "`n" | ConvertFrom-Json).quotas |
        Where-Object { $_.metric -eq 'SSD_TOTAL_GB' }
    if ($q -and ($q.limit - $q.usage) -lt $cfg.dataDiskGb) { $type = 'hyperdisk-ml' }
}
Write-Attempt "data disk $($cfg.dataDisk) ($type) from $DataSnapshot in $($created.Zone)"
$disk = @('compute', 'disks', 'create', $cfg.dataDisk, "--zone=$($created.Zone)", "--source-snapshot=$DataSnapshot", "--size=$($cfg.dataDiskGb)GB", "--type=$type", '--labels=app=xtp')
if ($type -eq 'hyperdisk-ml') { $disk += "--provisioned-throughput=$($cfg.dataDiskThroughput)" }
$null = Invoke-Gcloud -Arguments ($disk + $base)
$null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'attach-disk', $cfg.vm, "--zone=$($created.Zone)", "--disk=$($cfg.dataDisk)", "--device-name=$($cfg.dataDisk)") + $base)
Write-Attempt "READY: $($cfg.vm) $($created.Type) $(if ($created.Spot) { 'spot' } else { 'standard' }) in $($created.Zone), game disk attached. Next: new-vm.ps1$(if (-not $Image) { ', setup-vm.ps1' })."
exit 0
