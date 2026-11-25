# Deletes the build farm from the project: its VMs and their disks, its
# images, its Cloud NAT and routers, its firewall rule (all named with the
# configuration's prefix). Nothing else of the project is touched. The local
# state (ledger, SSH key) stays in the state folder.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Remove-Farm.ps1 -Yes

param([switch] $Yes)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$c = Get-FarmConfig
if (-not $Yes) { throw "Remove-Farm deletes the farm's VMs, disks, images, NAT and firewall rule in $($c.project): run it again with -Yes" }

foreach ($i in @(Get-FarmInstances -Config $c -Refresh)) {
    Write-FarmMessage "deleting $($i.name)"
    Invoke-FarmGcloud -Config $c -Arguments @('compute', 'instances', 'delete', $i.name, "--zone=$($i.zone)", '--delete-disks=all') | Out-Null
}
$images = @((Invoke-FarmGcloud -Config $c -Json -Arguments @('compute', 'images', 'list', '--no-standard-images', "--filter=family=$($c.imageFamily) AND labels.xtp-role=buildfarm")).Data)
foreach ($img in $images) {
    if (-not $img) { continue }
    Write-FarmMessage "deleting image $($img.name)"
    Invoke-FarmGcloud -Config $c -Arguments @('compute', 'images', 'delete', $img.name) | Out-Null
}
foreach ($region in Get-FarmRegions $c) {
    $name = "$($c.prefix)-nat-$region"
    $r = Invoke-FarmGcloud -Config $c -AllowFailure -Arguments @('compute', 'routers', 'describe', $name, "--region=$region")
    if ($r.ExitCode -eq 0) {
        Write-FarmMessage "deleting Cloud NAT and router $name"
        Invoke-FarmGcloud -Config $c -Arguments @('compute', 'routers', 'delete', $name, "--region=$region") | Out-Null
    }
}
$fw = "$($c.prefix)-allow-iap-ssh"
$r = Invoke-FarmGcloud -Config $c -AllowFailure -Arguments @('compute', 'firewall-rules', 'describe', $fw)
if ($r.ExitCode -eq 0) {
    Write-FarmMessage "deleting firewall rule $fw"
    Invoke-FarmGcloud -Config $c -Arguments @('compute', 'firewall-rules', 'delete', $fw) | Out-Null
}
# The shader pack's URL signer (Copy-FarmPack.ps1); its bucket binding
# then names a deleted account and grants nothing.
$signer = "$($c.packSigner)@$($c.project).iam.gserviceaccount.com"
$r = Invoke-FarmGcloud -Config $c -AllowFailure -Arguments @('iam', 'service-accounts', 'describe', $signer)
if ($r.ExitCode -eq 0) {
    Write-FarmMessage "deleting service account $signer"
    Invoke-FarmGcloud -Config $c -Arguments @('iam', 'service-accounts', 'delete', $signer) | Out-Null
}
foreach ($f in 'instances.json', 'slots.json', 'health.json', 'image.json') { Remove-Item -Force -LiteralPath (Join-Path $c.stateDir $f) -ErrorAction SilentlyContinue }
Remove-Item -Force -Path (Join-Path $c.stateDir 'vms\*'), (Join-Path $c.stateDir 'ssh\known_hosts') -ErrorAction SilentlyContinue
Write-FarmMessage 'farm removed'
