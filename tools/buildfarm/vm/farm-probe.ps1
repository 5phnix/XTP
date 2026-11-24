# Answers whether this VM is going away (a Spot reclaim or a shutdown in
# progress): prints "away" or "ok". The client asks after a job that failed,
# since Windows kills the job's processes, the runner included, when it shuts
# down: the exit code alone cannot tell a reclaim from a failed build.
#
# Windows PowerShell 5.1. ASCII only.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
. (Join-Path $PSScriptRoot 'FarmVm.ps1')
if (Test-FarmGoingAway) { [Console]::Out.Write('away') } else { [Console]::Out.Write('ok') }
