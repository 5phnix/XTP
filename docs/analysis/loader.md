# Loader layer: VIctory (XTP's base) against xcl

What XTP keeps of VIctory's loader layer, what it rewrites after xcl's
approach, and in what order. Written 2026-11-24 from VIctory as imported at
`ffe310e` (XTP's first commit) and the xcl snapshot in `E:/xtp-refs/xcl`.

xcl is GPL-2.0-only and XTP is GPL-3.0-or-later: xcl is a reference for
**behaviour** only. Nothing below is xcl code; xcl functions are named so a
worker can find the behaviour, and every backlog item is to be written fresh
against its acceptance test.

Crate names are the XTP ones being introduced in parallel:
`victory-pe` → `xtp-pe`, `victory-loader` → `xtp-loader`,
`victory-core` → `xtp-core`, `apps/victory` → `apps/xtp`, and (assumed by the
same rule) `victory-hle` → `xtp-hle`.

## 1. Scope

The loader layer is everything between "a title's files on disk" and "the
title's code runs with a platform under it": PE parsing, mapping and
relocation, import binding and the resolution policy, stubs for what is not
implemented, title DLLs and runtime `LoadLibrary`, static TLS, thread and
process notifications, the main thread, exceptions and crash reports,
console-only CPU instructions, the memory and thread APIs the image needs,
and the virtual file system that turns console paths into host ones.

Out of scope (other reports): the console APIs themselves (XGameRuntime,
XMem pools beyond what the loader needs, audio, input, network), the GPU,
shaders, data formats and tooling.

### 1.1 Files in XTP (VIctory at `ffe310e`)

| Path (XTP name) | Lines | Role |
|---|---:|---|
| `crates/xtp-pe/src/image.rs` | 547 | PE32+ parse via `object`: sections, imports, delay imports, exports, TLS, CodeView |
| `crates/xtp-pe/src/layout.rs` | 110 | raw header offsets for patching |
| `crates/xtp-pe/src/implib.rs` | 66 | COFF import libraries |
| `crates/xtp-loader/src/prepare.rs` | 283 | patch a copy so the Windows loader maps it but runs nothing |
| `crates/xtp-loader/src/windows.rs` | 432 | `load`, title DLLs, `bind_imports`, attach/detach, `spawn_main` |
| `crates/xtp-loader/src/resolve.rs` | 33 | `ImportResolver`, `Provenance`, `Resolution` |
| `crates/xtp-loader/src/thunks.rs` | 187 | stubs for unresolved imports (trap / return 0) |
| `crates/xtp-loader/src/tls.rs` | 219 | exe static TLS: rewrite `gs:[0x58]` loads to index the image's slot |
| `crates/xtp-loader/src/threads.rs` | 439 | thread notices via our own TLS callback, ordered process exit |
| `crates/xtp-loader/src/crash.rs` | 991 | VEH + unhandled filter, `module+rva`, unwinding, AMD-only instruction emulation |
| `crates/xtp-loader/src/stacks.rs` | 224 | scheduled stack dumps (feature `diagnostics`) |
| `crates/xtp-loader/tests/load_probe.rs`, `title_dlls.rs` | 284 | end-to-end on `tests/fixtures/hle-probe` (a DLL) |
| `crates/xtp-core/src/policy.rs`, `config.rs` | 376 | game-derived data stays outside the repo; paths |
| `apps/xtp/src/run.rs` | ~290 | run order; late title DLLs (`LateDlls`) and `LoaderLock` live here |
| `crates/xtp-hle/src/lib.rs`, `host.rs`, `registry.rs` | ~370 | `HleResolver`, `HostPolicy`, the HLE export table |
| `crates/xtp-hle/src/modules/kernel.rs` | 687 | `LoadLibrary*`, `GetProcAddress`, `GetModuleHandle*`, exit paths, `SetUnhandledExceptionFilter` |
| `crates/xtp-hle/src/modules/files.rs` | 941 | virtual drives (`G:`, `T:`, storage) over host folders |
| `crates/xtp-hle/src/modules/xmem.rs` | 1242 | `xmem.dll`: allocations, physical pages, placeholders |

### 1.2 Files in xcl (reference)

| Path (`crates/xcl/src/`) | Lines | Role |
|---|---:|---|
| `pe.rs` | 803 | hand-written PE32+ parser, file and mapped layouts, relocations, owned exports |
| `loader.rs` | 525 | manual mapper: reserve, copy, relocate, bind, TLS, protections, `.pdata`, GS hook; DLL init; main thread; thread events |
| `resolver.rs` | 612 | routing → HLE (+cross-module, +system fallback) → game DLL → system → stub; runtime `LoadLibrary`/`GetProcAddress`; import tracer |
| `modules.rs` | 205 | table of mapped images, lock-free address lookup, HLE pseudo handles |
| `tls.rs` | 614 | generated "carrier DLL" per TLS image; exe takes slot 0 by swapping TEB slots |
| `eh.rs` | 1154 | `RtlAddFunctionTable`, `RtlPcToFileHeader` hook, crash filter, `NtTerminateProcess` hook, GS failure, invalid-parameter handler, first-chance C++ log, console-only instructions, watchpoints |
| `insn.rs` | 243 | decoder for console-only instructions |
| `hooks.rs` | 30 | inline hooks through `retour` |
| `stubs.rs` + `data/stubdb.toml` | 342 + 61 | stubs that return a configured value; ordinal → name map |
| `vfs.rs` | 1129 | mounts, console prefixes, copy-on-write overlay, whiteouts |
| `vfs_hooks.rs` | 1741 | process-wide KERNELBASE file hooks feeding `vfs` |
| `hle/kernelx.rs`, `hle/kernelx/lazy.rs` | ~3100 | memory and thread overrides; physical pages committed on first touch |
| `host.rs`, `launcher.rs`, `state.rs` | 890 | child "host" process started with the game's command line |
| `preflight.rs`, `inspect.rs`, `manifest.rs` | 566 | CPU/GPU checks, static import report, `MicrosoftGame.Config` |

Also read: `README.md`, `AGENTS.md`, `HANDOFF.md` ("What works"),
`docs/specs/2026-11-20-xcl-design.md` §5.1–5.4, 5.6 and the e2e title
`tests/testtitle`.

## 2. Two designs

**VIctory lets Windows load the title.** `prepare` writes a copy of the image
marked as a DLL, with no entry point, an empty import directory (kept so
Windows still assigns static TLS), no bound imports and no TLS callback
pointer (its relocation neutralised). `LoadLibraryExW` then maps, relocates,
protects, registers `.pdata` and allocates static TLS for every thread. The
image is a real module: debuggers, profilers, `RtlPcToFileHeader`, C++
`throw`, `GetModuleHandleEx(FROM_ADDRESS)` and stack walks all work without a
hook. VIctory then fills the IAT itself and runs TLS callbacks and `DllMain`
when it chooses.

**xcl maps the title by hand.** It reserves at the preferred base, copies
sections, applies relocations, binds, sets protections, registers `.pdata`
with `RtlAddFunctionTable` and then patches the system so the image is
visible: an inline hook on `RtlPcToFileHeader` for C++ exceptions, a
generated "carrier DLL" per TLS image so Windows owns a TLS slot, and
KERNELBASE hooks so file calls from system DLLs reach its VFS.

The Windows-loader design needs fewer hooks, shows title frames to every
tool, and VIctory already runs Grand Theft Auto VI's executable and its
DLLs on it through the menus into the world. **XTP keeps it as the
foundation.** xcl is ahead in what surrounds the mapping: stub policy,
fast-fail visibility, the command line, the VFS overlay, and the memory and
thread overrides. Those are the rewrites.

## 3. What the game dump shows

Read-only inspection of `E:\uwu` (headers and import tables only). Per
`docs/asset-policy.md` only header facts, counts and public Windows / GDK API
names are recorded here, never an extracted table:

- `game_scarlett_master.exe`: preferred base `0x140000000`, relocations
  present (`0x12a350` bytes), `DYNAMIC_BASE | HIGH_ENTROPY_VA | NX_COMPAT`,
  a TLS directory, no delay imports, 68 import descriptors.
- 16 DLLs ship beside it, none with export forwarders. Three have static
  TLS: `XboxBvhBuilder.dll`, `amd_fidelityfx_upscaler_xbox.dll`,
  `libHttpClient.GDK.dll`. The console builds of `msvcp140*`,
  `vcruntime140*`, `concrt140`, `vcomp140`, `vccorlib140` ship too.
- Loader-layer imports of the exe, all through API sets (no `kernelx.dll`):
  `processthreads` (`CreateThread`, `SuspendThread`, `ResumeThread`,
  `SetThreadPriority`, `TerminateProcess`, `GetCurrentThreadStackLimits`,
  `IsProcessorFeaturePresent`), `processtopology`
  (`Get/SetThreadGroupAffinity`), `memory` (`VirtualAlloc`, `VirtualFree`,
  `VirtualQueryEx`), `libraryloader` (9 functions), `rtlsupport`
  (`RtlCaptureContext`, `RtlLookupFunctionEntry`, `RtlVirtualUnwind`),
  `errorhandling` (`SetUnhandledExceptionFilter`,
  `UnhandledExceptionFilter`), `sysinfo` (`GetLogicalProcessorInformation`,
  `GlobalMemoryStatusEx`), `windowserrorreporting` (`WerRegisterFile`),
  `xmem.dll` (12), `file-l1-1-0` (22).
- `vccorlib140.dll` imports `oleaut32.dll` `#4` and `#6` by ordinal;
  `oleaut32` is neither in `HostPolicy` nor served by HLE, so both get stubs
  if that DLL is mapped.

Not measured here: the live list of missing imports. It needs a build of
`xtp census` (item L-01 makes it a repeatable check).

## 4. Capability matrix and verdicts

Tests count `#[test]` functions in the files named; "e2e" means a fixture
title run end to end. Verdicts: **keep** (VIctory as is), **extend** (keep
VIctory, add behaviour xcl has), **rewrite** (replace VIctory's approach,
after xcl's), **new** (neither has it in a usable form), **no** (xcl has it,
XTP should not take it).

### 4.1 PE and mapping

| Sub-area | VIctory (XTP today) | xcl | Tests V / x | Verdict |
|---|---|---|---|---|
| PE parsing | `object`-based, one pass, every IAT slot checked inside the image, unused export slots skipped, CodeView | hand-written, file and mapped layouts, relocation list, owned exports with forwarders | 7 / 10 | **keep** (`object` is maintained and fuzzed upstream) |
| Mapping, relocation, protections | Windows loader on a patched copy (`prepare` + `load`), cached per build hash | manual reserve/copy/relocate/protect | 4 + e2e / 2 | **keep** |
| Exception tables | registered by Windows | `RtlAddFunctionTable` + `RtlPcToFileHeader` hook | 1 (unwind) / e2e | **keep**; **no** to the hook |
| Image visible to the system | it is a real module | lock-free range table + hooks | – / 2 | **keep** |

### 4.2 Imports and modules

| Sub-area | VIctory (XTP today) | xcl | Tests V / x | Verdict |
|---|---|---|---|---|
| Binding (static + delay, eager) | every slot bound before code runs, slot-wise protection | same; also writes the delay-load module-handle slot | e2e / via e2e | **keep** |
| Resolution order | HLE → host (`HostPolicy` allow list) → title DLL → stub | routing table (aliases, `@system`, `@stub`) → HLE (+ same name in another HLE module, + system fallback) → game DLL → system → any HLE module of that name → stub | 2 (registry) / 5 | **keep**; **no** to "any HLE module of that name" (same name, other semantics) |
| Ordinal imports | `#n` names in the HLE table | per-module ordinals + ordinal → name map in data | – / – | **extend** (L-04) |
| Unresolved imports | 32-byte stub: trap (log, break, abort) or return 0, one global policy | stub returns a per-DLL / per-function value from a data file (`E_NOTIMPL` for HRESULT modules), records caller and count | 2 / 3 | **rewrite** after xcl (L-04) |
| Export lookup, forwarders | `GetProcAddress` on the real module (forwarders go to host DLLs) | own table, forwarders through the resolver | e2e / – | **keep** (the dump has no forwarders; risk R-6) |
| Title DLLs at start | recursive over imports, dependencies first, `wanted` filter | recursive during binding | e2e / 2 | **keep** |
| Runtime `LoadLibrary`, `GetProcAddress`, `GetModuleHandle` | HLE overrides; title DLLs mapped late by `LateDlls` in `apps/xtp/src/run.rs`; pseudo handles for HLE modules | same shape in `resolver::load_library` / `get_proc_address` | 3 / 5 | **extend**: move `LateDlls` and `LoaderLock` into `xtp-loader` with tests (L-03) |
| Failed `DllMain`, concurrent loads | re-entrant loader lock; failed attach detaches and unmaps the batch (in the app, untested) | `LoadGuard`; failed DLL unregistered, a second load retries | – / 2 | **extend** (L-03) |

### 4.3 TLS, threads, process

| Sub-area | VIctory (XTP today) | xcl | Tests V / x | Verdict |
|---|---|---|---|---|
| Static TLS of DLLs | Windows assigns a slot (empty import directory kept) | carrier DLL per image, blocks filled by hand | 1 / 4 | **keep** |
| Exe TLS slot 0 | `gs:[0x58]` loads rewritten to call per-register thunks (byte match, 53,246 hits checked once against a disassembly) | swap slot 0 and the carrier slot in every TEB, our own TLS moved | 1 / 1 | **keep**; tighten the site check (L-17) |
| Thread notices | our TLS callback forwards attach/detach to attached images in Windows order; Rust std state saved and restored | same idea, from `xcl.exe`'s TLS callback | 2 + e2e / via e2e | **keep** |
| Process exit | `ExitProcess` order: other threads ended, DLLs detached in reverse, watchdog | report written on process detach; `NtTerminateProcess` hook logs who ended the process | 2 / – | **keep**; **extend** exit logging (L-05) |
| Main thread | `std::thread` with `SizeOfStackReserve`, entry gets null | `CreateThread` with reserve, entry gets the PEB | e2e / e2e | **keep** |
| `PEB.ImageBaseAddress` | untouched; `GetModuleHandle(NULL)` fixed for title imports only | set to the title (not under Wine) | 1 / e2e | **extend**, after an audit (L-10) |
| Title command line | the title sees `xtp.exe run … --execute` | child process started with the game's command line | – / e2e | **new** in-process (L-09) |
| Thread affinity, names | host pass-through | console masks mapped to host processors; `SetThreadName`/`GetThreadName`; debugger thread-name exception → `SetThreadDescription` | – / part of 30 | **extend** (L-11, L-15) |

### 4.4 Exceptions and crashes

| Sub-area | VIctory (XTP today) | xcl | Tests V / x | Verdict |
|---|---|---|---|---|
| Crash report | VEH (first chance, fatal kinds), unhandled filter ahead of the title's, `module+rva`, unwound stack, what the faulting address is (free/reserved/guard/protection), title's last debug lines, `crashes.log` | similar, plus heuristic stack scan, thread dump | 5 / 6 | **keep** |
| C++ exceptions | native; unhandled throws decoded (type, HRESULT, `what()`) | needs the `RtlPcToFileHeader` hook; first 16 first-chance throws logged | e2e / e2e | **keep**; **extend** first-chance log (L-15) |
| Fast-fail paths (no filter sees them) | `abort`, `terminate`, `_invalid_parameter_noinfo_noreturn` imports overridden | GS cookie failure redirected to a report; UCRT invalid-parameter handler for the process; `NtTerminateProcess` hook | – / 1 | **rewrite** after xcl (L-05) |
| Console-only instructions | `clzero`, `monitorx`, `mwaitx`, `clwb` (+ write-back loops), `extrq`/`insertq` | also `rdpru`, `rdpid`, `clzero` with an address-size prefix, whole `clzero` loops | 8 / 2 | **extend** (L-07) |
| CPU check before start | none (README states AVX2) | missing features listed, opt-out variable | – / 3 | **new** (L-08) |

### 4.5 Memory

| Sub-area | VIctory (XTP today) | xcl | Tests V / x | Verdict |
|---|---|---|---|---|
| `xmem.dll` | implemented: address-bit limits, 2 MiB/64 KiB pages, placeholders, physical pages as a pagefile section committed when mapped or written | implemented; zero physical pages reserved only, committed on first touch by a fault handler | 10 / 2 (`lazy`) | **keep**; measure, then maybe **rewrite** commit (L-18) |
| `VirtualAlloc` / `VirtualProtect` / `VirtualFree` | host pass-through (console flag bits reach the host) | overridden: console flags stripped | – / part of 30 | **extend** (L-12) |
| Title memory budget | none | `TitleMemoryStatus` from the manifest | – / part of 30 | out of scope (HLE report) |

### 4.6 Virtual file system

| Sub-area | VIctory (XTP today) | xcl | Tests V / x | Verdict |
|---|---|---|---|---|
| Console paths | drive letters only: `G:` the package, `T:` temp, storage drives; free host letters picked | `G:`, `GAME:`, `D:`…, mounts from configuration; unmapped console paths logged | 5 / 12 | **extend** (L-14) |
| Coverage | every path-taking **import** of title images (kernel32 file API and CRT/C++ library, `_Fiopen`) | KERNELBASE inline hooks, so host DLLs' calls are caught too | 5 / 10 | **keep** import binding; audit (L-16) |
| Game folder protection | none: `G:` is the game folder, writable | read-only; a write copies the file to an overlay first; reads overlay first; deletes leave a whiteout | – / e2e + 12 | **rewrite** after xcl (L-02, L-13) |
| Directory enumeration | host's, unchanged | overlay and original merged, whiteouts hidden | – / 3 | **rewrite** after xcl (L-13) |
| Long paths, ANSI | `\\?\` above 248 characters, narrow → wide | – | 1 / – | **keep** |

### 4.7 Diagnostics and tests

| Sub-area | VIctory (XTP today) | xcl | Tests V / x | Verdict |
|---|---|---|---|---|
| Bind summary | printed per image; `census` classifies without running | `report.json` (imports by source, stubs called with call sites, unmapped paths) written at exit and on crash; `inspect` | – / – | **extend** (L-01, L-04) |
| Import call tracer | none | `XCL_TRACE_IMPORTS`: arguments and result of chosen imports | – / – | **new**, late (L-19) |
| Stack dumps | `VICTORY_STACKS` (diagnostics feature) | `XCL_THREAD_DUMP_AT` | 1 / – | **keep** |
| e2e fixture | `hle-probe`: a DLL (bind, delay import, stub, `DllMain`, thread notices, late load) | `testtitle`: an exe (argv, static TLS on new threads, C++ exception in title code, `GetModuleHandle(NULL)`, game DLL, VFS overlay) | 3 / e2e | **extend**: an exe fixture (L-06) |

## 5. Porting backlog

Sizes: **S** under ~100 changed lines, **M** ~100–250, **L** ~250–400
(tests included). Each item fits one worker session. "xcl ref" points at
behaviour to read, never at code to copy. Items are in the recommended order;
an item may start once its dependencies have landed.

### L-01 Import census as a repeatable check — S
- **Goal:** know, before and after each item, which imports of the dump bind
  to HLE, host, title DLLs or stubs, without running the title.
- **Touches:** `apps/xtp/src/census.rs` (a `--json` output), a new opt-in
  test `apps/xtp/tests/census_sample.rs`.
- **xcl ref:** `inspect.rs` `run`, `resolver.rs` `classify` (dry-run per
  import, ordinals listed apart).
- **Acceptance:** with `XTP_SAMPLE_GAME=<dump>` the test classifies every
  image of the folder and prints the missing list; without the variable it
  passes doing nothing. The list for `E:\uwu` is pasted in the PR.
- **Depends on:** –

### L-02 Copy-on-write overlay: logic — L
- **Goal:** the package drive is read-only. Opening a package file for
  writing copies that one file into an overlay under the title's data folder
  and opens the copy; reads take the overlay first; deleting a package file
  records a whiteout; files that only exist in the overlay behave normally.
  Pure logic over `std::fs`, no Win32 binding yet.
- **Touches:** new `crates/xtp-hle/src/modules/files/overlay.rs` (or
  `files_overlay.rs`), its `mod` line.
- **xcl ref:** `vfs.rs` `Vfs::classify`, `open_decision_for`,
  `prepare_write`, `stage_write`/`commit_write`, `delete_file`,
  `is_whiteout`, `read_path_rel`.
- **Acceptance:** unit tests on a temp package folder: a write lands in the
  overlay and the package bytes are unchanged; a read after it sees the new
  bytes; a delete hides the file and a later create brings it back; a
  failed copy leaves neither layer changed.
- **Depends on:** –

### L-03 Late title DLLs and the loader lock in `xtp-loader` — M
- **Goal:** move `LateDlls` and `LoaderLock` from `apps/xtp/src/run.rs` into
  `xtp-loader` behind a public type, so runtime `LoadLibrary` of a title DLL
  is tested where it lives.
- **Touches:** new `crates/xtp-loader/src/late.rs`, `lib.rs` exports,
  `apps/xtp/src/run.rs` (use it), `crates/xtp-loader/tests/load_probe.rs`.
- **xcl ref:** `loader.rs` `LoadGuard`, `load_game_dll`; tests
  `a_dll_whose_dllmain_fails_is_not_loaded`,
  `a_dll_with_a_broken_import_table_is_not_left_registered`.
- **Acceptance:** two threads loading the same probe DLL map it once; a DLL
  whose `DllMain` returns FALSE is unmapped and a second load runs its
  `DllMain` again; a `DllMain` that loads another DLL does not deadlock.
- **Depends on:** –

### L-04 Stub database — M
- **Goal:** unresolved imports return a value chosen per DLL and per function
  from a data file (`E_NOTIMPL` for console HRESULT modules), and an ordinal
  → name map lets HLE serve ordinal imports. Each stub records its call count
  and first caller (`module+rva`). `Trap` stays the default; the data file
  applies under a new `--unimplemented table`.
- **Touches:** `crates/xtp-loader/src/thunks.rs` (stub keeps the caller),
  new `crates/xtp-loader/src/stubdb.rs`, a data file
  `crates/xtp-hle/data/stubs.toml` (our own values), `crates/xtp-hle/src/lib.rs`
  (ordinal names), `apps/xtp/src/main.rs` (flag).
- **xcl ref:** `stubs.rs` `make_stub_info`, `record_call`, `StubDb`
  `return_for_with_fallback`, `ordinal_name`; `data/stubdb.toml` (its
  sections, not its entries).
- **Acceptance:** unit tests: a stub for a listed HRESULT module returns
  `0x80004001`; a function entry overrides its module default; the caller
  recorded is the test's own return address; `oleaut32!#4` resolves by name
  when the map lists it.
- **Depends on:** –

### L-05 Fast-fail visibility — M
- **Goal:** failures that end the process without any filter running are
  named in `crashes.log`: a stack-cookie failure (the title's
  `__report_gsfailure` redirected to a report naming the function whose
  cookie failed, then ending with the same status), a CRT invalid parameter
  (a process-wide UCRT handler that logs the caller), and the process ending
  through `NtTerminateProcess` with a non-zero status (who called it).
- **Touches:** new `crates/xtp-loader/src/fastfail.rs`, `crash.rs`
  (`report_crashes` installs it), `windows.rs` (scan each executable section
  after `load`, before the TLS patch is flushed).
- **xcl ref:** `eh.rs` `find_gs_check`, `hook_gs_failure`, `gs_failure`,
  `install_invalid_parameter_handler`, `invalid_parameter`,
  `hk_nt_terminate_process`.
- **Acceptance:** the finder locates the failure jump in a synthetic copy of
  MSVC's cookie check (with and without `bnd` prefixes) and nothing in random
  code; a child-process test (pattern of `thunks::tests::trap_child`) passes
  a bad argument to a CRT function and the log names the caller.
- **Depends on:** –

### L-06 An exe-shaped test title — M
- **Goal:** a fixture built as an **exe** (our own code) that the loader runs
  end to end: static TLS read through slot 0 on the main and on new threads,
  a panic/SEH unwind through its frames, `GetModuleHandle(NULL)`, a thread
  started by the title. It guards L-09, L-10, L-17.
- **Touches:** new `tests/fixtures/title-probe/` (Cargo package, excluded
  from the workspace like `hle-probe`), new
  `crates/xtp-loader/tests/exe_probe.rs`.
- **xcl ref:** `tests/testtitle/src/main.rs` checks "static TLS: …", "C++-style
  exceptions …", "modules: GetModuleHandle(NULL) is the game image".
- **Acceptance:** the test prepares, loads, binds and runs the probe exe;
  each check reports through an exported counter; all pass on Windows 11.
- **Depends on:** –

### L-07 Remaining console-only instructions — S
- **Goal:** emulate `rdpru` (MPERF/APERF counters from the host's TSC),
  `rdpid` (processor number), `clzero` with an address-size prefix, and a
  whole `clzero` loop in one exception, as `clwb` loops already are.
- **Touches:** `crates/xtp-loader/src/crash.rs` (or a new `insn.rs` split out
  of it).
- **xcl ref:** `insn.rs` `decode`, `ConsoleOnly`; `eh.rs`
  `emulate_console_only`.
- **Acceptance:** unit tests in the style of
  `emulated_clzero_zeroes_its_line`: each instruction placed in an executable
  page runs under the handler and leaves the expected registers/memory; an
  unknown instruction still reaches the title's handlers.
- **Depends on:** –

### L-08 CPU check before the title starts — S
- **Goal:** `xtp run` lists the console CPU features this host lacks (AVX2,
  FMA, F16C, BMI1/2, …) and stops with a clear message, unless
  `XTP_SKIP_CPU_CHECK=1`; features XTP emulates (L-07) are not listed.
- **Touches:** `apps/xtp/src/run.rs`, new `apps/xtp/src/preflight.rs`.
- **xcl ref:** `launcher.rs` `check_cpu_features`,
  `missing_console_cpu_features`, `format_missing_cpu_message`.
- **Acceptance:** unit test with an injected feature probe: a host without
  AVX2 gets a message naming it; all present gives none.
- **Depends on:** L-07 (to know what is emulated)

### L-09 The title's own command line — M
- **Goal:** the title sees `<its exe path> <arguments after -->` from
  `GetCommandLineW/A` and from the CRT (`__p___argc`, `__p___argv`,
  `__p___wargv`, `_get_wide_winmain_command_line` and kin it imports), not
  XTP's command line. In-process: XTP keeps one process.
- **Touches:** `crates/xtp-hle/src/modules/kernel.rs` (overrides),
  `crates/xtp-hle/src/runtime.rs` (stores it), `apps/xtp/src/main.rs`
  (`-- args`), the L-06 fixture.
- **xcl ref:** `launcher.rs` `run`, `spawn_host` (argv\[0] = game exe), check
  "argv: command line belongs to the game" in `tests/testtitle`.
- **Acceptance:** the exe probe reads its command line and `argv` and finds
  its own path first and the test's arguments after it.
- **Depends on:** L-06

### L-10 `PEB.ImageBaseAddress` points at the title — S
- **Goal:** system code that asks for the process image (resources with a
  NULL module, `GetModuleHandle(NULL)` from host DLLs) gets the title, as
  on the console. First switch XTP's own uses of "the exe module" (e.g.
  `threads::started_by_rust`, which asks `GetModuleHandleA(NULL)`) to our own
  `__ImageBase`, then set the field before `spawn_main`, not under Wine.
- **Touches:** `crates/xtp-loader/src/threads.rs`, `windows.rs`
  (`spawn_main`), `crates/xtp-hle/src/modules/kernel.rs` (keeps working).
- **xcl ref:** `loader.rs` `run_main` (and its Wine exception).
- **Acceptance:** exe probe: `GetModuleHandleW(NULL)` called from a host
  DLL path (e.g. through `GetModuleFileNameW(NULL)`) names the title; the
  existing `threads` and `load_probe` tests still pass.
- **Depends on:** L-06

### L-11 Thread affinity and processor masks — S
- **Goal:** console affinity masks and group affinities (the title calls
  `SetThreadGroupAffinity`) are folded onto the host's processors instead of
  failing, and `GetLogicalProcessorInformation` answers consistently.
- **Touches:** new `crates/xtp-hle/src/modules/threads.rs`, `modules/mod.rs`
  (its `mod` and `all()` lines).
- **xcl ref:** `hle/kernelx.rs` `SetThreadAffinityMask`,
  `SetThreadGroupAffinity`, `SetThreadpoolAffinityMask`.
- **Acceptance:** unit test: a mask naming more processors than the host has
  is accepted and the thread's effective mask is non-empty and within the
  host's; a valid host mask is unchanged.
- **Depends on:** L-01 (confirm which calls the title makes)

### L-12 Console flags on `VirtualAlloc`/`VirtualProtect`/`VirtualFree` — S
- **Goal:** the memory API set goes through XTP: console-only allocation
  and protection bits are removed (reusing the masks `xmem.rs` already
  defines), everything else reaches the host unchanged; the first use of each
  console bit is logged.
- **Touches:** `crates/xtp-hle/src/modules/xmem.rs` (shared masks), a module
  entry serving `api-ms-win-core-memory-*`.
- **xcl ref:** `hle/kernelx.rs` `VirtualAlloc`, `VirtualProtect`,
  `VirtualFree`, `VirtualAllocEx`.
- **Acceptance:** unit tests: an allocation with console bits set succeeds
  and is readable/writable as the host bits ask; plain calls behave as the
  host's.
- **Depends on:** L-01

### L-13 Overlay bound to the file API — M
- **Goal:** the file imports in `files.rs` go through L-02 for the package
  drive: `CreateFile*`, attributes, delete, `MoveFileEx`, `CopyFile*`,
  `ReplaceFileW`, and `FindFirstFile*`/`FindNextFile` merging both layers
  with whiteouts hidden. `G:` is mounted read-only from then on.
- **Touches:** `crates/xtp-hle/src/modules/files.rs`, `apps/xtp/src/run.rs`
  (overlay folder under the title's data folder).
- **xcl ref:** `vfs_hooks.rs` `hk_create_file_w`, `hk_delete_file_w`,
  `hk_move_file_ex_w`, `hk_copy_file_ex_w`, `hk_replace_file_w`,
  `hk_find_first_ex_w`, `hk_find_next_w`; `vfs.rs` `needs_merge`,
  `has_whiteouts_under`.
- **Acceptance:** tests through the bound functions on a temp package: the
  package folder is byte-identical after writes, renames and deletes; a
  listing shows overlay files and hides deleted ones; `fopen` for writing on
  `G:` lands in the overlay.
- **Depends on:** L-02

### L-14 Console path prefixes and unmapped paths — S
- **Goal:** accept the console's named prefixes (`GAME:` and others the
  census or a run shows) besides drive letters, configurable in `xtp.toml`;
  log each console-looking path that matches no mount once.
- **Touches:** `crates/xtp-hle/src/modules/files.rs`,
  `crates/xtp-core/src/config.rs` (mounts).
- **xcl ref:** `vfs.rs` `Vfs::new` (mounts), `classify_with`,
  `unmapped_paths`.
- **Acceptance:** unit tests: `GAME:\x` and `G:\x` reach the same host file;
  an unknown `Q:` path that is not a host drive is logged once.
- **Depends on:** L-13

### L-15 First-chance C++ throws and thread names — S
- **Goal:** the first 16 first-chance C++ exceptions thrown by title code are
  logged (type, `what()` when readable, `module+rva`); the debugger
  thread-name exception sets the host thread description, so stack dumps
  and debuggers show title thread names.
- **Touches:** `crates/xtp-loader/src/crash.rs`.
- **xcl ref:** `eh.rs` `cpp_exception_logger` (and its thread-name branch),
  `bounded_cstr`.
- **Acceptance:** unit tests: raising the thread-name exception names the
  thread (`GetThreadDescription`); a raised C++-shaped exception whose
  `what()` points at unreadable memory is logged without faulting.
- **Depends on:** –

### L-16 VFS coverage audit — M
- **Goal:** find which host DLLs open files on the title's behalf (Media
  Foundation, XAudio2, `dbghelp`, the host CRT itself) with console paths,
  by logging host `CreateFileW` calls that carry a mounted console path
  during a run; bind or hook only what the log shows. Decide with data
  whether a KERNELBASE hook (new dependency) is needed.
- **Touches:** `crates/xtp-hle/src/modules/files.rs` (a diagnostics-only
  probe), `docs/analysis/loader.md` (findings).
- **xcl ref:** `vfs_hooks.rs` `install` (the hooked set and its re-entrancy
  guard), `hooks.rs` `hook`.
- **Acceptance:** a diagnostics run on the dump through the menus lists every
  escaping path (or none); the decision is written down with that list.
- **Depends on:** L-13

### L-17 Verify TLS patch sites — S
- **Goal:** each `gs:[0x58]` load rewritten in the exe is checked to lie
  inside a function of the image's `.pdata` and on an instruction start found
  by decoding from that function's start; a hit that fails is reported and
  left alone.
- **Touches:** `crates/xtp-loader/src/tls.rs` (and `xtp-pe` for `.pdata`
  ranges).
- **xcl ref:** none for patching (xcl swaps TEB slots instead: `tls.rs`
  `take_slot_zero`); its approach is the fallback if verification ever
  rejects real sites.
- **Acceptance:** unit test: the pattern placed in data or across an
  instruction boundary is not patched; real loads are; on the dump the count
  stays 53,246 (opt-in test).
- **Depends on:** L-06

### L-18 Commit physical pages on first touch — L
- **Goal:** cut the commit the title needs at start (README: several GiB,
  Windows error 1455): zero physical pages are mapped as reserved address
  space and committed when the CPU first touches them; buffers handed to
  kernel calls (file reads, DirectStorage) are committed first. Only if a
  measurement on the dump shows the current "commit when mapped or written"
  path still commits pages nothing touches.
- **Touches:** `crates/xtp-hle/src/modules/xmem.rs`,
  `crates/xtp-loader/src/crash.rs` (the fault is served before the crash
  report sees an access violation), `files.rs` (commit before reads).
- **xcl ref:** `hle/kernelx/lazy.rs` (module doc and its `ensure`).
- **Acceptance:** first a measurement (peak commit in the menus, before) in
  the PR; then unit tests: a mapped zero page is not committed until
  touched, a touched page reads zeros, a `ReadFile` into an untouched page
  succeeds; the peak commit after is reported.
- **Depends on:** L-01; coordinate with the HLE report's XMem items

### L-19 Import call tracer — M
- **Goal:** `XTP_TRACE_IMPORTS=<filters>` wraps chosen bound imports in a
  thunk that logs arguments and result, for bring-up of a new title.
  Diagnostics feature only.
- **Touches:** `crates/xtp-loader/src/windows.rs` (`bind_imports`),
  `thunks.rs` (tracing thunk).
- **xcl ref:** `resolver.rs` `TraceFilter`, `resolve_import`.
- **Acceptance:** `load_probe` with the variable set logs the probe's call
  to `GetTickCount` with its result and the probe still gets 777.
- **Depends on:** L-04

### Order at a glance

| # | Item | Size | Depends on |
|---|---|---|---|
| 1 | L-01 census check | S | – |
| 2 | L-02 overlay logic | L | – |
| 3 | L-03 late DLLs in the loader | M | – |
| 4 | L-04 stub database | M | – |
| 5 | L-05 fast-fail visibility | M | – |
| 6 | L-06 exe test title | M | – |
| 7 | L-07 console instructions | S | – |
| 8 | L-08 CPU check | S | L-07 |
| 9 | L-09 command line | M | L-06 |
| 10 | L-10 PEB image base | S | L-06 |
| 11 | L-11 affinity | S | L-01 |
| 12 | L-12 memory flags | S | L-01 |
| 13 | L-13 overlay on the file API | M | L-02 |
| 14 | L-14 console prefixes | S | L-13 |
| 15 | L-15 C++ throws, thread names | S | – |
| 16 | L-16 VFS coverage audit | M | L-13 |
| 17 | L-17 TLS patch verification | S | L-06 |
| 18 | L-18 lazy commit | L | L-01 |
| 19 | L-19 import tracer | M | L-04 |

Items 1–7 have no dependencies and can run in parallel. L-02 comes early
because writes to the package folder break the rule that the user's game
files are never modified.

### Not to port

- Manual mapping, the `RtlPcToFileHeader` hook and the TLS carrier DLL
  (`loader.rs` `map_image_ex`, `eh.rs` `install_pc_hook`, `tls.rs`
  `install`): Windows already does all three for XTP's prepared images.
- Resolving an import by its name in whichever HLE module has it
  (`resolver.rs`, the "known console function from an unexpected DLL"
  path): a name clash would bind the wrong function silently.
- The child host process (`host.rs`, `launcher.rs` `spawn_host`) as a whole:
  only its effect on the command line is wanted (L-09).

## 6. Risks

- **R-1 Licence.** xcl is GPL-2.0-only; XTP is GPL-3.0-or-later. Code taken
  from xcl could not be relicensed. Workers write from the behaviour and the
  acceptance tests here; a reviewer compares structure and comments of each
  port against the xcl function named.
- **R-2 Undocumented Windows loader behaviour.** XTP relies on Windows
  assigning static TLS to a dynamically loaded DLL only when it has an import
  directory (VIctory's test `windows_assigns_static_tls_to_a_prepared_dll`;
  xcl found the same on Windows 11). A Windows update that changes it breaks
  every TLS image; the test is the alarm.
- **R-3 TLS patch false positives.** The exe rewrite matches bytes, not
  instructions. It was checked once for one build of one title; a new build
  could hide the pattern in data. L-17 closes this.
- **R-4 Overlay migration.** VIctory mounted `G:` writable. If earlier runs
  wrote into the game folder, those files are now indistinguishable from
  package files. Check the dump against its package before L-13 lands.
- **R-5 `PEB.ImageBaseAddress`.** Rust std, the host CRT and XTP itself may
  take the PEB's image for theirs (resources, module path). L-10 audits our
  own uses first; a regression shows as wrong paths or a missing icon.
- **R-6 Forwarded exports.** Title DLL exports are looked up with
  `GetProcAddress`, which sends forwarders to host DLLs, not to HLE. The
  dump has none; a new title could.
- **R-7 Fault-driven commit.** L-18 adds a first-chance access-violation
  path ahead of the crash reporter, which prints access violations as they
  happen. Ordering mistakes make every lazy page look like a crash, or hide
  real ones.
- **R-8 Wine.** xcl skips the PEB change under Wine and VIctory commits
  sections whole there; items touching the PEB, memory commit or hooks need
  a Wine note in their PR.
- **R-9 Parallel rename.** Paths here use the `xtp-*` names; `xtp-hle` is
  assumed by the same rule. Items started before the rename lands use the
  `victory-*` paths.

## 7. Open questions

1. Unresolved imports today: not measured in this session (no build). L-01
   answers it; until then VIctory's README, which lists nothing the loader
   still needs, is the only source.
2. Stub default: keep `Trap` (VIctory: fail at the cause) or switch to the
   database values (xcl: keep going)? L-04 keeps `Trap` and makes the table
   opt-in; the person decides whether to flip it.
3. Does Grand Theft Auto VI read its command line or `argv` at all? If not,
   L-09 can wait.
4. Which console bits does the title pass to `VirtualAlloc` and
   `SetThreadGroupAffinity`, and with what masks? Answered by logging first
   (L-11, L-12).
5. Does the title use `GAME:` or other named prefixes, or only drive letters?
   (L-14 is sized for "a few".)
6. Do any host DLLs open title files with console paths (L-16)? If none,
   XTP never needs inline hooks.
7. Should the console builds of `msvcp140*`/`vcruntime140*` the game ships
   keep going to the host's (today's `HostPolicy`) or be mapped as title
   DLLs? It works today; differences in their behaviour would show as
   crashes in the C++ runtime.
8. Is the e2e exe fixture (L-06) allowed to depend on a C++ toolchain to
   throw real C++ exceptions, or does it stay Rust-only (panics and SEH)?
