<#
.SYNOPSIS
  Runs PowerShell on the VM, or opens a shell there (SSH through IAP).

.DESCRIPTION
  -Command '<PowerShell>'  runs a short command and prints its output
                           (Windows PowerShell 5.1 on the VM; keep it under
                           about 2,000 characters, use -File for more).
  -File <local .ps1>       copies the script to C:\xtp\jobs and runs it.
  -Shell                   an interactive cmd.exe on the VM (type
                           powershell for PowerShell, exit to leave).
  Starts the VM if it is stopped. Exits with the remote exit code.

.EXAMPLE
  tools\cloud\invoke-vm.ps1 -Command 'nvidia-smi'
  tools\cloud\invoke-vm.ps1 -File .\my-probe.ps1
  tools\cloud\invoke-vm.ps1 -Shell
#>
[CmdletBinding(DefaultParameterSetName = 'Command')]
param(
    [Parameter(ParameterSetName = 'Command', Mandatory, Position = 0)] [string]$Command,
    [Parameter(ParameterSetName = 'File', Mandatory)] [string]$File,
    [Parameter(ParameterSetName = 'Shell', Mandatory)] [switch]$Shell,
    [string]$Config
)
. "$PSScriptRoot\common.ps1"
$cfg = Get-XtpCloudConfig $Config
$zone = Start-XtpVm $cfg
switch ($PSCmdlet.ParameterSetName) {
    'Shell' {
        $a = @('compute', 'ssh', "$($cfg.sshUser)@$($cfg.vm)") + (Get-XtpSshArgList $cfg $zone)
        & gcloud @a
        exit $LASTEXITCODE
    }
    'File' {
        $name = Split-Path -Leaf $File
        Send-XtpVmFile -Cfg $cfg -Zone $zone -Path @((Resolve-Path -LiteralPath $File).Path) -Destination 'C:\xtp\jobs'
        $null = Invoke-XtpVm -Cfg $cfg -Zone $zone -Script "& 'C:\xtp\jobs\$name'" -PassThru -AllowFail
        exit $LASTEXITCODE
    }
    'Command' {
        $null = Invoke-XtpVm -Cfg $cfg -Zone $zone -Script $Command -PassThru -AllowFail
        exit $LASTEXITCODE
    }
}
