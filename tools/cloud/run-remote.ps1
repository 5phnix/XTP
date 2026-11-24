<#
.SYNOPSIS
  Runs XTP's checks, tests or a game run on the VM and brings the logs back.

.DESCRIPTION
  What runs (one of):
    -Check          the workspace check: cargo fmt --check, clippy (default
                    and all features, -D warnings), cargo test (the default)
    -Cargo '<args>' cargo with these arguments, e.g. 'test -p xtp-gpu', and
                    the environment variables of -Env @{...}
    -GpuTests       the GPU test set on the L4 (or -Adapter warp): the tests
                    of -Packages (default xtp-shader, xtp-gpu) and their
                    #[ignore = "hardware: ..."] ones, each in a process of
                    its own, -Parallel at a time (default 8), stopped after
                    -TestTimeout seconds (default 300); those that did not
                    pass run again alone; -Filter <text> keeps the tests
                    whose name holds it (see vm\test-each.ps1). The log,
                    results.csv and the failures' output come back
    -Game           builds the diagnostics xtp (unless -NoBuild) and runs the
                    title for -Seconds in the VM's desktop session, from the
                    golden data folder if the VM has one (see vm\run-game.ps1)
    -DeviceCheck    the D3D12 adapters and what they support

  Which code (for -Check, -Cargo and -Game's build):
    default         origin/main of the VM's clone (or -Ref <commit|branch>)
    -Local          your current commit (HEAD of the repository this script is
                    in), sent to the VM as a git bundle: commit first, local
                    changes are not sent; -Repo <path> and -LocalRev <branch>
                    send another repository's or another branch's commit
                    (e.g. -Repo E:\XTP -LocalRev main, not yet pushed)

  -Game options: -Seconds, -Autopilot <script in the repository> (instead of
  the scripted Enter presses), -Keys, -Env @{...}, -XtpArgs, -Name,
  -NoBuild, -NoGolden, -NoCapture. The VM's GPU memory and use are sampled
  every 2 s during the run (gpu.csv from nvidia-smi, vidmm.csv from
  Windows' GPU Adapter Memory counters).

  The VM is started if needed and stops by itself once idle. One build and
  one game run at a time: a second caller waits. Logs go to
  <localLogDir>\<date-time>-<what>\ (default %LOCALAPPDATA%\xtp\cloud-runs).

.EXAMPLE
  tools\cloud\run-remote.ps1 -Check -Local
  tools\cloud\run-remote.ps1 -Cargo 'test -p xtp-gpu --locked' -Local
  tools\cloud\run-remote.ps1 -Cargo 'test -p xtp-shader --locked' -Local -Env @{ XTP_REQUIRE_GPU = '1' }
  tools\cloud\run-remote.ps1 -GpuTests -Local
  tools\cloud\run-remote.ps1 -GpuTests -Local -Adapter warp
  tools\cloud\run-remote.ps1 -Game -Seconds 240 -Local
  tools\cloud\run-remote.ps1 -Game -NoBuild -Env @{ XTP_LOG = 'xtp_gpu=debug,info' }
  tools\cloud\run-remote.ps1 -Game -Local -Repo E:\XTP -LocalRev main -Autopilot autopilot\gameplay.txt -Seconds 300
#>
[CmdletBinding(DefaultParameterSetName = 'Check')]
param(
    [Parameter(ParameterSetName = 'Check')] [switch]$Check,
    [Parameter(ParameterSetName = 'Cargo', Mandatory)] [string]$Cargo,
    [Parameter(ParameterSetName = 'Game', Mandatory)] [switch]$Game,
    [Parameter(ParameterSetName = 'Tests', Mandatory)] [switch]$GpuTests,
    [Parameter(ParameterSetName = 'Device', Mandatory)] [switch]$DeviceCheck,
    [Parameter(ParameterSetName = 'Tests')] [string[]]$Packages = @('xtp-shader', 'xtp-gpu'),
    [Parameter(ParameterSetName = 'Tests')] [ValidateSet('hardware', 'warp')] [string]$Adapter = 'hardware',
    [Parameter(ParameterSetName = 'Tests')] [int]$TestTimeout = 300,
    [Parameter(ParameterSetName = 'Tests')] [int]$Parallel = 8,
    [Parameter(ParameterSetName = 'Tests')] [string]$Filter = '',
    [switch]$Local,
    [string]$Repo = '',
    [string]$LocalRev = 'HEAD',
    [string]$Ref = '',
    [Parameter(ParameterSetName = 'Game')] [string]$Autopilot = '',
    [Parameter(ParameterSetName = 'Game')] [int]$Seconds = 180,
    [Parameter(ParameterSetName = 'Game')] [string]$Keys = '',
    [Parameter(ParameterSetName = 'Game')] [Parameter(ParameterSetName = 'Cargo')] [hashtable]$Env = @{},
    [Parameter(ParameterSetName = 'Game')] [string]$Name = '',
    [Parameter(ParameterSetName = 'Game')] [string]$XtpArgs = '',
    [Parameter(ParameterSetName = 'Game')] [switch]$NoBuild,
    [Parameter(ParameterSetName = 'Game')] [switch]$NoGolden,
    [Parameter(ParameterSetName = 'Game')] [switch]$NoCapture,
    # Fail at once when another session holds the VM, instead of waiting.
    [switch]$NoWait,
    [string]$Config
)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$what = $PSCmdlet.ParameterSetName.ToLowerInvariant()
$out = Join-Path $cfg.localLogDir ("{0}-{1}" -f (Get-Date -Format 'yyyyMMdd-HHmmss'), $what)
New-Item -ItemType Directory -Force -Path $out | Out-Null

$zone = Start-XtpVm $cfg
Send-XtpVmToolFolder $cfg $zone

# Who runs this, shown to whoever waits for the VM: the Hangar pane (or the
# user) and the worktree.
$who = if ($env:HANGAR_PANE_NAME) { $env:HANGAR_PANE_NAME } else { $env:USERNAME }
$owner = "$who ($(Split-Path -Leaf $(if ($Repo) { (Resolve-Path -LiteralPath $Repo).Path } else { $script:XtpRepoRoot })), $env:COMPUTERNAME)"

# The code to build: a bundle of the local commit, or the VM clone's origin.
$source = @{}
if ($what -ne 'device') {
    if ($Local) {
        $repoDir = if ($Repo) { (Resolve-Path -LiteralPath $Repo).Path } else { $script:XtpRepoRoot }
        $head = (& git -C $repoDir rev-parse --short $LocalRev)
        if (-not $head) { throw "No commit $LocalRev in $repoDir" }
        $full = (& git -C $repoDir rev-parse $LocalRev)
        if ($LocalRev -eq 'HEAD' -and (& git -C $repoDir status --porcelain --untracked-files=no)) {
            Write-Warning "Local changes in $repoDir are not sent: only commit $head is."
        }
        # A name of its own: callers wait for the VM's lock in turn, and a
        # shared name let a later caller's bundle replace a waiting one's.
        $bundleName = "src-$(Split-Path -Leaf $out)-$PID.bundle"
        $bundle = Join-Path $out $bundleName
        $ErrorActionPreference = 'Continue'
        & git -C $repoDir bundle create $bundle $LocalRev 2>&1 | Out-Null
        $ErrorActionPreference = 'Stop'
        if (-not (Test-Path -LiteralPath $bundle)) { throw 'git bundle create failed' }
        Write-Step "Sending commit $head ($([Math]::Round((Get-Item $bundle).Length / 1MB, 1)) MB bundle)"
        Send-XtpVmFile -Cfg $cfg -Zone $zone -Path @($bundle) -Destination 'C:\xtp\jobs'
        Remove-Item -LiteralPath $bundle
        # The VM checks that it builds this commit (vm\lib.ps1, Set-XtpSource).
        $source = @{ Bundle = "C:\xtp\jobs\$bundleName"; Commit = $full }
    }
    if ($Ref) { $source['Ref'] = $Ref; $source.Remove('Commit') }
    $source['Owner'] = $owner
    $source['NoWait'] = $NoWait.IsPresent
}

function Get-RemoteLog([string[]]$Lines) {
    $log = ($Lines | Select-String -Pattern '\(log (C:\\xtp\\logs\\[^)]+)\)' | Select-Object -Last 1).Matches.Groups[1].Value
    if ($log) { Receive-XtpVmFile -Cfg $cfg -Zone $zone -Path $log -Destination $out }
}

$envText = ($Env.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ';'
$code = 0
switch ($what) {
    'check' {
        Write-Step 'Workspace check on the VM'
        $lines = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'check.ps1' -Arguments ($source + @{ Mode = 'check'; SessionUser = $cfg.sessionUser }) -AllowFail
        $code = $LASTEXITCODE
        Get-RemoteLog $lines
    }
    'cargo' {
        Write-Step "cargo $Cargo on the VM"
        $lines = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'check.ps1' -Arguments ($source + @{ Mode = 'cargo'; CargoArgs = $Cargo; Env = $envText; SessionUser = $cfg.sessionUser }) -AllowFail
        $code = $LASTEXITCODE
        Get-RemoteLog $lines
    }
    'tests' {
        Write-Step "GPU tests of $($Packages -join ', ') on the VM ($Adapter, each test alone, $TestTimeout s at most)"
        $lines = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'check.ps1' -AllowFail -Arguments ($source + @{
                Mode = 'tests'; Packages = ($Packages -join ','); Adapter = $Adapter; TestTimeout = $TestTimeout
                Parallel = $Parallel; Filter = $Filter; SessionUser = $cfg.sessionUser
            })
        $code = $LASTEXITCODE
        # The run's folder: run.log, results.csv, output\ of the tests that did not pass.
        $log = ($lines | Select-String -Pattern '\(log (C:\\xtp\\logs\\[^)]+)\\run\.log\)' | Select-Object -Last 1).Matches.Groups[1].Value
        if ($log) { Receive-XtpVmFile -Cfg $cfg -Zone $zone -Path $log -Destination $out -Recurse }
    }
    'device' {
        Write-Step 'D3D12 device check on the VM'
        $lines = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'd3d12-check.ps1' -AllowFail
        $code = $LASTEXITCODE
        $lines | Set-Content -LiteralPath (Join-Path $out 'device-check.txt') -Encoding utf8
    }
    'game' {
        if (-not $cfg.titleExe) { throw "Set 'titleExe' in $($cfg.path)" }
        Write-Step "$(if ($NoBuild) { '' } else { 'Diagnostics build, then ' })game run on the VM ($Seconds s)"
        if ($Autopilot) {
            # The script drives the title; no scripted Enter presses beside it.
            $XtpArgs = "$XtpArgs --autopilot $Autopilot".Trim()
            if (-not $Keys) { $Keys = '-' }
        }
        # One call: the VM keeps the clone locked from the build to the end of the run.
        $lines = Invoke-XtpVmTool -Cfg $cfg -Zone $zone -Name 'run-game.ps1' -AllowFail -Arguments ($source + @{
                TitleExe = $cfg.titleExe; Seconds = $Seconds; Keys = $Keys; Env = $envText; Name = $Name; XtpArgs = $XtpArgs
                Build = -not $NoBuild; NoGolden = $NoGolden.IsPresent; NoCapture = $NoCapture.IsPresent; SessionUser = $cfg.sessionUser
            })
        $code = $LASTEXITCODE
        if ($lines -match '^build FAILED') { Get-RemoteLog $lines }
        $lines | Set-Content -LiteralPath (Join-Path $out 'summary.txt') -Encoding utf8
        $run = ($lines | Select-String -Pattern '^RUN DIR (.+)$' | Select-Object -Last 1).Matches.Groups[1].Value
        if ($run) {
            # The log, the frames and the crash log; not the data folder (saves).
            $zip = "C:\xtp\jobs\$(Split-Path -Leaf $run).zip"
            $pack = "`$r = '$run'; `$items = @('run.log', 'env.txt', 'result.json', 'xtp.toml', 'gpu.csv', 'vidmm.csv', 'capture') | ForEach-Object { Join-Path `$r `$_ } | Where-Object { Test-Path `$_ }; " +
                "`$crash = Get-ChildItem -LiteralPath (Join-Path `$r 'data') -Recurse -Filter 'crashes.log' -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName; " +
                "if (`$crash) { Copy-Item -LiteralPath `$crash -Destination (Join-Path `$r 'crashes.log'); `$items += (Join-Path `$r 'crashes.log') }; " +
                "Compress-Archive -Path `$items -DestinationPath '$zip' -Force"
            $null = Invoke-XtpVm -Cfg $cfg -Zone $zone -Script $pack
            Receive-XtpVmFile -Cfg $cfg -Zone $zone -Path $zip -Destination $out
            $null = Invoke-XtpVm -Cfg $cfg -Zone $zone -Script "Remove-Item -LiteralPath '$zip' -Force"
            $localZip = Join-Path $out (Split-Path -Leaf $zip)
            Expand-Archive -LiteralPath $localZip -DestinationPath $out -Force
            Remove-Item -LiteralPath $localZip
        }
    }
}
Write-Host ''
Write-Host "Logs: $out"
exit $code
