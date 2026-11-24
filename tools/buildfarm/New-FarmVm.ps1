# Creates the farm VMs of the configuration that do not exist yet, from the
# farm image (New-FarmImage.ps1 makes it), waits until each one answers over
# SSH with its own new host key and the current agent, then stops it.
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\New-FarmVm.ps1 [-Name <vm>,...] [-KeepRunning]

param([string[]] $Name = @(), [switch] $KeepRunning)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'FarmCommon.ps1')
$c = Get-FarmConfig
Initialize-FarmNetwork $c
Initialize-FarmSsh $c | Out-Null
if (-not (Get-FarmPrices $c)) { Update-FarmPrices $c | Out-Null }

$existing = @(Get-FarmInstances -Config $c -Refresh | ForEach-Object { $_.name })
$todo = @(@($c.vms) | Where-Object { $existing -notcontains $_.name -and (-not $Name -or $Name -contains $_.name) })
if (-not $todo) { Write-FarmMessage 'every farm VM exists already'; exit 0 }

foreach ($vm in $todo) { New-FarmInstance -Config $c -Name $vm.name }
$agent = Get-FarmAgentVersion
foreach ($vm in $todo) {
    # The agent publishes the VM's new host key once it has made it.
    Update-FarmKnownHost -Config $c -Vm $vm.name -WaitSeconds 900
    Wait-FarmVmReady $c $vm.name
    $r = Invoke-FarmSsh -Config $c -Vm $vm.name -Command 'type C:\farm\bin\version.txt' -Capture
    if ($r.ExitCode -ne 0 -or $r.Out.Trim() -ne $agent) { Publish-FarmAgent $c $vm.name | Out-Null }
    Write-FarmMessage "$($vm.name) is ready"
    if (-not $KeepRunning) { Stop-FarmVm $c $vm.name -Async }
}
if (-not $KeepRunning) {
    $deadline = (Get-Date).AddMinutes(10)
    do {
        Start-Sleep -Seconds 10
        $inst = @(Get-FarmInstances -Config $c -Refresh) | Where-Object { $todo.name -contains $_.name }
    } while (@($inst | Where-Object { -not (Test-FarmStopped $_) }).Count -and (Get-Date) -lt $deadline)
    $inst | ForEach-Object { Write-FarmMessage "$($_.name): $($_.status)" }
}
