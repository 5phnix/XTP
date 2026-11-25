# The build farm: cargo on Google Cloud Windows VMs

XTP builds, lints and tests on a few Windows Spot VMs in Google Cloud instead
of the developer's PC. You keep editing in your worktree; `cargo build`,
`check`, `clippy`, `test` and `doc` run on a farm VM, their output streams
back to your terminal, and the exit code is cargo's. The PC stays free for
game sessions, and its disk is spared.

The tools are in [`tools/buildfarm/`](../../tools/buildfarm/). This page
explains how the farm works, what it costs, how its daily budget cap works,
how to set up your own farm in your own Google Cloud project, and how to use
it day to day.

The farm VMs hold **source code**, plus one file of your own copy of the
title: the shader pack `shaders.rpf`, which `Copy-FarmPack.ps1` puts on
their disks for [shader surveys](#shader-surveys). Nothing else of the
title goes there: builds send only what git tracks, plus the uncommitted
files git does not ignore (see [Security](#security)).

## How it works

```
 PC (your worktree)                         Google Cloud (no public address)
 ------------------                         --------------------------------
 cargo-slot.ps1 / xtp-check.ps1
   -> cargo-remote.ps1
     -> tools/buildfarm/Invoke-FarmCargo.ps1
        1. snapshot: HEAD, or a temporary commit of the uncommitted changes
        2. pick a VM and a slot, start the VM if needed (budget permitting)
        3. git bundle of the commits that VM lacks
        4. one SSH connection, tunnelled by IAP  ----->  farm-run.ps1 on xtp-farm-N
                                                          imports the bundle
                                                          checks out slot sN
                                                          runs cargo there
           output and exit code   <-------------------  (target dir kept per slot)
        5. build: fetch the executables that changed
```

**Snapshot.** Your branch is never pushed anywhere. The client copies your
worktree's index, stages every change into the copy (`git add -A`, which
respects `.gitignore`) and, if anything differs from HEAD, writes a temporary
commit on top of HEAD. Your own index, branch and files are never touched.
Uncommitted files larger than 20 MB are refused (`maxSendFileMB`).

**Sync.** Each VM keeps a bare mirror, `C:\farm\repo.git`. The client
remembers which commits it already sent to each VM and builds a
`git bundle` of only what is missing: usually a few kilobytes. A VM that
lost its disk asks for a full bundle (exit 97) and gets one automatically.

**Slots.** A slot is a checkout of the mirror, `C:\farm\w\s1` to `s6`, each
with its own persistent `target` directory. A worktree gets the same slot
each time it builds on that VM; a new worktree (a delivery check in a fresh checkout, for
example) takes the least recently used free slot. Since every branch is
close to `main`, a checkout over another branch's slot only rewrites the
files that differ, and cargo rebuilds only the crates that changed.

**Run.** `farm-run.ps1` reads the bundle from the SSH connection's stdin,
checks the slot out at the snapshot's commit and runs cargo with your
arguments, from the same subdirectory you are in. Paths of your worktree in
the arguments are rewritten to the slot's. Your `RUSTFLAGS`, `RUST_*`,
`CARGO_PROFILE_*` and `XTP_*` variables go along, except those that hold
paths of your PC (the game folder, caches). `CARGO_TARGET_DIR` and
`CARGO_BUILD_JOBS` do not: the VM uses its slot's target and all its cores.
Builds run without incremental compilation (`CARGO_INCREMENTAL=0`, whatever
the client sets): farm builds gain little from it, and its caches filled
the disks. cargo's stdout and stderr are the SSH connection's, so output
streams as it comes.

**Disk room.** Before its build, a job removes the incremental folders left
from before, in its own slot and in each slot no job holds: it takes that
slot's lock (`C:\farm\locks\<slot>.lock`, which `farm-run.ps1` holds for a
whole job) for the time it takes, so no job starts there meanwhile, and it
never touches a slot a job holds. (Until 25 November it judged a slot idle
by its incremental folder's date, which does not change while files deep
inside it do, and deleted caches under running builds: "unable to copy
...\incremental\... to ...\deps\..." failed two deliveries.) Under 20 GB
free after that, the whole `target` folders of free slots that had no job
for 30 minutes go too, least recently used first (each slot's last job is
in `C:\farm\state\<slot>.used`); their next build there is a full one.

**Time limits.** Nothing waits for ever. A job has `maxRunMinutes` (30) in
all, its attempts included, or `XTP_FARM_JOB_MINUTES` (`xtp-check.ps1` sets
12, so that a stuck farm still leaves time for the local check): the VM
stops cargo at its deadline (exit 124), and the client gives up a little
later on a VM that stopped answering. The bundle must go through with no
pause longer than 60 s, on both sides; short commands (probes, file lists)
get 3 minutes. On a timeout the client stops ssh and its IAP proxy, marks
the VM unavailable for a few minutes and runs the job elsewhere, or on the
PC (75). ssh's own keepalives (`ServerAliveInterval`) are not enough: a
connection stuck in the IAP proxy blocks ssh itself. Each job also stops
the farm's ssh processes and IAP proxies whose caller is gone (a
cargo-remote killed in the middle of a job), never those of a caller still
running. On the VM, the agent stops a run still there after 6 hours.

**Artifacts.** After a successful `cargo build`, the client fetches the
executables the build produced (and their `.pdb`) into your worktree's
`target`, but only those that differ from your local copy (compared by
SHA-256). So `cargo-slot.ps1 build --profile diag -p xtp` still leaves an
up-to-date `target\diag\xtp.exe` for a game run. If fetching fails after a
successful build, the command fails with the reason and how to fetch again
(`--farm-fetch`); it is never built again elsewhere or on the PC. For other
files:

```powershell
# after any successful command
$env:XTP_FARM_FETCH = 'target/diag/xtp.exe;target/diag/xtp.pdb'
# or later, from the slot of this worktree's last farm build
powershell -NoProfile -ExecutionPolicy Bypass -File <tools>\cargo-remote.ps1 --farm-fetch target/debug/deps/xtp_gpu-0123abcd.exe
```

**Scheduling.** The client prefers a running VM, the one that holds this
worktree's slot, then the least loaded (fewer than `maxBuildsPerVm` jobs,
2 by default), then the cheapest. When every running VM is busy it starts a
stopped one, if the budget allows it (below). Jobs from all worktrees of the
PC share leases in the state folder, so they spread over the VMs.

**Spot.** The VMs are Spot VMs: much cheaper, but Google may reclaim one at
any time. A reclaimed VM is stopped, not deleted, and keeps its disk. When
that happens during a job, Windows shuts down and kills the job's processes,
so a failed job's exit code alone cannot tell a reclaim from a failed build:
after any failed job the client asks the VM (`vm\farm-probe.ps1`, about 2 s),
which checks GCE's preemption flag (metadata `instance/preempted`) and
whether Windows is shutting down. When the VM is going away, does not answer,
or the SSH connection dropped, the client marks it unavailable for a few
minutes and runs the job again on another VM. When none is left, the command falls back to the PC (exit 75).

**Idle stop.** The farm agent (`vm\farm-agent.ps1`, a startup task on each
VM) shuts the VM down after 15 minutes (`idleMinutes`) without a farm run,
cargo, rustc, a linker or a test binary. A Windows shutdown stops the GCE
instance, and billing stops with it. Build processes left behind by a
client that went away are killed after 30 minutes, so nothing keeps a VM up
for ever.

**Agent updates.** Each job carries the version (a hash) of the scripts in
`tools\buildfarm\vm`. A VM with another version answers 96, and the client
copies the current scripts over and runs the job again.

**Access.** The VMs have no public address and no service account. SSH is
the only way in, through Identity-Aware Proxy (IAP) TCP forwarding:
`ssh.exe` runs `gcloud compute start-iap-tunnel ... --listen-on-stdin` as
its proxy, so no local port is opened. The farm has its own SSH key, given
to the VMs in their metadata (`ssh-keys`, project keys blocked). Each VM
publishes its SSH host key in its guest attributes, and the client pins it
from there before connecting. Outbound traffic (crates.io, rustup, the
installers) goes through Cloud NAT.

## Using it

On the maintainer's PC, the agents working on XTP run cargo through three
local scripts that live outside the repository, in a tools folder (`<tools>`
below): `cargo-slot.ps1` (cargo, politely), `xtp-check.ps1` (the delivery
check) and `cargo-remote.ps1` (the farm's entry point, see
[the local entry point](#5-the-local-entry-point)). Workers keep calling the
same scripts as before:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File <tools>\cargo-slot.ps1 test -p xtp-gpu --locked
powershell -NoProfile -ExecutionPolicy Bypass -File <tools>\xtp-check.ps1
```

- `cargo-slot.ps1` sends `build`, `check`, `clippy`, `test` and `doc` to the
  farm (through `cargo-remote.ps1`). Commands that change local files or must
  run on the PC (`fmt`, `run`, `xtask`, `bench`, `update`, `clippy --fix`,
  `doc --open`, ...) run locally, as before: cargo-remote answers 76 for
  them without doing anything. To run a game build built on the farm, build
  it (`cargo-slot.ps1 build --profile diag -p xtp`), then start
  `target\diag\xtp.exe`: the build brought it back.
- `xtp-check.ps1` runs its commands (fmt check, clippy twice, tests) on the
  farm in one job: one sync, one slot.
- Both fall back to the PC's cargo, with its usual low priority and wait for
  game sessions, when the farm cannot take the command: exit 75, with the
  reason (`farm: unavailable: ...`), e.g. over the daily budget, no VM can
  start, the farm is not set up.
- `XTP_FARM=off` skips the farm entirely.

Farm messages start with `farm:` and go to stderr; cargo's stdout is
untouched, so `--message-format json` consumers still work.

`cargo-remote.ps1` can also be called directly, with the same arguments as
cargo, and has a few farm commands:

| Command | What it does |
|---|---|
| `cargo-remote.ps1 <cargo args>` | the command on the farm (75: farm unavailable, 76: local-only command) |
| `--farm-steps "<args>" "<args>"` | several cargo commands in one job (what `xtp-check.ps1` uses) |
| `--farm-fetch <path> ...` | files of this worktree's last farm build, into the worktree |
| `--farm-status` | each VM's state and jobs, today's spend |
| `--farm-ledger [-Days 7] [-Intervals]` | machine-hours and spend per day |
| `--farm-start [-Name xtp-farm-2]` | start VMs ahead of a batch of builds |
| `--farm-stop [-Name ...]` | stop VMs now |

Things to know:

- GPU tests run on the VMs too. They have no GPU: D3D12 creates its device on
  the Microsoft Basic Render Driver (WARP), and tests that need a hardware
  feature skip as they do on any machine without it. Anything that must run
  on the RTX of the PC (game runs, benchmarks, `cargo run`) stays local.
- A run is a snapshot: edits you make while it runs are not in it.
- The first build of a new slot is a cold build (a few minutes); later ones
  rebuild only the crates that changed.

## Shader surveys

`xtp shader-survey` and `xtp translate --validate` over the pack need no
GPU, only cores and the Windows SDK's `dxil.dll`: they run on the farm, in
parallel, and the L4 stays free for game sessions.

```powershell
# from any worktree; -Rev: a commit, a branch or HEAD (default)
powershell -NoProfile -ExecutionPolicy Bypass -File <XTP>\tools\buildfarm\Invoke-FarmSurvey.ps1 -Rev main -Args 'shader-survey --host-lanes 32-32 --json'
powershell -NoProfile -ExecutionPolicy Bypass -File <XTP>\tools\buildfarm\Invoke-FarmSurvey.ps1 -Rev HEAD -Args 'shader-survey --validate --host-lanes 32-32 --json'
powershell -NoProfile -ExecutionPolicy Bypass -File <XTP>\tools\buildfarm\Invoke-FarmSurvey.ps1 -Args 'translate --validate --pipelines --host-lanes 32-32'
```

- It builds `xtp` at that commit (`cargo run --release -p xtp --locked`,
  as on the L4) in a farm slot, and runs it there with your arguments and
  the VM's pack, `C:\farm\pack\shaders.rpf`, added at the end (or where
  `{pack}` stands).
- `--json` with no file after it: the survey's JSON comes back as
  `survey.json`. (`translate` has no `--json`: its output is the log.) On
  the VM it is written to `C:\farm\tmp`, outside the slot (xtp refuses to
  write game-derived data inside a source tree), and deleted once fetched.
- Results: `<surveyDir>\<name>\` (by default `farm-surveys` next to the
  farm's state folder; on the maintainer's PC
  `E:\xtp-data\farm-surveys\<name>\`): `survey.log` (xtp's output, the
  totals), `farm.log` (the build, the farm's messages, xtp's stderr; written
  as it runs), `survey.json`, `meta.json` (commit, arguments, VM, slot, exit
  code, time). The command prints the totals and the folder at the end.
  `-Name` names the folder (default `<date-time>-<commit>-<command>`).
- `-Rev` takes a commit only, never your uncommitted changes; `-WithChanges`
  sends the worktree as it is (with `-Rev HEAD`), as cargo-remote does.
- Several at once, from any worktrees: each survey takes an idle running VM,
  else starts a stopped one if the daily budget allows it, else shares the
  least loaded one (in another slot). Surveys share their slots (key
  `farm-survey`), so the release build rebuilds only what changed after the first one
  on each slot.
- Time limit: `surveyMinutes` (90) for the build and the run, `-Minutes`
  for one survey.
- Exit code: xtp's; 75 the farm cannot take it now. There is no fallback
  to the PC: surveys never run there.
- `XTP_*` variables without a local path go along (`XTP_PAIRS`, ...).
- `--pipelines` makes its pipelines on the VM's software adapter (WARP),
  not on a GPU: compare its pipeline counts with other farm runs.

### The pack on the VMs

`Copy-FarmPack.ps1` copies `shaders.rpf`, and only that object, from the
game bucket of the cloud configuration (`gs://<bucket>/<gamePrefix>/shaders.rpf`,
see [cloud-setup.md](cloud-setup.md); `packSource` overrides it) to
`C:\farm\pack\shaders.rpf` on each VM. Each VM downloads it itself, inside
Google's network, through a signed URL valid for 15 minutes; the PC sends
only the command. The copy is checked against the object's size and CRC32C,
and a VM that already holds that object's generation is left alone, so run
it again whenever the pack changes in the bucket, or after making a VM:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File <XTP>\tools\buildfarm\Copy-FarmPack.ps1           # every VM (starts stopped ones, budget permitting)
powershell -NoProfile -ExecutionPolicy Bypass -File <XTP>\tools\buildfarm\Copy-FarmPack.ps1 -Name xtp-farm-3 -Force
powershell -NoProfile -ExecutionPolicy Bypass -File <XTP>\tools\buildfarm\Copy-FarmPack.ps1 -Status   # the pack and dxil.dll of each running VM
```

The VMs still have no service account. The URL is signed by a service
account of the farm's own, `<prefix>-pack` (`packSigner`), which the script
creates with no project role and two grants only:

- `roles/storage.objectViewer` on the game bucket, with an IAM condition
  that limits it to the pack's object
  (`resource.name == "projects/_/buckets/<bucket>/objects/<gamePrefix>/shaders.rpf"`):
  it can read that one file and nothing else, not even list the bucket;
- `roles/iam.serviceAccountTokenCreator` on that service account only, for
  the gcloud account that runs the script, so that it can sign with it.

`New-FarmImage.ps1` removes the pack from the builder before saving its
disk: images never hold it. `Remove-Farm.ps1` deletes the signer.

**dxil.dll.** The image's Windows SDK (10.0.26100.0) has
`C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\dxil.dll`,
version 1.9.2602.17 (validator 1.9), the one `--validate` loads (the L4's
surveys load the same SDK folder's). `Copy-FarmPack.ps1 -Status` shows it
for each VM.

Measured (November 2026, c2d-highcpu-16): the copy takes about 15 s per VM
(us-central1 bucket to europe-west1/4); `shader-survey --host-lanes 32-32
--json` of `main` takes 4 minutes on a slot that built xtp before (the
release build of a fresh slot adds about 80 s).

## Costs

List prices (Cloud Billing Catalog, November 2026), per VM:

| Item | c2d-highcpu-16, europe-west1 | europe-west4 |
|---|---|---|
| Spot vCPUs and memory | 0.096 USD/h | 0.262 USD/h |
| Windows Server licence (16 vCPU x 0.046) | 0.736 USD/h | 0.736 USD/h |
| Cloud NAT | 0.001 USD/h | 0.001 USD/h |
| **Running VM** | **0.834 USD/h** | **1.000 USD/h** |
| 110 GB balanced disk (also when stopped) | 0.36 USD/day | 0.40 USD/day |

The Windows licence is nearly 90 % of the price, and Spot does not discount
it: a VM costs its price per vCPU-hour, whatever the region. Stopped VMs
cost only their disks. Fixed costs (three disks, the image, two Cloud NAT
addresses) come to about 1.4 USD a day.

### The daily cap and the ledger

The farm keeps a ledger (`ledger.json` in its state folder) of every VM
run: one interval per start, from GCE's own start and stop times, priced at
the rate of prices.json. Today's spend is the machine-hours of the local day
at those rates, plus the fixed daily cost.

Before starting a VM, the client checks that today's spend, plus the idle
window (15 minutes) of every VM already running, plus the boot and idle
window of the VM to start, stays under `dailyCapUsd` (50 USD). If not, it
refuses, and says so:

```
farm: unavailable: daily budget: 49.10 USD spent today of 50.00 (machine-hours 55.3), 0.42 reserved for running VMs and the one to start: not starting another VM
```

Once today's spend reaches the cap, running VMs take no new jobs either;
the idle watcher stops them within 15 minutes, and the commands run on the
PC until midnight. See where you are with:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File <tools>\cargo-remote.ps1 --farm-ledger -Intervals
```

The ledger is an estimate from list prices. The project's Cloud Billing
reports are the reference. Refresh the prices now and then with
`Update-FarmPrices.ps1`: Spot prices change up to once a month.

## Setting up your own farm

You need: a Google Cloud project with billing, the Google Cloud SDK
(`gcloud`, authenticated with `gcloud auth login`), Windows with the OpenSSH
client (Settings > System > Optional features), git, and a clone of XTP.

### 1. Project and APIs

```powershell
gcloud projects create <your-project-id>
gcloud config set project <your-project-id>
gcloud services enable compute.googleapis.com iap.googleapis.com
```

(`New-FarmImage.ps1` enables the two APIs too.) Your account needs the
Compute Admin and IAP-secured Tunnel User roles; a project owner has both.

### 2. Quotas

Check them before creating anything:

```powershell
gcloud compute project-info describe --format="table(quotas.metric,quotas.limit,quotas.usage)" | Select-String CPUS_ALL_REGIONS
gcloud compute regions describe europe-west1 --format=json | ConvertFrom-Json | ForEach-Object { $_.quotas } | Where-Object metric -match 'CPUS|SSD_TOTAL_GB|INSTANCES'
```

| Quota | Scope | The farm needs (3 x c2d-highcpu-16) |
|---|---|---|
| `CPUS_ALL_REGIONS` | project | 48, plus your other VMs |
| `C2D_CPUS` (or `N2D_CPUS`, `C3D_CPUS` for other types) | region | 16 per VM in that region |
| `CPUS` | region | 16 per VM in that region |
| `SSD_TOTAL_GB` | region | 110 per VM (balanced disks count as SSD) |

- Spot VMs use the regular CPU quotas as long as the region has no
  `PREEMPTIBLE_CPUS` quota. **Do not request `PREEMPTIBLE_CPUS`**: once
  granted, Spot VMs in that region may only use it.
- New projects often have `CPUS_ALL_REGIONS` = 64 or less. Ask for more in
  the console (IAM & Admin > Quotas & System Limits, filter "CPUs (all
  regions)", Edit) or with `gcloud beta quotas preferences create
  --service=compute.googleapis.com --quota-id=CPUS-ALL-REGIONS-per-project
  --preferred-value=<n> --email=<you> --justification="..."`. Small or new
  projects are often refused automatically: then use smaller VMs or fewer of
  them (the machine type is a setting).
- `SSD_TOTAL_GB` is 250 GB per region by default: two 110 GB VMs per
  region, hence the third VM in a second region in the example.

### 3. Configuration

Copy [`tools/buildfarm/farm.example.json`](../../tools/buildfarm/farm.example.json)
to a folder of your own (the farm's state goes next to it) and set
`project`. Change the VMs, zones and machine type as your quotas allow, and
the cap. Then point the tools at it:

```powershell
[Environment]::SetEnvironmentVariable('XTP_FARM_CONFIG', 'D:\farm\farm.json', 'User')
```

| Setting | Default | Meaning |
|---|---|---|
| `project` | | your project id |
| `vms` | 3 VMs | name and zone of each VM; `machineType` per VM optional |
| `machineType` | `c2d-highcpu-16` | e.g. `c2d-highcpu-32`, `n2d-highcpu-16`, `c3d-highcpu-16` |
| `diskGb`, `diskType` | 110, `pd-balanced` | boot disk of each VM |
| `baseImageFamily` | `windows-2025` | Google's Windows Server image the farm image starts from |
| `idleMinutes` | 15 | idle time before a VM stops itself |
| `dailyCapUsd` | 50 | the daily budget cap |
| `maxBuildsPerVm` | 2 | jobs on a VM before another one is started |
| `slotsPerVm` | 6 | checkouts (with their target directory) per VM |
| `maxRunMinutes` | 30 | longest job, all attempts included (`XTP_FARM_JOB_MINUTES` overrides it) |
| `maxSendFileMB` | 20 | largest uncommitted file sent |
| `stateDir`, `tempDir` | the config's folder, `<stateDir>\tmp` | where the ledger, keys and temporary files go |
| `packSource` | `gs://<bucket>/<gamePrefix>/shaders.rpf` of the cloud configuration | the shader pack's object |
| `packSigner` | `<prefix>-pack` | service account that signs the pack's download URL |
| `surveyDir` | `farm-surveys` next to `stateDir` | where shader surveys come back |
| `surveyMinutes` | 90 | longest shader survey, its build included |

### 4. Image and VMs

```powershell
cd <your XTP clone>
powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\New-FarmImage.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File tools\buildfarm\New-FarmVm.ps1
```

`New-FarmImage.ps1` (30 to 45 minutes) creates the IAP firewall rule, Cloud
NAT in each region of your VMs, the farm's SSH key and the price list; then
the first VM from Google's Windows Server image. Its startup script
(`vm\setup-toolchain.ps1`) installs MinGit, Visual Studio 2022 Build Tools
(MSVC and the Windows SDK) and rustup with the toolchain of
`rust-toolchain.toml`, adds Defender exclusions for the build folders and
turns automatic updates off. The script then installs the farm agent, stops
the VM and saves its disk as an image of the `xtp-farm` family.
`New-FarmVm.ps1` creates the other VMs from that image, checks each one and
leaves them stopped.

Rebuild the image every month or so (Google patches its Windows images
monthly; the farm VMs do not update themselves), or when
`rust-toolchain.toml` changes, though a new toolchain also installs itself
on first use: delete a VM, run `New-FarmImage.ps1 -Builder <that VM>`, then
recreate the others with `New-FarmVm.ps1`.

### 5. The local entry point

`tools\buildfarm\Invoke-FarmCargo.ps1` is the engine; call it with
`XTP_FARM_CONFIG` set, from your worktree, with cargo's arguments:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File <XTP>\tools\buildfarm\Invoke-FarmCargo.ps1 test --workspace --locked
```

Day to day, a small `cargo-remote.ps1` of your own is handier: it takes the
tools from a checkout of `main` (not from the worktree being built), sets the
configuration and adds the farm's admin commands. The maintainer's PC uses
this one:

```powershell
# cargo-remote.ps1: cargo on the build farm. No param block: every argument goes to cargo.
$CargoArgs = @($args)
if (-not $env:XTP_FARM_CONFIG) { $env:XTP_FARM_CONFIG = 'D:\farm\farm.json' }
$tools = if ($env:XTP_FARM_TOOLS) { $env:XTP_FARM_TOOLS } else { 'D:\XTP\tools\buildfarm' }
$env:TMP = $env:TEMP = 'D:\farm\tmp'                 # keep temporary files where you want them
$env:CLOUDSDK_CORE_DISABLE_FILE_LOGGING = 'true'      # no gcloud log file per connection
if (-not (Test-Path (Join-Path $tools 'Invoke-FarmCargo.ps1'))) { exit 76 }
$admin = @{ '--farm-status' = 'Get-FarmStatus.ps1'; '--farm-ledger' = 'Get-FarmLedger.ps1'
            '--farm-start' = 'Start-Farm.ps1'; '--farm-stop' = 'Stop-Farm.ps1' }
if ($CargoArgs.Count -and $admin.ContainsKey($CargoArgs[0])) {
    $rest = @($CargoArgs | Select-Object -Skip 1)
    & (Join-Path $tools $admin[$CargoArgs[0]]) @rest
    exit $LASTEXITCODE
}
& (Join-Path $tools 'Invoke-FarmCargo.ps1') @CargoArgs
exit $LASTEXITCODE
```

A script that runs cargo for you (like `cargo-slot.ps1`) calls it first and
runs cargo locally when it answers 75 or 76:

```powershell
& <tools>\cargo-remote.ps1 @CargoArgs
if ($LASTEXITCODE -ne 75 -and $LASTEXITCODE -ne 76) { exit $LASTEXITCODE }
& cargo @CargoArgs
```

To remove the farm: `tools\buildfarm\Remove-Farm.ps1 -Yes` deletes its VMs,
disks, images, Cloud NAT and firewall rule, nothing else.

## The scripts

| Script | Role |
|---|---|
| `Invoke-FarmCargo.ps1` | sync and run (cargo-remote's engine) |
| `Invoke-FarmSurvey.ps1` | a shader survey of one commit, its output brought back |
| `Copy-FarmPack.ps1` | the shader pack from the bucket to the VMs; `-Status` |
| `pack\vm-pack.ps1` | the VM side of `Copy-FarmPack.ps1` (sent each time, not part of the agent) |
| `New-FarmImage.ps1` | network, keys, prices, first VM, toolchain, image |
| `New-FarmVm.ps1` | the other VMs from the image |
| `Start-Farm.ps1`, `Stop-Farm.ps1` | start (budget-checked) or stop VMs |
| `Get-FarmStatus.ps1` | VMs, jobs, today's spend |
| `Get-FarmLedger.ps1` | spend per day, VM intervals |
| `Update-FarmPrices.ps1` | list prices from the Cloud Billing Catalog |
| `Update-FarmAgent.ps1` | push the agent to running VMs |
| `Remove-Farm.ps1` | delete the farm |
| `Test-Farm.ps1` | offline checks of the helpers and time limits; `-Live`: a diagnostics build of `xtp` must bring `target\diag\xtp.exe` and its `.pdb` back, a job over its time must stop (124), a stuck command be cut off, an orphaned ssh be stopped |
| `FarmCommon.ps1` | the PC side's shared code |
| `vm\setup-toolchain.ps1` | toolchain installation (image builder's startup script) |
| `vm\farm-agent.ps1` | boot tasks and the idle watcher |
| `vm\farm-run.ps1`, `vm\farm-fetch.ps1`, `vm\farm-probe.ps1` | a job; files for the client; "is this VM going away?" |
| `vm\farm-install.ps1`, `vm\FarmVm.ps1` | agent installation; VM side's shared code |

Exit codes of the farm (cargo's pass through): 75 the farm cannot take the
command, 76 the command runs locally, 95 the VM is going away (Spot reclaim
or stop: the job runs again elsewhere), 96 agent outdated, 97 commits missing,
98 the VM could not prepare the job, 99 slot busy too long, 124 run over
time.

## Security

- **No game files from the PC.** Only git content leaves the PC: the commits of the
  worktree and its uncommitted changes, minus what `.gitignore` excludes
  (which covers every game data extension; `cargo xtask policy` guards what
  is committed). Variables holding local paths, such as `XTP_GAME_DIR`, are
  not forwarded. Never copy game files to a farm VM by hand.
- **The shader pack only.** The one exception is `shaders.rpf`, which
  `Copy-FarmPack.ps1` copies from your private bucket to the VMs' disks
  (`C:\farm\pack`), and refuses any other object. It never goes into an
  image, the repository or a slot, and its signer can read nothing else
  ([The pack on the VMs](#the-pack-on-the-vms)).
- **No public port.** No external address; SSH only, from IAP's range
  (35.235.240.0/20) through the `xtp-farm-allow-iap-ssh` rule; IAP checks
  your Google identity before any packet reaches the VM.
- **Least privilege.** No service account on the VMs: a compromised VM has
  no access to the project. The farm's SSH key is the only one accepted
  (`block-project-ssh-keys`), and host keys are pinned.
- **What runs there** is your own branches' code, the same as on your PC.
  Do not use the farm to build untrusted pull requests.
- No project id, address or account is in the repository: they are in your
  configuration file.

## Measurements

On the maintainer's farm (c2d-highcpu-16 Spot, europe-west1-b, November 2026),
from a worktree on the PC:

| What | Time |
|---|---|
| SSH round trip through IAP (`echo`) | 1.5 s |
| A job's overhead: snapshot, bundle, connection, checkout | 5 to 6 s |
| First sync of a VM (full bundle, 1 MB) | under 1 s |
| Later syncs (uncommitted changes only, 51 KB) | under 1 s |
| `xtp-check.ps1` on a new slot (cold target directory) | 100 s: clippy 41 s, clippy `--all-features` 17 s, tests built in 35 s, run |
| `xtp-check.ps1` with every VM stopped, after merging `main` (VM start included) | 116 s |
| `xtp-check.ps1` again, after small changes | 28 s |
| `cargo test -p xtp-gpu` after a one-line change in `crates/xtp-gpu` | 8.2 s (cargo 1.9 s) |
| A stopped VM started on demand, until the job runs | 29 s |
| A Spot reclaim during a job, until it runs again on another VM | 1 minute (measured once; bounded by the time limits) |
| A failed job's extra check (is the VM going away?) | about 2 s |
| An idle VM stopping itself (guest shutdown, then TERMINATED) | at its idle threshold, then 37 s |

## Limits

- **Session 0.** Commands run in the SSH session, which Windows treats as a
  service session (session 0, no desktop). APIs that need an interactive
  desktop refuse there: `PowerSetRequest(PowerRequestDisplayRequired)`
  answers `ERROR_NOT_SUPPORTED`, even with a virtual display device. A test
  that needs a desktop fails on the farm while it passes on a PC.
- **GPU.** No GPU: D3D12 tests run on WARP (`XTP_TEST_ADAPTER=warp` is the
  farm's default, as for local tests). Hardware-only features and timing
  measurements belong on the PC.
- **Snapshots.** A job sees the worktree as it was when the job started.
- **IAP tunnels.** gcloud's tunnel sometimes stops passing data (seen: an
  upload stalled at 0 or 100 KB). The time limits above turn that into a
  retry elsewhere instead of a hang.
- **Spot.** A reclaimed VM can take minutes to come back when its zone is
  short of capacity; the other VMs and the PC take over meanwhile.
