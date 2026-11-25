# Console wave64 compute on 32-lane hosts

> From VIctory (GPL-3.0-or-later, like XTP): the plan of Marvisak/VIctory
> #233, #320 and #439 by r4tedx, with Peter Bonanni (whole-wave sections
> across the halves, backward permutes: commits a31c82e and ab7882d, which
> the GitHub account @Raxstreams offered as r4tedx/VIctory#1 and #2) and
> ifBars (deferred mask phis),
> ported into XTP with the code it describes. Numbers such as #233 below
> are VIctory pull requests, and the measurements (RTX 5070, Radeon iGPU,
> testers' title runs) are theirs unless XTP is named. What XTP has of it,
> and what it measured, is in [In XTP](#in-xtp).

Status: steps 1, 2, 4 and 5 are in. On NVIDIA, wave64 programs whose lanes
read only their own half run as two 32-lane waves. Groups of one console
wave also run in the console flow: masks read whole, lanes of the other
half, divergent branches and loops, LDS shared in the console's
instruction order, and (in groups of exactly 64 threads) whole-wave
reductions and prefix sums, of integers and floats. For multiple complete waves, one host lane can carry two
console lanes ([Groups of several console waves](#groups-of-several-console-waves)).
Partial final waves in that form and forward `ds_permute` remain open. Wave64 mesh programs
run on 32-lane hosts ([Mesh programs](#mesh-programs)).

## The problem

The console runs most of the title's compute programs in waves of 64
lanes. Per-lane code does not care how many lanes a host wave has, so most
of them translate without a wave size. A program that depends on the lane
layout declares the console's (`WaveSize(64)`, see `emit::lower`, where
`wave_size` is set):

- **console lanes**: it reads other lanes (`v_readlane`, `v_writelane`,
  DPP, `ds_swizzle`, `ds_(b)permute`, `v_permlane*`), counts with
  `v_mbcnt`, reads a lane mask as a number, or tests its own bit of a mask
  held as a number;
- **one wave sharing LDS**: the group is a single console wave and uses
  LDS, where the console compiler leaves out barriers because one wave runs
  in order.

D3D12 reports NVIDIA's wave range as 32..32, so these programs cannot run
there as they are. Since step 1 they stop at translation with the reason
(`emit::Target`) instead of being refused at pipeline creation.
`xtp translate --host-lanes 32-32` and `xtp shader-survey --host-lanes
32-32` count them.

## The form

One console wave of 64 runs as two host waves of 32 lanes ("halves") of the
same thread group. The shader declares `WaveSize(32)`; console lane
= console thread % 64, half = bit 5 of it.

**Lane assignment.** The relabelling in `Lower::thread_id` assumes each
host wave holds a wave-aligned run of flat thread ids: it takes the first
lane's flat id, rounded down to the declared wave size, plus the host lane.
XCL's notes report NVIDIA forming waves from non-consecutive thread indices
in some 3-D groups. On an RTX 5070 with this relabelling, every group
`emit::split_tests` tries is exact: 32, 64 and 128 threads in 1-D, and
4x4x4, 8x8x2, 2x4x8, 16x2x4, 8x8x4 and 4x8x8. If another driver breaks it,
the fallback is to take console threads from a groupshared counter: one lane
of each host wave adds 32 to it when the wave starts and shares the result,
and the wave's lanes take the next 32 console threads in lane order.

**What stays inside a half** needs nothing: DPP16 and DPP8 (rows of 16,
groups of 8), `ds_swizzle` (groups of 32 or 4), and `v_permlane16` /
`v_permlanex16` (the two rows of a 32-lane half; not lowered at all yet).
Their lowering computes the source lane from the host lane's place in its
row or group, so it is unchanged in the split form.

Step 4 (`split` in `Lower::new`) runs a program as two 32-lane waves when
the host has no 64-lane waves and the program:
- reads other lanes only through DPP and `ds_swizzle`, and has no
  `v_readfirstlane` (the console wave's first lane is not in the upper
  half; such programs take the console flow);
- reads no mask whole and tests no bit of a mask held as a number;
- has no whole-wave sections (their reductions would cover a half);
- uses no LDS (the console compiler may have left out barriers between
  lanes of one wave).

Anything else still stops with the step 1 reason. `mask_dword` and the wave
reduction lowering refuse the split form as a safety net. A select or a
carry on a mask with a part held as a number (a constant, dwords built from
scalars) is not among the tests above; `atom` sends such a program to the
console flow rather than test the host lane's bit (in the upper half, host
lane n is console lane 32 + n). `lane_bit` only asserts it in debug builds.
(A wave64 program that declares no wave size at all still runs in whatever
waves the host forms, as on `main`: its `v_readfirstlane` reads the host
wave's first lane.)

**What crosses halves** goes through an exchange in groupshared memory:

| operation | exchanged |
|---|---|
| a lane mask read whole (ballots: scalar compares, `s_bcnt1`, `s_ff1`, mask tests) | each half's 32-bit ballot |
| `v_mbcnt_lo` in the upper half | the lower half's ballot |
| `v_readlane`, `v_readfirstlane` (uniform lane) | the value of lane `l & 31` from the half that owns `l` |
| `v_writelane` to the other half | the value and lane |
| `ds_bpermute` | nothing: on GFX10 a wave64's backward permute gathers within each 32-lane half (AMD's RDNA 2 ISA reference, as cited by the lowering's contributor; not checked on console hardware), so it is an in-half read |
| `ds_permute` | open: check whether GFX10 wave64 crosses halves for it, then 32 words per half if it does |

A mask read only lane by lane (a `v_cndmask` selector, a carry-in) needs
just the lane's own half. Exchanging those anyway is correct but slow.

## Synchronisation: the console flow

Both halves must reach every exchange together. On the console they do,
because a wave runs every instruction whatever EXEC holds. Our per-lane
structurisation turns EXEC into per-lane branches, so a half with no lane
on a path would skip that path and never reach an exchange there.

An earlier version of this plan made only the regions with an exchange
uniform. It turned out cleaner to run the whole program as the console
does (step 5, `lower::CONSOLE_FLOW`):

- `structure::structure_console` builds the per-lane booleans without
  resolving mask joins from what arriving lanes know, since every lane
  arrives. Each join that differs between edges becomes a phi.
  `Structured::execs` gives the lane's EXEC bit at each vector
  instruction.
- The lowering guards every vector instruction with that EXEC bit.
  `v_readlane`, `v_writelane` and `v_readfirstlane` ignore EXEC, as on the
  console.
- Branches are the console's `s_cbranch_*`. EXEC and VCC tests, and the
  SCC of a mask operation, ask whether some lane of the console wave has
  the bit: each half's ballot, exchanged. Both halves then take every
  branch the same way, and loops run while any lane of the console wave
  does.
- Exchange (`Lower::exchange`): each half stores its dword to group-shared
  slot `half`, then a group barrier, both read both slots, and a second
  barrier. Every thread of the group reaches it because the group is one
  console wave whose halves take the same branches. A group of 32 threads
  or fewer has no upper half: its word is 0 and no barrier is needed.
- Masks read whole (`mask_dword`) are the exchanged ballots. Lane bits of
  masks held as numbers use the console lane. `v_readlane` and
  `v_readfirstlane` read the owning half's value. `v_writelane` and
  `v_mbcnt` count with the console lane.

`lower` tries the per-lane form first. A program that needs the console
flow stops with `CONSOLE_FLOW`, and `emit` lowers it again from the
console structure. Eligible: wave64 compute on a host without 64-lane
waves, at most 64 threads per group, no `ds_permute`, and whole-wave
sections only in a group of exactly 64 threads.

**LDS.** The console runs an instruction in all 64 lanes before the next,
so one wave shares LDS without `s_barrier`: a lane reads what a lane of the
other half wrote one instruction earlier. In the console flow both halves
reach every LDS access together, so a group barrier before each access puts
it after every earlier one. This also takes the programs that needed 64-lane
waves only for that (one console wave per group, LDS, no cross-lane read).
Two testers' counts on the title put those first: 120 of the 223 programs
VIctory #233 still stopped on a 32-lane host. On an RTX 5070 most LDS tests pass
without the barrier too (that card keeps the two waves in step, and a
branch on EXEC is an exchange, which is a barrier itself). One does not:
with a write under one EXEC mask and a read under another and no branch
between them, the lower half reads before the upper half writes every
time unless the barrier is there
(`lds_written_and_read_in_different_masked_blocks`).

**Whole-wave sections** (by Peter Bonanni, offered by @Raxstreams). A section stays
recognised as it is (`emit::wave`), and only what it leaves crosses the
halves, where the section is lowered: each half's reduction is exchanged
and the two are combined with the section's own operation (sum, and, or,
xor, minimum, maximum of integers); a prefix sum in the upper half adds the
lower half's total. Only in a group of exactly 64 threads, so both halves
are whole. Floats cross as their bits: minimum and maximum stay exact, and
a sum is each half's sum added last, one more order of additions next to
the console's and the host wave's own (the last bits may differ, as they
already may in one host wave). A title run on VIctory #439 put sections next: 24
of the 45 programs the stack still stopped.

What still stops says why: `console wave64 lanes in a group of more than 64
threads`, `ds_permute across the halves`, or `a whole-wave section` (in
other groups), each `on a host without 64-lane waves`.

### Groups of several console waves

The inexpensive per-lane paths run first, including constant lane reads
through group-shared memory where the barriers meet. Otherwise a group
of *k* complete console waves can declare `[numthreads(32*k,1,1)]` and
`WaveSize(32)`. Host wave *w*, lane *i* carries console flat thread ids
`64*w+i` and `64*w+i+32`. As in the existing lane assignment, the first
lane's aligned flat id identifies its wave in the physical 1-D group. The
original group dimensions decode both ids into x, y, z. Direct and indirect dispatch keep their original group counts.

One host wave has all the registers of one console wave. At entry, the
flat-id mapping covers every console thread once. Each SSA instruction
takes its operands from the corresponding half. Wave operations on the
two sets of registers produce the two words of a ballot or the two
candidate values for a lane read. Both halves can use either result at the
same instruction. This gives `mbcnt` the console lane number and `readfirstlane`
the value from the first set bit in the complete EXEC mask.

A console branch makes one decision using both halves. Both register sets
therefore take the same phi edge, even when the next console wave takes a
different branch or runs a different number of loop iterations. A predicated
store or atomic executes under each half's own guard. Joining its result
under that guard preserves the register in each inactive lane. For LDS,
both halves finish the access before the next instruction's shared-memory
fence. Only the program's group synchronisation (and the existing loop-limit
drain when needed) brings the physical waves together.

The paired form executes a whole-wave section instruction by instruction.
This covers the FP16 reduction with an opposite-row gather in
`b22a16a178976e1b`. Each FP16 addition rounds at its original step.

#### World-session refusals on 32-lane hosts

The 23 distinct container hashes in the 20261125-000534 log all use
`v_mbcnt_lo/hi`, previously excluded from the large-group form. The table
gives the other relevant instructions and control flow in their disassemblies.
A number counts static occurrences. "Mask" counts `s_bcnt1_i32_b64` and
`s_ff1_i32_b64`. The branch column counts branches to an earlier instruction.

| Container | Back branches | Read first lane | Mask | LDS barriers | Read lane | Write lane |
|---|---:|---:|---:|---:|---:|---:|
| `02384a60fda6f2d0` | 10 | 12 | 2 | 5 | 6 | 0 |
| `0c028454b58543f0` | 0 | 7 | 3 | 1 | 4 | 0 |
| `191ecaed66eea3b1` | 0 | 9 | 5 | 0 | 0 | 0 |
| `1d44a723bdebbf27` | 0 | 9 | 5 | 0 | 0 | 0 |
| `240cb516c1af0bc3` | 0 | 9 | 5 | 0 | 0 | 0 |
| `31d703207b51549a` | 0 | 9 | 7 | 0 | 0 | 0 |
| `466ec41d3f9d24c0` | 4 | 10 | 8 | 0 | 0 | 0 |
| `56f3bf2d9388e5a7` | 4 | 10 | 8 | 0 | 0 | 0 |
| `5c6224a6bc3a4066` | 0 | 0 | 2 | 1 | 2 | 0 |
| `65de113e23fa4fdd` | 0 | 7 | 5 | 0 | 0 | 0 |
| `804b342cb4b9d1e9` | 0 | 7 | 5 | 0 | 0 | 0 |
| `922e1d0ee1e19a85` | 2 | 19 | 16 | 0 | 4 | 0 |
| `a1bf269d5b19f025` | 6 | 3 | 0 | 10 | 2 | 0 |
| `ab5455fc6abb804f` | 0 | 9 | 7 | 0 | 0 | 0 |
| `abd7a22106898c66` | 4 | 14 | 8 | 0 | 0 | 0 |
| `b22a16a178976e1b` | 0 | 0 | 0 | 0 | 8 | 0 |
| `b4b0427232707598` | 4 | 15 | 8 | 0 | 0 | 0 |
| `b8f3f7979d6bbc50` | 4 | 2 | 2 | 3 | 0 | 0 |
| `be6ce5d849d51127` | 2 | 27 | 12 | 0 | 4 | 0 |
| `ce79e9eba7ae0348` | 4 | 15 | 8 | 0 | 20 | 20 |
| `d7db57731ada919b` | 4 | 14 | 12 | 0 | 0 | 0 |
| `eae3498e6553afa8` | 3 | 2 | 2 | 2 | 0 | 0 |
| `fa0bd10a35bfef3c` | 4 | 14 | 12 | 0 | 30 | 30 |

All named lane reads in this set use immediate lanes. In
`0c028454b58543f0` their rendezvous blocks and the group barrier do not
form a dominance chain. Four programs also reach an LDS write from an LDS
read without an intervening `s_barrier`: `02384a60fda6f2d0` at 0x608,
`a1bf269d5b19f025` at 0x1d4, `b8f3f7979d6bbc50` at 0x101c and
`eae3498e6553afa8` at 0xff8. The old mixed-access check rejects each
path regardless of whether the addresses alias.

On the L4, `xtp shader-survey --host-lanes 32-32` examined the same
26,698 programs at `1c61fa1` and `f992374`. Translations rose from
24,433 to 24,476, including a rise from 2,266 to 2,309 compute programs.
Matching by pack record offset and program index found all 43 former
large-group refusals among the gains. All 24,433 previous translations
retained their DXIL hashes. The 23 containers above each passed DXIL
validation and D3D12 pipeline creation in isolation. The other 20 newly
translated compute programs also passed DXIL validation.

The L4 ran all 43 `emit::split_tests` cases successfully with
`tools/cloud/run-remote.ps1 -GpuTests -Local -Packages xtp-shader -Filter split_tests`.
GPU readbacks covered different loop counts in neighboring console waves,
masked LDS atomics across those waves, numeric masks, and the FP16
whole-wave section. The crate test suite passed 309 tests with 36 ignored.

### The console flow on every host

The per-lane form is wrong for a program that reads or writes a named
lane (`v_readlane`, `v_writelane`) where its lanes branch differently
(VIctory #225). Those instructions ignore EXEC on the console and a wave runs a loop
while any lane does. Per lane, a lane that left a loop or skipped a branch
is not there to be read or written, and never sees scalars set inside.
Measured before the change: a loop lane t runs t times, reading lane L in
each iteration, had 12 of 32 lanes wrong (wave32, RTX 5070 and Radeon) and
24 of 64 (wave64, Radeon); a `v_writelane` to lane 40 inside a branch only
lanes below 20 take never wrote lane 40.

Such compute programs now ask for the console flow on every host (the
second case of `CONSOLE_FLOW`): a program with `v_readlane` or
`v_writelane`, at least one branch that is not uniform, no `ds_permute`
and no whole-wave sections. On a host that runs the console's wave size,
one host wave is the console wave, so ballots are the wave's own and nothing
is exchanged; any group size and LDS are fine. If the console flow cannot
lower the program, it keeps the per-lane form (`Parts::per_lane_only`), so
nothing that translated before is refused.

## Mesh programs

D3D12 gives a mesh shader no wave size to ask for. A console wave64 mesh
program therefore runs in whatever waves the host forms: 32 lanes on
NVIDIA, 4 on WARP. XTP lowers mesh code per lane and numbers its lanes from
the flat thread id (console lane = id % 64, console wave = id / 64). Mesh
code reads no other lane except within quads, and reads no mask as a
number. Its one dependence on whole console waves is `v_readfirstlane_b32`:
- a value proven the same in every lane of the console wave is each lane's
  own (`emit/lower/uniform.rs`);
- any other one, in a block every lane of the group runs, comes through
  group-shared memory between group barriers (`emit/lower/mesh.rs`);
- the rest is refused on a host without 64-lane waves.

Barriers kept in mesh programs with LDS must sit where the whole group runs
them, and a group of one console wave with LDS is refused. The runtime lets
every wave64 mesh program the translator made for the host's lanes run.
`XTP_WAVE64_MESH=refuse` gives back VIctory #578's refusal.

For BTP 6's pack, 8,725 of 9,282 mesh programs translate and run on
NVIDIA, against none before. The design, the measurements and what stays
refused are in [analysis/mesh-wave64.md](analysis/mesh-wave64.md).

## Testing

The differential tests need the console SDK's compiler, which we do not
assume. These tests are hand-assembled GFX10 programs instead (as in
`emit::tests`), run twice:

- in the native form on an adapter with 64-lane waves (AMD RDNA, such as
  the integrated GPU of Ryzen 7000 desktop CPUs), as the reference;
- in the split form on a 32-lane adapter (NVIDIA),

then compared buffer for buffer. `emit::host::Host::on(name)` and
`XTP_ADAPTER=<part of the adapter's name>` pick the adapter;
`emit::tests::each_adapter_gets_what_it_runs` checks every adapter in the
machine (on a Ryzen 7800X3D with an RTX 5070: the Radeon reports 32-64 and
takes `WaveSize(64)`, the NVIDIA reports 32-32).

## Steps

1. ~~Translate for the host's wave range; cache by it; `--host-lanes`.~~
2. ~~Adapter selection for tests and tools.~~
3. ~~Measure the title's programs by reason (`--host-lanes 32-32`), to
   order the later steps.~~ Testers did, and from VIctory `965ce30` on a
   local offline run does; in XTP, `xtp shader-survey --host-lanes 32-32`.
4. ~~In-half programs: console-lane programs whose cross-lane operations
   all stay inside a half, with no mask read whole.~~ No new lane assignment
   was needed (see above). A later widening is LDS in multi-wave groups,
   whose waves already meet at barriers.
5. ~~The console flow and the one-wave exchange (ballots, readlane,
   readfirstlane, writelane, mbcnt, divergent branches and loops).~~
6. ~~One-wave LDS sharing (a group barrier before each LDS access in the
   console flow).~~
7. ~~Complete multi-wave groups without LDS, including wave-local branches
   and loops.~~ One host wave carries one console wave. Dispatch counts stay
   the same.
8. ~~Complete multi-wave groups with LDS.~~ All console waves in the group
   share the allocation and rendezvous at its group barriers.

## In XTP

Ported from VIctory #223, #233, #320 and #439 (heads `714b818`, `d1fecaa`,
`0fa6af4`, `c153b21`), one commit each, then XTP's own review commits.

- **Translator** (`crates/xtp-shader`): everything above. `emit::translate`,
  `translate_program` and `emit` keep their signatures and translate for
  `Target::ANY`; `translate_for`, `translate_program_for` and `emit_for`
  take an `emit::Target`. `HostShader::wave_size` reads the wave size a
  translation requires from its `PSV0`.
- **Tools** (`apps/xtp`): `xtp translate --host-lanes MIN-MAX` (and
  `--pipelines` translates for the device's own range), `xtp translate
  --validate` adds the module's summary to validator failures, and `xtp
  shader-survey --host-lanes MIN-MAX` surveys for a host's waves.
- **Not yet: the runtime** (`crates/xtp-gpu`, ported on its own): reading
  the device's range (D3D12 options 1: `WaveLaneCountMin` and `Max` when
  `WaveOps`), translating pipelines with `translate_for` for it, and keying
  the shader cache by `Target::key()`. Until then the runtime translates
  for any host, so on NVIDIA these programs still declare `WaveSize(64)`.

**Measured** on the person's `shaders.rpf` (26,698 programs), every
translation checked by dxil.dll, 2026-11-24:

| | before the stack | after, any host or AMD (`32-64`) | after, NVIDIA (`32-32`) |
|---|---|---|---|
| programs translated | 22,485 | 22,508 | 22,464 |
| compute programs translated | 1,444 | 1,467 | 1,423 |
| compute requiring `WaveSize(64)` | 320 (see below) | 332 | 0 |
| console wave64 compute at `WaveSize(32)` | 0 | 0 | 288 (276 valid) |

- On NVIDIA, 288 of the 332 console wave64 programs that require 64-lane
  waves elsewhere translate at `WaveSize(32)`. 276 are valid; the other
  12 are refused at `WaveSize(64)` too, for a compute `sample` with
  implicit derivatives (not this stack's). 320 of the 332 translated
  before the stack, requiring `WaveSize(64)` then too (the stack does not
  change the wave size a translation for 64-lane waves declares; the old
  survey did not count it), which NVIDIA refuses at pipeline creation. 44
  still stop: 40 in groups of more than 64 threads (steps 7
  and 8), 4 with a whole-wave section in a group of another size.
- For any host, nothing that translated before is lost. 250 compute
  translations change (the console flow on every host) and are as valid
  as before (240 valid, 10 refused before and after); 23 more translate
  (12 valid; the other 11 have the same compute `sample` refusal).
- Tests: on the PC's RTX 4080 SUPER (two halves) and its Radeon iGPU
  (native wave64), every split and console-flow test was exact. Tests on
  that PC now run on WARP (`XTP_TEST_ADAPTER=warp`), where the tests that
  need hardware adapters skip.

**Changed in review** (the commits after the four ports): where the
console flow is not needed for a program to run, a program both forms
refuse reports the per-lane form's blocker (the survey's buckets); the
module summary allocates once per operation, not per call; this crate's
GPU tests follow `XTP_TEST_ADAPTER`. **Known risks, not changed:** the
halves branch alike only while their scalars agree, which holds for
everything the translator computes but not for a scalar load of memory
another group changes during the dispatch (then the halves' exchanges
would not pair); lanes outside EXEC still read LDS at their own address
in the console flow (an out-of-range read of group-shared memory is
undefined in D3D12 but does not fault, and its value is discarded); every
console branch costs a ballot and two group barriers, and every LDS access
one barrier.

## Credit

The approach of pairing two 32-lane waves through groupshared memory, and
the finding that NVIDIA forms waves from non-consecutive thread ids in some
3-D groups, come from XCL's design notes (`docs/wave64-on-wave32.md` in
github.com/ivall/xcl and its fork Bathyal). XCL is GPL-2.0-only. Nothing
here is copied from its code.
