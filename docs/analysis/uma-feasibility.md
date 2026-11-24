# GPU-visible title memory on the RTX 4080: feasibility (G15)

Measured for `docs/analysis/gpu.md` phase 3 (G15–G18). The
question: can the title memory that `xtp-hle`'s xmem allocates be opened as
D3D12 heaps that shaders and copies use in place, so that a world frame stops
copying 220–285 MB into shadow buffers and back? And at what cost in commit,
residency, bandwidth and latency?

**Answer for NVIDIA: yes, for both kinds of title memory, under three
conditions.**
- Heaps hold at most 4 GiB − 64 KiB.
- They sit on committed memory.
- The title must not unmap or decommit that memory while a heap is open.

A newer call, `ID3D12Device13::OpenExistingHeapFromAddress1`, works on this
machine. It removes the "whole allocation, fully committed" rule that shaped
xcl's four 4 GiB windows. One trap was found the hard way: shader stores of
16 bytes into imported memory crawl at 0.1 GB/s, and 4 GiB of them hung the
GPU (section 5).

- Test: `crates/xtp-gpu/tests/uma_feasibility.rs`. It uses the `windows`
  crates only, nothing of `xtp-gpu`.
  - The quick tests run in the workspace check on every adapter present
    (the high-performance one and WARP) and skip without D3D12: the import
    matrix, and what an open heap does to unmapping and decommitting.
    - On this machine the three take 1.4 s in all: 0.24 s to create the
      RTX 4080's device, then a 210 ms matrix, and 0.84 s for WARP's.
    - Their GPU work is a few dispatches over 4 MiB at most.
  - The benchmarks are `#[ignore]`d. They load the GPU for minutes, so never
    run them on a machine someone is using:
    `cargo test -p xtp-gpu --release --test uma_feasibility -- --ignored --nocapture --test-threads=1`
    (`XTP_UMA_ADAPTER=warp` or `=nvidia` keeps one adapter).
- xcl (GPL-2.0-only) was read for behaviour only: which calls it makes, and
  what its `HANDOFF.md` says it measured on an RX 6700 XT. No code was taken.

## 1. Machine

| | |
|---|---|
| GPU | NVIDIA GeForce RTX 4080 SUPER, 16 GiB, driver 621.01 (UMD 32.0.16.2101) |
| Link | PCIe 4.0 x16, resizable BAR on (BAR1 16 GiB) |
| D3D12 | Windows 11 Pro build 26300, inbox runtime: existing heaps, `ID3D12Device13`, GPU upload heaps all present; not UMA |
| Budgets (DXGI) | 15,277 MiB local, 47,237 MiB non-local |
| CPU, RAM | Ryzen 9 7950X3D, 64 GiB |
| Comparison | WARP 10.0.26100.9818 (UMA, cache-coherent) |

Other programs held about 11 GiB of video memory during the runs. The machine
was in use, so expect noise of ±10–20 % on single numbers.

## 2. Summary

1. **Virtual memory imports.**
   - What imports: xmem's `XMemVirtualAlloc` reservations
     (`VirtualAlloc2` between 4 GiB and 2^40, 2 MiB-aligned, write-watched).
   - How: with `OpenExistingHeapFromAddress` when the whole allocation is
     committed; with `OpenExistingHeapFromAddress1` over any committed range.
   - The CPU and the GPU see each other's writes without any flush.
   - Write watch still reports the CPU's writes and ignores the GPU's.
2. **Physical pages import too.** The risk listed in `gpu.md` section 8 does
   not hold on NVIDIA or WARP.
   - xmem's views of `SEC_RESERVE` sections open by address, both as
     `XMemMapPhysicalPages` makes them (in placeholders) and as plain views.
   - The GPU then writes the section's pages: a second view of the same
     pages sees the writes.
   - `OpenExistingHeapFromFileMapping` refuses `SEC_RESERVE` sections and
     takes `SEC_COMMIT` ones, as xcl found.
3. **Limits.**
   - At most 4 GiB − 64 KiB per heap, with every call.
   - `OpenExistingHeapFromAddress` refuses partial commits, interior
     addresses and process-heap blocks.
   - The buffer placed over the heap must have `ALLOW_CROSS_ADAPTER`.
4. **An open heap pins its range.** Until the heap is released, the title
   cannot unmap the view (`UnmapViewOfFile2`) or decommit the memory
   (`VirtualFree`). Both fail with error 87.
5. **Cost.**
   - No commit beyond the title's own.
   - Every page is faulted in and locked: the working set grows by the
     heap's size, which the adapter counts as non-local memory in use.
   - Opening takes 0.2–0.55 s a GiB on fresh pages and about 0.05 s a GiB
     on pages already written. Releasing takes about 20 ms a GiB.
6. **Bandwidth over PCIe 4.0.**
   - Shaders read imported memory at 23–25 GB/s. Copy queues move it at
     26–27 GB/s either way. Shader stores of 4 bytes reach 19–22 GB/s;
     16-byte stores reach 0.1 GB/s.
   - For comparison: video memory 700 GB/s, a single-threaded CPU memcpy
     26–28 GB/s.
7. **A frame's 256 MiB.**
   - To the GPU: today's staged path takes 21 ms, in place 11 ms, a video
     memory mirror filled by the copy queue straight from imported memory
     11 ms.
   - Back to the CPU: staged 23 ms, mirror 11 ms.
   - These are all far below the 500–750 ms a world frame spends copying
     today. That time is VIctory's way of copying, not bandwidth.
8. **Latency.** One word through a round trip:
   - fence path: 65 µs in place, 66 µs staged;
   - a CPU polling the word sees the GPU's write in 29 µs.
9. **Recommendation (section 8).** Use heaps that follow the title's
   committed ranges (`FromAddress1`), not fixed windows.
   - Imported RAM for what the CPU writes and the GPU reads once or twice.
   - A video memory mirror, refreshed by the copy queue straight from the
     imported heaps, for what the GPU reads often.
   - Video memory, then a copy-queue copy back, for anything shaders write.
   - Physical pages imported through the runtime's own permanent views,
     never through the title's.

## 3. What imports

`title_memory_import_matrix`, 4 MiB per case. Identical on the RTX 4080 and
WARP, except where noted.

- **Checked both ways:** the CPU writes a pattern into the first half, a
  shader adds one to every word into the second half, and the CPU checks every
  word. Through a second view too, for physical pages.
- **Write watch:** host pages reported after the CPU's writes, out of those
  written, then after the GPU's.

| Title memory | Opened with | Result | Both ways | Write watch: CPU / GPU writes | Second view |
|---|---|---|---|---|---|
| virtual, reserved and committed at once, write-watched (xmem) | `FromAddress` | heap: custom, write-back, L0, flags 0x621 | yes | 512 of 512 / 0 (WARP: 512) | |
| the same, buffer without `ALLOW_CROSS_ADAPTER` | `FromAddress` | heap opens, `CreatePlacedResource` E_INVALIDARG | | | |
| the same | `FromAddress1` (0, 4 MiB) | heap, flags 0x421 | yes | 512 of 512 / 0 | |
| virtual, committed at once, not watched | `FromAddress` | heap, flags 0x421 | yes | | |
| virtual, reserved, then all committed by a second call | `FromAddress` | heap, flags 0x621 | yes | 512 of 512 / 0 | |
| virtual, reserved, first half committed | `FromAddress` | E_INVALIDARG | | | |
| the same | `FromAddress1` (0, 2 MiB) | heap | yes | 256 of 256 / 0 | |
| virtual, reserved only | `FromAddress` | E_INVALIDARG | | | |
| virtual, committed: 64 KiB into the allocation | `FromAddress` | E_INVALIDARG | | | |
| the same | `FromAddress1` (+64 KiB, 2 MiB) | heap | yes | 256 of 256 / 0 | |
| physical pages: `SEC_RESERVE` section, committed views in placeholders (xmem) | `FromAddress` | heap, flags 0x421 | yes | | sees the GPU's writes |
| the same | `FromFileMapping` | E_INVALIDARG | | | |
| the same | `FromAddress1` (0, 4 MiB) | heap | yes | | sees the GPU's writes |
| the same: the view's second half | `FromAddress1` (+2 MiB, 2 MiB) | heap | yes | | sees the GPU's writes |
| physical pages: `SEC_RESERVE` section, committed plain views (`page_window`, formats F3) | `FromAddress` | heap | yes | | sees the GPU's writes |
| the same | `FromFileMapping` | E_INVALIDARG | | | |
| physical pages: `SEC_COMMIT` section, views in placeholders | `FromAddress` | heap | yes | | sees the GPU's writes |
| the same | `FromFileMapping` | heap | yes | | sees the GPU's writes |
| the same, plain views | `FromAddress1` (0, 4 MiB) | heap | yes | | sees the GPU's writes |
| `XMemAllocDefault` block (process heap) | `FromAddress` | E_INVALIDARG | | | |

The heap flags:
- 0x621 is `SHARED | SHARED_CROSS_ADAPTER | ALLOW_WRITE_WATCH | ALLOW_SHADER_ATOMICS`;
- 0x421 is the same without `ALLOW_WRITE_WATCH`. Write watch keeps working
  through `FromAddress1` all the same.

Every heap is `CUSTOM`, CPU write-back, memory pool L0, so atomics are
allowed. Placing a buffer over the whole heap takes no measurable time.

**Under an open heap** (`physical_pages_remapped_under_an_open_heap`,
`virtual_memory_decommitted_under_an_open_heap`, on both adapters):
- With a heap open over a view of physical pages, xmem's unmapping fails
  with error 87 (`UnmapViewOfFile2` with `MEM_PRESERVE_PLACEHOLDER`, as
  `XMemMapPhysicalPages` does before mapping other pages there). The same
  call on a view with no heap succeeds. Once the heap is released, the view
  unmaps.
- With a heap open over virtual memory, `VirtualFree(MEM_DECOMMIT)` of one
  of its pages fails with error 87. It succeeds once the heap is released.

The question "what does the GPU read after the title remaps the address"
therefore never arises: the remapping itself is refused.

## 4. Sizes, open time and commit cost

`import_limits_and_commit_cost`.
- One full run before any game session (14:32).
- Eight rows of a second run, which added "pages written first" and the heap
  release time. It was then stopped to free the machine.
- Times in ms; Δ in MiB. "Far end": the GPU reads and writes the heap's
  last 128 KiB.

| Title memory | Opened with | 256 MiB | 1 GiB | 2 GiB | 4 GiB − 64 KiB | 4 GiB, 4 GiB + 64 KiB, 6 GiB |
|---|---|---|---|---|---|---|
| virtual, committed, write-watched | `FromAddress` | 47–311 | 182–530 | 955 | 1,175 | E_INVALIDARG |
| the same | `FromAddress1` (whole) | 48–91 | 221 | 942 | 1,015 | E_INVALIDARG |
| virtual, reserved, first ≤ 4 GiB − 64 KiB committed | `FromAddress1` (committed part) | 50–104 | 304 | 657 | 1,066 | opens its committed 4 GiB − 64 KiB: 1,197, 1,192, 1,291 |
| physical pages: `SEC_RESERVE` section, committed view in a placeholder | `FromAddress` | 59–281 | 281 | 1,694 | 2,189 | E_INVALIDARG |
| the same | `FromAddress1` (whole) | 82–183 | 462 | 866 | 1,845 | E_INVALIDARG |
| physical pages: `SEC_COMMIT` section | `FromFileMapping` | 13–176 | 377 | 223 | 996 | E_INVALIDARG |
| virtual, committed, every page written first | `FromAddress` | 13.7 (fresh pages, same run: 46.5) | | | | |

The first number of a range is the second run; the 256 MiB rows of the first
run include the process's first opens. Every heap that opened reached its far
end.

**Cost of a heap**, on the RTX 4080:
- **Commit:** none beyond the title's own. At opening, the process's and the
  system's commit change by 0 ± noise. Allocating commits as usual: virtual
  memory counts in the process's private bytes, sections only in the system's.
- **Residency:** on fresh pages the working set grows by the heap's whole
  size at opening. The driver faults every page in and locks it. On pages
  already written it does not grow, but they are locked all the same: the
  title can no longer decommit them (section 3).
- **Adapter accounting:** DXGI's non-local usage grows by the heap's size,
  against a non-local budget of 47 GiB here.
- **Time:** most of the opening time is faulting in demand-zero pages,
  0.05 ms a MiB on written pages against 0.2–0.55 ms a MiB on fresh ones.
  Releasing takes 5.6 ms for 256 MiB and 18.8 ms for 1 GiB; freeing the
  memory then takes 13 and 53 ms.

On WARP:
- address heaps cost the same working set and about 0.2 ms a MiB to open
  (682 ms for 4 GiB − 64 KiB);
- `FromFileMapping` over a `SEC_COMMIT` section is free (0.2 ms, nothing
  locked);
- the limits are the same.

xcl's notes report the same limits on AMD: partial commits refused, 4 GiB
refused, 4095 MiB open in 0.5 s.

## 5. Bandwidth

`bandwidth` (RTX 4080) and `imported_memory_by_kind`. GB/s from GPU
timestamps, CPU copies from the wall clock. The source is xmem's committed,
write-watched virtual memory, opened with `FromAddress`.

| Path | 256 MiB | 1 GiB | 4 GiB − 64 KiB |
|---|---|---|---|
| CPU writes title memory (memcpy, one thread) | 28.1 | 26.9 | 27.2 |
| GPU shader reads imported title memory | 23.2 | 23.0 | 24.7 |
| GPU shader writes imported title memory, 16 B a thread | **0.1** | **0.1** | **hung the GPU** |
| GPU shader writes imported title memory, 4 B a thread | 19.4–22.1 (by kind) | | |
| CPU reads title memory the GPU wrote | 20.2 | 25.2 | 26.1 |
| copy queue: imported title memory → video memory | 26.8 | 26.8 | |
| direct queue: the same | 23.6 | 23.2 | |
| copy queue: video memory → imported title memory | 26.3 | 26.3 | |
| CPU writes an upload heap (today's staging) | 25.6 | 25.7 | |
| copy queue: upload heap → video memory | 26.8 | 26.7 | |
| copy queue: video memory → readback heap | 26.3 | 26.3 | |
| CPU reads a readback heap | 26.0 | 25.7 | |
| CPU writes the GPU upload heap (video memory through the BAR) | 22.5 | 22.5 | |
| GPU shader reads the GPU upload heap | 686 | 710 | |
| GPU shader reads video memory | 710 | 694 | |
| GPU shader writes video memory | 707 | 489 | |

At 16 MiB, video memory reads show 2,340 GB/s: the buffer fits the GPU's L2
cache.

**By kind of title memory**, 256 MiB, RTX 4080. The kinds were:
- virtual, write-watched, with `FromAddress` and `FromAddress1`;
- virtual, not watched;
- physical pages from a `SEC_RESERVE` view;
- a `SEC_COMMIT` section by mapping and by address.

The rates are the same for every kind and every flag:

| | Shader reads | Shader writes, 16 B a thread | Shader writes, 4 B a thread | Copy queue in | Copy queue out |
|---|---|---|---|---|---|
| RTX 4080 | 22.7–23.9 | 0.1 | 19.4–22.1 | 26.2–26.4 | 26.8 |
| WARP | 4.1 | 4.2–4.3 | 4.0 | 5.0–5.1 | 5.0–5.2 |

**The 16-byte store trap.** On NVIDIA, a shader that writes imported memory
16 bytes a thread (`Store4`, what an RDNA `buffer_store_dwordx4` becomes)
runs 200 times slower than one that writes 4 bytes a thread.
- It does not depend on the memory kind, the write-watch flag or the opening
  call.
- One 4 GiB dispatch of such stores took long enough for Windows to declare
  the GPU hung (`DXGI_ERROR_DEVICE_HUNG` at 14:45), probably with a driver
  reset that disturbed the machine.
- The test now writes imported memory 4 bytes a thread, and times 16-byte
  stores over 16 MiB only.
- The cause is not known: a guess is partial-line writes to snooped system
  memory.

## 6. Latency

`latency`, 300 rounds. A list is submitted after the CPU writes one word; the
list reads it and writes it plus one; then the CPU waits until it sees the
answer. µs.

| Round trip | RTX 4080 min / median / p99 | WARP min / median / p99 |
|---|---|---|
| empty list, fence event | 50.0 / 59.0 / 281 | 20.5 / 32.1 / 72.7 |
| empty list, fence spin | 48.8 / 57.6 / 186 | 19.6 / 21.0 / 47.5 |
| imported: CPU write → shader → CPU read after the fence | 55.8 / 64.5 / 451 | 27.4 / 31.4 / 70.5 |
| imported: the same, the CPU polling the word (no fence) | 25.5 / 29.4 / 289 | 16.3 / 17.0 / 37.4 |
| staged: CPU → upload → video memory → shader → readback → CPU | 53.9 / 65.8 / 125 | 27.7 / 30.3 / 94.7 |

Submission and the fence cost about 58 µs; working in place adds about 7 µs,
the same as staging a word. The GPU's write reaches a CPU that polls title
memory in half the fence's time. Memory waits and completion writes in title
memory (G8–G10) can poll the word directly.

## 7. A frame's 256 MiB, each way

`frame_cost`: 256 MiB, the size a world frame copies today, once to the GPU
and once back. ms, median of 5; the CPU side is one thread.

| Direction | Path | RTX 4080 | WARP |
|---|---|---|---|
| CPU → GPU | staged (today): memcpy to an upload heap, copy to video memory, shader reads it | 21.4 | 156 |
| CPU → GPU | imported (G16): shader reads title memory in place | 10.9 | 62 |
| CPU → GPU | mirror (G17): copy queue pulls imported title memory into video memory, shader reads it | 10.7 | 119 |
| CPU → GPU | mirror through the GPU upload heap: memcpy into video memory, shader reads it | 12.7 | 72 |
| GPU → CPU | staged (today): shader writes video memory, copy to a readback heap, memcpy to title memory | 23.0 | 153 |
| GPU → CPU | imported: shader writes title memory in place, 16 B a thread | **2,857** | 64 |
| GPU → CPU | mirror (G17): shader writes video memory, copy queue pushes it to title memory | 11.0 | 117 |

The "imported, GPU → CPU" row was measured with 16-byte stores before the
test was changed. With 4-byte stores the rates above put it near 13 ms; that
was not measured, since long benchmarks no longer run on this machine.

At the hardware's rates, a frame's 285 MB costs about 45 ms round trip
staged, and about 22 ms in place or through a mirror. Today a world frame
spends 500–750 ms on it. Bandwidth is not the bottleneck: VIctory's way of
copying is (`gpu.md` 6.2: `ReadProcessMemory` over many ranges, snapshots
and comparisons, one thread, serial submissions). Any of the designs here
removes that; importing also removes the bookkeeping.

## 8. Recommended design for G15–G18 on NVIDIA

**Heaps follow the title's committed ranges, not fixed windows.**
- **Opening.** One heap per committed range of title memory, at most
  4 GiB − 64 KiB, opened with `OpenExistingHeapFromAddress1`.
  - A buffer cannot span two heaps, so heaps follow the title's own
    allocations. BTP 6 makes few, large ones (xmem notes 23
    `XMemVirtualAlloc` calls, and 6.1 GiB of physical pages in one call).
  - Open them on a background thread when the range is committed, or at its
    first GPU use: 0.05–0.55 s a GiB is no per-frame cost.
- **Placing.** One buffer over each heap, `ALLOW_UNORDERED_ACCESS |
  ALLOW_CROSS_ADAPTER`, in `COMMON`. A title address becomes the buffer's GPU
  address plus its offset in the heap. VIctory's shader ABI stays as it is:
  host descriptors point there.
- **Fallback.** Without `ID3D12Device13`, use `OpenExistingHeapFromAddress`
  on whole, fully committed allocations. That is xcl's constraint, so commit
  grows.
- **Before the title changes its memory.** Release the heap (after the GPU
  is done with it) before xmem decommits, releases or unmaps, then reopen
  what remains. Otherwise the title's call fails (error 87).
- **Limit.** Imported memory is locked RAM: cap the total imported, and keep
  the shadow path for the rest.

**Where each kind of title data lives:**

| Title data | Where | Why |
|---|---|---|
| CPU-written, read by the GPU once or twice a frame: constants, root argument tables, console descriptor heaps, indirect arguments, small dynamic vertex and index data | imported RAM (G16), read in place | 23 GB/s, the same as an upload; no copy, staging or dirty tracking; same latency |
| GPU results the CPU reads: readback buffers, queries, occlusion and exposure, memory writes and labels | written by copies (copy queue, `WriteBufferImmediate`) into imported RAM; shaders write video memory, then a copy-queue copy goes back | DMA writes run at 26 GB/s and are coherent; 16-byte shader stores crawl and can hang the GPU |
| Read by the GPU many times a frame: textures (detiled anyway), vertex and index pools, BVH, large structured buffers | a video memory mirror (G17a), refreshed by the copy queue **straight from the imported heaps**; write watch finds changed chunks (G17b) | 700 GB/s against 23 GB/s. Pulling from imported memory needs no CPU memcpy and no staging ring. Write watch survives import and is not dirtied by the GPU |
| GPU-only: render targets, depth, UAV scratch, metadata planes | video memory only (G19) | the CPU never needs them |
| Physical pages (the 6.1 GiB section) | heaps over the runtime's own permanent view of each section (formats F3), in pieces of at most 4 GiB − 64 KiB with `FromAddress1`; never over the title's views | an open heap blocks `XMemMapPhysicalPages`' unmapping; a permanent view is never unmapped while its section lives. The heap is on the section's pages, so every view stays coherent with it |

**Order of work:**
1. **G15b** (S; `xtp-gpu`, plus `xtp-hle`):
   - a new `uma.rs`: the registry of heaps over title ranges
     (`FromAddress1`, with `FromAddress` as fallback), address-to-buffer
     lookup, release;
   - hooks in xmem for commit, decommit, release and unmap, so heaps close
     first. This touches `crates/xtp-hle/src/modules/xmem.rs`, owned
     elsewhere;
   - the GPU tests take over this file's matrix and the two "under an open
     heap" tests.
2. **G16** (M): coherent small buffers in place (the first row of the table),
   behind `XTP_UMA=1`.
3. **G17a and G17b** (M each): video memory windows as planned. Dirty chunks
   are copied by the copy queue from the imported heaps, which replaces xcl's
   upload ring; write watch stays as today.
4. **Physical pages** (S, after formats F3): heaps over the permanent views.
   DirectStorage writes become plain copies there, and the GPU sees them.
5. **G18**: shadows retired. Then G19.

## 9. Risks and open questions

**Risks:**
1. **`ID3D12Device13`.** It comes with this Windows build's runtime. Older
   Windows 10 and 11 need the Agility SDK (G22). Without it, only whole,
   fully committed allocations import, and commit grows.
2. **Locked RAM.**
   - Every imported byte is resident and counted against the adapter's
     non-local budget.
   - Importing 10–13 GB of title memory is fine with 64 GB of RAM, not
     with 16 GB.
   - The cap and the shadow fallback are part of the design.
3. **Heaps block the title.**
   - Unmapping and decommitting fail under an open heap. A hook xmem
     misses becomes a failing title call.
   - xmem's hooks need the `xtp-hle` owner's agreement.
4. **Shader writes into imported memory.**
   - 16-byte stores run at 0.1 GB/s on NVIDIA and can hang the GPU.
     Translated shaders emit them.
   - Title buffers that shaders write must live in video memory and come
     back by copy queue.
   - Whether AMD or the cloud L4 (also NVIDIA) behave the same is unknown.
5. **PCIe.** Every read of imported memory costs size / 23 GB/s. Data read
   several times a frame belongs in the mirror.
6. **Heap size.** At most 4 GiB − 64 KiB, and buffers cannot span heaps.

**Not measured here:**
- Tiles of reserved resources mapped onto imported heaps. xcl reports a
  device removal with heaps over file mappings, and a GPU fault was not
  risked on a machine in use.
- AMD and Windows 10.
- Two devices importing the same memory.
- `FromAddress1` over the far part of a view larger than 4 GiB, as heaps
  over a 6.1 GiB permanent view need. At 4 MiB, a view's second half and an
  interior range of a virtual allocation both open.
- The second sizes run was cut after 256 MiB and 1 GiB.

**Incident.** At 14:45, the 4 GiB run of 16-byte stores into
imported memory hung the RTX 4080 (`DXGI_ERROR_DEVICE_HUNG`) while the
machine was in use. The benchmarks were stopped. The test no longer issues
those stores in bulk. Long GPU benchmarks now go to the cloud L4.
