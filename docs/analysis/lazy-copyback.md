# Lazy copy-back: what the CPU touches of what the GPU writes

Task 8c035076. The idea (a census of the CPU's first touch, then a copy-back
on first touch) is xcl issue #1640's. xcl is GPL-2.0-only: none of its code is
taken, and the census here is XTP's own.

## Why

Shaders write the host copies of title memory (`crates/xtp-gpu/src/shadow.rs`).
Once a submission has run, `Shadow::copy_back` puts what they changed back
into title memory. It compares each range the submission declared as
shader-written with that range's snapshot from before the submission. Where
a piece differs, it reads title memory and writes back only the GPU's bytes
(the three-way merge from simnJS/AnyPS5, 65a1b01db4).

In the Series S world, that costs 75–108 ms of a 260–333 ms frame (copyback's
sessions B2 and B3, task 78d58a42). The copystat line of session
copyback-B2-main shows where the time goes. Per frame, the copy-back:

- compares 637–641 MB;
- reads 3.7–7.7 MB of title memory in 65–85 reads (the pieces the GPU
  changed);
- writes 190 000–263 000 runs.

So the GPU changes about 1 % of what is compared. The cost is the
comparison, not the writing. A copy-back on first touch saves time only if
it also leaves the comparison alone: the whole declared range is protected
when the submission finishes, and a page is compared only when the CPU first
touches it. Whether that pays depends on how much of the declared ranges the
CPU touches before the GPU writes them again. The census measures that.

## The census (`XTP_COPYBACK_CENSUS`, diagnostics builds)

`shadow::census`. It is off unless `XTP_COPYBACK_CENSUS` is set, in a build
with `--features diagnostics`.

1. After each copy-back (`copy_back`, and `write_back` for the GPU's buffer
   copies), every 4 KiB page of its range gets a guard (`PAGE_GUARD`).
   `census::after_back` notes each 64 KiB block before guarding it. It
   keeps the protection under the guard as it was, and puts a block back as
   it was if that protection is not the expected one.
2. The first access to a guarded page faults once into the copy's vectored
   handler (`stopped`). The handler counts the access and lets it go on,
   because the system has already taken the guard off. It counts:
   - a read or a write by code other than XTP's (the title's, or the system
     code it calls);
   - an access by XTP's own code (its plain copies, its texture hashing).

   It also keeps the 32 instruction addresses that touched most.
3. A store to a page that is read-only under its guard faults on the
   protection first, and the guard stays. The handler counts that store in
   `census::settle`. The guard of view memory then opens the whole block,
   and its other guards go with it. The rest of that block counts as
   "uncounted after a write".
4. The next copy-back over a block (`census::before_back`) sorts its pages
   that no access reached:
   - written again by the GPU first, when the journal (`watch::writes`)
     shows no write since the guard;
   - written unseen, when the journal shows a write but no store faulted
     there (a kernel or runtime write, or a write through another view);
   - uncounted after a write, when a store to the block faulted (the
     journal knows blocks, not pages).
5. The census asks the journal once before the copy-back's own writes and
   once after them. A write found after them is someone else's, made while
   the copy-back ran, unless the copy-back wrote into that block too.

The frame line (`copystat`) gains a census line per frame. It gives the
bytes guarded, the bytes the GPU wrote again before any touch, and the bytes
touched first (reads, writes, XTP's own), also counted in whole 64 KiB
blocks. It also gives the bytes written unseen or uncounted after a write,
the GPU's bytes written back (touched or written again first), the faults,
the protection changes and the census's own time. Then it lists where the
touches came from, as module+offset. An address in no module the system
knows is most likely in the title's image, which XTP maps itself.

### Which memory gets guards

- `XTP_COPYBACK_CENSUS=1`: only view blocks (physical pages) that the guard
  of view memory (`watch.rs`) keeps read-only. The runtime calls
  `kernel_will_write` (`watch::writing`) before every kernel write into title
  memory (`ReadFile`, `fread`, DirectStorage's raw reads) and after its own
  writes into physical pages. That call opens such blocks, and its
  protection change takes the census's guard off too, so no system call
  fails on a census guard. Read-write memory is left alone: other memory
  (write-watched), and view blocks written since they were guarded. It
  counts as "not guarded".
- `XTP_COPYBACK_CENSUS=all`: read-write memory gets guards too. There, a
  kernel write fails once on the guard. The system takes the guard off
  (`the_census_guards_write_watched_memory_with_all_only` checks it with
  `WriteProcessMemory`), so BTP 6's synchronous reads, which retry, get
  through. A DirectStorage read or an `fread` into such a page would fail,
  though.

### What it does not see

- A kernel read of a guarded page (`ReadProcessMemory`, used by the title
  copies and some texture reads in `host.rs`) fails on the guard and takes
  it off without a fault. The page then counts as not touched.
  `copy_mapped` then reads again piece by piece (a "fallback" on the
  copystat line). Its 4 KiB pass meets each remaining guard once more, and
  gives zeros there. A failed `read_into` (a texture row copied on the CPU)
  gives zeros for that row. So a census session feeds the GPU some zeros:
  see the results.
- Guards that the guard of view memory takes off by itself go unnoticed:
  when it guards the other views of a page (#1122), and when it remaps.
- Pages touched while the copy-back over them runs are not counted, and are
  guarded again after it.

### The copy's handler, changed

A plain copy (`try_copy`) that meets a census guard now goes on through it.
Before, a guard fault at the copy's instruction ended the copy, as any fault
did: zeros in the host copy for that 4 KiB page.

### Tests

- `the_census_counts_the_first_touch_of_what_came_back` (view memory, in a
  process of its own): every page guarded; a read faults once and sees the
  GPU's bytes; a store to a read-only block leaves the rest of the block
  uncounted; the next copy-back counts the untouched pages as written again
  first; XTP's own copy reads through the guards; without a census, nothing
  is guarded.
- `the_census_guards_write_watched_memory_with_all_only`: write-watched
  memory is left alone with `1` and guarded with `all`. There, a store opens
  one page only, and a kernel write shows as written unseen.

## Results: session lazyback-census (Series S, 2026-11-24)

The setup:
- branch a2533dc: main a2b07a9 merged, plus the census;
- a diagnostics build, Series S, `XTP_SKIP_INTRO=1`, `XTP_WATCHDOG_HOOK=1`,
  `XTP_COPYBACK_CENSUS=1`;
- `session.ps1 -Name lazyback-census -Measure 120 -Limit 200`, from 22:09:23
  to 22:11:54 local time;
- 120 s of gameplay, the title exited with 0, no device removal.

The run folder is `E:\xtp-data\runs\stall-lazyback-census`. The numbers below
are per frame, over the 19 frame lines in the world (median, lowest,
highest). All memory sizes are in MB.

| | median | lowest | highest |
|---|---|---|---|
| fps | 3.5 | 1.7 | 4.7 |
| ms per frame | 283 | 214 | 585 |
| ms copying back (census included) | 77.6 | 53.7 | 163.5 |
| copy-back compared | 638 | 372 | 656 |
| census: guarded | 485 | 239 | 493 |
| census: not guarded (read-write memory) | 154 | 118 | 170 |
| written again by the GPU before any touch | 460 | 233 | 469 |
| touched first, 4 KiB pages | 12.5 | 3.0 | 13.2 |
| … of them reads (code other than XTP's) | 12.0 | 2.7 | 12.6 |
| … writes | 0.33 | 0.09 | 0.44 |
| … XTP's own accesses | 0.17 | 0.09 | 0.42 |
| touched, counted in whole 64 KiB blocks | 24.3 | 5.8 | 27.0 |
| written unseen | 0.50 | 0.07 | 1.01 |
| uncounted after a write | 10.4 | 2.5 | 12.9 |
| GPU bytes written back into guarded pages | 0.15 | 0.01 | 1.07 |
| census faults (a count) | 3 495 | 1 330 | 3 810 |
| census protection changes (a count) | 401 | 132 | 438 |
| census time, ms | 14.1 | 6.2 | 22.3 |

Over all 19 lines:

- **Of the declared ranges guarded, 95.2 % were written again by the GPU
  before any touch.** This counts every touch, unseen write and page left
  uncounted after a write as touched: 396 MB touched against 7 910 MB
  written again first.
- **Of the GPU's bytes written back into guarded pages, 96.0 % were written
  again by the GPU before any touch** (5.18 of 5.35 MB). That is xcl #1640's
  96 %.

Where the touches came from (instruction addresses, faults per frame summed
over the lines):
- almost all from one instruction in the title's image, at RVA 0x4e4b680:
  about 2 400 faults a frame, reads;
- a few hundred a frame from about ten other title sites, from XTP
  (`xtp.exe+0x23175b`), and from the C runtime's copies (`VCRUNTIME140`).

The title reads its pages of the big GPU heap through one routine. The GPU
bytes in the pages it touches are few (0.01 MB a frame): what it reads lies
in the declared ranges, but not in what the GPU changed.

### What biases these numbers

- **Kernel reads.** The title copies in `host.rs` (`copy_mapped`, through
  `ReadProcessMemory`) read guarded pages:
  - the copystat line shows 2 200–11 500 "fallbacks" a frame, against 0 in
    copyback's sessions without the census;
  - each such read fails on the guard, and `copy_mapped` reads again piece
    by piece;
  - its 4 KiB pass meets each remaining guard once more: 1–5 MB a frame of
    "unmapped" zeros went to the GPU during this session (only with the
    census);
  - the census does not see these reads.

  They cover at most the title copies' volume, 2.3–7.4 MB a frame. Counted
  as touched, the fraction written again before any touch is still ≥ 93.6 %.
- **Read-write memory.** 154 MB a frame (24 % of what is compared) got no
  guards. Write-watched memory and view blocks written since they were
  guarded are left alone without `all`. `all` was not run: a DirectStorage
  read cut by a guard is what made the Archive allocation fail
  (`archive-null.md`).
- **The census's own cost.** The census takes 14 ms a frame. Its two
  journal askings per copy-back show in the guard's time: 15–39 ms a frame,
  against 2–10 ms in copyback-B2-main. So this session's frame times are
  not a "before".

### What it means for a copy-back on first touch

On the guarded 76 % of the copy-back, a copy-back on first touch by 64 KiB
block would compare 24–31 MB a frame instead of 485 MB (the touched blocks,
plus the title copies' reads). That is about 380–480 faults a frame at 64 KiB
(about 3 000–3 800 at 4 KiB), at roughly 2–5 µs each, plus a 64 KiB
comparison each. Estimate: the copy-back's 75–108 ms falls to a few ms, for
the memory measured.

It needs more than `shadow.rs`:

1. `watch::writing` (`kernel_will_write`): bring a block "GPU-newer" back
   before a kernel write into it. A `ReadFile` into a no-access page fails,
   and BTP 6 retries it for ever.
2. `host::read` and `host::write_mapped` (`host.rs`): the census shows
   2–7 MB a frame of title copies reading these ranges through the kernel,
   which fails on a no-access page.
3. `Staging::refresh_now` (`texture.rs`): a texture read from title memory
   must see the GPU's deferred bytes.
4. View memory, where the census found its 95 %:
   - `watch.rs`'s guard puts `PAGE_READONLY` back over a no-access block at
     each asking, so it must leave GPU-newer blocks alone;
   - the other views of a page (#1122) must be protected with it;
   - the runtime's own writes into physical pages (DirectStorage's page
     destinations) are only reported after they are made. Such a write into
     a GPU-newer block can only be merged: where it wrote the very value
     the snapshot holds, the GPU's byte would go over it.
5. A first touch from two threads: the block is made accessible, then
   written. A thread that reads it in between, without a fault of its own,
   sees the older bytes. The eager copy-back has no such window: it finishes
   before the title is told the GPU is done.

### Another way: copies in the card's memory, compared on the card

fxceless's VIctory fork (`578d459`, `shadow.rs` `cards` and `card_limits`;
the private fork study, pick 11) keeps copies of big title buffers
in the card's memory, and finds what shaders wrote by hashing on the card.
The fork measures one skinning list going from 50.5 to 4.5 ms on the card.
XTP's copies are system memory (`host/buffers.rs`: a custom heap,
write-back, L0).

Against a copy-back on first touch:

- It removes the 640 MB comparison from the CPU altogether. Hashing 640 MB
  of the card's memory takes a few ms of GPU time. Only the pieces that
  changed come back (0.4–6.8 MB a frame in these sessions).
- It speeds up the GPU passes that use these buffers: they no longer go
  through PCIe.
- It keeps the eager copy-back's semantics: none of the hooks above, no
  page protections, no fault handler, no first-touch window.
- Its costs:
  - the card's memory: it fits in Series S's budget;
  - refreshes become GPU copies from an upload heap;
  - the changed pieces must be read back;
  - the snapshot the three-way merge needs must come from the card;
  - it changes `host.rs` and `host/buffers.rs` as well as `shadow.rs`.
- Once only changed pieces are compared, a copy-back on first touch would
  save little: writing back the 0.4–6.8 MB the GPU changed.

None of the fork's code is taken here. The fork study marks this pick
"reimplement": the idea only.

## Snapshots that do not fault (30fps.md N1, first half)

`docs/analysis/30fps.md` (§4.1) measured the snapshots that the three-way
merge needs (`Shadow::writing`) at 76.5 ms a frame, with about 74 000 page
faults. Two things in the old pool caused this:

- it kept at most 16 buffers and freed the rest, so big snapshots landed in
  fresh memory;
- each range that overlapped a piece already taken merged both into a new
  buffer, which copied the piece again.

Now a submission's snapshot of a host copy lives in a **mirror** of the
copy's size:

- each byte sits at its offset in the copy;
- a range copies only what the snapshot has not taken yet, each byte once;
- the mirror is kept for the copy's next snapshots, so its pages are touched
  already;
- two submissions in flight on one copy each have their own mirror.

The copystat line prints the snapshots: their count, MB, ms, and the
mirrors made and held.

`XTP_SNAPSHOT_MIRRORS=0` gives each snapshot fresh memory, to time the
difference. `XTP_SNAPSHOT_MIRRORS=ab` alternates the two within one run,
switching every 3 copystat lines; the first line of each phase is the
settling time and is left out.

Session lazyback-mirrors-ab: Series S, `XTP_FRAME_LINE=2`, 60 s of world,
the title exited with 0, on 2026-11-24. Medians per frame:

| | mirrors kept (6 lines) | fresh memory each time (8 lines) |
|---|---:|---:|
| snapshots, ms | **41.9** | 112.7 |
| snapshots, MB copied | 618 | 555 |
| page faults (the process) | **69 900** | 228 700 |
| mirrors made | 0.1 | 369 |
| preparing, ms | 70.5 | 173.2 |
| ms per frame | 249 | 352 |
| copy-back, ms | 81.2 | 109.2 |

The fresh mode costs more than the old pool did: it takes memory of the
copy's size each time, not of the piece's. So this A/B overstates the gain.

Against `main` before the change, from fps30's session B:
- snapshots: 76.5 → 41.9 ms a frame (fps30's timers against the copystat
  line);
- page faults: about 118 000 → 70 000 a frame (each session's copystat
  line).

That is about −35 ms, close to the −40 ms the plan expected. The 42 ms left
is the copy itself, about 618 MB a frame, at memory speed. Only finding the
GPU's bytes without a full snapshot (N1's second half) removes it, and the
copy-back's comparison with it.

## Writing back through the kernel (VIctory #1215)

VIctory #1215 writes the copy-back with `NtWriteVirtualMemory` instead of
`WriteProcessMemory`. It measures the copy-back falling from 1 713 to 85 ms
a frame. XTP's default path calls neither:

- Since W1, `write_title` writes with plain stores (`try_copy`). It opens
  the guard of view memory first (`watch::writing`).
- A store into a block that a refresh guarded again meanwhile faults into
  the copy's handler, which gives it to the guard and goes on (copyback,
  task 78d58a42).
- `host::write_mapped` (`WriteProcessMemory`) remains only in the kernel
  mode that diagnostics time (`XTP_SHADOW_ONCE=0`, `XTP_COPY_MODE=kernel`)
  and in `watch.rs`'s tests.

A system call into a block that the guard keeps read-only fails, whichever
call it is: `NtWriteVirtualMemory` would lose the GPU's bytes there without
a word, as `WriteProcessMemory` did before W1. XTP's copy-back time today is
the comparison (about 638 MB a frame), not the writes. #1215 is not needed.

## How to run it

A diagnostics build of the task branch, a Series S session under
`measure.lock`, announced on the task:

```
E:\tools\stall-probe\session.ps1 -Name lazyback-census -Measure 120 -Limit 240 `
  -Worktree <this worktree> -EnvPairs "XTP_WATCHDOG_HOOK=1;XTP_COPYBACK_CENSUS=1"
```

(`session.ps1` sets `XTP_SKIP_INTRO=1` and Series S itself.)
