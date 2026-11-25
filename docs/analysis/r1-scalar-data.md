# R1: the programs refused "a scalar memory read as data", sorted

Task r1smem (6776fa2d), 2026-11-25, phase 1 (analysis only). The title is
BTP 6, build 1.0.2.1; programs are named by hash only (the first 16 hex
digits of the BLAKE3 of the container, as the logs print it). Surveys,
probe output, extracted containers and disassembly stay under
`E:\xtp-data\` (on the PC) and `D:\xtp-data\` (on the L4 VM), none in the
repository. Times are local (UTC+1).

## 1. Summary

- **The class is empty on `main`.** The census (refusals.md, row 1) counted
  998 programs refused "a scalar memory read as data": 66 compute, 383
  mesh, 516 pixel, 33 vertex (the brief's "383 vertex, 33 mesh" swaps the
  census's MS and VS columns). In a survey of `main` after the landings of
  03:03 and 03:14 (section 2) **none** is refused this way: **732 translate**
  (MS 348, PS 336, VS 31, CS 17) and **266 stop at another blocker**. No
  program of the pack that translated before fails after (0 lost over the
  26,698). A PC world session at 04:06 logs this refusal for no program
  (0 of 1,846 refusal lines).
- **It was one pattern** (section 3): a buffer descriptor loaded with
  `s_load_dwordx4` and used by `s_buffer_load_dword*`, whose dwords the code
  also read as numbers or joined with numbers. `descriptor_data`
  (`emit/lower/smem.rs`) had arms for image and buffer uses only, and an
  `s_buffer_load` use fell to `_ => "a scalar memory read as data"`.
  rawfix's 6b3ef55 (port of VIctory #1762, on `main` with 5306537 at 03:03)
  added the `s_buffer_load` arm; meshdata (e512613, 03:14) landed in the
  same window. No address in the class came from memory: each one was
  traced to a root parameter plus an offset, so none of the brief's
  phase-2 tools (raw views over a pointer, a pointer from a root CBV as a
  root parameter, title memory by address) is needed for it.
- **The 266 left fall in four groups** (section 4). Two of them, **220
  programs**, are one mechanism: a descriptor (a sampler from the sampler
  heap, 179; a constant buffer from a descriptor table, 41) whose dwords
  only its accesses and joins read: the compiler reuses its registers for
  numbers after the descriptor's last access. `descriptor_data` counts a
  join's operand as a data read and refuses before `flow.rs`'s
  descriptor-word arm (ef52d24, beside meshdata's `dead_at_join`) can take
  the join, as it does for resource descriptors.
- **One rule would lower those 220** (section 5): where `descriptor_data`
  refuses today, a load whose dwords reach only joins, each of which also
  takes a value no descriptor load wrote, gets no host value for its dwords
  (`DESCRIPTOR_WORD`), as a load nothing reads does. A prototype of it,
  run program by program on a sample of 14 of the 220, emits DXIL for 12
  (7 of 8 in A, 5 of 6 in B); one moves to a lane-mask class and one did
  not finish in 90 s (section 5.3).
- **The other 46 belong elsewhere:** 12 mesh and vertex programs copy a
  buffer descriptor's dwords into VGPRs inside a divergent branch (a
  per-lane trace, W11's line), and 34 stop at lane-mask or wave64 classes
  (W1, W2).

## 2. Surveys and method

| Survey | Where | Translator | Translated | This class |
|---|---|---|---:|---:|
| `w6-before.json` | L4 VM, 02:51 | W6's survey before its change: the census's translator (the same counts) | 24,433 | 998 |
| `w4-main2.json` | L4 VM, 03:35 | `main` after 5306537 (03:03) and e512613 (03:14) | 25,441 | 0 |
| `w4-scoped.json` | L4 VM, 03:39 | the same plus W4's change (2967d20, reverted at 03:40) | 25,446 | 0 |

All three: `xtp shader-survey <pack> --host-lanes 32-32 --json`. The 998
records of `w6-before.json` are exactly the census survey's (the same 998
record and program pairs as `w64-d92-before.json`, which has the census
survey's bytes). `w4-main2.json` is W4's survey of `main` before its own
change: the VM does not record its commit; its time places it after both
landings of section 3, and the five programs W4's change adds in
`w4-scoped.json` are the five `v_cvt_f32_f64` ones, so `w4-main2.json`
does not hold W4's change.

Program by program, `w6-before.json` to `w4-main2.json`, the 998:

| Now | CS | MS | PS | VS | All |
|---|---:|---:|---:|---:|---:|
| translated | 17 | 348 | 336 | 31 | **732** |
| a descriptor read only as data | 33 | 10 | 146 | 2 | 191 |
| a constant buffer's descriptor read as data | 3 | 6 | 32 | | 41 |
| a lane mask read as data | 2 | 15 | 2 | | 19 |
| a lane mask holding lanes elsewhere read as data | 11 | | | | 11 |
| a first-lane read of a value lanes may hold differently (wave64 mesh group) | | 4 | | | 4 |
| **all** | 66 | 383 | 516 | 33 | **998** |

Over the whole pack, no program translated in `w6-before.json` fails in
`w4-main2.json`.

**The world session.** `E:\xtp-data\cache\play\20261125-030623` (04:06,
Series S; its log does not name its commit): 1,846 refusal lines, none
"a scalar memory read as data". Of the 998's hashes it logs 37 translated
and 54 refused: 38 "a descriptor read only as data", 8 "a constant
buffer's descriptor read as data", 8 lane masks; the groups of section 4
cost the image there, not only the pack's count.

**Sorting the 266.** A probe build of `main` (7b63628 plus a patch that is
not committed, `E:\xtp-data\r1smem\probe2.patch`) writes into each refusal
of `descriptor_data` the load's mnemonic and offset, its address (the root
parameter's kind, the offset, whether a computed term is added), the
used-resource entry of the slot, every instruction and join that reads its
dwords, and the effect where the refusal surfaces. It ran as
`xtp translate --list --host-lanes 32-32` over the 266 containers,
extracted from the pack on the L4 VM (391 s, two jobs, below-normal
priority; no translation on the PC). Twelve representatives were
disassembled there with `xtp shader-info --disasm` (three each of groups A
and B, two of C, four of the 732); the listings stay in
`E:\xtp-data\r1smem\dis`.

## 3. What the class was

Before 6b3ef55, `descriptor_data` asked how the program uses a descriptor
load whose dwords are also read as data (a twin load included) and matched
the using instruction: an image (sizes from `getDimensions`), a buffer
(the record count), and `_ => "a scalar memory read as data"`. A
`Role::Resource` descriptor is used by an image, a buffer or an
`s_buffer_*` instruction (`resource.rs`, `trace`), so the arm caught
exactly the `s_buffer_load` uses: the console compiler's uniform reads of a
raw or constant buffer through a descriptor it loads from a table entry or
the heap, where the program also read the descriptor's dwords (the record
count, or its registers joined with numbers).

6b3ef55 added the `s_buffer_load` arm: an SRV or UAV entry, or a slot no
entry describes, gives the record count from the raw view `smem` reads it
through (`record_count_view`); a CBV entry refuses "a constant buffer's
descriptor read as data" (no D3D12 query gives a constant buffer's size).
Its commit message names this refusal. That accounts for the 732 and for
the 41 of group B, which now stop at the CBV arm. meshdata's `dead_at_join`
and `lane_copies` (e512613) landed eleven minutes later and are in the same
survey; how many of the 732 needed them is not measured (a survey of
5306537 alone would tell, and changes nothing below).

No address of the class came from memory. The refusal is the lowering's
(the survey's `emit:` step), which runs after `resource.rs` has traced
every scalar address of the program to a root parameter; an address it
cannot trace stops the program at the `resources:` step instead ("an
address not from a root parameter", 64 programs, another class). For the
266, section 4 gives the parameter: the sampler heap's user-data dword or
a descriptor table, plus a constant (two plus a computed term).

## 4. The 266 that remain

| Group | Programs | Address | Used for | Refusal now | Representatives |
|---|---:|---|---|---|---|
| **A** sampler joined after its last use | 179: PS 146, CS 33 | the sampler-heap root parameter (a user-data dword) plus a constant (177: three constants, one of them in 167 programs) or a computed term (2) | the sampler of `image_sample` / `image_sample_l`; then only joins | a descriptor read only as data | `00ae82eaf8b736cf`, `072f5fe96efd27e1` (CS), `257fb752e6b64ce4` (computed offset) |
| **B** constant buffer joined after its last use | 41: PS 32, MS 6, CS 3 | a descriptor-table entry the used-resource list gives as a CBV (nine entries; the two commonest in 11 and 8 programs) | `s_buffer_load_dword*`; then only joins | a constant buffer's descriptor read as data | `01d73927f08aed16`, `578027faffb06047`, `077869cd9b1456ee` (CS) |
| **C** buffer descriptor copied into VGPRs | 12: MS 10, VS 2 | the sampler-heap root parameter plus one of two constants (6 programs each) | copied with `v_mov_b32` into VGPRs inside an EXEC-masked branch, then the descriptor of `buffer_load … idxen` after it | a descriptor read only as data, at an export | `02523c2a5d6a788d` (VS), `0914e7ab5623cd47` (MS) |
| **D** lane masks and wave64 | 34: MS 19, CS 13, PS 2 | (no scalar load at the refusal) | | a lane mask read as data (19), lanes elsewhere (11), a wave64 mesh group's first-lane read (4) | `133edc77b7d5e27b`, `0ae69044bfd15035`, `425c36bb6923788d` |

**A.** The program loads a sampler (four dwords) from the sampler heap
at a constant offset into four SGPRs and samples with it. After that last
access the compiler reuses the first two of those SGPRs: an EXEC save in
the waterfall loops further on, and an `s_buffer_load_dword` result on
another path. Where the paths meet, the program compares the register
with zero behind a flag it keeps in a VGPR, so it reads the joined value
only where the number was written. In all 179 the readers the probe
lists (up to six) are samples and joins. A sampler is a `Role::Sampler`
descriptor, so `descriptor_data` finds no resource use and refuses; the
refusal surfaces at the branch on the joined register.

**B.** The same with a constant buffer: an `s_load_dwordx4` from a
descriptor-table entry the used-resource list gives as a CBV, the
descriptor of one or more `s_buffer_load_dword*`, after which its SGPRs
hold other descriptors (an image's `s_load_dwordx8`, a sampler built from
constants) or numbers computed in a loop whose entry brings the CBV's
dwords. In 37 the probe lists joins among the readers; in the other 4
its list stops at the first six readers, all `s_buffer_load`, so their
joins are not shown (for one of them, `578027faffb06047`, the prototype of
section 5.3 found no instruction reading the CBV's dwords).

**C.** Not the join mechanism: a buffer descriptor's dwords go into VGPRs
with `v_mov_b32` inside an EXEC-masked branch, lanes outside the branch
keep the VGPRs' old values, and the refusal surfaces at an export; the
probe sees the copies as the readers, not what reads the copies. It needs a
per-lane trace of the copies (meshdata's `lane_copies` says when VGPR
copies of one load's dwords are that load, for a waterfall loop's
descriptor), in `resource.rs`: W11's line, not this task's files.

**D.** Lane-mask and wave64 refusals with no scalar memory read at the
refusing instruction: W1 (masks) and W2 (wave64), whose classes these are.

## 5. Proposed lowering (phase 2)

### 5.1 The rule

In `descriptor_data`, at the two ends that refuse today (no resource use
with the "a descriptor read only as data" refusal; a CBV use with "a
constant buffer's descriptor read as data"), before refusing: if

1. no instruction other than a descriptor access reads the load's dwords,
2. every join that takes one of them also takes a value no descriptor load
   wrote, and
3. no access takes such a join as its descriptor,

the dwords have no host value: `Val::Missing(DESCRIPTOR_WORD)`, as for a
load whose dwords nothing reads. `flow.rs` already lowers such a dword at
a data join as undefined on its edge (the descriptor-word arm beside
`dead_at_join`).

- **Why it is exact.** The console compiler gives a register to the
  descriptor while the descriptor is live, then to a number. A join of the
  two is two variables in one register: where the program reads the joined
  value, the number is the variable it reads (on the descriptor's edge it
  reads nothing it defined). Conditions 1 and 2 find that statically: the
  descriptor's dwords are never read as numbers, and each join mixes them
  with something that is not a descriptor. The shortcut is the existing
  descriptor-word rule's: wrong only if the program read the descriptor's
  dword as a number after the join on the descriptor's edge, which register
  allocation does not produce.
- **Descriptors chosen by control flow keep today's path** (condition 3):
  a sampler chosen between a heap entry and one built from constants is a
  join the access takes; and a join of descriptors only, read as a number,
  fails condition 2 and stays refused.
- **No translated program changes**: the rule only replaces two refusals,
  so translations of today are untouched ("0 lost" by construction, to be
  checked by the surveys of 5.4).

### 5.2 Files

- `crates/xtp-shader/src/emit/lower/smem_data.rs` (new, this task): the
  test "read only by joins that also take data" over the load's dwords
  (instructions outside descriptor accesses, phis and their arguments,
  `descriptor_dword` for the other arguments).
- `crates/xtp-shader/src/emit/lower/smem.rs`: two calls at the refusing
  ends of `descriptor_data`. **Held by w7skin (099ee32d)**; the hook is a
  few lines.
- `crates/xtp-shader/src/emit/lower/mod.rs`: the `mod smem_data;` line
  (mechanical).
- `crates/xtp-shader/src/emit/r1_tests.rs` (new, this task) and its `mod`
  line in `emit/mod.rs` (mechanical). No `resource.rs` change.

### 5.3 Prototype

A throw-away version of the rule (`E:\xtp-data\r1smem\probe3.patch`,
behind `XTP_R1_JOIN=1`, not committed) tested conditions 1 and 2 at the top
of `descriptor_data`, before the use search, and without condition 3: an
upper bound of what the rule does, not the rule. It ran on the L4 VM,
which another worker's session kept at 100 % CPU, at below-normal priority:

- **over the 266 in one run** (`translate --list`, two jobs) it did not
  finish: the remote command ended after 1,623 s with exit code -1 and no
  program's line written;
- **program by program**, 90 s at most each, on 14 of groups A and B (eight
  of A, six of B: the first in hash order of each address kind in the
  probe's list, and `578027faffb06047`):

| Group | Sample | DXIL emitted | Moved to another class | Not finished in 90 s |
|---|---:|---:|---:|---:|
| A | 8 (PS) | 7 | | 1 (`00ae82eaf8b736cf`) |
| B | 6 (PS 4, CS 2) | 5 (`578027faffb06047` among them) | 1 CS: a lane mask holding lanes elsewhere read as data | |

The DXIL was not validated in this run (no `--validate`), and the times
are those of a loaded VM (`01d73927f08aed16` took 52 s). What phase 2 must
find first: why `00ae82eaf8b736cf` takes more than 90 s with the rule
(refused within the probe's run without it), since it is the likeliest
reason the full run did not finish. In `578027faffb06047`, one of the four
programs of B whose readers the probe's list cut short, the prototype's
test found no instruction reading the CBV's dwords.

### 5.4 Tests and measurement

- `emit::r1_tests` (names start with `smem_`, so the task's check runs
  them): a compute program that samples with a sampler loaded from the
  sampler heap, overwrites its first SGPR with an `s_buffer_load` on one
  branch and tests it after the join: translated, validated, and on WARP
  the number's edge gives the number; the same with a CBV table entry used
  by `s_buffer_load`; a join of two samplers read as a number, and a CBV
  dword read by an instruction, still refused with their reasons today.
- Before and after on the same machine (the farm's surveys, task
  3efcaa74, or the L4): the groups' programs translate, `--validate`
  passes on them, nothing else changes.

## 6. How this was made

- Surveys fetched from the L4 VM with `Receive-XtpVmFile`
  (`tools/cloud/common.ps1`); compared program by program with a script
  under `E:\xtp-data\r1smem\scripts` (`moves.py`, `lists.py`, `agg.py`).
- The probe and the prototype: patches `probe2.patch`, `probe3.patch`
  under `E:\xtp-data\r1smem`, built in release on the build farm
  (`cargo-slot.ps1 build --release -p xtp --locked`), run on the VM over
  the extracted containers (`E:\xtp-data\r1smem\vm\r1-probe.ps1`,
  `r1-join.ps1`, `r1-sample.ps1`). The sources were restored after each
  build; nothing of them is in this commit.
- The world session's log: `E:\xtp-data\cache\play\20261125-030623\run.log`,
  read only (`session.py`).
