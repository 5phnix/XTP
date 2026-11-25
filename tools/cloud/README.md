# tools/cloud

PowerShell scripts that set up and drive a Google Cloud Windows VM with an
NVIDIA L4 for XTP's checks, tests and game runs, with your own game files in
a private bucket. The guide is [docs/dev/cloud-setup.md](../../docs/dev/cloud-setup.md).

| Script | Runs on | Does |
|--------|---------|------|
| `new-bucket.ps1` | your PC | the private bucket and the VM's read-only service account |
| `upload-game.ps1` | your PC | your game files (and golden data folder) to the bucket |
| `new-vm.ps1` | your PC | firewall check, the VM (retries zones on stockouts), its data disk, a daily stop |
| `setup-vm.ps1` | your PC → VM | driver, Git, Build Tools, Rust, the clone, the desktop session, idle shutdown |
| `sync-game.ps1` | your PC → VM | the bucket to the VM's `D:` |
| `run-remote.ps1` | your PC → VM | `-Check`, `-Cargo` (with `-Env`), `-GpuTests` (the GPU tests on the L4, each test alone with a time limit), `-Game` (with `-Autopilot`), `-DeviceCheck`, on origin or a local commit (`-Local`); logs and GPU samples come back |
| `invoke-vm.ps1` | your PC → VM | a PowerShell command, a script or a shell on the VM |
| `start-vm.ps1`, `stop-vm.ps1` | your PC | start (retried while the zone has no GPU free), stop, status |
| `rdp-vm.ps1` | your PC | Remote Desktop through an IAP tunnel |
| `common.ps1` | – | shared helpers (configuration, gcloud, SSH through IAP) |
| `vm/*.ps1` | the VM | what the scripts above run there (copied by them) |

Configuration: copy `cloud.example.json` to `%LOCALAPPDATA%\xtp\cloud.json`
(outside the repository) and fill it in.

### L4 queue

Run `tools\cloud\run-remote.ps1 -Status` to see who holds the clone
(`src`) and the desktop (`session`), followed by the waiters for each
in admission order. Times are seconds held or seconds waiting. Status
reads the VM without starting it or uploading scripts.

Each lock request joins its own FIFO on arrival at the VM. Tickets under
`C:\xtp\jobs\queue\<lock>\` carry the VM controller's PID, process start
time and owner. Only the head can take the lock. `-NoWait` fails when the
requested lock is held or another request is ahead. The next contender removes
dead owners' tickets and locks, including locks from older clients. If
the controller exits while its scheduled desktop job is active, the lock
protects the job until it finishes.

After one hour, a warning identifies a possible stall when the controller
and live worker processes have used less than 1% of one CPU core on
average. The status display and waiting callers report it. A long-running
desktop job also checks its own lock. The warning leaves the job running.
