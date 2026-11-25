# XTP's GPU tests on a hardware GPU (the cloud L4)

XTP's GPU tests had run on WARP only, on the PC and on the build farm. WARP
runs a group's waves one after the other and, at a group barrier, does not
wait for a thread that has returned, so a program that hangs a hardware GPU
can pass there. five-star-recomp found exactly that when it ran its WARP
cases on an RTX 5070: 21 of 284 differed, two mesh cases hung the GPU
(pass 7, item 143, of the maintainers' private watch notes; the idea of
running the same tests with a hardware switch is theirs). This page reports
the same exercise for XTP: `xtp-shader` and `xtp-gpu` on the Google Cloud
L4, against WARP, on the same machine and commits, on 2026-11-24.

**Result:** one difference on the L4, a real XTP bug, now fixed: integer
UAV clears whose values do not fit their channels (section 2.1). A second
one came from a contributor's Intel UHD adapter (issue #34), also a real
bug, also fixed: NaN in `v_med3`, and the same risk in `v_min`/`v_max`
(section 2.2). No test hung the GPU, none ran into its time limit, and
Windows logged no GPU reset in any run. Section 3 lists what the hardware
set cannot see yet: the tests that run on one adapter only, and the four
that run nowhere.

## 1. What ran

The machine: the `xtp-l4` VM ([cloud-setup](../dev/cloud-setup.md)), NVIDIA
L4 (Ada, 32-lane waves, like the person's RTX 4080), RTX Virtual
Workstation driver 596.86 (32.0.15.9686), Windows Server 2025, the
system's D3D12 (shader model 6.8). WARP is the same machine's.

How: `tools\cloud\run-remote.ps1 -GpuTests -Local` (new,
[`vm\test-each.ps1`](../../tools/cloud/vm/test-each.ps1)): each test of
`xtp-shader` and `xtp-gpu` (unit and integration tests, not doc tests) in a
process of its own, 8 at a time, stopped after 300 s, with
`XTP_TEST_ADAPTER=hardware` or `warp` and `XTP_REQUIRE_GPU=1`. On the L4 it
adds the 31 tests marked `#[ignore = "hardware: ..."]` (`emit::split_tests`,
`emit::lane_read_loop_tests`), which need a hardware adapter and had never
run on one. A test that did not pass runs again alone, and one that passes
after printing "skipped" counts as a skip. Beside it, the plain form, as a
developer would type it (`-Cargo 'test -p xtp-shader -p xtp-gpu --locked'
-Env @{ XTP_TEST_ADAPTER = 'hardware'; XTP_REQUIRE_GPU = '1' }`): libtest's
threads in one process, on one device.

| UTC | Commit | Adapter | Form | Tests | Pass | Skip | Fail | Time |
|-----|--------|---------|------|------:|-----:|-----:|-----:|-----:|
| 20:06 | 970eb9a (see below) | L4 | each test alone | 455 | 451 | 4 | 0 | 89 s |
| 20:21 | c135f4c (main) | WARP | each test alone | 437 | 432 | 5 | 0 | 34 s |
| 20:25 | c135f4c | L4 | `cargo test` | 232 of `xtp_gpu` (cargo stopped after it) | 231 | – | 1 | 3 s |
| 20:27 | c135f4c | L4 | `cargo test -- --ignored split_tests lane_read_loop_tests` | 31 | 31 | – | 0 | 49 s with the build |
| 20:29 | c135f4c | L4 | each test alone | 468 | 463 | 4 | 1 | 52 s |
| 20:35 / 20:53 | c4b1c0a (the new clear test, before the fix) | L4 / WARP | `-Filter uav_clears` | 3 | 1 / 3 | – | 2 / 0 | – |

The first run asked for c135f4c and built 970eb9a, another worker's commit:
every `run-remote.ps1 -Local` sent its bundle to the same file on the VM,
and a caller waiting for the VM's lock read it only once it held the lock,
so it built the bundle of whoever had sent one last (the log's `commit`
line shows it). Fixed in `9d4a630` (on main): each run sends a bundle of
its own, and the VM fails unless it checked out the commit sent. That run
is still a clean run of 970eb9a on the L4; it is not counted below.

The 468 tests of the 20:29 run are WARP's 437 plus the 31 hardware ones. The
final runs, after the fix, are in [section 4](#4-after-the-fix).

## 2. Differences

### 2.1 Integer UAV clears of values wider than their channels (fixed)

- **Test:** `xtp-gpu` `list::tests::gpu::uav_clears_of_a_texture_go_through_host_views`.
- **WARP:** passes. **L4:** `texel 2,0: [1, 2, 3, 255]` where it wants
  `[1, 2, 3, 4]` (`list.rs`, the texel check); it fails alone too, and in the
  plain `cargo test`.
- **Cause, a real XTP bug:** the test clears an R8G8B8A8_UNORM texture with
  `ClearUnorderedAccessViewUint` and the values `[1, 2, 3, 0x104]`. D3D
  documents that such a clear copies the low *n* bits of each value into its
  *n*-bit channel (0x104 gives 4), and WARP does that. NVIDIA's driver
  saturates instead. XTP passed the title's values to the host as they came
  (`clear_uav` in `list.rs`), so what a title's clear wrote depended on the
  driver. FidelityFX's upscaler (FSR4) clears UAVs this way on Series S.

What the driver does, measured with a new test,
`list::tests::gpu::uav_clears_of_typed_buffers_write_the_documented_bits`
(one typed buffer view per format, each word as the clear left it, before
the fix):

| View format | Values | D3D's rule (WARP) | L4 |
|-------------|--------|------------------|----|
| R8G8B8A8_UNORM | 1, 2, 3, 0x104 | `04030201` | `ff030201` |
| R8G8B8A8_UINT | 0x1ff, 0x80, 0x12345678, 7 | `077880ff` | `07ff80ff` |
| R8G8B8A8_SINT | -128, 383, 128, -1 | `ff807f80` | `ff80ffff` |
| R16G16_UINT | 0x12345, 0xffff0001 | `00012345` | `ffffffff` |
| R16G16_SINT | 0x18000, -2 | `fffe8000` | `ffffffff` |
| R10G10B10A2_UINT | 0x401, 0x3ff, 0x7ff, 5 | `7ffffc01` | `ffffffff` |
| R32_UINT | 0xc1ea0000 | `c1ea0000` | `c1ea0000` |

The driver clamps each value to the channel's largest *unsigned* value,
signed formats included (-128 gives 0xff in an 8-bit signed channel, not
0x80), and writes a value that fits as it is. Float clears agree on both:
NaN gives 0 and the rest is clamped then rounded, in UNORM and SNORM
(`[-0.5, 1.5, NaN, 0.25]` gives `4000ff00`, `[-2, 2, NaN, -0.25]` in SNORM
`e0007f81`, on WARP and on the L4).

**Fix** (`f1fa1ec`): before the host call, `clear_uav` cuts each value to
the bits its channel holds (`resource::format::uint_clear`, with a table of
channel widths, `channel_bits`), in the host format the clear writes: the
host texture's (its view is of the same family, whose formats share their
channels), or a typed buffer view's. Values that fit are written alike by
every driver. No sign extension: the driver would clamp a sign-extended
-128 to 0xff. Unit tests check the cuts and that each format's channels
fill its element. shortcut: a stand-in format has wider channels than the
title's, and its clear keeps the stand-in's bits.

### 2.2 NaN in med3, min and max, on a third adapter (issue #34, fixed)

- **Test:** `xtp-shader` `emit::valu_tests::v_med3_f32_matches_rdna2_with_nan`.
- **Reported** by a contributor (fogydes, issue #34) on an Intel UHD hardware
  adapter: `v_med3_f32(1.0, 2.0, NaN)` gave 2.0 where RDNA2 gives 1.0, the
  minimum of the operands that are numbers. It passes on WARP, and on the
  L4 before and after the fix (each case runs on the hardware adapter and on
  WARP within the test).
- **Cause, a real XTP bug:** the fallback for a NaN among the operands was
  `FMin(FMin(a, b), c)` (`lower::valu::med3`), so it relied on what the
  host's FMin does with a NaN, and hosts differ there. `max_min`
  (`v_min`/`v_max`, `v_min3`/`v_max3`, f32 and f16) relied on the same.
- **Fix** (`0cf7c3b`): `med3` replaces each NaN operand by one that is not
  a NaN (the first such) before the minimum, which never meets a NaN, and
  three NaNs give NaN; `max_min` gives the other operand when one is a NaN
  (two give NaN), as RDNA2 does for quiet NaNs. A host whose FMin/FMax
  already skip NaNs folds these selects. `v_med3_f16` shares `med3`.
- **Checked** on the L4 and on WARP, with a new test of `v_min`, `v_max`,
  `v_min3` and `v_max3` beside one, two and three NaNs
  (`emit::valu_tests::min_and_max_give_the_number_beside_a_nan`, `9e1a1ae`;
  none had a NaN case). No Intel adapter is in reach: the reporter's machine
  is the check for the case that failed.
- **Not changed, not in this task's files:** the VOP3 clamp modifier
  (`lower/operand.rs`, DXIL `Saturate`, which should give 0 for NaN) has no
  NaN test; its NaN result also rests on the host. `valu16.rs`'s
  `v_cvt_u16_f16`/`v_cvt_i16_f16` clamp with FMax/FMin but select 0 for
  NaN themselves: not exposed.

### 2.3 Nothing else

Every other test gives the same result on both adapters. In particular:

- the 31 hardware tests (console wave64 programs on 32-lane waves, split
  into two host waves) pass on the L4: they had never run before;
- no test reached its 300 s limit, and Windows logged no GPU reset (System
  event 4101) during any run. The longest test on the L4 is
  `emit::valu_tests::v_cvt_norm_i16_and_u16_f16_round_the_exact_product`,
  35 s (2 s on WARP): it runs its cases on the hardware adapter and on WARP
  within the test; its results are the same.

## 3. What the hardware set does not see yet

| Test | Why | Owner |
|------|-----|-------|
| `emit::diff_tests::translated_mesh_shaders_draw_as_pc_ones`, `translated_shaders_draw_as_pc_ones`, `translated_compute_matches_pc`, `float_semantics_are_the_consoles` | need the console SDK's `dxc.exe` (`XTP_CONSOLE_DXC`), on no machine of ours: they skip on WARP and on the L4. Among them was the only test that drew a translated mesh program against the PC's. | the person (a console compiler on the VM is their call: it is the SDK's) |
| `emit::lower::flow::tests::descriptor_dwords_join_data_as_undefined_on_their_edge` | created its device with `Host::new(true)`, WARP on every machine, so it did not run on the L4 in section 2's run. | done: uses `Host::test_device()` (`XTP_TEST_ADAPTER`) |
| `resource::tests::buffer_data_reaches_a_stand_in_texture_converted` | the reverse: skips on WARP ("the host has every use of B5G5R5A1"), runs and passes on the L4, whose driver lacks some. | – (as intended) |
| xtp-shader's GPU tests | returned early and passed when they got no device, whatever `XTP_REQUIRE_GPU` said (`eprintln!("...; skipped")`); xtp-gpu's failed (`testgpu::skip`). | done: routed through `emit::host::skip_gpu` (`XTP_REQUIRE_GPU`) |

five-star's two hangs were mesh programs. XTP's mesh lowering already
leaves out the `s_barrier`s of blocks that the allocation request dominates
(one lane runs them) and of programs without LDS (`lower/mod.rs`, the
mesh setup). Main gained `emit::mesh_tests` meanwhile, whose
`wave64_mesh_reads_draw_the_console_waves_values` draws a console wave64
mesh program on the test adapter: its L4 result is in section 4. Item 143's
other leads (FMA contraction, `NonUniformResourceIndex`) are outside these
tests.

## 4. After the fix

On this task's branch, with main merged (which brought `emit::mesh_tests`):
`f1fa1ec` has the clear fix, `0cf7c3b` the NaN fix on top, `9e1a1ae` the
NaN test.

| UTC | Commit | Adapter | Form | Tests | Pass | Skip | Fail | Time |
|-----|--------|---------|------|------:|-----:|-----:|-----:|-----:|
| 20:56 | f1fa1ec | L4 | each test alone | 501 | 497 | 4 | 0 | 74 s |
| 21:04 | f1fa1ec | WARP | each test alone | 470 | 465 | 5 | 0 | 25 s |
| 21:10 | 0cf7c3b | L4 | `cargo test --no-fail-fast` | 470 (244 + 3 + 223) | 470 | – | 0 | 70 s with the build |
| 21:29 / 21:34 | 9e1a1ae | L4 / WARP | `-Filter valu_tests` | 14 | 14 | – | 0 | 45 s / 7 s |

The skips are those of section 3 (the four `diff_tests`, and on WARP the
stand-in test). The new mesh tests pass on the L4, among them
`wave64_mesh_reads_draw_the_console_waves_values`, which draws.

**Still to confirm on the VM:** the whole set, each test alone, on the
delivered commit (with the NaN fix): `-GpuTests -Local`, on the L4 and on
WARP. It waited for the VM from 21:11 to 22:00 behind other workers'
sessions and was stopped for the delivery. What the NaN fix changes was
run: every `xtp-shader` and `xtp-gpu` test on the L4 in the plain form
(21:10), and the `valu_tests` alone on both (21:29, 21:34). What it did
not reach is the 31 hardware tests with the fix.

A follow-up for `tools/cloud`: a run stopped while it waits for the VM
leaves its bundle in `C:\xtp\jobs` (one was removed by hand after this
task's last run); `Set-XtpSource` could remove bundles older than a day.

## 5. Running it again

```powershell
tools\cloud\run-remote.ps1 -GpuTests -Local                  # the hardware set on the L4
tools\cloud\run-remote.ps1 -GpuTests -Local -Adapter warp    # the same tests on WARP
tools\cloud\run-remote.ps1 -Cargo 'test -p xtp-shader -p xtp-gpu --locked --no-fail-fast' -Local -Env @{ XTP_TEST_ADAPTER = 'hardware'; XTP_REQUIRE_GPU = '1' }
```

See [cloud-setup, GPU tests on the L4](../dev/cloud-setup.md#gpu-tests-on-the-l4).
Two things to know when reading the results:

- plain `cargo test` stops after the first test binary that fails: without
  `--no-fail-fast`, `xtp-shader` did not run at 20:25;
- after a run with a failing test, gcloud prints "Recommendation: To check
  for possible causes of SSH connectivity issues..." and `plink.exe exited
  with return code [1]`: that is the remote exit code, not a connection
  problem.
