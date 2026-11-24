<#
  Runs ON THE VM in the desktop session of the automatic-logon account: the
  scheduled task xtp-session-job starts it (run-game.ps1 starts the task).
  Windows PowerShell 5.1.

  Reads C:\xtp\jobs\job.json:
    { "exe": ..., "arguments": ..., "workDir": ..., "log": ..., "seconds": N,
      "env": { "NAME": "value", ... }, "done": "<path of the result file>" }
  starts the program with its output and errors in "log" (through cmd.exe,
  without a console window; the program's own windows show normally on the
  desktop), stops it and its children after "seconds" (0: no limit), and
  writes "done" as JSON: exit code, start, end, whether it was stopped.
#>
$ErrorActionPreference = 'Stop'
$jobFile = 'C:\xtp\jobs\job.json'
$job = Get-Content -LiteralPath $jobFile -Raw | ConvertFrom-Json
$result = [ordered]@{ started = (Get-Date).ToString('o'); ended = $null; exitCode = $null; timedOut = $false; error = $null }
try {
    foreach ($p in $job.env.PSObject.Properties) { [Environment]::SetEnvironmentVariable($p.Name, [string]$p.Value, 'Process') }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = "$env:SystemRoot\System32\cmd.exe"
    $psi.Arguments = '/d /c ""' + $job.exe + '" ' + $job.arguments + ' > "' + $job.log + '" 2>&1"'
    $psi.WorkingDirectory = $job.workDir
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $proc = [System.Diagnostics.Process]::Start($psi)
    $limit = [int]$job.seconds
    if ($limit -gt 0) {
        if (-not $proc.WaitForExit($limit * 1000)) {
            $result.timedOut = $true
            & taskkill.exe /PID $proc.Id /T /F 2>&1 | Out-Null
            $proc.WaitForExit(30000) | Out-Null
        }
    } else {
        $proc.WaitForExit()
    }
    $result.exitCode = $proc.ExitCode
} catch {
    $result.error = $_.Exception.Message
} finally {
    $result.ended = (Get-Date).ToString('o')
    $result | ConvertTo-Json | Set-Content -LiteralPath $job.done -Encoding ASCII
}
exit 0
