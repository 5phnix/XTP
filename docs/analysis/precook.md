# The precook after a translator change: where the time goes, and what a narrow change costs

`xtp precook` runs before every PC session (`cargo xtask play`). After a
translator change it made every session wait 13 to 23 minutes before the
title started: about 26,500 records of the shader pack translated again,
then about 80,800 kept graphics and mesh pipelines and 10,100 derived ones
made into the host pipeline library. This page says where that time went
(the PC's logs of 2026-11-25), what changed in the precook (task 2cf06494,
resuming precook4c's work for task 13245ce1), and what a translator change
that alters one program costs now, measured. Logs and caches are under
`E:\xtp-data\` and on the L4 VM's `D:\xtp-data\`, none in the repository.

In short (§3): on the L4, a translator change that alters one program now
costs 682 s, of which 661 s is translating the pack again. The 92,915
pipelines whose translations did not change cost 16 s in all (6 s of it
reading their descriptions) and no `LoadPipeline`. The two broad changes
before it cost 2,331 s and 2,543 s. On the PC, whose translation of the
pack takes about 3 minutes, such a change should cost about 3 to 3.5
minutes instead of 13 to 23. Since §4, the library keeps one generation
per translator, so a switch back to a branch whose translator was
precooked before finds its pipelines held instead of making some 90,000
again.

## 1. Where the time went (PC, 2026-11-25)

Every PC session's precook writes its report into the run's `play.out.txt`
(`E:\xtp-data\runs\<run>\`). The precooks of the day that did a whole pass,
on 32 threads:

| run | translated | compute: made / from the library | graphics: made / from the library | total |
|---|---|---|---|---|
| `stall-imagecheck-1` | 25,554 | 2,465 / 65 | 90,037 / 252 (717 s) | 1,357 s |
| `stall-lightterms-1` | 25,594 (191 s) | 1,993 / 537 (465 s) | 37,254 / 53,345 (662 s) | 1,341 s |
| `stall-r9e5-522e976e-after1` | 0 (from the cache) | 0 / 2,327 | 33,870 / 56,418 (350 s) | 819 s |
| `stall-stride4b-1` | 25,597 (189 s) | 3 / 2,530 (47 s) | 34,179 / 56,420 (641 s) | 894 s |
| `stall-lightsrc-2` | 25,393 (162 s) | 2,271 / 60 (250 s) | 90,354 / 245 (372 s) | 792 s |

- **The pipelines made cost the most.** A compute pipeline made takes
  seconds of the driver's compiler: 2,000 to 2,500 of them took 250 to
  465 s. Of the graphics ones, about 60,000 are done in the first minute
  (loads, and compiles the driver's own cache answers), and the remaining
  30,000 or so take 5 to 10 minutes.
- **A full translation of the pack takes 160 to 190 s** on 32 threads. Its
  progress lines show no long tail (the last 14 records took 10 s): it is
  work, not waiting for a few large programs.
- **Most of the day's translator changes altered most programs.** A
  pipeline keeps its library entry when its translated shaders, root
  signature and state are byte for byte the same (its name hashes them).
  `stall-imagecheck-1` and `stall-lightsrc-2` found 252 and 245 of some
  90,000 in the library: those changes reached nearly every program. Their
  pipelines cannot be kept; only the driver's compile time can shorten that
  case.
- **The library forgot the translator before it.** Past its bound
  (`LIBRARY_REBUILD_BYTES`, 2 GiB), a pass after a translator change
  compacts the library into a new generation that holds only what that pass
  asked for. Sessions alternating between two branches whose translators
  differ (an A/B pair) then remade the other branch's pipelines at every
  switch: `stall-r9e5-522e976e-after1` translated nothing (its translations
  were in the cache) and still made 33,870 pipelines in 819 s. The library
  now keeps one generation per translator (§4).
- **A change that alters a handful of programs** cost, before this task,
  the full translation (about 3 minutes on the PC) and one `LoadPipeline`
  per pipeline: about 2,500 compute ones (47 s in `stall-stride4b-1`) and
  about 90,000 graphics ones, with the few pipelines of the changed programs
  made.

## 2. What changed

- **Translations: the current translator, always (option B of task
  13245ce1).** The shader cache is keyed by the translator's sources and
  settings, so after a change every container is translated again. Nothing
  older is ever served. Each translation made is compared with the entry
  the translator of the last whole pass kept for the same container: the
  report's `compared` line counts those that are the same, changed, or not
  kept before, with the first changed containers' hashes. A per-program
  fingerprint of the translator code a translation ran (option A) was
  rejected: code changes a translation without running in it (constants,
  types, inlined generics, tables), so no such fingerprint can be proven.
- **Pipelines: by name, without `LoadPipeline`.** A pipeline's name in the
  library hashes its translated shaders, root signature and state
  (`pipeline::library::name`), so a pipeline whose translations did not
  change has the name it had. Every save writes the names the library holds
  beside it (`host-pipelines-<id>.names` since §4, tied to the library file
  by its size and BLAKE3, `complete` or `partial`). The precook answers a pipeline whose
  name the library holds without loading it (`library::without_loading`):
  its work is to have each pipeline in the file, and a session loads it
  when it needs it. A library read without its names (an older file)
  learns them as they load, once.
- **Broken entries are repaired.** An entry whose name the library holds
  but which does not load for its description used to be compiled again in
  every session: `StorePipeline` refuses a name the library holds. The
  pipeline now goes under the next name (`name~1` to `name~3`), which the
  names file records and the next process loads.
- **Timers without pauses.** A precook pauses while a title runs. Its phase
  times, its total and the `XTP_PRECOOK_SECONDS` budget now leave the pauses
  out, and say them apart.
- **The shader cache's index** (c3da2f9) only sizes the cache: a read
  always checks its entry's file. The Codex review of c3da2f9 (task
  212e2594) found that it could stay stale. Changes:
  - `xtp precook` lists the folder again and rewrites the index before
    every pass, up to date or not (`Cache::reindex`, a listing: no entry
    file opened). A stale count lasts one session at most. Old staging files
    of cut writes go at the same time.
  - The index's snapshot carries its entry count (format 2): a snapshot cut
    short anywhere is damage, even between two records, and the cache then
    lists its folder.
  - A listing that could not read a folder writes no index, and a reindex
    in that case leaves the index as it was: an index without that folder's
    entries is never trusted afterwards.
  - What no format can tell without a listing is appends lost whole with
    the machine: the entry is found on disk at its first read, and the next
    precook's listing counts it.

## 3. Measured on the L4

The cloud VM's L4 (16 vCPU, NVIDIA L4, 32-lane waves), the title's pack
(`D:\game\shaders.rpf`, 26,503 records, 25,892 distinct containers), on a
cache of its own (`D:\xtp-data\precook-measure\cache`, `XTP_CACHE_DIR`),
`XTP_PRECOOK_SECONDS=100000` so that no kept pipeline is left for later.
Each run is `tools\cloud\run-remote.ps1 -Local -LocalRev <branch> -Cargo
'run --profile diag -p xtp --features diagnostics --locked -- precook
--no-wait D:\game\shaders.rpf'`, one after the other:

1. **Warm-up: main at b581dd1** (precook4c's run, translator key
   `bfad9419`), on a library sessions had left: everything translated, and
   nearly every pipeline made.
2. **B0: this branch** (d193e0f, on main ad3e308). Main's translator
   changed meanwhile (the fp32 denormal mode declared on every program's
   `main`, c52c0fd; the loop-limit counter's UAVs), and the library had
   passed its 2 GiB bound: a new generation. The `compared` line says the
   change reached 25,396 of the 25,892 translations.
3. **B1: this branch with a translator change that alters one program**
   (`measure/precook5-trivial-branch`: precook4c's `3df5ba6`, a static
   sampler added to the translation of the one vertex program of container
   digest `550ebac3…`, never merged).
4. **M1: main ad3e308 with the same change** (`measure/precook5-trivial-main`):
   what main costs today for such a change, on the library B1 left.

| run | translation | compute PSOs | graphics | total |
|---|---|---|---|---|
| warm-up | 633 s, 25,393 translated | 464 s: 1,444 made, 887 loaded | 1,222 s: 89,816 made, 784 loaded | 2,331 s |
| B0 | 644 s, 25,393 translated (496 the same, 25,396 changed) | 617 s: 2,269 made, 62 held | 1,269 s: 79,141 made, 11,458 held | 2,543 s |
| **B1** | **661 s**, 25,393 translated (25,891 the same, 1 changed) | **4.8 s**: 0 made, 16 loaded, 2,315 held | **5.1 s** (read in 6.0 s): 0 made, 0 loaded, 90,600 held | **682 s** |

"Held" is a pipeline whose name the library's names file holds, answered
without `LoadPipeline`.

- **B1, the change that alters one program, took 682 s, against 2,331 s
  and 2,543 s for the two broad ones.** 661 s of it is the translation
  of the whole pack, which option B keeps. The 92,915 pipelines whose
  translations did not change cost 16 s in all, 6 s of it reading their
  descriptions; before this task each was one `LoadPipeline`. The 16 loads are
  one per thread: the precook answers a held pipeline with one it made or
  loaded, so each thread's first pipeline is loaded. Nothing was added, so
  the library was not saved again (0.000 s).
- The one program the change altered has no kept or derived pipeline (the
  `compared` line names its container, `6f17771de44c1b0f`), so nothing was
  made. A change that reaches programs with pipelines makes those alone:
  `apps/xtp/tests/precook.rs` checks it on one thread: three compute
  programs, one changed; its pipeline is made, one other is loaded (the
  thread's first) and the third is held.
- **B0's broad change cost what it costs before this task:** every
  pipeline is made again. Its 11,458 held graphics pipelines are
  descriptions that give a pipeline made earlier in the same pass, which
  was one `LoadPipeline` each before.
- **On the PC** (32 threads), the same change should cost the PC's
  translation of the pack (162 to 191 s in the day's logs, §1) plus the
  same few seconds: about 3 to 3.5 minutes, against 13 to 23. Not measured
  there yet: the PC was running image sessions.

## 4. One library generation per translator, and what is left

### 4.1 Generations

The PC queue runs sessions of worker branches one after another, and
their translators differ. Before this change the host pipeline library was
one file (`host-pipelines.bin`). Past its 2 GiB bound, a pass after a
translator change compacted it to that translator's pipelines, and the
next switch back made the other branch's pipelines again (§1). The
library now keeps one generation per translation key
(`pipeline::library`, task be47721e, resuming libgen's work for task
bc28902e):

- **Files.** A generation is `host-pipelines-<id>.bin` and its names file
  `host-pipelines-<id>.names`, where `<id>` is the first 16 hexadecimal
  digits of the translation key (`shader_cache::host_key`: the
  translator's sources, its settings and the host's wave sizes, the key
  the shader cache uses). `generations.txt` lists them, most recently
  opened first, one line each: `<id> own`, `<id> from <id>` (seeded from
  another generation, or `legacy`), `<id> compacted <bytes>`.
- **A process opens its own generation only.** It settles it once, at its
  first use of the library: a key with no generation starts from the most
  recently opened one, its file hard-linked (copied where a link fails)
  and its names file copied, so that a change that alters few programs
  still finds every other pipeline by its name (§3's B1 case). The first
  generation made takes over the one file of before (a rename). The file a
  session reads into memory is no larger than before.
- **Eviction.** Past 4 generations (`GENERATIONS_KEPT`) or 8 GiB of files
  (`GENERATIONS_BYTES`), the least recently opened go, never the current
  one. A generation linked from another counts the shared file's bytes
  again: the budget errs on the safe side. Every write of a library file
  replaces it by a rename, so a generation linked from another never
  changes the other's file.
- **Compaction is per generation.** `xtp precook` compacts its
  generation (as before: a new library, what the pass asks for taken from
  the generation before, the rest made) when its file is past
  `LIBRARY_REBUILD_BYTES` and it holds another translator's pipelines
  (seeded from another generation and not compacted since), or has no
  compacted size past the bound (it crossed the bound since), or has
  doubled since that size. The size after a compaction is kept in the
  generations' list, not in the precook's note (version 5 drops it): the
  note is one for the whole cache, and says the last pass's key.
- **The precook's note and a generation started now.** A generation that
  had no file before this process (started empty, or seeded again after
  it was let go) is not the file the note's pass left, whatever its size:
  that pass is whole.
- **The report** says the generation opened: `library      generation
  <id> (its own | from <id> | compacted to <bytes> bytes) (<bytes> bytes)`.
- **Limits.**
  - The list is changed by one thread at a time in a process. Two
    processes of different translators on one cache at once could each
    let the other's generation go. Sessions and precooks on a machine run
    one at a time (the session lock).
  - A build older than this change still reads and writes
    `host-pipelines.bin`. Once generations exist, that file is left alone
    and not counted in the budget. Merging main ends it.

### 4.2 Measured: A, then B, then A again, on the L4

Checked end to end by `apps/xtp/tests/precook.rs`
(`precook_keeps_pipelines_across_translation_keys`, green on the L4 with
`XTP_REQUIRE_GPU=1`): three translation keys one after the other, then
the first again. The second key's generation starts from the first's
(`from <id>`) and makes no pipeline; the first key, back, opens its own
generation as it left it (`its own`) and makes none.

On the whole pack, the run under way is three precooks on a cache of its
own (`D:\xtp-data\precook-gen\cache` on the L4: the kept descriptions of
§3's cache, no library, `XTP_PRECOOK_SECONDS=100000`). A is this change.
B is the same plus a broad translator change, never merged: every
program's fp32 denormal mode left undeclared, which c52c0fd's change
showed reaches nearly every translation (§3, B0). The sequence is A, B,
then A again. Expected: the second A makes no pipeline and costs its
translation alone (11 minutes on the L4 at most, 3 on the PC, less with
its translations still in the shader cache), against some 40 minutes on
the L4 and 13 to 23 on the PC before. Its figures are not in this page
yet.

### 4.3 What is left

- **The full translation stays.** About 3 minutes on the PC, 11 on the
  L4, for any translator change. Less needs the translator to say which
  programs a change can reach (task 13245ce1's option A), which nothing
  can prove from its sources today (§2).
- **A broad change still costs its first pass.** A change to code every
  program runs (the denormal mode of c52c0fd, a common prologue) gives
  every program a new translation and every pipeline a new name: 13 to 23
  minutes on the PC, made by the driver's compiler, the first time that
  translator is precooked. Only the driver's own cache shortens that (its
  size on the PC decides how many of the day's pipelines it keeps). A
  switch back to a translator whose generation is kept no longer pays it.
- **Disk.** Up to 8 GiB of library generations in the cache folder,
  beside the shader cache.

## 5. The shader cache's index, cold

The Codex review asked for the first read of an existing index, cold. The
probe `cache::tests::probe_opening_a_real_cache` (xtp-pipeline, ignored by
default) reads the folder's own index unbuffered (`FILE_FLAG_NO_BUFFERING`:
from the disk, whatever the system cache holds) with
`XTP_CACHE_PROBE_WAY=cold`. On the PC's shader cache
(`E:\xtp-data\cache\shaders`, read only, release build), three runs: its
10,630,672 bytes from the disk in 0.006 to 0.010 s, and the whole open
(records decoded and applied: 111,630 entries, 8,589,877,367 bytes) in
0.068 to 0.078 s. Before the index, the same open read every entry's
header: 122.9 s for 25,925 entries cold (task 13245ce1).

## 6. The Codex review of d286ffd, and a session lost at its start

- **A start that waited on the shader cache (`stall-lightsrc-9`,
  2026-11-25).** The title's start waited more than 5 minutes in the
  cache's open, one `CreateFileW` per entry, cold. That session ran a
  branch older than the cache's index (c3da2f9): its precook printed
  `cache … (opened in 362.3 s)`, without the listing main's precook
  prints. On main an open reads the index file, and without one lists
  the folder (sizes and times from the listing, no entry opened). The
  test `cache::tests::opening_opens_no_entry_file` holds every entry
  open, shared with no one, so that any open of one fails: an open from
  the index and one from a listing both count every entry and its
  bytes. A branch older than c3da2f9 must merge main before its next
  session.
- **Point 1: "up to date" for other pipelines.** The note kept the
  translation key alone, so a setting that changes the pipelines and
  not the translations (`XTP_ROOT_SRV_DIRECT=1`: the root signatures)
  left it up to date, and each session made the new pipelines at their
  first use. The note (version 4) now also keeps those settings by their
  effective values (`PipelineSettings`: the root SRVs' binding, the
  wave64 mesh programs refused, the host's waves, shader model, depth
  bounds and mesh shaders), and other settings make a whole pass.
- **Point 2: an append lost to a snapshot.** A snapshot (a listing, or
  a rewrite of the index read) replaced the index file without the
  records other processes appended to it meanwhile. It now carries
  them: the records after the file as the listing or read found it, or
  after the snapshot that replaced it since. The snapshot holds the file
  against appends while it reads them and renames (a handle shared
  without writing; an append holds the file shared without deleting), so
  that none goes in between.
- **Point 3: an eviction on an incomplete listing.** A listing that
  missed a folder no longer evicts anything, at the open or at any
  access after, until a whole listing (`xtp precook` reindexes before
  each pass): the oldest entries may be in the folder it missed, and the
  counts may be an index's that lists entries gone since. The cache may
  pass its budget meanwhile.
- **Point 4: the mesh library entry an older build left.** The test of
  970/972 now also stores the mesh pipeline as builds before 284010f
  made it (its stream without the primitive topology) and loads it with
  this build's stream, in this process and the next. It fails in strict
  mode (`XTP_REQUIRE_GPU`) on a host without mesh shaders, and whenever
  the child process gives no answer.
- **Point 5: the depth bounds in a pipeline's name.** The name hashed
  the depth bounds test the title asks for; the pipeline has it only
  where the host does. The name now hashes the state the host makes.
- **Point 6.** The loop limit's test changed the limit by halving it,
  which gives 1 again for `XTP_SHADER_LOOP_LIMIT=1`; it now flips its
  lowest bit, another value whatever the run's.
