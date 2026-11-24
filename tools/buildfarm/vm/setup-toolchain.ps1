# Toolchain setup of an XTP build farm VM (Windows Server on Google Cloud).
#
# Runs as SYSTEM from the `windows-startup-script-ps1` metadata of the image
# builder (New-FarmImage.ps1), at every boot until it has finished once.
# Progress goes to the serial console, each line prefixed "xtp-farm-setup:",
# which New-FarmImage.ps1 follows.
#
# Installs MinGit, Visual Studio 2022 Build Tools (MSVC and the Windows SDK),
# rustup and the toolchain pinned by the repository's rust-toolchain.toml
# (passed in the `xtp-farm-rust-toolchain` metadata attribute). The VM holds
# source code only: nothing here, nor anywhere in the farm, copies game files.
#
# Windows PowerShell 5.1. ASCII only.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Root = 'C:\farm'
foreach ($d in 'bin', 'w', 'tmp', 'log', 'locks', 'run', 'state', 'setup', 'cargo', 'rustup', 'dl') {
    New-Item -ItemType Directory -Force -Path (Join-Path $Root $d) | Out-Null
}
$Done = Join-Path $Root 'setup\done.json'

function Write-Setup([string] $Message) {
    Write-Host "xtp-farm-setup: $Message"
    Add-Content -Path (Join-Path $Root 'log\setup.log') -Value ('{0:u} {1}' -f (Get-Date), $Message)
}

function Get-Metadata([string] $Key, [string] $Default = '') {
    try {
        $v = Invoke-RestMethod -UseBasicParsing -TimeoutSec 10 -Headers @{ 'Metadata-Flavor' = 'Google' } `
            -Uri "http://metadata.google.internal/computeMetadata/v1/instance/attributes/$Key"
        if ($null -eq $v -or "$v" -eq '') { return $Default }
        return "$v".Trim()
    } catch {
        return $Default
    }
}

function Get-Download([string] $Url, [string] $OutFile) {
    for ($i = 1; $i -le 5; $i++) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $OutFile -Headers @{ 'User-Agent' = 'xtp-buildfarm' }
            return
        } catch {
            Write-Setup "download failed ($i/5): $Url : $($_.Exception.Message)"
            Start-Sleep -Seconds (15 * $i)
        }
    }
    throw "cannot download $Url"
}

# Runs a console program, sends its output (stdout and stderr) to the setup
# log, and fails on a non-zero exit code (other than those listed as fine).
function Invoke-Logged([string] $Exe, [string[]] $Arguments, [int[]] $Fine = @(0)) {
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Exe @Arguments 2>&1 | ForEach-Object { Write-Setup ('  ' + "$_") }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $old
    }
    if ($Fine -notcontains $code) { throw "$Exe exited with $code" }
}

if (Test-Path $Done) {
    Write-Setup 'already set up'
    exit 0
}

try {
    Write-Setup "start ($([Environment]::OSVersion.VersionString))"

    # Builds touch tens of thousands of files: keep Defender off the farm's
    # folders and the compiler processes (a large speed-up on Windows).
    try {
        Add-MpPreference -ExclusionPath $Root, 'C:\BuildTools'
        Add-MpPreference -ExclusionProcess 'cargo.exe', 'rustc.exe', 'clippy-driver.exe', 'rustdoc.exe', 'link.exe', 'cl.exe', 'git.exe'
        Write-Setup 'Defender exclusions set'
    } catch {
        Write-Setup "Defender exclusions not set: $($_.Exception.Message)"
    }

    # No automatic updates: a build VM lives for minutes at a time and the
    # image is rebuilt from Google's monthly patched image instead.
    $wu = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU'
    New-Item -Path $wu -Force | Out-Null
    Set-ItemProperty -Path $wu -Name NoAutoUpdate -Value 1 -Type DWord

    # Remote commands run through cmd.exe, so binary output (tar streams of
    # artifacts) passes untouched.
    New-Item -Path 'HKLM:\SOFTWARE\OpenSSH' -Force | Out-Null
    Set-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell -Value 'C:\Windows\System32\cmd.exe'

    # MinGit: git without a shell, enough for bundles and checkouts.
    $git = Join-Path $Root 'git\cmd\git.exe'
    if (-not (Test-Path $git)) {
        $url = Get-Metadata 'xtp-farm-mingit-url'
        if (-not $url) {
            $rel = Invoke-RestMethod -UseBasicParsing -Headers @{ 'User-Agent' = 'xtp-buildfarm' } `
                -Uri 'https://api.github.com/repos/git-for-windows/git/releases/latest'
            $asset = $rel.assets | Where-Object { $_.name -match '^MinGit-[0-9][0-9.]*-64-bit\.zip$' } | Select-Object -First 1
            if (-not $asset) { throw 'no MinGit 64-bit zip in the latest Git for Windows release' }
            $url = $asset.browser_download_url
        }
        Write-Setup "MinGit: $url"
        Get-Download $url (Join-Path $Root 'dl\mingit.zip')
        Expand-Archive -Path (Join-Path $Root 'dl\mingit.zip') -DestinationPath (Join-Path $Root 'git') -Force
    }
    Invoke-Logged $git @('config', '--system', 'core.longpaths', 'true')
    Invoke-Logged $git @('config', '--system', '--replace-all', 'safe.directory', '*')
    Invoke-Logged $git @('--version')

    # Visual Studio 2022 Build Tools: MSVC x64 and the Windows SDK.
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $vs = $null
    if (Test-Path $vswhere) {
        $vs = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    }
    if (-not $vs) {
        Write-Setup 'Visual Studio Build Tools: installing (10 to 20 minutes)'
        $vsExe = Join-Path $Root 'dl\vs_BuildTools.exe'
        Get-Download 'https://aka.ms/vs/17/release/vs_BuildTools.exe' $vsExe
        $p = Start-Process -FilePath $vsExe -Wait -PassThru -ArgumentList @(
            '--quiet', '--wait', '--norestart', '--nocache', '--installPath', 'C:\BuildTools',
            '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended')
        if (@(0, 3010) -notcontains $p.ExitCode) { throw "Visual Studio Build Tools installer exited with $($p.ExitCode)" }
        $vs = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (-not $vs) { throw 'Visual Studio Build Tools installed but MSVC is not found' }
    }
    Write-Setup "MSVC: $vs"

    # rustup and the pinned toolchain, machine-wide under C:\farm.
    $env:RUSTUP_HOME = Join-Path $Root 'rustup'
    $env:CARGO_HOME = Join-Path $Root 'cargo'
    [Environment]::SetEnvironmentVariable('RUSTUP_HOME', $env:RUSTUP_HOME, 'Machine')
    [Environment]::SetEnvironmentVariable('CARGO_HOME', $env:CARGO_HOME, 'Machine')
    $rustup = Join-Path $env:CARGO_HOME 'bin\rustup.exe'
    if (-not (Test-Path $rustup)) {
        $init = Join-Path $Root 'dl\rustup-init.exe'
        Get-Download 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe' $init
        Invoke-Logged $init @('-y', '--no-modify-path', '--default-toolchain', 'none', '--profile', 'minimal')
    }
    $toolchain = Get-Metadata 'xtp-farm-rust-toolchain' 'stable'
    Write-Setup "Rust toolchain: $toolchain"
    Invoke-Logged $rustup @('toolchain', 'install', $toolchain, '--profile', 'minimal', '-c', 'rustfmt', '-c', 'clippy', '-t', 'x86_64-pc-windows-msvc')
    Invoke-Logged $rustup @('default', $toolchain)

    $path = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    foreach ($d in (Join-Path $env:CARGO_HOME 'bin'), (Join-Path $Root 'git\cmd')) {
        if (($path -split ';') -notcontains $d) { $path = "$path;$d" }
    }
    [Environment]::SetEnvironmentVariable('Path', $path, 'Machine')

    # What the farm's runner sets for every build (see farm-run.ps1).
    @{
        CARGO_HOME  = $env:CARGO_HOME
        RUSTUP_HOME = $env:RUSTUP_HOME
        PathPrepend = @((Join-Path $env:CARGO_HOME 'bin'), (Join-Path $Root 'git\cmd'))
        Git         = $git
    } | ConvertTo-Json | Set-Content -Encoding ascii -Path (Join-Path $Root 'farm-env.json')

    # The D3D12 debug layer (Graphics Tools), for XTP_D3D12_DEBUG runs. Optional.
    try {
        $cap = Get-WindowsCapability -Online -Name 'Tools.Graphics.DirectX~~~~0.0.1.0'
        if ($cap.State -ne 'Installed') {
            Add-WindowsCapability -Online -Name 'Tools.Graphics.DirectX~~~~0.0.1.0' | Out-Null
        }
        Write-Setup 'Graphics Tools (D3D12 debug layer) installed'
    } catch {
        Write-Setup "Graphics Tools not installed (optional): $($_.Exception.Message)"
    }

    # sshd's host key goes to the guest attributes, where the client reads
    # it for its known_hosts (no trust on first use). The agent does the
    # same at every boot and regenerates the keys on a VM made from the image.
    $pub = 'C:\ProgramData\ssh\ssh_host_ed25519_key.pub'
    for ($i = 0; $i -lt 120 -and -not (Test-Path $pub); $i++) { Start-Sleep -Seconds 5 }
    if (-not (Test-Path $pub)) { throw "no sshd host key ($pub): is enable-windows-ssh=TRUE set?" }
    $meta = 'http://metadata.google.internal/computeMetadata/v1/instance'
    $hdr = @{ 'Metadata-Flavor' = 'Google' }
    $id = "$(Invoke-RestMethod -UseBasicParsing -Headers $hdr -Uri "$meta/id")".Trim()
    Set-Content -Encoding ascii -Path (Join-Path $Root 'state\instance-id') -Value $id
    Invoke-RestMethod -UseBasicParsing -Method Put -Headers $hdr -Uri "$meta/guest-attributes/xtp-farm/hostkey" `
        -Body (Get-Content -Raw $pub).Trim() | Out-Null
    Write-Setup 'host key published'

    $versions = @{
        os        = [Environment]::OSVersion.VersionString
        git       = (& $git --version)
        rustc     = (& (Join-Path $env:CARGO_HOME 'bin\rustc.exe') -V)
        cargo     = (& (Join-Path $env:CARGO_HOME 'bin\cargo.exe') -V)
        msvc      = "$vs"
        toolchain = $toolchain
        finished  = (Get-Date).ToUniversalTime().ToString('o')
    }
    $versions | ConvertTo-Json | Set-Content -Encoding ascii -Path $Done
    Write-Setup ("versions: " + ($versions | ConvertTo-Json -Compress))
    Write-Setup 'done'
} catch {
    Write-Setup "FAILED: $($_.Exception.Message)"
    Write-Setup "at: $($_.InvocationInfo.PositionMessage -replace '\s+', ' ')"
    exit 1
}
