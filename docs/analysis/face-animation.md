# Face animation: sixteen-bit typed buffer accesses

Faces did not animate: mouths stayed shut while characters talked. The
network that drives them runs on the GPU and moves its tensors with
sixteen-bit typed buffer instructions. XTP translated those accesses
through 32-bit floats, as VIctory did before fxceless/victory-fork#927.
The views the title gives them are 16-bit unsigned, so each access read a
float through an integer view. D3D12 leaves that undefined. XTP now moves
each channel's 16 bits as an unsigned integer
(`crates/xtp-shader/src/emit/lower/buffer.rs`).

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
`bufferStore.i32` through the same kind of handle. Other typed buffer
accesses keep their float handles. The code carries the credit line
"Ported from fxceless/victory-fork#927 (5dc9e63f04) by tippythetaidum".

Tests (`buffer16_tests.rs`; WARP, and the hardware adapter where there is
one). Each test lowers a synthetic GFX10 program and validates it with
`dxil.dll` when present:

| Test | Program | Checks |
| --- | --- | --- |
| `halves_cross_r16g16b16a16_uint_views_as_they_are` | `buffer_load_format_d16_xyzw` from an `R16G16B16A16_UINT` SRV, `buffer_store_format_d16_xyzw` to an `R16G16B16A16_UINT` UAV | 128 halves come back unchanged: both zeros, subnormals, infinities, not-a-numbers with payloads, and integers above `0x7bff` |
| `indices_load_from_r16_uint_views_as_they_are` | `buffer_load_format_d16_x` from an `R16_UINT` SRV, the register stored as a dword | 32 indices: the low half of each register is the element's 16 bits |
| `a_descriptor_loaded_inside_a_range_is_the_one_moved` | the same move as the first, with the SRV loaded at byte 0x20 into a range one entry describes from dword 0, a decoy view in slot 0, and the UAV at byte 0x40 | the halves come from slot 1, not from the range's start |

With the change all three pass (build farm, commit `4f937e4`):
`cargo-slot.ps1 test -p xtp-shader --locked --lib buffer16_tests` gives
3 passed, 0 failed.

## 4. Critical review: what was ported and what was not

- **Ported:** the translator's integer path, with the fork's masking and
  packing. The handle's component type (`0x405`) and the `ResRet.i32`
  return type are the fork's.
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

Two surveys of the whole pack (`xtp shader-survey --validate --host-lanes
32-32 --json`) ran on the build farm (`Invoke-FarmSurvey.ps1`): the same
pack (26,503 records, 26,698 programs), the base `997af76`, then this
change `4f937e4`. Each program's translation hash (DXIL and binding tables)
was compared:

| | Base `997af76` | With the change `4f937e4` |
| --- | ---: | ---: |
| Programs translated | 25,812 | 25,812 |
| Programs refused | 886 | 886 |
| Programs whose outcome changed | | 0 |
| Programs whose translation changed | | **65** |
| Of those, validated by `dxil.dll` | 65 | 65 |

The 65 changed programs are all compute programs, and they are exactly the
65 programs of the face network (the fork's `compute_graph_ops` group). No
other program's translation changed, including the head's blend-shape and
normal programs downstream, and the report's totals and categories are
identical. All 65 network programs were already translated on the base:
nothing refused the network at translation, but its reads went through the
float path that reads 0 (§2). Results:
`E:\xtp-data\farm-surveys\faceanim-before-997af76` and
`faceanim-after-4f937e4` (outside the repository).

## 6. Open points

- **No session probe.** The fork's evidence in the game is a diagnostics
  patch that hashes the arena every fourth frame, and that patch is not in
  its PR. XTP has no such probe. The faces can be checked by eye in the
  prologue's cutscenes on a session with this change.
- **Neighbouring cases, not changed here:**
  - A typed load without d16 (`buffer_load_format_x` to `_xyzw`) still
    reads through a float handle. Through a 16- or 32-bit unsigned or
    signed view the console returns integers, so that is the same mismatch
    in 32-bit form. It is not counted for this title.
  - `Lower::buffer` does not take `tbuffer_load_format_d16_*` as d16
    (`d16` requires the `buffer_` prefix), so it would write one channel to
    a register, not two. Whether the title uses it was not counted.
  - Image d16 results are still taken as floats (`lower/image.rs`).
- **One handle per descriptor and block.** Handles are cached by
  descriptor, block and kind (`Lower::handle`), not by component type. A
  program that reads one descriptor both with and without d16 in the same
  block would get the first access's handle for both. The key is in
  `lower/descriptor.rs`, outside this change; no such program is known.
