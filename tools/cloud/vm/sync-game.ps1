<#
  Runs ON THE VM, over SSH (sync-game.ps1 on your PC calls it). Windows PowerShell 5.1.

  Copies the game files from the bucket to D:\game, and the golden data
  folder (if the bucket has one) to D:\golden\data, with `gcloud storage
  rsync` as the VM's service account (read-only on the bucket). Only what
  changed is copied. D:\golden\data is mirrored (files the bucket no longer
  has are deleted); D:\game too with -Mirror.
#>
param(
    [Parameter(Mandatory)] [string]$Bucket,
    [string]$GamePrefix = 'game',
    [string]$GoldenPrefix = 'golden',
    [switch]$Mirror
)
$ErrorActionPreference = 'Stop'
$env:CLOUDSDK_STORAGE_PROCESS_COUNT = '16'
$env:CLOUDSDK_STORAGE_THREAD_COUNT = '8'

function Sync([string]$From, [string]$To, [bool]$Delete) {
    New-Item -ItemType Directory -Force -Path $To | Out-Null
    $t0 = Get-Date
    $a = @('storage', 'rsync', $From, $To, '--recursive', '--quiet')
    if ($Delete) { $a += '--delete-unmatched-destination-objects' }
    Write-Output "--> $From -> $To"
    $ErrorActionPreference = 'Continue'
    & gcloud @a 2>&1 | ForEach-Object { "    $_" }
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -ne 0) { throw "gcloud storage rsync $From failed ($code)" }
    $files = @(Get-ChildItem -LiteralPath $To -Recurse -File -Force)
    $bytes = ($files | Measure-Object -Property Length -Sum).Sum
    $secs = [Math]::Max(1, ((Get-Date) - $t0).TotalSeconds)
    Write-Output ("    {0} files, {1:N1} GB on disk; {2:N0} s" -f $files.Count, ($bytes / 1GB), $secs)
}

Sync "gs://$Bucket/$GamePrefix" 'D:\game' $Mirror.IsPresent
$ErrorActionPreference = 'Continue'
$golden = & gcloud storage ls "gs://$Bucket/$GoldenPrefix/data/" 2>$null
$ErrorActionPreference = 'Stop'
if ($golden) {
    Sync "gs://$Bucket/$GoldenPrefix/data" 'D:\golden\data' $true
} else {
    Write-Output '--> no golden data folder in the bucket: game runs start from the first-boot screens'
}
$vol = Get-Volume -DriveLetter D
Write-Output ("D: {0:N0} GB free of {1:N0} GB" -f ($vol.SizeRemaining / 1GB), ($vol.Size / 1GB))
exit 0
