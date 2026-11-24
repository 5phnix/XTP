<#
.SYNOPSIS
  Creates the Windows VM with its GPU, its data disk, SSH access through IAP
  and a daily stop schedule. Safe to run again: what exists is kept.

.DESCRIPTION
  0. The firewall of the default network: refuses to go on while a rule lets
     SSH or RDP in from 0.0.0.0/0, and adds xtp-allow-iap (tcp 22 and 3389
     from IAP's 35.235.240.0/20 to VMs tagged xtp) unless rules already do.
  1. The VM (machineType, accelerator, imageFamily, a bootDiskGb boot disk)
     in the first configured zone with capacity. GPUs are often out of stock
     (ZONE_RESOURCE_POOL_EXHAUSTED): with -RetryMinutes the zones are tried
     again every 4 minutes until one has capacity.
     It runs as the bucket-reader service account, has an ephemeral external
     address for outgoing traffic only (the firewall lets in nothing but
     Google's IAP range), Windows SSH enabled (enable-windows-ssh) and only
     its own SSH keys (block-project-ssh-keys).
  2. The data disk <vm>-data (dataDiskGb), created in the VM's zone and
     attached. setup-vm.ps1 formats it as D:.
  3. A resource policy that stops the VM every day (stopSchedule, timeZone),
     a backstop for the VM's own idle shutdown. The Compute Engine service
     agent gets the right to stop and start this VM only.
  The GPU driver and the tools are installed by setup-vm.ps1.

.EXAMPLE
  tools\cloud\new-vm.ps1
  tools\cloud\new-vm.ps1 -RetryMinutes 360
#>
[CmdletBinding()]
param(
    [string]$Config,
    # Keep trying the zones for this long when none has capacity (0: one round).
    [int]$RetryMinutes = 0
)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$base = Get-XtpBaseArgList $cfg

$metadata = @{
    'enable-windows-ssh'             = 'TRUE'
    'block-project-ssh-keys'         = 'TRUE'
    'sysprep-specialize-script-cmd' = 'googet -noconfirm=true install google-compute-engine-ssh'
    'xtp-idle-minutes'               = "$($cfg.idleMinutes)"
}

Write-Step "Service account $($cfg.serviceAccountEmail)"
$null = Invoke-Gcloud -Arguments (@('iam', 'service-accounts', 'describe', $cfg.serviceAccountEmail, '--format=value(email)') + $base) -AllowFail
if ($LASTEXITCODE -ne 0) {
    $null = Invoke-Gcloud -Arguments (@('iam', 'service-accounts', 'create', $cfg.serviceAccount, '--display-name=XTP cloud VM (reads the game bucket)') + $base)
    Start-Sleep -Seconds 10
}

# --- 0. The firewall: SSH and RDP from Google's IAP range only ---------------
# A new project's default network lets SSH (22) and RDP (3389) in from the
# whole Internet; the VM has an external address (for outgoing traffic), so
# such a rule would expose it. Refuse it; add the IAP rule when missing.
$iapRange = '35.235.240.0/20'
Write-Step 'Firewall (network default)'
$rules = @((Invoke-Gcloud -Arguments (@('compute', 'firewall-rules', 'list', '--format=json') + $base)) -join "`n" | ConvertFrom-Json |
        Where-Object { $_.network -like '*/networks/default' -and $_.direction -eq 'INGRESS' -and -not $_.disabled })
function Test-RuleOpensPort($rule, [int]$port) {
    foreach ($a in $rule.allowed) {
        if ($a.IPProtocol -eq 'all') { return $true }
        if ($a.IPProtocol -ne 'tcp') { continue }
        if (-not $a.ports) { return $true }
        foreach ($p in $a.ports) {
            if ($p -match '^(\d+)-(\d+)$') { if ($port -ge [int]$Matches[1] -and $port -le [int]$Matches[2]) { return $true } }
            elseif ([int]$p -eq $port) { return $true }
        }
    }
    return $false
}
function Test-RuleScope($rule) { return (-not $rule.targetTags -and -not $rule.targetServiceAccounts) -or ($rule.targetTags -contains 'xtp') }
$open = @($rules | Where-Object { (Test-RuleScope $_) -and ($_.sourceRanges -contains '0.0.0.0/0') -and ((Test-RuleOpensPort $_ 22) -or (Test-RuleOpensPort $_ 3389)) })
if ($open.Count) {
    $names = ($open | ForEach-Object { $_.name }) -join ', '
    throw "Firewall rule(s) $names let SSH or RDP in from the whole Internet. Restrict them to IAP first, e.g.:`n  gcloud compute firewall-rules update <rule> --source-ranges=$iapRange --project=$($cfg.project)`n(or delete them). See docs/dev/cloud-setup.md, 'Project'."
}
$fromIap = @($rules | Where-Object { (Test-RuleScope $_) -and ($_.sourceRanges -contains $iapRange) })
$ssh = @($fromIap | Where-Object { Test-RuleOpensPort $_ 22 }).Count
$rdp = @($fromIap | Where-Object { Test-RuleOpensPort $_ 3389 }).Count
if (-not $ssh -or -not $rdp) {
    $null = Invoke-Gcloud -Arguments (@('compute', 'firewall-rules', 'create', 'xtp-allow-iap', '--network=default', '--direction=INGRESS',
            '--action=allow', '--rules=tcp:22,tcp:3389', "--source-ranges=$iapRange", '--target-tags=xtp',
            '--description=SSH and RDP to the XTP VM through IAP only') + $base)
}

# --- 1. The VM -------------------------------------------------------------
$zone = Get-XtpVmZone $cfg
$created = $false
if ($zone) {
    Write-Step "$($cfg.vm) exists in $zone ($(Get-XtpVmStatus $cfg $zone))"
    # Bring the metadata up to date (a VM made by an earlier version, or by hand).
    $have = (Invoke-Gcloud -Arguments (@('compute', 'instances', 'describe', $cfg.vm, "--zone=$zone", '--format=json(metadata.items)') + $base)) -join "`n" | ConvertFrom-Json
    # (The sysprep script only matters at the first boot.)
    $missing = @($metadata.Keys | Where-Object { $k = $_; $k -ne 'sysprep-specialize-script-cmd' -and
            -not ($have.metadata.items | Where-Object { $_.key -eq $k -and $_.value -eq $metadata[$k] }) })
    if ($missing.Count) {
        Write-Step "Setting metadata: $($missing -join ', ')"
        $pairs = $missing | ForEach-Object { "$_=$($metadata[$_])" }
        $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'add-metadata', $cfg.vm, "--zone=$zone", "--metadata=$($pairs -join ',')") + $base)
    }
    $tags = Invoke-Gcloud -Arguments (@('compute', 'instances', 'describe', $cfg.vm, "--zone=$zone", '--format=value(tags.items)') + $base)
    if (($tags -join ';') -notmatch '(^|;)xtp(;|$)') {
        $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'add-tags', $cfg.vm, "--zone=$zone", '--tags=xtp') + $base)
    }
} else {
    $common = @('compute', 'instances', 'create', $cfg.vm,
        "--machine-type=$($cfg.machineType)",
        "--image-project=$($cfg.imageProject)", "--image-family=$($cfg.imageFamily)",
        "--boot-disk-size=$($cfg.bootDiskGb)GB", "--boot-disk-type=$($cfg.diskType)", "--boot-disk-device-name=$($cfg.vm)",
        "--service-account=$($cfg.serviceAccountEmail)", '--scopes=cloud-platform',
        "--metadata=$(($metadata.Keys | ForEach-Object { "$_=$($metadata[$_])" }) -join ',')",
        '--labels=app=xtp', '--tags=xtp', '--shielded-vtpm', '--shielded-integrity-monitoring')
    if ($cfg.accelerator) {
        $common += @("--accelerator=type=$($cfg.accelerator),count=$($cfg.acceleratorCount)", '--maintenance-policy=TERMINATE')
    }
    $deadline = (Get-Date).AddMinutes($RetryMinutes)
    $round = 0
    while (-not $created) {
        $round++
        foreach ($z in @($cfg.zones)) {
            Write-Host ("    {0:HH:mm:ss} round {1}: {2} ..." -f (Get-Date), $round, $z) -NoNewline
            $null = Invoke-Gcloud -Arguments ($common + @("--zone=$z") + $base) -AllowFail -Quiet
            if ($LASTEXITCODE -eq 0) { Write-Host ' created'; $zone = $z; $created = $true; break }
            $err = $global:XtpGcloudError
            if ($err -match 'RESOURCE_POOL_EXHAUSTED|STOCKOUT|does not have enough resources|resource_availability') { Write-Host ' no capacity'; continue }
            Write-Host ' failed'
            if ($err -match 'QUOTA|[Qq]uota') { throw "Quota refused in ${z}: see docs/dev/cloud-setup.md, 'Quotas'.`n$err" }
            throw "Creating the VM in $z failed:`n$err"
        }
        if ($created) { break }
        if ((Get-Date) -ge $deadline) {
            throw "No configured zone has capacity for $($cfg.machineType) + $($cfg.accelerator) right now. Try later, add zones, or pass -RetryMinutes."
        }
        Start-Sleep -Seconds 240
    }
    if ($zone -notlike "$($cfg.region)-*") {
        Write-Warning "The VM is in $zone, outside the bucket's region $($cfg.region): reading the bucket will be slower and billed."
    }
}
$region = $zone -replace '-[a-z]$', ''

# --- 2. The data disk ------------------------------------------------------
Write-Step "Data disk $($cfg.dataDisk) ($($cfg.dataDiskGb) GB $($cfg.dataDiskType)) in $zone"
$null = Invoke-Gcloud -Arguments (@('compute', 'disks', 'describe', $cfg.dataDisk, "--zone=$zone", '--format=value(name)') + $base) -AllowFail
if ($LASTEXITCODE -ne 0) {
    $a = @('compute', 'disks', 'create', $cfg.dataDisk, "--zone=$zone", "--size=$($cfg.dataDiskGb)GB",
        "--type=$($cfg.dataDiskType)", '--labels=app=xtp')
    # Hyperdisk ML has its own quota (HDML_TOTAL_GB), and bills its throughput.
    if ($cfg.dataDiskType -eq 'hyperdisk-ml') { $a += "--provisioned-throughput=$($cfg.dataDiskThroughput)" }
    $null = Invoke-Gcloud -Arguments ($a + $base) -AllowFail
    if ($LASTEXITCODE -ne 0) {
        if ($global:XtpGcloudError -match 'SSD_TOTAL_GB|[Qq]uota') {
            throw "Disk quota refused in ${region}: the boot and data disks need $($cfg.bootDiskGb + $cfg.dataDiskGb) GB of SSD_TOTAL_GB (or set dataDiskType to hyperdisk-ml). See docs/dev/cloud-setup.md, 'Quotas'.`n$global:XtpGcloudError"
        }
        throw $global:XtpGcloudError
    }
}
$attached = Invoke-Gcloud -Arguments (@('compute', 'instances', 'describe', $cfg.vm, "--zone=$zone", '--format=value(disks[].deviceName)') + $base)
if (($attached -join ';') -notmatch "(^|;)$([regex]::Escape($cfg.dataDisk))(;|$)") {
    $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'attach-disk', $cfg.vm, "--zone=$zone", "--disk=$($cfg.dataDisk)",
            "--device-name=$($cfg.dataDisk)") + $base)
}

# --- 3. The daily stop schedule --------------------------------------------
if ($cfg.stopSchedule) {
    $policy = "$($cfg.vm)-daily-stop"
    Write-Step "Instance schedule $policy ($($cfg.stopSchedule), $($cfg.timeZone))"
    $null = Invoke-Gcloud -Arguments (@('compute', 'resource-policies', 'describe', $policy, "--region=$region", '--format=value(name)') + $base) -AllowFail
    if ($LASTEXITCODE -ne 0) {
        $null = Invoke-Gcloud -Arguments (@('compute', 'resource-policies', 'create', 'instance-schedule', $policy, "--region=$region",
                "--vm-stop-schedule=$($cfg.stopSchedule)", "--timezone=$($cfg.timeZone)",
                '--description=Stops the XTP VM daily, in case its idle shutdown did not') + $base)
    }
    # The schedule acts as the Compute Engine service agent: let it stop and
    # start this VM (and nothing else).
    $number = Invoke-Gcloud -Arguments (@('projects', 'describe', $cfg.project, '--format=value(projectNumber)') + $base) | Select-Object -First 1
    $agent = "serviceAccount:service-$number@compute-system.iam.gserviceaccount.com"
    $iam = (Invoke-Gcloud -Arguments (@('compute', 'instances', 'get-iam-policy', $cfg.vm, "--zone=$zone", '--format=json') + $base)) -join "`n" | ConvertFrom-Json
    if (-not ($iam.bindings | Where-Object { $_.role -eq 'roles/compute.instanceAdmin.v1' -and $_.members -contains $agent })) {
        $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'add-iam-policy-binding', $cfg.vm, "--zone=$zone", "--member=$agent",
                '--role=roles/compute.instanceAdmin.v1') + $base)
    }
    $policies = Invoke-Gcloud -Arguments (@('compute', 'instances', 'describe', $cfg.vm, "--zone=$zone", '--format=value(resourcePolicies.basename())') + $base)
    if (($policies -join ';') -notmatch [regex]::Escape($policy)) {
        $null = Invoke-Gcloud -Arguments (@('compute', 'instances', 'add-resource-policies', $cfg.vm, "--zone=$zone", "--resource-policies=$policy") + $base)
    }
}

$status = Get-XtpVmStatus $cfg $zone
Write-Host ''
Write-Host "VM $($cfg.vm): $zone, $status. Next: tools\cloud\setup-vm.ps1 (driver, tools, data disk, session)."
if ($status -eq 'RUNNING') {
    Write-Host 'It is running (and billed) now; setup-vm.ps1 uses it right away. Stop it with tools\cloud\stop-vm.ps1 if you continue later.'
}
