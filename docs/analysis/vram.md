# Host texture VRAM: pooled heaps (task 450ec9ba)

Status: WORK IN PROGRESS (design fixed before the PC restart of
2026-11-24; no code yet). Measurements, the world session and the critical
review come later.

## Inputs

- stall (6bb65399): past a global VidMm threshold, each residency operation
  waits 0.3–2 s almost regardless of its size (1.4 MB BC7: 0.3–1.2 s; 64 MB
  buffer: 0.5–0.74 s; 256 MB buffer: ~1.2 s). Per-texture residency gives
  ~0.9 MB/s there, 256 MiB heaps ~210 MB/s. In game: ~6,700 resources at an
  address before the stall, 3,086 live host textures, 8,141 MiB.
- Samuel: the 9.5 GB ceiling is probably the machine's state (driver after
  TDRs, reboot pending); do not size the pool on it.

## Design (to implement)

- `resource/ranges.rs`: free ranges of one heap, best fit with coalescing
  (BTreeMap by offset + BTreeSet by (size, offset)). Buddy rejected: a 1.4 MB
  texture takes a 2 MiB block (43 % lost). TLSF gives the same fit as best
  fit for our counts (hundreds per heap, ~300 ops/s); best fit is simpler.
- `resource/pool.rs`:
  - classes: small (4 KiB placement, D3D12's small-resource rule: not RT/DS,
    1 sample, array 1, mip 0 in ≤ 16 tiles of 4 KiB, then checked by
    GetResourceAllocationInfo granting 4096) and normal (64 KiB). No 4 MiB
    MSAA class: MSAA textures are always RT/DS, which stay committed.
  - heaps: DEFAULT, `ALLOW_ONLY_NON_RT_DS_TEXTURES` (valid on tiers 1 and
    2), `CREATE_NOT_RESIDENT | CREATE_NOT_ZEROED`; sizes grow per class
    (normal 32 → 256 MiB, small 8 → 64 MiB); max size by env for tests in game.
  - a background thread makes heaps ahead of need (CreateHeap +
    EnqueueMakeResident, one fence per request), releases heaps, and watches
    the DXGI budget-change event (RegisterVideoMemoryBudgetChangeNotificationEvent).
    The title thread only places (CreatePlacedResource) and waits for a heap
    at most TIMEOUT, with one deadline per heap (not per texture).
  - free returns the range when the last `Arc<HostTexture>` drops (lists hold
    it until their submission finished); one empty heap kept per class
    (hysteresis), all empty heaps released when usage passes the budget.
  - `XTP_TEXTURE_POOL=0`: the old committed path.
- Pooled: title-written textures only (no RT/DS/UAV flags: texture.rs uploads
  every mip and slice of plane 0 from title memory, so NOT_ZEROED is safe).
  Committed (own resource, residency per texture, SetResidencyPriority HIGH
  for RT/DS): render targets, depth targets, UAV textures, MSAA, planar
  formats, stand-ins whose contents are not uploaded (116/117), textures
  over 64 MiB.
- Residency (`resource/residency.rs`, rewritten): one fence per request
  (review item 1: values could complete out of order); a texture or heap not
  resident in time stays *pending*: `host_resource`/`host_copy` answer None
  until its fence completes, re-asked (DENY_OVERBUDGET) from 1 s after a
  refusal (review item 3), never refused for good, never made resident past
  the budget.
- Counters in the frame line: pool heaps (count, MiB reserved, MiB used),
  pooled and committed textures by kind, pending textures, extra MiB from
  stand-ins (67 → RGBA16F, 85/86/115 → RGBA8: half of their bytes).

## Review items from Samuel (dark-world's code, now mine)

1. residency.rs: one fence per request.
2. texture_at: never dereference `before` after unlocking; on failure re-read
   the key under the lock; `usable` evaluated under the lock. draw.rs uses
   the returned pointer within the draw recording only (≤ GRACE): no change
   needed there (to tell nvidia-fix).
3. Pending instead of refused (above).
4. Stale shared copy: a new non-source texture made at the address drops the
   placed views there whose copy is shared and has no live source (narrower
   than "shared and no source", which would break transient targets read
   after their object is released: test at resource.rs:3010).
5. alias.rs: compare the family of the title format; memory_remapped also
   drops placed textures that start before the range and overlap it (keep
   their end in the map, scan from start minus the largest length).

Tests to add: ranges (alloc, free, coalesce, reuse, model test), classes,
pool on WARP (alignment, reuse, hysteresis, residency requests per heap not
per texture, contents through a copy), budget refusal then retry, expiry,
concurrent residency with own fences, review items 2, 4, 5. Benchmark:
ignored test, 5,000 BC7 1024² (9 mips), direct committed vs NOT_RESIDENT +
EnqueueMakeResident vs pooled: time per creation, VRAM (QueryVideoMemoryInfo)
and private bytes per texture.
