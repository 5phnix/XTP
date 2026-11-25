# vm-pack.ps1: the farm VM's side of Copy-FarmPack.ps1 (not part of the farm
# agent: the client sends this text over SSH each time, after a line that
# sets $Request).
#
# $Request is base64 JSON: mode ('status' or 'copy'); for 'copy' also url (a
# signed URL valid for a few minutes, for the one object), source (gs://...),
# size, crc32c (as GCS gives it: base64 of the big-endian CRC32C) and
# generation. The pack goes to C:\farm\pack\shaders.rpf, with what was copied
# in shaders.rpf.json beside it. Prints one JSON object: the pack's state and
# the Windows SDK's dxil.dll files (the validator `xtp --validate` loads is
# the newest one).
#
# Windows PowerShell 5.1. ASCII only.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$req = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Request)) | ConvertFrom-Json
$dir = 'C:\farm\pack'
$file = Join-Path $dir 'shaders.rpf'
$stampPath = "$file.json"

if (-not ('FarmCrc32C' -as [type])) {
    Add-Type -TypeDefinition @'
public static class FarmCrc32C {
    static readonly uint[] Table = Make();
    static uint[] Make() {
        var t = new uint[256];
        for (uint i = 0; i < 256; i++) {
            uint c = i;
            for (int k = 0; k < 8; k++) c = (c & 1) != 0 ? (c >> 1) ^ 0x82F63B78u : c >> 1;
            t[i] = c;
        }
        return t;
    }
    // Base64 of the big-endian CRC32C, as Cloud Storage reports it.
    public static string OfFile(string path) {
        uint c = 0xFFFFFFFFu;
        var buf = new byte[1 << 20];
        using (var f = System.IO.File.OpenRead(path)) {
            int n;
            while ((n = f.Read(buf, 0, buf.Length)) > 0) {
                for (int i = 0; i < n; i++) c = Table[(c ^ buf[i]) & 0xFF] ^ (c >> 8);
            }
        }
        c ^= 0xFFFFFFFFu;
        return System.Convert.ToBase64String(new byte[] { (byte)(c >> 24), (byte)(c >> 16), (byte)(c >> 8), (byte)c });
    }
}
'@
}

function Get-PackState {
    $stamp = $null
    if (Test-Path -LiteralPath $stampPath) { try { $stamp = Get-Content -Raw -LiteralPath $stampPath | ConvertFrom-Json } catch { $stamp = $null } }
    $size = $(if (Test-Path -LiteralPath $file) { (Get-Item -LiteralPath $file).Length } else { -1 })
    $dxil = @()
    $kits = 'C:\Program Files (x86)\Windows Kits\10\bin'
    foreach ($d in @(Get-ChildItem -LiteralPath $kits -Directory -ErrorAction SilentlyContinue | Sort-Object Name)) {
        $p = Join-Path $d.FullName 'x64\dxil.dll'
        if (Test-Path -LiteralPath $p) {
            $v = (Get-Item -LiteralPath $p).VersionInfo
            $dxil += [pscustomobject]@{ path = $p; fileVersion = "$($v.FileVersion)".Trim(); productVersion = "$($v.ProductVersion)".Trim() }
        }
    }
    [pscustomobject]@{
        vm      = $(if (Test-Path 'C:\farm\state\instance-name') { (Get-Content -Raw 'C:\farm\state\instance-name').Trim() } else { $env:COMPUTERNAME })
        path    = $file
        size    = $size
        stamp   = $stamp
        freeGb  = [Math]::Round((Get-PSDrive -Name C).Free / 1GB, 1)
        dxil    = @($dxil)
        copied  = $false
        seconds = 0
    }
}

if ($req.mode -eq 'status') {
    [Console]::Out.Write((ConvertTo-Json -InputObject (Get-PackState) -Depth 4 -Compress))
    return
}
if ($req.mode -ne 'copy') { throw "unknown mode $($req.mode)" }

$state = Get-PackState
if ($state.size -eq [int64] $req.size -and $state.stamp -and $state.stamp.crc32c -eq $req.crc32c -and "$($state.stamp.generation)" -eq "$($req.generation)") {
    [Console]::Out.Write((ConvertTo-Json -InputObject $state -Depth 4 -Compress))
    return
}
if ($state.freeGb -lt ([double] $req.size / 1GB + 5)) { throw "only $($state.freeGb) GB free on C:" }
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$part = "$file.part"
Remove-Item -Force -LiteralPath $part -ErrorAction SilentlyContinue
$t0 = Get-Date
# curl.exe directly (no cmd.exe): the URL's & and % pass untouched.
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = Join-Path $env:SystemRoot 'System32\curl.exe'
$psi.Arguments = '-fsS --retry 3 --retry-delay 5 --connect-timeout 30 --max-time 1200 -o "' + $part + '" "' + $req.url + '"'
$psi.UseShellExecute = $false
$psi.RedirectStandardError = $true
$p = [System.Diagnostics.Process]::Start($psi)
$err = $p.StandardError.ReadToEnd()
$p.WaitForExit()
if ($p.ExitCode -ne 0) {
    Remove-Item -Force -LiteralPath $part -ErrorAction SilentlyContinue
    throw "download failed (curl $($p.ExitCode)): $($err.Trim())"
}
$got = (Get-Item -LiteralPath $part).Length
if ($got -ne [int64] $req.size) {
    Remove-Item -Force -LiteralPath $part
    throw "download incomplete: $got of $($req.size) bytes"
}
$crc = [FarmCrc32C]::OfFile($part)
if ($crc -ne $req.crc32c) {
    Remove-Item -Force -LiteralPath $part
    throw "CRC32C mismatch: $crc here, $($req.crc32c) in the bucket"
}
Move-Item -Force -LiteralPath $part -Destination $file
$stamp = [pscustomobject]@{ source = $req.source; generation = "$($req.generation)"; size = $got; crc32c = $crc; copied = (Get-Date).ToUniversalTime().ToString('o') }
[IO.File]::WriteAllText($stampPath, (ConvertTo-Json -InputObject $stamp))
$state = Get-PackState
$state.copied = $true
$state.seconds = [Math]::Round(((Get-Date) - $t0).TotalSeconds, 1)
[Console]::Out.Write((ConvertTo-Json -InputObject $state -Depth 4 -Compress))
