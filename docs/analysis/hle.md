# HLE system libraries: what XTP keeps from VIctory, what it rewrites after xcl

Status: analysis. Inputs: XTP at the VIctory import (VIctory
`ffe310e`, crates since renamed `victory-*` → `xtp-*`), xcl at `47efa2e`
(`E:/xtp-refs/xcl`), and the import tables of a title folder
(the title executable and the DLLs beside it, read-only).

xcl is GPL-2.0-only and XTP GPL-3.0-or-later: nothing below is to be copied
from xcl. Each xcl reference names a file and a function so a worker can read
the **behaviour** there; the implementation is written fresh in XTP's own
style (`xtp-hle` module layout, `hle_export!`, `tracing`).

Out of scope here (other reports): the loader and import stubs, D3D12.X and
`xg_xs`, shaders, BCPack/texture formats, DirectStorage, tooling.

## 1. Scope and files

### XTP (VIctory) — `crates/xtp-hle`

| Area | Files | Lines | Unit tests |
|------|-------|------:|-----------:|
| Resolution, host pass-through | `lib.rs`, `registry.rs`, `host.rs` | 366 | 6 |
| Process state, manifest | `runtime.rs` | 226 | 1 |
| kernel32/kernelbase overrides | `modules/kernel.rs` | 687 | 3 |
| Life cycle, window, app state | `modules/lifecycle.rs` | 438 | 4 |
| Console memory (`xmem.dll`) | `modules/xmem.rs` | 1242 | 10 |
| Virtual drives (path rewriting) | `modules/files.rs` | 941 | 5 |
| Runtime entry points, `QueryApiImpl` | `modules/xgameruntime.rs` | 344 | 1 |
| XTaskQueue / XAsync | `modules/xasync.rs` | 1337 | 7 |
| XUser | `modules/xuser.rs` | 425 | 2 |
| XGameSave | `modules/xgamesave.rs` | 1355 | 7 (one is a kill-at-every-step child-process test) |
| Persistent local storage | `modules/storage.rs` | 240 | 1 |
| XSystem, analytics, device-type registry | `modules/xsystem.rs` | 400 | 4 |
| XGame (title id, features, invites, events) | `modules/xgame.rs` | 241 | 2 |
| XPackage | `modules/xpackage.rs` | 371 | 1 |
| XStore | `modules/xstore.rs` | 479 | 1 |
| XGameUi | `modules/xgameui.rs` | 321 | 1 |
| Accessibility / speech | `modules/speech.rs` | 151 | 1 |
| HDR, URIs, display deferrals, screenshots | `modules/display.rs` | 195 | 1 |
| XNetworking | `modules/xnetworking.rs` | 210 | 1 |
| Winsock / WinHTTP gate | `modules/net.rs` | 1069 | 1 |
| Audio processor (`AcpHal.dll`), XMA contexts | `modules/audio.rs`, `modules/audio/capture.rs` | 1396 | 5 |
| Audio decoder (`xapu.dll`, Opus via `opus-pure`) | `modules/apu.rs` | 524 | 1 |
| Audio DSP (`chad.dll`) | `modules/dsp.rs` | 314 | 1 |
| Media Foundation source resolver | `modules/media.rs` | 531 | 2 |
| Input (`GameInput.dll`) | `modules/gameinput.rs` | 1615 | 7 |
| XMA2 decoder | `crates/xtp-formats/src/xma.rs` (+ tables) | ~2100 | 7 (one opt-in vs. a reference decode) |

Integration: `tests/fixtures/hle-probe` (a stand-in DLL importing
`XGameRuntimeInitialize` by name, for loader tests); `crates/xtp-hle/tests/
no_dll_planting.rs`. Nothing exercises the runtime's thunk protocol from a
title end to end.

### xcl — `crates/xcl/src`

| Area | Files | Lines | Unit tests |
|------|-------|------:|-----------:|
| Routing, registry | `hle/mod.rs`, `resolver.rs`, `stubs.rs` | 1227 | 9 |
| kernelx (kernel32 family, memory, registry, affinity, app state) | `hle/kernelx.rs`, `hle/kernelx/lazy.rs`, `hle/kernelx/placeholders.rs` | 3116 | 32 |
| Thunk protocol, objects from the GDK table | `hle/xgameruntime/api.rs`, `gdk_table.rs` (generated) | 2460 | 16 |
| By-name runtime functions (XUser, XSystem, XThread…) | `hle/xgameruntime/mod.rs` | 1169 | 9 |
| XTaskQueue / XAsync | `hle/xgameruntime/taskqueue.rs`, `xasync.rs` | 2334 | 24 |
| XGameSave | `hle/xgameruntime/gamesave.rs` | 1137 | 10 |
| XNetworking | `hle/xgameruntime/networking.rs` | 886 | 7 |
| XAccessibility | `hle/xgameruntime/accessibility.rs` | 217 | 3 |
| Token and signature | `hle/xgameruntime/xsign.rs` | 344 | 4 |
| Offline network | `hle/netoff.rs` | 333 | 2 |
| Window (console screen) | `hle/user.rs` | 295 | 3 |
| Input | `hle/gameinput.rs`, `hle/kbm.rs` | 1955 | 15 |
| Audio processor, decoder, DSP | `hle/acphal.rs` | 1296 | 7 |
| XAudio2 with compressed voices | `hle/xaudio2.rs` | 1789 | 9 |
| Opus (Windows `MSOpusDecoder.dll`) | `hle/opus.rs` | 2049 | 13 |
| XMA1/XMA2 decoder | `hle/xma2/` | 2729 | 7 |
| Media Foundation helpers | `hle/mfplat.rs` | 364 | 3 |

Integration: `tests/testtitle` (a console title of xcl's own, run by
`crates/xcl/tests/e2e.rs`), `tests/pls_quota.rs`, opt-in tests on real
inputs (`XCL_SAMPLE_DLLS`: the game's libHttpClient initialises through the
thunks; `XCL_SAMPLE_AWC`: a game sound decodes). Docs read:
xcl's design notes (`docs/specs/`), `docs/xnetworking-rust-review.md`,
`HANDOFF.md` (sections on XGameRuntime, audio, retail bring-up).

## 2. What the title asks for

Console-only DLLs in the import tables of the title folder (static and delay-load):

| DLL | Imported by | Functions | XTP | xcl |
|-----|-------------|-----------|-----|-----|
| `xmem.dll` | exe (12), Bink, BVH builder, FSR | XMem alloc/free, physical pages, scatter, transfer, statistics, tools memory | all 12 | 11 (`XMemIsToolsMemAvailable` becomes a stub) |
| `AcpHal.dll` | exe | 9: heap, address maps, SHAPE contexts, `AcpHalCreate` | all | all |
| `xapu.dll` | exe | 5: connect, enqueue, dequeue, count, disconnect | all, Opus decoded | all, Opus decoded |
| `chad.dll` | exe | 5: XDsp connect, activate, submit, deactivate, disconnect | all, output silent | all, nothing computed |
| `GameInput.dll` | exe | ordinal 1 (`GameInputCreate`) | yes | yes |
| `XAudio2_9.dll` | Bink2Scarlett | ordinal 1 | host pass-through | wrapped (XMA/Opus voices) |
| `MFPlat.DLL`, `MFReadWrite.dll` | exe | 6 + 1 | host, local-only resolver | host, D3D12 device manager helper |
| `WS2_32.dll`, `WINHTTP.dll` | exe, XCurl, libHttpClient | 25 + 15 | gated, always offline | gated only with `XCL_NETWORK=0` |
| `xgameruntime.dll` | nobody imports it | loaded by the GDK thunk library: `InitializeApiImpl*`, `QueryApiImpl`, `XErrorReport` | yes | yes |

The title reaches **every** runtime API through `QueryApiImpl(class, iid)`
and a vtable slot; there are no by-name runtime imports. Notable Windows
imports whose console meaning may differ and that XTP passes to the host
today: `SetThreadGroupAffinity`/`GetThreadGroupAffinity`,
`GetLogicalProcessorInformation`, `GlobalMemoryStatusEx`, `VirtualAlloc`,
`CoCreateInstance`, `RegisterAppConstrainedChangeNotification`.

## 3. The two designs

**Import resolution.** Both route console DLLs to their own modules and pass
shared Windows API sets to the host. XTP decides per DLL with an allow list
(`HostPolicy`) and overrides only what differs; xcl routes the whole kernel
family into `kernelx` and falls back per function. Equivalent for this title;
XTP's model is smaller and already tested (`lib.rs` tests). Keep.

**Runtime protocol.** Both serve `QueryApiImpl` with COM-style objects.

- XTP: one hand-written vtable per area, slots read from the title's call
  sites; an object answers only interface ids whose layout is known; a
  missing slot logs its byte offset (`xgameruntime::missing`). Fully
  self-contained: no system runtime is loaded.
- xcl: vtables built at start from a generated table of the 396 public GDK
  functions (class, iid, slot), so functions are placed **by name**, an
  unknown iid of a known class gets the class's main vtable, and
  unimplemented calls are logged and counted by function name in
  `report.json`. Everything xcl does not implement (task queues, XThread,
  dialogs, speech, invites…) is forwarded to the PC's
  `System32\xgameruntime.dll` (Gaming Services). XStore, app broadcast and
  capture are refused.

Verdict: keep XTP's self-contained runtime (no Gaming Services dependency,
no component that could talk to Xbox Live, no foreign-handle crashes: xcl
notes the PC runtime faults on handles it did not create). Take from xcl the
**naming** of slots and the per-name report (backlog R1), not the forwarding.

## 4. Capability matrix

Legend: ✔ implemented and tested, ◐ partial, ✘ missing, — not needed.

### 4.1 kernelx (kernel32 family) and memory

| Sub-area | XTP | xcl | Tests XTP / xcl | Verdict |
|----------|-----|-----|-----------------|---------|
| Module handles, `LoadLibrary*`/`GetProcAddress` for console DLLs, own image path | ✔ pseudo handles, title DLL loader | ✔ | 3 / several | Keep XTP |
| Process exit, crash filter, `OutputDebugString` | ✔ | ✔ | — / — | Keep XTP |
| Console timer resolution | ✔ `runtime::use_console_timers` | ✔ | 0 / 1 (`fine_timer_makes_short_sleeps_short`) | Keep XTP, add a test (part of K3) |
| Device-type registry key (`ConsoleMode`) | ✔ `xsystem` registry module | ✔ | yes / yes | Keep XTP |
| App state (suspend on close), constrained mode | ✔ state; constrained → host | ✔ both | 4 / — | Keep XTP |
| Thread affinity with console masks | ✘ host (fails for masks beyond the PC's processors) | ✔ masks fitted to the process | — / 2 | **Rewrite after xcl** (K3) |
| `GlobalMemoryStatusEx`, processor topology as the console reports them | ✘ host values | ◐ title memory status only | — / 1 | **New work**, after a check (open question Q4) |
| XMem allocation, hooks (`__XMemAllocationHooks__`), address constraints | ✔ | ✔ | 10 for all of `xmem` / ~8 | Keep XTP |
| Physical pages: allocate, map, scatter, alias | ✔ sections, committed at map time | ✔ committed on first touch (`kernelx/lazy.rs`) | (in the 10) / ~10 | Keep XTP; **lazy commit after xcl** if measured worthwhile (K1, K2) |
| Placeholder reservations for physical pages | ✔ in `XMemVirtualAlloc`; the title's plain `VirtualAlloc` goes to the host | ✔ also plain `VirtualAlloc`, console flags stripped (`placeholders.rs`) | (in the 10) / 2 | Keep XTP; revisit only if a run shows console flags reaching the host's `VirtualAlloc` |
| Virtual drives, path rewriting | ✔ `files` | ✔ VFS + overlay | 5 / e2e | Keep XTP (VFS is another report's) |

### 4.2 XGameRuntime

| Area | XTP | xcl | Tests XTP / xcl | Verdict |
|------|-----|-----|-----------------|---------|
| Entry points, `QueryApiImpl`, `XErrorReport` | ✔ | ✔ | 1 / 16 | Keep XTP |
| Slot naming, unknown-iid fallback, per-name census | ✘ offsets only | ✔ generated GDK table | — / 1 (`vtables_follow_the_gdk_table`) | **Rewrite after xcl** (R1, R2) |
| XTaskQueue: manual/pool/serialized/immediate ports, delayed, composite, terminate, monitors | ✔ | ✔ own + PC fallback | 7 / 10 | Keep XTP |
| XTaskQueue wait handles (`RegisterWaiter`) | ✘ slots 0xa0/0xa8 | ✔ | — / yes | **Rewrite after xcl** (R3) |
| XThread time sensitivity | ✘ slot 0xd8 | ✔ `XThreadIsTimeSensitive`… | — / — | **Rewrite after xcl** (R3) |
| XAsync begin/run/schedule/complete/cancel/results | ✔ | ✔ | (in 7) / 14 | Keep XTP |
| XUser: one offline signed-in user, privileges, change events, device association | ✔ | ✔ | 2 / several | Keep XTP |
| XUser gamer picture | ✘ | ✔ generated PNG | — / 1 | After xcl, if the title asks (R8) |
| XUser token and signature | ◐ fails as offline | ✔ signed with an ephemeral key | — / 4 | Keep XTP (offline by design) |
| XGameSave: provider, containers, blobs, atomic updates, quota | ✔ versioned folders, crash-safe | ✔ plain files | 7 / 10 | **Keep XTP** (stronger: survives a kill at every step) |
| XGameSave blocking variants, by-prefix enumeration, all-blob info | ✘ 5 slots | ✔ | — / yes | **Rewrite after xcl** (R4) |
| Persistent local storage | ✔ path, space, prompt | ✔ | 1 / 1 + `pls_quota` | Keep XTP |
| XPackage: identity, chunks ready, mount, enumerate | ✔ (20 slots unset) | ✔ | 1 / yes | Keep XTP; fill on census (R1) |
| XGame, features, invites, events | ✔ | ✔ / PC | 2 / — | Keep XTP |
| XSystem, analytics | ✔ | ✔ | 4 / yes | Keep XTP |
| XStore: own licence active, add-ons none, store calls fail as offline | ✔ | ✘ refused class | 1 / — | **Keep XTP** (better) |
| XNetworking: stack up, reaches nothing | ✔ | ✔ real hints, CryptoAPI certificate checks | 1 / 7 | Keep XTP (offline by design) |
| XGameUi: profile card, text entry (host dialog) | ✔ | → PC runtime | 1 / — | Keep XTP |
| XGameUi message and error dialogs | ✘ | → PC runtime | — / — | **New work** (R5) |
| Speech synthesis | ✔ Windows voices, text and SSML to 48 kHz PCM | → PC runtime | 4 / — | Done (R6) |
| Closed captions, high contrast | ✔ Windows settings and title override | ✔ `accessibility.rs` | 5 / 3 | Done (R7) |
| XError callback and options | ✘ class absent | ✔ | — / — | After xcl, if the title asks (R9) |
| Relaunch (`XLaunchNewGame`), restart on crash | ◐ relaunch untested live | ✔ both | 4 (lifecycle) / — | Verify (R10) |
| HDR, URIs, deferrals, screenshots | ✔ | ◐ | 1 / — | Keep XTP |

### 4.3 Audio

| Sub-area | XTP | xcl | Tests XTP / xcl | Verdict |
|----------|-----|-----|-----------------|---------|
| APU heap, address maps, SHAPE context arrays | ✔ | ✔ | 5 / 7 | Keep XTP |
| Coprocessor command/message queues (fmod's start-up handshake) | ✔ | ✔ frames tick | (in 5) / (in 7) | Keep XTP |
| XMA contexts decoded into their output rings | ✔ own decoder thread | ✘ (xcl decodes XMA only in XAudio2 voices) | 1 / — | **Keep XTP** |
| XMA2 bitstream decoder | ✔ `xtp-formats::xma` | ✔ `hle/xma2` | 7 (1 opt-in vs. reference) / 7 (vs. FFmpeg) | Keep XTP |
| SHAPE SRC/EQ/filter/mixer units | ✘ | ✘ | — / — | — for this title (it uses XMA contexts only) |
| xapu Opus decoder | ✔ `opus-pure` | ✔ `MSOpusDecoder.dll` or libopus | 1 / 13 | Keep XTP; **verify output** (A1) |
| DSP convolution (reverb) | ✘ silent, IR layout unknown | ✘ | 1 / 1 | **New work** (A2, A3) |
| XAudio2 | host pass-through | wrapped: Opus/XMA voices, console flags dropped | — / 9 | Keep XTP (Bink's voices are PCM; see Q6) |
| Media Foundation | host + local-only resolver | host + device manager helper | 2 / 3 | Keep XTP |
| Capture for diagnosis | ✔ `XTP_AUDIO_CAPTURE`, `XTP_XMA_DUMP`, `XTP_DSP_DUMP` | ◐ `XCL_TRACE_APU` | — | Keep XTP |

### 4.4 Input

| Sub-area | XTP | xcl | Tests XTP / xcl | Verdict |
|----------|-----|-----|-----------------|---------|
| Gamepads | ✔ 4 XInput slots, hot-plug, callbacks | ◐ one always-connected pad | 7 / 9 | **Keep XTP** |
| Keyboard device | ✔ own device | ✘ | (in 7) / — | Keep XTP |
| Keyboard as stand-in pad | ✔ by virtual key, only while no pad | ✔ by scan code (layout-independent), on top of a pad | (in 7) / 6 | Rewrite the key mapping after xcl (I1) |
| Mouse as right stick, wheel as bumpers | ✘ | ✔ Raw Input, velocity model, cursor held | — / 4 | **Rewrite after xcl** (I2) |
| Rumble (low/high motors) | ✔ | ✔ | 1 / — | Keep XTP |
| Trigger rumble | ✘ | ✘ (motors 3–4 dropped) | — / — | **New work** (I3) |

### 4.5 Network

| Sub-area | XTP | xcl | Verdict |
|----------|-----|-----|---------|
| Title's Winsock/WinHTTP: off-machine connects, sends, lookups refused, binds narrowed | ✔ always | ✔ only with `XCL_NETWORK=0` | Keep XTP |
| Proxies, WPAD | ✔ refused | ◐ | Keep XTP |
| Other ways out (system DLLs reached directly, COM objects) | ✘ not covered | ◐ inline hooks in system winhttp / wininet / ws2_32 (a retail crash reporter bypassed the import gate) | **Rewrite after xcl** (N1) |

### 4.6 Saves

Covered in 4.2: XTP's XGameSave is kept as is (atomic, versioned,
case-exact names, crash test); only the missing slots come after xcl (R4).

## 5. Porting backlog

Ordered by value for running the title and by risk; each item is one worker
session (a few hundred lines). Paths are XTP's. "xcl ref" is for behaviour
only. Items marked *conditional* are done only if the census (R1) or a run
shows the title calls them.

### N1. Close the network paths around the import gate — M
- **Goal:** no title code reaches the network by a route other than its
  imports: functions looked up in host DLLs the title (or a host DLL on its
  behalf) loads, and COM objects from `CoCreateInstance` that can fetch URLs
  (XMLHTTP, WinHTTP request objects, BITS). Same refusals as `net.rs`.
- **Files:** `crates/xtp-hle/src/modules/net.rs`, `modules/kernel.rs`
  (`GetProcAddress` path), new `modules/com.rs` (`CoCreateInstance` gate),
  `modules/mod.rs`.
- **xcl ref:** `hle/netoff.rs` `install_system_hooks` (what is hooked and why:
  issue #305, a crash reporter uploading a minidump) and `enabled`.
- **Acceptance:** unit test: every CLSID class in the deny list fails with
  `REGDB_E_CLASSNOTREG`, others reach the host; integration test on Windows:
  a socket connect and a WinHTTP request to a non-loopback address made
  through a host DLL loaded by name both fail, loopback still works.
- **Depends on:** —

### R1. Name every runtime slot; count what the title calls — M
- **Goal:** a table (class, iid, slot) → public GDK function name, generated by
  an XTP `xtask` command from the public PC GDK's `xgameruntime.lib` the
  developer points it at (never committed: only the generated Rust table, if
  Q1 allows). `xgameruntime::missing` logs the function name; at exit, one
  summary line per unimplemented function and per unknown (class, iid) with
  call counts.
- **Files:** `xtask/src/gdk_table.rs` (new), `crates/xtp-hle/src/modules/
  gdk_table.rs` (generated), `modules/xgameruntime.rs`, `modules/mod.rs`.
- **xcl ref:** `hle/xgameruntime/gdk_table.rs` header and
  `scripts/gen_gdk_table.py` (what is extracted), `api.rs` `unknown_slot`,
  `note`, `family`.
- **Acceptance:** test that each hand-written vtable slot in `xuser`,
  `xgamesave`, `xasync`, `xpackage`, `xstore` matches the table's name for it
  (XTP's version of `vtables_follow_the_gdk_table`); a test calling a
  missing slot sees its function name in the log.
- **Depends on:** Q1 answered.

### R11. End-to-end test of the thunk protocol — M
- **Goal:** extend `tests/fixtures/hle-probe` so the probe behaves like the
  GDK thunk library: `LoadLibraryExA("xgameruntime.dll")`,
  `InitializeApiImplEx2`, `QueryApiImpl` for XTaskQueue, XUser and XGameSave,
  then add a user, write and read a save blob through vtable slots on a
  manual queue. Safety net for every later runtime item.
- **Files:** `tests/fixtures/hle-probe/src/lib.rs`, the loader integration
  test that runs it (`crates/xtp-loader/tests/`).
- **xcl ref:** `tests/testtitle/src/main.rs`, `crates/xcl/tests/e2e.rs`
  `test_title_runs_and_game_folder_is_untouched`, `api.rs` test
  `thunk_protocol_with_xcl_objects_on_pc_queues`.
- **Acceptance:** the probe's checks pass under `cargo test -p xtp-loader`;
  the save lands under the data directory and nowhere else.
- **Depends on:** —

### K3. Console affinity masks on any PC — S
- **Goal:** `SetThreadAffinityMask`, `SetThreadGroupAffinity` and
  `SetThreadpoolAffinityMask` (if imported) accept the console's masks: bits
  beyond the host's processors are folded onto existing ones instead of
  failing; an empty result keeps the current affinity. Add a timer test.
- **Files:** `crates/xtp-hle/src/modules/kernel.rs` (or a new
  `modules/thread.rs`), `modules/mod.rs`.
- **xcl ref:** `hle/kernelx.rs` `SetThreadAffinityMask`,
  `SetThreadGroupAffinity`; tests `affinity_masks_outside_the_process_are_fitted`,
  `set_thread_affinity_mask_takes_console_masks`,
  `fine_timer_makes_short_sleeps_short`.
- **Acceptance:** unit test: with a fake 4-processor process mask, masks
  `0x7F`, `0x3F00` and `0x1` give non-empty subsets of it; the call succeeds;
  a 1 ms sleep after `use_console_timers` lasts under 4 ms.
- **Depends on:** —

### I1. Keyboard stand-in pad by key position — S
- **Goal:** map the stand-in pad by scan code, so WASD/QE/ZC keep their
  places on AZERTY and other layouts; letters shown in README follow the
  US layout as "positions".
- **Files:** `crates/xtp-hle/src/modules/gameinput.rs` (`FALLBACK_BUTTONS`,
  `fallback`, host key source), `README.md` input line.
- **xcl ref:** `hle/kbm.rs` `scancode_to_vk`, `keyboard_layout_for_window`;
  test `scancodes_map_to_distinct_vks_and_fall_back_safely`.
- **Acceptance:** unit test with a fake source giving AZERTY scan codes: the
  key at the W position moves the left stick up; the keyboard device still
  reports the layout's characters.
- **Depends on:** —

### R4. XGameSave: the remaining slots — S
- **Goal:** blocking initialize, quota, delete and submit (same code as the
  async forms, on the calling thread); enumerate containers by name prefix;
  all blob info of a container.
- **Files:** `crates/xtp-hle/src/modules/xgamesave.rs`.
- **xcl ref:** `hle/xgameruntime/gamesave.rs` `XGameSaveInitializeProvider`,
  `XGameSaveGetRemainingQuota`, `XGameSaveEnumerateContainerInfoByName`,
  `XGameSaveSubmitUpdate` (argument order, prefix semantics).
- **Acceptance:** unit tests through `xgameruntime::method` for each slot;
  the existing crash test still passes for the blocking submit.
- **Depends on:** R1 (names confirm the slots), optional.

### R3. Task queue wait handles and XThread — S
- **Goal:** `XTaskQueueRegisterWaiter`/`UnregisterWaiter` (a callback on a
  port when a kernel handle is signalled, via a host thread-pool wait) and
  XThread time sensitivity (per-thread flag; assert/verify log once).
- **Files:** `crates/xtp-hle/src/modules/xasync.rs`.
- **xcl ref:** `hle/xgameruntime/taskqueue.rs` `XTaskQueueRegisterWaiter`;
  `hle/xgameruntime/mod.rs` `XThreadIsTimeSensitive` and neighbours.
- **Acceptance:** test: an event set after registration runs the callback
  once on its port (manual dispatch); unregistering before the set runs
  nothing; terminate cancels a pending wait.
- **Depends on:** R1, optional.

### A1. Check the Opus decoder against Windows' — S
- **Goal:** settle "Opus results unverified": an opt-in test that decodes
  packets captured from the title's xapu commands with `opus-pure` and with
  Windows' Opus MFT (test-only, dynamically loaded) and compares them; a
  capture switch (`XTP_APU_DUMP=<dir>`) beside `XTP_XMA_DUMP`.
- **Files:** `crates/xtp-hle/src/modules/apu.rs` (dump), a test in the same
  file or `crates/xtp-hle/tests/opus_reference.rs`.
- **xcl ref:** `hle/opus.rs` `create_opus_mft` (how the MFT is driven),
  `parse_packet`; `hle/acphal.rs` module doc (command layout as the game
  sends it); test `decodes_a_game_sound`.
- **Acceptance:** with `XTP_SAMPLE_OPUS=<dir>` set, mono and stereo packets
  decode with SNR ≥ 60 dB between the two decoders; without it the test
  prints that it skipped and passes.
- **Depends on:** —

### I2. Mouse as the right stick — M
- **Goal:** while the title's window has focus, mouse movement (Raw Input
  counts, not the cursor) drives the right stick through a velocity model,
  buttons the triggers, wheel notches the bumpers; cursor held in the window,
  freed on focus loss; `[input] keyboard_mouse` and `mouse_sensitivity` in
  `xtp.toml`. Works with a pad connected too.
- **Files:** `crates/xtp-hle/src/modules/gameinput.rs`, `modules/lifecycle.rs`
  (Raw Input registration on the title window), `crates/xtp-core/src/config.rs`.
- **xcl ref:** `hle/kbm.rs` `stick_from_mouse`, the velocity `update` /
  `step`, `notches`, `hold_cursor`, `on_message`, `apply`; its tests.
- **Acceptance:** unit tests: constant counts converge to a constant
  deflection, no counts settle to zero, three wheel notches give three bumper
  presses, focus loss resets everything.
- **Depends on:** I1.

### R5. Message and error dialogs — M
- **Goal:** `XGameUiShowMessageDialogAsync`/`Result` and
  `XGameUiShowErrorDialogAsync`/`Result` as host task dialogs on a thread of
  their own (like text entry), returning the chosen button; cancel → the
  documented result.
- **Files:** `crates/xtp-hle/src/modules/xgameui.rs`.
- **xcl ref:** `hle/xgameruntime/api.rs` `classes` (the XGameUi forward list:
  which functions exist and that they take no user handle). xcl forwards to
  the PC runtime; XTP implements them.
- **Acceptance:** unit test with the dialog replaced by a fake answering
  button 2: the async result reports button 2; with no window the call
  completes as cancelled.
- **Depends on:** R1, optional.

### R10. Relaunch and restart on crash, live — S
- **Goal:** confirm `XLaunchNewGame` (wait for the old process, start again
  with the arguments) on the real title, and add `XLaunchRestartOnCrash`
  (record the arguments; the crash reporter relaunches once).
- **Files:** `crates/xtp-hle/src/modules/xgame.rs`, `modules/lifecycle.rs`,
  `apps/xtp/src/run.rs` (relaunch path).
- **xcl ref:** `hle/xgameruntime/api.rs` `XLaunchNewGame`,
  `XLaunchRestartOnCrash`.
- **Acceptance:** a child-process test (pattern of the save crash test):
  a probe calling restart-on-crash then faulting is started a second time
  with the recorded arguments, and only once.
- **Depends on:** —

### R7. Closed captions and high contrast — S (done)
- **Goal:** answer the accessibility slots from the host: Windows
  closed-caption settings (enabled, colours, size) and high-contrast mode.
- **Files:** `crates/xtp-hle/src/modules/speech.rs` and `speech/captions.rs`.
- **xcl ref:** `hle/xgameruntime/accessibility.rs` `XClosedCaptionGetProperties`,
  `XClosedCaptionSetEnabled`, `XHighContrastGetMode` (structure filled, defaults).
- **Acceptance:** unit test: the structure is fully written with documented
  defaults when the host has no setting; set-enabled is read back.
- **Depends on:** —

### R6. Speech synthesis through the host — M (done)
- **Goal:** narration speaks: voices enumerated from the host's speech
  synthesizer, text and SSML streams rendered to 48 kHz WAVE in memory.
- **Files:** `crates/xtp-hle/src/modules/speech.rs` and `speech/narrator.rs`.
- **xcl ref:** none (xcl forwards speech to the PC runtime). New work.
- **Acceptance:** Windows tests check PCM output for text and SSML, and the
  empty WAVE for blank text, an unavailable voice, or a failed host engine.
- **Depends on:** —

### I3. Trigger rumble — M
- **Goal:** the console's four motors reach Xbox pads that have trigger
  motors, through `Windows.Gaming.Input` (`Gamepad.Vibration`), matched to the
  XInput slot; other pads keep the two-motor path.
- **Files:** `crates/xtp-hle/src/modules/gameinput.rs`,
  `crates/xtp-hle/Cargo.toml`.
- **xcl ref:** none (`hle/gameinput.rs` `set_rumble_state` drops the trigger
  motors). New work.
- **Acceptance:** unit test with the fake source: a four-motor state reaches
  the source with all four values; manual check on a pad (noted in the PR).
- **Depends on:** —

### K1. Measure what title memory costs in commit — S
- **Goal:** a diagnostics log (`XTP_ACTIVITY`-style, periodic) of process
  commit, physical pages allocated, mapped and committed, and how many
  mapped pages were never written; decides whether K2 is worth doing.
- **Files:** `crates/xtp-hle/src/modules/xmem.rs`.
- **xcl ref:** `HANDOFF.md` "Retail game bring-up" (what `XCL_ACTIVITY` and
  `XCL_TRACE_MEM` report).
- **Acceptance:** unit test of the counters over allocate/map/unmap/free;
  a run's numbers recorded in the PR.
- **Depends on:** —

### K2. Commit physical pages on first touch — M
- **Goal:** pages mapped but still zero stay reserved; a vectored handler
  commits a page on the CPU's first access; every kernel call that writes
  title memory (`ReadFile`, DirectStorage destinations, GPU read-backs)
  commits first.
- **Files:** `crates/xtp-hle/src/modules/xmem.rs`, `modules/files.rs`
  (`ReadFile`), `modules/dstorage.rs` (destination commit).
- **xcl ref:** `hle/kernelx/lazy.rs` module doc and `ensure`; tests
  `a_page_away_from_home_is_committed_once`,
  `mapping_lazy_pages_again_does_not_query_each_page`.
- **Acceptance:** tests: a mapped, untouched page is not committed; a write
  commits it once; `ReadFile` into an uncommitted page succeeds; aliased
  views see the same data.
- **Depends on:** K1 showing a gain.

### A2. Capture the DSP's impulse responses — S
- **Diagnostic:** Set `XTP_DSP_DUMP` to an absolute directory outside the repository.
  The first 32 streams per process record connection and stream parameters, an
  impulse response up to 4 MiB and four commands' gains and input/output blocks
  for offline study of the partitioned frequency-domain layout.
- **Files:** `crates/xtp-hle/src/modules/dsp.rs`.
- **xcl ref:** `hle/acphal.rs` XDsp functions (`XDspConnectWithMaximumStreamLimit`,
  `XDspActivate`: argument meaning as observed).
- **Acceptance:** unit test: a stream activated as the title does writes the
  expected files; nothing is written without the variable.
- **Depends on:** —

### A3. Convolution reverb — L, split once the layout is known
- **Goal:** compute the DSP's convolution (overlap-save over the
  partitioned IR) so reverb returns are wet.
- **Files:** `crates/xtp-hle/src/modules/dsp.rs`, possibly
  `crates/xtp-formats/src/` for the FFT.
- **xcl ref:** none. New work.
- **Acceptance:** an IR captured by A2 convolved with an impulse gives back
  the IR's time-domain form; the title's reverb send is non-silent in a
  capture.
- **Depends on:** A2 and Q5.

### R2. Unknown interface versions get the class's main vtable — S
- **Goal:** for a known class, an iid not in the table is answered with the
  main vtable (logged once) when the table shows later GDK versions only
  append slots; otherwise still refused.
- **Files:** `crates/xtp-hle/src/modules/xgameruntime.rs`.
- **xcl ref:** `hle/xgameruntime/api.rs` `QueryApiImpl`, `forward_alias_obj`,
  `layout` (interface ids sharing slots).
- **Acceptance:** unit test: an unknown iid of XUser gets the XUser object;
  an unknown class still fails without touching the output.
- **Depends on:** R1.

### R12. The title's libHttpClient initialises through XTP — S
- **Goal:** opt-in test: map the user's `libHttpClient.GDK.dll` with the
  loader and call `HCInitialize`; it succeeds through XTP's runtime objects.
- **Files:** `crates/xtp-loader/tests/` (new test), no product code expected.
- **xcl ref:** test `libhttpclient_initialises_through_the_xgameruntime_thunks`.
- **Acceptance:** with `XTP_SAMPLE_DLLS=<dir>` the call returns `S_OK`;
  without it the test prints that it skipped.
- **Depends on:** R11.

### R8, R9. Gamer picture, XError callbacks — S each, *conditional*
- **R8:** `XUserGetGamerPictureAsync`/`ResultSize`/`Result`: a plain PNG of the
  requested size. Files `modules/xuser.rs`. xcl ref `api.rs`
  `gamer_picture_async`, `gamer_picture_result`, test `gamer_picture_is_a_png`.
  Acceptance: the result decodes as a PNG of each documented size.
- **R9:** XError class: `XErrorSetCallback`, `XErrorSetOptions`, and
  `XErrorReport` calling the callback. Files `modules/xgameruntime.rs`. xcl ref
  `api.rs` `error_set_callback`, `error_set_options`, `XErrorReport`.
  Acceptance: a registered callback receives the reported HRESULT.
- **Depends on:** R1 (the census says whether the title calls them).

### Summary

| # | Item | Size | Depends on |
|---|------|------|-----------|
| 1 | N1 network paths around the gate | M | — |
| 2 | R1 slot names and census | M | Q1 |
| 3 | R11 thunk-protocol e2e test | M | — |
| 4 | K3 affinity masks, timer test | S | — |
| 5 | I1 positional keys | S | — |
| 6 | R4 XGameSave remaining slots | S | (R1) |
| 7 | R3 waiters, XThread | S | (R1) |
| 8 | A1 Opus check | S | — |
| 9 | I2 mouse as right stick | M | I1 |
| 10 | R5 message/error dialogs | M | (R1) |
| 11 | R10 relaunch, restart on crash | S | — |
| 12 | R7 closed captions, high contrast (done) | S | — |
| 13 | R6 speech synthesis (done) | M | — |
| 14 | I3 trigger rumble | M | — |
| 15 | K1 commit measurement | S | — |
| 16 | K2 lazy commit | M | K1 |
| 17 | A2 DSP capture | S | — |
| 18 | A3 convolution reverb | L | A2, Q5 |
| 19 | R2 unknown-iid fallback | S | R1 |
| 20 | R12 libHttpClient opt-in test | S | R11 |
| 21 | R8, R9 (conditional) | S | R1 |

Not ported, on purpose: forwarding to the PC runtime (`api.rs` `pc`,
`pc_object`, `xasync::use_pc`), XAudio2 compressed voices (`xaudio2.rs`; the
title feeds XMA and Opus to the audio processor, not to XAudio2), real
certificate checks and token signing (`networking.rs`, `xsign.rs`; XTP stays
offline), the always-connected single pad of `gameinput.rs` (XTP's model is
richer), xcl's XGameSave storage (XTP's is crash-safe).

## 6. Risks

- **Licence contamination.** Reading xcl while writing the same function is
  the main risk. Workers read the cited behaviour, close the file, write from
  the public GDK contract and XTP's own tests; reviewers check that no xcl
  identifiers beyond public API names, comments or test data appear.
  `xcl/hle/xma2` is itself an FFmpeg port: XTP already has its own.
- **Generated GDK table.** Its contents come from Microsoft's PC GDK; whether
  a generated table of names, GUIDs and slots may live in a GPL repository
  is not settled (Q1). Fallback: keep it out of the repository and generate
  it at build time from the developer's GDK, or derive slots from the
  title's own thunks as today and keep names only in logs.
- **Hand-placed slots.** XTP's slots were read from the title's call sites;
  a wrong slot fails silently in the title. R1's cross-check is the guard;
  xcl found two of its own guessed XPackage slots wrong that way.
- **Vectored handler (K2).** A first-touch commit handler runs before the
  title's own handlers and the crash reporter; ordering mistakes turn into
  missed crashes or recursion. Kernel calls on uncommitted pages fail
  instead of faulting, so every writer must commit first (xcl hit this).
- **Network gate gaps (N1).** Hooking system DLLs in place (xcl's approach)
  is invasive and version-dependent; a deny list of COM classes is never
  complete. Keep both narrow and testable, and keep loopback working
  (titles make socket pairs).
- **Raw Input and focus (I2).** Holding the cursor in the window conflicts
  with the text-entry dialog and with Alt+Tab; release on every focus change.
- **Trigger rumble (I3).** `Windows.Gaming.Input` delivers readings only to a
  foreground app of its own; matching its gamepads to XInput slots is not
  guaranteed.
- **Reverb (A3).** The IR layout may not be recoverable from captures; the
  item may end as "documented, still dry".
- **Console-only CPU instructions** (xcl `insn.rs`, emulated in its fault
  handler) can stop the title on older Intel CPUs before any HLE code runs:
  belongs to the loader report, noted here because it shows up as an HLE-time
  crash.

## 7. Open questions

- **Q1.** May XTP commit a table generated from the public PC GDK's
  `xgameruntime.lib` (names, GUIDs, slot numbers)? The person decides; R1's
  design depends on it.
- **Q2.** Which runtime classes and slots does the title actually call past
  the first 3D scene? Only a run with R1's census answers it; R8, R9, R2 and
  the unset XPackage/XStore slots wait for that list.
- **Q3.** Does any path in the title (crash reporter, telemetry, Bink, the
  BVH or FSR libraries) reach the network through COM or a host DLL today?
  N1's audit answers it; xcl saw a retail crash reporter do so.
- **Q4.** Should `GlobalMemoryStatusEx` and `GetLogicalProcessorInformation`
  answer as a Series X (13.5 GB title memory, 8 cores) or as the PC? A title
  sizing pools or worker counts from them behaves differently; needs a run
  comparing both.
- **Q5.** What layout does `chad.dll` expect for impulse responses (partition
  size, FFT size, packing)? A2 collects the data; nobody has documented it.
- **Q6.** Which formats do Bink's XAudio2 voices use? If not PCM, XTP needs a
  piece of xcl's XAudio2 wrapping (`xaudio2.rs` `CreateSourceVoice` slot 5);
  a run logging `CreateSourceVoice` formats answers it.
- **Q7.** Relaunch: does the title call `XLaunchNewGame` in practice (language
  change, account switch)? R10 tests the mechanism; only play shows the path.
