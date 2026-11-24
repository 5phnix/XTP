# Formats and the data pipeline: VIctory (XTP's base) and xcl

Analysis for XTP. XTP is VIctory imported at `ffe310e`; xcl is
read as a reference only (GPL-2.0-only: behaviour may be re-implemented, code
is never copied). Crate names below are the XTP ones (`xtp-formats` is
VIctory's `victory-formats`, and so on); paths in VIctory are given where the
rename has not landed yet only when they differ in more than the prefix.

## 1. Summary

* **Keep VIctory's pipeline.** Its DirectStorage scheduler (one pool of
  workers for every queue, weighted by priority, bounded by the staging
  buffer, completions in queue order) and its zero-copy destinations are
  ahead of xcl, which runs one worker thread per queue and copies every
  request through two or three intermediate buffers. xcl's BCPack decoder is
  a port of VIctory's.
* **Fix the loading bottleneck in `xtp-hle` xmem, not in the decoders.**
  Every DirectStorage request into physical pages maps a view of the page
  section, commits it, writes, and unmaps it. These calls change the address
  space, which is a process-wide kernel lock, and the unmap flushes the TLB on
  every core. 32 workers do this thousands of times a second, while the
  title's own page faults wait on the same lock. Section 6 gives the fix:
  every allocation keeps one permanent view (xcl gives each page a permanent
  home address too), writes are plain copies, and a journal of written pages
  replaces hashing and guard flips.
* **Take xcl's approach for what VIctory lacks in texture layouts:** the XOR
  swizzle modes with the console's pipe configuration (Series X and S), the
  render-target metadata planes (CMASK, HTILE), and testing against the
  console's own layout library as an oracle.
* **One correctness bug to fix first:** `xtp-formats` reads the BC7 entries
  of the BCPack dictionary one 16-bit word off (section 5.3). Upstream
  VIctory fixed it after `ffe310e` (its PR #658). That fix is GPL-3.0 and may
  be taken as it is.
* **RPF needs no parser to run the title.** XTP does not read the title's
  archives; the title reads them itself. Both projects only scan for plain
  shader records.

## 2. Scope and files

In scope: archive formats, the file and streaming path into title memory,
DirectStorage, zlib and BCPack decoding, texture tiling and layouts, the
ahead-of-time conversion pipeline and its caches. Out of scope (other
reports): loader, HLE in general, GPU command translation, shaders, tooling.
Where a fix in this area touches a GPU file, this report says so.

| Sub-area | XTP (VIctory) files | xcl files (reference) |
|---|---|---|
| RPF archives | none (the title reads them); `xtp-game/src/sgd5.rs` scans shader records in any file | `rpf.rs` (`xcl rpf`: list, find, extract), `rpf_probe.rs` (bounded-memory header probe), `gpu/pack.rs` (record scanner), `tests/rpf_e2e.rs` |
| File path into title memory | `xtp-hle/src/modules/files.rs` (virtual drives, `ReadFile`/`fread` open guarded blocks first) | `vfs.rs`, `vfs_hooks.rs` (KERNELBASE hooks, read-only game folder with copy-on-write overlay) |
| DirectStorage | `xtp-hle/src/modules/dstorage.rs` (1,545 lines) | `hle/dstorage.rs` (2,486 lines) |
| zlib | `miniz_oxide` in `dstorage.rs::inflate` | `miniz_oxide` in `hle/dstorage.rs::execute` |
| BCPack | `xtp-formats/src/bcpack.rs`, `bcpack-tables.bin` (policy exception) | `hle/bcpack/{mod,victory,tables,tests}.rs`, `tests/bcpack_parity.rs`, `scripts/gen_bcpack_victory_tables.py`, `tests/data/bcpack/` |
| Physical pages and write tracking | `xtp-hle/src/modules/xmem.rs` (sections, `page_window`, `write_physical`), `xtp-gpu/src/watch.rs` (write watch, guarded views) | `hle/kernelx.rs` (`page_home`, `write_physical_pages`), `gpu/uma.rs` |
| Texture tiling | `xtp-formats/src/texture.rs` (addrlib-based `Layout`, `Texture`), `xtp-hle/src/modules/xg.rs` | `gpu/tiling.rs`, `gpu/tiling_tables.rs` (from `scripts/gen_tiling.py`), `gpu/texture.rs`, `hle/xg.rs`, `tests/xg_oracle.rs` |
| Title textures in memory | `xtp-gpu/src/texture.rs` | `hle/d3d12x/title_textures.rs` |
| Ahead-of-time conversion and caches | `xtp-pipeline/src/{lib,cache}.rs`, `xtp-gpu/src/shader_cache.rs`, `xtp-gpu/src/pipeline/library.rs` | DXIL cache (`dxc::compile_cached`), `xcl shaders --precompile` |

Documents read: VIctory `README.md`, `docs/architecture.md`,
`docs/asset-policy.md`; xcl `README.md`, `AGENTS.md`, `HANDOFF.md` (searched),
`THIRD_PARTY.md`, xcl's design notes (`docs/specs/`).

## 3. How data reaches the title in XTP today

```
title code ──ReadFile/fread──► files.rs ──► host file (game folder)        (sync reads)
     │                           └─ kernel_will_write: open guarded blocks
     └─DStorage queue──► dstorage.rs scheduler (N = cores workers, WRR by priority,
                          32 MiB staging bound) ──► read (seek_read)
                          ──► zlib (miniz_oxide) ──► BCPack (xtp-formats)
                          ──► destination:
                               address  → in place (after kernel_will_write)
                               pages    → xmem::page_window: MapViewOfFile + commit,
                                          decode in place, UnmapViewOfFile
                               scattered→ Vec, then xmem::write_physical (a view per run)
GPU side: xtp-gpu watch.rs (GetWriteWatch / read-only guards) and texture.rs
(physical-page textures: BLAKE3 of the whole memory at each submission that
samples them; a change detiles the whole texture).
```

## 4. Measurements

Taken on a local title folder, read-only (32 logical cores), no game data
copied.

| What | Result |
|---|---|
| RPF headers | all start with `9FPR` (RPF9) |
| Disk, sequential | 3,668 MB/s (4 MiB reads, 2 GiB from the middle of a large archive, one thread) |
| Disk, random | 4,914 IOPS, 322 MB/s (64 KiB reads, queue depth 1, one thread) |
| BCPack decode, `xtp-formats` (`bcpack_throughput`, release, synthetic) | BC1 93.8 MB/s, BC4 90.8 MB/s, BC7 54.1 MB/s out, per core |
| zlib inflate, proxy | 244 MB/s out per core (C zlib through Python on 64 MiB of the title's executable, ratio 0.52); `miniz_oxide` is not measured yet (item F14) |

Reading: one core of BCPack or zlib delivers 50-250 MB/s, so the 32 workers
can decode 1.7-8 GB/s together, at or above what the disk delivers. Decoding
is not the first suspect for the 50 s load. Writes into title memory are
(section 6), but no stage timing exists yet to show it, so item F2 adds
one. xcl publishes no throughput figures for DirectStorage, BCPack or
detiling.

## 5. Capability matrix and verdicts

### 5.1 Matrix

| Sub-area | VIctory (XTP) state | xcl state | Tests: XTP / xcl | Throughput: XTP / xcl |
|---|---|---|---|---|
| RPF9 container | not parsed; the title reads it | record scan without TOC, list/extract, bounded probe for >1 GB | 0 / 11 (rpf, probe, e2e) | n/a / not stated |
| Shader records in archives | `sgd5.rs`: marker scan, sizes cross-checked, inflate | `gpu/pack.rs`: same record shape, qualified names fixed (26,503 records) | 2 / 4 | not measured on either side |
| File path, game folder | virtual drives over host folders; the game drive is the game folder itself (writes reach it) | VFS: read-only game folder, overlay, whiteouts, hooks on KERNELBASE | 5 / 22 | n/a |
| DirectStorage scheduling | shared pool, weighted round robin over 3 priority classes, staging bound, in-order statuses, auto-submit at half capacity | one thread per queue, requests in order | 13 / 14 | 5 s debug summary (MB/s, busy, reading, BCPack workers) / counters only |
| DirectStorage API surface | factory: open file, status array, staging size, queue; queue: enqueue request/status, submit, close. Signal, cancel, error event/record, query: logged as missing | also EnqueueSignal (fence), CancelRequestsWithTag, GetErrorEvent, RetrieveErrorRecord, Query; file Close/GetHandle/GetId; two factory IIDs; console 0x8924xxxx error codes | see above | n/a |
| Request options | zlib, BCPack, page destination, memory source | also source in physical pages (bit 23); swizzle (bits 5-20) refused by both | | |
| Page arrays | read at execution | snapshot at enqueue (stack arrays); entries 0 at enqueue read again at execution | `physical_page_destinations` / `a_destination_page_filled_in_after_enqueue` | |
| zlib | `miniz_oxide` straight into the destination when possible; short streams pass | `miniz_oxide` into a new `Vec`, then copied; `intermediate_size` checked | in dstorage tests / `multi_stage_intermediate_size_validation_and_zlib` | not measured |
| BCPack | decoder of BC1, 3, 4, 5, 6H, 7; tables parsed from `bcpack-tables.bin`; **BC7 dictionary entries read one word off** | the same decoder (ported from VIctory) with tables generated as Rust; the BC7 entry layout corrected; parity against a reference decoder on retail chunks of all six modes | 11 (synthetic encoder, noise, opt-in reference chunks) / 6 + parity + synthetic fixtures from an independent encoder | 54-94 MB/s per core / not stated |
| Destinations in title memory | address: in place; pages: a temporary view per request (map, commit, unmap) | address: in place after a check; pages: copy to each page's permanent home address | | **the README's limit** / not stated |
| Write tracking for the GPU | write watch (64 KiB units); views guarded read-only, the first write faults; physical-page textures hashed | write watch per 4 GiB window before each ExecuteCommandLists; ranges closer than 1 MiB merged; batched uploads | 3 / 13 (title textures) | not measured |
| Tiling, non-XOR modes | addrlib (`Layout::new`), thin and thick 3D, mip tail, depth/stencil as two planes; copies by contiguous runs | addrlib patterns over GF(2), per-element offsets with column and row tables | 9 / 14 | runs (memcpy per run) / per element |
| Tiling, XOR modes (`_X`, `_T`) | **not addressable**; `XGSuggestSwizzleMode` gives them only to render targets and depth | all 14 2D modes, with the pipe configuration: Series X 32 pipes / 8 packers, Series S 8 / 4 | 0 / in tiling and oracle tests | |
| Metadata planes | missing (README: "render-target metadata planes") | CMASK and HTILE placement rules checked on 1,872 descriptions | 0 / in `xg_oracle.rs` | |
| Checked against the console | no oracle test; no retail texture check | `xg_oracle.rs` compares against the console's layout library (29,700 descriptions); retail texture archive: 909 of 912 textures | | |
| Ahead-of-time conversion | `Converter` trait and content-addressed cache; only user: shader translation; `xtp translate` warms the shader cache | `xcl shaders --precompile` fills the DXIL cache from `shaders.rpf` | 4 / several | n/a |
| Caches | BLAKE3-keyed, write then rename, refuses the repo; no size limit or pruning; shader key = BLAKE3 of the executable | DXIL cache: one compile per shader across threads and processes (named mutex), cached blobs validated, key = translator sources + DXC version | 4+2 / several | n/a |

### 5.2 Verdicts

| Sub-area | Verdict | Why |
|---|---|---|
| RPF9 container | **Keep VIctory** (no parser); a bounded probe is optional new work (F17) | XTP does not read the title's archives; the title reads them itself |
| Shader records | **Keep VIctory** | Same scan as xcl; the shader report owns warm-up |
| File path, game folder | **Rewrite after xcl** for the read-only rule; handed to the HLE report | Writes now reach the person's game folder. The policy is right, but the module belongs to the HLE report |
| DirectStorage scheduling | **Keep VIctory** | Better than xcl's thread per queue |
| DirectStorage API surface | **Rewrite after xcl** (F9, F10) | Robustness for other titles and SDK paths. BTP 6 does not call these slots, so low priority |
| Page arrays | **Rewrite after xcl** (F8) | Reading at execution risks stack arrays freed after enqueue |
| zlib | **Keep VIctory** | Already in place and zero-copy |
| BCPack | **Keep VIctory, fix BC7** (F1) | It is the original; take upstream VIctory's fix |
| Destinations in title memory | **New work, xcl's idea of a permanent home** (F3-F7) | The bottleneck, section 6 |
| Write tracking | **New work** (F5, F6), with xcl's merged ranges and batching as reference for the GPU report | |
| Tiling, non-XOR | **Keep VIctory** | Correct, faster copies |
| Tiling, XOR and planes | **Rewrite after xcl** (F11-F13) | Needed once the title copies render targets or depth through the CPU, or aliases them with textures |
| Ahead-of-time conversion | **Keep VIctory** | Nothing but shaders can be converted ahead of time (XTP does not read the archives) |
| Caches | **Keep VIctory, add limits** (F15) | Single flight and pruning, after xcl's DXIL cache |

### 5.3 The BC7 dictionary bug

`bcpack-tables.bin` holds 255 BC7 dictionary entries of 116 bytes each: a
16-bit control word, 24 endpoint words, two index widths, then 64 bytes.
`Tables::parse` in `xtp-formats` reads those 64 bytes as 32 signed indices.
In the shipped file they hold the two widths again (one 16-bit word), then
the 16 indices of subset 0 and the 15 of subset 1. Reading the file shows it:
for entry 1, the word after the widths `[3, 0]` is 3 again, and the indices
that follow match the corrected layout. The current parse therefore shifts
every BC7 dictionary index by one slot. A BC7 block that refers to the
dictionary (references -1 to -255) decodes with wrong indices. The tests do
not catch it: they build their own tables in the parser's layout. xcl's
generator script notes the corrected layout as "VIctory PR #658", so
upstream VIctory fixed it after `ffe310e`.

## 6. "Loading is limited by writes into title memory": diagnosis and fix

### 6.1 What happens per request today

For a page destination (`PAGE_DESTINATION`, how the title streams texture
and buffer data into its physical pages):

1. `xmem::runs` takes the global page-table mutex. The title's own
   `XMemMapPhysicalPages*` calls take the same mutex.
2. `page_window` calls `MapViewOfFile` on the section, then `VirtualAlloc`
   `MEM_COMMIT` on the view (the sections are `SEC_RESERVE`).
3. The decoder writes into the view. Each 4 KiB page touched for the first
   time is a demand-zero fault: the kernel zeroes it and adds it to the
   working set.
4. Dropping the view calls `UnmapViewOfFile`, which flushes the TLB on every
   core that runs a thread of the process (32 here).
5. Pages of several allocations or out of order take another path: a heap
   `Vec` (zeroed, so first-touch faults again), then `write_physical` with one
   map, commit and unmap per run.

For an address destination, `kernel_will_write` takes the `watch` mutex. For
each guarded 64 KiB block in the range it calls `VirtualProtect` once
(`open` does not merge neighbouring blocks). A destination in a mapped view
that a copy has guarded since costs one exception and one `VirtualProtect`
per block.

Steps 2, 4 and the protection changes all modify the address space. Windows
serialises those per process (the address-space lock) and runs page faults
against the same lock. With up to 32 workers each doing three such calls per
request, plus TLB-flush interrupts, the workers spend their time waiting on
each other and on the title. Adding workers then does not raise throughput.
This fits the README's statement. The existing debug summary
(`busy_workers`, `reading_workers`, `bcpack_workers`) can show busy workers
that are not reading or decoding, but nothing times the destination step
itself. F2 adds that before anything else.

Then the GPU side adds a cost per submission: every texture that samples
physical-page memory is hashed in full with BLAKE3 (views cannot be write
watched), and a change detiles the whole texture.

### 6.2 What xcl does differently (behaviour)

xcl gives each 64 KiB physical page a permanent "home" address inside its
unified-memory windows (`hle/kernelx.rs`: `page_home`,
`resolve_physical_pages`). `write_physical_pages` and `read_physical_pages`
are plain copies to those addresses under one mutex; no request maps or
unmaps anything. Its price is that mapping a page at a second address copies
the data there and back, so two addresses of one page never alias at once.
VIctory's sections do alias correctly. XTP should keep the sections and adopt
only the permanent address.

### 6.3 The fix

1. **One permanent view per allocation** (F3). When
   `XMemAllocatePhysicalPages` creates a section (BTP 6 asks
   for 6.1 GiB in one call), map one view of all of it, once, for the
   runtime's own use. Commit each 64 KiB slice the
   first time it is written: a bit per slice, a single `VirtualAlloc` per
   run of uncommitted slices, never per request. `page_window` becomes
   address arithmetic on that view. `write_physical` copies into it run by
   run. Nothing is unmapped until the allocation is freed. Lookups take the
   page table's read lock (`RwLock`), so workers no longer queue behind each
   other.
2. **No per-request allocations** (F4). Each worker keeps its source,
   intermediate and scatter buffers across requests, so fresh zeroed heap
   pages stop faulting on every request. Scattered pages decode into the
   worker's buffer, then one copy per run into the permanent view.
3. **Merged protection changes** (F5). `watch::open` restores write access
   once per run of neighbouring guarded blocks, not once per block. An
   atomic count of guarded blocks lets `kernel_will_write` skip the mutex
   when nothing is guarded.
4. **A journal of written physical pages** (F6). Every runtime write into
   physical pages (DirectStorage, `write_physical`) notes the page numbers
   with the journal's next number, as `watch.rs` already does for
   write-watched memory. `xtp-gpu/src/texture.rs` then asks the journal
   whether a physical-page texture changed instead of hashing it, and can
   detile only the written pages' blocks later. Writes by the title's own
   code through its mapped views stay covered by the guards. This touches
   `xtp-gpu` and needs the GPU report's owner to agree.
5. **Faults off the request path, if F2 shows them** (F7). Touch newly
   committed slices on a background thread after allocation, so streaming
   writes find resident pages.

Expected effect: per request, zero address-space changes in the steady state
(instead of three or more), one mutex taken for reading, and no TLB flushes.
Throughput should then scale with workers up to the decode and disk limits
of section 4. F3's acceptance test measures this.

## 7. Porting backlog (ordered)

Each item fits one worker session. "xcl reference" names where the
behaviour can be read; its code is never copied. Environment variables follow
the `XTP_` prefix once the rename lands.

### F1. Read the BC7 dictionary entries in their real layout (S)
- **Goal:** parse each BC7 dictionary entry as control word, 24 endpoint
  words, two widths, the widths again (skipped), then 16 + 15 indices (the
  16th index of subset 1 is 0).
- **Files:** `crates/xtp-formats/src/bcpack.rs` (`Tables::parse`, its test
  table builder).
- **xcl reference:** `scripts/gen_bcpack_victory_tables.py` (BC7 loop:
  behaviour only). Better: upstream VIctory PR #658, GPL-3.0, which may be
  merged as it is.
- **Acceptance:** a unit test that builds a table file in the real layout,
  with distinct values in each field, and checks that a BC7 block referring
  to dictionary entry 1 decodes to the indices written there;
  `the_shipped_tables_parse` still passes; `reference_chunks` passes on BC7
  chunks when the person sets the variables.
- **Depends on:** none.

### F2. Time each DirectStorage stage (S)
- **Goal:** the 5 s debug summary splits busy time into read, inflate,
  BCPack, and destination (map/commit/unmap, guard opening, copies), plus
  counts of views mapped and protections changed.
- **Files:** `crates/xtp-hle/src/modules/dstorage.rs` (`Lanes::count`,
  `execute`, `into_destination`); counters in
  `crates/xtp-hle/src/modules/xmem.rs` (`View::map`).
- **xcl reference:** none (xcl counts requests and bytes only).
- **Acceptance:** a unit test runs a page-destination request and a zlib
  request and sees each stage's counter increase. One loading run with
  `XTP_LOG=xtp_hle=debug` is reported on the task with the split (the run
  only reports numbers; no data is stored).
- **Depends on:** none.

### F3. Permanent views of physical-page sections (M)
- **Goal:** section 6.3 point 1: one view per allocation, mapped once;
  commit tracked per 64 KiB slice; `page_window` and `write_physical` with
  no map or unmap; the page table behind an `RwLock`.
- **Files:** `crates/xtp-hle/src/modules/xmem.rs` (`Slice`, `View`,
  `runs`, `page_window`, `write_physical`, allocation and free),
  `crates/xtp-hle/src/modules/dstorage.rs` (`into_destination`).
- **xcl reference:** `hle/kernelx.rs`: `page_home`,
  `resolve_physical_pages`, `write_physical_pages` (each page has a fixed
  address and is written by copy).
- **Acceptance:** the existing `physical_page_destinations`,
  `zlib_into_pages_in_place_and_across_allocations` and
  `page_windows_cover_one_run_in_place` pass. A new test runs 10,000
  page-destination requests and finds the view-map counter of F2 unchanged
  after the first request per allocation. An ignored benchmark (16 threads
  writing 64 KiB into pages) reports MB/s before and after, in the
  delivery. Freeing an allocation unmaps its view, and title views still
  alias the same bytes (a test writes through one and reads through the
  other).
- **Depends on:** F2.

### F4. Reuse each worker's buffers (S)
- **Goal:** per-worker source, intermediate and scatter buffers kept
  across requests; scattered pages decoded into the worker's buffer, then
  copied run by run into F3's views.
- **Files:** `crates/xtp-hle/src/modules/dstorage.rs` (`read_source`,
  `execute`, `into_destination`).
- **xcl reference:** none (xcl allocates per request).
- **Acceptance:** dstorage tests pass. A test sends requests of growing
  sizes through one worker and checks that buffer capacity only grows, and
  that a scattered destination over two allocations gets the right bytes.
- **Depends on:** F3.

### F5. Merge guard openings, skip the mutex when nothing is guarded (S)
- **Goal:** `watch::open` calls `VirtualProtect` once per run of
  neighbouring guarded blocks. `writing` returns at once when an atomic
  count of guarded blocks is 0.
- **Files:** `crates/xtp-gpu/src/watch.rs` (GPU area: agree with its owner).
- **xcl reference:** none (xcl has no guarded views); new work.
- **Acceptance:** a test guards 64 neighbouring blocks of a mapped view,
  calls `writing` over all of them and sees one protection change (counter).
  The existing three watch tests pass.
- **Depends on:** F2 (to show the gain).

### F6. A journal of written physical pages for the GPU side (M)
- **Goal:** section 6.3 point 4. Runtime writes into physical pages note
  their page numbers. Physical-page textures consult the journal instead of
  hashing their memory at every submission.
- **Files:** `crates/xtp-hle/src/modules/xmem.rs` (noting writes),
  `crates/xtp-gpu/src/watch.rs` (the journal API),
  `crates/xtp-gpu/src/texture.rs` (using it; GPU area).
- **xcl reference:** `hle/d3d12x/title_textures.rs` (textures re-uploaded
  from written ranges, ranges closer than 1 MiB merged, first upload
  deferred to the next submission, uploads batched).
- **Acceptance:** a test (no GPU needed for the decision part) creates a
  texture over physical pages, writes through `write_physical` and sees it
  marked changed once, then unchanged with no hashing (hash counter
  unchanged) when nothing was written.
- **Depends on:** F3. Coordinate with the GPU report.

### F7. Take first-touch faults off the request path (S, only if F2 shows them)
- **Goal:** slices committed by F3 are touched on a background thread
  after allocation.
- **Files:** `crates/xtp-hle/src/modules/xmem.rs`.
- **xcl reference:** none.
- **Acceptance:** F2's destination time per MB drops in the F3 benchmark.
  Commit charge is unchanged (it is taken at commit anyway).
- **Depends on:** F3, F2.

### F8. Snapshot page arrays at enqueue, read late entries again (S)
- **Goal:** copy the destination page array when the request is enqueued.
  An entry that is 0 there is read again from the title's array at
  execution.
- **Files:** `crates/xtp-hle/src/modules/dstorage.rs` (`Request`,
  `enqueue_request`, `into_destination`).
- **xcl reference:** `hle/dstorage.rs`: `snapshot_page_array`, the late
  re-read in `execute`; test `a_destination_page_filled_in_after_enqueue`.
- **Acceptance:** two tests. An array on a frame that is overwritten after
  enqueue still lands at the snapshot's pages. An entry that is 0 at enqueue
  and filled before submission lands at the filled page.
- **Depends on:** none.

### F9. DirectStorage queue slots: signal and cancel (S)
- **Goal:** queue slot 0x28 (EnqueueSignal: set an `ID3D12Fence` from the
  CPU once earlier requests finish, holding a reference) and 0x38
  (CancelRequestsWithTag(mask, value): requests enqueued before the call
  whose `tag & mask == value` and that have not started are skipped and
  record no error; requests enqueued after it run).
- **Files:** `crates/xtp-hle/src/modules/dstorage.rs` (`QUEUE_VTABLE`,
  `Command`, `Lane`).
- **xcl reference:** `hle/dstorage.rs`: `queue_vtbl`, `FenceRef`,
  `CancelRule`; test `cancel_pending_and_submitted_requests_by_tag`.
- **Acceptance:** tests: a signal after two reads fires after both; a
  cancel skips only matching reads enqueued before it, their destinations
  stay untouched and the next status is a success; a closed queue releases
  its fence references.
- **Depends on:** none.

### F10. Error event and record, query, sources in physical pages (S)
- **Goal:** queue slots 0x48 (GetErrorEvent: a duplicated event handle),
  0x50 (RetrieveErrorRecord: 0x290 bytes, clears the error, resets the
  event), 0x58 (Query: description, free slots, requests until auto-submit);
  file slots Close and GetHandle; option bit 23 (source in physical pages);
  validation failures with the console's DirectStorage error codes.
- **Files:** `crates/xtp-hle/src/modules/dstorage.rs`.
- **xcl reference:** `hle/dstorage.rs`: `queue_vtbl`, `file_vtbl`,
  `execute`; tests `error_record_resets_after_retrieval_and_event_is_duplicated`,
  `requests_are_checked_like_the_console`,
  `physical_page_destinations_and_sources`.
- **Acceptance:** one test per slot and the bit-23 source read.
- **Depends on:** F3 (sources read through the permanent views).

### F11. Pattern tables for the XOR swizzle modes (S)
- **Goal:** our own generator turns Mesa addrlib's `gfx10SwizzlePattern.h`
  (MIT) into Rust tables for the RB+ (GFX10.3) XOR modes of the 2D and 3D
  thick patterns. The generated file keeps addrlib's MIT notice, as
  `texture.rs` already does.
- **Files:** `xtask/src/gen_tiling.rs` (or a `cargo xtask gen-tiling`
  subcommand: tooling area, agree with it),
  `crates/xtp-formats/src/texture/patterns.rs` (generated).
- **xcl reference:** `scripts/gen_tiling.py` (which header, which rows: read
  for behaviour, written anew).
- **Acceptance:** generation is reproducible (a test or xtask check
  regenerates and compares). For every pipe configuration and element size,
  each pattern is a permutation of its block.
- **Depends on:** none.

### F12. Addressing and copies for the XOR modes (M)
- **Goal:** `Layout` takes a pipe configuration (Series X: 32 pipes, 8
  packers; Series S: 8 and 4) and a base swizzle. `offset`, `detile` and
  `tile` work for modes 16-27, with the x bits computed once per mip and the
  y and slice bits once per row. Contiguous runs stay where the mode allows
  them. `XG` and the GPU side pick the configuration from the console mode.
- **Files:** `crates/xtp-formats/src/texture.rs`,
  `crates/xtp-hle/src/modules/xg.rs`.
- **xcl reference:** `gpu/tiling.rs`: `PipeConfig`, `Layout::new`,
  `Chain::detile_level` (row and column tables);
  `gpu/texture.rs::console_pipes`; HANDOFF "Texture layout library" (rules:
  levels padded from mip 0 halved rounding up, Z modes of 1- and 2-byte
  elements have a 64 x 128 tail, XOR pipe bits from coordinates above the
  block and from the array slice).
- **Acceptance:** `addressable()` is true for the XOR modes;
  `addresses_are_a_bijection_and_copies_round_trip` extended to them; a test
  that Series X and Series S give different offsets for 64KB_R_X; an ignored
  benchmark of detile MB/s per mode, reported in the delivery.
- **Depends on:** F11.

### F13. Render-target metadata planes (M)
- **Goal:** XG layouts with CMASK (one-level render targets, after the
  texels, aligned to 0x2000) and HTILE (depth buffers: HTILE first, aligned
  to 64 KiB, then depth, then stencil, all in mode 24). Also the empty-plane
  cases (one-level BCn render target or depth target).
- **Files:** `crates/xtp-formats/src/texture.rs` (`Texture`, `Plane`),
  `crates/xtp-hle/src/modules/xg.rs`.
- **xcl reference:** `hle/xg.rs` (planes), HANDOFF "Planes"; test
  `render_target_and_depth_layouts_match` in `tests/xg_oracle.rs`.
- **Acceptance:** unit tests on each placement rule; F14's oracle when
  available.
- **Depends on:** F12.

### F14. Oracle and benchmarks (M)
- **Goal:** (a) an opt-in test that loads the console layout library of the
  user's configured runtime through the XTP loader (`XTP_SAMPLE_XG`, skipped when unset) and compares
  layouts, offsets and planes over a generated grid of descriptions;
  (b) an ignored benchmark of `miniz_oxide` inflate as `dstorage` calls it.
- **Files:** `crates/xtp-hle/tests/xg_oracle.rs` (new),
  `crates/xtp-hle/src/modules/dstorage.rs` (benchmark test).
- **xcl reference:** `tests/xg_oracle.rs` (approach: the console library
  answers the same questions as ours).
- **Acceptance:** with the variable unset the test passes without running
  anything; with it, the counts of matching descriptions are printed and
  any mismatch fails. The benchmark prints MB/s.
- **Depends on:** F12, F13 for full coverage. The loader must map a console
  DLL outside a title run (loader report).

### F15. Cache limits and single flight (S)
- **Goal:** `xtp-pipeline` `Cache` gets a size limit with pruning of the
  oldest entries, statistics (entries, bytes, hits), and single flight: a
  key converted by one thread is waited for, not converted again by another.
- **Files:** `crates/xtp-pipeline/src/cache.rs`, `src/lib.rs`.
- **xcl reference:** DXIL cache behaviour (HANDOFF #8: one compile per
  shader across threads and processes, cached blobs validated, atomic
  writes).
- **Acceptance:** tests: 8 threads asking for one key convert once; pruning
  keeps the cache under its limit and never removes an entry being read; the
  existing four tests pass.
- **Depends on:** none. The shader cache's key (BLAKE3 of the executable)
  is the shader report's to change.

### F16. Read-only game folder (S, hand to the HLE report)
- **Goal:** writes to the game drive go to an overlay under the data folder.
  Reads prefer the overlay. Deletions are recorded, never applied.
- **Files:** `crates/xtp-hle/src/modules/files.rs`.
- **xcl reference:** `vfs.rs` (`Decision`, overlay, whiteouts).
- **Acceptance:** a test opens a game file for writing and finds the
  original unchanged and the overlay holding the write.
- **Depends on:** none. Listed here because a modified title folder would make
  every later measurement on it suspect.

### F17. Bounded RPF9 probe as a library (S, optional)
- **Goal:** `xtp-game::rpf::probe(reader)`: counts RPF9 headers at 2 KiB
  steps, notes truncated ones and whether TOC areas repeat,
  with bounded memory. For diagnostics and for locating shader records in
  large archives without reading them whole.
- **Files:** `crates/xtp-game/src/rpf.rs` (new), `src/lib.rs` (`mod` line).
- **xcl reference:** `rpf_probe.rs::inspect`.
- **Acceptance:** synthetic archives (made by the test) for: root header,
  nested headers, truncated header, identical and different TOC areas.
- **Depends on:** none.

Order: F1, F2, F3, F4, F8, F5, F6, F7, F11, F12, F13, F14, F9, F10, F15,
F16, F17. F1, F2, F8, F11, F15, F16 and F17 have no dependencies and can run
in parallel.

## 8. Risks

* **F3 changes memory the title aliases.** A permanent view adds one more
  alias of each section. Wrong commit tracking shows up as access
  violations in DirectStorage workers or zero pages in textures.
  Mitigation: F3's aliasing test, and a loading run before delivery.
* **Address space.** One permanent view per allocation doubles the address
  space reserved for physical pages (6.1 GiB for BTP 6, per
  `xmem.rs`). That is trivial in 128 TB of address space, and the commit
  charge is unchanged.
* **Wine.** `xmem` commits sections whole under Wine (`on_wine`). F3 must
  keep that path.
* **The README's diagnosis is unmeasured.** If F2 shows reads or decoding
  dominate, F3-F7 still remove waste, but the gain will be smaller. Then
  look at queue depth and unbuffered reads first.
* **BC7 fix untested on retail data in XTP.** Without the person's
  reference chunks, only synthetic tests cover F1.
* **XOR modes are only checked against an oracle.** Without the console
  layout library, F12-F13 rest on addrlib and
  xcl's described rules. A pipe configuration mistake scrambles CPU copies
  of render targets only.
* **Clean-room discipline.** Workers who read xcl must not paste from it.
  The backlog names behaviour; reviewers should reject code that mirrors
  xcl's structure line by line.

## 9. Licence notes

* **xcl is GPL-2.0-only. XTP is GPL-3.0-or-later.** GPL-2.0-only code
  cannot be combined into a GPL-3.0 work, so nothing of xcl is copied: no
  code, no generated tables, no generator scripts, no test fixtures
  (`tests/data/bcpack`). Facts learnt from it (vtable slots, request
  layout, option bits, error codes, pipe counts, placement rules) are
  interoperability facts and are written anew, as the asset policy allows
  ("small interoperability constants we write").
* **BCPack.** xcl's decoder and its `tables.rs` derive from VIctory (used
  there under GPL-2.0-only with the author's permission). XTP takes the
  decoder and its fixes from upstream VIctory (GPL-3.0), never from xcl.
  `bcpack-tables.bin` stays the asset policy's one exception
  (`xtask/src/policy.rs`); the repository licence does not cover it. xcl's
  THIRD_PARTY says its 128 rANS models are those of Microsoft's
  `silicon-rans` sample (MIT). If confirmed, `NOTICE.md` should carry that
  MIT notice for the table file.
* **addrlib (Mesa, MIT).** `texture.rs` already carries the notice. F11's
  generated tables come from the same MIT source through our own generator
  and keep the notice. xcl's `tiling_tables.rs` is not used even though it
  derives from MIT data: its generator and layout are GPL-2.0-only work.
* **Crates.** `miniz_oxide` (MIT/Zlib/Apache-2.0) and `blake3`
  (CC0/Apache-2.0) are compatible with GPL-3.0.
* **Game data.** Measurements here read a title folder in place. Opt-in tests
  (`reference_chunks`, F14's oracle) take paths from environment variables
  and assert properties, never snapshots (asset policy rule 4).

## 10. Open questions

1. Does upstream VIctory's PR #658 change anything beyond the BC7 entry
   layout (decoder side)? Merge it whole or re-implement F1?
2. Should the `silicon-rans` MIT notice go into `NOTICE.md` now (the person
   decides on attribution of the table file)?
3. Which swizzle modes does the title actually use for CPU-touched
   textures? A one-off log of modes seen in `XGCreateTextureComputer` and
   placed resources would set F12's priority.
4. Is `XGSuggestSwizzleMode`'s rule (older library function) still right for
   streamed textures? `xg.rs` flags it as a shortcut.
5. Does the title ever pass a page array on its stack (F8's risk), or always
   from its own tables? A debug check of the array address against the
   thread's stack would answer it.
6. F16 belongs to the HLE report; does its owner take it, or this backlog?
