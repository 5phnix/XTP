# Addresses used by the DXR trace programs

All eight row 6 programs first fail on a read of the guest root node at
AS + 0x30 (build 1.0.2.1). Tracing both address words back to the scalar
descriptor load identifies the source view. Table offsets below are in
**bytes** from the descriptor table in user data SGPR 5. PCs are byte
offsets from the start of the native program.

| Program | First root read (PC) | Descriptor offset | Other descriptor offsets seen at root reads |
|---|---:|---:|---|
| `f22a76fcb967ee59` (screen probe rays) | 0x7e8 | 0x20 | 0x60, 0xc0 |
| `32ed072b9f845048` (prototype) | 0xc4 | 0x20 | — |
| `559d9f0184e15fe3` | 0x143c | 0x60 | — |
| `f607780aa8e502d0` | 0x384 | 0x80 | 0x60 |
| `6d7fe7745ab9c6d8` | 0x834 | 0x20 | 0x80, 0xe0 |
| `3592426e3e9142d8` | 0x8d8 | 0x80 | 0x40, 0xc0 |
| `36f04a4ed0ad1d9b` | 0x470 | 0x20 | 0x60 |
| `b58eb3ca6ee46427` | 0x7f0 | 0x80 | 0x40, 0xc0 |

The first root read in each of the six family B programs keeps bits 8–39
of the descriptor base and sets the low byte to 0x30. The two rays in the
smallest family B program (`f607780aa8e502d0`) read their roots at PCs
0x384 and 0xbc4 from different views in the same dispatch. Each view
needs its own lookup. Whether they alias depends on the bound descriptors.

## Screen probe rays

The indirect lighting capture requests 17 groups of `f22a76fcb967ee59`.
The five walks use three views. The last column gives the instruction
immediately after the loop latch.

| Descriptor offset | Root read | Intersection | Loop exit |
|---:|---:|---:|---:|
| 0x20 | 0x7e8 | 0xb1c | 0x1210 |
| 0x60 | 0x16c4 | 0x1994 | 0x21b0 |
| 0x20 | 0x2280 | 0x2544 | 0x2c40 |
| 0xc0 | 0x2f6c | 0x329c | 0x3818 |
| 0xc0 | 0x3974 | 0x3ca4 | 0x4220 |

There are 21 global loads outside the five loops. Five read the root.
The other sixteen decode hit records in two groups of eight at
0x12ac–0x1444 and 0x2d10–0x2eac. The first group combines the AS base
and the instance node index. It uses the low 24 bits at +0x34, the BLAS
reference at +0x3c (in 256-byte units), and the low 16 bits at +0x70.
The triangle accesses combine the BLAS base and a node index in 64-byte
units. They use the upper 24 bits at +0x30 and a word selected from
+0x34 onward according to the triangle type. The second group repeats
this decoding for the instance selected by the preceding paths. The
splice needs to cover these reads and preserve the values they feed to
the rest of the shader. Mapping them to DXR hit data still requires a
differential check, including both triangles of a pair.

In an L4 translation with only the walks and root reads replaced by
diagnostic values, the resource tracker stopped at 0x12ac, the first read
in the first group.

An offline diagnostic bypasses the five loop bodies, supplies lane-varying
stand-ins for their VGPR results and the 21 AS loads, and removes the three
AS descriptor loads. The original ray setup, intervening branches and
output accesses still go through the translator. In this diagnostic the
resource tracker traced 54 descriptor uses. The baseline then failed at
the atomic add at 0x509c. Its input is the low word of a masked whole-wave
sum. Each addition writes a carry flag for the high word, but only the low
word reaches the atomic. That word can be reduced modulo 2³². With that
change the diagnostic validates and creates a compute pipeline on L4
(SDK 10.0.26100.0). Its substitute hit values do not test scene lighting.

The preceding activation shader, `39f0e9aeb156a61c`, has a different
dependency. Nested branches reload a single table entry. Reconstructing
the shared root and byte offset lets the original program translate.
On L4 its DXIL validates and its compute pipeline is created.

## Tile mask, ray and output in the prototype

In `32ed072b9f845048`, the table in SGPR 5 supplies the mask at +0 and
the output UAV at +0x40. The ray comes from a separate table in SGPR 4
at +0xa0. Two indexed loads from the same element read the origin and
distance limit at offset 0, then the packed direction at offset 20.
Each group of 32 ray indices shares one mask word. The result goes to
the UAV at the ray index. The bound view supplies the element stride.

An offline diagnostic retained these accesses, the tile mask and the
direction normalization. It accumulated the seven ray inputs into the
output to keep both loads observable. On L4, revision `106a53b` tracked
the four descriptor uses, validated the DXIL with SDK 10.0.26100.0 and
created the compute pipeline. This checks the surrounding accesses.
Validation of the original shader requires the RayQuery splice in
task 0ac4924f. The diagnostic and disassemblies stay outside the repo.
