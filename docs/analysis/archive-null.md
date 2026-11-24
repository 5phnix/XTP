# The Archive object that comes back null (int3 at title+0x23d8f03)

Task 54cb7617, 2026-11-24. BTP 6 1.0.2.1, Series S. RVAs and behaviour
only: no bytes or listings of the title's code (docs/asset-policy.md).

**In short:**
- The stop is not the title's watchdog, and not a lack of memory.
- One second before it, XTP's DirectStorage failed two raw reads with
  `ERROR_INVALID_USER_BUFFER`. One of them left an archive header unwritten.
- The title then could not make that Archive entry's object, and its master
  build's own check broke.
- That error is what Windows returns when the destination page becomes
  read-only while the read copies into it. The GPU side's write guards do
  exactly that when a copy re-arms them, between `kernel_will_write` and the
  end of the read.
- The fix belongs in `crates/xtp-hle/src/modules/dstorage.rs` and
  `files.rs`, or in `crates/xtp-gpu/src/watch.rs` (see the end).

## The stop

Session mesh64-a (run `E:\xtp-data\cache\play\20261124-191312`, mesh64's
binary f6af110, `XTP_WATCHDOG_HOOK=1`) reached gameplay at 44 s. It ended
8 s later on `EXCEPTION_BREAKPOINT` at title+0x23d8f03. Its stack, from
`crashes.log`, innermost first:

| Frame | RVA | What it is |
|---|---|---|
| 0 | `0x23d8f03` | the `int3` in `0x23d8e70`, slot 3 of the Archive store's vtable |
| 1 | `0x2593e91` | `0x2593560`, which moves one request through its states |
| 2 | `0x2592d69` | `0x2592c50`, a request worker thread's function |
| 3 | `0x2215eca` | `0x2215ea0`, the title's common thread start |
| 4–5 | KERNEL32, ntdll | the host's thread start |

Before ending, the title ran its crash reporter (Breakpad). It wrote "Game
Running Time 51 s", the intro mission in prestream, 699 streaming
requests, and its own memory figures:

| Category | Used | Size | Full |
|---|---:|---:|---:|
| Game Virtual | 923,879,360 | 1,218,445,296 | 76 % |
| Game Physical | 1,287,928,512 | 1,339,031,552 | 96 % |
| Resource Virtual, Resource Physical | 0 | 0 | — |
| Pool | 267,235,712 | 270,532,592 | 98.8 % |
| TileHeap (its own line) | 3,030 MB | 3,977 MB | 76 % |

These figures turned out to have nothing to do with the stop (below).

## It is not the title's watchdog

- `0x2215eca` follows the call into the thread's own function in the
  title's common thread start (`0x2215ea0`). It sits at the bottom of every
  title thread's stack, the watchdog's included.
- The watchdog's loop is the function at `0x2215a60` (`0x2215a60`–`0x2215e09`
  in `.pdata`). In that run the hook was applied (`sites=2`) and never
  counted a firing.
- The watchdog's handler pointer (`0x71b6348`) is read only at its two
  firing sites, which the hook patches, and by its setter (`0x22161f0`). The
  loop has no other fault. `crates/xtp-game/src/watchdog.rs` records this.

## What the title does

- **The request.** A request worker (`0x2592c50`) hands each request to
  `0x2593560`, which writes the request's state (5 bits, `0x2591de0`) as it
  goes.
- **The store.** From state `0xc` to `0xd`, the worker calls slot 3 of the
  request's store. Here that is the "Archive" store, the archives of files
  that streaming mounts:
  - vtable `0x61cf360`, built at `0x5b55850`;
  - a singleton, behind the title's pointer at `0x8fdce30`, made at start-up
    by `0x22798e0` through `0x5b55650`.
- **Its layout.** The store has 12,005 slots (`+0x78`), entries of `0x30`
  bytes (`+0x88`) at `+0x70`. An entry holds a hash at `+0x00`, its
  definition at `+0x08`, the loaded object at `+0x10` and the object being
  placed at `+0x20`.
- **The census.** `XTP_TITLE_ASSERTS`'s census, on the L4, 60 s in: 4,171
  entries with a definition, 52 loaded, 3 being placed. Every definition is
  of one class: vtable `0x62fd890`, whose second function, `0xfc2490`,
  makes the entry's object.
- **Making the object.** `0xfc2490` allocates a 0x80-byte archive object
  (vtable `0x62fd480`) with `operator new`. A null there would fault on the
  next instruction, so the null that reaches the check does not come from
  this allocation. The object's constructor (`0xfc2540`) has `0x1fb2600`
  load the archive's header:
  - with a header buffer from the request, it requires that buffer to start
    with the archive magic;
  - without one, it reads 16 bytes, then the rest of the header, from the
    file, and requires full reads.

  The header is then checked against the definition (`0x8ec200`) and parsed
  (`0x87aec0`). The object is valid only when the parse found the header
  (`+0x30` set; the validity test, slot `0x3a0`, is `0xfc2530`). An invalid
  object is destroyed and `0xfc2490` returns null.
- **The check.** Slot 3 (`0x23d8e70`) checks that pointer. **A null
  breaks**: the `int3` at `0x23d8f03`, left in the master build. The code
  after it empties the slot and returns.
- **The caller.** The caller does not test the result. For an Archive, its
  next two calls (vtable `0x20` and `0xd0`) are stubs that return null, and
  the request goes through states `0xe` and `0xf` to done. On the console
  nothing catches the break either: the title counts on the header being
  there.

## The failed reads

One second before the `int3`, run A logged two failed DirectStorage
requests, both plain copies (options `0x0`, a raw destination in title
memory), with hr `0x800706f8`, that is `ERROR_INVALID_USER_BUFFER` (1784):

| Time (UTC) | File offset | Size |
|---|---:|---:|
| 19:14:04.256 | 71,066,013,696 | 9,856 |
| 19:14:04.289 | 4,263,010,304 | 800 |

The `int3` came at 19:14:05. Across the last 40 runs in
`E:\xtp-data\cache\play`, run A is the only one with a failed DirectStorage
request, and the only one with this `int3`. Session mesh64-b (same binary)
has neither.

`0xfc2490` finds an archive header that is not there: a failed read leaves
the destination as it was.

### What makes a read fail with ERROR_INVALID_USER_BUFFER

`dstorage.rs` reads a raw destination in place. It calls
`xtp_gpu::kernel_will_write`, which opens the GPU side's write guards on
those blocks, then makes a synchronous `ReadFile` straight into the title's
memory. On the host, measured on this PC with a standalone test:

| Destination when `ReadFile` runs | Result |
|---|---|
| read-write, committed (plain, write-watched, section view, view in a placeholder) | read |
| read-only, or not committed, before the call | `ERROR_NOACCESS` (998) |
| read-write at the call, made read-only by another thread **during** the read | **`ERROR_INVALID_USER_BUFFER` (1784)**: 344 of 99,979 8 MiB reads, the rest 998 or read |

The kernel checks the buffer when the call starts (998). A page that
becomes read-only while the file system copies into it faults inside the
copy, and NTFS returns that as an invalid user buffer (1784).

The GPU side does exactly that. Before it copies view memory
(`xtp_gpu::watch::guard`), it makes every block written since its last copy
read-only again, and `kernel_will_write` has just marked the read's blocks
written. So when a frame's copy covers a block a read is landing in, the
copy re-arms the guard in the middle of the read, and the read fails. The
same window also lets a copy take the block's old bytes.

The window is as wide as a read. It opens only when a GPU copy covers that
block at the same moment, which is why the failure is rare and depends on
timing. Run A was a cold run: the first one with the wave64 mesh programs
allowed, many translations, 510 ms submissions, 699 requests in flight. It
had the most such overlaps.

## Ruled out

- **A lack of memory.** The object's allocation is not where the null comes
  from (above), and XTP refused no XMem call in run A, run B, or the two
  ledger runs.
- **The budget XTP reports.** With `XTP_XMEM_LEDGER=1`, the title asks for
  `XMemGetWorkingSetStatistics` 24 times in its first 10 s, then never
  again. We answer a limit of 7,776 MiB, and our process's commit as its
  use: 7,651 MiB on the PC, 6,235 MiB on the L4. Its two readers size
  nothing:
  - `0x2215fd0` turns tools memory into title memory (`XMemTransferMemory`),
    0 here;
  - `0x2474500` checks for 100 MiB free before it writes a final report.

  Its heaps come from the device type ("Conole Type: Lockhart").
- **Physical pages never returned.** The title takes its physical memory
  at start-up (4,679 MiB in 4 calls on Series S) and carves its heaps from
  it. The ledger shows no later physical-page traffic in the time covered.
- **XTP's allocations counted against the title.** Only the committed
  resources of the title's device take memory from the title's allocator
  (allocator id 128, about 150 MiB in the first 10 s), as on the console.

The Pool and Game Physical figures are the title's own fill; they do not
reach this path.

## Measurements in the tree

- `XTP_TITLE_ASSERTS=continue` (`crates/xtp-game/src/asserts.rs`): the
  `int3` becomes a count, then the title's own null path. It also logs a
  census of the Archive store 60 s in and at the first skip.
- `XTP_XMEM_LEDGER=1` (`crates/xtp-hle/src/modules/xmem.rs`): XMem by
  category every 10 s (calls, failures, frees, live and peak bytes, the
  budget reported).

## The fix (proposal, outside this task's files)

**1. Quick: retry the read.** In `dstorage.rs`'s `read_into`, and in
`files.rs`'s `ReadFile` and `fread` wrappers, retry a read that fails with
`ERROR_INVALID_USER_BUFFER`:
- call `kernel_will_write` again, then read again, a few times at most;
- the retry loses nothing: the failed read wrote nothing that counts, and
  the next copy takes the block because it is marked written again;
- for an overlapped `ReadFile`, the error comes at completion: those reads
  need the second fix.

**2. Thorough: keep blocks open while the kernel writes them.** In
`watch.rs`:
- `kernel_will_write` returns a token, and blocks with a token out are
  neither re-armed nor counted clean by `guard`;
- the copy then takes them unguarded, and the next copy takes them again;
- this closes both the failure and the stale copy.

**The workaround until then:** `XTP_TITLE_ASSERTS=continue` lets test
sessions pass the check. The entry stays empty; what the title does with it
later is not known.
