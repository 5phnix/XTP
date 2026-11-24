<#
  Runs ON THE VM, as an administrator (setup-vm.ps1 calls it over SSH).
  Windows PowerShell 5.1. Safe to run again.

  D3D12 swapchains need an interactive desktop, which an SSH session does
  not have. This sets one up that remote commands can use:
  - a standard (non-administrator) local account, -User (default xtp), with a
    random password that is never shown or written to a file: it is kept
    only as the LSA secret Windows' automatic logon reads;
  - automatic logon of that account at every boot, no lock screen, no screen
    saver, no Server Manager;
  - the scheduled task "xtp-session-job": it runs C:\xtp\tools\vm\session-job.ps1
    in that account's desktop session when started (run-game.ps1 starts it);
  - the scheduled task "xtp-idle-shutdown": every 5 minutes, as SYSTEM, it
    shuts the VM down when nothing ran for the metadata value
    xtp-idle-minutes (see idle-shutdown.ps1). A shut-down VM is stopped and
    no longer billed for its CPU and GPU.
  A reboot is needed for the first automatic logon.
#>
param(
    [string]$User = 'xtp',
    # Give the account a new random password (when the automatic logon fails).
    [switch]$ResetPassword
)
$ErrorActionPreference = 'Stop'

# --- The account --------------------------------------------------------------
$password = $null
function New-Password {
    Add-Type -AssemblyName System.Web
    return [System.Web.Security.Membership]::GeneratePassword(32, 6)
}
if (-not (Get-LocalUser -Name $User -ErrorAction SilentlyContinue)) {
    $password = New-Password
    New-LocalUser -Name $User -Password (ConvertTo-SecureString $password -AsPlainText -Force) -PasswordNeverExpires `
        -AccountNeverExpires -Description 'XTP game runs (automatic logon)' | Out-Null
    Add-LocalGroupMember -Group 'Users' -Member $User -ErrorAction SilentlyContinue
    Write-Output "--> created the local account $User"
} elseif ($ResetPassword) {
    $password = New-Password
    Set-LocalUser -Name $User -Password (ConvertTo-SecureString $password -AsPlainText -Force)
    Write-Output "--> new password for $User"
}

# --- Automatic logon -------------------------------------------------------------
# The password goes to the LSA secret DefaultPassword (not the registry's
# readable DefaultPassword value). A new password is set only with a new account.
$lsa = @'
using System;
using System.Runtime.InteropServices;
public static class XtpLsa {
    [StructLayout(LayoutKind.Sequential)] struct LSA_UNICODE_STRING { public ushort Length; public ushort MaximumLength; public IntPtr Buffer; }
    [StructLayout(LayoutKind.Sequential)] struct LSA_OBJECT_ATTRIBUTES { public int Length; public IntPtr RootDirectory; public IntPtr ObjectName; public uint Attributes; public IntPtr SecurityDescriptor; public IntPtr SecurityQualityOfService; }
    [DllImport("advapi32.dll")] static extern uint LsaOpenPolicy(IntPtr system, ref LSA_OBJECT_ATTRIBUTES attrs, uint access, out IntPtr handle);
    [DllImport("advapi32.dll")] static extern uint LsaStorePrivateData(IntPtr handle, ref LSA_UNICODE_STRING key, ref LSA_UNICODE_STRING data);
    [DllImport("advapi32.dll")] static extern uint LsaClose(IntPtr handle);
    [DllImport("advapi32.dll")] static extern int LsaNtStatusToWinError(uint status);
    static LSA_UNICODE_STRING Str(string s) {
        var u = new LSA_UNICODE_STRING();
        u.Buffer = Marshal.StringToHGlobalUni(s);
        u.Length = (ushort)(s.Length * 2);
        u.MaximumLength = (ushort)(s.Length * 2 + 2);
        return u;
    }
    public static void Store(string key, string value) {
        var attrs = new LSA_OBJECT_ATTRIBUTES();
        IntPtr policy;
        uint st = LsaOpenPolicy(IntPtr.Zero, ref attrs, 0x00000020 /* POLICY_CREATE_SECRET */, out policy);
        if (st != 0) throw new System.ComponentModel.Win32Exception(LsaNtStatusToWinError(st));
        try {
            var k = Str(key); var v = Str(value);
            st = LsaStorePrivateData(policy, ref k, ref v);
            Marshal.FreeHGlobal(k.Buffer); Marshal.FreeHGlobal(v.Buffer);
            if (st != 0) throw new System.ComponentModel.Win32Exception(LsaNtStatusToWinError(st));
        } finally { LsaClose(policy); }
    }
}
'@
$winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
if ($password) {
    Add-Type -TypeDefinition $lsa
    [XtpLsa]::Store('DefaultPassword', $password)
    $password = $null
    Write-Output '--> automatic logon password stored as an LSA secret'
}
Set-ItemProperty -Path $winlogon -Name AutoAdminLogon -Value '1'
Set-ItemProperty -Path $winlogon -Name DefaultUserName -Value $User
Set-ItemProperty -Path $winlogon -Name DefaultDomainName -Value $env:COMPUTERNAME
Remove-ItemProperty -Path $winlogon -Name DefaultPassword -ErrorAction SilentlyContinue
Remove-ItemProperty -Path $winlogon -Name AutoLogonCount -ErrorAction SilentlyContinue

# --- A desktop that stays usable --------------------------------------------------
function Set-Policy([string]$Key, [string]$Name, $Value, [string]$Type = 'DWord') {
    if (-not (Test-Path $Key)) { New-Item -Path $Key -Force | Out-Null }
    Set-ItemProperty -Path $Key -Name $Name -Value $Value -Type $Type
}
Set-Policy 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' NoLockScreen 1
Set-Policy 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Control Panel\Desktop' ScreenSaveActive '0' 'String'
Set-Policy 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System' InactivityTimeoutSecs 0
Set-Policy 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Server\ServerManager' DoNotOpenAtLogon 1
Set-Policy 'HKLM:\SOFTWARE\Microsoft\ServerManager' DoNotOpenServerManagerAtLogon 1
$ErrorActionPreference = 'Continue'
& powercfg /change monitor-timeout-ac 0 2>&1 | Out-Null
& powercfg /change standby-timeout-ac 0 2>&1 | Out-Null
$ErrorActionPreference = 'Stop'
# Windows Audio is off on Server; titles open audio devices.
Set-Service -Name Audiosrv -StartupType Automatic -ErrorAction SilentlyContinue
Start-Service -Name Audiosrv -ErrorAction SilentlyContinue

# --- What the account may write ---------------------------------------------------
foreach ($d in 'C:\xtp\jobs', 'D:\xtp-data') {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
    $acl = Get-Acl $d
    $rule = New-Object System.Security.AccessControl.FileSystemAccessRule($User, 'Modify', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
    $acl.AddAccessRule($rule)
    Set-Acl -Path $d -AclObject $acl
}

# --- The scheduled tasks ----------------------------------------------------------
$job = New-ScheduledTaskAction -Execute 'powershell.exe' `
    -Argument '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\xtp\tools\vm\session-job.ps1'
$principal = New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\$User" -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Hours 12) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName 'xtp-session-job' -Action $job -Principal $principal -Settings $settings -Force | Out-Null
Write-Output '--> task xtp-session-job (runs jobs in the desktop session)'

$idle = New-ScheduledTaskAction -Execute 'powershell.exe' `
    -Argument '-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\xtp\tools\vm\idle-shutdown.ps1'
$every = New-ScheduledTaskTrigger -Once -At (Get-Date).Date -RepetitionInterval (New-TimeSpan -Minutes 5)
$boot = New-ScheduledTaskTrigger -AtStartup
$system = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
$idleSettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 5) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName 'xtp-idle-shutdown' -Action $idle -Trigger @($every, $boot) -Principal $system -Settings $idleSettings -Force | Out-Null
Write-Output '--> task xtp-idle-shutdown (every 5 minutes)'
Write-Output 'session ready (reboot for the first automatic logon)'
exit 0
