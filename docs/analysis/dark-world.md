# The dark world: from the 1440p targets to the presented image

Why Grand Theft Auto VI's world reaches the screen black under XTP, what was
fixed, and what still blocks it. Written on 2026-11-24 from XTP's own runs on
the person's RTX 4080 SUPER (`E:\xtp-data\runs`, never in the repository),
the VIctory community's reports, and a reading of the title's code (build
blake3 `b99f876b74d4a77f…`; addresses are RVAs relative to
`game_scarlett_master.exe`; findings are described, no title bytes are
quoted). It answers `gpu.md` §6.1 in part and open question 3.

## 1. Summary

- In Series X mode (XTP's default) the title renders its scene at
  2560×1440 and writes a 3840×2160 output, which it presents as plane 0.
  The 1440p scene targets are rendered: VIctory issue #841 dumped them full
  of a street scene while the 4K output held 0 to 1 of 255. One tester put
  a 1440p target on screen instead of the 4K one and saw the world.
- The step between them is the title's **own upscaler**, not FidelityFX:
  the title picks FidelityFX (FSR) only on a Series S, and XTP's runs never
  load the FSR provider DLL (section 3).
- That upscaler, like the rest of the frame, reads the scene through views
  of render-target memory. Before this change, a texture made at the address
  of a render target (a placed view of its memory, a resource placed at the
  same heap offset) got a **host copy of its own filled from title memory**,
  which the GPU never writes: zeros. A pass reading the scene that way writes
  black (#841's 4K output: a full-screen write of 0 to 1). This is fixed
  (VIctory #643, reworked, section 5.1).
- `PresentX` with several planes now composes them (VIctory #211, section
  5.2). Series X presents one plane; Series S presents two.
- Still blocking on this machine, outside this task's files: compute
  programs refused on NVIDIA for their 64-lane waves (118 in the first runs,
  shader L1, being ported), and, at the time of writing, a stall on the
  loading screen that keeps every session out of the world (section 7).

## 2. The chain, as the title runs it (Series X mode)

| Step | What | Evidence |
|---|---|---|
| 1 | Scene targets at 2560×1440: colour in R8G8B8A8 (f28) and B8G8R8A8 (f87), R10G10B10A2 (f24), depth, and two NVIDIA-refused formats (B5G5R5A1, B4G4R4A4) that now take stand-ins (gpu-nvidia's delivery). | #841's dumps (frames 2000 to 2200: means 139 to 181 of 255); XTP's first runs' refusals |
| 2 | The upscaler: 1440p to the 3840×2160 R8G8B8A8 output. The title's own (type 6) in Series X mode. | Code reading, section 3 |
| 3 | The interface (title card, subtitles, HUD) drawn into the 4K output. | Front-end dumps of run `play-F`: the 4K target `0x19cff9472b0` holds the profile screen and its button prompts |
| 4 | `PresentX`: plane 0's first resource, one of two 4K back buffers in turn (`0x19bec6a0340`, `0x19bec69f3a0` in `play-F`), copied to the swap chain. | `queue::present`; the presented frames match the 4K target in the front end |

In the world the chain breaks between steps 1 and 2: the 4K output is
written (a full-screen pass leaves 0 to 1 of 255 in it, not the cleared 0)
but from black input; the interface of step 3 then draws over black, which
is the README's "the title card shows, and the scene after it stays black"
on AMD.

## 3. Which upscaler runs (code reading)

- The function at `0x7e93f0` returns the upscaler type the renderer uses;
  `0x7e93b0` maps it to an object: type 6 is the title's own upscaler
  (constructed at `0x56d9d80`, function table at `0x6300bf0`), type 7 the
  FidelityFX one (constructed at `0x56d9c20`, table at `0x6300b50`; its
  third method, `0x28ef1d0`, calls `ffxCreateContext` at `0x28ef2cc` and
  `ffxQuery` after it). Any other type means no upscaling: the render size
  is the output size.
- Type 7 is chosen only when `0x75140` is true **and** bit `0x40` of a flag
  word (computed once by `0x56dcf00`, cached at `0x7b9846c`) is set.
  `0x75140` is "the device type is 6": `0x55deb88` reads
  `HKLM\Software\Microsoft\Windows NT\CurrentVersion\OEM` and maps its
  console type value 0x60 to 6 (Series S), 0x64 to 7 (Series X), 0x68 to 8
  (development kit). So **FSR runs on a Series S only**; on a Series X the
  title's own upscaler writes the 4K output.
- Consistent with that: `amd_fidelityfx_loader_xbox.dll` is imported (mapped
  at start) in every run, but `amd_fidelityfx_upscaler_xbox.dll` is never
  loaded. The loader loads its providers by name (`GetModuleHandleA`, then
  `LoadLibraryA`) on the first `ffx*` call, and XTP logs every DLL the title
  loads late (`title dll … (loaded by the title)`); no run has that line.
  A missing FSR provider is therefore not the cause in Series X mode.

## 4. Where it breaks in XTP

### 4.1 Aliases of render targets read zeros (fixed)

- XTP gives each title texture a host copy. What the GPU renders or writes
  stays in that copy; title memory keeps its old bytes (zeros for a target).
- The title reaches the same memory through more than one object. It makes
  shader views of memory without a resource object
  (`CreatePlacedShaderResourceViewX`, device method 0x230, by address and
  description: `resource::texture_at`) and places resources over memory it
  manages itself. XTP's world logs show placed views everywhere in the
  world's draws (run `B-save`'s device-removed report: dozens per draw).
- Each such object used to get a host copy of its own. Its contents came
  from title memory (`texture::title_written`: a texture without
  render-target, depth or unordered-access use is filled from memory), so
  a view of a render target read zeros. A pass reading the scene that way
  writes black with whatever it adds (dither, film grain): #841's 0-to-1
  output.
- Testers with VIctory #643 on an RX 9070 XT saw the "Rockstar Games
  presents" card for the first time, and cutscene subtitles stopped piling
  up on each other: aliases had been resolving to the wrong copy.

### 4.2 Work dropped on NVIDIA (not this task)

- 118 compute pipelines refused (`E_INVALIDARG`) for their 64-lane waves on
  this 32-lane GPU (run-world §5); 115 dispatches per frame dropped. If the
  upscaler's or the tone mapper's programs are among them, their output
  stays black on NVIDIA whatever the aliases do. The wave64 port (shader L1)
  is the fix.
- Two 2560×1440 targets (formats 86 and 115) had no host texture in the
  first runs; gpu-nvidia's delivery gives them R8G8B8A8 stand-ins.

### 4.3 Not tied to the image

- **VIctory #469** (a depth target with unordered access gets a twin
  texture for its UAVs) and **#206** (a structured view off an element of
  its host copy becomes a raw view): the runs log each case once
  (`view the host does not allow unordered=true format=53`,
  `buffer view not on an element of its host copy`), and both make null
  views. #841's scene renders fully at 1440p on a tree with neither, and
  nothing in the captures ties them to the black output. They are
  stability fixes (watch §4.1 item 9, the NVIDIA world set); their files
  (`host.rs`, `list.rs`, `descriptor/host.rs`) belong to other tasks. Not
  ported here.

## 5. What was changed

### 5.1 Textures at one address share the GPU's copy (VIctory #643, nathanbzrr)

`resource/alias.rs` (new) and `resource.rs`:
- Every live texture whose own description asks for GPU writes (render
  target, depth, unordered access) is registered by address with its host
  copy.
- A texture made later at that address (`texture_at`, a placed resource, a
  resource at an address) takes that copy when the host can view it as
  asked: same shape (dimension, size, array or depth, mips, samples) and
  format family; only the writes the copy allows; shader reads only if the
  copy allows them; a writable alias of a typed copy in the copy's format.
  The copy's tracked state is shared with it.
- `texture_at` keys its cache by a canonical description (the two padding
  words zeroed: titles pass descriptions from the stack). When a GPU-written
  texture comes to an address after a placed view of it was made, or leaves
  it, the next use of the view looks again (a counter of registry changes
  makes this one comparison when nothing changed) and makes a new resource
  over the right copy.
- Diagnostics builds log the first 64 aliases met (then every power of two)
  with what became of them, and `XTP_ALIAS_SHARING=0` turns sharing off for
  A/B runs.

Tests (GPU, on the RTX 4080; skipped without a device): a render target
cleared by a title list, then read through a placed view (twice, with
different padding: one resource) and through a resource at its address,
returns the cleared texels; aliases of another shape, family or format, or
asking for writes the copy lacks, keep their own copy, and a read-only
alias is no source once the target is released; a placed view made before
its target follows it. Plus padding canonicalization (no GPU).

**Critical review of #643.**
- *Kept:* the mechanism (look up a live GPU-written copy by address), the
  compatibility rules, canonical resource descriptions, the re-check in
  `texture_at`, the tests' scenario.
- *Changed: what is a source.* The PR registers every resource that ends up
  with a writable host copy, including read-only aliases that merely share
  one. `texture_at` never releases its resources, so such an alias stays a
  source for the run: after the target is released and the memory reused
  (a streamed texture of the same shape, say), the new texture would read
  the old target instead of its own data. Here only resources whose own
  description asks for GPU writes are sources.
- *Changed: no sharing between title-written textures.* The PR falls back
  to sharing a read-only copy. That brings nothing (each fills from the same
  memory) and is wrong when two descriptions at one address have different
  console layouts: both would detile into one copy. Removed.
- *Changed: cost of the re-check.* The PR calls the registry (a lock and a
  map lookup, plus `GetDesc`) on every `texture_at` hit whose copy is not
  writable, which is every streamed texture on every draw. Here it runs
  only after the registry changed.
- *Changed: a released target's alias.* The PR's re-check upgrades only a
  view whose copy is not writable; a view over a released target whose
  address now holds a new target kept the old copy. Here a view follows
  whatever copy the registry gives.
- *Not taken: canonical view descriptions* (`descriptor.rs`: padding and
  unused union words of SRV and UAV descriptions zeroed). They only remove
  duplicate entries from the view table, a memory matter, not an image one;
  `descriptor.rs` belongs to another task (nvidia2). The rules themselves
  checked out against the D3D12 layouts.
- *Doubtful claim:* "`texture::title_written` overwrites the separate host
  copy with CPU zeros". The separate copy is filled from title memory, which
  is zero where only the GPU drew; nothing is overwritten. Same effect.
- *Limits:* aliases of another shape or format family (an R32_UINT view of
  a colour target, a half-size alias) still get their own copy; that needs
  shared memory (G19), not a lookup by address. A writable alias the copy
  cannot serve (an unordered-access alias of a render target) gets its own
  copy, and the two do not see each other's writes.

### 5.2 Planes of a present composed (VIctory #211, Yaekai)

`dxgi/planes.rs` (new), and 8 lines in `queue.rs` `present`:
- Each plane's parameters are read 0x48 bytes apart; a plane counts only if
  its first resource is one of ours (its function table is checked through
  a read that cannot fault).
- With two planes or more, each is drawn into an image of ours (the largest
  plane's size, plane 0's format family): plane 0 stretched and opaque,
  the others blended over it as premultiplied alpha. The image is shown by
  the unchanged `Host::present`. With one plane nothing changes.
- The number of planes and each plane's resource are logged whenever the
  number changes: "the title presents planes".

Tests: plane formats, planes that name no resource of ours (or cannot be
read) left out, and two planes composed on the GPU (a red scene plane under
a 40 % green interface corner gives 153, 102, 0 there and red elsewhere).

**Critical review of #211.**
- *Kept:* the composition (full-screen triangle per plane, plane 0 opaque,
  the others premultiplied) and the plane stride, 0x48.
- *Changed: the stride is not trusted.* The PR reads planes 1 and after at
  0x48 bytes and dereferences their resource list blindly; a wrong stride
  (it is not checked against the SDK's headers, and xcl does not know it
  either) would crash the present. Here an unreadable plane or one whose
  resource is not ours is left out and logged with its raw words.
- *Changed: where it lives.* The PR rewrites `Host::present` (355 lines in
  `host.rs`, which the asynchronous-submission port is rewriting). Here the
  composition is a module of its own that hands `Host::present` one image;
  the cost is one more full-screen copy per frame with several planes.
- *Same effect, other means: plane states.* The PR's own barriers (each
  plane to the pixel-shader state and back) are right; here the planes go
  through the host list's state tracking (`ensure_state`), as title lists
  do, and stay in the pixel-shader state until the title moves them.
- *Not taken:* the swap chain sized to the largest plane inside
  `Host::present`; the composed image has that size and `Host::present`
  resizes for it.
- *Shortcut kept and said:* rectangles, colour space and flags of each plane
  are ignored; premultiplied alpha is the PR's assumption, confirmed only by
  its Series S menu captures.

### 5.3 Diagnostics (`capture.rs`)

- `XTP_CAPTURE_WORLD=<draws>[:<after>[:<count>[:<every>]]]` lists frames by
  what they draw: the first frame that ran at least `draws` draws arms it,
  then `count` frames (3) `every` (30) frames apart are saved, dumped and
  traced, starting `after` (30) frames later. The world starts at a frame
  number that varies by a thousand between runs; its frames run 900 to
  6 000 draws, the menus and loading screens a few dozen.
- `XTP_DUMP_TARGETS=<width>` saves only targets at least that wide; every
  target of a dumped frame is logged in first-use order with its size,
  format and flags.

## 6. World capture after the fixes

Pending: see section 7.

## 7. What still blocks it

- **Reaching the world.** Since 14:40 every session stalls on the loading
  screen (no frame for ~30 s, then the title's watchdog ends it), with the
  same binary that reached the world earlier; run-world is bisecting it
  (pipeline library ruled out, the audio mute and the cache under test).
- **NVIDIA wave64 refusals** (section 4.2): until the wave64 port lands, a
  refused pass in the final chain keeps it black on this GPU.
