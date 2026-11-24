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
| `run-remote.ps1` | your PC → VM | `-Check`, `-Cargo`, `-Game`, `-DeviceCheck`; logs come back |
| `invoke-vm.ps1` | your PC → VM | a PowerShell command, a script or a shell on the VM |
| `start-vm.ps1`, `stop-vm.ps1` | your PC | start, stop, status |
| `rdp-vm.ps1` | your PC | Remote Desktop through an IAP tunnel |
| `common.ps1` | – | shared helpers (configuration, gcloud, SSH through IAP) |
| `vm/*.ps1` | the VM | what the scripts above run there (copied by them) |

Configuration: copy `cloud.example.json` to `%LOCALAPPDATA%\xtp\cloud.json`
(outside the repository) and fill it in.
