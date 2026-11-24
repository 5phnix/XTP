# FarmVm.ps1: helpers of the scripts that run on a build farm VM.
# Windows PowerShell 5.1. ASCII only.

Set-StrictMode -Off
$ErrorActionPreference = 'Stop'

$script:Root = 'C:\farm'

function Get-FarmRoot { $script:Root }

function Write-FarmErr([string] $Message) {
    [Console]::Error.WriteLine("farm: $Message")
}

# Quotes one argument the way CommandLineToArgvW and the C runtime read it.
function ConvertTo-FarmArg([string] $Arg) {
    if ($Arg.Length -gt 0 -and $Arg -notmatch '[\s"]') { return $Arg }
    $sb = New-Object System.Text.StringBuilder
    [void] $sb.Append('"')
    $bs = 0
    foreach ($ch in $Arg.ToCharArray()) {
        if ($ch -eq [char] '\') { $bs++; continue }
        if ($ch -eq [char] '"') {
            [void] $sb.Append('\' * (2 * $bs + 1)).Append('"')
            $bs = 0
            continue
        }
        if ($bs) { [void] $sb.Append('\' * $bs); $bs = 0 }
        [void] $sb.Append($ch)
    }
    [void] $sb.Append('\' * (2 * $bs)).Append('"')
    $sb.ToString()
}

function New-FarmStartInfo([string] $FilePath, [string[]] $ArgumentList, [hashtable] $Environment, [string] $WorkingDirectory) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $FilePath
    $psi.Arguments = (@($ArgumentList | ForEach-Object { ConvertTo-FarmArg $_ }) -join ' ')
    $psi.UseShellExecute = $false
    if ($WorkingDirectory) { $psi.WorkingDirectory = $WorkingDirectory }
    if ($Environment) {
        foreach ($k in $Environment.Keys) {
            if ($null -eq $Environment[$k]) { $psi.EnvironmentVariables.Remove($k) } else { $psi.EnvironmentVariables[$k] = [string] $Environment[$k] }
        }
    }
    $psi
}

# Runs a program and returns its exit code and output (stdin closed).
function Invoke-FarmCapture([string] $FilePath, [string[]] $ArgumentList, [hashtable] $Environment = @{}, [string] $WorkingDirectory = '') {
    $psi = New-FarmStartInfo $FilePath $ArgumentList $Environment $WorkingDirectory
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = New-Object Text.UTF8Encoding $false
    $psi.StandardErrorEncoding = New-Object Text.UTF8Encoding $false
    $p = [System.Diagnostics.Process]::Start($psi)
    try {
        $p.StandardInput.Close()
        $o = $p.StandardOutput.ReadToEndAsync()
        $e = $p.StandardError.ReadToEndAsync()
        $p.WaitForExit()
        [pscustomobject]@{ ExitCode = $p.ExitCode; Out = $o.Result; Err = $e.Result }
    } finally {
        $p.Dispose()
    }
}

function Stop-FarmTree([int] $ProcessId) {
    & taskkill.exe /PID $ProcessId /T /F 2>&1 | Out-Null
}

# This process's ssh session: the shell that started it and the sshd process
# of the connection. When one of them is gone the client has left.
function Get-FarmSessionPids {
    $pids = @()
    $cur = $PID
    for ($i = 0; $i -lt 5; $i++) {
        $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$cur" -ErrorAction SilentlyContinue
        if (-not $proc) { break }
        $parent = Get-CimInstance Win32_Process -Filter "ProcessId=$($proc.ParentProcessId)" -ErrorAction SilentlyContinue
        if (-not $parent) { break }
        $name = "$($parent.Name)".ToLowerInvariant()
        if (@('services.exe', 'wininit.exe', 'svchost.exe', 'explorer.exe') -contains $name) { break }
        if ($name -like 'sshd*') {
            $gp = Get-CimInstance Win32_Process -Filter "ProcessId=$($parent.ParentProcessId)" -ErrorAction SilentlyContinue
            if ($gp -and "$($gp.Name)".ToLowerInvariant() -eq 'services.exe') { break }
            $pids += [int] $parent.ProcessId
            break
        }
        $pids += [int] $parent.ProcessId
        $cur = $parent.ProcessId
    }
    $pids
}

# Runs a program with this process's stdout and stderr (output streams to
# the client), stops its whole tree when it runs over MaxMinutes or when the
# ssh session goes away. 124: over time; 125: client gone.
function Invoke-FarmWatched {
    param([string] $FilePath, [string[]] $ArgumentList, [hashtable] $Environment, [string] $WorkingDirectory,
        [int] $MaxMinutes = 120, [int[]] $Watch = @())
    $psi = New-FarmStartInfo $FilePath $ArgumentList $Environment $WorkingDirectory
    $psi.RedirectStandardInput = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    try {
        $p.StandardInput.Close()
        $deadline = (Get-Date).AddMinutes($MaxMinutes)
        $tick = 0
        while (-not $p.WaitForExit(2000)) {
            if ((Get-Date) -gt $deadline) {
                Write-FarmErr "cargo ran over $MaxMinutes minutes: stopped"
                Stop-FarmTree $p.Id
                $p.WaitForExit()
                return 124
            }
            $tick++
            if ($tick % 3 -eq 0) {
                foreach ($w in $Watch) {
                    if (-not (Get-Process -Id $w -ErrorAction SilentlyContinue)) {
                        Stop-FarmTree $p.Id
                        $p.WaitForExit()
                        return 125
                    }
                }
            }
        }
        $p.WaitForExit()
        $p.ExitCode
    } finally {
        $p.Dispose()
    }
}

# The environment of a build: the toolchain under C:\farm, the variables the
# client forwards; never the client's target directory or job count.
function Get-FarmBuildEnv($Forward) {
    $fe = Get-Content -Raw (Join-Path $script:Root 'farm-env.json') | ConvertFrom-Json
    $h = @{}
    $h['CARGO_HOME'] = $fe.CARGO_HOME
    $h['RUSTUP_HOME'] = $fe.RUSTUP_HOME
    $h['PATH'] = ((@($fe.PathPrepend) -join ';') + ';' + $env:PATH)
    $h['TMP'] = Join-Path $script:Root 'tmp'
    $h['TEMP'] = Join-Path $script:Root 'tmp'
    $h['RUSTUP_AUTO_INSTALL'] = '1'
    $h['CARGO_TARGET_DIR'] = $null
    $h['CARGO_BUILD_TARGET_DIR'] = $null
    $h['CARGO_BUILD_JOBS'] = $null
    # No GPU here: GPU tests run on WARP, as on the PC (unless the client
    # forwards another XTP_TEST_ADAPTER).
    $h['XTP_TEST_ADAPTER'] = 'warp'
    if ($Forward) {
        foreach ($p in $Forward.PSObject.Properties) { $h[$p.Name] = [string] $p.Value }
    }
    $h
}

function Get-FarmGit {
    $fe = Get-Content -Raw (Join-Path $script:Root 'farm-env.json') | ConvertFrom-Json
    $fe.Git
}

function Get-FarmCargo {
    $fe = Get-Content -Raw (Join-Path $script:Root 'farm-env.json') | ConvertFrom-Json
    Join-Path $fe.CARGO_HOME 'bin\cargo.exe'
}

function ConvertFrom-FarmBase64([string] $Text) {
    [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Text)) | ConvertFrom-Json
}

function Test-FarmSlotName([string] $Slot) {
    if ($Slot -notmatch '^s\d{1,3}$') { throw "bad slot name '$Slot'" }
}

