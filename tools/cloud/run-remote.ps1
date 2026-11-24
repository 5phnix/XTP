<#
.SYNOPSIS
  Runs XTP's checks, tests or a game run on the VM and brings the logs back.

.DESCRIPTION
  What runs (one of):
    -Check          the workspace check: cargo fmt --check, clippy (default
                    and all features, -D warnings), cargo test (the default)
    -Cargo '<args>' cargo with these arguments, e.g. 'test -p xtp-gpu'
    -Game           builds the diagnostics xtp (unless -NoBuild) and runs the
                    title for -Seconds in the VM's desktop session, from the
                    golden data folder if the VM has one (see vm\run-game.ps1)
    -DeviceCheck    the D3D12 adapters and what they support

  Which code (for -Check, -Cargo and -Game's build):
    default         origin/main of the VM's clone (or -Ref <commit|branch>)
    -Local          your current commit (HEAD of the repository this script is
                    in), sent to the VM as a git bundle: commit first, local
                    changes are not sent

  The VM is started if needed and stops by itself once idle. One build and
  one game run at a time: a second caller waits. Logs go to
  <localLogDir>\<date-time>-<what>\ (default %LOCALAPPDATA%\xtp\cloud-runs).

.EXAMPLE
  tools\cloud\run-remote.ps1 -Check -Local
  tools\cloud\run-remote.ps1 -Cargo 'test -p xtp-gpu --locked' -Local
  tools\cloud\run-remote.ps1 -Game -Seconds 240 -Local
  tools\cloud\run-remote.ps1 -Game -NoBuild -Env @{ XTP_LOG = 'xtp_gpu=debug,info' }
#>
[CmdletBinding(DefaultParameterSetName = 'Check')]
param(
    [Parameter(ParameterSetName = 'Check')] [switch]$Check,
    [Parameter(ParameterSetName = 'Cargo', Mandatory)] [string]$Cargo,
    [Parameter(ParameterSetName = 'Game', Mandatory)] [switch]$Game,
    [Parameter(ParameterSetName = 'Device', Mandatory)] [switch]$DeviceCheck,
    [switch]$Local,
    [string]$Ref = '',
    [Parameter(ParameterSetName = 'Game')] [int]$Seconds = 180,
    [Parameter(ParameterSetName = 'Game')] [string]$Keys = '',
    [Parameter(ParameterSetName = 'Game')] [hashtable]$Env = @{},
    [Parameter(ParameterSetName = 'Game')] [string]$Name = '',
    [Parameter(ParameterSetName = 'Game')] [string]$XtpArgs = '',
    [Parameter(ParameterSetName = 'Game')] [switch]$NoBuild,
    [Parameter(ParameterSetName = 'Game')] [switch]$NoGolden,
    [Parameter(ParameterSetName = 'Game')] [switch]$NoCapture,
    [string]$Config
)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$what = $PSCmdlet.ParameterSetName.ToLowerInvariant()
$out = Join-Path $cfg.localLogDir ("{0}-{1}" -f (Get-Date -Format 'yyyyMMdd-HHmmss'), $what)
New-Item -ItemType Directory -Force -Path $out | Out-Null

$zone = Start-XtpVm $cfg
Send-XtpVmToolFolder $cfg $zone

# The code to build: a bundle of the local commit, or the VM clone's origin.
$source = @{}
if ($what -ne 'device') {
    if ($Local) {
        $repo = $script:XtpRepoRoot
        $head = (& git -C $repo rev-parse --short HEAD)
        if (& git -C $repo status --porcelain --untracked-files=no) { Write-Warning "Local changes in $repo are not sent: only commit $head is." }
        $bundle = Join-Path $out 'src.bundle'
        $ErrorActionPreference = 'Continue'
        & git -C $repo bundle create $bundle HEAD 2>&1 | Out-Null
        $ErrorActionPreference = 'Stop'
        if (-not (Test-Path -LiteralPath $bundle)) { throw 'git bundle create failed' }
        Write-Step "Sending commit $head ($([Math]::Round((Get-Item $bundle).Length / 1MB, 1)) MB bundle)"
        Send-XtpVmFile -Cfg $cfg -Zone $zone -Path @($bundle) -Destination 'C:\xtp\jobs'
        Remove-Item -LiteralPath $bundle
        $source = @{ Bundle = 'C:\xtp\jobs\src.bundle' }
    }
    if ($Ref) { $source['Ref'] = $Ref }
}

function Get-RemoteLog([string[]]$Lines) {
    $log = ($Lines | Select-String -Pattern '\(log (C:\\xtp\\logs\\[^)]+)\)' | Select-Object -Last 1).Matches.Groups[1].Value
    if ($log) { Receive-XtpVmFile -Cfg $cfg -Zone $zone -Path $log -Destination $out }
}

$code = 0
switch ($what) {
    'check' {
        Write-Step 'Workspace check on the VM'
        $lines = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'check.ps1' -Arguments ($source + @{ Mode = 'check' }) -AllowFail
        $code = $LASTEXITCODE
        Get-RemoteLog $lines
    }
    'cargo' {
        Write-Step "cargo $Cargo on the VM"
        $lines = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'check.ps1' -Arguments ($source + @{ Mode = 'cargo'; CargoArgs = $Cargo }) -AllowFail
        $code = $LASTEXITCODE
        Get-RemoteLog $lines
    }
    'device' {
        Write-Step 'D3D12 device check on the VM'
        $lines = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'd3d12-check.ps1' -AllowFail
        $code = $LASTEXITCODE
        $lines | Set-Content -LiteralPath (Join-Path $out 'device-check.txt') -Encoding utf8
    }
    'game' {
        if (-not $cfg.titleExe) { throw "Set 'titleExe' in $($cfg.path)" }
        if (-not $NoBuild) {
            Write-Step 'Diagnostics build of xtp on the VM'
            $lines = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'check.ps1' -Arguments ($source + @{ Mode = 'build' }) -AllowFail
            if ($LASTEXITCODE -ne 0) { Get-RemoteLog $lines; throw "The build failed; log in $out" }
        }
        Write-Step "Game run on the VM ($Seconds s)"
        $envText = ($Env.Keys | ForEach-Object { "$_=$($Env[$_])" }) -join ';'
        $lines = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'run-game.ps1' -AllowFail -Arguments @{
            TitleExe = $cfg.titleExe; Seconds = $Seconds; Keys = $Keys; Env = $envText; Name = $Name; XtpArgs = $XtpArgs
            NoGolden = $NoGolden.IsPresent; NoCapture = $NoCapture.IsPresent; SessionUser = $cfg.sessionUser
        }
        $code = $LASTEXITCODE
        $lines | Set-Content -LiteralPath (Join-Path $out 'summary.txt') -Encoding utf8
        $run = ($lines | Select-String -Pattern '^RUN DIR (.+)$' | Select-Object -Last 1).Matches.Groups[1].Value
        if ($run) {
            # The log, the frames and the crash log; not the data folder (saves).
            $zip = "C:\xtp\jobs\$(Split-Path -Leaf $run).zip"
            $pack = "`$r = '$run'; `$items = @('run.log', 'env.txt', 'result.json', 'xtp.toml', 'capture') | ForEach-Object { Join-Path `$r `$_ } | Where-Object { Test-Path `$_ }; " +
                "`$crash = Get-ChildItem -LiteralPath (Join-Path `$r 'data') -Recurse -Filter 'crashes.log' -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName; " +
                "if (`$crash) { Copy-Item -LiteralPath `$crash -Destination (Join-Path `$r 'crashes.log'); `$items += (Join-Path `$r 'crashes.log') }; " +
                "Compress-Archive -Path `$items -DestinationPath '$zip' -Force"
            $null = Invoke-XtpVm -Cfg $cfg -Zone $zone -Script $pack
            Receive-XtpVmFile -Cfg $cfg -Zone $zone -Path $zip -Destination $out
            $null = Invoke-XtpVm -Cfg $cfg -Zone $zone -Script "Remove-Item -LiteralPath '$zip' -Force"
            $local = Join-Path $out (Split-Path -Leaf $zip)
            Expand-Archive -LiteralPath $local -DestinationPath $out -Force
            Remove-Item -LiteralPath $local
        }
    }
}
Write-Host ''
Write-Host "Logs: $out"
exit $code
