# Builds the build farm's Windows image: creates the builder VM (the first VM
# of the farm configuration, or -Builder) from Google's Windows Server image,
# installs the toolchain on it (vm\setup-toolchain.ps1: MinGit, Visual Studio
# Build Tools, rustup and the toolchain of rust-toolchain.toml) and the farm
# agent, stops it and saves its disk as a new image of the farm's image
# family. The builder stays as a farm VM; New-FarmVm.ps1 makes the others
# from the image.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\New-FarmImage.ps1 [-Builder <vm>]
#
# Also creates what the farm needs first: the IAP SSH firewall rule, Cloud NAT
# in the farm's regions, the farm's SSH key, the price list. Takes 30 to 45
# minutes. The configuration comes from XTP_FARM_CONFIG (docs/dev/build-farm.md).

param([string] $Builder = '', [switch] $SkipImage)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$c = Get-FarmConfig
if (-not $Builder) { $Builder = @($c.vms)[0].name }
$spec = Get-FarmVmSpec $c $Builder
$t0 = Get-Date

Initialize-FarmApis $c
Initialize-FarmNetwork $c
Initialize-FarmSsh $c | Out-Null
$prices = Get-FarmPrices $c
if (-not $prices -or ((Get-Date).ToUniversalTime() - (ConvertTo-FarmUtc $prices.fetched)).TotalDays -gt 7) { $prices = Update-FarmPrices $c }

$inst = @(Get-FarmInstances -Config $c -Refresh) | Where-Object { $_.name -eq $Builder } | Select-Object -First 1
if (-not $inst) {
    $tc = Get-FarmRustToolchain
    New-FarmInstance -Config $c -Name $Builder -FromBaseImage -StartupScript (Join-Path $PSScriptRoot 'vm\setup-toolchain.ps1') `
        -Metadata @("xtp-farm-rust-toolchain=$tc")
} elseif (Test-FarmStopped $inst) {
    Start-FarmVm $c $Builder
}
Wait-FarmSetup $c $Builder
Update-FarmKnownHost -Config $c -Vm $Builder -WaitSeconds 300
Wait-FarmVmReady $c $Builder
$v = Publish-FarmAgent $c $Builder
Write-FarmMessage "agent $v installed on $Builder"
$r = Invoke-FarmSsh -Config $c -Vm $Builder -Command 'cargo -V && rustc -V && git --version && type C:\farm\setup\done.json' -Capture
if ($r.ExitCode -ne 0) { throw "toolchain check on $Builder failed: $($r.Err.Trim())" }
$r.Out.Trim() -split "`r?`n" | ForEach-Object { Write-FarmMessage "$Builder> $_" }

# The startup script has done its work: VMs made from the image must not
# carry it.
Invoke-FarmGcloud -Config $c -AllowFailure -Arguments @('compute', 'instances', 'remove-metadata', $Builder, "--zone=$($spec.zone)",
    '--keys=windows-startup-script-ps1,sysprep-specialize-script-cmd,xtp-farm-rust-toolchain') | Out-Null

# The shader pack (Copy-FarmPack.ps1) stays on the VMs' disks, never in an
# image: run Copy-FarmPack.ps1 again afterwards.
$r = Invoke-FarmSsh -Config $c -Vm $Builder -Command 'if exist C:\farm\pack rmdir /s /q C:\farm\pack' -Capture
if ($r.ExitCode -ne 0 -or (Invoke-FarmSsh -Config $c -Vm $Builder -Command 'if exist C:\farm\pack exit 1' -Capture).ExitCode -ne 0) {
    throw "cannot remove C:\farm\pack from ${Builder}: not making an image that holds the shader pack"
}

Write-FarmMessage "stopping $Builder"
Stop-FarmVm $c $Builder
Get-FarmInstances -Config $c -Refresh | Out-Null
if ($SkipImage) { Write-FarmMessage 'image not made (-SkipImage)'; exit 0 }

$name = '{0}-{1}' -f $c.imageFamily, (Get-Date).ToUniversalTime().ToString('yyyyMMdd-HHmm')
Write-FarmMessage "saving the disk of $Builder as image $name (family $($c.imageFamily))"
Invoke-FarmGcloud -Config $c -Arguments @('compute', 'images', 'create', $name, "--source-disk=$Builder", "--source-disk-zone=$($spec.zone)",
    "--family=$($c.imageFamily)", '--labels=xtp-role=buildfarm', '--description=XTP build farm: Windows Server, MSVC, Rust (tools/buildfarm)') | Out-Null
$img = (Invoke-FarmGcloud -Config $c -Json -Arguments @('compute', 'images', 'describe', $name)).Data
$gb = [Math]::Round([double] $img.archiveSizeBytes / 1GB, 1)
Write-FarmJson (Join-Path $c.stateDir 'image.json') ([pscustomobject]@{ name = $name; archiveGb = $gb; diskGb = $img.diskSizeGb; created = (ConvertTo-FarmStamp (Get-Date).ToUniversalTime()) })
Write-FarmLog $c "image $name made from $Builder ($gb GB stored)"
Write-FarmMessage ('image {0} ready ({1} GB stored) in {2:N0} minutes' -f $name, $gb, ((Get-Date) - $t0).TotalMinutes)
