# The cloud L4 and the Series S world: two stalls on the loading screen

Why the cloud test machine (Google Cloud `g2-standard-16`, NVIDIA L4 under
RTX Virtual Workstation, Windows Server 2025, `docs/dev/cloud-setup.md`)
sits on BTP 6's loading screen where the person's PC reaches the Series S
world in 20 to 27 s. Measured on 2026-11-24 and 25. Times are UTC. Logs,
captures, stack dumps and debugger output stay outside the repository, under
`E:\xtp-data\cloud-runs\`, `E:\xtp-data\tmp\l4world\` and
`E:\xtp-data\tmp\l4lock\` (local copies) and `D:\xtp-data\runs\` (on the
VM). Title code is named by RVA in the title executable (build blake3
`b99f876b74d4a77f…`). No title bytes or listings are quoted.

## 1. Summary

Two things stand between the VM and the world. The first is XTP's and is
fixed here. The second is a missing unlock in the title's audio output,
which only a machine without an audio output reaches. It is proven here:
with that lock released at run time, the VM reaches the world. XTP does not
fix it yet (§8).

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
- **B. The title's audio output keeps its own lock (found, proven, not
  yet fixed in XTP).** With A fixed, the world still does not come: GPU
  memory stays at 2.6 GB, and no thread of the title is busy. A debugger
  attached to the stalled process shows the mutex at
  `poi(T+0xa924088)+0x3ced0` held by an idle job worker, two priority-15
  threads waiting on it, and the streaming worker waiting for job tokens
  (§6.1, §6.2). That object is the title's **audio output**, and the mutex
  is its device's (§6.3).
  - The VM has **no audio output**: no render endpoint at all (§6.5). XTP
    passes WASAPI through, so the title's open of its output fails (it
    counts 0 active endpoints) and its device keeps 0 output streams.
  - A per-frame audio job then calls `T+0x5267720` on the device. That
    function locks the device and, finding no stream, **returns without
    unlocking** (§6.4). On a console, or a PC with speakers, the device has
    streams and that return is never taken.
  - It happens on the first call, within 8 s of start-up (S8), and S1 to
    S7 all stalled: it does not depend on the machine's pace. From then on
    the device loop and the audio update loop (the two priority-15
    threads) wait for ever, and so, behind them, does the world load.
  - **Proof (S8, §6.6):** the same build, with that one return redirected
    at run time to the function's own unlocking exit, reaches the world on
    the L4. The autopilot meets the world condition at +98 s, GPU memory
    rises to 7.2 GB, and the world's audio and string tables come as on
    the PC.
- **What XTP does differently from the PC** is nothing of its own: it
  shows the title the host's audio endpoints as they are, and the VM has
  none. Thread priorities, affinity, the core count, the sync calls,
  I/O completions and thread pools are not involved (§8.2).
- **Not the cause**: DirectStorage (every request completes, none waits),
  the disk (512-byte logical sectors, no failed read), the L4's driver,
  budget or VidMm (GPU memory flat, residency never refused), the golden
  save and the FidelityFX HLE (§7).
- **Next step** (§8.1): make XTP release that lock, or give the title an
  audio output, so that every VM run reaches the world without a debugger.
  Once there, the world runs at 2 to 3 fps on the L4, a separate problem.
  Meanwhile `run-remote.ps1 -Game -AudioLockStopgap` releases it on the VM
  with cdb, as S8 did (S9 reached the world); the silent output is planned
  in §8.1.

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
| S8 `20261124-231421-l4lock-fix` | `cloud-runs\20261125-001206-game`, `tmp\l4lock\s8` | `main` `1a4267b` | `cdb` attached at +23 ms; at the leak, its stack, then the return redirected to the unlocking exit and `cdb` detached; `cdb -pv` at +269 s; 300 s | **world reached** at +98 s (autopilot), 2 to 3 fps there; §6.6 |
| S9 `20261124-234018-stopgap` | `cloud-runs\20261125-003821-game` | `37a35f1`: `main` `0dac909` plus step 0 | `-AudioLockStopgap` (§8.1): the same redirect, made by `vm\audio-lock-stopgap.ps1` at the first audio call (+5.4 s); 300 s | **world reached** at +88 s (autopilot), 2 to 3 fps there; §8.1 |
| PC `20261124-195925` | `E:\xtp-data\cache\play\` | `main` of 19:59 | `cargo xtask play`, Series S | world 13 s after `story000.rpf` opens |
| PC `20261124-163332` | `E:\xtp-data\cache\play\` | `main` of 16:33 | Series S, `XTP_STACKS` every 1 s | world at +38 s; its 14 dumps during the load are the reference of §6 |

Every VM run used `run-remote.ps1 -Game` with `--console series-s`,
`XTP_SKIP_INTRO=1`, `XTP_WATCHDOG_HOOK=1` and the autopilot
(`autopilot\gameplay.txt`, or a copy with longer timeouts in S1); S9 only
the autopilot, `-Local`, Series S being the default by then. The commit
each one built is in its summary line (`commit …`).

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

## 6. Stall B: the title's audio output keeps its own lock

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
it. No title C++ exception was thrown (XTP logs the first 16), and the
title imports no fibre functions. It is not kept across asynchronous work
for a continuation to release, as first supposed: the job's function
returns without unlocking (§6.4).

### 6.3 The object is the title's audio output

Read in the title's code (1.0.2.1, `cdb -z` on the mapped image, functions
bounded by `.pdata`, import names from the import slots). Of the eight
instructions that address `+0x3ced0` from that object, two belong to the
loops of the two priority-15 threads, one to the method of §6.4, and the
others to functions that build, open, start, stop and reset an audio
device:

- `T+0x43aea20` **opens the output**: `CoCreateInstance` of the
  `MMDeviceEnumerator` (kept at `+0x3ce28`),
  `RegisterEndpointNotificationCallback` (its client at `+0x3ce20`),
  `EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)` (`+0x3ce30`) and its
  `GetCount` (kept at `+0x3dd80`). **With a count of 0 it returns false.**
  With several endpoints it reads each one's form factor; then
  `GetDefaultAudioEndpoint(eRender, eConsole)` (`+0x3ce38`) and
  `T+0x5c59500`, which activates `IAudioClient` (for the mix format) and
  `ISpatialAudioClient` (interface ids read from the image), creates the
  output streams (`T+0x1e7f580`) and sets `+0x3dd84` = 1.
- The structure at `+0x3ced0` is the **output device**. Its first 0x20 bytes
  are the mutex of §6.2 (lock `T+0x4b40`, unlock `T+0x4d9dab0`). `+0x52c`
  counts its output streams (0x58-byte entries from `+0x530`). Its reset
  (`T+0x5bb64a0`, reached from `T+0x5bb6410`) releases them and sets that
  count to 0.
- The two priority-15 threads are the device loop (`T+0x43adc40`: each pass
  locks the device in `T+0x43ae0f0` to walk its streams, then sleeps) and
  the audio update loop (`T+0x1e95040`). A third one (`T+0x1da8740`, waiting
  at `T+0x1da8776` in every dump on both machines) reopens the output after
  a device change: stop, reset, 100 ms sleep, open, until the open succeeds.

So the brief's "streaming manager" is the audio output, and the mutex is
its device's.

### 6.4 The leak

A per-frame audio job (`T+0x537f810`, which runs `T+0x6d8da0` once per
frame) ends with a virtual call, slot 0x80 of the output's vtable
(`T+0x65f48b0`, the slot at `T+0x65f4930`): `T+0x5267350`. That method does
nothing while a device error or a reset is flagged (`+0x3ceba`, `+0x3cebb`,
`+0x3cdfa`). It does not check whether the output was ever opened. It calls
`T+0x5267720` on the device, which:

1. locks the device's mutex (`T+0x526773b`);
2. if the device has no output stream (`+0x52c` = 0), **returns at once
   without unlocking** (`T+0x5267747` jumps to `T+0x5267cfb`, a plain
   return);
3. otherwise does its work and leaves through `T+0x5267c7f`, which unlocks
   (a tail call to `T+0x4d9dab0`).

On a console, and on a PC with an audio output, the device has its streams
when this method runs, so step 2 never happens. On a machine
with no active render endpoint, the open fails at count 0, the device keeps
0 streams, and **the first call leaves the mutex locked by the job worker
that ran it**. That is the state S7 read: owner a job worker with no job
(0xe88), recursion 1, and two waiters, the device loop and the audio update
loop, which never run again.

### 6.5 What differs on the VM: it has no audio output

On the VM (2026-11-25): `Audiosrv` and `AudioEndpointBuilder` run, but
there is no `AudioEndpoint` device, no sound device (`Win32_SoundDevice`)
and no render endpoint under `MMDevices\Audio\Render`.
`docs/dev/cloud-setup.md` already says "the machine has no sound card".
The person's PC has an audio output: its test sessions play sound (they
have been muted per session since 2026-11-24 21:00, which silences the
session, not the endpoint).

XTP passes WASAPI through. `crates/xtp-hle/src/modules/audio.rs`: "Mixed
audio leaves through the host's WASAPI, which titles and middleware reach
through COM like on PC". The title therefore sees the host's endpoint list
as it is. A console always has an audio output; a host without one shows
the title a state its console build never meets, and takes it down a path
with a missing unlock.

This does not depend on pace. In S8 (§6.6) the first call takes the leaking
path within the first 8 s, during start-up, long before the world load.

### 6.6 Proof: S8 reaches the world

S8 runs `main` `1a4267b` (fix A, nothing else) as S4 to S7 did, with
`run-remote.ps1 -Game -Local -Autopilot autopilot\gameplay.txt -Seconds 300`.
Beside it, a script on the VM (`invoke-vm.ps1 -File`) attaches `cdb`
invasively to the run's `xtp.exe` 23 ms after it starts, with every
exception left to XTP (`sxi`), and a breakpoint at `T+0x5267740` (the test
of the stream count, after the lock). The first time it finds 0 streams,
the breakpoint prints the stack, rewrites the displacement of the jump at
`T+0x5267747` so that it leads to the unlocking exit `T+0x5267c7f` instead
of the plain return, and `cdb` detaches. The rest of the run has no
debugger. That exit only pops the same frame and unlocks: the call does
what the title does with no stream, minus the leak. A second, non-invasive
`cdb` reads the state at +269 s.

At the leak (before +8 s; the log's "audio DSP connected" is at +7 s):

- thread 0x1a50, a job worker: `T+0x2215eca` → `T+0x221c29c` (the worker's
  loop) → `T+0x537f832` (the job) → `T+0x6d9159` (its virtual call) →
  `T+0x52673af` (the method) → `T+0x5267740`;
- the device's mutex `00001a50_00000001`: just taken by that worker, nobody
  waiting yet; the device's stream count 0.

At +269 s, beside S7's values at +170 s (no redirect):

| read | S8 | S7 |
|---|---|---|
| the device's mutex | `ffffffff_00000000`: free | `00000e88_00000003`: an idle worker, 2 waiters |
| its stream count (`+0x52c`) | 0 | (not read) |
| endpoints counted, output open (`+0x3dd80`, `+0x3dd84`) | 0, 0 | (not read) |
| the streaming worker's tokens (`T+0x75e0e78`) | `00000000_0000007f`: 127 of 127 free | `ffffffff_00000005`: none, one waiting |
| the device loop, the audio update loop | sleeping between passes (`T+0x43addeb`, `T+0x43ae2ad` ← `T+0x1e95177`), as on the PC | waiting on the mutex |
| the reopening thread | in its retry loop (`T+0x1da87a9`, 100 ms sleeps): the open keeps failing | waiting for a device change |
| the streaming worker | not waiting | waiting for a token |

And the run:

| | S8 (lock released) | S4 to S7 |
|---|---|---|
| `story000.rpf` opened | +34 s | about +30 s |
| string tables `00048`, `00052`, `00051` | +73 s | never |
| autopilot world condition (`0xab43664` = 0, `0x9e58138` = 0, `0x74dfa9d` = 1) | met at +98 s (frame 2563); control handed back | never (`0xab43664` stays 9) |
| first audio coprocessor commands 5, 18 and 7 (the world's sounds; at +38 s on the PC) | +98 s | never |
| GPU memory | 2.8 GB until about +80 s, then up to 7.2 GB at +109 s, 6.4 to 6.7 GB after | 2.6 GB to the end |
| draws a frame | 980 to 1 924 | 8 or 9 |

So the 127 jobs that held the streaming worker's tokens in S7 were waiting,
directly or not, on the audio device. With the lock released, nothing else
stops the load. The world comes 64 s after `story000.rpf`, against 13 s on
the PC, and runs at 2 to 3 fps on the L4 (320 to 460 ms a frame, a quarter to
40 % of it in copies back to the CPU, 730 to 930 pipelines still being
made). That is the L4's next problem, not this one. The last capture is
frame 2048, still the loading screen: at 2 to 3 fps the run ended before
frame 4096.

### 6.7 Reading the stalled process again

The addresses (build 1.0.2.1; `T` is the title module as `cdb` names it,
`xtp_image_b99f876b74d4a77f`):

| What | Where | Value in S7 |
|---|---|---|
| the audio output ("manager" above) | `poi(T+0xa924088)` | `0x90927bf80` |
| its device's mutex | output + `0x3ced0` (owner TID in bits 32-63, recursion count at +0x10) | `00000e88_00000003`, recursion 1 |
| the device's output streams | output + `0x3ced0` + `0x52c` (`u32`) | not read (0 in S8) |
| endpoints counted, output open | output + `0x3dd80` (`u32`), output + `0x3dd84` (`u8`) | not read (0, 0 in S8) |
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
mutex's upper word.

S8's first file, for an invasive attach (`cdb -p <pid> -cf …`, no `-pv`)
made as soon as the run's `xtp.exe` exists: every exception goes to XTP's
handlers (`clzero`, guard pages), and at the first call with no stream the
breakpoint prints the stack and redirects the jump. With no `gc` at the
end of that branch `cdb` stops there, reads the next line, `qd`, and
detaches:

```text
sxi ii
sxi av
sxi gp
sxi eh
sxi ch
sxi dz
sxi out
sxi sov
sxi *
bu xtp_image_b99f876b74d4a77f+0x5267740 ".if (dwo(@rdi+0x52c) == 0) { .printf \"@@LEAK tid %x dev %p\\n\", @$tid, @rdi; dq @rdi L4; kn 40; ed xtp_image_b99f876b74d4a77f+0x5267749 0x532; bc * } .else { gc }"
g
qd
```

The value written is the jump's new displacement (`T+0x5267c7f` minus the
end of the jump, `T+0x526774d`). The script that waits for the process,
attaches and reads the state at +269 s (with the reads of the table above)
is `E:\xtp-data\tmp\l4lock\vm\l4lock-watch.ps1`. It is a diagnostic, kept
outside the repository. If `cdb` cannot attach, it writes the same
displacement itself with `WriteProcessMemory`.

## 7. What it is not

| Lead (from the task) | Verdict |
|---|---|
| A CPU difference | Half of it. `clzero` emulation (stall A) is Intel-only and fixed. Stall B is not a CPU matter: S8 reaches the world on the same Xeon once the audio lock is released. The title starts the same 49 workers on 16 and on 32 logical processors. |
| DirectStorage requests that never complete | No: every request completed, none failed, none waited (`waiting=0`). After the `story000` files the title stops asking for data. |
| The disk (read-only, sector size, alignment, unbuffered reads) | No: the VM's `D:` has 512-byte logical sectors (4 096 physical), as an NVMe drive. The front end reads 5 000 to 6 000 requests per 5 s and no read failed. |
| A race hidden by the PC's speed | No. Stall B looked like one (the same waits clear within a second on the PC), but it is a lock the title never releases, taken on the first call of a per-frame audio job when there is no audio output (§6.4). It does not depend on pace. Nor is it the race between `watch::guard` and `kernel_will_write` that the archive task found (`archive-null.md`): no `0x800706f8`, 998 or 1784, and no thread retried a read. |
| The audio output | **Yes: stall B.** The VM has no render endpoint, the title's open of its output fails, and `T+0x5267720` keeps the device's lock (§6.3 to §6.6). |
| The golden save copy | No: the save loads and the title opens the first mission's packs, as on the PC. |
| The L4 (vWS, 22 GB budget, no Resizable BAR, driver 596.86, VidMm) | Not involved: GPU memory stays at 2.6 GB and residency was never refused. The budget is not passed to the title. The one device warning at start-up (a view heap capped at 1 000 000 descriptors) appears on the PC too. |
| The FidelityFX DLL | Ruled out by S1, which has the HLE. |

## 8. Still open

### 8.1 Next step: release the lock, or give the title an audio output

S8 released the lock with a debugger. For every VM run to reach the world,
one of these has to land. They are not in this task's files, so they are
follow-ups:

1. **XTP hook, per build (cheapest; recommended first).** In
   `crates/xtp-game` (beside `intro.rs`, which locates its sites by RVA for
   build 1.0.2.1), redirect the plain return of `T+0x5267720` to its
   unlocking exit: the 32-bit displacement of the jump at `T+0x5267747`
   becomes `T+0x5267c7f - T+0x526774d`. It is written where the intro cut
   is installed (`apps/xtp/src/run.rs`, `xtp_game::intro::apply_if_asked`:
   imports bound, no title code run yet). Unlike that cut it writes code,
   so it lifts and restores the page's protection as `tls::redirect` does
   (`crates/xtp-loader/src/tls.rs`). It refuses unless the build is 1.0.2.1
   and the site still holds a near `je` to `T+0x5267cfb`. On by default: it
   only changes the path the title takes with no output stream, where the
   call does nothing but leak the lock; a PC whose output opens does not
   reach it. A switch to turn it off gives an A/B. Synthetic tests: the
   displacement written into a buffer laid out like the site, and a refusal
   on another build or another instruction there. Check: one
   `run-remote.ps1 -Game` on the L4, which should meet the autopilot's world
   condition as S8 did.
2. **A silent audio output in XTP (faithful, larger).** When the host has
   no active render endpoint, XTP shows the title one: a wrapper around
   `CoCreateInstance(CLSID_MMDeviceEnumerator)` whose endpoint activates an
   `IAudioClient` and an `ISpatialAudioClient` that accept the title's
   streams and drain them at the real-time rate. The console always has an
   output, so this is what it would see. It also stops the reopening thread
   retrying every 100 ms (§6.6), and would let any title run on a host with
   no sound. It is far more work: the title uses spatial audio (a format
   enumerator, an object render stream), not only `IAudioClient`.
3. **A render endpoint on the VM (no XTP code).** A virtual audio driver
   on the VM gives the title a device. That is a third-party driver on the
   person's machine, their decision, and it only helps that machine.

Option 1 unblocks the L4 at once. Option 2 is the lasting one.

What came of them (2026-11-25):

**Step 0, a stopgap on the VM only (in place).** Option 1 done from the VM,
not in XTP: `tools\cloud\run-remote.ps1 -Game -AudioLockStopgap` (with
`-Game`'s other options, e.g. `-Autopilot autopilot\gameplay.txt -Seconds
300`). `vm\run-game.ps1` starts `vm\audio-lock-stopgap.ps1` just before the
title. It attaches cdb (`C:\xtp\jobs\cdb\cdb.exe`, §6.7) to the run's
`xtp.exe` as it starts, with every exception left to XTP; at the first call
of `T+0x5267720` (the image is mapped, its `je` has not run yet) it rewrites
the displacement at `T+0x5267749` from `0x5ae` to `0x532` if the bytes there
are the expected ones, and detaches. In memory only, build 1.0.2.1 only
(read from the title's `MicrosoftGame.Config`; the RVAs live in that script
only). Without cdb it writes the displacement itself, later. Its report,
`audio-lock-stopgap.txt` (last line `STOPGAP ...`) and cdb's output
`audio-lock-cdb.txt`, come back with the run's logs, and the summary prints
it; nothing of it runs after the run. S9 (`37a35f1`, `-Seconds 300`,
autopilot `gameplay.txt`): patched at +5.4 s; the autopilot's world
condition met at +88.4 s (frame 2756), control handed back; 2 to 3 fps in
the world, as in S8. It goes once XTP has option 2.

**Option 2, a silent audio output: the plan for its task.** A draft is on
the local branch `nullaudio-option2` (`b3822b4`, not built nor reviewed):
`crates/xtp-hle/src/modules/audio/null.rs` and its tests.

1. *What the title calls* (read in its code, build 1.0.2.1). Its output's
   open (`T+0x43aea20`): `CoCreateInstance(CLSID_MMDeviceEnumerator, NULL,
   CLSCTX_ALL, IID_IMMDeviceEnumerator)` through its import from
   `api-ms-win-core-com-l1-1-0.dll`; the enumerator's
   `RegisterEndpointNotificationCallback` (slot 6), `EnumAudioEndpoints(eRender,
   DEVICE_STATE_ACTIVE)` (3) and the collection's `GetCount` (3); with more
   than one endpoint, for each, `Item` (4), `OpenPropertyStore(STGM_READ)`
   (4) and `GetValue(PKEY_AudioEndpoint_FormFactor)` (5), headphones or
   headset noted; `GetDefaultAudioEndpoint(eRender, eConsole)` (4), the
   device's `GetId` (5). Its streams (`T+0x1e7f580`): `Activate(IAudioClient)`
   (device slot 3) and only `GetMixFormat` (8), for the channel count;
   `Activate(ISpatialAudioClient)`; `GetSupportedAudioObjectFormatEnumerator`
   (6), its `GetCount` (3, 0 fails) and `GetFormat(0)` (4);
   `IsAudioObjectFormatSupported` (8); an event (`CreateEventA`);
   `ActivateSpatialAudioStream` (10) with a `VT_BLOB` of the 0x28-byte
   activation parameters (that format, the static channels its
   configuration asks for, no dynamic object, category 6, game effects, the
   event), whose result must be exactly `S_OK` (`T+0x1e7fba6` tests `jne`);
   then, per static channel, `GetStaticObjectPosition` (3, in
   `T+0x5f0bd40`). The update passes on the stream
   (`BeginUpdatingAudioObjects`, the objects' `GetBuffer`,
   `EndUpdatingAudioObjects`) are deeper in the device loop and its jobs;
   a run lists them. The image holds no other audio interface id:
   `IAudioRenderClient` and `IAudioClock` are not used.
2. *What to build.* `CoCreateInstance` served by the audio module for
   `api-ms-win-core-com-*` (and `ole32.dll`, for lookups by name), the host's
   for every other class. For the device enumerator: the host's, untouched,
   when it has an active render endpoint (`EnumAudioEndpoints(eRender,
   ACTIVE)` counts one or more); otherwise, or if the host's fails, ours,
   which lists one render endpoint, a null device: `IMMDevice` (id, state,
   a property store with the form factor, speakers, and a name);
   `IAudioClient` 1 to 3 that take any PCM or float format, drain what
   `IAudioRenderClient` is given in real time, signal their event every
   10 ms and advance `IAudioClock`; `ISpatialAudioClient` with one object
   format (48 kHz mono float), every static channel and a few dynamic
   objects, whose object render stream signals its event every 10 ms and
   hands out one period's buffer per object per pass (480 frames);
   `SetEndOfStream` frees the object's channel after the pass. Nothing is
   played. `XTP_AUDIO_NULL=1` gives the null device whatever the host has,
   `=0` never. Each method logs its first call. No new `windows-sys`
   features: hand-written method tables, as `media.rs`, and the host's
   `CoTaskMemAlloc` and `QueryPerformanceCounter` looked up by name.
3. *Tests* (Windows, in `xtp-hle`, named so that `cargo test -p xtp-hle
   audio` runs them): the enumeration as the title makes it (one render
   endpoint, none for capture, id, form factor); an `IAudioClient` on a
   format of the title's own (initialize, event, buffer and clock calls,
   padding falling and position rising in real time, running dry, stop and
   reset); the spatial stream opened with the title's parameters (exactly
   `S_OK`), an object per static channel, update passes on the event;
   `XTP_AUDIO_NULL`'s decision; and through the export, a render endpoint on
   any host, other classes left to the host.
4. *Acceptance*: as this task's: on the PC with `XTP_AUDIO_NULL=1`, a
   Series S session reaches the world with no change in fps; on the L4
   (`run-remote.ps1 -Game`, without `-AudioLockStopgap`) the autopilot's
   world condition is met with no patch to the title's code; then step 0's
   switch and script can go.

**Option 3 was not needed** and nothing was installed. It would take a
render endpoint driver on the VM image: a signed third-party virtual device
(a virtual audio cable, installed by its own setup as an administrator,
which adds an active render endpoint), or Microsoft's SysVAD sample from the
driver kit, which is not signed for release and needs test signing on the VM
(`bcdedit /set testsigning on`, Secure Boot off) and a reboot. Both only help
that VM, and both are the person's decision.

### 8.2 What XTP does differently from the PC that loads

The difference is the host's, passed through as it is: the VM has no audio
output (§6.5). The other leads of the task, against what S8 showed:

| Lead | Today | Verdict |
|---|---|---|
| Thread priorities (VIctory #1505 took `THREAD_PRIORITY_BELOW_NORMAL` off our pipeline workers) | The title sets its own (`SetThreadPriority`, the host's): 6 threads at 15, 13 at 8, down to 6 at 1, alike on both machines. XTP's threads run at normal priority. | Not involved: the lock is never released, whatever the scheduling, and S8 loads with the same priorities. |
| Affinity, and the cores the title sees | `SetThreadGroupAffinity` and `GetLogicalProcessorInformation` are the host's: 8 cores and 16 threads on the VM, 16 and 32 on the PC. The title starts the same 49 workers. | Not involved: S8 loads with the same 16 logical processors. |
| HLE sync calls (events, semaphores, waits with time-outs, wait-on-address, SRW, critical sections) | They go straight to the host (`CreateEventW`, `CreateSemaphore(Ex)W`, `WaitForSingleObject(Ex)`, `EnterCriticalSection`, `AcquireSRWLockExclusive`). The title's mutex and wait-on-address are its own code over host events. | Not involved: the mutex works as written. It is never unlocked, because the title returns without unlocking. |
| Async I/O completions (DirectStorage, file reads) | Every DirectStorage request completes (`waiting=0`), no read fails. | Not involved: S8's reads are the same, and the world comes. |
| Job continuations on a thread pool XTP runs with fewer threads | The title runs its own job system on its own 49 threads. It imports no fibre functions and no thread-pool work items. XTP adds no pool under it. | Not involved: the lock is taken and kept by one job on the title's own worker (§6.6). |
| Timers, GPU pace | 1 ms timers. The L4 is 3 to 7 times slower per frame than the 4080 SUPER on the loading screen. | Not involved in the stall: the leak comes at the first call, within 8 s. The pace only sets how fast the world runs once loaded (§6.6). |

The orchestrator reported the same signature on the person's PC with
slower integration builds (the world-black task, `c19d640b`). This leak
needs the output's open to fail: no active render endpoint, or a device
that refuses one of the title's steps (its spatial audio client, its
streams). If that PC stalled the same way, either its audio output was
missing or refused at the time (a headset or monitor switched off, a
disabled device), or the stall was another one.
Two reads in the stalled process tell (§6.7): the device's stream count
(0 here) and the owner of its mutex (an idle job worker here).

### 8.3 Other open points

- **The world on the L4 runs at 2 to 3 fps** (S8: 320 to 460 ms a frame,
  much of it in copies back to the CPU, 730 to 930 pipelines being made).
  The L4 reaches the world, but it is not yet a place to measure it.
- **One exception per `clzero` loop remains** on CPUs without it. Patching
  the recognised loops when the image is mapped would remove even that.
  After the fix it costs about 6 ms per 5 s on the VM, so it is not
  urgent.
- **The VM's GPU frame** on the loading screen takes 10 to 11 ms against
  1.5 to 3 ms on the PC. That is expected of an L4 against a 4080 SUPER.

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
- Stall B's object (§6.3, §6.4): the instructions whose displacement is
  `0x3ced0`, functions bounded by the image's `.pdata` (chained unwind
  entries followed to the function's start), callers found by their
  `call`/`jmp` displacements, imported names read from the import slots
  (the prepared copy keeps the original thunk values), interface ids read
  from the image, and the vtable found by the method's address in `.rdata`.
  Python scripts in `E:\xtp-data\tmp\l4lock\` (`pdata.py`, `calls.py`,
  `imp2.py`, `riprefs.py`, `lockscan.py`).
- The VM's audio (§6.5): `Get-Service Audiosrv, AudioEndpointBuilder`,
  `Get-PnpDevice -Class AudioEndpoint`, `Get-CimInstance
  Win32_SoundDevice` and the `MMDevices\Audio\Render` registry key, through
  `tools\cloud\invoke-vm.ps1 -Command`.
- S8 (§6.6): an invasive `cdb` attach from the SSH session, made by a
  script that polls for the run's `xtp.exe` every 200 ms. With every
  exception set to `sxi`, XTP's vectored handlers (`clzero`, guard pages)
  run as without a debugger, only slower. The debugger detached at +8 s, so
  the rest of the run had none.
