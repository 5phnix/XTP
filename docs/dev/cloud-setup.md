# A cloud machine for XTP (Google Cloud, NVIDIA L4, Windows)

This guide sets up a Windows virtual machine with an NVIDIA L4 GPU in **your
own** Google Cloud project, holding **your own** game files, so that XTP's
checks, tests and game runs can happen there instead of on your PC, driven by
you or by Claude Code. Everything is done by the PowerShell scripts in
[`tools/cloud/`](../../tools/cloud); this page explains them, the quotas they
need, what it costs and the rules that keep your files private.

You end up with:

```text
 your PC                                Google Cloud project (yours)
 ┌──────────────────────┐  SSH / RDP    ┌──────────────────────────────────────────┐
 │ tools\cloud\*.ps1    │  through IAP  │ VM xtp-l4: g2-standard-16 + NVIDIA L4    │
 │ gcloud, PowerShell   │ ────────────▶ │ (RTX Virtual Workstation), Windows 2025  │
 │ Claude Code          │  no public    │  C: Windows, build tools, Rust, C:\xtp   │
 │ your game files      │  port         │  D: D:\game, D:\golden, D:\xtp-data      │
 └──────────┬───────────┘               │  desktop session: builds, tests, runs    │
            │ upload-game.ps1           │  stops itself when idle                  │
            ▼                           └──────────────────────────────────────────┘
 ┌──────────────────────┐   sync-game.ps1, inside Google's network      ▲
 │ private bucket in    │ ──────────────────────────────────────────────┘
 │ the same region      │   (the VM's service account: read-only)
 └──────────────────────┘
```

- [Prerequisites](#prerequisites)
- [1. Project, APIs and firewall](#1-project-apis-and-firewall)
- [2. Quotas](#2-quotas)
- [3. Configuration file](#3-configuration-file)
- [4. Setup, step by step](#4-setup-step-by-step)
- [5. Running checks, tests and the game](#5-running-checks-tests-and-the-game)
- [6. Connecting it to Claude](#6-connecting-it-to-claude)
- [7. Costs, and keeping them down](#7-costs-and-keeping-them-down)
- [8. Security rules](#8-security-rules)
- [9. Troubleshooting](#9-troubleshooting)
- [10. Removing everything](#10-removing-everything)

## Prerequisites

- **Your own game files.** A dump of a copy of the title that you own,
  legally obtained. They are yours alone: they go into a private bucket in
  your project and onto your VM's disk, and **never** anywhere shared or
  public, never into the repository ([asset policy](../asset-policy.md)).
  Nobody else's files, and nobody else gets yours.
- **A Google Cloud account with billing enabled**, which you pay for. Set a
  budget alert (Billing → Budgets & alerts) before you start; see
  [Costs](#7-costs-and-keeping-them-down).
- **The gcloud CLI** ([install](https://cloud.google.com/sdk/docs/install)),
  logged in: `gcloud auth login`. Run the scripts from PowerShell (7, or the
  Windows PowerShell 5.1 that comes with Windows). Git Bash's environment
  often does not find gcloud's credentials or Python: use PowerShell (or set
  `CLOUDSDK_PYTHON` to the SDK's bundled Python).
- **Upstream bandwidth** for one upload of the game files (about 210 GB):
  about 5 hours at 100 Mbit/s. On a fibre line we measured 1.19 Gbit/s at
  the start and 40 minutes for the whole 208.6 GiB (88 MiB/s on average:
  the last, largest files go up with fewer parallel streams). Later uploads
  only send what changed.
- A clone of XTP on your PC (the scripts live in it; they do not build
  anything locally).

## 1. Project, APIs and firewall

Use a project of its own, so that its quotas, costs and access are separate:

```powershell
gcloud projects create <your-project-id> --name="XTP cloud"
gcloud billing accounts list                                  # your billing account id
gcloud billing projects link <your-project-id> --billing-account=<billing-account-id>
gcloud config set project <your-project-id>
gcloud services enable compute.googleapis.com storage.googleapis.com iap.googleapis.com cloudquotas.googleapis.com
```

| API | Why |
|-----|-----|
| `compute.googleapis.com` | the VM, its disks, the schedule |
| `storage.googleapis.com` | the bucket |
| `iap.googleapis.com` | SSH and Remote Desktop through Identity-Aware Proxy, without public ports |
| `cloudquotas.googleapis.com` | reading quotas and asking for more from the command line (optional: the console works without it) |
| `cloudbilling.googleapis.com` | only if you want to read prices from the catalogue (optional) |

**Firewall.** A new project's `default` network comes with rules that let
SSH (22) and RDP (3389) in **from the whole Internet** (`0.0.0.0/0`). The VM
has an external address (for its downloads), so restrict those rules to
Google's IAP range before anything else:

```powershell
gcloud compute firewall-rules update default-allow-ssh --source-ranges=35.235.240.0/20
gcloud compute firewall-rules update default-allow-rdp --source-ranges=35.235.240.0/20
```

(or delete them). `new-vm.ps1` refuses to go on while any rule of the
`default` network opens 22 or 3389 to `0.0.0.0/0`, and adds a rule
`xtp-allow-iap` (22 and 3389 from `35.235.240.0/20` only, to VMs tagged
`xtp`) when none lets IAP in.

## 2. Quotas

Quotas are limits on what your project may use; capacity is whether Google
has a free GPU in a zone right now. You need both. Quotas are per project,
and most are per region.

### What the VM needs

| Quota (metric) | Scope | Needed | In the project we set up (2026-11) |
|----------------|-------|--------|------------------------------------|
| `GPUS_ALL_REGIONS` (`compute.googleapis.com/gpus_all_regions`) | global | 1 | 1 (new projects often start at **0**) |
| `NVIDIA_L4_GPUS` (`compute.googleapis.com/nvidia_l4_gpus`) | per region | 1 | 1 |
| `NVIDIA_L4_VWS_GPUS` (`compute.googleapis.com/nvidia_l4_vws_gpus`) | per region | 1 | 1 |
| `CPUS` (`compute.googleapis.com/cpus`) | per region | 16 | 200 |
| `CPUS_ALL_REGIONS` (`compute.googleapis.com/cpus_all_regions`) | global | 16 | 64 |
| `SSD_TOTAL_GB` (`compute.googleapis.com/ssd_total_storage`) | per region | 400 (100 boot + 300 data) | **250** in europe-west1, -west2, -west3, -west4, -west6; 1000 in us-central1 |
| `IN_USE_ADDRESSES` (`compute.googleapis.com/in_use_addresses`) | per region | 1 | 8 |

Defaults differ between projects and change over time: check yours (below).

The VM uses the `nvidia-l4-vws` accelerator: an L4 with an NVIDIA RTX
Virtual Workstation licence that Google adds and bills by the hour. It is what
gives D3D12 graphics on Windows with a supported driver. Its quota is
`NVIDIA_L4_VWS_GPUS`, separate from `NVIDIA_L4_GPUS` (which plain `nvidia-l4`
uses); `GPUS_ALL_REGIONS` counts both.

Disks: the G2 machine series does not accept `pd-standard`, so both disks
are `pd-balanced`, which counts against `SSD_TOTAL_GB`. A region with the
common default of 250 GB cannot hold the 300 GB data disk: ask for 500 GB
there.

### Checking what you have

```powershell
# global quotas
gcloud compute project-info describe --format="table(quotas.metric,quotas.limit,quotas.usage)" | Select-String 'GPUS_ALL|CPUS_ALL'
# one region
gcloud compute regions describe <region> --format=json | ConvertFrom-Json | Select-Object -ExpandProperty quotas |
    Where-Object metric -match 'L4|^CPUS$|SSD_TOTAL|IN_USE' | Format-Table metric, limit, usage
# the vWS quota is only visible through Cloud Quotas
gcloud beta quotas info describe NVIDIA-L4-VWS-GPUS-per-project-region --service=compute.googleapis.com --project=<your-project-id>
```

### Asking for more

In the console: **IAM & Admin → Quotas & System Limits**.

1. In the filter, type the metric's display name or metric name, e.g.
   `GPUs (all regions)`, `NVIDIA L4 Virtual Workstation GPUs`,
   `Persistent Disk SSD (GB)`; add `region:<your region>` for regional ones.
2. Tick the row, then **Edit quotas** (top right).
3. Enter the new value (ask for what you need, no more: 1 GPU, 500 GB),
   and a justification, then your contact details, then **Submit request**.

From the command line (Cloud Quotas API), for example:

```powershell
gcloud beta quotas preferences create --service=compute.googleapis.com --project=<your-project-id> `
    --quota-id=SSD-TOTAL-GB-per-project-region --dimensions=region=<region> --preferred-value=500 `
    --email=<your contact address> `
    --justification="One Windows workstation VM (g2-standard-16 + 1 NVIDIA L4 vWS) for an open-source emulator's tests; it needs a 100 GB boot disk and a 300 GB data disk, both pd-balanced (G2 does not support pd-standard)."
```

Quota ids: `GPUS-ALL-REGIONS-per-project`, `NVIDIA-L4-VWS-GPUS-per-project-region`,
`NVIDIA-L4-GPUS-per-project-region`, `CPUS-per-project-region`,
`CPUS-ALL-REGIONS-per-project`, `SSD-TOTAL-GB-per-project-region`.

**What to write.** One or two sentences: what runs (one Windows VM with one
L4 for graphics tests of an open-source project), how many and for how long
(1 GPU, a few hours a week), and why that size. Small, concrete requests
pass; vague or large ones do not.

**How long it takes.** Small increases for a project with some billing
history are usually decided automatically within minutes. GPU requests on a
new project or a new billing account can take one or two business days, or be
refused at once: Google wants some billing history first. We saw a request
for 128 CPUs refused automatically within a minute, while the defaults were
enough for this VM.

**When it is refused.**

- Ask for less (exactly what this VM needs, see the table).
- Use the project for a few days (a small VM, the bucket) so that it has
  billing history, then ask again.
- Try another region whose defaults are already enough: check before asking.
- If it is still refused, the refusal email says how to contact Cloud
  Billing or sales support with your use case. Do not open several identical
  requests: it does not speed things up.

### Capacity: when no zone has a GPU free

Even with quota, creating the VM can fail with `ZONE_RESOURCE_POOL_EXHAUSTED`:
the zone has no L4 free at that moment. This is common. On 2026-11-24 every L4
zone of europe-west1, -west2, -west3, -west4, -west6 and us-central1 was
stocked out for about twenty minutes; `us-central1-a` had one at the third
round of retries. `new-vm.ps1` tries the configured zones in order, and with
`-RetryMinutes <n>` tries them again every four minutes. List several regions'
zones in `zones`, your preferred ones first.

Starting a stopped VM hits the same wall, and its zone is fixed: that day
our VM stopped itself while idle and could not start again for a while
(`ZONE_RESOURCE_POOL_EXHAUSTED` in us-central1-a). Every script retries a
start every 2 minutes for `startRetryMinutes`; `start-vm.ps1 -RetryMinutes
120` waits longer. When you need the GPU at a given time (a measurement
session), start the VM ahead and keep it from stopping (raise
`xtp-idle-minutes`, see [Troubleshooting](#9-troubleshooting)), or pay for
a [reservation](https://cloud.google.com/compute/docs/instances/reservations-overview)
of a g2-standard-16 with an L4 in its zone, billed like a running VM
whether you use it or not.

## 3. Configuration file

Copy [`tools/cloud/cloud.example.json`](../../tools/cloud/cloud.example.json) to
`%LOCALAPPDATA%\xtp\cloud.json` (or anywhere outside the repository, and set
`XTP_CLOUD_CONFIG` to its path, or pass `-Config <path>` to each script) and
fill it in. The scripts refuse a configuration file inside the repository: it
names your project, bucket and paths.

| Key | Meaning |
|-----|---------|
| `project` | your project id |
| `region` | the bucket's region; the VM should be in one of its zones |
| `zones` | zones tried in order when the VM is created (another region's zones work as a fallback; the VM then reads the bucket across regions, which is slower and billed) |
| `bucket` | a globally unique bucket name |
| `gameDir` | your dump of the title on your PC (only read) |
| `titleExe` | the title's executable, relative to `gameDir` |
| `goldenDir` | optional: an XTP data folder (saves, settings) that game runs start from, e.g. one with the first boot done and a save in the world |
| `vm`, `machineType`, `accelerator`, `bootDiskGb`, `dataDiskGb`, `diskType` | the VM (defaults: `xtp-l4`, `g2-standard-16`, `nvidia-l4-vws`, 100, 300, `pd-balanced`) |
| `idleMinutes` | the VM shuts itself down after this long without activity (default 30; 0: never) |
| `startRetryMinutes` | how long the scripts keep trying to start a stopped VM whose zone has no GPU free (default 20) |
| `stopSchedule`, `timeZone` | a daily stop, as a backstop (default `0 3 * * *`, `Europe/Paris`) |
| `sshUser` | your account on the VM (default: your Windows user name in lowercase) |
| `sessionUser` | the local account whose desktop runs the game (default `xtp`) |
| `localLogDir` | where logs come back (default `%LOCALAPPDATA%\xtp\cloud-runs`) |

## 4. Setup, step by step

Every script is idempotent: run it again after a failure or a change, and it
does only what is missing. Each prints what it does.

1. **The bucket** — `tools\cloud\new-bucket.ps1`
   Creates `gs://<bucket>` in `region` with uniform bucket-level access and
   public access prevention enforced, creates the VM's service account
   (`xtp-vm`) and lets it read this bucket only (`roles/storage.objectViewer`
   on the bucket).

2. **The upload** — `tools\cloud\upload-game.ps1`
   `gcloud storage rsync` of `gameDir` to `gs://<bucket>/game` and of
   `goldenDir` to `gs://<bucket>/golden/data`, with parallel composite uploads
   (several streams per large file), at below-normal priority. Your folders
   are only read. Run it in a window of its own; it can be stopped and
   started again (it resumes, sending only what is missing). `-What golden`
   sends only the golden folder (after you make a new one). Nothing is deleted
   from the bucket unless you pass `-Mirror`.

3. **The VM** — `tools\cloud\new-vm.ps1 [-RetryMinutes 120]`
   Checks the firewall, creates the VM in the first zone with capacity (see
   [Capacity](#capacity-when-no-zone-has-a-gpu-free)), creates and attaches the
   data disk, and attaches a daily stop schedule (the Compute Engine service
   agent gets the right to stop and start this VM only). The VM runs as the
   bucket-reader service account; metadata enables Windows SSH and blocks
   project-wide SSH keys. Billing starts when the VM is created.

4. **The software** — `tools\cloud\setup-vm.ps1` (30 to 45 minutes the first time)
   Over SSH, on the VM: formats the data disk as `D:`, adds Defender
   exclusions for `C:\xtp` and `D:\`, installs the NVIDIA driver with
   [Google's script](https://github.com/GoogleCloudPlatform/compute-gpu-installation)
   (the vWS driver for this GPU), Git for Windows, Visual Studio Build Tools
   (C++ workload), rustup for all users (`C:\xtp\rust`), clones XTP to
   `C:\xtp\src` and installs the toolchain `rust-toolchain.toml` pins; reboots
   when the driver asks. Then it creates the desktop session (see
   [Game runs](#game-runs-and-the-desktop-session)) and the idle shutdown,
   reboots, and checks the D3D12 device.

5. **The game files on the VM** — `tools\cloud\sync-game.ps1`
   On the VM, `gcloud storage rsync` from the bucket to `D:\game` and
   `D:\golden\data`. Inside the region it costs nothing; we measured about
   120 MiB/s (29 minutes for 208.6 GiB), close to what a 300 GB
   `pd-balanced` disk can write. Later runs copy only what changed.

6. **Check it works**

   ```powershell
   tools\cloud\run-remote.ps1 -DeviceCheck   # the L4 as D3D12 sees it
   tools\cloud\run-remote.ps1 -Check         # the workspace check on origin/main
   tools\cloud\run-remote.ps1 -Game          # a 3-minute game run
   ```

   What the device check printed on our VM (driver 596.86, Windows Server
   2025):

   ```text
   adapter 0: NVIDIA L4 (vendor 0x10de device 0x27b8, 22,595 MB dedicated)
       feature level 12_2, shader model 6.8, mesh shader tier 1, wave lanes 32-32, ray tracing tier 1.2
   adapter 1: Microsoft Basic Render Driver (vendor 0x1414 device 0x008c, 0 MB dedicated)
   ...
   nvidia-smi: NVIDIA L4, 596.86, WDDM, 23034 MiB
   nvidia-smi:        Product Name                                   : NVIDIA RTX Virtual Workstation
   nvidia-smi:        License Status                                 : Licensed (Expiry: Permanent)
   XTP device check: OK (adapter 0 is hardware with shader model 6.6+ and mesh shaders)
   ```

   XTP creates its device on adapter 0, so the L4 must come first (it does:
   the VM has no other display adapter). The image is Windows Server 2025
   because XTP ships no D3D12 Agility SDK and relies on the system's D3D12,
   which there reaches shader model 6.8. The L4 runs 32-lane waves: compute
   programs that need 64-lane waves (see the README's status table) do not
   run on it.

7. **Stop it** — `tools\cloud\stop-vm.ps1` (or let it stop itself).

## 5. Running checks, tests and the game

All of these start the VM if it is stopped and bring the logs back to
`<localLogDir>\<date-time>-<what>\`.

```powershell
tools\cloud\run-remote.ps1 -Check -Local                    # your current commit
tools\cloud\run-remote.ps1 -Check -Ref origin/some-branch   # a branch of the VM's clone
tools\cloud\run-remote.ps1 -Cargo 'test -p xtp-gpu --locked' -Local
tools\cloud\run-remote.ps1 -Game -Local -Seconds 240
tools\cloud\run-remote.ps1 -Game -NoBuild -Env @{ XTP_LOG = 'xtp_gpu=debug,info'; XTP_GPU_TIMING = '1' }
tools\cloud\run-remote.ps1 -Game -Local -Repo E:\XTP -LocalRev main -Autopilot autopilot\gameplay.txt -Seconds 420
tools\cloud\invoke-vm.ps1 'nvidia-smi'                       # any short PowerShell on the VM
tools\cloud\invoke-vm.ps1 -Shell                             # a shell on the VM
```

**Which code.** By default the VM's clone is put on `origin/main` (fetched
first) or on `-Ref`. With `-Local` your current commit is sent as a git bundle
(only committed work: the script warns about local changes). The VM keeps one
clone and one `target` folder, so builds are incremental; one build runs at a
time and a second caller waits.

**The check** (`-Check`) runs the commands CI runs: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`, the same
with `--all-features`, and `cargo test --workspace --locked`. Its log comes
back as `<date-time>-check.log`.

**Builds and tests run in the VM's desktop session**, like on a PC, not in
the SSH session: some tests need a desktop. For example `xtp-hle`'s
`display::tests::sdr_and_display_deferrals` asks Windows to keep the display
on, which fails with `ERROR_NOT_SUPPORTED` (0x80070032) in an SSH session and
passes in the desktop session.

### Game runs and the desktop session

D3D12 swapchains need an interactive desktop; an SSH session has none. So
`setup-vm.ps1` creates a standard local account (`sessionUser`, default `xtp`)
that Windows logs on automatically at every boot. Its random password exists
only as the LSA secret Windows' automatic logon reads: it is never shown and
never written to a file. A scheduled task, `xtp-session-job`, runs jobs in
that account's desktop session: builds, tests and game runs, one at a time,
their output streamed back to the SSH session that asked for them. The
account may write to `C:\xtp\src`, `C:\xtp\rust`, `C:\xtp\logs`,
`C:\xtp\jobs` and `D:\xtp-data`, not to `C:\xtp\tools` (whose scripts run as
administrator or SYSTEM).

`run-remote.ps1 -Game` builds the diagnostics build (`target\diag\xtp.exe`,
needed for scripted keys and frame capture), then `vm\run-game.ps1`:

- makes `D:\xtp-data\runs\<date-time>\` with a copy of `D:\golden\data`
  (when the bucket has a golden folder; the golden folder itself is never
  written) and an `xtp.toml` pointing at `D:\game`, `D:\xtp-data\cache` and
  that copy;
- sets `XTP_KEYS` (by default Enter, the pad's A, every 2.5 s from 5 s to
  50 s, which takes the title past its start screens; `-Keys` changes it,
  `-Keys '-'` presses nothing) and `XTP_CAPTURE` (presented frames as PNG);
- starts the title in the desktop session, stops it after `-Seconds`, and
  prints a summary: exit code, the device line, the last frame-rate lines,
  errors, the end of `crashes.log`.

Back on your PC you get `run.log`, `env.txt`, `result.json`, `xtp.toml`,
`crashes.log` (if any) and `capture\frame-<n>.png`. The data copy (saves)
stays on the VM.

**Watching a run.** `tools\cloud\rdp-vm.ps1 -NewPassword` prints a new
Windows password for *your* account (once, in your console: keep it in your
password manager) and opens Remote Desktop through an IAP tunnel. Log on as
yourself. Do not log on as the session account: Remote Desktop would take over
its console session, and once you disconnect, game runs fail until the VM
restarts.

**The machine has no sound card**: audio output goes nowhere (the Windows
Audio service is on so that the title can open its devices).

## 6. Connecting it to Claude

### Claude Code on your PC, driving the VM

The simplest setup: Claude Code runs in your local clone (or in a Hangar
worker's worktree) and uses the scripts, which reach the VM through gcloud
and IAP. Claude never needs the Windows password: SSH uses gcloud's key, which
gcloud generates once (`%USERPROFILE%\.ssh\google_compute_engine`) and adds to
the VM's metadata for your lowercase user name.

Tell it what it may run. In `.claude/settings.local.json` of your clone (kept
out of git) or in `~/.claude/settings.json`, for example:

```json
{
  "permissions": {
    "allow": [
      "PowerShell(tools\\cloud\\run-remote.ps1 *)",
      "PowerShell(tools\\cloud\\invoke-vm.ps1 *)",
      "PowerShell(tools\\cloud\\sync-game.ps1 *)",
      "PowerShell(tools\\cloud\\start-vm.ps1 *)",
      "PowerShell(tools\\cloud\\stop-vm.ps1 *)",
      "PowerShell(gcloud compute instances list *)",
      "PowerShell(gcloud compute instances describe *)"
    ],
    "deny": [
      "PowerShell(gcloud compute instances delete *)",
      "PowerShell(gcloud compute disks delete *)",
      "PowerShell(gcloud storage rm *)",
      "PowerShell(gcloud storage buckets *)",
      "PowerShell(gcloud iam *)",
      "PowerShell(gcloud projects *)",
      "PowerShell(gcloud compute reset-windows-password *)"
    ]
  }
}
```

Rules match the command Claude types (PowerShell rules are written like Bash
rules, `Tool(<command> *)`, and match case-insensitively); a `gcloud` call
*inside* a script is not checked against them, so allow the scripts you trust
and keep destructive or account-changing commands for yourself. Leave
`new-bucket.ps1`, `new-vm.ps1`, `setup-vm.ps1` and `rdp-vm.ps1` to ask each
time. Check your rules with a harmless prompt before you rely on them.

What to ask for, typically: "run the workspace check on the cloud VM for my
current commit" (`run-remote.ps1 -Check -Local`), "run the game for 4 minutes
on the VM and tell me how far it got" (`run-remote.ps1 -Game -Local -Seconds 240`,
then reading `summary.txt`, `run.log` and the captured frames).

### Claude Code installed on the VM

For long, unattended work on the VM itself (bisecting, iterating on a shader
against the real GPU), Claude Code can run there:

1. `tools\cloud\invoke-vm.ps1 -Shell`, then `powershell`, then
   `irm https://claude.ai/install.ps1 | iex` (installs it for your account).
2. Log in with `claude` and follow the prompts; when the VM has no browser,
   open the address it prints on your PC. (An `ANTHROPIC_API_KEY` set in your
   account's environment also works; it is a secret: never put it in the
   repository.)
3. Work in `C:\xtp\src`. The VM's tools are on the machine `PATH`
   (`cargo`, `git`); game runs go through `C:\xtp\tools\vm\run-game.ps1
   -TitleExe <exe>` (an SSH session or Claude cannot open a desktop itself).

What it can reach: your account is an administrator of the VM, so Claude on
the VM can do what you can there, including reading the game files. The VM's
service account can only read the bucket; it cannot change or delete cloud
resources. Give Claude on the VM the same kind of permission rules as above
(allow `cargo *`, `git *`, the `C:\xtp\tools\vm\*.ps1` scripts; deny
`gcloud *`), and do not let it push or open pull requests from the VM: it has
no GitHub credentials and should not get any.

### Hangar workers

A Hangar worker is a Claude Code session in a worktree of its own on your PC.
To keep your PC free for game sessions, workers build and test elsewhere:
everyday `cargo` and the delivery check on the [build farm](build-farm.md),
and on this VM what needs the L4 or the game:

- each worker runs `tools\cloud\run-remote.ps1 -Game -Local` (or `-Check`,
  `-Cargo`) from its worktree: its own commit is bundled and checked out on
  the VM (`-Repo <path> -LocalRev <branch>` sends another one, such as an
  unpushed `main`), builds are serialized by a lock on the VM, and each
  worker gets its logs in its own folder;
- workers do not stop the VM (another may be using it): it stops itself after
  `idleMinutes` without activity, and the orchestrator or you can run
  `stop-vm.ps1`;
- one game run at a time: a second worker's run waits, or fails if the first
  one hangs (`run.lock` names the holder);
- the same rules as below apply: workers run their own branches, never a
  stranger's pull request.

**Builds and tests without game files** belong on the
[build farm](build-farm.md): Spot VMs without a GPU that run every worker's
`cargo` and the delivery check, and never receive game files. The L4 VM is
for what needs the real GPU or the game: GPU tests on hardware, game runs,
measurements. A single build machine of your own can also come from these
scripts, with another configuration file: `accelerator` empty, another `vm`
name and `machineType` (for example `c3-standard-22`), a `serviceAccount` of
its own (`new-vm.ps1` creates it without any access to the bucket; do not
run `new-bucket.ps1` with that configuration), a smaller `dataDiskGb`, and no
`sync-game.ps1`. `setup-vm.ps1` then skips the NVIDIA driver and still sets
up the desktop session, where builds and tests run.

## 7. Costs, and keeping them down

Prices from Google's catalogue (on-demand, USD, November 2026; check
[the pricing pages](https://cloud.google.com/compute/all-pricing) for yours):

| Item | us-central1 | europe-west4 |
|------|-------------|--------------|
| g2-standard-16: 16 vCPUs | 16 × 0.02499 = 0.400 /h | 16 × 0.02626 = 0.420 /h |
| g2-standard-16: 64 GB memory | 64 × 0.00293 = 0.188 /h | 64 × 0.00308 = 0.197 /h |
| NVIDIA L4 | 0.560 /h | 0.589 /h |
| RTX Virtual Workstation licence (vWS) | 0.200 /h | 0.200 /h |
| Windows Server licence (per vCPU) | 16 × 0.046 = 0.736 /h | 0.736 /h |
| External IPv4 address while running | 0.005 /h | 0.005 /h |
| **Running, compute total** | **≈ 2.09 /h** | **≈ 2.15 /h** |
| Disks, 100 + 300 GB pd-balanced (running or stopped) | 0.10 /GB-month = 40 /month | 0.11 /GB-month = 44 /month |
| Bucket, ≈ 210 GB Standard | 0.020 /GB-month ≈ 4.2 /month | ≈ 4.2 /month |
| **Stopped, total** | **≈ 44 /month** | **≈ 48 /month** |

Examples (us-central1): 40 hours of use a month ≈ 40 × 2.09 + 44 ≈ **128 USD**;
left running all month ≈ **1,570 USD**. Network: bucket to VM in the same
region is free; uploads to the bucket are free; downloads to your PC (logs)
are tiny.

Keeping them down:

- **Let it stop.** The VM shuts itself down after `idleMinutes` (default 30)
  without SSH or Remote Desktop sessions, builds, game runs or Claude Code; a
  daily schedule stops it at `stopSchedule` in any case. `stop-vm.ps1` stops
  it now. A stopped VM costs only its disks.
- **Keep the machine size honest.** The Windows licence is billed per vCPU:
  a `g2-standard-8` costs about 1.42 /h instead of 2.09.
- **Spot VMs** cost much less for the CPU and GPU (Windows and vWS licences
  are unchanged) but can be stopped by Google at any time: fine for tests,
  not for long game sessions. They use the `PREEMPTIBLE_NVIDIA_L4_VWS_GPUS`
  quota.
- **Unused for weeks?** Delete the data disk (`gcloud compute disks delete
  <vm>-data`, after `stop-vm.ps1`), saving 30 /month; `new-vm.ps1`,
  `setup-vm.ps1` and `sync-game.ps1` bring it back in minutes from the bucket.
- **Budget alert**: Billing → Budgets & alerts, for example at 50 and 150 USD
  a month.

## 8. Security rules

- **The bucket is never public.** Uniform bucket-level access and public
  access prevention stay enforced (`new-bucket.ps1` sets them and refuses a
  policy that names `allUsers` or `allAuthenticatedUsers`). No signed URLs, no
  sharing the bucket with anyone: every collaborator uses their own files in
  their own project.
- **Least access.** The VM's service account reads this one bucket and
  nothing else. Only you (the project owner) can change the project.
- **No public ports.** SSH and Remote Desktop only through IAP
  (`35.235.240.0/20`); nothing listens for the Internet. Do not add firewall
  rules from `0.0.0.0/0`.
- **Never run untrusted code on a machine that holds game files.** The VM
  has your game files and Internet access: code from a pull request you have
  not read could copy them out. Run your own commits (`-Local`) or code you
  have reviewed; read a contributor's pull request before you run it there,
  and if in doubt, run it on a machine without game files.
- **GitHub CI never gets access.** No service account keys in GitHub
  secrets, no workload identity federation to this project, no self-hosted
  runner on this VM. CI runs the checks that need no game files; game runs
  happen only on machines you control.
- **Passwords stay with you.** `gcloud compute reset-windows-password` (or
  `rdp-vm.ps1 -NewPassword`) prints a password in your console: keep it in
  your password manager, never in a file in the repository, an issue, a log
  or a chat with an agent. The session account's password is never shown at
  all.
- **Your configuration stays outside the repository** (the scripts refuse
  otherwise): it names your project, bucket and paths.
- **What comes back from runs** (logs, summaries) contains no game data, but
  captured frames are pictures of the game: look at them, do not publish
  them or attach them to issues ([asset policy](../asset-policy.md)).

## 9. Troubleshooting

| Symptom | Cause and fix |
|---------|---------------|
| `ZONE_RESOURCE_POOL_EXHAUSTED` when creating | No GPU free in that zone right now. `new-vm.ps1 -RetryMinutes 120`, more zones in `zones`, or try again later. |
| `ZONE_RESOURCE_POOL_EXHAUSTED` when starting | The VM's zone has no GPU free right now; its zone cannot change. `start-vm.ps1 -RetryMinutes 120`, or later. See [Capacity](#capacity-when-no-zone-has-a-gpu-free). |
| `Quota ... exceeded` | See [Quotas](#2-quotas); the message names the metric. |
| SSH: `Server refused our key` | Right after creation the guest agent may not have created your account yet: wait a minute. Otherwise the user name has capitals: Windows' sshd only matches the lowercase name, and the scripts use `sshUser` (lowercase) for that reason. With gcloud by hand: `gcloud compute ssh <lowercase-user>@<vm> --tunnel-through-iap`. |
| `unrecognized arguments` from gcloud in your own PowerShell | PowerShell splits unquoted `a,b` into two arguments and eats a bare `--`: quote such arguments (`"--accelerator=type=nvidia-l4-vws,count=1"`). |
| Game run: `... is not logged on to the console` | The automatic logon failed (or someone took over the session with Remote Desktop). Restart the VM (`invoke-vm.ps1 'Restart-Computer -Force'`), or `setup-vm.ps1 -ResetSessionPassword`. |
| The D3D12 check fails | `invoke-vm.ps1 'nvidia-smi'`: no GPU means a driver problem (run `setup-vm.ps1` again); a licence status other than *Licensed* means the vWS licence is missing (the VM must use `nvidia-l4-vws`). |
| Serial console warnings `logging.logEntries.create denied` | Harmless: the VM's service account may not write to Cloud Logging. Grant it `roles/logging.logWriter` on the project if you want the guest agent's logs there. |
| The VM stopped during your work | The idle shutdown saw no activity for `idleMinutes`: start it again; for long pauses with nothing running, write a time into `C:\xtp\keep-awake-until` (`invoke-vm.ps1 "(Get-Date).AddHours(3).ToString('o') \| Set-Content C:\xtp\keep-awake-until"`) or raise the limit: `gcloud compute instances add-metadata <vm> --zone=<zone> --metadata=xtp-idle-minutes=120`. |

## 10. Removing everything

```powershell
$p = '<your-project-id>'; $z = '<zone>'; $r = '<region>'
gcloud compute instances delete xtp-l4 --zone=$z --project=$p              # and its boot disk
gcloud compute disks delete xtp-l4-data --zone=$z --project=$p
gcloud compute resource-policies delete xtp-l4-daily-stop --region=$r --project=$p
gcloud storage rm --recursive gs://<bucket>                                 # your copy of the game files
gcloud iam service-accounts delete xtp-vm@$p.iam.gserviceaccount.com --project=$p
gcloud compute firewall-rules delete xtp-allow-iap --project=$p            # if new-vm.ps1 created it
```

Or delete the whole project (`gcloud projects delete <your-project-id>`),
which stops all billing for it.
