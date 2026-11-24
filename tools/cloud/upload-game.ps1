<#
.SYNOPSIS
  Uploads your game files (and the golden data folder, if configured) from
  your PC to the bucket. Safe to run again: only what changed is sent.

.DESCRIPTION
  `gcloud storage rsync` from gameDir to gs://<bucket>/<gamePrefix>, and from
  goldenDir to gs://<bucket>/<goldenPrefix>/data. The local folders are only
  read. Large files go up as parallel composite uploads (several streams per
  file); gcloud runs at below-normal priority so the PC stays usable, but the
  upload uses your connection's full upstream until it ends.

  Nothing is deleted from the bucket unless -Mirror is given, and -Mirror
  refuses an empty source folder (a wrong path must not wipe the copy).
  The golden folder is always mirrored (it is small, and a stale file in it
  would change every run).

.PARAMETER What
  game, golden or all (default all).

.PARAMETER Processes
  gcloud worker processes (default 4). Threads per process: -Threads (8).

.EXAMPLE
  tools\cloud\upload-game.ps1
  tools\cloud\upload-game.ps1 -What golden
#>
[CmdletBinding()]
param(
    [string]$Config,
    [ValidateSet('game', 'golden', 'all')] [string]$What = 'all',
    [switch]$Mirror,
    [int]$Processes = 4,
    [int]$Threads = 8
)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$base = Get-XtpBaseArgList $cfg

function Get-FolderSize([string]$Dir) {
    $files = @(Get-ChildItem -LiteralPath $Dir -Recurse -File -Force)
    return [pscustomobject]@{ Count = $files.Count; Bytes = [long](($files | Measure-Object -Property Length -Sum).Sum) }
}

function Sync-Folder([string]$Source, [string]$Destination, [bool]$Delete) {
    if (-not (Test-Path -LiteralPath $Source -PathType Container)) { throw "No folder $Source" }
    $size = Get-FolderSize $Source
    if ($size.Count -eq 0) { throw "$Source is empty; check the path in $($cfg.path)" }
    Write-Step ("{0} -> {1}: {2} files, {3:N1} GB" -f $Source, $Destination, $size.Count, ($size.Bytes / 1GB))
    $a = @('storage', 'rsync', $Source, $Destination, '--recursive') + $base
    if ($Delete) { $a += '--delete-unmatched-destination-objects' }
    $started = Get-Date
    $ErrorActionPreference = 'Continue'
    & gcloud @a 2>&1 | ForEach-Object { Write-Host "    $_" }
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    $secs = [Math]::Max(1, ((Get-Date) - $started).TotalSeconds)
    if ($code -ne 0) { throw "gcloud storage rsync failed ($code)" }
    # What the bucket holds now, against the local folder.
    $remote = (Invoke-Gcloud -Arguments (@('storage', 'du', '--summarize', $Destination) + $base) | Select-Object -First 1) -split '\s+' | Select-Object -First 1
    Write-Host ("    done in {0:N0} s; bucket {1:N1} GB, local {2:N1} GB" -f $secs, ([double]$remote / 1GB), ($size.Bytes / 1GB))
    if ([long]$remote -lt $size.Bytes) { Write-Warning "The bucket holds less than the local folder: run this script again." }
}

# Parallel transfers at below-normal priority, for this process only (your
# gcloud configuration is not changed). gcloud keeps its temporary files in
# %TEMP%: point TEMP and TMP elsewhere first if that drive must be spared.
$env:CLOUDSDK_STORAGE_PARALLEL_COMPOSITE_UPLOAD_ENABLED = 'True'
$env:CLOUDSDK_STORAGE_PROCESS_COUNT = "$Processes"
$env:CLOUDSDK_STORAGE_THREAD_COUNT = "$Threads"
try { (Get-Process -Id $PID).PriorityClass = 'BelowNormal' } catch { Write-Verbose "priority unchanged: $_" }

$bucket = "gs://$($cfg.bucket)"
if ($What -in 'game', 'all') {
    if (-not $cfg.gameDir) { throw "Set 'gameDir' in $($cfg.path)" }
    Sync-Folder $cfg.gameDir "$bucket/$($cfg.gamePrefix)" $Mirror.IsPresent
}
if ($What -in 'golden', 'all') {
    if ($cfg.goldenDir) {
        Sync-Folder $cfg.goldenDir "$bucket/$($cfg.goldenPrefix)/data" $true
    } elseif ($What -eq 'golden') {
        throw "Set 'goldenDir' in $($cfg.path)"
    } else {
        Write-Host 'No goldenDir configured: game runs will start from the first-boot screens.'
    }
}
