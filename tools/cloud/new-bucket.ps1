<#
.SYNOPSIS
  Creates the private bucket that holds your copy of the game files, and the
  VM's service account that may read it. Safe to run again.

.DESCRIPTION
  - The bucket is in the configured region, with uniform bucket-level access
    and public access prevention enforced: no object can ever be made public.
  - The service account (default xtp-vm) gets roles/storage.objectViewer on
    this bucket only; it is the VM's identity, nothing else can use it.
  An existing bucket is checked (region, access settings) and fixed where it
  can be; a bucket in another region is reported, not moved.

.EXAMPLE
  tools\cloud\new-bucket.ps1
#>
[CmdletBinding()]
param([string]$Config)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$base = Get-XtpBaseArgList $cfg
$url = "gs://$($cfg.bucket)"

Write-Step "Bucket $url in $($cfg.region)"
$desc = Invoke-Gcloud -Arguments (@('storage', 'buckets', 'describe', $url, '--format=json') + $base) -AllowFail
if ($LASTEXITCODE -ne 0) {
    if ($global:XtpGcloudError -notmatch '404|not found|does not exist') { throw "Cannot read $url :`n$global:XtpGcloudError" }
    $null = Invoke-Gcloud -Arguments (@('storage', 'buckets', 'create', $url, "--location=$($cfg.region)",
            '--default-storage-class=STANDARD', '--uniform-bucket-level-access', '--public-access-prevention',
            '--labels=app=xtp') + $base)
    $desc = Invoke-Gcloud -Arguments (@('storage', 'buckets', 'describe', $url, '--format=json') + $base)
}
$b = ($desc -join "`n") | ConvertFrom-Json
if ($b.location -ne $cfg.region.ToUpper()) {
    Write-Warning "$url is in $($b.location), not $($cfg.region): the VM would read it across regions (slower, billed)."
}
if (-not $b.uniform_bucket_level_access -or $b.public_access_prevention -ne 'enforced') {
    Write-Step 'Enforcing uniform bucket-level access and public access prevention'
    $null = Invoke-Gcloud -Arguments (@('storage', 'buckets', 'update', $url, '--uniform-bucket-level-access', '--public-access-prevention') + $base)
}

Write-Step "Service account $($cfg.serviceAccountEmail)"
$null = Invoke-Gcloud -Arguments (@('iam', 'service-accounts', 'describe', $cfg.serviceAccountEmail, '--format=value(email)') + $base) -AllowFail
if ($LASTEXITCODE -ne 0) {
    $null = Invoke-Gcloud -Arguments (@('iam', 'service-accounts', 'create', $cfg.serviceAccount,
            '--display-name=XTP cloud VM (reads the game bucket)') + $base)
}

Write-Step 'Read access for the service account on this bucket only'
$policy = (Invoke-Gcloud -Arguments (@('storage', 'buckets', 'get-iam-policy', $url, '--format=json') + $base)) -join "`n" | ConvertFrom-Json
$member = "serviceAccount:$($cfg.serviceAccountEmail)"
$has = $policy.bindings | Where-Object { $_.role -eq 'roles/storage.objectViewer' -and $_.members -contains $member }
if (-not $has) {
    # A just-created service account can take a few seconds to be visible to IAM.
    for ($i = 0; ; $i++) {
        $null = Invoke-Gcloud -Arguments (@('storage', 'buckets', 'add-iam-policy-binding', $url, "--member=$member",
                '--role=roles/storage.objectViewer') + $base) -AllowFail -Quiet
        if ($LASTEXITCODE -eq 0) { break }
        if ($i -ge 6) { throw $global:XtpGcloudError }
        Start-Sleep -Seconds 10
    }
}
# Anything public would be a mistake: say so loudly.
$public = $policy.bindings | Where-Object { $_.members -contains 'allUsers' -or $_.members -contains 'allAuthenticatedUsers' }
if ($public) { throw "$url grants access to allUsers/allAuthenticatedUsers: remove it (gcloud storage buckets remove-iam-policy-binding)." }

Write-Host "Bucket ready: $url ($($b.location), uniform access, public access prevention enforced)."
