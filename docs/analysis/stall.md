# The world-load stall: VidMm holds every new allocation past ~9.5 GB

Status: draft (WIP, 2026-11-24 16:25 local, before the reboot test). Why
every session since 13:37Z stalls while loading the world, measured on the
person's PC (RTX 4080 SUPER 16 GB, driver 621.01, Windows 11 26300, AMD iGPU
beside it). GB below are GiB (2^30), as the tools print them. Times are UTC
unless marked local (UTC+1). Logs and samples are under `E:\xtp-data\runs\`;
none is in the repository. Title code is named by RVA (build blake3
`b99f876b74d4a77f…`); no title bytes or listings are quoted.

## 1. Summary

- **The call**: the title's "resource at my address" device method 0x428
  (title+0x193dff8) → `xtp_gpu::resource::create_at_address`
  (resource.rs:1130 at `2d0257e`) → `hand_out_resource` → `host_texture`
  (resource.rs:934) → `ID3D12Device::CreateCommittedResource` of the host
  copy of a title texture: DEFAULT heap, `D3D12_HEAP_FLAG_NONE`, the title's
  description without console layout, for example BC7_UNORM (98) 1024×1024,
  9 mips, 1 441 792 bytes.
- **Where it waits** (Microsoft public symbols): `D3D12Core!CDevice::
  CreateCommittedResource` → `CreateTestResourceAndHeap_Worker` →
  `CHeap::LUCCompleteLayerConstruction` → `CDevice::MakeResident_Worker` →
  `PerNodePagingOperation::Wait` → `D3DKMTWaitForSynchronizationObjectFromCpu`
  (win32u+0x60e4 = `NtGdiDdDDIWaitForSynchronizationObjectFromCpu`+0x14).
  The runtime waits on the CPU for VidMm's paging fence of the new heap, and
  does so **holding the device's residency mutex** (device+0xED8): every
  other resident creation of the device queues behind it.
- **It is slow, not stuck**: past the threshold below, each creation takes
  0.3 to 3.6 s (≈ 1.5–1.7 s on average in game). The title creates about
  6 700 resources up to that point, and needs far more; its frames stop and
  its watchdog kills it (write to 0 at RVA 0x23cf683) ~30 s later.
- **Why**: once the VRAM resident in the local segment, **all processes
  together**, passes about 9.5–9.75 GB of 15.67 (VidMm's free pages below
  ≈ 5.9 GB), VidMm admits new resident allocations at about **one per 0.5–2 s,
  whatever their size**, with no eviction and no transfer. It is reproduced
  without the title or XTP by a D3D12 probe (§3). The DXGI budget stays at
  14.92 GB, so `DENY_OVERBUDGET` never triggers.
- **dark-world's NOT_RESIDENT path** (`EnqueueMakeResident` +
  bounded 5 s wait) removes the unbounded wait but not the stall: sessions R
  and S (§4) wait the same ≈ 1.5 s per texture inside `make_resident`.
- **Fix**: first the machine test (reboot, then the probe, §6.1). Whatever
  it shows, XTP must not pay one admission per texture: host textures in
  large heaps reserved ahead of need (task tex-pool), residency off the
  title's thread (§6.2).

## 2. The call that waits

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
| title+0x193dffe | the title's call of device method 0x428 |

`MakeResident_Worker` takes the device's residency mutex, issues the
`MakeResident` of the new heap, then waits for the paging fence values it
recorded before unlocking. No NVIDIA frame is on this thread: the wait is the
runtime's, on a kernel fence.

Other threads (lead b): no XTP lock is held on this path. The eight pipeline
workers are idle (one in `recv()` holding the `Mutex<Receiver>`, make.rs:192,
seven waiting for that mutex). xtp-caption waits in `SetWindowTextW` because
the title thread that owns the window no longer pumps: a consequence.

Timing, from `xtp_gpu::resource=debug` logs (sessions O, P, Q, R, S): the
title's "resource at an address" calls run at thousands per second during
the front-end (4 500–4 600 in 5 s) and in the first burst of the world load
(1 600–2 000 in 5 s, VRAM of xtp 4.4 → 8.3 GB in ~4 s), then 2 to 9 per 5 s
until the title dies. dark-world's line in S: "a host texture was slow to
make" 205 ms, 1 MiB, budget 15 277 MiB, usage 8 530 MiB.

## 3. Why it waits: reproduced without the title

`E:\tools\stall-probe\alloc_probe.py`: a fresh D3D12 process on the 4080
creates committed DEFAULT resources one after the other and times each call
(run under `E:\xtp-data\measure.lock`, no game running, Discord quit):

| probe (UTC) | resources | fast up to | past it, per call |
|---|---|---|---|
| 14:58 | BC7 1024² 9 mips (1.4 MB) | 8.18 GB | 814 ms |
| 14:59 | 64 MB buffers | 8.0 GB | 482–598 ms |
| 15:05–15:08 (×3) | BC7 1.4 MB | 8.16–8.37 GB | 316–1 212 ms |
| 15:15 | 256 MB buffers | 8.5 GB | 0.6–2.1 s each, admitted up to 10.5 GB |
| 15:20 | A holds 6.0 GB, B allocates 64 MB buffers | B: 2.44 GB | 717–739 ms |
| 15:22 | BC7 1.4 MB, `CREATE_NOT_ZEROED` | 8.28 GB | 308–1 395 ms |
| 15:27 | BC7 1.4 MB, right after Windows Memory Cleaner emptied the RAM lists (free/zero pages 0.37 → 40.3 GB, standby 41 → 2.7 GB, modified 4.9 → 0.02 GB) | 8.13 GB | 325–577 ms; VidMm plateau 9.64 GB resident, 6.03 GB of free VRAM pages |

Below the threshold a call takes 0.3 ms (texture) to 2–7 ms (buffer).

VidMm's own counters (`D3DKMTQueryStatistics`, no admin rights;
`kmt_adapter.py`), local segment of the 4080 (aperture 0, limit 15.67 GB):

- idle: 1.34 GB resident (dwm ~1.4 GB and the desktop); page lists:
  allocated 0.81, free 14.34, modified 0.52, zero 0, standby 0 GB;
- filling: +1.75 GB resident per 0.5 s, each new allocation filled (zeroed)
  by the paging engine;
- at the threshold: 9.47–9.86 GB resident, **free pages 5.81–5.94 GB**,
  and nothing moves: no eviction, no transfer, 1 to 3 trims of a few bytes
  per 0.5 s while each call waits;
- past it (256 MB probe): resident climbs to 11.6 GB in 256 MB steps, one
  per ~1 s, still with no eviction.

So it is not memory exhaustion (6 GB free per VidMm, 6 GB free per
nvidia-smi), not the budget (14.92 GB, usage 8.2–8.5), not the number of
allocations (64 MB and 256 MB buffers hit it too), not zeroing
(`CREATE_NOT_ZEROED` changes nothing), not per process (two processes
share it), and not system RAM (emptying the standby and modified lists
leaves it where it was). It is an admission rate VidMm (or the driver) applies once its
free local pages fall under ≈ 5.9 GB. The kernel side of that decision was
not seen: it needs an elevated ETW trace (§6.1).

## 4. In game, with and without dark-world's change

| session | build, data | outcome | VRAM at the stall |
|---|---|---|---|
| M, N (14:33, 14:38) | `2d0257e`, golden | no frame after the load burst, title kills itself | xtp 8.3 GB dedicated |
| O (14:43) | main `e1a4d29`, autopilot | same; creations 0.6–3.6 s | xtp 8.39 GB |
| Q (14:59) | `c7a4220`, Discord quit | same | adapter 9.98 GB |
| R (15:11) | dark-world `d6edb00` (NOT_RESIDENT), golden, autopilot | not reached, title killed at +132 s; creations 1.4–1.7 s, no budget or timeout line | local 9.73 GB, 3 086 host textures, 8 141 MiB |
| S (15:16) | `d6edb00`, **fresh data**, Enter every 2.5 s | not reached, killed at +81 s; 6 717 creations, same profile as R | local 9.75 GB |

The save is not the cause (S starts a new game), nor Discord (Q, probes),
nor the pipeline library, audio, TMP, focus or keys (run-world's
bisection).

## 5. Why earlier sessions passed (not settled yet)

The same binary reached the world in C (13:25Z), test1 (13:20Z) and E
(13:33Z). Their loads were already very slow: E presented frames 2.8 to
17.7 s apart for 2.5 min before its first world frame (~13:36:10Z), C up to
17.5 s; the title's watchdog fires past roughly 30 s without a frame. Two
explanations remain, and the reboot test separates them:

1. **Machine state**: the threshold or the admission latency got worse
   around 13:36Z (E was killed in the world by play.ps1's timeout at
   13:36:33Z; the driver logged nvlddmkm 153 errors at 13:12, 13:16, 13:22
   and 13:46Z; no driver restart since boot at 12:47 local; a WSL VM ran
   13:30:27–13:31:50Z, before E, which passed). A reboot then lifts the
   threshold.
2. **Same throttle all day**: C and E passed narrowly, and small changes
   since (other processes' VRAM, XTP's own allocations) push every load past
   the watchdog. Each 100 MB less of fast headroom adds ~70 throttled
   1.4 MB creations, about 100 s.

## 6. Fixes

### 6.1 For the person (machine)

Reboot (planned 16:30 local). Then, before any session: the probe
(`powershell -File E:\tools\stall-probe\run_probe2.ps1 -Name after-reboot
-ProbeArgs "--cap-gb 13 --stop-ms 300 --max-slow 4"`). If it creates 13 GB at
~0.3 ms per call, the state was the cause; then one autopilot session.
If the threshold is still ~9.5 GB, an elevated trace would show VidMm's
reason: `wpr -start GPU -filemode`, the probe, `wpr -stop
E:\xtp-data\runs\stall-probe\vidmm.etl` (run by the person, admin).

### 6.2 For XTP (code, other tasks)

1. **Host textures in large heaps** (task tex-pool, 450ec9ba): past the
   threshold the cost is per allocation, so 256 MB heaps admit ~210 MB/s
   against ~0.9 MB/s for 1.4 MB textures. Reserve the heaps ahead of need,
   off the title's thread (at device creation and while loading is below
   the threshold); placed resources need no `MakeResident`.
2. **No residency wait on the title's thread**: dark-world's bounded wait
   (5 s) never fires because each wait is ~1.5 s. Making textures resident
   asynchronously, with the GPU waiting for the residency fence before the
   first submission that uses them, lets the load go on.
3. **Diagnostics**: log VidMm's free local pages (segment group usage,
   `D3DKMTQueryStatistics` type 17) with slow creations; the DXGI budget does
   not show this limit.
4. Measure the host-texture footprint during the load (8.1 GiB alive at
   the stall) against what the title really samples.

## 7. Tools (E: only)

- `E:\tools\windbg\amd64\`: the x64 debuggers (cdb, kd, dbgeng; WinDbg
  1.2611.4001.0) extracted from Microsoft's official package, without
  installing it. Symbolise an offset: set
  `_NT_SYMBOL_PATH=srv*E:\tools\symbols*https://msdl.microsoft.com/download/symbols`,
  then `cdb -z C:\Windows\System32\D3D12Core.dll -c ".reload /f; ln
  D3D12Core+0x15541; q"` (for XTP, `-z <target>\diag\xtp.exe -y <target>\diag`).
  Stacks of a live process without stopping it: `cdb -pv -p <pid> -c "~*k; q"`.
- `E:\tools\symbols\`: the symbol cache.
- `E:\tools\stall-probe\`: the probe, VidMm readers, GPU sampler and
  session launcher (README.txt there).
- Not installed: REA, a JDK and Ghidra. The cause is in VidMm, not in the
  title, and the title's call site was already read (dark-world). No MCP
  configuration was changed.

## 8. Next steps

After the reboot: the probe, then one autopilot session with
`xtp_gpu::resource=debug` and the samplers (`session.ps1`), then this
report completed with the answer to §5.
