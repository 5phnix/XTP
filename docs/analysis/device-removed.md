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
- **Where the GPU stopped, on both machines.** In a host
  acceleration-structure build of XTP's own `rt::Builder`, not in a title
  dispatch: on the PC list 0 of 26 in flight, command 310 of 638; on the
  L4 list 0 of 19, command 184 of 394, both
  `BUILDRAYTRACINGACCELERATIONSTRUCTURE`. DRED 1.2 gives the L4's device
  state: hung. The host RT commands carried no marker, so neither report
  names the build; task ec5a7ada adds markers to them (§3, §4).
- **DRED in XTP was already there** (`XTP_D3D12_DRED=1`, diagnostics
  builds). This task adds DRED 1.2 to its report (`dred.rs`, §2): the
  device state, each stopped list's last finished and first unfinished
  command with the text of the marker before it, and the address ranges of
  the allocations at a page fault.
- **DRED is slow on the L4**, about five times: the loss comes 24 to 41
  frames into the world, which takes a DRED session about 15 minutes of
  world there (§4).
- **The likely cause: BLAS builds given indices past their vertices.**
  With task ec5a7ada's check of the build inputs (4a0cdb3), the L4 ran
  20 minutes of world, about 70 frames, without losing the device, while
  4,096 to 8,191 BLAS builds were refused for an index past their vertices
  (index 65401 for 529 vertices, 63487 for 105, …). Before 4a0cdb3 nothing
  refused them, and the driver read past their vertex buffers (§5).
- **The check now reads what the build reads** (Codex review 11f27db1,
  §6): on the GPU, after every write of the submission before the build,
  instead of title memory when the build is recorded. Two more defects of
  the review are fixed: a batch entry that recorded no build no longer
  skips the next build's UAV barrier, and a TLAS whose empty-TLAS stand-in
  cannot be made is refused instead of naming a null BLAS.

## 2. Device-removed data in XTP

`XTP_D3D12_DRED=1` in a diagnostics build:

- before the host device is made, `host::create` turns on DRED's automatic
  breadcrumbs and page-fault data (`ID3D12DeviceRemovedExtendedDataSettings`),
  and `dred::enable` again through `ID3D12DeviceRemovedExtendedDataSettings1`;
  with `XTP_D3D12_DRED=contexts` it also turns on breadcrumb contexts, with
  which the runtime keeps the text of each marker with its breadcrumb.
  They are off otherwise: on the L4 they added about 3 s to a world frame
  and came back empty (session `dred-12-long`, §4);
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

The marker texts come from DRED's breadcrumb contexts when it kept any,
and otherwise from our notes, the k-th note for the k-th marker command
(the way `Host::removal_data` pairs them); the report says which. With no
list stopped part way (DRED's counts may lag the GPU, xcl#1592), it names
the last command of the last finished list and the first command of the
first list not started.

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
| dred-12-b | this branch (1b24311) | on, with contexts | 13:54:09, frame 2421 | about 15 (16–24 s a frame) | no, stopped at 420 s |
| dred-12-long | this branch (1b24311) | on, with contexts | 14:01:26, frame 2567 | about 50 (about 20 s a frame) | **yes**, removed 14:18:14 |
| nodred-main | main 81aeecd (with the CBV cap) | off | 14:23:10, frame 2933 | about 115 (0.3 to 0.9 fps) | **yes**, removed 14:25:08, 2 lists in flight |
| debug-gbv-main | main 81aeecd | off; D3D12 debug layer and GPU-based validation | 14:31:25, frame 2028 | about 40 s | no: ended by a CPU fault in title code (+0x23cf683, a write at 0), as cxstrideon2-both did |
| dred-markers-l4 | this branch at 3706ba2 (4a0cdb3's build markers and input checks) | on | 14:38:25, frame 2400 | about 70 in 20 minutes | **no**; ended by a breakpoint exception in title code (+0x63966f) |

Without DRED the L4 runs the world at 0.3 fps (3.5 s a frame); with it,
0.1 fps, and about 0.05 fps with breadcrumb contexts. Both the time spent
preparing submissions and the time outside them grow by about 7 s a frame.
The loss follows the number of frames in the world (24 to 41), not the
clock, so a DRED session on the L4 needs about 15 minutes of world.

`dred-12-long`'s report (`E:\xtp-data\cloud-runs\20261125-145217-game`,
line 15101 of `run.log`):

- `DRED: device hung (a command did not finish in time)`; page fault VA 0;
  NVIDIA's event 153 in the System log, as on the PC;
- 458 lists in DRED's output, 431 finished, 26 not started; one stopped
  part way: list 0 of the 19 in flight, after 184 of its 394 commands. The
  last finished is command 183, a resource barrier; the first unfinished
  is command 184, `BUILDRAYTRACINGACCELERATIONSTRUCTURE`;
- commands 185 to 383 carry no marker (host RT commands); the next marked
  ones are dispatches of 969c1cef8b783b39 (5, 4 and 1 groups) at 385, 388
  and 390;
- by the order of the list's 41 notes, 37 of them come before command 184,
  the last of which is a dispatch of 3cf39f4495b908d0 (4 groups) after a
  0x20-byte root view copy. That program reads four buffers through its
  table (42,991,392 bytes at title 0x2ec40010, 1 MiB, 182 MB at title
  0x276c0000, 48 KiB) and writes a 12 MiB one;
- the breadcrumb contexts were on, and none came back: the report then
  said "no marker before it" for commands that 37 markers precede. Hence
  the pairing by order (§2).

The PC and the L4 lose the device the same way: list 0, a host build
without a marker, then dispatches of 969c1cef8b783b39. On the L4 the build
had the constant buffer view cap (760ec60); on the PC it did not.

## 5. Indices past their vertices

`dred-markers-l4` ran this branch at 3706ba2, which holds task ec5a7ada's
work (4a0cdb3): a DRED marker on every host DXR build, copy and instance
rewrite, and a check of each BLAS's inputs before its build is recorded
(`E:\xtp-data\cloud-runs\20261125-153431-game`).

- The device was not lost in 20 minutes of world, about 70 frames; every
  earlier L4 session lost it 24 to 115 frames in (§4).
- The input check refused 4,096 to 8,191 BLAS builds (its log is at powers
  of two) for an index past the geometry's vertices, from the world's
  first seconds on: index 65401 at 0x64052dc90 for 529 vertices, 63487 for
  105, 65405 for 5195, 56504 for 210, 51200 for 240, 63488 for 349. Values
  like 0xFF79, 0xF7FF or 0xF800 are not indices of such small meshes: the
  title memory read when the build was recorded either did not hold the
  indices yet, or held another kind of data.
- The run ended on a breakpoint exception in title code, not on the GPU.

So the hangs came, most likely, from BLAS builds whose index buffers held
such values when the GPU ran them: DXR leaves an index past `VertexCount`
undefined, and the driver then reads far past the vertex buffer, in the
host copy's heap, without faulting. The debug layer and GPU-based
validation said nothing about these builds (`debug-gbv-main`): neither
checks index values.

The check of 4a0cdb3 read title memory when the build was recorded, while
the build reads the host copy, refreshed at submission and possibly
written by a copy or a shader of the same submission before the build
(Codex review 11f27db1, P1-2). §6 moves it to the GPU.

## 6. Fixes (Codex review 11f27db1)

- **P1-2, the indices a build reads** (`rt/indices.rs`, `indices.hlsl`):
  before each BLAS build with indices, a pass of ours copies each indexed
  geometry's indices, as 32-bit ones, into a buffer of the build's own,
  after a UAV barrier that orders every earlier write of the submission.
  An index past its geometry's vertices raises the build's flag, and then
  every index of the build becomes 0: degenerate triangles, which DXR
  leaves inactive, so the build takes none of them. The build reads that
  buffer. The flag is copied to a read-back word and logged once the
  submission has run ("host DXR BLAS input refused on the GPU"). The CPU
  read of title memory at recording is gone (it also allocated and read a
  whole index buffer per build, Codex P2-5). Shortcut: a refused build
  still takes the triangles of its geometries without indices.
- **P1-1, a missing barrier in a batch**: `build_one` tells apart a
  structure published and a build recorded (`Mirrored`); a batch orders
  its next entry by the second. A TLAS of no instance is the shared empty
  TLAS, records no build, and leaves the next entry its own barrier.
- **P1-3, a TLAS without its stand-in**: when the empty TLAS and its
  stand-in BLAS cannot be made, the title's TLAS is refused and logged,
  instead of being built with no instance or with a null BLAS for an
  instance of a BLAS the host does not have.
- **P1-2, buffers left in the unordered-access state**: no change. The
  title's shaders write host copies through GPU addresses (root views) or
  heap views, which D3D12 does not tie to a resource: no state change is
  recorded, the copy stays in the common state for D3D12, and the build
  reads it by implicit promotion. What orders those writes before the
  build is the UAV barrier, which P1-1 restores where it was skipped and
  which the index pass records itself.
- **`XTP_D3D12_DRED=0`** (or empty) now leaves device-removed data off;
  the variable's presence alone turned it on.
- **`XTP_RT_BUILDS=0`** refuses the title's builds, copies and
  rehydrations but keeps the empty TLAS and the trace commands: a trace
  binds the empty TLAS and every ray misses, as on the console with an
  empty structure (task ac895e39's traces were dropped under 4a0cdb3).

## 7. Tests

In `rt/tests.rs` (GPU tests, through `testgpu`):

- `rt_indices_are_checked_as_the_build_reads_them`: a GPU copy in the
  build's list writes the indices' host copy before the build. Valid in
  title memory but made invalid by the copy, the build takes no triangle
  and every ray misses; invalid in title memory but made valid by the
  copy, the BLAS is built and hit (4a0cdb3 refused it).
- `rt_a_tlas_of_no_instance_leaves_the_next_build_its_barrier` and
  `rt_a_tlas_without_its_stand_in_is_refused` (P1-1, P1-3; the stand-in's
  failure is simulated per thread).
- `rt_builds_off_keeps_the_empty_tlas_and_traces_miss`.
- `rt_blas_of_known_bounds_from_title_memory` (watch item 497, an upstream
  fork's fixture #1816, reimplemented): a triangle with bounds (−5, 0, 1) to
  (4, 8, 9), its vertices and indices in title buffers, then in title
  memory outside any buffer object across a 4 MiB window boundary (XTP's
  loose window copies), built twice with the title's CPU moving it by 1
  along z in between; eight rays near its edges read the distances and
  misses each build must give. It passes: address translation and the
  refreshed copy are right on both paths, before any title geometry.

In `dred.rs`: the marker pairing by order, the report's texts, the
switches.

## 8. Checked

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
  760ec60): not the cause. `dred-12-long` (760ec60 and this branch) and
  `nodred-main` (81aeecd) have the cap and lost the device all the same
  (§4).
