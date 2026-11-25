# Shader compiler: VIctory vs xcl (analysis)

What XTP keeps of VIctory's shader translator (console RDNA2 machine code →
host DXIL) and what it should rewrite after xcl's approach, with an ordered
backlog. xcl (GPL-2.0-only) is a reference for **behaviour only**: nothing of
its code may enter XTP (GPL-3.0-or-later). Identifiers below name where a
behaviour lives in xcl; they are not code to port.

Crate names use the new `xtp-*` names. At the time of writing the worktree
still had `crates/victory-shader`; read `crates/xtp-shader` for it.

## 1. Summary

* **The two translators do not have the same shape.** xcl goes RDNA2 → HLSL
  text → DXC → DXIL. VIctory (and therefore XTP) goes RDNA2 → SSA IR →
  **DXIL bitcode it writes itself**, with no HLSL and no DXC. This is the
  main reason to keep VIctory's pipeline: the whole retail pack turns into
  final DXIL in 45 s here, while xcl's DXC step alone takes 0.45 s per
  program (median) after it has written 9.2 GB of HLSL.
* **xcl covers more programs, by a different execution model.** On the same
  pack (the person's `shaders.rpf`, 26 503 records, 26 698 programs) xcl
  produces HLSL with no unsupported instruction for 26 698 programs (100 %);
  VIctory translates 22 485 (84.2 %). 4 213 programs fail in VIctory, and
  section 4 sorts them by cause. Most of them are not missing instructions:
  they are lane masks and memory reads that VIctory's lane-local model cannot
  express, which xcl's wave-uniform model (masks as ballots, all memory as
  one flat buffer) gets for free.
* **xcl's "100 %" means "no unsupported mnemonic", not "correct".** Two
  places were seen where xcl appears to approximate silently where VIctory
  refuses (section 5.3). XTP keeps VIctory's fail-closed rule: a construct
  that cannot be translated exactly stops the program.
* **Verdict:** keep VIctory's decoder, IR, structurisation, resource
  tracking, DXIL writer and cache. Port xcl's *ideas* where VIctory's model
  is the blocker — masks as numbers, a flat-memory fallback for data reads,
  merged ES+GS, wave64 on 32-lane GPUs — as new lowering paths inside
  `xtp-shader`, each behind the existing lane-local path.

## 2. Scope and files

In scope: the console shader container, the GFX10 decoder, the translation
to host shaders, the DXIL/DXC step, the translation cache as far as the
translator keys it, and the tests of all that. Out of scope: the rest of the
GPU layer (pipeline creation, command lists, binding at draw time), loader,
HLE, formats, tooling — except where an item below needs them, which is said.

### XTP (from VIctory at ffe310e) — `crates/xtp-shader`, 26 444 lines

| File | Lines | Role |
|---|---:|---|
| `src/lib.rs` | 36 | entry, `ShaderError` (`Unsupported { what }` names the blocker) |
| `src/dxbc.rs`, `src/dxil.rs` | 181, 211 | DXBC container, DXIL part header |
| `src/native.rs` | 1 023 | XDAT → XBCH (zlib) → FSSM: native programs per hardware stage, root layout and used-resource tables, stage I/O data, export patches |
| `src/gfx10/{mod,fields,opcodes,sp3}.rs` | 496, 813, 1 333, 784 | decoder: control-flow-following instruction walk, operand fields, opcode names (generated from Mesa ACO, MIT), SP3 text |
| `src/ir.rs` | 1 227 | one-to-one SSA, implicit operands (EXEC, SCC, M0) explicit |
| `src/structure.rs` | 1 234 | structurisation: EXEC removed, masks → per-lane booleans, guards only where unproven |
| `src/resource.rs` | 3 053 | traces every descriptor and scalar address to a root parameter; waterfall loops → non-uniform heap index |
| `src/io.rs` | 700 | inputs, interpolation, exports per stage |
| `src/emit/lower.rs` | 7 421 | IR → DXIL, one machine instruction at a time (since S0b `src/emit/lower/`, 19 files, map in `lower/mod.rs`) |
| `src/emit/wave.rs` | 679 | whole-wave sections (reductions, prefix sums) simulated → DXIL wave ops |
| `src/emit/{bitcode,shader,container,sampler}.rs` | 1 208, 665, 389, 125 | LLVM 3.7 bitcode writer, DXIL module/metadata, SFI0/ISG1/OSG1/PSV0 container + hash, static samplers |
| `src/emit/{validator,host}.rs` | 221, 1 381 | `dxil.dll` validator (tests), host D3D12 checks |
| `src/emit/{valu,waterfall,diff}_tests.rs` | 939, 249, 1 287 | GPU/WARP execution tests, differential tests against the console compiler |

Outside the crate: `crates/xtp-gpu/src/shader_cache.rs` (on-disk cache of
translations), `apps/xtp/src/{translate,shader,survey}.rs` (survey tools;
`survey.rs` is S0's `xtp shader-survey`),
`docs/architecture.md` § Shaders.

### xcl (reference, `E:/xtp-refs/xcl` at 47efa2e) — about 20 900 + 3 200 lines

| File | Lines | Role |
|---|---:|---|
| `crates/xcl/src/gpu/rdna2.rs`, `rdna2_spec.rs` | 848, 1 209 | decoder on tables generated from AMD's machine-readable ISA XML (`scripts/gen_rdna2.py`); `decode`, `encode` (test assembler), `branch_target`, `resolve_far_jumps` |
| `gpu/rdna2_xbox.rs` | 54 | console-only opcodes (the gfx10.1 non-fused MAD family) |
| `gpu/xbox.rs`, `gpu/dxbc.rs`, `gpu/shader.rs` | 843, 181, 532 | console container, PSV0, capture/classification |
| `gpu/recomp/mod.rs` | 1 487 | `translate`, stage interfaces (`VsInterface` … `CsInterface`), structured control flow (`emit_structured`, `structurable_to`), `geometry_program`, `wave_hazards` |
| `gpu/recomp/emit.rs` | 5 864 | per-instruction HLSL; prelude; `ballot_fn`, `rla_half_fn`, `mimg`, `interp`, `smem`, `window_read`, `mesh_passthrough`, `x64_prelude` |
| `gpu/recomp/{x64_masks,upper_lanes,mesh_wave,mesh_dispatch}.rs` | 454, 982, 611, 342 | wave64 on 32-lane GPUs |
| `gpu/recomp/{dxc,pso,runner}.rs` | 1 239, 1 022, 1 483 | in-process DXC + DXIL cache, PSO hook and in-memory cache, GPU test runner |
| `gpu/recomp/tests.rs` | 7 454 | 134 tests, most run single instructions on the GPU |
| `shaders.rs`, `gpu/pack.rs`, `insn.rs`, `disasm.rs` | 1 295, 225, 243, 158 | pack survey (`xcl shaders --recomp`), RPF9 record scan, CLI |
| `docs/wave64-on-wave32.md` | 293 | the split-wave64 design |

## 3. How the measurement was made

Both translators were run on `<title folder>\shaders.rpf`, read-only;
nothing of it was copied.

* **xcl:** its own build, `E:\xcl\target\release\xcl.exe shaders
  <title folder>\shaders.rpf --recomp` (13 s) and `--compile 64` (DXC on a sample:
  64 OK, 0 errors; DXC 1.8.2502, median 0.45 s, p90 2.34 s per program).
* **XTP/VIctory:** XTP has no reader for RPF9 shader packs (`xtp translate`
  scans files for DXBC magic and SGD5 records; the pack's records are raw
  deflate). A throwaway program outside the repo (scratchpad, not
  committed) scanned the records, inflated them and ran every VS/PS/CS/MS
  program through `ir::build` → `structure` → `resource::track` →
  `io::raise` → `emit::emit`, the steps of `emit::translate`, counting the
  first error of each program. Every 50th record was also validated with
  `dxil.dll` 1.8 (`emit::validator`). 45 s on all cores. Backlog item S0
  brings this into the repo.

**Since S0** the run is in the repository:
`cargo run --release -p xtp -- shader-survey [PACK] [--json FILE]
[--validate | --validate-every 50]` (the pack: the argument, else
`XTP_SAMPLE_SHADERS`, else `shaders.rpf` in the configured game folder).
`xtp translate` did find the pack's records (the SGD5 marker scan of
`xtp-game/src/sgd5.rs` sees them as the pack stores them), but it skips
repeated containers (614) and counts every step's failure; the survey
counts each of the 26 698 programs once, under the error
`emit::translate_program` stops it with. It reproduces the tables of this
section and of section 4 exactly, and `--validate-every 50` the 450 of
453 below.

Results per stage (programs; one record can hold several):

| Stage | Programs | xcl, no unsupported instruction | XTP translated |
|---|---:|---:|---:|
| VS | 2 236 | 2 236 | 2 177 (97.4 %) |
| PS | 12 101 | 12 101 | 10 138 (83.8 %) |
| CS | 2 689 | 2 689 | 1 444 (53.7 %) |
| MS (NGG) | 9 282 | 9 282 | 8 726 (94.0 %) |
| ES + GS (merged) | 195 + 195 | 390 | 0 (not attempted) |
| **Total** | **26 698** | **26 698** | **22 485 (84.2 %)** |

DXIL validation of the XTP sample: 450 of 453 pass. The three failures: a
mesh shader whose vertex + primitive outputs exceed 32 KB, an output
signature part that does not match the module, and two signature elements
packed into one row with different interpolation modes.

For comparison, the VIctory README's in-game counts (the world): mesh
8 191 / 8 506, VS/PS 10 617 / 12 346, CS 1 643 / 2 539.

## 4. Why XTP's 4 213 programs fail

First blocker per program (fixing it can reveal a second one).
`xtp shader-survey` prints this table, with the backlog item of each row.

| Cause (XTP error text, shortened) | CS | MS | PS | VS | Total |
|---|---:|---:|---:|---:|---:|
| **Lane masks and wave layout** | 576 | 105 | 235 | 11 | **927** |
| — select on an untracked mask | 289 | | | | 289 |
| — mask holding lanes elsewhere read as data | 273 | | | | 273 |
| — lane mask read as data | 4 | 103 | 132 | 11 | 250 |
| — cross-lane operation outside compute / in a mesh shader | | 2 | 46 | | 48 |
| — opaque / constant mask, mask join of an untracked mask | 4 | | 57 | | 61 |
| — whole-wave step not simulated, `v_permlanex16_b32` | 6 | | | | 6 |
| **Memory read as data, descriptor tracking** | 626 | 451 | 493 | 48 | **1 618** |
| — scalar memory read as data | 27 | 390 | 327 | 32 | 776 |
| — descriptor read only as data | 307 | 5 | 77 | 7 | 396 |
| — descriptor's address or format read as data | 46 | | 46 | | 92 |
| — structured buffer of unknown stride | 145 | 1 | 2 | | 148 |
| — address not from a root parameter / from a phi | 97 | | 4 | | 101 |
| — per-lane descriptor cases (first lane of computed VGPRs, waterfall compares, partial joins, chosen by control flow, neither loaded nor built) | 4 | 55 | 37 | 9 | 105 |
| **Images** | 18 | | 794 | | **812** |
| — 64-bit image atomic | 2 | | 657 | | 659 |
| — biased sample with 16-bit coordinates, `image_sample_d_cl`, `image_gather4_l` | 16 | | 137 | | 153 |
| **Pixel inputs and exports** | | | 439 | | **439** |
| — interpolation with computed barycentrics, barycentrics read directly, first half of an interpolation | | | 248 | | 248 |
| — packed input half no element holds | | | 166 | | 166 |
| — hardware SGPR input, conditional export | | | 25 | | 25 |
| **LDS** (bytes/halves, unallocated, other) | 21 | | | | **21** |
| **Other** (`v_cvt_f32_f64`, `s_getpc_b64`) | 4 | | 2 | | **6** |
| **Merged ES + GS** | | | | | **390** |

No program failed in the decoder: XTP decodes every instruction of the
pack (452 distinct mnemonics by xcl's count).

## 5. Capability matrix and verdicts

### 5.1 Matrix

| Area | XTP (VIctory) | xcl | Verdict |
|---|---|---|---|
| Container (DXBC, XDAT/XBCH/FSSM) | full; also reads the root layout and used-resource tables | full; ignores the title's root layout (own ABI) | **keep VIctory** |
| Decoder | hand-written fields, opcode names from Mesa ACO; walks control flow so data after the code is skipped; 6 unit tests | tables generated from AMD's MR-ISA XML; checked against 23 812 LLVM MC encodings (names and lengths); test assembler `encode` | **keep VIctory**, add xcl's kind of conformance test (S6) |
| Instruction coverage | ≈ 370 mnemonics referenced; prefix families (`ds_*`, `v_pk_*`, MUBUF/MTBUF, MIMG) decoded by name; f64 partial; 64-bit image atomics, some A16 forms refused | ≈ 480 mnemonics; f64, 16-bit, packed, dot, sub-dword LDS/buffer, BVH intersect, 64-bit atomics on buffers | **keep VIctory's lowering**, fill the long tail after xcl's list (S1, M4, S5) |
| Control flow | SSA + structurisation: the program as one lane runs it; reducible CFG required; per-lane guards only where EXEC is unproven | wave-uniform interpreter: SGPRs, EXEC, VCC are uniform variables; structured `if`/loops where possible, else a `pc` dispatcher; never fails | **keep VIctory** as the fast path; **new work after xcl**: masks as numbers (M1, M2) |
| Wave size | lane-local code is wave-size independent; cross-lane compute declares the console's `WaveSize`; cross-lane in VS/PS/MS refused | relies on host wave = console wave; wave32 programs as halves of 64-lane waves; wave64 on 32-lane GPUs by an exchange protocol (`docs/wave64-on-wave32.md`) and `upper_lanes` for VS/PS | **new work after xcl**, late (L1–L3): the dev GPU runs wave64 |
| Resources | each descriptor traced to the title's root parameter; host descriptor heap at the console index; root CBV/SRV; static samplers decoded; waterfall loops → one non-uniform access | own ABI root signature (`io`, `res`, `mem` UAVs + directly indexed heaps); all memory one flat `ByteAddressBuffer` over title memory (UMA windows); T# rewritten to hold the heap index | **keep VIctory's model**; **new work after xcl**: a flat-memory fallback for data reads (R1–R4), needs `xtp-gpu` |
| Pixel I/O | signature-driven; interpolation from the program's barycentric pair | rasteriser interpolation, I/J operand not used | **keep VIctory**, add exact barycentrics (P1, P2) |
| Mesh / NGG | native mesh shader (SM 6.6), no cross-lane | compute pass (`WaveSize(64)`) + generated pass-through mesh shader (`mesh_passthrough`) | **keep VIctory**; xcl's two-pass form only if M-items leave mesh programs behind (L4) |
| Geometry (ES+GS) | not attempted | ES and GS merged into one compute program + pass-through (`geometry_program`) | **new work after xcl** (G1, G2) |
| HLSL emission | none | text HLSL, 9.2 GB for the pack | **not adopted** |
| DXC / DXIL | own bitcode writer, container and hash; no DXC at runtime; `dxil.dll` only in tests | in-process DXC, `-O` level by size, memory limits; incidents with > 30 GB and minutes per program | **keep VIctory** |
| Caching | `xtp-gpu/shader_cache.rs`: content + build hash, atomic writes | DXIL cache keyed by HLSL + DXC toolchain, single-flight across processes, in-memory cache, stand-in PSOs while compiling | **keep VIctory** (DXIL writing is fast enough that stand-ins matter less; cross-process single-flight is for the GPU report) |
| Tests | ≈ 120: unit, GPU + WARP execution of synthetic programs, waterfall, differential against the console compiler (`VICTORY_CONSOLE_DXC`) | 134 recompiler tests, most run one instruction on the GPU with a register file in and out (`runner`); retail-pack tests behind `XCL_SAMPLE_RPF`; console-compiler tests | **keep VIctory**, add a pack-survey regression (S0) |

### 5.2 Why not switch to HLSL + DXC

1. Speed: 45 s for the whole pack to final DXIL against a DXC cost of
   ≈ 3.3 CPU-hours (26 698 × 0.45 s median) plus 13 s of HLSL; xcl needed
   stand-in pipelines and a disk cache to hide it.
2. Robustness: xcl reports DXC optimiser blow-ups (superlinear time, a
   machine out of memory) and had to tune `-O` levels by HLSL size.
3. Dependency: no `dxcompiler.dll` to find, version or ship; XTP's DXIL is
   signed by its own container hash.
4. What xcl gains from DXC (an optimiser, a validator) the host driver's
   compiler and the test-time validator already cover.

The cost: every new instruction is written against DXIL ops and LLVM types,
which is more work per instruction than an HLSL string. The backlog accepts
that cost.

### 5.3 Where xcl's coverage is weaker than it looks

Seen while reading, to confirm before relying on xcl's behaviour:

* `mimg` in `emit.rs` issues image atomics on a 32-bit texel whatever DMASK
  says; a 64-bit image atomic (DMASK 3) does not seem to be handled as one.
  657 pixel programs of the pack use them (XTP refuses them).
* `interp` in `emit.rs` takes the value the PC rasteriser interpolates and
  does not use the I/J registers the instruction names, so a program that
  computes its own barycentrics (248 programs fail on this in XTP) gets the
  default interpolation instead.
* Wave64 VS/PS on 32-lane GPUs: xcl's own README says they "can still
  differ there".

So xcl is the reference for *what the console does* only where its tests
check it; for the rest the ISA documentation and the console compiler
(differential tests) are the oracle.

## 6. Porting backlog (ordered)

Each item fits one worker session (a few hundred changed lines). "Gain" is
the number of pack programs whose *first* blocker the item removes; the
real gain is measured by S0 after each item. Files are under
`crates/xtp-shader/src/` unless a path says otherwise. xcl references are
for behaviour only.

Order: the yardstick first, then the largest buckets that need no GPU-layer
change, then those that do, then geometry, then 32-lane GPUs.

**S0 — Pack survey in the repo** (S, deps: none)
- Goal: `xtp translate` reads RPF9 shader packs (raw-deflate records) and
  prints per-stage counts and first-blocker reasons, as section 3 does; an
  opt-in test (`XTP_SAMPLE_SHADERS`, nothing when unset) fails when the
  translated count drops below a recorded floor.
- Files: `apps/xtp/src/{translate,shader}.rs`; a record scanner beside
  `crates/xtp-game/src/sgd5.rs` (tooling report may own it: coordinate).
- xcl: `gpu/pack.rs` `scan`, `inflate`; `shaders.rs` survey output.
- Test: on the person's pack, 26 503 records, 26 698 programs, 22 485
  translated; unit test on a synthetic record stream with decoy names.
- Done: `xtp shader-survey` (`apps/xtp/src/survey.rs`)
  through `emit::translate_program`; `--json` writes one program per line
  with the hash of its translation, so two runs compare with
  `git diff --no-index before.json after.json`. Opt-in test
  `survey::tests::the_pack_survey_keeps_its_floor` (`XTP_SAMPLE_SHADERS`,
  floor 22 485: raise it when an item lands; `cargo test --release -p xtp
  survey`).

**S0b — Split `emit/lower.rs`** (M, deps: S0)
- Goal: no behaviour change; split the 7 421-line file by encoding (SALU,
  VALU, memory, images, I/O) so the items below can run in parallel
  without conflicts.
- Files: `emit/lower.rs` → `emit/lower/{mod,salu,valu,smem,buffer,image,lds,io}.rs`.
- xcl: none.
- Test: S0's survey byte-identical DXIL for every program (hash of all
  outputs unchanged); crate tests pass.
- Done: `emit/lower/` holds `mod` (types, `new`, `run`, the
  dispatch `inst`), `operand`, `flow`, `mask`, `compare`, `lanes`, `mesh`,
  `salu`, `smem`, `descriptor`, `buffer`, `valu`, `valu16`, `integer`,
  `image`, `lds`, `input`, `export`, `tests`; which item edits which file
  is at the top of `emit/lower/mod.rs`, and in the items' Files lines
  below. The survey's JSON is byte-identical before and after (outputs
  blake3 `8dfd4306…`, 22 485 translations).

**S1 — MIMG addressing long tail** (S, deps: S0b) — gain 153
- Goal: biased samples with 16-bit coordinates, `image_sample_d_cl`,
  `image_gather4_l`, with the A16 address packing of each form.
- Files: `emit/lower/image.rs` (the address and sample parts of `image`).
- xcl: `emit.rs` `mimg`, `sample_coord`, `moved_coords`; test
  `gpu_images_a16_d16`.
- Test: synthetic programs per form, run on GPU/WARP against a CPU
  reference (pattern of `emit/valu_tests.rs`); survey +153.

**M1 — Masks the boolean tracker loses, as numbers** (M, deps: S0b) — gain ≈ 330
- Goal: where `structure` has no per-lane boolean for a mask (select on an
  untracked mask, opaque or constant masks, joins of untracked masks), keep
  the mask as a 32/64-bit number and let each lane test its own bit by its
  console lane; in compute with console lanes the number is the ballot.
- Files: `structure.rs`, `emit/lower/{mask,compare}.rs` (mask reads),
  `emit/lower/valu.rs` (`v_cndmask`), `emit/lower/salu.rs` (carries).
- xcl: `recomp/mod.rs` module notes (masks from `WaveActiveBallot`, lanes
  test their bit); `emit.rs` `ballot_fn`, `vopc_write`, `mask_zero`.
- Test: synthetic compute programs (`v_cmp` → `s_and` with a constant →
  `v_cndmask`) on GPU/WARP; survey +289 CS at least.

**M2 — Masks with lanes elsewhere, read as data, in compute** (M, deps: M1) — gain ≈ 280
- Goal: a mask read as a number (`s_bcnt1`, `s_ff1`, compares with
  constants) when some of its lanes are elsewhere in the structured
  program: inactive lanes count as 0 bits, as on the console; requires the
  console wave layout (`WaveSize`), as `console_lanes` already gives.
- Files: `structure.rs`, `emit/lower/{mask,lanes,salu}.rs`, `emit/wave.rs`.
- xcl: `docs/wave64-on-wave32.md` § "Execution model it relies on";
  `emit.rs` `ballot_fn`.
- Test: synthetic waterfall-like and count-active-lanes programs, wave32 and
  wave64, on GPU; survey +273 CS.

**R1 — Data reads through root descriptors** (M, deps: S0b) — gain: part of 776
- Goal: first classify the 776 "scalar memory read as data" programs by
  where the address comes from (root descriptor, table in the heap, built
  from constants); then lower every `s_load_dword*` whose address traces to
  a root CBV/SRV address as a raw load from that root descriptor, which XTP
  already binds.
- Files: `resource.rs` (classification), `emit/lower/smem.rs` (`smem`,
  `descriptor_data`).
- xcl: `emit.rs` `smem`, `window_read` (every scalar load is a flat-memory
  read); `recomp/mod.rs` notes on the flat model.
- Test: unit test in `resource.rs` per address source; GPU test reading a
  root SRV through `s_load_dwordx4`; survey count of the classified part.

**I1 — 64-bit image atomics** (M, deps: S0b; `xtp-gpu` view format) — gain 659
- Goal: image atomics with DMASK 3 (and 0xF for compare-swap) on a 64-bit
  unsigned view, SM 6.6 64-bit typed atomics; the translation states the
  view it needs (R64_UINT) and the host requirement
  (`AtomicInt64OnTypedResourceSupported`).
- Files: `emit/lower/image.rs` (the atomic branch of `image`),
  `emit/mod.rs` (binding notes, `HostShader`); `crates/xtp-gpu` view
  creation (GPU report).
- xcl: no reference (see 5.3); buffer 64-bit atomics: `emit.rs` `atomic64`,
  tests `atomic_fmin_fmax_x2_are_native_64_bit_atomics`.
- Test: synthetic PS/CS doing `image_atomic_umax` 64-bit from many lanes,
  result against a CPU reference; survey +657 PS.

**P1 — Packed 16-bit pixel inputs** (S, deps: S0b) — gain 166
- Goal: an input half (fp16 packed pair) that no signature element holds:
  find it through the FSSM input table's second row, as `native.rs` already
  reads it.
- Files: `io.rs`, `native.rs`, `emit/lower/input.rs` (input loads).
- xcl: `recomp/mod.rs` `ps_input_table`, `ps_attribute_slot`, test
  `packed_registers_are_one_param`.
- Test: synthetic PS with a packed pair, drawn on GPU; survey +166.

**P2 — Exact interpolation from the program's barycentrics** (M, deps: P1)
— gain 248
- Goal: `v_interp_p1/p2` (and f16 forms) on computed I/J, direct reads of
  barycentric VGPRs, and a split p1/p2 pair: read the three vertex values
  (SM 6.1 `GetAttributeAtVertex` on a `nointerpolation` input, or the
  attribute at the provoking vertex plus deltas) and interpolate in the
  shader as the hardware does.
- Files: `io.rs`, `emit/lower/input.rs` (`interp`), `emit/shader.rs`
  (signature interpolation modes).
- xcl: no exact reference (see 5.3); `emit.rs` `interp`, `interp_fields`
  for the operand layout only.
- Test: PS that scales I/J before p2, drawn on GPU against a CPU
  interpolation; survey +248.

**S2 — Validator failures** (S, deps: S0)
- Goal: fix the three `dxil.dll` rejections of section 3 (mesh output size
  limit — refuse cleanly; output signature part; interpolation modes packed
  into one row — repack).
- Files: `emit/container.rs`, `emit/shader.rs`, `emit/mod.rs`.
- xcl: none (DXC packs signatures for it).
- Test: S0 run with validation of every program: 0 rejections.

**R2 — Descriptor memory as data (heap shadow)** (M, deps: R1; `xtp-gpu`)
— gain ≈ 490 + rest of 776
- Goal: the GPU layer binds the title's descriptor-heap bytes (console
  T#/V#/S# words) as one raw SRV; the translation reads descriptor dwords
  and table data from it when the code uses them as numbers (sizes, record
  counts, format bits), and keeps host views for the accesses.
- Files: `emit/lower/smem.rs` (`descriptor_data`), `emit/mod.rs` (binding),
  `crates/xtp-gpu` (bind the shadow; GPU report decides).
- xcl: flat model, `gpu/uma.rs`, `gpu/desc.rs` (T#/V#/S# field meanings).
- Test: GPU test reading width/height bits of a T# and a record count of a
  V# from the shadow; survey +396 +92 and part of 776.

**R3 — Structured buffers of unknown stride** (S, deps: R2) — gain 148
- Goal: an indexed buffer access whose used-resource entry has no stride:
  read it as raw with the stride from the V# (via R2) or from the
  instruction's own arithmetic.
- Files: `emit/lower/buffer.rs`, `emit/lower/descriptor.rs` (`used_kind`),
  `resource.rs`.
- xcl: `emit.rs` `mubuf`, `buffer_op`.
- Test: synthetic CS with an idxen load on a V# of stride 12 and 20; survey
  +148.

**R4 — Address and per-lane descriptor long tail** (M, deps: R2) — gain ≈ 206
- Goal: addresses from phis and from non-root SGPRs (through the shadow or
  title memory), per-lane descriptors the waterfall tracer cannot pin
  (computed first lanes, partial joins, chosen by control flow): fall back
  to one non-uniform access whose index is read per lane.
- Files: `resource.rs`, `emit/lower/descriptor.rs`.
- xcl: none specific (its uniform model runs waterfall loops literally).
- Test: extend `resource::tests` and `emit/waterfall_tests.rs` with each
  shape; survey gain.

**S3 — Small instruction and I/O gaps** (S, deps: S0b) — gain ≈ 52
- Goal: LDS byte/half reads and writes, `v_cvt_f32_f64`, `s_getpc_b64`,
  `v_permlanex16_b32`, the hardware SGPR inputs of PS, conditional exports.
- Files: `emit/lower/{lds,valu,salu,lanes,input,export}.rs` (one per
  gap), `io.rs`.
- xcl: `emit.rs` `ds`, `ds_d16`, `valu64`, `sopp`, `export`; tests
  `gpu_format_stores_atomics_global_lds`, `gpu_f64_rounding_and_conversions`.
- Test: synthetic program per instruction on GPU/WARP; survey gain.

**S4 — Float mode register** (S, deps: S0b)
- Goal: `s_setreg*`/`s_round_mode`/`s_denorm_mode` on the float MODE bits:
  translate rounding/denormal changes the program makes (or refuse when it
  cannot be expressed), and check that non-fused `v_mad_f32`/`v_mac_f32`
  are not contracted to FMA by the host compiler.
- Files: `emit/lower/{salu,valu}.rs`, `emit/bitcode.rs` (flags).
- xcl: `emit.rs` `configure_round_modes`, `decode_hwreg`; tests
  `gpu_nonfused_mad_and_mac_prevent_fma_contraction`,
  `gpu_s_round_mode_controls_f16_rounding_across_modes`.
- Test: GPU test where MAD and FMA differ in the last bit.

**S5 — f64 and 16-bit completeness** (S, deps: S0b)
- Goal: the f64 and 16-bit VALU forms xcl handles and XTP does not
  reference (`v_*_f64`, `v_frexp_*`, `v_ldexp_*`, `v_pk_*` ints, `v_sad_*`,
  `v_dot*` variants) — none blocks the pack today, but title DLL shaders
  (BVH builder, upscaler) use some.
- Files: `emit/lower/{valu,valu16,integer}.rs`.
- xcl: `emit.rs` `valu64`, `valu16`, `valu_pk`; tests
  `gpu_f64_math_random`, `gpu_packed_math_dots_movrel`.
- Test: randomised GPU tests against Rust references.

**S6 — Decoder conformance** (implemented in [PR #52](https://github.com/MiamiGame/XTP/pull/52))
- `gfx10_conformance.rs` checks dword lengths and mnemonics across 19 encoding
  families against the generated LLVM MC GFX10/GFX10.3 corpus. The checked-in
  data keeps one representative per mnemonic, length and decoder selector
  combination. Regenerate it with `cargo xtask gfx10-corpus`.
- The test pins nine decoder gaps and two unresolved legacy MAD/MAC versus
  FMA/FMAC names. It flags pins when the corresponding difference goes away.

**G1 — ES + GS pairing and GS stage data** (M, deps: S0)
- Goal: `native.rs` returns ES and GS of one pipeline as a pair with the GS
  stage data (ESGS item size, output layout); `translate` accepts the pair.
- Files: `native.rs`, `emit/mod.rs` (entry point), `apps/xtp` survey.
- xcl: `recomp/mod.rs` `GsInterface`, `geometry_program`, `es_item_bytes`.
- Test: unit test on a synthetic pair; survey lists 195 pairs.

**G2 — Merged ES + GS as one mesh shader** (L, deps: G1, M1; `xtp-gpu` VS+GS pipelines)
- Goal: lower the ES then the GS in one mesh-shader group (ESGS ring in
  group-shared memory, `s_sendmsg` emit/cut → triangle-strip outputs), no
  cross-lane needed; split into two sessions if larger than a few hundred
  lines (LDS ring + ES part, then GS emit/cut).
- Files: `emit/lower/{mesh,lds,export}.rs` (or a new file beside them),
  `io.rs`, `emit/shader.rs`.
- xcl: `recomp/mod.rs` module notes on GS, `translate_geometry`; tests
  `gpu_geometry_mode_es_gs_strip`.
- Test: synthetic ES+GS strip drawn on GPU, every vertex exact; survey +390.

**L1 — Wave64 compute on 32-lane GPUs, one console wave per group** (M, deps: M2)
- Goal: when the host's `WaveLaneCountMax` is 32, a compute program that
  needs console lanes runs as two 32-lane halves exchanging ballots and
  lane reads through group-shared memory with a group barrier.
- Files: `emit/lower/lanes.rs`, `emit/wave.rs`, `emit/mod.rs` (a translation
  option, part of the cache key in `xtp-gpu/shader_cache.rs`).
- xcl: `docs/wave64-on-wave32.md` (sections "The form", "Synchronisation"
  first case); `emit.rs` `x64_prelude`, `x64_entry`, `rla_half_fn`;
  `x64_masks.rs` `analyze`.
- Test: the same programs as M2 on WARP forced to 32 lanes and on an
  NVIDIA GPU if available; both halves end with the same scalar state.

**L2 — Wave64 compute, several console waves per group** (M, deps: L1)
- Goal: pairwise rendezvous through arrival counters, double-buffered slots,
  bounded wait; thread → console-thread remap for non-consecutive waves.
- Files: as L1.
- xcl: `docs/wave64-on-wave32.md` "Synchronisation" second case, hazards
  1–5; test `gpu_split_wave64_waves_diverge`.
- Test: 128-thread group with diverging waves on a 32-lane device.

**L3 — Wave64 VS/PS lanes 32..63** (M, deps: L1)
- Goal: what console lanes ≥ 32 hold where a VS/PS reads them (unwritten,
  whole-wave constant, spill shadow), for hosts that run them in 32 lanes.
- Files: `emit/wave.rs`, `emit/lower/lanes.rs`.
- xcl: `upper_lanes.rs` (module notes), tests
  `gpu_wave64_ps_spill_slots`, `gpu_wave64_ps_whole_wave_reductions`.
- Test: wave64 PS with a reduction and a spill slot on a 32-lane device.

**L4 — Cross-lane mesh and graphics programs** (L, deps: M2, survey after M1–R4)
- Goal: only if mesh/VS/PS programs still fail on cross-lane or
  mask-as-data after M1–R4: run such a mesh program as a compute pass with
  the console wave size writing a per-group record, then a pass-through
  mesh shader.
- Files: `emit/*`, `crates/xtp-gpu` (two-pass draw).
- xcl: `recomp/mod.rs` mesh notes, `emit.rs` `mesh_passthrough`,
  `mesh_dispatch.rs`, `mesh_wave.rs`.
- Test: synthetic mesh program with a ballot, drawn; survey gain.

Together the items before L1 address every bucket of section 4; how many
programs they really bring is what S0 measures after each one (second
blockers, risk 1).

## 7. Risks

1. **First-blocker counts hide second blockers.** A program fixed by one
   item may stop on the next cause; every item is measured by S0, not by
   the table.
2. **Licence.** xcl is GPL-2.0-only and cannot be combined with
   GPL-3.0-or-later code; workers must write from behaviour descriptions
   and the ISA, never from xcl's source open beside their editor. xcl
   itself contains code ported from AnyPS5 (GPL-2.0), a second reason.
3. **Cross-layer items.** I1, R2–R4, G2 and L4 need `xtp-gpu` changes
   (view formats, a descriptor-heap shadow, VS+GS and two-pass pipelines,
   the cache key). If the GPU report decides otherwise, they stop at the
   translator half.
4. **One 7 421-line file** (resolved by S0b: `emit/lower/`, one file per
   encoding or concern). Without S0b most items edit `emit/lower.rs` and
   conflict when run in parallel.
5. **Silent approximations.** Porting from xcl's behaviour where xcl
   approximates (5.3) would turn refusals into wrong images. Fail-closed
   stays the rule; every item needs a GPU test against an independent
   reference.
6. **Host support.** 64-bit typed atomics, mesh shaders and wave64 are not
   on every GPU; the translation must say what it needs so pipeline
   creation can refuse cleanly.
7. **Pack ≠ runtime.** The pack holds every permutation; the world uses a
   subset (README counts). Priorities may shift once the runtime reports
   which programs it misses.
8. **Driver compilers.** xcl reports the AMD compiler crashing on some
   DXIL; XTP's own bitcode has fewer users than DXC's output, so validator
   runs (S2) and host PSO tests stay in CI.

## 8. Open questions (for the person)

1. Confirm the main decision: keep direct DXIL emission and do not add an
   HLSL + DXC path (recommended, 5.2)?
2. May `xtp-gpu` expose the title's descriptor-heap memory (or title memory,
   as xcl's UMA windows do) to shaders as a raw buffer? R2–R4 depend on it.
3. How much do NVIDIA (32-lane only) users matter now? L1–L3 can wait if
   the target is AMD.
4. Which runtime misses come first in the world: are the 390 merged ES+GS
   programs (one effect family) on the critical path of the first 3D scene?
5. Is the console shader compiler available to run the differential tests
   (`VICTORY_CONSOLE_DXC`), the best oracle for P2, I1 and S4?
6. Should S0's pack reader live in `xtp-game` (BTP 6–specific record
   format) or be a generic tool? The tooling report may own it.
