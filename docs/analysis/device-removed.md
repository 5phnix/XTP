# Device removed in the world: DRED, classification, sessions

Why the host GPU is lost 35 s to 2.5 min into the world, on the person's PC
(RTX 4080 SUPER) and on the cloud L4 alike (task 61ae65c7), and what the
device-removed report says about it. Programs are named by hash only (the
first 16 hex digits of the BLAKE3 of the container the title passes, as the
runtime logs them). Times are UTC unless marked local. Logs are under
`E:\xtp-data\runs\`, `E:\xtp-data\cache\play\` (PC) and
`E:\xtp-data\cloud-runs\` (L4); none is in the repository.

## 1. Summary

- **A hang, not a page fault.** Every loss logs NVIDIA's event 153 twice in
  the System log ("Error occurred on GPUID"), the first one at the moment
  the GPU stops answering, and never event 13. The PC's DRED report (task
  ec5a7ada, session `rt2a4-dred`) has no page fault (§3).
- **Where the PC's GPU stopped.** In a host acceleration-structure build of
  XTP's own `rt::Builder` (list 0 of 26 in flight, command 310 of 638,
  `BUILDRAYTRACINGACCELERATIONSTRUCTURE`), not in a title dispatch. Its
  commands 311 to 626 carried no marker, so the report could not name the
  build; task ec5a7ada adds markers to the host RT commands (§3).
- **DRED in XTP was already there** (`XTP_D3D12_DRED=1`, diagnostics
  builds). This task adds DRED 1.2 to its report (`dred.rs`, §2): the
  device state, each stopped list's last finished and first unfinished
  command with the text of the marker before it, and the address ranges of
  the allocations at a page fault.
- **DRED is slow on the L4**, about five times: the loss comes 24 to 41
  frames into the world, and a 420 s session with DRED reaches about 20
  (§4).

## 2. Device-removed data in XTP

`XTP_D3D12_DRED=1` in a diagnostics build:

- before the host device is made, `host::create` turns on DRED's automatic
  breadcrumbs and page-fault data (`ID3D12DeviceRemovedExtendedDataSettings`),
  and `dred::enable` turns on breadcrumb contexts
  (`ID3D12DeviceRemovedExtendedDataSettings1`): the runtime then keeps the
  text of each marker with its breadcrumb;
- XTP names its host objects (`host::name`) and sets a marker before each
  draw, dispatch and copy it records (`HostList::note_op`): the operation,
  the pipeline and the hashes of its programs, the user data, root views
  and tables (`draw.rs` `op_text`);
- when the device is removed, the run ends with one report line
  (`Host::removed`): first what `dred::report` reads, then
  `Host::removal_data`'s own report.

What `dred::report` adds:

| Part | Source | Text |
| --- | --- | --- |
| Device state | `ID3D12DeviceRemovedExtendedData2::GetDeviceState` | `DRED: device hung`, `faulted` or `page fault` (the kernel's states 3, 6 and 7) |
| Allocations at a page fault | `GetPageFaultAllocationOutput2`, each node's object | name, kind, host address range of a buffer and where the fault lies in it |
| Lists | `GetAutoBreadcrumbsOutput1` | how many lists finished, were not started, and for each list that stopped part way its last finished and first unfinished commands, each with the marker before it |

With no list stopped part way (DRED's counts may lag the GPU, xcl#1592),
it names the last command of the last finished list and the first command
of the first list not started.

`Host::removal_data` keeps its own part: the faulting address and the
allocation names there, the title memory copies and descriptor heaps near
it (`shadow::near`, `descriptor::host::near_heaps`), every list's progress,
and for each stopped list the commands between the barriers around where it
stopped, with our markers paired with the breadcrumbs by order.

Device setup stays in `host.rs` (`create`): `device.rs` is the device the
title sees, not the host one.

## 3. Classification and the PC's report

NVIDIA's events in the System log at each loss (`nvlddmkm`, id 153, "Error
occurred on GPUID", the same data `0 0 0 0 2 0 48 0 0 0 0 0 153 0 …` every
time; no id 13):

| Machine | Session | Events | Loss in XTP's log |
| --- | --- | --- | --- |
| L4 | cxstrideon2-off (build 5018dd4) | 13:16:12, 13:16:28 | GPU silent from about 13:16:13; removed 13:16:28, 37 lists in flight |
| L4 | cxstrideon2-table (5018dd4) | 13:21:33, 13:21:37 | removed 13:21:37, 33 lists in flight |
| PC | -124128 | 13:42:04, :05, :09 local | 12:42:04 |
| PC | rt2a4-dred | 15:01:33, :35 local | removed 14:01:38, 26 lists in flight |

Two events per loss, the first at the start of the hang, and no page fault
in the one DRED report: the GPU stops answering and Windows resets it
(TDR). `DXGI_ERROR_DEVICE_HUNG` (0x887A0006) is the removal reason the L4
logs give.

The PC's DRED report (session `rt2a4-dred`, main 3d3cd28 with task
ec5a7ada's builder verdict; world 32 s after the start, removal 14:01:38):

- page fault VA 0, no allocation listed;
- of the 26 lists in flight, list 0 stopped at command 310 of 638, a
  `BUILDRAYTRACINGACCELERATIONSTRUCTURE` without a marker; the other lists
  had not started;
- commands 311 to 626 have no marker either: host RT commands (builds,
  barriers, copies) that `rt::Builder` records; the first marked commands
  after them are dispatches of 969c1cef8b783b39 (35, 6 and 1 groups) at
  627 and after;
- the title's BVH builder program was refused the whole session ("held …
  refused"), so the console's BVHs are never built on the GPU and
  `rt::Builder`'s host builds stand in.

## 4. Sessions on the L4

| Session | Build | DRED | World | Frames in the world | Loss |
| --- | --- | --- | --- | --- | --- |
| cxstrideon2-off | 5018dd4 | off | 13:15:20, frame 2771 | about 41 (last logged 2812) | yes, GPU silent 13:16:13 |
| cxstrideon2-table | 5018dd4 | off | 13:21:02, frame 2919 | about 24 (last logged 2943) | yes, 13:21:37 |
| cxstrideon2-both (`XTP_RUN_TIME_VIEWS=1`) | 5018dd4 | off | 13:26:04, frame 2352 | about 25–30 | no; the run ended in a CPU fault in title code at 13:29 |
| dred-760ec60-a | 760ec60 | on | 13:46:26, frame 3052 | about 20 (15–18 s a frame) | no, stopped at 420 s |
| dred-12-b | this branch | on, with contexts | 13:54:09, frame 2421 | about 15 (16–24 s a frame) | no, stopped at 420 s |

Without DRED the L4 runs the world at 0.3 fps (3.5 s a frame); with it,
0.1 fps, and 0.05 fps with breadcrumb contexts. Both the time spent
preparing submissions and the time outside them grow by about 7 s a frame.
The loss follows the number of frames in the world (24 to 41), not the
clock, so a DRED session on the L4 needs about 12 minutes of world.

## 5. Checked

- **The loop budget** (`XTP_SHADER_LOOP_BUDGET`): on by default, 2^20 back
  edges per lane across all of a lane's loops (`DEFAULT_LOOP_BUDGET`,
  xtp-shader `emit/mod.rs`), not 0. A walker of a BVH that was never built
  can still run up to that many rounds per lane, which a full-screen
  dispatch cannot finish within the reset timeout; but no walker was in
  flight in the PC's report (§3).
- **No command allocator is reused after a failed fence wait** (VIctory
  #2158): `Host::wait_gpu` has no time-out exit; on a removed device the
  fence reads all ones, and `Timeline::finish` calls `Host::complete`,
  which ends the run on the removal, before the signals that hand
  allocators back (`Retired::retire`) run; `retire` keeps only an allocator
  whose `Reset` succeeded.
- **Constant buffer views over 64 KiB** (capped in e0f8101, on main since
  760ec60): none of the sessions that lost the device had the cap (5018dd4
  and 3d3cd28 are older).
