# RT BLAS inputs: the console builder's own formats

Task 39e97d58, after 61ae65c7 (`device-removed.md`). Series S mode.

## 1. Summary

The thousands of title BLAS refused for "an index past its vertices"
were not bad geometry, late writes or a misread descriptor. The title's
BVH builder (`XboxBvhBuilder.dll`, which the title ships and calls for its
batched builds and its rehydrations) takes DXR's 56-byte triangle
descriptions, but gives three of their fields a meaning DXR does not have:

| Field | Value | What the console builder reads |
|---|---|---|
| `IndexFormat` | `R16_TYPELESS` | meshlet-packed indices (section 3.1) |
| `VertexFormat` | `R32G32_FLOAT` | quantized positions, 8 bytes a vertex, after a 32-byte header (section 3.2) |
| `VertexBuffer.StrideInBytes` | bit 15 | turns each meshlet-packed triangle; the stride is the low 15 bits (section 3.3) |

XTP read `R16_TYPELESS` as `R16_UINT`. It took the packed buffer's
header and tables for indices, such as 0xF800, 0xC800 or 0xFF79, past the
mesh's vertices. It built `R32G32_FLOAT` vertices as (x, y, 0) float
pairs, so the BLAS it did build from them were wrong too.

The host now decodes all three on the GPU, in the pass that makes every
input of a host BLAS build (`crates/xtp-gpu/src/rt/indices.rs`,
`indices.hlsl`, the layouts in `rt/console.rs`). It does so for builds
and rehydrations alike.

## 2. The verification sessions left by 61ae65c7

| Session | Build | GPU lost | Refused when recorded | Refused on the GPU |
|---|---|---|---|---|
| L4 `verify-p12-nodred` (`run.log` on the VM) | dred's branch at 8b5e242, no DRED | no: about 9 min 45 s of the world (frames up to 4,380), ended by a title CPU crash (0xC0000005, a write in the title's code) | 9 lines, all "vertices outside title memory" (the record-time index check was removed) | 16 lines, logged at powers of two: 32,768 to 65,535 |
| PC `dred-markers-1` (`cache/play/20261125-150906`) | 3706ba2 (the record-time index check), DRED on | no: 120 s of gameplay at 0.7 fps, title exit code 0 | 13 lines: 4,096 to 8,191 | none (no GPU check in that build) |

The GPU check reads the indices in the version the build reads, after
every write of the submission. It refused builds at the same rate as the
record-time check, every build of a packed geometry. So lead (d), memory
written after the build was recorded, is ruled out. The same title
addresses were refused on both machines, from the first seconds in the
world. Refusing those builds kept the GPU on both machines, which
confirms 61ae65c7's reading: builds given indices past their vertices
lost it.

## 3. The console builder's formats

How this was found: the builder's CPU side turns each geometry
description into a dispatch of one of its GPU programs. It passes the
vertex address, the index address with its low two bits cleared, and the
vertex address minus 128 as three root views. Its constants carry the
vertex format as a code, the stride, the low two bits of the index
address, the triangle count, and a flag word. The index format sets one
of three flag bits: `R16_UINT`, `R32_UINT` and `R16_TYPELESS` each have
their own. The program that fetches triangles for a build and the one
that fetches them for a rehydration then read the inputs as below. Both
agree. Only layouts and behaviour are reproduced here.

### 3.1 Meshlet-packed indices (`R16_TYPELESS`)

At the index address, its low two bits cleared:

| Part | Size | Content |
|---|---|---|
| header | 12 bytes | word 0: meshlet count in bits 0–16; word 1: length of the triangle table, in words; word 2: not read |
| meshlets | 16 bytes each | first vertex entry = bit 14 of word 0 (as bit 16) over bits 0–15 of word 1 |
| triangles | 4 bytes each | three corners, 7 bits each, at bits 0, 7 and 14; the meshlet at bits 21–31 |
| vertex entries | 2 bytes each | vertex indices |

A corner's vertex is the entry at its meshlet's first entry plus the
corner. `IndexCount` is three per triangle, as in DXR, and gives the
triangle count. The header's other bits are not part of the counts.

### 3.2 Quantized vertices (`R32G32_FLOAT`)

Every `R32G32_FLOAT` geometry is read this way. The 32 bytes before the
first vertex are a header: an origin (three floats), the axis order (a
word), a scale (three floats) and 4 unused bytes. Each vertex is 8
bytes, at the stride:

- `a`: bits 8–31 of the first word (24 bits);
- `b`: bits 0–7 of the first word, then bits 0–15 of the second (24 bits);
- `c`: bits 16–31 of the second word (16 bits).

Each is `scale * q + origin`, with the header's first, second and third
component respectively. The position is (c, b, a) for axis order 0,
(b, c, a) for 1, and (a, b, c) for any other.

### 3.3 The stride's bit 15

The builder reads the stride's low 15 bits as the stride. Bit 15 swaps
the first and third corners of each meshlet-packed triangle, which turns
it to face the other way. The title passes 8 and 0x8008.

### 3.4 What the title passes

From the builder capture of `cache/play/20261125-100513` (about 40 s of
the world, entry point counts):

| Calls | Vertex format | Index format | Descriptions |
|---|---|---|---:|
| builds and refits | `R32G32B32_FLOAT` | `R16_TYPELESS` | 8,171 |
| builds and refits | `R32G32_FLOAT` | `R16_TYPELESS` | 2,111 |
| builds and refits | `R32G32B32_FLOAT` | `R16_UINT` | 2,368 |
| rehydrations | `R32G32_FLOAT` | `R16_TYPELESS` | 2,743 |
| prebuild queries | both | `R16_UINT` only | 8,218 |

The title turns `R16_TYPELESS` into `R16_UINT` for its prebuild queries
(their sizes depend only on the triangle count), and passes it as is to
its builds. The refused example "index 65401 past its 529 vertices" is an
`R32G32_FLOAT`/`R16_TYPELESS` geometry with stride 8, whose packed
indices start 0xDC10 bytes after its vertices. The `R16_UINT` geometries
(no packing) were never among the refused.

## 4. The leads, from strongest to weakest

| Lead | Verdict |
|---|---|
| (a) descriptors decoded wrongly in the hook | the descriptions are the standard 56-byte ones, no field swapped; the error was in the meaning of their formats (section 3) |
| (b) the 16/32-bit index format | yes: `R16_TYPELESS` is not 16-bit indices but meshlet-packed ones |
| (c) a base vertex, a console `VertexCount` | partly: the meshlets carry a first vertex entry; `VertexCount` is the DXR one |
| (d) memory written after recording | no: the GPU check refused as many (section 2) |
| (e) primitive restart | no: the builder has none |

## 5. The fix

The pass that 61ae65c7 added to check indices on the GPU now makes every
input of a host BLAS build in a buffer of the build's own
(`rt/indices.rs`):

- **indices**, as 32-bit ones: copied from `R16_UINT` and `R32_UINT`,
  decoded from meshlet-packed ones (the stride's bit 15 swapping the
  first and third corners);
- **vertices**: decoded from quantized ones to `R32G32B32_FLOAT`
  positions, or copied in their format, at the stride's low 15 bits;
- **the transform**, copied.

The decoded indices are checked against the geometry's vertex count,
which is the number of positions decoded. An index past it, or a part of
a packed buffer outside the bytes title memory held when the build was
recorded, raises the build's flag. That makes every index of the build
0, and the refusal is logged once the submission has run, saying which
of the two it was. The bytes a packed buffer spans are read from title
memory when the build is recorded (`console::packed_bytes`): its header,
its triangle words, the meshlets they name and the furthest vertex entry
they reach. The pass reads none past them. A version that reaches
further when the build runs is refused, not read out of bounds.

Two points of the Codex review of 61ae65c7 (dcc6e70a) are fixed with it:

- **P1-1:** the pass reads the title's copies as UAVs, as the instance
  rewrite does, then returns them to the common state. A title shader
  that wrote one earlier in the submission left it promoted to the UAV
  state, which the shader resource read did not take. Since the build
  now reads only the pass's buffer, no title copy is left in a read
  state the next build's pass could not read as a UAV.
- **P1-2:** each window is read through a view whose base is 16-byte
  aligned, with its offset given to the shader.

Shortcuts kept:

- A title copy that a draw of the same submission read before the pass
  is promoted to a read state, which a UAV read may not take. The debug
  layer may report it, and the instance rewrite shares it.
- A refused build still takes its geometries without indices.

Cost: one dispatch per input (vertices, indices, transform) and one per
indexed geometry to zero refused indices, plus one to clear the flag; two
global UAV barriers and one transition per title copy read. Vertices are
copied once more than before.

## 6. Tests and session

GPU tests in `crates/xtp-gpu/src/rt/tests.rs`, on hand-made geometry with
the exact distance of each hit (`ray_console.hlsl`: five hits on three
triangles in planes of their own, three misses just outside them):

- `rt_meshlet_packed_indices_build_the_triangles_they_name`: meshlets
  whose first entry is past 65,535, header bits above the meshlet count,
  the same hits, faces and triangles as plain indices; bit 15 turns every
  triangle, as the plain indices with their corners swapped; the same
  bytes read as `R16_UINT`, the path before, leave every ray missing;
- `rt_quantized_vertices_build_their_positions`: each axis order, 24-bit
  coordinates past 16 bits, a 16-byte stride;
- `rt_title_like_geometry_builds_and_rehydrates_faithfully`: quantized
  vertices at stride 8 and 0x8008 with packed indices, built and
  rehydrated;
- `rt_inputs_a_title_shader_wrote_are_read_as_it_left_them`: a title
  pass writes the packed indices before the build in the same list (P1-1);
- views 16-byte aligned, with windows 2, 4, 6 and 8 bytes past a 16-byte
  boundary in the tests above (P1-2);
- `console.rs`: the bytes a packed buffer spans, a meshlet's first entry
  past 16 bits.

Also in `rt/dispatch.rs`, for draws' traces (task ac895e39):
`dispatch_descriptor_tlas_in`, the dispatch of a trace with the TLAS a
descriptor names at its place in submission order, from the host list
alone (`rt_a_trace_by_its_host_list_takes_the_tlas_submitted_before_it`).

A decoder of the archived assets in another project (read for its
behaviour only) reads the same position layout (header 32 bytes before
the positions, the same three fields and axis orders) and the same
triangle words. Its meshlets give vertex counts, from which it sums each
meshlet's first entry; the console builder reads the first entry from the
meshlet's own words (section 3.1).

The verification session is in section 7.

## 7. Verification

- The RT GPU tests on the L4 (NVIDIA, each test in a process of its
  own, this change before the test fix below): 30 of 30 pass, no GPU
  reset logged.
- On WARP (the build farm), the `rt` tests pass, alone and in parallel
  (8 runs of 80). In parallel, the existing
  `rt_tlas_rebuilds_reuse_a_result_nothing_holds` failed 3 times in 8
  before: two `watch` tests report a remap of the whole address space,
  which retires every structure and spare of the shared registry, and
  the longer suite made them overlap. A try that a release covering its
  TLAS cut into now runs again; its assertions are unchanged.
- A world session on the L4 (`blasin-verify`, autopilot, 900 s, no
  DRED) was started from this change's code when this was written. Its counts (BLAS
  refused on the GPU and when recorded, GPU loss, captured frames) are
  reported on task 39e97d58.
