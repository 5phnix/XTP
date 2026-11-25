# Face animation: sixteen-bit typed buffer accesses

Faces did not animate: mouths stayed shut while characters talked. The
network that drives them runs on the GPU and moves its tensors with
sixteen-bit typed buffer instructions. XTP translated those accesses
through 32-bit floats, as VIctory did before fxceless/victory-fork#927.
The views the title gives them are 16-bit unsigned, so each access read a
float through an integer view. D3D12 leaves that undefined. XTP now moves
each channel's 16 bits as an unsigned integer
(`crates/xtp-shader/src/emit/lower/buffer.rs`). The head's blend-shape
extraction reads the network's output with plain typed loads and takes
the low half of each as a half. Reading those loads as floats put rainbow
squares on faces in the fork. XTP names that one program by the
fingerprint of its code and gives its plain typed loads the same integer
path: a shortcut for this title (§3, §4).

## 1. What the fork found

PR fxceless/victory-fork#927 is by tippythetaidum (GPL-3.0-or-later):
commit `5dc9e63f04` with review fix `befaf3b5aa`. It was read with
`gh api` only. Its findings, measured on an RTX 5080 (driver 621.14,
Series X profile):

- The faces are driven by a network of about 2,650 compute dispatches a
  frame in the opening cutscene: the `compute_graph_ops` programs (zero,
  SiLU, gather, vector-matrix and small matrix products, scatter). Their
  output goes to the head's blend-shape extraction (`970c4fbb3b017c0e`)
  and to the normal reconstruction (`e4a7e088c7780ce8`).
- Those programs move every tensor with `buffer_load_format_d16_x`,
  `buffer_load_format_d16_xyzw` and `buffer_store_format_d16_xyzw`: 616,
  99 and 441 instructions. These are all of the pack's d16 typed buffer
  instructions. They are spread over 65 programs, which load their 231
  descriptors from fixed places in tables.
- The title views the tensors as `R16G16B16A16_UINT` and `R16_UINT`: 524
  and 13 views onto a 24 MB arena in one frame. The buffers hold half
  floats with plain 16-bit integers (indices) beside them. On the console
  each channel's 16 bits go through as they are.
- Before the fix the arena read all zeros in 557 of 557 samples, and the
  network's output in 559 of 559. After it, the arena changed in 1,074 of
  1,074 samples and the output in 539 of 539. By eye, the mouths move.
- Showing the integer views as 16-bit float views instead made some
  mouths move but not others, and the player's face twitched. Not-a-number
  patterns do not survive a float round trip (1 of the 2,046 did on that
  card), and the indices are integers.

The fork's change has two parts. Its translator moves the 16 bits as
unsigned integers (`bufferLoad.i32`, `bufferStore.i32`, a `u32` handle).
That applies where the descriptor is loaded from a fixed place in a table.
`HostShader::half_views` names those places. The fork's host then shows a
16-bit float or signed buffer view there as its unsigned twin, in a table
made for the draw. It logs once a view whose format the integers do not
carry exactly.

Review by avt0m4t48: the first version named the start of the descriptor
range (`used_kind`'s `table_offset_dwords`), not the descriptor loaded. For
a load at byte 0x20 into a range that one entry describes from dword 0, it
named dword 0 instead of 8. The host would then look at the wrong slot for
a float view to swap. `befaf3b5aa` takes the load's own place (traced
offset plus first dword). A descriptor picked at run time names nothing and
keeps the float path.

Later follow-ups on the PR, read on 2026-11-25:

- `6d65a930c2` (avt0m4t48) is a WARP test of the fork's `State::tables`. A
  table made afresh for a half twin at dword 8 reserves one slot, because
  its length counts only range starts; the next allocation takes the
  second slot. The suggested fix adds the half places to the slots the
  length counts.
- `efd753e` (iGoldenNinja) checks that a half view's handle is the start of
  a slot before reading its record.
- iGoldenNinja reproduced the output result on another RTX 5080: at the
  extraction's `R16_UINT` input, 0 of 76 reads were nonzero before and 69
  of 69 after. Their audit found all 425 sixteen-bit access sites of the
  24 network programs they saw at the start of their ranges.
- The author put the PR back to draft. With it, some faces show small
  rainbow-coloured squares on the skin (the opening cutscene's waitress,
  every time; one A/B each way). The cause given: the head's blend-shape
  extraction (`970c4fbb3b017c0e`) reads the network's output with a plain
  `buffer_load_format_x`, not d16, through the same 16-bit unsigned view.
  It uses only the low half of each result, as a half (`v_fma_mix_f32`
  with that half selected). The translator kept that load as a float load.
  Before the fix the output was all zeros, so what it read was zero too.
  The author is extending the change to "a format load whose every use
  reads one half as a half float"; no code for it is published yet.

## 2. XTP had the same defect

`Lower::buffer` in `crates/xtp-shader/src/emit/lower/buffer.rs` (before
this change):

- typed load: `bufferLoad.f32` through a typed handle declared as four
  floats (`0x409`), then `legacyF32ToF16` on each channel;
- typed store: `legacyF16ToF32` on each half, then `bufferStore.f32`;
- the same comment as the fork before #927: "shortcut: d16 data is taken
  as f16 (integer formats would keep 16 bits)".

XTP's host makes each view in the title's own format (`descriptor.rs`
`host_desc`, `resource::pc_format`), so the network's tensors are read
through `R16G16B16A16_UINT` and `R16_UINT` views, as on the fork.

Reproduction: `crates/xtp-shader/src/emit/buffer16_tests.rs` (§3) run with
the translator as it was before this change (`buffer.rs` of base `997af76`),
on the build farm's WARP:

| Test | Before the change |
| --- | --- |
| `halves_cross_r16g16b16a16_uint_views_as_they_are` | 127 of 128 halves changed, every one to `0x0000`. Only `0x0000` came through. |
| `indices_load_from_r16_uint_views_as_they_are` | 31 of 32 indices changed, every one to `0x0000` |
| `a_descriptor_loaded_inside_a_range_is_the_one_moved` | 127 of 128 halves changed to `0x0000` |

WARP gives what the fork saw on the RTX 5080: through the unsigned views
the float path reads 0.

## 3. The change

A `buffer_load_format_d16_*` is now a `bufferLoad.i32` through a handle
declared as four unsigned integers (`0x405`). Each channel keeps the low 16
bits of what it reads, packed two to a register as before (the low half
first; an odd last channel leaves the high half 0). A
`buffer_store_format_d16_xyzw` stores each half, masked to 16 bits, with
`bufferStore.i32` through the same kind of handle. The code carries the
credit line "Ported from fxceless/victory-fork#927 (5dc9e63f04) by
tippythetaidum".

**Shortcut for this title: the head's blend-shape extraction.** The
plain typed loads (`buffer_load_format_*`, not d16) of the programs listed
in `INTEGER_LOAD_PROGRAMS` (`lower/buffer.rs`) take the same integer path.
Each register then holds the element's 16 bits zero-extended, as on the
console through a 16-bit unsigned view. Programs are named by FNV-1a on
64 bits of their native code (`code_fingerprint`). The list holds one
program: the extraction, container `970c4fbb3b017c0e`, whose 2,112 bytes
of code hash to `0x4a229537a8657146`. That value was printed on the build
farm from the program the translator reads.

What the program does, from its disassembly on the farm (outside the
repository):
- it loads a descriptor from dword 16 of its table (`s_load_dwordx4
  s[36:39], s[4:5], 0x40`);
- it reads three elements with `buffer_load_format_x`;
- it widens the low half of each with `v_fma_mix_f32 vN, sel_lo(v11),
  1.0, vN`.

The fork measured that view as `R16_UINT`, 1,064,960 bytes: tippythetaidum
read it from the program's code, and iGoldenNinja probed it on another
RTX 5080 at root 0, dword 16. Through a float handle, which D3D12 leaves
undefined over an integer view, the extraction gave the faces rainbow
squares once the network computed (§1). The fork has published no code for
this case. XTP's shortcut follows tippythetaidum's analysis; §4 says why
it is not a general rule.

Every other typed buffer access keeps its float handle.

Tests (`buffer16_tests.rs`; WARP, and the hardware adapter where there is
one). Each test lowers a synthetic GFX10 program and validates it with
`dxil.dll` when present:

| Test | Program | Checks |
| --- | --- | --- |
| `halves_cross_r16g16b16a16_uint_views_as_they_are` | `buffer_load_format_d16_xyzw` from an `R16G16B16A16_UINT` SRV, `buffer_store_format_d16_xyzw` to an `R16G16B16A16_UINT` UAV | 128 halves come back unchanged: both zeros, subnormals, infinities, not-a-numbers with payloads, and integers above `0x7bff` |
| `indices_load_from_r16_uint_views_as_they_are` | `buffer_load_format_d16_x` from an `R16_UINT` SRV, the register stored as a dword | 32 indices: the low half of each register is the element's 16 bits |
| `a_descriptor_loaded_inside_a_range_is_the_one_moved` | the same move as the first, with the SRV loaded at byte 0x20 into a range one entry describes from dword 0, a decoy view in slot 0, and the UAV at byte 0x40 | the halves come from slot 1, not from the range's start |
| `a_listed_program_reads_its_format_loads_as_16_bit_integers` | the extraction's read: `buffer_load_format_x` (not d16) from an `R16_UINT` SRV, `v_fma_mix_f32` of its low half with 1.0 and 0, the result stored as a dword; the program named in the list for the test (its fingerprint added by a test-only override) | 32 halves come out as their float values: zeros, normal numbers, infinities, and not-a-numbers as not-a-numbers |
| `other_programs_keep_the_float_path_of_format_loads` | the same program not listed; then a variant that also stores the register whole, from an `R32_FLOAT` SRV | no load lowered as integers; the 32 floats come back as they were |
| `programs_are_named_by_the_fnv_1a_hash_of_their_code` | `code_fingerprint` | FNV-1a 64's published values for `""`, `"a"` and `"foobar"` |

Each translating test also checks how many of its loads the translator
lowered as 16-bit integers (`Lowered::integer_loads`, kept for tests
only): one in the first four, none in the fifth.

Results on the build farm:

- The first three pass from commit `4f937e4` on, and all six from
  `1a86ee8` on.
- With the shortcut cut (`d16` alone, the rest as at `1a86ee8`),
  `a_listed_program_reads_its_format_loads_as_16_bit_integers` fails on
  its count of integer loads (0 instead of 1). The other five pass.
- Run on WARP, the old float path happens to keep the bits of the
  extraction's plain load. The 16 bits come back as the float's bits,
  untouched until the program reads their low half. So WARP alone does
  not show the squares; the fork saw them on an RTX 5080.

## 4. Critical review: what was ported and what was not

- **Ported:** the translator's integer path, with the fork's masking and
  packing. The handle's component type (`0x405`) and the `ResRet.i32`
  return type are the fork's.
- **Added: a shortcut for the extraction, not a general rule.** XTP
  lowers the extraction's plain load exactly as the fork did: the d16
  part alone leaves the extraction unchanged (§5). So the d16 part alone
  would very likely show the fork's squares here too. A general rule
  ("a plain typed load whose every use reads one half as a half moves
  16-bit integers", the fork's announced direction) was written, tested
  and dropped, for two reasons:
  - **It missed the extraction.** Its three loads run inside a partial
    EXEC mask (`v_cmpx`). The registers are later written again under
    other partial masks (`v_mov_b32 v11, v13`), then read as floats in the
    lanes that wrote them. XTP's SSA form merges the loaded value through
    a phi and counts those partial writes as reads, so the code alone
    cannot show that the loaded values are read only as halves. That
    would take reasoning lane by lane.
  - **It changed 658 other programs**, most of them pixel shaders:
    textured and ped materials, particles, breakable glass, cables,
    decals, fog composite, the deferred resolve, light proxies, and the
    compute light probes, water and scattering programs. All validated.
    Their code reads a plain typed load only as a half, for example
    `buffer_load_format_x v40` then `v_cvt_f32_f16 v40, v40` in
    `PS_ResolveDeferred_1`. That is right on the console only if the view
    holds integer bits. With an `R16_UINT` or `R32_UINT` view, the
    unsigned handle is right and today's float handle is outside D3D12.
    With an `R32_FLOAT` view filled with `asuint`, it is the other way
    round. Which one the title binds is known only on the host, so a
    separate task probes those formats in a session (some of these
    programs feed the lighting).
  The shortcut names the one program whose view the fork measured. It
  changes that program alone (§5), and the test checks that the same
  loads elsewhere keep their float path.
- **Not ported: `6d65a930c2` and `efd753e`.** Both concern the fork's host
  twin (the length of a table made afresh for it, and the handles of its
  half views). XTP has no twin and makes no table for halves.
- **Not ported: the host twin.** This covers `half_view`, `half_inexact`,
  `HostShader::half_views`, the shader cache field and the fresh tables in
  `State::tables`. The fork's own counts say the title's views at these
  places are all `R16G16B16A16_UINT` and `R16_UINT`. Its author writes
  that for this title no extra table is ever made. The twin only matters
  for a 16-bit float or signed view, which the title does not give these
  programs. Porting it would add a field to every `HostShader` and change
  the shader cache's encoding, `State::tables` and `descriptor/host.rs`,
  three files other tasks hold, for code this title never runs. The task
  scoped the change to the translator.
- **Consequence (shortcut, written at the code):** a 16-bit float or
  signed view would now be read through an unsigned handle. A wider,
  normalised or 8-bit view, whose values the console converts to halves,
  is read as its integers. The fork logs that second case once; XTP does
  not. It did not appear in the fork's runs.
- **The condition differs from the fork's.** The fork takes the integer
  path only for a descriptor loaded from a fixed place in a table, because
  only there can its host swap a twin view. XTP swaps no view, so that
  restriction protects nothing: XTP takes the integer path for every
  sixteen-bit typed access, wherever its descriptor comes from. For the
  title the set is the same, since its 231 descriptors are all loaded from
  fixed places.
- **The +32-byte fix does not apply.** The defect was in the place the
  fork names for its host. XTP names no place and changes no view. The
  handle itself was never affected: in both codebases the heap index comes
  from the traced address of the load, not from the used-resource entry.
  `a_descriptor_loaded_inside_a_range_is_the_one_moved` runs the
  reviewer's layout and reads the loaded descriptor.
- **The fork's diff-test change is left for later.** In the
  `translated_compute_matches_pc` case "16-bit and 64-bit arithmetic", the
  fork shows the translated program an unsigned twin of its
  `R16G16B16A16_FLOAT` UAV (`View::HalfUav`). XTP's `diff_tests.rs` is
  outside this change, and the test needs the console's compiler, so it is
  skipped. If it runs one day, that case will show a float view to a
  program that now stores unsigned integers. It should get an
  `R16G16B16A16_UINT` view there, as in the fork.

## 5. Shader survey

Surveys of the whole pack (`xtp shader-survey --validate --host-lanes 32-32
--json`) ran on the build farm (`Invoke-FarmSurvey.ps1`), all over the same
pack (26,503 records, 26,698 programs):
- the base `997af76`;
- the d16 part `4f937e4`;
- with the extraction's shortcut, `1a86ee8`.

Each program's translation hash (DXIL and binding tables) was compared
with the base's:

| | Base `997af76` | d16 part `4f937e4` | With the shortcut `1a86ee8` |
| --- | ---: | ---: | ---: |
| Programs translated | 25,812 | 25,812 | 25,812 |
| Programs refused | 886 | 886 | 886 |
| Programs whose outcome changed | | 0 | 0 |
| Programs whose translation changed | | **65** | **66** |
| Of those, validated by `dxil.dll` | | 65 | 66 |

- The 65 are exactly the 65 compute programs of the face network (the
  fork's `compute_graph_ops` group).
- The 66th is the head's blend-shape extraction. From `4f937e4` to
  `1a86ee8` it is the only program that changed.
- No other program's translation changed, and the report's totals and
  categories are identical.
- All 65 network programs were already translated on the base. Nothing
  refused the network at translation, but its reads went through the
  float path that reads 0 (§2).

The general rule, dropped (§4), changed 723 programs over the base at its
commit (`eb42e38`, no longer on the branch): the 65, plus 658 others, and
not the extraction. Results, outside the repository:
`E:\xtp-data\farm-surveys\faceanim-before-997af76`,
`faceanim-after-4f937e4`, `faceanim-shortcut-1a86ee8` and
`faceanim-ext-eb42e38`.

## 6. Open points

- **Plain typed loads read as halves in 658 programs** (§4). A separate
  task probes, in a session, the formats of the views those loads read:
  the deferred resolve, fog composite, light proxies and light probes
  among them. An unsigned view would want the same integer path as the
  extraction, and the float handle they use today is outside D3D12 over
  it. A 32-bit float one keeps today's path.
- **The shortcut names a program by its code.** A title update that
  changes the extraction's code changes its fingerprint, and its loads
  fall back to the float path silently. The survey shows whether the
  extraction still changes (§5).

- **No session probe.** The fork's evidence in the game is a diagnostics
  patch that hashes the arena every fourth frame, and that patch is not in
  its PR. XTP has no such probe. The faces can be checked by eye in the
  prologue's cutscenes on a session with this change.
- **Neighbouring cases, not changed here:**
  - A plain typed load (`buffer_load_format_x` to `_xyzw`) read other
    than as low halves still goes through a float handle. Through a 16- or
    32-bit unsigned or signed view the console returns integers, so that
    is the same mismatch for a program that uses them as integers. It is
    not counted for this title.
  - `Lower::buffer` does not take `tbuffer_load_format_d16_*` as d16
    (`d16` requires the `buffer_` prefix), so it would write one channel to
    a register, not two. Whether the title uses it was not counted.
  - Image d16 results are still taken as floats (`lower/image.rs`).
- **The length of a table made afresh** (XTP's counterpart of
  `6d65a930c2`, outside the d16 path). `State::tables` in
  `crates/xtp-gpu/src/draw.rs` makes some tables afresh in dynamic slots:
  those holding a constant buffer, and those reaching past the host heap.
  It copies `n` slots, where `n` is one past the last range start the
  used-resource entries give. A shader that loads a descriptor inside a
  range, past its last start, would read a slot not copied: whatever the
  next allocation puts there. This does not touch the face network, whose
  sixteen-bit access sites all sit at the start of their ranges (the
  fork's count and iGoldenNinja's). Whether any other program loads past
  its last range start through such a table was not counted.
- **One handle per descriptor and block.** Handles are cached by
  descriptor, block and kind (`Lower::handle`), not by component type. A
  program that reads one descriptor in the same block both as 16-bit
  integers and as floats would get the first access's handle for both. The key is in
  `lower/descriptor.rs`, outside this change; no such program is known.
