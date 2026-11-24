<#
  Runs ON THE VM, as an administrator (setup-vm.ps1 calls it over SSH).
  Windows PowerShell 5.1. Safe to run again: each step checks first.

  - the data disk as D: (NTFS, label xtp-data), the folders under C:\xtp and D:\
  - Defender exclusions for C:\xtp and D:\, long paths
  - the NVIDIA driver (Google's installer script; the vWS driver on a -vws GPU)
  - Git for Windows, Visual Studio Build Tools (MSVC + Windows SDK)
  - rustup for all users (C:\xtp\rust), the XTP clone in C:\xtp\src and the
    toolchain its rust-toolchain.toml pins
  Prints "REBOOT NEEDED" when the driver or the build tools want a restart.
#>
param(
    [string]$RepoUrl = 'https://github.com/MiamiGame/XTP.git',
    [int]$DataDiskGb = 300,
    # The Build Tools channel (17: Visual Studio 2022).
    [string]$VsChannel = '17'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$reboot = $false
$downloads = 'C:\xtp\downloads'

function Step([string]$t) { Write-Output "--> $t" }
# A native program, its output indented; throws on a non-zero exit. (Under
# 'Stop', Windows PowerShell 5.1 turns any stderr line into an error.)
function Invoke-Native([string]$Exe, [string[]]$ArgList, [switch]$AllowFail) {
    $ErrorActionPreference = 'Continue'
    & $Exe @ArgList 2>&1 | ForEach-Object { "    $_" }
    if ($LASTEXITCODE -ne 0 -and -not $AllowFail) { throw "$Exe $($ArgList -join ' ') failed ($LASTEXITCODE)" }
}
function Get-File([string]$Url, [string]$Name) {
    New-Item -ItemType Directory -Force -Path $downloads | Out-Null
    $path = Join-Path $downloads $Name
    Invoke-WebRequest -Uri $Url -OutFile $path -UseBasicParsing
    return $path
}
function Add-MachinePath([string]$Dir) {
    $p = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    if (($p -split ';') -notcontains $Dir) { [Environment]::SetEnvironmentVariable('Path', "$p;$Dir", 'Machine') }
    if (($env:Path -split ';') -notcontains $Dir) { $env:Path = "$env:Path;$Dir" }
}
function Find-Smi {
    return @('C:\Windows\System32\nvidia-smi.exe', 'C:\Program Files\NVIDIA Corporation\NVSMI\nvidia-smi.exe') |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}

# --- Data disk ---------------------------------------------------------------
Step 'Data disk'
if (-not (Get-Volume -DriveLetter D -ErrorAction SilentlyContinue | Where-Object FileSystem -eq 'NTFS')) {
    # NVMe disks do not show their device name: take the volume labelled
    # xtp-data (a disk set up before), else the blank disk of the data disk's size.
    $disk = Get-Volume -FileSystemLabel 'xtp-data' -ErrorAction SilentlyContinue | Get-Partition | Get-Disk | Select-Object -First 1
    if (-not $disk) {
        $disk = Get-Disk | Where-Object { $_.PartitionStyle -eq 'RAW' -and [math]::Round($_.Size / 1GB) -eq $DataDiskGb } | Select-Object -First 1
    }
    if (-not $disk) { throw "No blank $DataDiskGb GB data disk attached; run new-vm.ps1." }
    if ($disk.PartitionStyle -eq 'RAW') {
        Initialize-Disk -Number $disk.Number -PartitionStyle GPT
        $part = New-Partition -DiskNumber $disk.Number -UseMaximumSize -DriveLetter D
        Format-Volume -Partition $part -FileSystem NTFS -NewFileSystemLabel 'xtp-data' -Confirm:$false | Out-Null
    } else {
        # Partitioned already (a disk moved from another VM): give it D: back.
        if ($disk.IsOffline) { Set-Disk -Number $disk.Number -IsOffline $false }
        $part = Get-Partition -DiskNumber $disk.Number | Where-Object { $_.Type -eq 'Basic' } | Sort-Object Size -Descending | Select-Object -First 1
        if ($part.DriveLetter -ne 'D') { Set-Partition -DiskNumber $disk.Number -PartitionNumber $part.PartitionNumber -NewDriveLetter D }
    }
}
foreach ($d in 'C:\xtp\tools', 'C:\xtp\jobs', 'C:\xtp\logs', 'D:\game', 'D:\golden', 'D:\xtp-data\cache', 'D:\xtp-data\runs') {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}
$vol = Get-Volume -DriveLetter D
Write-Output ("    D: {0:N0} GB free of {1:N0} GB" -f ($vol.SizeRemaining / 1GB), ($vol.Size / 1GB))

# --- System settings ----------------------------------------------------------
Step 'Defender exclusions, long paths'
try {
    $pref = Get-MpPreference
    foreach ($p in 'C:\xtp', 'D:\') { if ($pref.ExclusionPath -notcontains $p) { Add-MpPreference -ExclusionPath $p } }
} catch { Write-Output "    (Defender not available: $($_.Exception.Message))" }
Set-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -Name LongPathsEnabled -Value 1 -Type DWord

# --- NVIDIA driver ----------------------------------------------------------
Step 'NVIDIA driver'
$smi = Find-Smi
$nvidia = Get-CimInstance Win32_PnPEntity -Filter "DeviceID LIKE 'PCI\\VEN_10DE%'" -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $smi -and -not $nvidia) {
    Write-Output '    no NVIDIA GPU on this machine: skipped (a build machine)'
} elseif (-not $smi) {
    # Google's script picks the driver for this machine (the vWS one on a
    # -vws GPU), checks its SHA-256 and installs it silently, without rebooting.
    $script = Get-File 'https://github.com/GoogleCloudPlatform/compute-gpu-installation/raw/main/windows/install_gpu_driver.ps1' 'install_gpu_driver.ps1'
    Invoke-Native powershell @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $script) -AllowFail
    $smi = Find-Smi
    if (-not $smi) { throw 'The NVIDIA driver did not install (no nvidia-smi).' }
    $reboot = $true
}
if ($smi) { Invoke-Native $smi @('--query-gpu=name,driver_version', '--format=csv,noheader') }

# --- Git ---------------------------------------------------------------------
Step 'Git'
$git = 'C:\Program Files\Git\cmd\git.exe'
if (-not (Test-Path $git)) {
    $rel = Invoke-RestMethod -Uri 'https://api.github.com/repos/git-for-windows/git/releases/latest' -UseBasicParsing
    $asset = $rel.assets | Where-Object { $_.name -match '^Git-[\d.]+-64-bit\.exe$' } | Select-Object -First 1
    if (-not $asset) { throw 'No Git for Windows 64-bit installer in the latest release.' }
    $exe = Get-File $asset.browser_download_url $asset.name
    $p = Start-Process -FilePath $exe -ArgumentList '/VERYSILENT', '/NORESTART', '/NOCANCEL', '/SP-', '/SUPPRESSMSGBOXES' -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "Git setup failed ($($p.ExitCode))" }
}
Add-MachinePath 'C:\Program Files\Git\cmd'
Invoke-Native $git @('config', '--system', 'core.longpaths', 'true')
# Builds run as several accounts (SSH users, the session account).
Invoke-Native $git @('config', '--system', '--replace-all', 'safe.directory', '*')
Invoke-Native $git @('--version')

# --- Visual Studio Build Tools ---------------------------------------------------
Step 'Visual Studio Build Tools (C++)'
$vswhere = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'
$vc = $null
if (Test-Path $vswhere) { $vc = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath }
if (-not $vc) {
    $exe = Get-File "https://aka.ms/vs/$VsChannel/release/vs_buildtools.exe" 'vs_buildtools.exe'
    $p = Start-Process -FilePath $exe -ArgumentList '--quiet', '--wait', '--norestart', '--nocache',
        '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended' -PassThru
    $null = $p.Handle
    $t0 = Get-Date
    # A line now and then keeps the SSH connection busy during the long silence.
    while (-not $p.WaitForExit(60000)) { Write-Output ("    installing ({0:N0} min)" -f ((Get-Date) - $t0).TotalMinutes) }
    if ($p.ExitCode -eq 3010) { $reboot = $true } elseif ($p.ExitCode -ne 0) { throw "Build Tools setup failed ($($p.ExitCode))" }
    $vc = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $vc) { throw 'The C++ build tools are not installed.' }
}
Write-Output "    $vc"

# --- Rust ----------------------------------------------------------------------
Step 'rustup (all users, C:\xtp\rust)'
$rustupHome = 'C:\xtp\rust\rustup'
$cargoHome = 'C:\xtp\rust\cargo'
[Environment]::SetEnvironmentVariable('RUSTUP_HOME', $rustupHome, 'Machine')
[Environment]::SetEnvironmentVariable('CARGO_HOME', $cargoHome, 'Machine')
$env:RUSTUP_HOME = $rustupHome
$env:CARGO_HOME = $cargoHome
Add-MachinePath "$cargoHome\bin"
if (-not (Test-Path "$cargoHome\bin\rustup.exe")) {
    $exe = Get-File 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe' 'rustup-init.exe'
    Invoke-Native $exe @('-y', '--default-toolchain', 'none', '--profile', 'minimal', '--no-modify-path')
}

# --- The repository --------------------------------------------------------------
Step "XTP clone in C:\xtp\src ($RepoUrl)"
if (-not (Test-Path 'C:\xtp\src\.git')) {
    Invoke-Native $git @('clone', '--quiet', $RepoUrl, 'C:\xtp\src')
} else {
    Invoke-Native $git @('-C', 'C:\xtp\src', 'fetch', '--quiet', 'origin')
}
Push-Location C:\xtp\src
try {
    # Installs the channel, components and targets rust-toolchain.toml pins.
    Invoke-Native "$cargoHome\bin\rustup.exe" @('toolchain', 'install')
    Invoke-Native "$cargoHome\bin\rustc.exe" @('--version')
} finally { Pop-Location }

# --- gcloud: fast CRC32C for composite objects ---------------------------------------
Step 'gcloud (preinstalled on Google Windows images)'
if (-not (Get-Command gcloud -ErrorAction SilentlyContinue)) { throw 'gcloud is not installed on this VM.' }
$crc = Invoke-Native gcloud @('components', 'list', '--filter=id=gcloud-crc32c', '--format=value(state.name)') -AllowFail
if ("$crc" -notmatch 'Installed') {
    # Not fatal: without it, downloads of composite objects are slower.
    Invoke-Native gcloud @('components', 'install', 'gcloud-crc32c', '--quiet') -AllowFail
}

if ($reboot) { Write-Output 'REBOOT NEEDED' }
Write-Output 'toolchain ready'
exit 0
