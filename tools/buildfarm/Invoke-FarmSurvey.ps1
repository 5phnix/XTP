# A shader survey of one commit on the build farm: builds xtp (release, as on
# the L4) at that commit in a farm slot, runs it there against the VM's own
# copy of the pack (C:\farm\pack\shaders.rpf, see Copy-FarmPack.ps1) and
# brings the output back. No GPU needed; the PC runs nothing but the client.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\Invoke-FarmSurvey.ps1 `
#       -Rev <commit|branch|HEAD> -Args '<xtp arguments>' [-Name <name>] [-Minutes 90] [-WithChanges]
#
#   -Args 'shader-survey --host-lanes 32-32 --json'
#   -Args 'shader-survey --validate --host-lanes 32-32 --json'
#   -Args 'translate --validate --pipelines --host-lanes 32-32'
#
# In -Args, {pack} stands for the VM's pack; without it the pack is added at
# the end. A --json with no file after it (shader-survey) writes the survey's
# JSON, which comes back as survey.json. -Rev defaults to HEAD (the commit,
# not the uncommitted changes); -WithChanges sends the worktree as it is, as
# cargo-remote does. XTP_* variables without a local path go along (e.g.
# XTP_PAIRS).
#
# Results go to <surveyDir>\<name>\ (default E:\...\farm-surveys next to the
# farm's state folder): survey.log (xtp's stdout: the totals), farm.log (the
# build, the farm's messages, xtp's stderr), survey.json, meta.json.
# Several surveys can run at once, from any worktree: each takes an idle VM
# (starting a stopped one if the budget allows it) or a free slot.
#
# Exit code: xtp's (or cargo's); 75 the farm cannot take the job now (never
# run locally instead: no surveys on the PC).

param(
    [string] $Rev = 'HEAD',
    [Alias('Args')] [Parameter(Mandatory = $true)] [string] $XtpArgs,
    [string] $Name = '',
    [int] $Minutes = 0,
    [switch] $WithChanges
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$Pack = 'C:\farm\pack\shaders.rpf'

try {
    $c = Get-FarmConfig
    $cfg = (Get-FarmSshPaths $c).Config
    if (-not (Test-Path -LiteralPath $cfg) -or (Get-Item -LiteralPath $c.configPath).LastWriteTime -gt (Get-Item -LiteralPath $cfg).LastWriteTime) {
        Initialize-FarmSsh $c | Out-Null
    }
    $wt = Get-FarmWorktree (Get-Location).Path
    if ($WithChanges -and $Rev -ne 'HEAD') { throw '-WithChanges sends the worktree as it is: use it with -Rev HEAD' }
    $r = Invoke-FarmGit -Dir $wt.Top -Arguments @('rev-parse', '--verify', '--quiet', "$Rev^{commit}") -AllowFailure
    if ($r.ExitCode -ne 0) { throw "no commit $Rev in $($wt.Top)" }
    $commit = $r.Out.Trim()

    $a = @("$XtpArgs" -split '\s+' | Where-Object { $_ })
    if (-not $a) { throw '-Args is empty: e.g. -Args ''shader-survey --host-lanes 32-32 --json''' }
    $sub = ($a | Where-Object { -not $_.StartsWith('-') } | Select-Object -First 1)

    if (-not $Name) { $Name = '{0}-{1}-{2}' -f (Get-Date -Format 'yyyyMMdd-HHmmss'), $commit.Substring(0, 8), $sub }
    $Name = ($Name -replace '[^A-Za-z0-9._-]', '-').Trim('-.')
    New-Item -ItemType Directory -Force -Path $c.surveyDir | Out-Null
    # A folder of its own, even for two surveys started in the same second.
    $out = $null
    for ($n = 1; -not $out; $n++) {
        $try = Join-Path $c.surveyDir $(if ($n -eq 1) { $Name } else { "$Name-$n" })
        try { New-Item -ItemType Directory -Path $try -ErrorAction Stop | Out-Null; $out = $try; $Name = Split-Path -Leaf $try } catch { if ($n -gt 50) { throw } }
    }

    # The VM's paths: the pack, and the JSON outside the source tree (xtp
    # refuses to write game-derived data inside it), in the farm's own
    # temporary folder.
    $jsonFile = "survey-$Name.json"
    $jsonVm = "C:\farm\tmp\$jsonFile"
    $wantJson = $false
    $xa = @()
    for ($i = 0; $i -lt $a.Count; $i++) {
        $x = $a[$i]
        if ($x -eq '--json' -and ($i + 1 -ge $a.Count -or $a[$i + 1].StartsWith('-') -or $a[$i + 1] -eq '{pack}')) {
            $xa += '--json'
            $x = '{json}'
        }
        if ($x -eq '{json}') { $wantJson = $true }
        $xa += $x
    }
    if ($xa -notcontains '{pack}') { $xa += '{pack}' }
    $xa = @($xa | ForEach-Object { $_.Replace('{pack}', $Pack).Replace('{json}', $jsonVm) })
    $cargoArgs = @('run', '--release', '-p', 'xtp', '--locked', '--') + $xa

    $jobMinutes = $(if ($Minutes -gt 0) { $Minutes } else { [int] $c.surveyMinutes })
    $env:XTP_FARM_JOB_MINUTES = "$jobMinutes"
    $what = $(if ($WithChanges) { "$($commit.Substring(0, 8)) + uncommitted changes" } else { $commit.Substring(0, 8) })
    Write-FarmMessage "survey $Name ($what): xtp $($xa -join ' ')"
    Write-FarmMessage "output: $out (farm.log as it runs)"

    $info = @{}
    $t0 = Get-Date
    $logOut = Join-Path $out 'survey.log'
    $logErr = Join-Path $out 'farm.log'
    $jobArgs = @{
        Config     = $c
        WorkDir    = $wt.Top
        Steps      = @(, $cargoArgs)
        SlotKey    = 'farm-survey'
        Spread     = $true
        StdoutFile = $logOut
        StderrFile = $logErr
        Info       = $info
    }
    if (-not $WithChanges) { $jobArgs.Commit = $commit }
    $code = [int] @(Invoke-FarmJob @jobArgs)[-1]

    if ($wantJson -and $info.vm) {
        # The JSON as a gzipped tar on the SSH channel, then gone from the VM.
        $tgz = Join-Path $out 'survey.json.tgz'
        $r = Invoke-FarmSsh -Config $c -Vm $info.vm -StdoutFile $tgz -TimeoutSeconds 600 -Command "if exist $jsonVm (tar.exe -czf - -C C:\farm\tmp $jsonFile) else (exit 2)"
        if ($r.ExitCode -eq 0) {
            $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
            $x = Invoke-FarmProcess -FilePath $tar -ArgumentList @('-xzf', $tgz, '-C', $out) -Capture -NoStdin
            if ($x.ExitCode -eq 0) {
                Move-Item -Force -LiteralPath (Join-Path $out $jsonFile) -Destination (Join-Path $out 'survey.json')
            } else {
                Write-FarmMessage "unpacking the JSON failed: $($x.Err.Trim())"
                if ($code -eq 0) { $code = 1 }
            }
        } elseif ($code -eq 0) {
            Write-FarmMessage "the JSON did not come back from $($info.vm) (exit $($r.ExitCode))"
            $code = 1
        }
        Remove-Item -Force -LiteralPath $tgz -ErrorAction SilentlyContinue
        $null = Invoke-FarmSsh -Config $c -Vm $info.vm -Capture -Command "if exist $jsonVm del /q $jsonVm"
    }
    $meta = [ordered]@{
        name     = $Name
        rev      = $Rev
        commit   = $commit
        changes  = [bool] $WithChanges
        args     = "xtp $($xa -join ' ')"
        vm       = $info.vm
        slot     = $info.slot
        exit     = $code
        seconds  = [Math]::Round(((Get-Date) - $t0).TotalSeconds)
        started  = (ConvertTo-FarmStamp $t0.ToUniversalTime())
        worktree = $wt.Top
    }
    Write-FarmJson (Join-Path $out 'meta.json') ([pscustomobject] $meta)

    if (Test-Path -LiteralPath $logOut) {
        Get-Content -LiteralPath $logOut | Where-Object { $_ -match '^(pack|validator|host waves|translated|containers|programs|structured|DXIL|             \d|json) ' } |
            ForEach-Object { Write-FarmMessage "  $_" }
    }
    if ($code -ne 0 -and (Test-Path -LiteralPath $logErr) -and (Select-String -LiteralPath $logErr -Pattern 'farm\\pack\\shaders\.rpf.*(os error [23]\b|cannot find|not found)' -Quiet)) {
        Write-FarmMessage "the VM may lack the pack: run tools\buildfarm\Copy-FarmPack.ps1 -Name $($info.vm)"
    }
    Write-FarmMessage ('survey {0}: exit {1} in {2:N0} s on {3} slot {4}; {5}' -f $Name, $code, $meta.seconds, $info.vm, $info.slot, $out)
    exit $code
} catch {
    Write-FarmMessage "unavailable: $($_.Exception.Message)"
    exit $ExitUnavailable
}
