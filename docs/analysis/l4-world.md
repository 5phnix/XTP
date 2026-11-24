# The cloud L4 and the Series S world: two stalls on the loading screen

Why the cloud test machine (Google Cloud `g2-standard-16`, NVIDIA L4 under
RTX Virtual Workstation, Windows Server 2025, `docs/dev/cloud-setup.md`)
sits on BTP 6's loading screen where the person's PC reaches the Series S
world in 20 to 27 s. Measured on 2026-11-24. Times are UTC. Logs, captures,
stack dumps and debugger output stay outside the repository, under
`E:\xtp-data\cloud-runs\` and `E:\xtp-data\tmp\l4world\` (local copies) and
`D:\xtp-data\runs\` (on the VM). Title code is named by RVA in
`game_scarlett_master.exe` (build blake3 `b99f876b74d4a77f…`). No title
bytes or listings are quoted.

## 1. Summary

Two things stand between the VM and the world. The first is XTP's and is
fixed here. The second is a wait inside the title that the VM's timing
triggers. It is characterised here but not yet fixed.

- **A. Emulated `clzero` made every frame slow (fixed).** The VM's CPU is an
  Intel Xeon (Cascade Lake, 2.2 GHz), which lacks AMD's `clzero`. A title
  worker zeroes structures with `clzero` loops, 20 of them per frame, and
  each one traps into XTP's emulation (`xtp_loader::crash::emulate_amd_only`).
  Before zeroing a loop in one go, the emulation checked its pages with
  `VirtualQuery`. In the title's 1 096 MiB heap that call cost **4.5 ms**,
  because it measures the whole run of pages alike, page by page. The
  loading screen ran at **7 fps** (124 ms a frame outside submissions). The
  fix (`crates/xtp-loader/src/crash.rs`) asks the working set for each page
  instead (`QueryWorkingSetEx`, about 1 µs). After it: 0 `VirtualQuery`,
  6 ms of emulation per 5 s, and a loading screen at **30 fps**. The
  person's PC (Ryzen 9 7950X3D) runs `clzero` natively and never took this
  path. Any Intel host did.
- **B. The load then waits on itself (open).** With A fixed, the world
  still does not come: GPU memory stays at 2.6 GB, and no thread of the
  title is busy. A debugger attached to the stalled process shows a
  circular wait inside the title (§6):
  - the mutex of a title manager object (`poi(title+0xa924088)+0x3ced0`)
    is held by a job worker that is idle, so a job took it and gave the
    thread back without releasing it;
  - two priority-15 threads of the title wait on that mutex;
  - the title's streaming worker waits for a token on a counter
    (`title+0x75e0e78`) that only the completion of a job gives back.
  On the PC the same waits happen and clear within a second. The VM falls
  into them and never comes out. The orchestrator reports the same
  signature on the PC with builds that make each frame slower, so this
  looks like a race on the title's timing, not a property of the L4.
- **Not the cause**: DirectStorage (every request completes, none waits),
  the disk (512-byte logical sectors, no failed read), the L4's driver,
  budget or VidMm (GPU memory flat, residency never refused), the golden
  save and the FidelityFX HLE (§7). The title starts the same 49 workers on
  16 and on 32 logical processors. What else differs between the VM and
  the PC, and how to check each point, is listed in §8.

## 2. Runs

| Run (VM folder) | Local copy | Code | What | Result |
|---|---|---|---|---|
| S1 `20261124-195703-l4-main-hle` | `cloud-runs\20261124-205505-game` | `main` `ff139ca` | the orchestrator's run: autopilot from the golden save, 900 s | loading screen to the end, 7 fps, GPU memory 2 622 MiB |
| S2 `20261124-202015-l4world-stacks` | `cloud-runs\20261124-211831-game` | `main` `df510ee` | `XTP_STACKS=40,60,90,130,180`, DirectStorage at debug | same; §4 |
| S3 `20261124-202946-l4world-probe` | `cloud-runs\20261124-212801-game` | `2863e58`: `df510ee` plus counters in the emulation (local branch, never delivered) | the counters every 5 s | same; §4 |
| S4 `20261124-205444-l4world-fix` | `cloud-runs\20261124-215300-game` | `03d52cb` (fix A) | captures every 256 frames, 420 s | loading screen at 30 fps, world not reached |
| S5 `20261124-210452-l4world-probe2` | `cloud-runs\20261124-220312-game` | `c6d27ae`: `03d52cb` plus the counters (local branch, never delivered) | `XTP_STACKS` ×6, counters, DirectStorage at debug | as S4; §5, §6 |
| S6 `20261124-213400-l4world-cdb` | (log not fetched) | `03d52cb` | `cdb -pv` at +200 s: every stack | as S4; §6 |
| S7 `20261124-224016-l4world-cdb2` | `cloud-runs\20261124-224101-game` | `03d52cb` | `cdb -pv` at +170 s: stacks, registers, the mutex and the counter | as S4; §6 |
| PC `20261124-195925` | `E:\xtp-data\cache\play\` | `main` of 19:59 | `cargo xtask play`, Series S | world 13 s after `story000.rpf` opens |
| PC `20261124-163332` | `E:\xtp-data\cache\play\` | `main` of 16:33 | Series S, `XTP_STACKS` every 1 s | world at +38 s; its 14 dumps during the load are the reference of §6 |

Every VM run used `run-remote.ps1 -Game` with `--console series-s`,
`XTP_SKIP_INTRO=1`, `XTP_WATCHDOG_HOOK=1` and the autopilot
(`autopilot\gameplay.txt`, or a copy with longer timeouts in S1). The
commit each one built is in its summary line (`commit …`).

## 3. Symptoms against the PC

In every VM run:

- The front end ends on time (autopilot line met at +9.7 to +10 s), the
  intro is cut, and the world's packs open: `story000_script.rpf`,
  `story000.rpf` and its three `.ttpf` files, about +30 s after the start.
  That is also the last file the PC opens before its world. The files are
  the same 79 on both machines.
- The title stays on its loading screen (`0xab43664` = 9, the Series S
  loading value; `0x9e58138` = 1; byte `0x74dfa9d` = 0), with 9 draws a
  frame. GPU memory stays at 2 622 MiB and the host textures at about
  1 235 (2.3 GB). On the PC, xtp.exe goes from 4.4 to 8.3 GB of video
  memory and the world comes 13 s after `story000.rpf`.
- About 8 s after `story000.rpf`, the PC loads three string tables
  (`memory:…str:00051`, `00048`, `00052`) and its world passes start
  (hundreds of first pipeline binds, 2 343 draws by frame 512). The VM
  never gets there.
- No error: no failed DirectStorage request, no failed read, no crash, no
  watchdog firing.

## 4. Stall A: `clzero` emulation

### 4.1 Where the time went

S2 took every thread's stack five times (+40 to +180 s). The stacks were
symbolised with `cdb` against the PDB of the build that ran and against
the VM's own system DLLs, fetched from the VM, with Microsoft's public
symbols. One title thread works: a worker (start `0x2215ea0`, priority 7)
is, in all five dumps, inside XTP's vectored handler, emulating a `clzero`
of the function at RVA `0x4e8aa10`. The innermost frames are
`ntdll!NtQueryVirtualMemory` ← `KERNELBASE!VirtualQuery` ←
`xtp!xtp_loader::crash::handler` (the inlined `writable` of a `clzero` loop)
← `ntdll!RtlpCallVectoredHandlers` ← `KiUserExceptionDispatch` ← title.
Every other thread waits.

The function at `0x4e8aa10` zeroes five consecutive spans of a structure,
from 0x40 to 0x7440 bytes past its start (about 29 KB), with five `clzero`
loops (one cache line per pass). Each loop has the form
`insn::clzero_loop` recognises, so XTP's handler zeroes a whole loop in one
exception. A scan of the title's code finds 11 `clzero` (these 10 and one
elsewhere), 2 `monitorx` and 2 `mwaitx` (one spin-wait function) and 1
`rdpru`.

S3 counted the emulation from the inside. Every 5 s in steady state (from
about +45 s): 1 019 to 1 102 `clzero` loops; no line emulated alone;
nothing else emulated (one `monitorx` in the whole run); 4 820 to 4 940 ms
in the emulation, of which 4 818 to 4 938 ms in `VirtualQuery`, one call
per loop; every call reporting a region of 1 096.1 MiB; no check refused;
5.9 to 6.4 MB zeroed. That is 4.5 ms per loop, nearly all of it in
`VirtualQuery`. Over the 145 s of S3: 33 549 loops, 190 MB zeroed, 133.6 s
in `VirtualQuery`.

### 4.2 Why `VirtualQuery` costs that much

Measured on the VM with a small C# program (`Add-Type`), outside XTP: one
region of N MiB is committed read-write and every page is touched, then
`VirtualQuery` is timed one page into it (50 calls), and
`K32QueryWorkingSetEx` on 8 of its pages (2 000 calls).

| region | `VirtualQuery` | `RegionSize` reported | `QueryWorkingSetEx`, 8 pages |
|---|---|---|---|
| 16 MiB | 104 µs | 15 MiB | 1.5 µs |
| 256 MiB | 1 599 µs | 255 MiB | 1.2 µs |
| 1 024 MiB | 6 799 µs | 1 023 MiB | 1.2 µs |
| 16 MiB, write-watched | 99 µs | 15 MiB | 1.2 µs |
| 256 MiB, write-watched | 1 697 µs | 255 MiB | 1.2 µs |
| 1 024 MiB, write-watched | 7 395 µs | 1 023 MiB | 1.2 µs |

`VirtualQuery` grows linearly with the region (about 7 µs per MiB). The
working-set query does not, and it gives each resident page's current
protection.

### 4.3 The fix

`crates/xtp-loader/src/crash.rs`, `writable` (the check before a whole
`clzero` loop is zeroed in one exception):

1. For the pages of the range, 32 at a time, `K32QueryWorkingSetEx` gives
   each page's working-set entry. A page in the working set (`Valid`) is
   judged by its `Win32Protection`: read-write or copy-on-write, executable
   or not, and no `PAGE_GUARD`. A resident page that is not writable
   answers "no" at once.
2. A page outside the working set (never touched, or trimmed), or every
   page if the call fails, is asked of `VirtualQuery`. That one answer
   covers every page up to the end of the run it reports, so a loop over
   untouched memory costs one call, not one per page.
3. The rest is unchanged: a loop whose pages are all writable is zeroed in
   one exception, otherwise only the faulting line is.

No page is asked about once per 64-byte line: the whole loop's range is
checked once, each page once. Lines zeroed one at a time (a loop that is
not writable, a lone `clzero`) are not checked at all, as before.
`crates/xtp-loader/Cargo.toml` gains the windows-sys feature
`Win32_System_ProcessStatus`.

Tests (`crash::tests`):

- `writable_answers_for_touched_untouched_and_protected_pages`: touched
  pages, untouched committed pages, both over two batches, a page past the
  allocation, a resident read-only page, an untouched guard page, a
  reservation without commit, and a range past the end of the address
  space.
- `a_loop_in_a_large_touched_heap_asks_no_region_query`: a 29 KB loop in
  a 16 MiB touched heap (its pages locked in the working set) makes no
  `VirtualQuery` call; a loop over 64 untouched pages makes one.
- `working_set_flags_say_whether_a_resident_page_is_writable`: the
  decoding of the entry's flags.
- The existing emulation tests, among them
  `a_clzero_loop_into_memory_it_cannot_write_goes_a_line_at_a_time` and
  `rdpru_rdpid_and_clzero_loops_run_on_any_cpu`, which on the VM's Xeon
  goes through the real handler. `cargo test -p xtp-loader` passes on the
  VM (80 tests) and on the build farm.

## 5. After the fix

S4 and S5 run the fix. S5 also carries the counters of S3.

| per 5 s, steady state | S3 (before) | S5 (after) |
|---|---|---|
| `clzero` loops emulated | 1 019 to 1 102 | 2 920 to 3 000 |
| `VirtualQuery` calls | one per loop | 0 |
| time in the emulation | 4 820 to 4 940 ms | 5.8 to 6.6 ms |
| bytes zeroed | 5.9 to 6.4 MB | 16.9 to 17.4 MB |
| loading screen | 7 to 11 fps, 80 to 124 ms a frame outside submissions | 30 fps, 28 ms outside submissions |

The loops run at exactly 3 000 per 5 s, 20 per frame at 30 fps. The
zeroing was per-frame work, on the path of every frame. It explained the
VM's slow front end and loading screen, but not the stop of the load: in
S4 and S5 the world still does not come.

## 6. Stall B: a circular wait inside the title

### 6.1 Nobody is busy

S5 took six stack dumps (+55 to +180 s). No title thread runs; every one
of them waits. Against the PC's dumps during a load that succeeds
(`cache\play\20261124-163332`, 14 dumps from `story000.rpf` to the
world), the title's threads are the same: 49 workers started at
`0x2215ea0` with the same priorities (6 at 1, 3 at 4, 8 at 5, 11 at 6, 2
at 7, 13 at 8, 6 at 15), 5 at `0x23e10` and 1 at `0x55e8b00`. What differs:

- **The streaming worker** (priority 7; `0x23cf104` → `0x23cf247` →
  `0x4fd1ae5` → `0x4fd1e88` → `0x11fdbd` → `0x1209a0` → `0xc9e603` →
  `0x24848bd` → `0xcd72e7`) waits in `0xcd73d9`, in every dump. On the PC
  it creates textures at their addresses (`0x193dffe`, the device's
  method 0x428) and waits in `0xcd73d9` only now and then (4 of the 14 dumps),
  for a second at most.
- **Two priority-15 threads** wait on a title mutex (`0x4b40` →
  `0xa7c330`): the loop at `0x1e95xxx` (through `0x5594650`) and the loop
  at `0x43adxxx` (through `0x43ae0f0`). On the PC the first sleeps between
  rounds (`0x1e951bb`) and takes that mutex regularly. The second waits
  at `0x43adeda`.

### 6.2 What they wait for

The function at `0xcd7320` takes a token from a 64-bit counter at
`title+0x75e0e78`: the upper 32 bits count the tokens. With none left it
spins, then sleeps in the title's wait-on-address (`0x6180`). With a token
it fills a job and queues it (`0x1f6330`, queue `poi(title+0x8759ff8)`),
passing the counter's address. The job's completion gives the token back.

The title's mutex (`0x4b40`) keeps its owner's thread id
(`GetCurrentThreadId`, cached in the thread's TLS) in the upper 32 bits of
its first word, and its recursion count at +0x10.

Two non-invasive `cdb` attaches (`cdb -pv`) on the stalled process:

| read | S6 (+200 s) | S7 (+170 s) |
|---|---|---|
| token counter `title+0x75e0e78` | `ffffffff00000000` | `ffffffff00000005` |
| manager `poi(title+0xa924088)` | (not read) | `0x90927bf80` |
| its mutex, `+0x3ced0` | (not read) | `00000e88_00000003`, recursion 1 |
| thread 0xe88 | – | a job worker, idle at `0x221c3c2` ← `0x2215eca` |
| what the two priority-15 threads wait on | – | `rsi` = `0x9092b8e50` in both (`.frame /r 6`): that mutex |

The counter holds no token and one thread waits for one: the streaming
worker. The manager's mutex is held by thread 0xe88, a job worker with no
job: a job took the mutex on it and gave the thread back without releasing
it. The title most likely keeps that mutex across asynchronous work and
lets a continuation release it. Neither that continuation nor the
completion that would return the streaming worker's token has come. No
title C++ exception was thrown (XTP logs the first 16), and the title
imports no fibre functions.

### 6.3 Reading

A circular wait inside the title, triggered by timing. On the PC that
loads, the same pieces show: the worker waits for a token now and then,
and the `0x1e95` thread takes the mutex regularly. Both clear within a
second. On the VM they never do. The orchestrator reports the same
signature on the person's PC with integration builds that make each frame
slower (the world-black task, `c19d640b`): the trigger is the rhythm of
the title's frames and jobs, not the L4 or the Intel CPU.

Not established yet: which job holds the manager's mutex, which completion
it waits for, and what XTP does (or delays) that keeps that completion
from coming. Finding them means reading the job queue at
`poi(title+0x8759ff8)` and the job in flight in the stalled process.

### 6.4 Reading the stalled process again

The addresses (build 1.0.2.1; `T` is the title module as `cdb` names it,
`xtp_image_b99f876b74d4a77f`):

| What | Where | Value in S7 |
|---|---|---|
| manager object | `poi(T+0xa924088)` | `0x90927bf80` |
| its mutex | manager + `0x3ced0` (owner TID in bits 32-63, recursion count at +0x10) | `00000e88_00000003`, recursion 1 |
| job tokens of the streaming worker | `T+0x75e0e78` (tokens in bits 32-63) | `ffffffff_00000005` |
| the job descriptor `0xcd7320` fills before it queues | `T+0x75e0dd8` (type byte), entries from `T+0x75e0de0` | not read yet |
| the job queue it goes to | `poi(T+0x8759ff8)` | `0x2073f7e6da0` |
| the mutex's owner | the TID read above, here 0xe88: a job worker idle at `T+0x221c3c2` | thread ids change at every run |

On the VM: copy `cdb.exe`, `dbgeng.dll`, `dbghelp.dll`, `dbgcore.dll`,
`dbgmodel.dll`, `msdia140.dll`, `srcsrv.dll`, `symsrv.dll` and
`winext\ext.dll`, `winext\uext.dll` from a Windows debugger install to
`C:\xtp\jobs\cdb`. Then, once the title has sat on the loading screen for
a minute or more, from `tools\cloud\invoke-vm.ps1 -File`:

```powershell
$p = Get-Process -Name xtp | Select-Object -First 1
& C:\xtp\jobs\cdb\cdb.exe -pv -p $p.Id -cf C:\xtp\jobs\cdb-cmds.txt 2>&1 |
    Out-File -Encoding utf8 C:\xtp\jobs\cdb-out.txt
```

`-pv` attaches without stopping the process for longer than the reads,
and `qd` detaches. Commands go in a file (`-cf`): PowerShell mangles quotes
on a `-c` command line. S6's file:

```text
.reload
lm m xtp*
~*kn 45
qd
```

S7's file (registers of every thread at frame 6, which for the two
priority-15 threads is the mutex's lock function, `T+0x4b40`, with the
mutex in `rsi`):

```text
.reload
? poi(xtp_image_b99f876b74d4a77f+0xa924088)
dq poi(xtp_image_b99f876b74d4a77f+0xa924088)+0x3ced0 L4
dq xtp_image_b99f876b74d4a77f+0x75e0e78 L2
dq xtp_image_b99f876b74d4a77f+0x8759ff8 L1
~
~*kn 30
~*e .printf "@@THREAD %x\n", @$tid; .frame /r 6; dq @rsi L2; .frame /r 7
qd
```

The owner is the thread whose `Id: <pid>.<tid>` line in `~` matches the
mutex's upper word. The next reads: the descriptor at `T+0x75e0dd8`, the
queue's head and its pending jobs at `poi(T+0x8759ff8)`, and an invasive
attach with a breakpoint on the job's completion (the release that adds
`1 << 32` to the tokens) to see whether it ever runs.

## 7. What it is not

| Lead (from the task) | Verdict |
|---|---|
| A CPU difference | Half of it. `clzero` emulation (stall A) is Intel-only and fixed. The number of host CPUs is not: the title starts the same 49 workers on 16 and on 32 logical processors. The title reads `cpuid` itself, which XTP cannot intercept, so a CPU-dependent path in stall B is not excluded, but the same signature on the PC (§6.3) argues against it. |
| DirectStorage requests that never complete | No: every request completed, none failed, none waited (`waiting=0`). After the `story000` files the title stops asking for data. |
| The disk (read-only, sector size, alignment, unbuffered reads) | No: the VM's `D:` has 512-byte logical sectors (4 096 physical), as an NVMe drive. The front end reads 5 000 to 6 000 requests per 5 s and no read failed. |
| A race hidden by the PC's speed | Yes for stall B, inside the title (§6). Not the race between `watch::guard` and `kernel_will_write` that the archive task found (`archive-null.md`): no `0x800706f8`, 998 or 1784, and no thread retried a read. |
| The golden save copy | No: the save loads and the title opens the first mission's packs, as on the PC. |
| The L4 (vWS, 22 GB budget, no Resizable BAR, driver 596.86, VidMm) | Not involved: GPU memory stays at 2.6 GB and residency was never refused. The budget is not passed to the title. The one device warning at start-up (a view heap capped at 1 000 000 descriptors) appears on the PC too. |
| The FidelityFX DLL | Ruled out by S1, which has the HLE. |

## 8. Still open

- **Stall B**: see §6.3 and §6.4. It needs the job queue and the job in
  flight read in the stalled process, then a fix in XTP for whatever keeps
  that job's completion from coming.
- **What XTP does differently on the VM from the PC that loads.** Each
  point below says what XTP does today and how to check it. On the VM
  every check is one run with the `cdb` attach of §6.4.
  1. *Thread priorities.* The title sets its own (`SetThreadPriority`,
     imported from the host, not intercepted): 6 threads at 15
     (time-critical), 13 at 8, down to 6 at 1, the same on both machines.
     XTP's own threads run at normal priority (6): DirectStorage workers,
     pipeline workers (no longer below normal since VIctory #1505), the
     GPU finisher that signals title fences once submissions finish
     (`queue::signal` → `Host::after`). On 16 logical processors, 6
     time-critical and 13 above-normal title threads leave less room for
     XTP's normal-priority finisher than on 32. Check: `~*kn` shows no
     thread running in stall B, so starvation would show as a *late*
     completion, not a missing one. Log the delay between a title
     `Signal` and its `fence::signal` (finisher) in both runs.
  2. *Affinity.* The title calls `SetThreadGroupAffinity` itself (host,
     not intercepted), with masks written for the console's cores. On 16
     logical processors and on 32, the same bit means a different core:
     an SMT sibling on one, a whole core on the other. Check: log every
     `SetThreadGroupAffinity` call (thread, mask) on both machines through
     an HLE stand-in. Then try running the VM with the masks dropped, or
     mapped to whole cores.
  3. *Cores the title sees.* `GetLogicalProcessorInformation` is the
     host's (not intercepted): 8 cores and 16 threads with an Intel cache
     layout on the VM, 16 cores and 32 threads with two AMD L3s on the PC.
     The title starts the same 49 workers either way (§6.1), but it may
     size other things from it (job batches, queue depth, the 127 the title
     stores at `T+0x75e0e78` when it starts). Check: an HLE stand-in that reports the
     console's topology (8 cores with SMT, one L3) on both machines, and
     whether the VM then loads.
  4. *Synchronisation calls.* Events, semaphores and waits with time-outs
     (`CreateEventW`, `CreateSemaphore(Ex)W`, `ReleaseSemaphore`,
     `SetEvent`, `WaitForSingleObjectEx`) and the clocks
     (`QueryPerformanceCounter`, `GetTickCount64`) go straight to the host.
     XTP adds no latency there. The title's own wait-on-address (`0x6180`,
     the token and mutex waits above) sleeps in `WaitForSingleObject` on
     its own events. Timers: XTP asks for 1 ms (`timeBeginPeriod(1)`) and
     opts out of Windows' timer throttling for hidden windows
     (`runtime::use_console_timers`). The VM's title runs in a desktop
     session nobody watches. Check: read the effective timer resolution
     from another process while the title runs on the VM
     (`NtQueryTimerResolution`), and the PC's for comparison.
  5. *The GPU's pace.* The L4 takes 10 to 11 ms a frame on the loading
     screen where the 4080 SUPER takes 1.5 to 3. Title fences (and any job
     that waits for one) complete later on the VM. That matches the
     orchestrator's report: integration builds slower per frame stall the
     same way on the PC. Check: reproduce on the PC with a deliberate
     delay before each fence signal, and attach `cdb` there (§6.4) to see
     whether the mutex owner and the token counter read as on the VM.
- **One exception per `clzero` loop remains** on CPUs without it. Patching
  the recognised loops when the image is mapped would remove even that.
  After the fix it costs about 6 ms per 5 s on the VM, so it is not
  urgent.
- **The VM's GPU frame** on the loading screen takes 10 to 11 ms against
  1.5 to 3 ms on the PC. That is expected of an L4 against a 4080 SUPER,
  but it changes the title's rhythm.

## 9. Method

- Stacks: `XTP_STACKS` (`crates/xtp-loader/src/stacks.rs`), parsed and
  symbolised with `E:\xtp-data\runs\frame-prof-tools\stacks.py` (`cdb -z`
  with the PDB). The VM's `xtp.exe`, `xtp.pdb` and system DLLs were copied
  from the VM right after each build, before anyone else could rebuild.
- Counters (S3, S5): local branches only (`l4world-probe` `2863e58`,
  `l4world-probe2` `c6d27ae`), never delivered: atomics in
  `emulate_amd_only` and `writable`, and a thread that prints them every
  5 s when `XTP_EMU_STATS` is set.
- Debugger (S6, S7): the debugger's command-line engine copied to the VM
  (`cdb.exe` and its DLLs, 10 MB), attached non-invasively (`-pv`) to the
  stalled process from an SSH session. Commands from a file (`-cf`): the
  module list, `poi`/`dq` reads of title globals, `~*kn`, and
  `~*e .frame /r 6; dq @rsi`.
- The `VirtualQuery` benchmark: a PowerShell script with an inline C#
  class, run on the VM through `tools\cloud\invoke-vm.ps1 -File`.
- Title code: byte-pattern scans of its executable section (AMD-only
  instructions, references to `0x75e0e78`), then `cdb -z` on the mapped
  image (`E:\xtp-data\cache\images\`) to read the functions around them.
  Addresses `cdb` prints relative to an export are converted to RVAs from
  the absolute address it gives next to them.
