# The world-load stall: VidMm holds every new allocation past ~9.5 GB

Why every session since 13:37Z on 2026-11-24 stalls while loading the world,
measured on the person's PC (RTX 4080 SUPER 16 GB, driver 621.01, Windows 11
26300, an AMD iGPU beside it, 64 GB). GB below are GiB (2^30), as the tools
print them. Times are UTC unless marked local (UTC+1). Logs and samples are
under `E:\xtp-data\runs\` (sessions in `play-*`, `stall-*` and
`E:\xtp-data\cache\play\<UTC stamp>`, probes in `stall-probe`); none is in
the repository. Title code is named by RVA (build blake3
`b99f876b74d4a77f…`); no title bytes or listings are quoted.

## 1. Summary

- **The call**: the title's "resource at my address" device method 0x428
  (call site title+0x193dff8, return title+0x193dffe) →
  `xtp_gpu::resource::create_at_address` (resource.rs:1130 at `2d0257e`) →
  `hand_out_resource` → `host_texture` (resource.rs:934) →
  `ID3D12Device::CreateCommittedResource` of the host copy of a title
  texture: DEFAULT heap, `D3D12_HEAP_FLAG_NONE` (NOT_RESIDENT since
  dark-world's change), the title's description without its console layout,
  most often BC7_UNORM (98) 1024×1024 with 9 mips, 1 441 792 bytes.
- **Where it waits** (Microsoft public symbols): `CDevice::
  CreateCommittedResource` → `CreateTestResourceAndHeap_Worker` →
  `CHeap::LUCCompleteLayerConstruction` → `CDevice::MakeResident_Worker` →
  `PerNodePagingOperation::Wait` → `D3DKMTWaitForSynchronizationObjectFromCpu`.
  The runtime waits on the CPU for VidMm's paging fence of the new heap,
  **holding the device's residency mutex**: every other resident creation of
  the device queues behind it.
- **It is slow, not stuck**: once past the threshold below, each creation
  takes 0.3 to 3.6 s, about 1.5 s on average in game. The world load needs
  thousands of them, the title stops presenting, and its watchdog kills it
  (write to 0 at RVA 0x23cf683) about 30 s later.
- **Why**: once the VRAM resident in the local segment, **all processes
  together**, passes about 9.5–9.75 GB of 15.67 (VidMm's free pages under
  ≈ 5.9 GB), VidMm admits new resident allocations at about **one per 0.5 to
  2 s, whatever their size**, with no eviction and no transfer. A D3D12 probe
  reproduces it without the title or XTP (§3). The DXGI budget stays at
  14.92 GB, so `DENY_OVERBUDGET` never triggers.
- **Not the machine's state**: emptying the RAM lists, restarting the
  display device (new adapter, VidMm counters back to zero) and disabling
  the AMD iGPU leave the threshold where it was (§3, §5). The sessions that reached the world before
  13:37Z were already loading with frames 2.8 to 17.7 s apart: they passed
  under the watchdog narrowly.
- **The fix is in XTP** (§6): stop paying one admission per texture. Host
  textures go into large heaps (≥ 256 MB) reserved ahead of need, off the
  title's thread (task tex-pool, 450ec9ba), and no residency wait runs on the
  title's thread. dark-world's NOT_RESIDENT path, landed on main, only bounds
  the wait (5 s), and each wait is shorter than that (sessions R, S).

## 2. The call that waits

### 2.1 The stack

Session M (`E:\xtp-data\runs\play-20261124-153411-M-stall-stacks`, binary
`2d0257e`, sha256 7c15cc28…): the title thread (priority 9) in every
snapshot after the last frame, symbolised with cdb against the PDB of that
build and Microsoft's public symbols:

| frame | symbol |
|---|---|
| win32u+0x60e4 | `NtGdiDdDDIWaitForSynchronizationObjectFromCpu`+0x14 (after `syscall`) |
| D3D12Core+0x118dd | `CallAndLogImpl<D3DKMTWaitForSynchronizationObjectFromCpu>`+0x1d |
| D3D12Core+0x15541 | `PerNodePagingOperation::Wait`+0x8d |
| D3D12Core+0x14c6f | `CDevice::MakeResident_Worker`+0xbf |
| D3D12Core+0x14b29 | `CHeap::LUCCompleteLayerConstruction`+0x5d |
| d3d12+0x2a84, +0x171e | `NOutermost::CDeviceChild::LUCCompleteLayerConstruction`, `NOutermost::CDevice::CreateLayeredChild` |
| D3D12Core+0x83a40, +0x83e17, +0x61fed | `CreateTestResourceAndHeap_Worker`, `CreateCommittedResource_Worker`, `CDevice::CreateCommittedResource` |
| xtp+0x1800dd | `hand_out_resource`+0x22d (resource.rs:934, `host_texture` inlined) |
| xtp+0x17fdfb | `create_at_address`+0x1fb (resource.rs:1130) |
| title+0x193dffe | return from the title's call of device method 0x428 |

`MakeResident_Worker` takes the device's residency mutex, issues the
`MakeResident` of the new heap (through the driver callback), then waits for
the paging fence value it got, and only then unlocks. No NVIDIA frame is on
this thread: the wait is the runtime's, on a kernel fence. With
`CREATE_NOT_RESIDENT` and `EnqueueMakeResident` (dark-world, now on main)
the creation itself is fast, and the same admission is waited for on a
fence in `residency::make_resident`.

### 2.2 What XTP creates there (lead a)

`create_at_address` (method 0x428: address, `D3D12_RESOURCE_DESC1`, initial
layout, clear value) keeps the title's memory as the resource's backing and,
for a texture, makes one host copy with `CreateCommittedResource`. At
`2d0257e` that is the only D3D12 call on this path: no `CreateHeap`,
`OpenExistingHeapFromAddress`, `MakeResident` or `Evict`, and nothing for a
buffer. The title's own reading of the call site is in dark-world's report
(`dark-world.md`): the description is built by title+0xc4b010, layout 6.

From `xtp_gpu::resource=debug` (sessions O to T), the textures are mostly
1024×1024 and 2048×2048 BC7/BC1/BC4/BC5 (formats 98, 71, 78, 83) with full
mips, plus 512² and 256² ones; 1.4 MB and 5.6 MB for the two most common.
The title asks `GetResourceAllocationInfo` for each first (64 KiB aligned with
its layout 266, 4 KiB without). About 6 700 resources at an address are made
before the stall, of which 3 086 host textures, 8 141 MiB, are alive at the
stall (session R).

### 2.3 Locks (lead b)

No XTP lock is held when the title calls 0x428 (at `2d0257e`, `texture_at`
held `PLACED_TEXTURES` around its own creation; dark-world moved that
creation out of the lock). In session M the eight pipeline workers are idle
(one in `recv()` holding the `Mutex<Receiver>`, make.rs:192, seven waiting
for that mutex), the 32 DirectStorage threads and the title's submission
thread are idle, and xtp-caption waits in `SetWindowTextW` because the title
thread that owns the window no longer pumps. The only lock that matters is
the D3D12 runtime's residency mutex, held through each wait.

### 2.4 Timing

The title's "resource at an address" calls run at about 900 per second
during the front-end (4 500–4 600 in 5 s) and in the first burst of the world
load (1 600–2 000 in 5 s; xtp's VRAM goes 4.4 → 8.3 GB in ~4 s), then 2 to 9
per 5 s until the title dies. dark-world's line in session S: "a host texture
was slow to make" 205 ms, 1 MiB, budget 15 277 MiB, usage 8 530 MiB.

## 3. Why it waits: reproduced without the title

`E:\tools\stall-probe\alloc_probe.py`: a fresh D3D12 process on the 4080
creates committed DEFAULT resources one after the other and times each call
(under `E:\xtp-data\measure.lock`, no game running, Discord quit):

| probe (UTC) | resources | fast up to | past it, per call |
|---|---|---|---|
| 14:58 | BC7 1024² 9 mips (1.4 MB) | 8.18 GB | 814 ms |
| 14:59 | 64 MB buffers | 8.0 GB | 482–598 ms |
| 15:05–15:08 (×3) | BC7 1.4 MB | 8.16–8.37 GB | 316–1 212 ms |
| 15:15 | 256 MB buffers | 8.5 GB | 0.6–2.1 s each; admitted up to 10.5 GB |
| 15:20 | A holds 6.0 GB, B allocates 64 MB buffers | B: 2.44 GB | 717–739 ms |
| 15:22 | BC7 1.4 MB, `CREATE_NOT_ZEROED` | 8.28 GB | 308–1 395 ms |
| 15:27 | BC7 1.4 MB, right after Windows Memory Cleaner emptied the RAM lists (free/zero pages 0.37 → 40.3 GB, standby 41 → 2.7 GB, modified 4.9 → 0.02 GB) | 8.13 GB | 325–577 ms |
| 15:34 | BC7 1.4 MB, right after `pnputil /restart-device` of the 4080 (new adapter LUID 0x197347a5, VidMm counters back to zero) | 8.0 GB | 0.19–1.3 s |
| 15:41 | BC7 1.4 MB, the AMD iGPU disabled (`pnputil /disable-device`, device in Error) | 8.09 GB | 1.05–2.67 s; plateau 9.32 GB resident, 6.35 GB free |

Below the threshold a call takes 0.3 ms (texture) to 2–7 ms (buffer).

VidMm's own counters (`D3DKMTQueryStatistics`, no admin rights,
`kmt_adapter.py`), local segment of the 4080 (limit 15.67 GB):

- idle: 1.34 GB resident (dwm ~1.4 GB and the desktop); page lists:
  allocated 0.81, free 14.34, modified 0.52, zero 0, standby 0 GB;
- filling: +1.75 GB resident per 0.5 s, each new allocation filled (zeroed)
  by the paging engine;
- at the threshold: 9.47–9.86 GB resident, **free pages 5.81–6.03 GB**,
  and nothing moves: no eviction, no transfer, 1 to 3 trims of a few bytes
  per 0.5 s while each call waits;
- past it (256 MB probe): resident climbs to 11.6 GB in 256 MB steps, one
  per ~1 s, still with no eviction.

So it is not memory exhaustion (6 GB free per VidMm and per nvidia-smi), not
the budget (14.92 GB, usage 8.2–8.5), not the number of allocations (64 MB
and 256 MB buffers hit it too), not zeroing (`CREATE_NOT_ZEROED` changes
nothing), not per process (two processes share it), not system RAM (lead c:
emptying the standby and modified lists leaves it where it was; commit
53–82 GB of 97 in every run), not the driver's state (lead d: a reloaded
driver and a new adapter object have it too), and not the hybrid setup (the
AMD iGPU disabled, same threshold). It is an admission rate VidMm
(or the driver) applies once its free local pages fall under ≈ 6 GB, about
38 % of the card. Its exact reason was not seen: it needs an elevated ETW
trace (§6.3). The cost of one admission barely depends on its size: past the
threshold, 256 MB buffers are admitted at about 210 MB/s, 1.4 MB textures
at about 0.9 MB/s.

## 4. In game

| session (UTC) | build, data | outcome | VRAM at the stall |
|---|---|---|---|
| M, N (14:33, 14:38) | `2d0257e`, golden | no frame after the load burst, title kills itself | xtp 8.3 GB dedicated |
| O (14:43) | main `e1a4d29`, autopilot | same; creations 0.6–3.6 s | xtp 8.39 GB |
| Q (14:59) | `c7a4220`, Discord quit | same | adapter 9.98 GB |
| R (15:11) | dark-world `d6edb00` (NOT_RESIDENT), golden, autopilot | not reached, title killed at +132 s; creations 1.4–1.7 s, no budget or timeout line | local 9.73 GB, 3 086 host textures, 8 141 MiB |
| S (15:16) | `d6edb00`, **fresh data**, Enter every 2.5 s | not reached, killed at +81 s; 6 717 creations, same profile as R | local 9.75 GB |
| T (15:37) | main `85e37c3`, `XTP_WATCHDOG_HOOK=1`, golden, autopilot, 6 min allowed | cut at +126 s by the iGPU test (§4.1); 6 792 creations | local 9.81 GB, 5.86 GB free |

The save is not the cause (S starts a new game), nor Discord (Q, probes),
nor the pipeline library, audio, TMP, focus or keys (run-world's
bisection).

### 4.1 Without the title's watchdog (session T)

Session T applied the hook (`title watchdog hook applied … sites=3`) to see
whether the load ends, slowly, when the title no longer kills itself. It
was cut short: at 15:39:53Z, disabling the iGPU closed the game's window
("window closed: suspending the title"), and the stalled title could not
suspend in time, so XTP ended it (exit code 0). In its ~90 s past the
threshold it made 77 more resources at an address (6 715 → 6 792, about
1.1 s each) and did not reach the world. Releasing its VRAM at the end also
took ~50 s. Whether the load finishes without the watchdog is still open; a
6-minute rerun, with no display change during it, answers it.

## 5. Why earlier sessions passed

The same binary reached the world in test1 (13:20Z), C (13:25Z) and E
(13:33Z). Their loads were already very slow: E presented frames 2.8 to
17.7 s apart for 2.5 min before its first world frame (~13:36:10Z), C up to
17.5 s, test1 up to 5.5 s. The title's watchdog fires after roughly 17 to
35 s without a frame (run-world: 17.5 s survived, 35 s killed). The same
admission rate was most likely already at work, and these sessions passed
narrowly. Since then every load has crossed the watchdog's limit. Past the
threshold each 100 MB of headroom is worth about 70 throttled 1.4 MB
creations, about 100 s, so small changes are enough: other processes'
VRAM (dwm, the Hangar app, Discord's stream from about 13:35Z), and XTP's own
allocations between builds.

What was ruled out as a state that appeared at 13:37Z: the RAM lists
(cleaned, §3), the driver and VidMm's adapter object (device restarted,
§3), Discord (quit), and WSL (a VM ran 13:30:27–13:31:50Z, before E, which
passed). The nvlddmkm 153 errors (13:12, 13:16, 13:22, 13:46Z) were left
behind by the restart. Only a full reboot remains untested; it would only
matter if the cause were a global Windows state that survives a device
restart.

## 6. Fixes

### 6.1 In XTP (fix tasks)

1. **Host textures in large heaps reserved ahead of need** (task tex-pool,
   450ec9ba). Past the threshold, the cost is per allocation, so heaps of
   256 MB or more admit about 200 times more per second than one committed
   texture each. Reserve them off the title's thread, from device creation
   and while the load is still under the threshold, and grow the pool ahead
   of demand; textures are then placed resources, which need no
   `MakeResident`.
2. **No residency wait on the title's thread.** dark-world's bounded wait
   (5 s) never fires, since each wait is about 1.5 s. Making textures
   resident asynchronously, with the GPU waiting on the residency fence
   before the first submission that samples them, lets the load go on.
3. **Diagnostics**: log VidMm's free local pages (`D3DKMTQueryStatistics`,
   segment group usage) with slow creations and in the frame line. The DXGI
   budget does not show this limit.
4. **Footprint**: 8.1 GiB of host textures are alive at the stall; measure
   what the title really samples, and release host copies the title frees.

### 6.2 For the person (machine)

Nothing to change on the machine is known to help: closing Discord, cleaning
RAM, restarting the display device and disabling the iGPU were tried. A full
reboot remains untested. During a session, no display may be disabled or
enabled: it closes the game's window and XTP ends the title (session T).

### 6.3 Optional: the reason inside VidMm

An elevated ETW trace during the probe would show which VidMm path holds the
allocations: `wpr -start GPU -filemode`, then
`python -I E:\tools\stall-probe\alloc_probe.py --cap-gb 12 --max-seconds 15`,
then `wpr -stop E:\xtp-data\runs\stall-probe\vidmm.etl`, opened in WPA or
GPUView. It needs an administrator; the person runs it.

## 7. Tools (on E: only) and how to reuse them

- `E:\tools\windbg\amd64\`: the x64 debuggers (cdb, kd, dbgeng; WinDbg
  1.2611.4001.0) extracted from Microsoft's official package, without
  installing it (no registry, nothing on C:). Symbolise an offset: set
  `_NT_SYMBOL_PATH=srv*E:\tools\symbols*https://msdl.microsoft.com/download/symbols`,
  then `cdb -z C:\Windows\System32\D3D12Core.dll -c ".reload /f; ln
  D3D12Core+0x15541; q"`; for XTP, `cdb -z <target>\diag\xtp.exe -y
  <target>\diag -c ".reload /f xtp.exe; .lines -e; ln xtp+0x1800dd; q"`.
  Stacks of a live process without stopping it: `cdb -pv -p <pid> -c "~*k;
  q"`.
- `E:\tools\symbols\`: the symbol cache.
- `E:\tools\stall-probe\` (README.txt there): `alloc_probe.py` (the probe;
  `--kind texture|buffer`, `--buffer-mb`, `--cap-gb`, `--stop-ms`,
  `--max-slow`, `--max-seconds`, `--heap-flags`, `--enqueue` for
  NOT_RESIDENT + `EnqueueMakeResident`); `run_probe2.ps1` (the probe under
  measure.lock with VidMm counters every 0.5 s); `memclean_probe.ps1` (the
  same, plus RAM lists before and after); `two_probes.ps1` (per process or
  global); `kmt_stats.py` and `kmt_adapter.py` (VidMm segments, page lists,
  evictions, without admin rights); `vram_budget.py` (DXGI budget);
  `gpu_sampler.ps1` (VRAM and engine use per process, nvidia-smi);
  `session.ps1` (`cargo xtask play` from a worktree with the samplers and a
  hard stop). Python is the person's existing 3.12.
- Not installed: REA, a JDK and Ghidra. The cause is in VidMm, not in the
  title, and the title's call site had already been read (dark-world). No
  MCP configuration was changed.
