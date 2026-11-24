# Host texture VRAM: pooled heaps (task 450ec9ba)

Why the world stalled while loading, what XTP now does with host textures,
what it costs, and what is still to measure. Code:
`crates/xtp-gpu/src/resource/{pool,ranges,residency}.rs` and
`resource.rs` (`host_texture`, `committed`, `pooled`).

## 1. The problem

While the world loads, Grand Theft Auto VI creates thousands of resources in
its memory (device method 0x428, `create_at_address`); each texture gets a
host copy (`host_texture`). Until this change every host copy was a
committed resource: its own implicit heap, its own residency operation.

stall's measurements (task 6bb65399, this PC, 2026-11-24; a D3D12 probe
without XTP, then sessions R and S in game):

- Past a global threshold (about 9.5–9.75 GB resident for all processes on
  the RTX 4080's 15.67 GB local segment), VidMm admits about one new
  resident allocation a second: 0.3–1.2 s for a 1.4 MB BC7 texture,
  0.5–0.74 s for a 64 MB buffer, 0.6–2.1 s (≈ 1.2 s) for a 256 MB buffer.
  The cost is per allocation, almost not per byte.
- The DXGI budget stays at 14.92 GB, so DENY_OVERBUDGET never fires there.
- CREATE_NOT_ZEROED changes nothing to it.
- In game: about 6,700 resources at an address before the stall, 3,086 live
  host textures for 8,141 MiB at the stall; past the threshold 2–4 textures
  come out every 5 s. A device restart did not change it (Samuel): it is this
  machine's behaviour, not a passing state. On the cloud L4 there is no
  ceiling (21 GB in 7.8 s).
- With `XTP_WATCHDOG_HOOK=1` the world is reached (intro-skip's session T2,
  main 5b51ddd): 160 s of loading, the world at +184 s, VidMm at
  9.5–9.7 GiB from +50 s, 2,431 creations during the loading. **The number
  to beat: 160 s.**

Per-texture residency gives about 0.9 MB/s past the threshold; heaps of
256 MB about 210 MB/s.

## 2. What XTP does now

### 2.1 Which textures go where

| host texture | where | why |
|---|---|---|
| no render target, depth or unordered access; 1 sample; not planar; contents uploaded | **pool** | the title fills it: `texture::upload` writes every mip and slice of plane 0 from title memory before a shader reads it, so pool memory needs no zeroing |
| render targets, depth targets | committed, residency priority HIGH | placed targets need a clear, discard or copy before first use (D3D12's rule for placed RT/DS), and their contents start at zero today |
| unordered access textures | committed | GPU-written: their contents start at zero today, which a reused pool range would not give |
| MSAA | committed | always targets on D3D12 (so no 4 MiB class) |
| planar video formats (NV12, P010, ...) | committed | only plane 0 is uploaded |
| stand-ins whose contents are not uploaded (116, 117) | committed | their committed memory stays zero, as before |
| more than a quarter heap (64 MiB) | committed | one heap each anyway |
| over the budget, or a placement the driver refuses | committed, logged | see 2.4 |

`XTP_TEXTURE_POOL=0` makes every host texture committed, as before.

### 2.2 The pool (`resource/pool.rs`)

- Two classes by placement alignment: **4 KiB** (D3D12's small resource
  rule: no target, one sample, one slice, mip 0 in at most 16 standard 4 KiB
  tiles, granted by `GetResourceAllocationInfo`) and **64 KiB** (the rest).
  A committed texture always takes 64 KiB at least: a 16x16 texture is 1 KiB
  of texels in a 64 KiB allocation; in the pool it takes 4 KiB.
- Heaps: DEFAULT, `ALLOW_ONLY_NON_RT_DS_TEXTURES` (valid on resource heap
  tiers 1 and 2), `CREATE_NOT_RESIDENT | CREATE_NOT_ZEROED` (zeroed if the
  runtime refuses the flag). 256 MiB for the 64 KiB class, 32 MiB for the
  small one (`XTP_TEXTURE_HEAP_MIB`).
- Sub-allocation (`resource/ranges.rs`): best fit with merging of free
  neighbours. A buddy allocator rounds a 1.4 MB BC7 texture to 2 MiB (43 %
  lost); TLSF fits as well as best fit at these counts (hundreds of ranges a
  heap, a few hundred placements a second) and is more code.
- A thread (`xtp-texture-heaps`) keeps a **reserve** of free bytes in each
  class: 512 MiB and 64 MiB by default (`XTP_TEXTURE_RESERVE_MIB`). Heaps
  still becoming resident count in it: the title's thread places textures
  in them and the GPU gets those once the heap is resident. Each heap is one
  residency request (`EnqueueMakeResident`, its own fence, within the
  budget); a texture is none.
- The title's thread only calls `CreatePlacedResource`. In the first 100 ms
  of a heap, a placement into it waits at most 10 ms for its residency (so
  the title can use a texture right after making it, as below the
  threshold); past the threshold that is at most about 100 ms a heap, never
  a texture. If the reserve is gone, the title's thread makes a heap itself
  (counted: "by the title's thread"), not resident, not waited for.
- A range goes back when the host copy is dropped. Lists hold host copies
  until their submission has run, so a range is never reused under the GPU.
  An empty heap is released unless the reserve needs it (hysteresis), and
  every empty heap goes when DXGI's budget-change notification shows usage
  past the budget.

### 2.3 Residency (`resource/residency.rs`)

- Every request has a fence of its own (review of dark-world's delivery:
  with one fence and values taken under a lock but enqueued outside it, a
  later request finishing first said an earlier texture was resident: a GPU
  page fault).
- Nothing is refused for good (review): a request refused (past the budget,
  or an error) is asked again a second later; meanwhile the texture is
  handed out but not given to the GPU. `host_resource`, `host_copy`,
  `host_transition` and `texture_at` answer as if it had no host copy until
  it is resident (`HostTexture::ready`).
- Committed textures wait at most 10 ms for their residency, then are
  handed out the same way.

### 2.4 The budget

- Before a heap, the pool checks `QueryVideoMemoryInfo`: usage plus heaps
  not yet resident plus the new heap must stay within the budget. Past it,
  no heap is made (logged once each time it starts, counted), the texture is
  made committed and asked to become resident with `DENY_OVERBUDGET`
  (refused, logged, retried every second). The process is never made
  resident past its budget.
- The budget-change notification
  (`RegisterVideoMemoryBudgetChangeNotificationEvent`) is followed by the
  thread: logged, and past the budget the empty heaps go.
- Render targets and depth targets get `D3D12_RESIDENCY_PRIORITY_HIGH`.

### 2.5 Counters (frame line, `format::report`)

`host textures: N alive, M MiB (P pooled, X MiB; C committed, Y MiB;
stand-in formats +S MiB); residency asked A times, refused R; texture
pool: H heaps (Q not resident yet), B MiB, U MiB placed in T textures; so
far ...`. Stand-ins (67 → RGBA16F, 85/86/115 → RGBA8, 116/117 → RGBA16F,
189 → RGBA16_SNORM, 190 → RG8) take twice the title's bytes: the `+S MiB`
is that half.

## 3. Measurements

### 3.1 Micro-benchmark (WARP)

`resource::pool::tests::bench_5000_bc7_textures` (ignored; `cargo test -p
xtp-gpu --release -- --ignored --nocapture pool::tests::bench`): 5,000 BC7
1024² textures of 9 mips made three ways.

TO BE FILLED.

### 3.2 In game

TO BE FILLED (session before/after with `E:\tools\stall-probe\session.ps1`,
`XTP_WATCHDOG_HOOK=1`, autopilot, golden).

## 4. Review items of dark-world's delivery (Samuel's relay)

1. Residency fences in order: one fence a request (2.3).
2. Use-after-free in `texture_at`: a resource is handed out only while it
   is in the map, looked at under its lock; a failed creation looks the key
   up again instead of using `before`, which a remapping may have released
   meanwhile. The returned pointer stays valid for `GRACE` (2 s): draw.rs
   uses it while it records the draw, so nothing changes there.
3. A texture not resident in time is no longer refused for good (2.3).
4. A stale target copy: views that show a released target's copy are
   dropped when the title makes another texture (one the GPU does not
   write) at that address. Narrower than "shared and no source left": that
   would drop the copy as soon as the target's object is released, while
   the title may still read what was drawn through a view (title memory
   holds it on the console).
5. `alias` compares the title format's family (a stand-in format is shared
   by title formats of other families); `memory_remapped` also releases
   placed views that start before the range and overlap it.

## 5. Critical review

TO BE COMPLETED with the measurements. Known limits so far:

- A view made while its texture is not resident is a null view, and stays
  one until the title writes that descriptor again (descriptor/host.rs makes
  views when the title writes descriptors). Below the threshold this does
  not happen (the 10 ms wait); past it, textures placed in a heap still
  becoming resident are invisible until their descriptor is rewritten. A
  descriptor-side follow-up would remake such views once resident.
- Directly indexed views are not uploaded (texture.rs shortcut): in the
  pool their texels are what the range held before (another texture's)
  instead of zeros.
- texture.rs makes a committed upload buffer per texture it detiles (its
  staging buffer): one allocation and residency operation each, in system
  memory. Not measured against the threshold yet.
- The reserve keeps up to 512 MiB + 64 MiB unused.
