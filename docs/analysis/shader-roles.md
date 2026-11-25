# Shader roles in a world frame

Use these program hashes to match a dropped draw or dispatch to the work it
would have done. The pass order comes from the Series S frame 805 in the
[PIX study][pix4], with hash identifications from neighbouring frames and
the [light-chain measurements][tiles]. In frame 805 the pre-pass covered
8.3% of the main depth and the G-buffer brought that to 11.8%. The material
resolve wrote scene colour over that same 11.8%. All 232 main deferred
rectangles ran and left the sampled scene colour at zero. A shader's role
identifies its place in the frame even when its inputs or outputs are empty.

Each row gives the first 16 hex digits of the program container hash and
the stage reported by the local record list or the system-library dispatch
notes. Evidence marked *code* is from reading the matching program locally.
High confidence means the evidence ties the program to the stated
operation or output. Medium means it supports
the role but leaves some details or the exact pass open. Low means the role
is tentative. Confidence is about the stated role, including any narrower
qualification in the row.

## Depth, geometry and material resolve

The PIX notes give pass totals for the depth pre-pass, decals and material
classification. They give individual hashes for only some of the geometry
work. The depth-only draws below come from a separate timing study, where
the pixel programs export depth without colour. Their exact depth passes
are open. The two skinned shadow programs have their own pass evidence.

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `f5257487f2945fdb` | Mesh | Supplies geometry to a depth-only draw. | [GPU frame §10.4][gpu104], code of its pixel partner | medium |
| `2edbd011d7110758` | Pixel | Computes the per-fragment depth of that draw. | [GPU frame §10.4][gpu104], code | high |
| `7972997d15fd9e69` | Mesh | Supplies geometry to an indirect depth-only draw. | [GPU frame §10.5][gpu105], code of its pixel partner | medium |
| `9f14b72117e2abec` | Pixel | Computes per-fragment depth for the indirect draw. | [GPU frame §10.5][gpu105], code | high |
| `f0c7efd2aceb2aaa` | Mesh | Supplies geometry to a depth-only mesh draw. | [GPU frame §10.5][gpu105], code of its pixel partner | medium |
| `7d85c9cab3c80721` | Pixel | Computes per-fragment depth for the mesh draw. | [GPU frame §10.5][gpu105], code | high |
| `910b4be059bfb28f` | Mesh | Emits skinned geometry for shadow-map depth. | [Refusals §6.2][ref62] | high |
| `321d411b20a35ef9` | Mesh | Emits another batch of skinned geometry for shadow-map depth. | [Refusals §6.2][ref62] | high |
| `48426bae580a080c` | Compute | Prepares four-component geometry records from indexed buffer data in the culling work. | [PIX §6.3][pix63], [Image status §4.1][image41], code | medium |
| `969c1cef8b783b39` | Compute | Builds 64-byte records from several input buffers in the culling work. | [PIX §6.3][pix63], [Image status §4.1][image41], code | medium |
| `db4835a364449a45` | Mesh | Builds mesh output and supplies a vertex colour used by the G-buffer pixel stage. | [PIX §11.2][pix112] | high |
| `09eb0a73df63b87d` | Pixel | Writes G-buffer material data, including albedo derived in part from vertex colours. | [PIX §7][pix7], [PIX §11.2][pix112] | high |
| `d42a2d8a9d2c954a` | Pixel | Writes material targets in the first G-buffer pass. | [PIX §7][pix7] | high |
| `6590393ed93ff071` | Pixel | Writes material targets in the first G-buffer pass. | [PIX §7][pix7] | high |
| `94d281151ca58b4f` | Pixel | Writes material targets in the first G-buffer pass. | [PIX §7][pix7] | high |
| `63051e97862e7dad` | Pixel | Writes later G-buffer material data, including albedo independent of the second vertex colour. | [PIX §7][pix7] | high |
| `57a27c5e2e1d62a9` | Pixel | Writes material data in a later G-buffer pass that adds albedo. | [PIX §7][pix7] | high |
| `f0c91b4d2578f5bf` | Compute | Resolves G-buffer and depth inputs into packed material targets, a material-class depth plane and an initial HDR scene colour. | [PIX §§4, 5.2][pix4], [Image status §4.1][image41], [Fallback §8.3][rtf83] | high |

## Local lights and deferred shading

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `d3e5b737ea7cc150` | Compute | Assigns lights to clusters in world space. | [Light chain §3.1][tiles] | high |
| `3173882c28a20b75` | Compute | Computes depth bounds on the 40×23 cluster tile grid. | [Light chain §3.1][tiles] | high |
| `e18931c72af42e46` | Compute | Reduces buffered inputs into a four-word lighting record. The record's consumer is uncertain. | [Light chain §3.1][tiles], [Refusals §6.3][ref63], code | low |
| `a7eeed0646be1ab7` | Compute | Zeros the per-cluster light masks before the light volumes are drawn. | [Light chain §3.1][tiles], [GPU frame §10.11][gpu1011] | high |
| `0f0b95cf52ddeb26` | Compute | Initialises the bounds for the shadow masks. | [Light chain §3.1][tiles], code | high |
| `a67d0bcde30cead3` | Vertex | Places light proxy geometry for rasterisation into the cluster grid. | [Light chain §3.1][tiles] | high |
| `7f4b62129572a14d` | Vertex | Places a second light proxy shape in the same cluster grid. | [Light chain §3.1][tiles] | high |
| `5ad3a82b1c3cc63a` | Pixel | Marks light coverage in cluster masks where the light proxies rasterise. | [Light chain §3.1][tiles], [GPU frame §10.11][gpu1011] | high |
| `7c90a988167650a1` | Compute | Reads the rasterised cluster buffer and prepares a secondary cluster buffer. | [Light chain §§3.1, 5][tiles] | high |
| `bad01f53f53248e5` | Compute | Classifies materials on the 80×45 grid and counts work for indirect lighting dispatches. | [Light chain §3.1][tiles], [PIX §11.3][pix113], [Ray tracing §1][rt1] | high |
| `11eeb27c2d15980d` | Compute | Merges cluster masks across each tile's depth range, filters excluded lights and writes the 13-slice tile mask. | [Light chain §§3.1–3.3][tiles], [Ray tracing §1][rt1] | high |
| `fea2b7d3f17b504f` | Compute | Builds coarse material classification over the 10×6 tile grid. | [Light chain §3.1][tiles], code, [Ray tracing §1][rt1] | high |
| `0a5ecfee6b79e13b` | Compute | Reads tile masks and queues shadow-ray work for individual lights. | [Light chain §3.1][tiles], code | high |
| `5cf00dc39042edaf` | Compute | Queues shadow-ray work in the same chain with additional shadow sampling and image writes. | [Light chain §3.1][tiles], code | high |
| `495c6013589afd60` | Compute | Converts the queued shadow-ray count into indirect dispatch arguments. | [Light chain §3.1][tiles], [Ray tracing §4][rt4] | high |
| `32ed072b9f845048` | Compute | Traces local-light shadows and supplies shadow distances for a deferred light type. | [PIX §5.3][pix53], [Lighting terms §4.4][terms44], [DXR §4.1][dxr41], [DXR trace addresses][r4tile], [Fallback §8.2][rtf82] | high |
| `559d9f0184e15fe3` | Compute | Traces the queued local-light shadow rays for checkerboard visibility masks. | [Light chain §3.1][tiles], [Ray tracing §2][rt2], [DXR §4.1][dxr41], [DXR trace addresses][r4dxr], [Fallback §8.2][rtf82] | high |
| `0ea6fac38d096770` | Compute | Filters the traced shadow results into the wide tile visibility texture. | [Light chain §3.1][tiles], code | high |
| `307e048e4cdd99d4` | Compute | Clears light bits from tile masks where the shadow visibility marks them occluded. | [Light chain §§3.1–3.2][tiles] | high |
| `fb9c7f430da14c4c` | Compute | Computes sky spherical-harmonic sums and lighting scale factors, including the directional-light scale read by deferred shading, from exposure, sky texture and constant inputs. | [Lighting terms §4.3][terms43] | high |
| `a5b1c338a9607317` | Compute | Writes the 16×1 ramp texture read by the directional-light diffuse term in deferred shading. | [Lighting terms §7][terms7], [Lighting terms §3][terms3] | high |
| `5741294cf872ffb9` | Compute | Writes the 80×45 R8 per-tile mask used to gate directional-light deferred rectangles. | [Lighting terms §4.3][terms43] | high |
| `f11c3ddeaeb7d602` | Compute | Reduces the 80×45 R8 directional-light tile mask to the 10×6 R8 mask read by the deferred-light rectangle vertex stage. | [Lighting terms §4.3][terms43] | high |
| `25ebc92075b62822` | Vertex | Places deferred-light rectangles over the material tiles tested against the class depth plane. | [PIX §5.1][pix51], [World black §8.2][black82] | high |
| `1cd9bc634417a981` | Pixel | Combines albedo with probe light, a directional-light term and masked local lights, adds a specular term, and scales by exposure for deferred scene colour. | [PIX §11.3][pix113], [Light chain §3.1][tiles], [Lighting terms §3][terms3], [Fallback §8.3][rtf83] | high |
| `1d2c970773ce2183` | Vertex | Places rectangles in a follow-up lighting pass. | [World black §8.2][black82], code | high |
| `e070816c3b889df0` | Pixel | Shades rectangles in that follow-up lighting pass. | [World black §8.2][black82] | high |

## Probe and traced lighting

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `39f0e9aeb156a61c` | Compute | Selects active screen-probe rays and prepares tracing work. | [Light chain §4][probes], [DXR trace addresses][r4probe] | high |
| `f22a76fcb967ee59` | Compute | Traces screen-probe rays against the acceleration structure and writes hit records for indirect diffuse lighting. | [Light chain §4][probes], [DXR §4.1][dxr41], [DXR trace addresses][r4probe], [Fallback §8.2][rtf82] | high |
| `b48d48f8304e5bd3` | Compute | Handles rays that missed and writes their contribution for probe injection. | [Light chain §4][probes] | high |
| `6bb493a39685ddcc` | Compute | Combines run-time ray results with the miss path and deposits them in the screen-probe atlas. | [Light chain §4][probes] | high |
| `8cb27ccf2d50c606` | Compute | Filters the probe atlas after injection. | [Light chain §4][probes], [PIX §11.3][pix113] | high |
| `17e736204a05a105` | Compute | Filters neighbouring probe-atlas texels and writes the later atlas outputs. | [Light chain §4][probes], code | high |
| `2dc364df5d3278b8` | Compute | Gathers filtered probe light into a full-resolution R11G11B10 lighting input for temporal resolve. | [Light chain §4][probes], [PIX §11.3][pix113] | high |
| `940c2635dc1312f4` | Compute | Supplies the other indirect final-gather path and its full-resolution lighting input. | [Light chain §4][probes], [PIX §11.3][pix113] | high |
| `34fb51b8a0312e82` | Compute | Accumulates the current probe lighting with history and writes packed indirect light for deferred shading. | [PIX §11.3][pix113], [Light chain §4][probes] | high |
| `dce085c07b9414b6` | Compute | Reads lighting textures and writes packed light that deferred shading samples through a shared-exponent view. | [PIX §11.4][pix114], code | medium |
| `71883e40620a567a` | Compute | Updates a packed lighting texture also written by the probe temporal resolve. Its separate contribution is open. | [PIX §11.4][pix114], code | medium |
| `f607780aa8e502d0` | Compute | Traces shadow rays for the irradiance cache. | [PIX §5.3][pix53], [Ray tracing §2][rt2], [DXR §4.1][dxr41], [DXR trace addresses][r4dxr], [Fallback §§3, 8.2][rtf3] | high |
| `3592426e3e9142d8` | Compute | Traces the primary gather for light probes. | [PIX §5.3][pix53], [Ray tracing §2][rt2], [DXR §4.1][dxr41], [DXR trace addresses][r4dxr], [Fallback §§3, 8.2][rtf3] | high |
| `36f04a4ed0ad1d9b` | Compute | Traces the probe gather for transparent surfaces. | [PIX §5.3][pix53], [Ray tracing §2][rt2], [DXR §4.1][dxr41], [DXR trace addresses][r4dxr], [Fallback §§3, 8.2][rtf3] | high |
| `b58eb3ca6ee46427` | Compute | Gathers ray records for the irradiance cache. | [PIX §5.3][pix53], [Ray tracing §2][rt2], [DXR §4.1][dxr41], [DXR trace addresses][r4dxr], [Fallback §§3, 8.2][rtf3] | high |
| `6d7fe7745ab9c6d8` | Compute | Traces reflection visibility for screen rays, including a use in transparent rendering. | [PIX §5.3][pix53], [Ray tracing §2][rt2], [DXR §4.1][dxr41], [DXR trace addresses][r4dxr], [Image comparison §4][fork4], [Fallback §8.2][rtf82] | high |
| `0873f80dd84a3c5c` | Compute | Traverses the acceleration structure and stores ray results. A frame binding and the result's consumer are open. | [Light chain §1][light1], code, [Image comparison §4][fork4] | medium |
| `caf96a3229145fa6` | Compute | Tests GPU particles for collisions against the acceleration structure. | [PIX §5.3][pix53], [Ray tracing §2][rt2], [DXR §4.1][dxr41], [Fallback §8.2][rtf82] | high |

## Ray tracing

The [stand-in study][rt2] tied nine of the thirteen traces it tracked to
dropped dispatches in frame 707. The four distant sun-shadow variants
were absent from that frame, which used shadow cascades. The tables below
add variants and related passes. The local-light traces are listed under
[local lights][local-lights], and the main gathers, reflection trace and
particle collision are under [probe and traced lighting][traced-lighting].

### Shadow traces and mask preparation

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `a1b20bc47e3c9597` | Compute | Traces sun-shadow visibility beyond the shadow cascades. | [DXR §4.1][dxr41], [Ray tracing §2][rt2] | high |
| `e8c962358d862e3f` | Compute | Traces sun-shadow visibility beyond the shadow cascades. | [DXR §4.1][dxr41], [Ray tracing §2][rt2] | high |
| `17829ac71aab4e8d` | Compute | Traces sun-shadow visibility beyond the shadow cascades. | [DXR §4.1][dxr41], [Ray tracing §2][rt2] | high |
| `afeea48bbb9913ad` | Compute | Traces sun-shadow visibility beyond the shadow cascades. | [DXR §4.1][dxr41], [Ray tracing §2][rt2] | high |
| `5f13c9550dc4ab39` | Compute | Traces irradiance-cache shadows when far-field tracing is disabled. | [Fallback §3][rtf3], code | high |
| `e22a5d3d6857a2f5` | Compute | Clears the local-light shadow-mask counters before indirect dispatch arguments are built. | [Ray tracing §§2, 4][rt4] | high |

### Probe and irradiance gathers

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `aeb93e0888250bd3` | Compute | Traces the primary light-probe gather with far-field tracing disabled. | [Ray tracing §3][rt3], [Fallback §3][rtf3], code | high |
| `a74a0746279f960d` | Compute | Traces the irradiance-cache gather with far-field tracing disabled. | [Ray tracing §3][rt3], [Fallback §3][rtf3], code | high |
| `3d121c93c7f9a057` | Compute | Traces the transparent-surface probe gather with far-field tracing disabled. | [Ray tracing §3][rt3], [Fallback §3][rtf3], code | high |
| `c519f7a7946e3702` | Compute | Copies gather data into the irradiance-cache atlas and writes auxiliary buffers. | [Ray tracing §4][rt4], [Fallback §8.3][rtf83] | high |
| `ab619d0b7b7684d2` | Compute | Reads the irradiance-cache atlas while mixing lighting inputs. | [Fallback §§5, 8.3][rtf83] | high |
| `6a62009bc6a4146e` | Compute | Mixes lighting inputs in the probe system. Its atlas read is open. | [Fallback §5][rtf5] | medium |

### Reflections

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `2cc4c197ee29e291` | Compute | Prepares tiles for reflection rays. The exact tile layout is open. | [Image comparison §§1, 4][fork4] | medium |

### BVH build and refit helpers

The [refusals study][ref62] identifies 34 compute programs from the
system library in the acceleration-structure list. The individual
kernel operations and their split between build and refit are open.
The title's instance-transform and skinning helpers are listed with the
[other programs from that study][refusal-roles].

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `3b90165a77d95cdf` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `f6e47efa50226beb` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `caa78b8ca62bc3d6` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `5552dac6a2b7a1fd` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `198f6dfd2b81bcea` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `5062bc7a48e842f8` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `5279ae36796c31fc` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `c42a31951201f9a0` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `162216b9cfd91aaf` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `28918b7150a7aad5` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `3df629ffc3d0a2a6` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `6f6a359d1166bb27` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `f28a33a89eaa2caa` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `4b5aedc0e96b0770` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `a63e2dfc25ba3155` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `bb55c842c391589f` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `cb9647e77864cb3a` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `7bb5b1b91d283a28` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `a197f50cd8481541` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `c85822de2e41cfaa` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `190fbf0ebe040169` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `4ddd6f9702fab2a7` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `5e68767498c57eb6` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `14394a322f6dd5c2` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `e63d22c4def9ba6a` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `e95858837a9aa22d` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `f6eb6498409ca974` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `3cea4534bbc15632` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `894c91654720595f` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `9205d2655bf8cc88` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `35e51bb27a463294` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `5c09eb42b7a3c362` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `f94e5d8bf4e1cf05` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |
| `ffd93c8a029ff016` | Compute | Runs a kernel in the system acceleration-structure builder. | [Refusals §6.3][ref63] | medium |

### Stand-in and fallback paths

XTP's [miss stand-in][rt1] fills outputs for dropped traces under the
original dispatch hashes. The voxel and fallback variants below have
their own hashes. The [fallback A/B][rtf82] observed the primary-probe
and transparent-probe fallback dispatches and [measured their
outputs][rtf83]. The irradiance cache skips its gather and shadow
dispatches on that path.

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `1a1c5c02e73751b6` | Compute | Uses the voxel scene for the primary light-probe gather. | [Ray tracing §3][rt3] | medium |
| `64673ea7805c27be` | Compute | Uses the voxel scene for the irradiance-cache gather. | [Ray tracing §3][rt3] | medium |
| `d7752c9f275803d8` | Compute | Uses the voxel scene for the transparent-surface probe gather. | [Ray tracing §3][rt3] | medium |
| `05767b5d544b0c7a` | Compute | Uses the voxel scene for the screen-probe rays. | [Ray tracing §3][rt3] | medium |
| `12f7da9158b1ce24` | Compute | Runs the voxel variant of the deferred gather. Its outputs are open. | [Ray tracing §3][rt3] | medium |
| `81560230fcde2b0a` | Compute | Runs the primary-probe fallback gather and writes a single-channel gather texture. | [Fallback §§3, 8.3][rtf83] | high |
| `29dc558346584906` | Compute | Gathers and shades the fallback probes for transparent surfaces, writing the two 32×16 textures. | [Ray tracing §3][rt3], [Fallback §8.3][rtf83] | high |
| `a953d5b93efe8424` | Compute | Supplies the fallback variant for screen-probe rays. Its outputs are open. | [Ray tracing §3][rt3], [Fallback §4][rtf4] | medium |
| `62543c14e97e7ed7` | Compute | Supplies the fallback variant of the deferred gather. Its outputs are open. | [Ray tracing §3][rt3] | medium |

## Sky, fog and post-processing

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `53e3931a17a13ac5` | Compute | Builds the atmospheric transmittance lookup texture. | [World black §8.3][black83] | high |
| `545138fdb1d94e1b` | Compute | Builds the multiple-scattering lookup texture used in sky preparation. | [World black §8.3][black83] | high |
| `26b8bbb2e7ae4dc6` | Compute | Uses transmittance and multiple scattering to generate the 192×108 sky-view lookup textures. | [World black §8.3][black83] | high |
| `348e1e8c414c7eca` | Compute | Evaluates the sky from scene depth, exposure and atmospheric lookup textures and writes scene colour. | [PIX §4][pix4], [World black §8.2][black82], [Fallback §8.3][rtf83] | high |
| `d91fdb5004fdadf1` | Vertex | Places the sky dome geometry. | [World black §8.2][black82] | high |
| `3ae25b11290cadde` | Pixel | Shades the rasterised sky dome. | [World black §§8.2, 8.4][black82] | high |
| `6e750a37b1da6949` | Vertex | Places geometry for the cloud draw. | [World black §8.2][black82] | high |
| `d7b97830ede61511` | Pixel | Shades the clouds over the sky. | [World black §8.2][black82] | high |
| `d9978c76c746d809` | Compute | Reconstructs part of the 480×272 volumetric fog input from sampled textures. | [PIX §4][pix4], [World black §10][black10], code | high |
| `982f3cccd5c04294` | Compute | Reconstructs the companion fog input at the same resolution. The output split between the two dispatches is open. | [PIX §4][pix4], [World black §§10, 10.2][black10], code | high |
| `577248728544d74b` | Compute | Composites depth-dependent fog scattering and transmittance into HDR scene colour. | [PIX §4][pix4], [World black §10.2][black102] | high |
| `c5ffa556ab926155` | Mesh | Supplies geometry to the forward pass after fog application. | [World black §8.2][black82] | high |
| `c4a269f4eeba3cac` | Pixel | Shades that forward geometry into scene colour. | [World black §8.2][black82] | high |
| `2da4beb386509b77` | Compute | Builds a depth-weighted colour pyramid for refraction by transparent materials. | [World black §§9.2, 9.6][black92] | high |
| `731a39171b568298` | Compute | Composites the scene before upscaling, reading half-resolution inputs and writing scene colour and motion vectors. | [PIX §4][pix4], [World black §§8.2, 9.6][black82] | high |
| `3fe821a8916a4564` | Compute | Updates the small exposure record read by later passes. | [World black §4.3][black43] | high |
| `30ee260ef303ac7b` | Vertex | Places the full-screen draw that prepares the half-resolution depth-of-field layer. | [World black §9.6][black96], code | high |
| `f987c56098b9faff` | Pixel | Writes the depth-of-field colour layer with the circle of confusion in alpha. | [World black §9.6][black96] | high |
| `44fdeb707f882b7d` | Compute | Applies the first blur step to the depth-of-field layer. | [World black §9.6][black96] | high |
| `03c01507703d7449` | Compute | Applies the following blur step before depth-of-field compositing. | [World black §9.6][black96] | high |
| `5ec7617b0f7cb43c` | Vertex | Places the full-screen depth-of-field composite draw. | [World black §9.6][black96], code | high |
| `813a7ead7f09ebd7` | Pixel | Uses depth to composite the blurred layer into the full-size image. | [World black §9.6][black96] | high |
| `51e19585b4753814` | Compute | Reduces sampled colour across neighbouring lanes and writes two image outputs, including a 960×540 RGBA16F target. The downstream pass is open. | [World black §8.1][black81], code | medium |

Two pixel programs in the pack sample a colour texture, apply a nonlinear
curve and export a single colour. Their behaviour fits the tone-mapping
stage. The frame notes give only its RGB10A2 target and draw count.

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `a2530e1f5fec64a3` | Pixel | Applies a nonlinear display curve to sampled colour, a tone-mapping candidate. | Code, [PIX §4][pix4] for pass context | medium |
| `b70bf4564303b0ee` | Pixel | Combines multiple texture samples and applies a nonlinear display curve, a tone-mapping candidate. | Code, [PIX §4][pix4] for pass context | medium |

## Other programs in the GPU timing study

The timing tables identify the programs and their draw or dispatch kinds.
The broader descriptions here use local code where the measurements give
no output bindings.

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `ba8d52f4ea1a1bac` | Mesh | Supplies geometry for a draw shaded with clustered local lights. | [GPU frame §10.7][gpu107] | high |
| `b37565055f2f7d3b` | Pixel | Uses depth to select a light cell and shades the lights in its mask, within the uploaded index range. | [GPU frame §§10.7, 10.11][gpu1011] | high |
| `24f989b98dcb03f0` | Compute | Transforms vertex data from buffers in the 32-thread transform family. | [GPU frame §10.7][gpu107] | high |
| `3a79195f06d3ed85` | Compute | Transforms vertex data and writes three-component results. | [GPU frame §10.7][gpu107] | high |
| `ebe4e53f031f5249` | Compute | Transforms vertex data from buffers in the 32-thread transform family. | [GPU frame §10.7][gpu107] | medium |
| `12eae112b559091c` | Compute | Transforms vertex data from buffers in the 32-thread transform family. | [GPU frame §10.7][gpu107] | high |
| `512416d975ace48b` | Compute | Transforms vertex data from buffers in the 32-thread transform family. | [GPU frame §10.7][gpu107] | medium |
| `53af89c0e1dccd2a` | Compute | Processes indexed records and writes several output buffer fields. The consumers are open. | [GPU frame §4.2][gpu42], code | low |
| `a1d865bdfb3559eb` | Compute | Transforms values gathered from arrays and writes structured buffer results. | [GPU frame §4.2][gpu42], code | low |
| `42683eebec081520` | Compute | Filters and combines image samples, sharing values across lanes before writing several image outputs. | [GPU frame §4.2][gpu42], code | medium |
| `aaf7da7a94dc72a9` | Compute | Reads individual 16-bit buffer values and writes packed four-component results. The buffer's purpose is open. | [GPU frame §4.2][gpu42], code | medium |
| `8bbcbda6008eaa37` | Compute | Evaluates tiled lighting, sampling textures and shadow comparisons before writing image outputs. | [GPU frame §§4.2, 10.11][gpu1011], code | medium |
| `776d835da5ffa646` | Compute | Evaluates the companion tiled-lighting path with the same image access pattern. | [GPU frame §§4.2, 10.11][gpu1011], code | medium |
| `87df99711607dd60` | Compute | Evaluates a related tiled-lighting path that also reads three-component buffer values. | [GPU frame §10.11][gpu1011], code | medium |
| `915ed81ea580a276` | Compute | Evaluates the companion tiled-lighting path with the additional buffer reads. | [GPU frame §10.11][gpu1011], code | medium |
| `660814153af43185` | Compute | Samples textures and writes work records, updating counters and bounds in buffers. The consumer is open. | [GPU frame §10.5][gpu105], code | low |
| `f8f50ab85e7b10bb` | Compute | Samples textures and appends results to buffers for later work. The consumer is open. | [GPU frame §10.5][gpu105], code | low |
| `c9da71da40af2e28` | Compute | Builds output buffer records and updates their counters during indirect work. | [GPU frame §10.5][gpu105], code | low |
| `6a04b371a8e26853` | Compute | Initialises packed buffer entries without sampling input data. | [GPU frame §10.5][gpu105], code | medium |
| `7de0f9db96ddd295` | Compute | Reduces packed buffer values within a group and writes a packed result. | [GPU frame §10.5][gpu105], code | medium |
| `1d65098be346d062` | Vertex | Places a full-screen draw. | [GPU frame §10.4][gpu104] | high |
| `5510a9fd27ceb4b1` | Pixel | Filters texture samples into one colour target in that full-screen draw. The effect is open. | [GPU frame §10.4][gpu104], code | medium |
| `32c5e0e62380b489` | Mesh | Supplies geometry to a draw with one colour output. | [GPU frame §10.5][gpu105], code of its pixel partner | medium |
| `4c7bfa5c7bc65187` | Pixel | Samples one image and writes one colour attachment for that mesh draw. | [GPU frame §10.5][gpu105], code | medium |
| `37f363f0a5dcd806` | Mesh | Supplies geometry for a draw writing six material targets. | [GPU frame §10.5][gpu105], code of its pixel partner | medium |
| `4a45195a99ac14f7` | Pixel | Samples material textures and writes six G-buffer-like targets. | [GPU frame §10.5][gpu105], code | medium |
| `c962201cddbdd7dd` | Mesh | Supplies geometry for another draw writing six material targets. | [GPU frame §10.5][gpu105], code of its pixel partner | medium |
| `e5e9af2bbd2c006a` | Pixel | Samples material textures and writes six G-buffer-like targets for that draw. | [GPU frame §10.5][gpu105], code | medium |
| `a26422c01dcf5612` | Mesh | Supplies geometry for a draw that samples lighting and writes two colour targets. | [GPU frame §10.5][gpu105], code of its pixel partner | medium |
| `6a3a8d3fe52b4eab` | Pixel | Samples material and shadow data and writes two colour targets for a lit mesh draw. | [GPU frame §10.5][gpu105], code | medium |
| `8f690b0f5dbbce02` | Mesh | Supplies geometry for an indirect draw writing several material targets. | [GPU frame §10.5][gpu105], code of its pixel partner | medium |
| `271a87c54ac648a5` | Pixel | May discard fragments and writes five G-buffer-like targets for that draw. | [GPU frame §10.5][gpu105], code | medium |
| `a467d848ace5810d` | Pixel | Computes one colour output from interpolated inputs. The effect is open. | [GPU frame §10.11][gpu1011], code | low |

## Other programs named in the refusals study

These roles come from other world frames and include work that prepares
geometry or lighting before the main passes. Pass and output associations
come from the capture notes. The system library's acceleration-structure
programs are listed in the [BVH table][bvh-roles].

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `55679a0547649d0b` | Compute | Filters instances for each view and prepares lists and indirect draw arguments. | [Refusals §6.3][ref63] | medium |
| `17af15c4015cb4b6` | Compute | Filters another instance batch and prepares per-view indirect draw work. | [Refusals §6.3][ref63] | medium |
| `1c4b3c0547e3e6fa` | Compute | Produces an instance list and draw arguments for a per-view culling batch. | [Refusals §6.3][ref63] | medium |
| `42f717f1b4d18e94` | Compute | Filters a batch of instances before geometry and shadow draws. | [Refusals §6.3][ref63] | medium |
| `89d08e561897d4e5` | Compute | Builds draw work for a batch of visible instances. | [Refusals §6.3][ref63] | medium |
| `9209777e8face9dd` | Compute | Filters a separate per-view instance batch into draw work. | [Refusals §6.3][ref63] | medium |
| `fdd56d97a806bcca` | Compute | Prepares instance and indirect argument buffers in the culling lists. | [Refusals §6.3][ref63] | medium |
| `adc5c689b0552cc2` | Mesh | Emits foliage geometry for the G-buffer. | [Refusals §6.3][ref63] | medium |
| `ef5409053e14aa82` | Mesh | Emits character geometry for G-buffer shading. | [Refusals §6.3][ref63] | medium |
| `903776ba469c8bac` | Mesh | Emits geometry for G-buffer draws. | [Refusals §6.3][ref63] | medium |
| `4e35b816f2f91d68` | Vertex | Transforms vertices for a distant geometry draw in the G-buffer. | [Refusals §6.3][ref63] | medium |
| `45f081396c2c59ba` | Mesh | Emits distant geometry from a lower-detail mesh. | [Refusals §6.3][ref63] | medium |
| `89a622617919c92a` | Mesh | Emits foliage geometry for the G-buffer. | [Refusals §6.3][ref63] | medium |
| `b3ba0ef9ee352720` | Vertex | Transforms vertices for a distant geometry draw in the G-buffer. | [Refusals §6.3][ref63] | medium |
| `093c64546c55554e` | Mesh | Emits character geometry for G-buffer shading. | [Refusals §6.3][ref63] | medium |
| `5bd95fdec765c0d6` | Mesh | Emits character geometry for G-buffer shading. | [Refusals §6.3][ref63] | medium |
| `fe660f2d6d57de76` | Mesh | Emits geometry for G-buffer draws. | [Refusals §6.3][ref63] | medium |
| `a402e650d0acbc92` | Pixel | Shades character pixels into the G-buffer material targets. | [Refusals §6.3][ref63] | medium |
| `9aff566ca0fa7c98` | Mesh | Emits geometry for G-buffer draws. | [Refusals §6.3][ref63] | medium |
| `c0f0db84d228f441` | Mesh | Emits geometry for G-buffer draws. | [Refusals §6.3][ref63] | medium |
| `4ee6e34d8e008c63` | Mesh | Emits character geometry for G-buffer shading. | [Refusals §6.3][ref63] | medium |
| `e819d0b31c213166` | Pixel | Shades geometry into the G-buffer material targets. | [Refusals §6.3][ref63] | medium |
| `f841ad74ec6e7367` | Mesh | Emits character geometry for G-buffer shading. | [Refusals §6.3][ref63] | medium |
| `46395a33fdf2a3fa` | Pixel | Writes G-buffer material data for character geometry. | [Refusals §6.3][ref63] | medium |
| `0979c5189155cf3f` | Mesh | Emits character geometry for G-buffer shading. | [Refusals §6.3][ref63] | medium |
| `09a88636ff0d843c` | Mesh | Emits character geometry for G-buffer shading. | [Refusals §6.3][ref63] | medium |
| `235e56869c1ea883` | Mesh | Emits character geometry for G-buffer shading. | [Refusals §6.3][ref63] | medium |
| `c3ba22ec6fc5dd4a` | Mesh | Emits geometry in the character and general G-buffer lists. | [Refusals §6.3][ref63] | medium |
| `a223e485dd8247a3` | Pixel | Writes G-buffer material data for character geometry. | [Refusals §6.3][ref63] | medium |
| `36e010b53acffd77` | Compute | Skins character vertices for geometry and shadow passes. | [Refusals §6.3][ref63] | medium |
| `2bba29bbd7c019bc` | Compute | Skins character vertices for geometry and shadow passes. | [Refusals §6.3][ref63] | medium |
| `a31407f77c16d796` | Compute | Skins character vertices for geometry and shadow passes. | [Refusals §6.3][ref63] | medium |
| `a1bf269d5b19f025` | Compute | Computes scattering for the lighting passes and contributes to their image targets. | [Refusals §6.3][ref63] | medium |
| `6b6db9f1a526daeb` | Pixel | Writes into the small depth and surface map used for puddle detection. | [Refusals §6.3][ref63] | medium |
| `73644c6a0b7b5b35` | Pixel | Shades transparent geometry into HDR scene colour. | [Refusals §6.3][ref63] | medium |
| `2fd01fca16993001` | Pixel | Writes into the small depth and surface map used for puddle detection. | [Refusals §6.3][ref63] | medium |
| `5c6224a6bc3a4066` | Compute | Uses shared memory to process an image input and write a transparency-pass image. | [Refusals §6.3][ref63], code | medium |
| `97175c2c1a214a2f` | Pixel | Shades fragments in the alpha pre-pass and scattering list. | [Refusals §6.3][ref63] | medium |
| `b22a16a178976e1b` | Compute | Runs matrix products in a buffer-based compute graph. | [Refusals §6.3][ref63] | medium |
| `098fc28e44347a00` | Compute | Updates buffers for hair strands. The exact strand operation is open. | [Refusals §6.3][ref63] | medium |
| `f7c4f227a2a4c4b2` | Compute | Updates buffers for hair strands. The exact strand operation is open. | [Refusals §6.3][ref63] | medium |
| `b3e7781d53ff8326` | Compute | Updates buffers for hair strands. The exact strand operation is open. | [Refusals §6.3][ref63] | medium |
| `02384a60fda6f2d0` | Compute | Processes buffers in the visual-effects pass. The consumers are open. | [Refusals §6.3][ref63] | low |
| `aab1b3c7248ee5de` | Compute | Queues work records and writes an image result in the traced-lighting lists. The downstream consumer is open. | [Refusals §6.3][ref63], code | low |
| `e30970f1727063fd` | Compute | Prepares transforms for impostor instances in the ray-tracing acceleration structure. | [Refusals §6.3][ref63], [Refusals §6.2][ref62] | medium |
| `82a2ad6d07417d36` | Compute | Skins geometry into vertex buffers for the ray-tracing acceleration structure. | [Refusals §6.3][ref63] | medium |
| `90a0692af4320129` | Compute | Skins geometry into vertex buffers for the ray-tracing acceleration structure. | [Refusals §6.3][ref63] | medium |
| `21d27f785da6d2b1` | Compute | Processes indexed records and writes vector results to buffers. The pass is open. | [Refusals §6.1][ref61], code | low |
| `77a7af30c430cd16` | Compute | Samples textures while processing indexed buffer records and writing vector results. The pass is open. | [Refusals §6.1][ref61], code | low |
| `9aa4a785e0800cda` | Compute | Writes related vector records after reading buffers and sampled textures. The pass is open. | [Refusals §6.1][ref61], code | low |
| `5ed77e9e8f084dea` | Compute | Redistributes an image input through group memory into image tiles and buffer outputs. The pass is open. | [Refusals §6.1][ref61], code | low |
| `0a5d4f9047b3dce4` | Compute | Stages image values in group memory and writes several buffer results. The pass is open. | [Refusals §6.1][ref61], code | low |

### Material shading at ray hits

The refusals study places these compute programs in the ray-hit shading
lists that feed the 640×360 R11G11B10 targets. The evidence identifies
the shared role at pass level. It leaves individual material variants open.

| Hash | Stage | Role | Evidence | Confidence |
|---|---|---|---|---|
| `535f8a954d8ec638` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `3e7e04c03727b40c` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `df1c34cb6d9dbcfa` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `46539424133f9b74` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `02a4ed10c682ac3a` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `29be03ccaa4e353a` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `2a5cb98cf0749a82` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `2ec1dd27e95fdd16` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `2f12d0f639fb7ed0` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `34121970d8a2e435` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `355f182eb23a2ccf` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `571bf6d258f65ed0` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `8f8dd4e691330c74` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `479975ca9ff7e370` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `adc76ddaa3a13d20` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `d84c0ca92452e526` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `e0ba27015903a5f3` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `e90b8dcb8b4cfc6b` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `efc770e7f2c27b03` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `f6da51317fcef92e` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `f91ef42e5b381cb9` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `06989306fb3b54c2` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `072f5fe96efd27e1` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `0b3d156291158496` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `16622000e3a30bf0` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `203f78efece5bbdf` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `3fa1fda35402c93f` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `52ab310c43b9f99c` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `649850fad69f9228` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `74930ddd74e36464` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `a1266cc7391ae0d6` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `a4df980a1c65fdbe` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `a62731e90739f195` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `ad1cf51b8a6c7096` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `c3cb00c582d26394` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `de4e3af66d68d980` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `dffafa6052629c85` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `05ef21a2132db12d` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `1c02f8131c508c75` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `7f05ddab771799a9` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `d2c52fcd936985b4` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `d5ef4f2bb16d9fa3` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |
| `ad17f5c4f7acac25` | Compute | Shades a material at ray hits for the half-resolution lighting group. | [Refusals §6.3][ref63] | medium |

## Coverage of the frame

The frame summary does not enumerate the hashes bound in its main depth
pre-pass, decal and material-class draws, half-resolution lighting group,
tone-mapping draws or scene and interface planes. Of the 183 pixel programs
in the main deferred pass, it names one. It also leaves the screen-probe
hit shaders unnamed. A pack record can establish a stage or a likely role,
but identifying these frame bindings requires the per-draw or per-dispatch
pipeline records. The bilinear upscaler in the measured frame is XTP's
[FidelityFX stand-in][fx], so it has no title program hash in this map.

[pix4]: pix-world-frame.md#4-the-passes-frame-805
[pix51]: pix-world-frame.md#51-the-lighting-pass
[pix53]: pix-world-frame.md#53-ray-tracing-in-the-frame
[pix63]: pix-world-frame.md#63-what-is-dropped
[pix7]: pix-world-frame.md#7-the-albedo-of-the-first-g-buffer-pass
[pix112]: pix-world-frame.md#112-albedo-color2-is-read-as-the-title-writes-it
[pix113]: pix-world-frame.md#113-lighting-the-light-it-is-given-is-0-at-its-writer
[pix114]: pix-world-frame.md#114-the-r9g9b9e5-decodings-run-before-their-writers-watch-item-360
[light1]: light-chain.md#1-verdict
[tiles]: light-chain.md#31-the-chain-frame-675-of-imgsrc-1
[probes]: light-chain.md#4-the-screen-probe-light-ds-view-s01008
[image41]: image-status.md#41-depth-albedo-lighting
[terms3]: lighting-terms.md#3-the-lighting-pass-and-its-program
[terms43]: lighting-terms.md#43-the-directional-light
[terms44]: lighting-terms.md#44-the-lamps
[terms7]: lighting-terms.md#7-sessions-lightterms-1-and-lightterms-2
[local-lights]: #local-lights-and-deferred-shading
[traced-lighting]: #probe-and-traced-lighting
[refusal-roles]: #other-programs-named-in-the-refusals-study
[bvh-roles]: #bvh-build-and-refit-helpers
[rt1]: rt-standin.md#1-summary
[rt2]: rt-standin.md#2-the-world-frame-pass-by-pass
[rt3]: rt-standin.md#3-the-titles-own-switch
[rt4]: rt-standin.md#4-what-the-lighting-reads-from-the-traces
[dxr41]: rt-dxr.md#41-two-families
[r4dxr]: r4-dxr.md
[r4probe]: r4-dxr.md#screen-probe-rays
[r4tile]: r4-dxr.md#tile-mask-ray-and-output-in-the-prototype
[rtf3]: rt-fallback.md#3-the-probe-systems-dispatch-steps
[rtf4]: rt-fallback.md#4-the-other-callers
[rtf5]: rt-fallback.md#5-forcing-the-fallback-xtp_rt_path
[rtf82]: rt-fallback.md#82-the-path-changes
[rtf83]: rt-fallback.md#83-probes-irradiance-cache-lighting
[fork4]: fork-image-diff.md#4-zero-lighting
[gpu104]: gpu-frame.md#104-the-frame-and-the-gpu-by-kind
[gpu105]: gpu-frame.md#105-by-program-the-top-20
[gpu107]: gpu-frame.md#107-the-top-costs-why-and-who-owns-them
[gpu42]: gpu-frame.md#42-by-program
[gpu1011]: gpu-frame.md#1011-n6b-the-bounds-of-the-pixel-light-loop
[black43]: world-black.md#43-leads-checked-and-set-aside
[black81]: world-black.md#81-the-open-question
[black82]: world-black.md#82-the-scene-colours-writers
[black83]: world-black.md#83-the-lead-the-skys-view-luts-192108
[black92]: world-black.md#92-what-it-does
[black96]: world-black.md#96-session-present-1-0046-series-s
[black10]: world-black.md#10-volumetric-fog-reconstruction-and-the-dark-world
[black102]: world-black.md#102-what-the-fog-dispatch-actually-reads
[ref61]: refusals.md#61-sessions-read
[ref62]: refusals.md#62-the-frames-lists-and-what-is-dropped-in-each
[ref63]: refusals.md#63-ranked-the-refused-programs-that-run-in-the-world
[fx]: fidelityfx.md
