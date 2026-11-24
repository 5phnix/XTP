<#
  Runs ON THE VM beside a game run: run-game.ps1 -AudioLockStopgap starts it
  (run-remote.ps1 -Game -AudioLockStopgap). Windows PowerShell 5.1.

  A stopgap for stall B of docs/analysis/l4-world.md (sections 6.4 and 8.1),
  until XTP gives titles a silent audio output: this VM has no audio render
  endpoint, the title's audio output opens with no stream, and its per-frame
  audio call (T+0x5267720) then returns with its device's mutex held, so the
  world never loads. This does to the run what S8 did (sections 6.6 and 6.7),
  in memory only, nothing to the title's files:
  1. waits for the run's xtp.exe (the first one started after -After) and
     attaches cdb (C:\xtp\jobs\cdb\cdb.exe) to it at once, with every
     exception left to XTP's own handlers;
  2. at the first call of T+0x5267720 (the code is mapped, its je has not run
     yet), rewrites the displacement of the je at T+0x5267747 so that, with
     no stream, it leads to the function's unlocking exit T+0x5267c7f instead
     of its plain return T+0x5267cfb; with streams the je is not taken, so
     the change does nothing. Then cdb detaches;
  3. if cdb did not (no cdb, the attach failed), writes the same
     displacement itself once the image is mapped.
  Only for build 1.0.2.1 (the RVAs are its), and only over the expected
  bytes. The result goes to -Log; its last line starts with "STOPGAP".
#>
param(
    [Parameter(Mandatory)] [string]$Log,
    # Round-trip date ('o'): the run's xtp.exe is the first one started after it.
    [Parameter(Mandatory)] [string]$After,
    [string]$GameDir = 'D:\game',
    [string]$Cdb = 'C:\xtp\jobs\cdb\cdb.exe',
    # How long xtp.exe may take to start, and the image to be mapped.
    [int]$StartSeconds = 900,
    [int]$MapSeconds = 120
)
$ErrorActionPreference = 'Stop'
$build = '1.0.2.1'
$module = 'xtp_image_b99f876b74d4a77f'   # as cdb names xtp-image-b99f876b74d4a77f.dll (that build)
$imageFile = 'xtp-image-b99f876b74d4a77f.dll'
$site = 0x5267747                        # je rel32 to T+0x5267cfb
$original = [byte[]](0x0f, 0x84, 0xae, 0x05, 0x00, 0x00)
$patched = [byte[]](0x0f, 0x84, 0x32, 0x05, 0x00, 0x00)   # T+0x5267c7f - T+0x526774d

$t0 = $null
function Say([string]$Text) {
    $at = if ($t0) { ' +{0:N1} s' -f ((Get-Date) - $t0).TotalSeconds } else { '' }
    Add-Content -LiteralPath $Log -Value ('{0:HH:mm:ss.fff}{1} {2}' -f (Get-Date), $at, $Text) -Encoding UTF8
}
function Finish([string]$Text) { Say "STOPGAP $Text"; exit 0 }

$config = Join-Path $GameDir 'MicrosoftGame.Config'
$version = if (Test-Path -LiteralPath $config) { ([xml](Get-Content -LiteralPath $config -Raw)).Game.Identity.Version } else { '' }
if ($version -ne $build) { Finish "skipped: the title is build '$version', the stopgap's RVAs are build $build's" }

$haveCdb = Test-Path -LiteralPath $Cdb
$cmds = Join-Path (Split-Path -Parent $Log) 'audio-lock-cdb-cmds.txt'
$cdbOut = Join-Path (Split-Path -Parent $Log) 'audio-lock-cdb.txt'
if ($haveCdb) {
    # The breakpoint has no `gc`: cdb stops after it and reads the next line, `qd` (detach).
    @"
.symopt+0x40
sxi ii
sxi av
sxi gp
sxi eh
sxi ch
sxi dz
sxi out
sxi sov
sxi *
bu $module+0x5267740 ".printf \"@@HIT tid %x streams %d\\n\", @`$tid, dwo(@rdi+0x52c); .if ((dwo($module+0x5267747) == 0x05ae840f) & (dwo($module+0x5267749) == 0x5ae)) { ed $module+0x5267749 0x532; .printf \"@@PATCHED\\n\" } .elsif (dwo($module+0x5267749) == 0x532) { .printf \"@@ALREADY\\n\" } .else { .printf \"@@UNEXPECTED\\n\" }; u $module+0x5267740 L3; bc *"
g
qd
"@ | Set-Content -LiteralPath $cmds -Encoding ASCII
} else {
    Say "no ${Cdb}: the patch will be written from here once the image is mapped"
}

Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class XtpStopgapMem {
  [DllImport("kernel32.dll", SetLastError=true)] public static extern IntPtr OpenProcess(uint a, bool i, int pid);
  [DllImport("kernel32.dll", SetLastError=true)] public static extern bool ReadProcessMemory(IntPtr h, IntPtr addr, byte[] buf, IntPtr size, out IntPtr n);
  [DllImport("kernel32.dll", SetLastError=true)] public static extern bool WriteProcessMemory(IntPtr h, IntPtr addr, byte[] buf, IntPtr size, out IntPtr n);
  [DllImport("kernel32.dll", SetLastError=true)] public static extern bool VirtualProtectEx(IntPtr h, IntPtr addr, IntPtr size, uint prot, out uint old);
  [DllImport("kernel32.dll", SetLastError=true)] public static extern bool FlushInstructionCache(IntPtr h, IntPtr addr, IntPtr size);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr h);
}
'@

# 1. The run's xtp.exe, as soon as it exists.
$since = [datetime]::Parse($After, $null, [Globalization.DateTimeStyles]::RoundtripKind).ToLocalTime()
$deadline = (Get-Date).AddSeconds($StartSeconds)
$p = $null
while (-not $p) {
    $p = [System.Diagnostics.Process]::GetProcessesByName('xtp') | Where-Object {
        try { $_.StartTime -ge $since } catch { $false }
    } | Sort-Object StartTime | Select-Object -First 1
    if ($p) { break }
    if ((Get-Date) -gt $deadline) { Finish "not applied: no xtp.exe started within $StartSeconds s" }
    Start-Sleep -Milliseconds 20
}
$t0 = $p.StartTime
$dbg = $null
if ($haveCdb) {
    $dbg = Start-Process -FilePath $Cdb -ArgumentList @('-p', "$($p.Id)", '-cf', "`"$cmds`"") -PassThru `
        -RedirectStandardOutput $cdbOut -RedirectStandardError "$cdbOut.err"
    Say "xtp.exe pid $($p.Id): cdb attaching (pid $($dbg.Id))"
} else {
    Say "xtp.exe pid $($p.Id)"
}

# 2. The image's base, for the check (and the patch if cdb does not make it).
function Get-ImageBase {
    try {
        $m = (Get-Process -Id $p.Id -ErrorAction Stop).Modules | Where-Object { $_.ModuleName -eq $imageFile } | Select-Object -First 1
        if ($m) { return [Int64]$m.BaseAddress }
    } catch { }
    return [Int64]0
}
$base = [Int64]0
while ($base -eq 0 -and ((Get-Date) - $t0).TotalSeconds -lt $MapSeconds) {
    if ($p.HasExited) { Finish 'not applied: xtp.exe ended before the title image was mapped' }
    Start-Sleep -Milliseconds 200
    $base = Get-ImageBase
}
if ($base -eq 0) { Finish "not applied: no $imageFile in xtp.exe within $MapSeconds s (another build?)" }
Say ('image mapped at 0x{0:x}' -f $base)

function Read-Site {
    $h = [XtpStopgapMem]::OpenProcess(0x0010, $false, $p.Id)   # VM_READ
    $buf = New-Object byte[] 6; $n = [IntPtr]::Zero
    $ok = [XtpStopgapMem]::ReadProcessMemory($h, [IntPtr]($base + $site), $buf, [IntPtr]6, [ref]$n)
    [void][XtpStopgapMem]::CloseHandle($h)
    if (-not $ok) { return '' }
    return ($buf | ForEach-Object { $_.ToString('x2') }) -join ' '
}
$hexOriginal = ($original | ForEach-Object { $_.ToString('x2') }) -join ' '
$hexPatched = ($patched | ForEach-Object { $_.ToString('x2') }) -join ' '

# 3. cdb's patch: it detaches after the first call; it stays only if the title never makes it.
if ($dbg) {
    while (-not $dbg.HasExited -and -not $p.HasExited) { Start-Sleep -Milliseconds 200 }
    if (-not $dbg.HasExited) { $null = $dbg.WaitForExit(30000) }
    $said = @(Get-Content -LiteralPath $cdbOut -ErrorAction SilentlyContinue | Where-Object { $_ -match '^@@' })
    $said | ForEach-Object { Say "cdb: $_" }
    Get-Content -LiteralPath "$cdbOut.err" -ErrorAction SilentlyContinue | Select-Object -Last 5 | ForEach-Object { Say "cdb error: $_" }
    if ($p.HasExited -and -not ($said -match '@@HIT')) { Finish 'not applied: xtp.exe ended before the title made its first audio call' }
    $now = Read-Site
    if ($said -match '@@PATCHED|@@ALREADY') {
        if ($now -eq $hexPatched) { Finish ('applied by cdb at the first audio call ({0})' -f (($said | Where-Object { $_ -match '@@HIT' }) -replace '^@@HIT ', '')) }
        Finish "FAILED: cdb reported the patch but the site holds $now"
    }
    if ($said -match '@@UNEXPECTED') { Finish "not applied: unexpected bytes at T+0x5267747 ($now)" }
    Say 'cdb exited without the patch: writing it from here'
}

# 4. No cdb, or it did not patch: write the displacement from here (later than cdb would).
$now = Read-Site
if ($now -eq $hexPatched) { Finish 'already applied' }
if ($now -ne $hexOriginal) { Finish "not applied: unexpected bytes at T+0x5267747 ($now)" }
$h = [XtpStopgapMem]::OpenProcess(0x0438, $false, $p.Id)   # VM_OPERATION, VM_READ, VM_WRITE, QUERY_INFORMATION
$at = [IntPtr]($base + $site); $old = [uint32]0; $n = [IntPtr]::Zero
[void][XtpStopgapMem]::VirtualProtectEx($h, $at, [IntPtr]6, 0x40, [ref]$old)
$ok = [XtpStopgapMem]::WriteProcessMemory($h, $at, $patched, [IntPtr]6, [ref]$n)
[void][XtpStopgapMem]::VirtualProtectEx($h, $at, [IntPtr]6, $old, [ref]$old)
[void][XtpStopgapMem]::FlushInstructionCache($h, $at, [IntPtr]6)
[void][XtpStopgapMem]::CloseHandle($h)
if ($ok -and (Read-Site) -eq $hexPatched) { Finish 'applied from here (no cdb patch; the first audio call may have come first)' }
Finish 'FAILED: WriteProcessMemory did not take'
