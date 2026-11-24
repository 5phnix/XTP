# Files of a slot for the client (the PC's Receive-FarmFiles).
#
# The request is base64 JSON: mode ('list' or 'tar'), slot, paths (relative
# to the slot), artifacts (also the executables of the slot's last build),
# pdb (with each executable, its .pdb when there is one).
#   list: prints a JSON array of {path, size, sha256}.
#   tar:  writes a gzipped tar of the paths to stdout.
#
# Windows PowerShell 5.1. ASCII only.

param([Parameter(Mandatory = $true)] [string] $Request)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
. (Join-Path $PSScriptRoot 'FarmVm.ps1')
$Root = Get-FarmRoot
$req = ConvertFrom-FarmBase64 $Request
Test-FarmSlotName $req.slot
$slotDir = Join-Path $Root "w\$($req.slot)"
if (-not (Test-Path $slotDir)) { Write-FarmErr "no slot $($req.slot) on $env:COMPUTERNAME"; exit 2 }

$paths = @(@($req.paths) | Where-Object { $_ })
if ($req.artifacts) {
    $state = Join-Path $Root "state\$($req.slot).artifacts.json"
    if (Test-Path $state) {
        foreach ($exe in @(Get-Content -Raw $state | ConvertFrom-Json)) {
            if (-not $exe) { continue }
            $paths += $exe
            if ($req.pdb) {
                $pdb = [IO.Path]::ChangeExtension($exe, '.pdb')
                # rustc names the .pdb with underscores for a crate with dashes.
                $alt = (Split-Path -Parent $pdb).Replace('\', '/') + '/' + ((Split-Path -Leaf $pdb) -replace '-', '_')
                foreach ($p in $pdb, $alt) {
                    if ((Test-Path (Join-Path $slotDir $p)) -and $paths -notcontains $p) { $paths += $p; break }
                }
            }
        }
    }
}

$items = @()
foreach ($p in $paths) {
    $rel = "$p".Replace('\', '/').TrimStart('/')
    if ($rel -match '(^|/)\.\.(/|$)' -or $rel -match '^[A-Za-z]:') { Write-FarmErr "refusing path $p"; exit 2 }
    $full = Join-Path $slotDir ($rel -replace '/', '\')
    if (-not (Test-Path -LiteralPath $full -PathType Leaf)) { Write-FarmErr "no file $rel in slot $($req.slot)"; exit 2 }
    $items += [pscustomobject]@{
        path   = $rel
        size   = (Get-Item -LiteralPath $full).Length
        sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $full).Hash.ToLowerInvariant()
    }
}

if ($req.mode -eq 'list') {
    [Console]::Out.Write((ConvertTo-Json -InputObject @($items) -Compress))
    exit 0
}
if ($req.mode -ne 'tar') { Write-FarmErr "unknown mode $($req.mode)"; exit 2 }
if (-not $items) { Write-FarmErr 'nothing to send'; exit 2 }

# tar writes straight to this process's stdout (the SSH channel).
$psi = New-FarmStartInfo (Join-Path $env:SystemRoot 'System32\tar.exe') (@('-czf', '-', '-C', $slotDir) + @($items | ForEach-Object { $_.path })) @{} $slotDir
$psi.RedirectStandardInput = $true
$p = [System.Diagnostics.Process]::Start($psi)
$p.StandardInput.Close()
$p.WaitForExit()
exit $p.ExitCode
