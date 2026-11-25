//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory. Physical-page views cannot
//! be watched: there the memory is guarded as buffer copies are
//! (`watch::writes` with `guarded`), and a content hash of the whole memory
//! tells a change only when a block of it was written since the last check
//! (or cannot be guarded); a clean check costs a walk of its blocks, not a
//! hash. Every writer of such memory opens the guards: the title's CPU by
//! faulting, our copies and the kernel's reads (`watch::writing`), and
//! DirectStorage's page destinations (xmem's report after its writes).
//! `XTP_TEXTURE_GUARDS=0` (or `off`, `false`) hashes at every check, as
//! before: a world frame checked the same textures about a hundred times
//! (`docs/analysis/gpu-frame.md`).
//!
//! A writer that opens no guard would leave such a texture stale for good,
//! later lists' copies predicated off: the title's own views of sections,
//! its memory calls that bypass xmem, GPU writes through UMA heaps
//! (`watch`'s shortcuts). A safety net covers them: at each check it hashes
//! guarded textures checked since it last hashed them, round-robin by
//! resource, within a budget a frame (`XTP_TEXTURE_NET`, 2 MiB by default,
//! taken from a texture a slice at a time; a budget a second where frames
//! do not advance), a lap every 2 s at most (a few textures are not hashed
//! again at every frame: 0.86 ms a frame of an L4 VM's CPU for six of them,
//! the whole budget, before). A texture whose memory no longer hashes as
//! what was detiled is hashed at its next check whatever its guards say,
//! refreshed if it changed, and logged once (with the count of such changes
//! so far). Its budget costs at most 0.65 ms a frame on a build-farm CPU
//! (`tests::net_costs`), 0.86 ms on the L4 VM's; not measured in the world
//! yet.
//!
//! A check hashes and detiles one snapshot of the memory: hashed in place
//! and detiled after the wait for an earlier copy, a title write during the
//! wait was detiled under the hash of the memory before it, which the
//! title writing those bytes back then matched.
//!
//! A texture whose host format stands in for the title's
//! (`resource::format`) has its texels converted on the way.
//!
//! Each list that samples such a texture records the copy of the staging
//! buffer into the host copy, under a predicate of its own
//! (`SetPredication`): at submission, once the staging buffer is up to
//! date, the copy is let run only if the host copy has not got these
//! bytes yet, so the first list after a change copies and the others do
//! not (a world frame samples the same textures in dozens of lists:
//! `docs/analysis/gpu-frame.md`). `XTP_TEXTURE_PREDICATION=0` runs every
//! list's copy, as before.
//!
//! A staging buffer goes once no list holds it (the lists that copied from
//! it ran and were reset) and no upload of it was recorded for a couple of
//! passes ([`release_staging`]); what was detiled and copied is kept, and
//! the next upload makes the buffer again, detiling and copying only what
//! changed (or for a host copy not known to hold it). Before, a staging
//! buffer lived as long as its texture, and the world's thousands filled
//! the staging limit of the textures picked by index for good.
//!
//! Textures read through descriptor tables are brought up to date by the
//! draws that read them. Directly indexed views are not known per draw:
//! the textures shaders pick from the view heap by an index they compute
//! are uploaded by a pass once a frame (`draw.rs` `heap_textures`, the
//! queue of `descriptor::host::TextureQueue`), the slots the title wrote
//! last first, and then checked for changes in turns at record time
//! ([`memory_change`]: the guards or the write watch, no hash), the changed
//! ones uploaded as a table read does (memory neither covers within a
//! budget of bytes a pass, as the check at submission hashes it). Ported
//! from fxceless's VIctory fork (commit 578d459), which uploads such a
//! texture once and again only when the title rewrites its slot.
//!
//! GPU writes into a texture the title fills (copies, resolves) and its
//! uploads follow submission order, not recording order (watch item 263,
//! [`gpu_write`]): from the submission of the first such write on, its
//! host copy keeps the host's contents, and an upload recorded in a list
//! submitted after it runs no copy, decided at that list's submission as
//! whether the host copy has the staging buffer's bytes is. A write into
//! part of the texture first uploads its title bytes, in the same list, so
//! the rest stays as the title filled it. VIctory #1934 (zondy1i) decides
//! both at submission too; this was written without its code.
//! shortcut: without a predicate (`XTP_TEXTURE_PREDICATION=0`, or none
//! left) an upload's copy runs whatever was submitted before it, as before.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Plane 0 is uploaded (every mip and array slice), depth and stencil
//! planes are not.
//!
//! shortcut: the safety net covers guarded textures only, not those in
//! write-watched memory.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use crate::host::{HostList, host};
use crate::object::This;
use crate::resource::format::Convert;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled. The buffer goes once
/// no list holds the state any more (its last copy ran) and is made again
/// for the next upload ([`release_staging`]); what was detiled and copied
/// is kept, so that an unchanged texture is neither detiled nor copied
/// again.
pub(crate) struct Staging {
    /// The staging buffer; `None` once released.
    upload: Mutex<Option<Upload>>,
    /// The buffer holds nothing detiled: made (again), nothing detiled
    /// into it since. No copy reads it then.
    blank: AtomicBool,
    /// The next check detiles the memory whatever its watch, guards or
    /// hash say: a blank buffer a copy is due from.
    force: AtomicBool,
    /// [`SWEEPS`] when its last upload was recorded.
    recorded: AtomicU64,
    /// The staging buffer's bytes.
    size: u64,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: xtp_formats::texture::Layout,
    /// How texels convert into the host format, if it stands in for the
    /// title's.
    convert: Option<Convert>,
    /// Mips per array slice.
    mips: u32,
    memory: *const u8,
    last: Mutex<Last>,
    /// How many times the staging buffer was written (detiled).
    written: AtomicU64,
    /// What a host copy last got from it, as decided at submission: the
    /// count of writes then and the host copy (held weakly: its address is
    /// not reused meanwhile).
    copied: Mutex<Option<(u64, Weak<crate::resource::HostTexture>)>>,
    /// Checked through its guards since the safety net last hashed it.
    sampled: AtomicBool,
    /// The safety net found its memory differ from what was detiled: its
    /// next check hashes it whatever its guards say.
    suspect: AtomicBool,
    /// Changes its guards missed.
    missed: AtomicU64,
    /// A GPU write into its texture was submitted ([`host_owned`]): nothing
    /// is detiled into it any more.
    host_owned: AtomicBool,
    /// Tells it apart in tests from a staging state of another test freed
    /// at the same address (whose copies and hashes the address would
    /// count).
    #[cfg(test)]
    id: u64,
    /// Its checks since the last summary (diagnostics).
    #[cfg(feature = "diagnostics")]
    seen: Seen,
}

/// A texture's checks since the last summary (diagnostics): all of them,
/// those again in a submission that had checked it, those its guards found
/// clean, the bytes hashed, the changes found.
#[cfg(feature = "diagnostics")]
#[derive(Default)]
struct Seen {
    checks: AtomicU64,
    again: AtomicU64,
    /// Found clean by the guards (no hash).
    clean: AtomicU64,
    hashed: AtomicU64,
    changed: AtomicU64,
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after,
/// with room to read the memory into; and the submission that checked it
/// last (`timestamps::submissions` then, diagnostics).
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    asked: u64,
    read: Vec<u8>,
    #[cfg(feature = "diagnostics")]
    checked: Option<u64>,
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

/// Staging states by title resource, in the order the safety net walks.
static STAGING: Mutex<Option<BTreeMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// The bytes of the staging buffers that live (upload memory): counted
/// when one is made, until it is destroyed ([`Upload`]'s drop), whether its
/// texture's state is in [`STAGING`] or only held by lists.
static STAGED: AtomicU64 = AtomicU64::new(0);

/// A texture's staging buffer and its mapping, counted in [`STAGED`] while
/// it lives.
struct Upload {
    buffer: ID3D12Resource,
    data: *mut u8,
    size: u64,
}

impl Drop for Upload {
    fn drop(&mut self) {
        STAGED.fetch_sub(self.size, Ordering::Relaxed);
    }
}

/// Calls of [`release_staging`] so far: the clock of [`Staging::recorded`].
static SWEEPS: AtomicU64 = AtomicU64::new(0);

/// Staging buffers released so far: count and bytes.
static RELEASED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The resource [`release_staging`] looked at last: the next call goes on
/// after it.
static RELEASE_CURSOR: Mutex<usize> = Mutex::new(0);

/// Room for a check's snapshot of texture memory, kept at the largest
/// changed so far.
static SNAPSHOT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Textures whose host copy a GPU write (a copy or resolve into it) was
/// submitted for, as their lists' submissions are prepared ([`gpu_write`]):
/// their contents are on the host from then on, not in title memory, which
/// stays as it was.
static GPU_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// Whether a GPU write into `resource`'s host copy was submitted (in the
/// submission being prepared, or before).
fn gpu_written(resource: usize) -> bool {
    GPU_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&resource))
}

/// `list` writes `resource`'s host copy on the GPU next: a copy or resolve
/// into it, of every texel of every subresource if `whole`. From the
/// list's submission on, the host copy keeps the host's contents: title
/// memory is not uploaded into it any more (shortcut: even if the title
/// writes it later). Uploads recorded in lists submitted after that run no
/// copy, those submitted before run as usual: the order is the
/// submissions', not the recordings' (module doc). A write into part of a
/// texture the title fills keeps the rest as the title filled it: its title
/// bytes are uploaded first, in this list, unless the host copy has them
/// already or a GPU write was submitted before (both decided at
/// submission). Recorded where the list has the texture in the write's
/// state, before the write.
pub(crate) fn gpu_write(list: &HostList, resource: This, whole: bool) {
    let key = resource as usize;
    if gpu_written(key) {
        return;
    }
    if !whole
        && let Some(texture) = crate::resource::host_resource(resource)
        && title_written(texture)
    {
        load_before_write(list, resource, texture);
    }
    // After the upload's decision above, in the list's order.
    list.defer_host((0, 0), move || host_owned(key));
}

/// Records the upload of `resource`'s title bytes into its host copy
/// `texture` before a GPU write into part of it, recorded next in the state
/// the list has the texture in now: the write waits for the upload, which
/// may write the same texels (through a transition out of the copy
/// destination state and back if the write is a copy too).
fn load_before_write(list: &HostList, resource: This, texture: &ID3D12Resource) {
    const COMMON: u32 = 0;
    const COPY_DEST: u32 = 0x400;
    // The write's state: the one the list moves the texture out of here.
    let state = match list.transition(resource, COPY_DEST) {
        Some((host, before)) => {
            let barrier = crate::host::transition(
                host,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(COPY_DEST as i32),
            );
            // SAFETY: an open list and a live texture.
            unsafe { list.list.ResourceBarrier(&[barrier]) };
            before
        }
        None => COPY_DEST,
    };
    let uploaded = upload(list, resource, texture);
    if state != COPY_DEST {
        crate::list::ensure_state(list, resource, state);
    } else if uploaded {
        crate::list::ensure_state(list, resource, COMMON);
        crate::list::ensure_state(list, resource, COPY_DEST);
    }
}

/// At the submission of a GPU write into `resource`'s host copy: the host
/// has its contents from now on, and its staging buffer is not needed any
/// more (the lists holding it keep it until they are reset; its bytes leave
/// [`STAGED`] then).
fn host_owned(resource: usize) {
    GPU_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(resource);
    let gone = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|map| map.remove(&resource));
    if let Some(gone) = gone {
        gone.host_owned.store(true, Ordering::Relaxed);
    }
}

/// The title released a resource (its staging buffer goes once the lists
/// holding it are reset).
pub(crate) fn forget(resource: usize) {
    let gone = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|map| map.remove(&resource));
    drop(gone);
    let mut set = GPU_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    const TARGET_DEPTH_UNORDERED: i32 = 0x7;
    // SAFETY: a live resource.
    unsafe { texture.GetDesc() }.Flags.0 & TARGET_DEPTH_UNORDERED == 0
}

/// Records, once per list, the copy of `resource`'s staging buffer into
/// its host copy, and the detiling that refreshes the staging buffer at
/// submission if the memory changed. The copy runs only if the host copy
/// has not got the staging buffer's bytes yet and no GPU write into it was
/// submitted before (both decided at submission, see the module). Leaves
/// the texture in the copy destination state. Whether a copy was recorded.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) -> bool {
    #[cfg(feature = "diagnostics")]
    summary_due();
    // shortcut: a texture the host GPU wrote keeps the host's contents even
    // if the title later writes its memory too.
    if gpu_written(resource as usize) || !list.first_use(resource as usize) {
        return false;
    }
    // Made again if it was released: this list holds it from now on.
    let Ok(staging) = staging(resource, texture) else {
        return false;
    };
    let Some(buffer) = staging.buffer() else {
        return false;
    };
    staging
        .recorded
        .store(SWEEPS.load(Ordering::Relaxed), Ordering::Relaxed);
    // The host copy `texture` is: what the copy writes (the list holds it
    // through its state below).
    let target = crate::resource::host_copy(resource)
        .filter(|c| c.texture.as_raw() == texture.as_raw())
        .map(|c| Arc::downgrade(&c));
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    // Without a predicate (none left, or predication off) the copy runs.
    let predicate = target.as_ref().and_then(|_| Predicate::take());
    // A buffer made again after a release holds nothing: an unchanged
    // texture whose host copy has what was detiled last needs neither a
    // detiling nor a copy (decided at submission), any other gets one.
    if staging.blank.load(Ordering::Acquire)
        && (predicate.is_none() || !staging.holds(target.as_ref()))
    {
        staging.force.store(true, Ordering::Release);
    }
    let timed = crate::timestamps::command(list, crate::timestamps::What::TextureUpload, None);
    if let Some(p) = &predicate {
        // SAFETY: an open list; the predicate's buffer lives for the run.
        unsafe {
            list.list
                .SetPredication(&p.of.buffer, p.offset(), D3D12_PREDICATION_OP_EQUAL_ZERO)
        };
    }
    // The staging buffer lives as long as `staging` holds it, and it is
    // not released while the deferred refresh below holds `staging`, until
    // the list is reset.
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
            let len = u64::from(f.RowPitch) * u64::from(staging.rows[i]) * u64::from(f.Depth);
            list.note_read(
                || {
                    format!(
                        "texture upload {resource:?} ({}x{}x{} f{} mips {}) sub {i} of {} from staging {gpu:#x}+{:#x}: offset {:#x} {}x{}x{} pitch {}",
                        d.Width,
                        d.Height,
                        d.DepthOrArraySize,
                        d.Format.0,
                        d.MipLevels,
                        staging.footprints.len(),
                        staging.size,
                        footprint.Offset,
                        f.Width,
                        f.Height,
                        f.Depth,
                        f.RowPitch
                    )
                },
                (gpu + footprint.Offset, len),
            );
        }
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    if predicate.is_some() {
        // SAFETY: an open list.
        unsafe {
            list.list
                .SetPredication(None::<&ID3D12Resource>, 0, D3D12_PREDICATION_OP_EQUAL_ZERO)
        };
    }
    drop(timed);
    // Once per submission however many of its lists sample the texture.
    list.defer_texture(staging.clone());
    // Then, for this list: whether its copy runs. Not over what a GPU write
    // submitted before it put there.
    let (bytes, key) = (staging.size, resource as usize);
    list.defer_host((0, 0), move || {
        // Not from a buffer that holds nothing detiled (its memory was not
        // mapped at the check).
        let due = !gpu_written(key)
            && !staging.blank.load(Ordering::Acquire)
            && target.as_ref().is_none_or(|t| staging.copy_due(t));
        let runs = match &predicate {
            Some(p) => p.decide(due, crate::timestamps::submissions()),
            None => true,
        };
        COPIES[usize::from(runs)].fetch_add(1, Ordering::Relaxed);
        COPIED[usize::from(runs)].fetch_add(bytes, Ordering::Relaxed);
        #[cfg(test)]
        DECIDED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((staging.id, runs));
    });
    true
}

/// What a look at a texture's memory before its upload is recorded tells
/// ([`memory_change`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    /// Not written since its staging buffer was detiled; or nothing to
    /// upload (the host GPU's own contents).
    No,
    /// Written since, suspected by the safety net, never detiled, or never
    /// staged (no staging buffer could be made for it then, or it took the
    /// place of the texture uploaded before): an upload is due.
    Yes,
    /// Memory neither the write watch nor the guards cover (or guards that
    /// are off or verified): only the check at submission, which hashes its
    /// bytes (these many), tells.
    Unknown(u64),
}

/// Whether the memory of `resource`, a texture a shader picks from the view
/// heap by index, changed since its staging buffer was last detiled: asked
/// at record time by the pass over such textures (`draw.rs`
/// `heap_textures`), so that the unchanged ones record no upload. The
/// write watch or the guards answer as they do for the check at
/// submission, without its hash; nothing is taken from them (the journal
/// keeps the writes for the check). A texture its guards find clean is the
/// safety net's to hash, as one a check found clean.
pub(crate) fn memory_change(resource: This) -> Change {
    if gpu_written(resource as usize) {
        return Change::No;
    }
    let staging = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()
            .and_then(|map| map.get(&(resource as usize)).cloned())
    };
    let Some(staging) = staging else {
        // Not "clean": nothing was ever uploaded from it.
        return Change::Yes;
    };
    // Never detiled (its memory was not mapped at the checks so far).
    if staging.suspect.load(Ordering::Relaxed) || staging.written.load(Ordering::Acquire) == 0 {
        return Change::Yes;
    }
    let asked = staging.last.lock().unwrap_or_else(|e| e.into_inner()).asked;
    let (start, end) = staging.range();
    let since = |written: &[(usize, u64)]| {
        if written.iter().any(|&(_, w)| w > asked) {
            Change::Yes
        } else {
            Change::No
        }
    };
    if let Some((_, written)) = crate::watch::writes(start, end, false) {
        return since(&written);
    }
    if guards()
        && let Some((_, written)) = crate::watch::writes(start, end, true)
    {
        let change = since(&written);
        if change == Change::Yes {
            return change;
        }
        if !verify_guards() {
            staging.sampled.store(true, Ordering::Relaxed);
            return change;
        }
    }
    Change::Unknown(staging.layout.size)
}

/// The bytes of `resource`'s staging buffer, if it has one now (tests).
#[cfg(test)]
pub(crate) fn staged_bytes(resource: This) -> Option<u64> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?;
    let upload = staging.upload.lock().unwrap_or_else(|e| e.into_inner());
    upload.as_ref().map(|u| u.size)
}

/// What came of making a texture's staging buffer ([`stage`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Staged {
    /// It has one: these many bytes new (0: it had one already).
    Ready(u64),
    /// It would take these many bytes, more than the room given: none made.
    NoRoom(u64),
    /// None now (no host, or no upload memory for it): asked again later.
    NotNow,
    /// None ever: its contents cannot be uploaded (no console layout, an
    /// XOR mode, a stand-in format without a conversion).
    Never,
    /// None needed: a GPU write into its host copy was submitted, which
    /// keeps the host's contents.
    Host,
}

/// Makes the staging buffer of `resource` (host copy `texture`) for its
/// first upload if it has none, within `room` bytes; tells whether it has
/// one now. For the pass over textures shaders pick by index (`draw.rs`
/// `heap_textures`), which spends a budget on new ones: a texture that
/// already has one costs it nothing, and one that cannot have one yet is
/// looked at again later rather than taken for uploaded.
pub(crate) fn stage(resource: This, texture: &ID3D12Resource, room: u64) -> Staged {
    if gpu_written(resource as usize) {
        return Staged::Host;
    }
    match staging_within(resource, texture, room) {
        Ok((_, new)) => Staged::Ready(new),
        Err(why) => why,
    }
}

/// Whether `resource` has a staging buffer over title memory at `address`:
/// a texture placed there that lives yet (its staging buffer goes when it
/// is released). Only the address is used: `resource` may be released.
pub(crate) fn staged_at(resource: usize, address: u64) -> bool {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .and_then(|map| map.get(&resource))
        .is_some_and(|s| s.memory as u64 == address)
}

/// The bytes of every staging buffer that lives.
pub(crate) fn staged_total() -> u64 {
    STAGED.load(Ordering::Relaxed)
}

/// Staging buffers released by [`release_staging`] so far: count and bytes.
pub(crate) fn released_total() -> (u64, u64) {
    let [count, bytes] = RELEASED.each_ref().map(|a| a.load(Ordering::Relaxed));
    (count, bytes)
}

/// Releases the staging buffers that no list holds any more and whose
/// texture had no upload recorded in the last `idle` calls, looking at up
/// to `looks` textures from where the last call stopped. A list holds its
/// textures' staging states until it is reset, and one reset while it runs
/// until its submission has run: a state held by nothing but [`STAGING`]
/// is read by no submission, so its buffer can go (its last copy ran, the
/// fence passed). What was detiled and copied is kept: the next upload
/// makes the buffer again ([`staging_within`]), and detiles and copies
/// only if the memory changed or another host copy needs it. Returns the
/// buffers released and their bytes, which leave [`staged_total`]. Once a
/// pass over the textures picked by index (`descriptor::host`).
///
/// Ported from fxceless/victory-fork (578d459) by fxceless: its
/// `texture::let_go_idle` lets go of a staging state held by no list
/// (`Arc::strong_count`) and counts the bytes that live by their drop.
/// Changed here: only the buffer goes, not what was detiled and copied
/// (the fork's next upload detiles and copies again, and here a pass's
/// look at a served texture would find one without a state changed:
/// `memory_change`); idle is counted in calls, a pass each, not 10 s and
/// 4 frames (a world loading leaves gigabytes waiting for room); a call
/// looks at `looks` textures in turns, and the buffers are destroyed out
/// of the map's lock.
pub(crate) fn release_staging(idle: u64, looks: usize) -> (u64, u64) {
    release_staging_of(idle, looks, |_| true)
}

/// [`release_staging`] for the resources `of` takes (tests: other tests'
/// textures are in the same map).
fn release_staging_of(idle: u64, looks: usize, of: impl Fn(usize) -> bool) -> (u64, u64) {
    use std::ops::Bound::{Excluded, Unbounded};
    let now = SWEEPS.fetch_add(1, Ordering::Relaxed) + 1;
    let mut gone = Vec::new();
    {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        let Some(map) = map.as_ref() else {
            return (0, 0);
        };
        let mut cursor = RELEASE_CURSOR.lock().unwrap_or_else(|e| e.into_inner());
        let after = map.range((Excluded(*cursor), Unbounded));
        let before = map.range(..=*cursor);
        for (&resource, s) in after.chain(before).take(looks.min(map.len())) {
            *cursor = resource;
            // Under the map's lock: a state held by the map alone gets no
            // other holder meanwhile (every one is taken from the map).
            if Arc::strong_count(s) != 1
                || now.saturating_sub(s.recorded.load(Ordering::Relaxed)) < idle
                || !of(resource)
            {
                continue;
            }
            if let Some(upload) = s.upload.lock().unwrap_or_else(|e| e.into_inner()).take() {
                gone.push(upload);
            }
        }
    }
    let bytes: u64 = gone.iter().map(|u| u.size).sum();
    let count = gone.len() as u64;
    RELEASED[0].fetch_add(count, Ordering::Relaxed);
    RELEASED[1].fetch_add(bytes, Ordering::Relaxed);
    // Destroyed out of the map's lock: each one is a call into the runtime.
    drop(gone);
    (count, bytes)
}

/// The bytes of title memory a texture the title placed itself takes, by
/// its resource description; `None` without a console layout for it.
pub(crate) fn placed_len(resource: &[u32; 14]) -> Option<usize> {
    let words = crate::resource::canonical_resource_desc(*resource);
    // SAFETY: 56 bytes in the description's layout, as `texture_at` reads
    // them.
    let desc = unsafe {
        words
            .as_ptr()
            .cast::<crate::resource::ResourceDesc>()
            .read_unaligned()
    };
    Some(crate::resource::texture_layout(&desc)?.size as usize)
}

/// Whether any of `[at, at + len)` is committed memory (a texture streaming
/// in has some of its pages mapped before the others, not always its
/// first).
pub(crate) fn any_committed(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p.saturating_add(len.max(1));
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        if info.State == MEM_COMMIT {
            return true;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    false
}

/// Each copy decided in tests: its staging state's [`Staging::id`] and
/// whether it ran.
#[cfg(test)]
static DECIDED: Mutex<Vec<(u64, bool)>> = Mutex::new(Vec::new());

/// Each hash a check took in tests: its staging state's [`Staging::id`].
#[cfg(test)]
static HASHED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The ids of staging states in tests.
#[cfg(test)]
static IDS: AtomicU64 = AtomicU64::new(0);

/// GPU copies of staging buffers recorded, by whether they ran: count and
/// bytes (staging sizes), since the last summary.
static COPIES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static COPIED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// An environment variable's value, if set.
fn env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// Whether `value` turns off a switch that is on by default: `0`, `off`
/// or `false`, in any case.
fn off(value: &str) -> bool {
    let v = value.trim();
    v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false")
}

/// What `XTP_TEXTURE_GUARDS` asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardMode {
    /// `0`, `off`, `false`: every check hashes.
    Off,
    /// Unset or anything else: a check hashes only after a write (or when
    /// the safety net suspects the texture).
    On,
    /// `verify`, in a diagnostics build: every check the guards find clean
    /// is hashed too, a change counted as missed (a writer that opens no
    /// guard). Other builds take it as on.
    Verify,
}

/// The mode `value` asks for, in a diagnostics build or not.
fn guard_mode_of(value: Option<&str>, diagnostics: bool) -> GuardMode {
    match value.map(str::trim) {
        Some(v) if off(v) => GuardMode::Off,
        Some(v) if diagnostics && v.eq_ignore_ascii_case("verify") => GuardMode::Verify,
        _ => GuardMode::On,
    }
}

fn guard_mode() -> GuardMode {
    static MODE: OnceLock<GuardMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let value = env("XTP_TEXTURE_GUARDS");
        let diagnostics = cfg!(feature = "diagnostics");
        let mode = guard_mode_of(value.as_deref(), diagnostics);
        if !diagnostics && value.is_some_and(|v| v.trim().eq_ignore_ascii_case("verify")) {
            tracing::warn!(
                "XTP_TEXTURE_GUARDS=verify needs a diagnostics build: the guards are on, \
                 checked only by the safety net"
            );
        }
        mode
    })
}

/// Whether texture memory in views is guarded, a hash taken only after a
/// write (`XTP_TEXTURE_GUARDS`).
fn guards() -> bool {
    guard_mode() != GuardMode::Off
}

/// Whether every check the guards find clean is hashed too
/// (`XTP_TEXTURE_GUARDS=verify`, diagnostics).
fn verify_guards() -> bool {
    guard_mode() == GuardMode::Verify
}

/// Changes the guards missed, found by the safety net or by `verify`,
/// since the start.
static MISSED: AtomicU64 = AtomicU64::new(0);

/// Whether texture copies run under a predicate (`XTP_TEXTURE_PREDICATION`,
/// on unless `0`, `off` or `false`).
pub(crate) fn predication() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| env("XTP_TEXTURE_PREDICATION").is_none_or(|v| !off(&v)))
}

/// The safety net's budget in bytes a frame, as `value` (of
/// `XTP_TEXTURE_NET`) asks: MiB, 2 by default; `0`, `off` or `false` turn
/// the net off.
fn net_budget_of(value: Option<&str>) -> u64 {
    const DEFAULT_MIB: u64 = 2;
    let mib = match value.map(str::trim) {
        None => DEFAULT_MIB,
        Some(v) if off(v) => 0,
        Some(v) => v.parse::<u64>().unwrap_or_else(|_| {
            tracing::warn!(value = v, "XTP_TEXTURE_NET is not a number of MiB: 2 MiB");
            DEFAULT_MIB
        }),
    };
    mib.min(1 << 20) << 20
}

/// The safety net's budget a frame (`XTP_TEXTURE_NET`), 0 when it is off or
/// the guards are (every check then hashes).
fn net_budget() -> u64 {
    static BUDGET: OnceLock<u64> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let budget = net_budget_of(env("XTP_TEXTURE_NET").as_deref());
        if guards() { budget } else { 0 }
    })
}

/// What the safety net may still hash: a budget a frame, or a second where
/// frames do not advance (a title that does not wait for them).
#[derive(Debug, Default)]
struct Allowance {
    left: u64,
    frame: u64,
    given: Option<Instant>,
}

impl Allowance {
    /// A new budget once the frame changed or a second went by since the
    /// last one; what was left of that one is not carried over.
    fn renew(&mut self, budget: u64, frame: u64, now: Instant) {
        let due = self.given.is_none_or(|at| {
            frame != self.frame || now.saturating_duration_since(at) >= Duration::from_secs(1)
        });
        if due {
            *self = Allowance {
                left: budget,
                frame,
                given: Some(now),
            };
        }
    }
}

/// What the safety net reads and hashes at a time: a texture bigger than
/// what is left of a budget is hashed over several.
const NET_SLICE: usize = 256 << 10;

/// The shortest time from one lap of the safety net to the next: textures
/// that take less than its budget are not hashed again at every frame.
const NET_LAP: Duration = Duration::from_secs(2);

/// Whether the safety net may begin a lap at `now`, the last one begun at
/// `last`.
fn lap_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= NET_LAP)
}

/// The safety net (module doc): its budget, where its walk is, the texture
/// it is hashing.
#[derive(Default)]
struct Net {
    allowance: Allowance,
    /// The resource it last looked at: the walk goes on after it.
    cursor: usize,
    /// When it last went round from the last resource to the first.
    lap: Option<Instant>,
    hashing: Option<Hashing>,
    /// Room for what it reads.
    read: Vec<u8>,
}

/// A texture the safety net is hashing: how far it got, the hash so far,
/// and its staging buffer's writes when it began.
struct Hashing {
    staging: Arc<Staging>,
    at: usize,
    hasher: blake3::Hasher,
    written: u64,
}

impl Hashing {
    /// The whole memory hashed: the texture is suspect if the hash is not
    /// that of what was detiled, unless it was detiled again meanwhile
    /// (from a snapshot newer than some of what was hashed here).
    fn finish(self) {
        let hash = self.hasher.finalize();
        let last = self.staging.last.lock().unwrap_or_else(|e| e.into_inner());
        if self.staging.written.load(Ordering::Acquire) == self.written && last.hash != Some(hash) {
            self.staging.suspect.store(true, Ordering::Relaxed);
        }
    }
}

static NET: Mutex<Option<Net>> = Mutex::new(None);

/// The safety net's bytes hashed, microseconds and laps since the last
/// summary (diagnostics).
static NET_SEEN: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Frames and time for the safety net's budget and laps: the title's
/// frames and the clock, and in tests what a test adds ([`NET_LATER`]).
fn net_clock() -> (u64, Instant) {
    let (frame, now) = (crate::frame::current(), Instant::now());
    #[cfg(test)]
    let (frame, now) = {
        let [frames, ms] = NET_LATER.each_ref().map(|a| a.load(Ordering::Relaxed));
        (frame + frames, now + Duration::from_millis(ms))
    };
    (frame, now)
}

/// Frames and milliseconds tests add for the safety net: the title's frame
/// counter is `frame`'s tests' to advance, and tests do not wait for laps.
#[cfg(test)]
static NET_LATER: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The safety net's turn, at a check (module doc): hashes what its budget
/// allows of the guarded textures checked since it last hashed them, in
/// resource order from where it stopped, a lap every [`NET_LAP`] at most,
/// and marks suspect those whose memory no longer hashes as what was
/// detiled.
fn net_walk() {
    let budget = net_budget();
    if budget == 0 {
        return;
    }
    let started = Instant::now();
    let (frame, now) = net_clock();
    let mut net = NET.lock().unwrap_or_else(|e| e.into_inner());
    let Net {
        allowance,
        cursor,
        lap,
        hashing,
        read,
    } = net.get_or_insert_with(Net::default);
    allowance.renew(budget, frame, now);
    // Textures looked at in this walk: a lap at most.
    let mut looked = 0;
    let mut hashed = 0;
    while allowance.left > 0 {
        let Some(h) = hashing else {
            *hashing = next_sampled(cursor, &mut looked, lap, now);
            if hashing.is_none() {
                // Nothing left to hash, or the next lap is not due: the
                // rest waits for the next budget.
                allowance.left = 0;
            }
            continue;
        };
        let size = h.staging.layout.size as usize;
        if h.at < size {
            let n = (size - h.at).min(NET_SLICE).min(allowance.left as usize);
            read.resize(NET_SLICE, 0);
            // Through the kernel: the title may unmap the memory meanwhile.
            if !crate::host::read_into(h.staging.memory.wrapping_add(h.at), &mut read[..n]) {
                // Not all mapped: its check sees to it.
                *hashing = None;
                continue;
            }
            h.hasher.update(&read[..n]);
            h.at += n;
            allowance.left -= n as u64;
            hashed += n as u64;
        }
        if h.at >= size
            && let Some(done) = hashing.take()
        {
            done.finish();
        }
    }
    NET_SEEN[0].fetch_add(hashed, Ordering::Relaxed);
    NET_SEEN[1].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// The next texture after `cursor` checked through its guards since the
/// safety net last hashed it, going round to the first resource if a lap is
/// due (`lap`: when the last one began); `looked` counts the textures looked
/// at, a lap at most.
fn next_sampled(
    cursor: &mut usize,
    looked: &mut usize,
    lap: &mut Option<Instant>,
    now: Instant,
) -> Option<Hashing> {
    use std::ops::Bound::{Excluded, Unbounded};
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.as_ref()?;
    let after = map.range((Excluded(*cursor), Unbounded));
    for (&resource, s) in after.chain(map.range(..=*cursor)) {
        if *looked >= map.len() {
            return None;
        }
        if resource <= *cursor {
            // Round again.
            if !lap_due(*lap, now) {
                return None;
            }
            *lap = Some(now);
            NET_SEEN[2].fetch_add(1, Ordering::Relaxed);
        }
        *looked += 1;
        *cursor = resource;
        if s.sampled.swap(false, Ordering::Relaxed) {
            return Some(Hashing {
                staging: s.clone(),
                at: 0,
                hasher: blake3::Hasher::new(),
                written: s.written.load(Ordering::Acquire),
            });
        }
    }
    None
}

/// Predicates for texture copies: 8-byte words of one upload buffer (in a
/// read state, which predication's is part of), one per recorded copy.
struct Predicates {
    buffer: ID3D12Resource,
    data: *mut u64,
    free: Mutex<Vec<u32>>,
}

// SAFETY: D3D12 objects are free-threaded; each word of the persistent
// mapping is written only by the holder of its `Predicate`.
unsafe impl Send for Predicates {}
// SAFETY: as above.
unsafe impl Sync for Predicates {}

/// Predicate words: 64 Ki (512 KiB); a world frame records a few thousand
/// copies, held until their lists are reset.
const PREDICATES: u32 = 1 << 16;

static PREDICATE_WORDS: OnceLock<Option<Predicates>> = OnceLock::new();

/// One copy's predicate word: the copy runs unless it is 0. It goes back
/// when its holder goes (the list's submission step, dropped once the list
/// is reset and nothing running uses its recording).
struct Predicate {
    of: &'static Predicates,
    at: u32,
    /// The submission being prepared (`timestamps::submissions`, plus one)
    /// when the word was last set to run; 0: never.
    ran_in: AtomicU64,
}

impl Predicate {
    /// A free word, set to run (so a copy whose step never ran would run);
    /// `None` without predication or once they are all taken (logged once).
    fn take() -> Option<Predicate> {
        let of = PREDICATE_WORDS
            .get_or_init(|| {
                if !predication() {
                    return None;
                }
                let (buffer, data) = host()?.upload_buffer(u64::from(PREDICATES) * 8)?;
                crate::host::name(&buffer, || "texture copy predicates".into());
                Some(Predicates {
                    buffer,
                    data: data.cast(),
                    free: Mutex::new((0..PREDICATES).rev().collect()),
                })
            })
            .as_ref()?;
        let Some(at) = of.free.lock().unwrap_or_else(|e| e.into_inner()).pop() else {
            once!("texture copy predicates ran out: copies run every time");
            return None;
        };
        let p = Predicate {
            of,
            at,
            ran_in: AtomicU64::new(0),
        };
        p.set(true);
        Some(p)
    }

    fn offset(&self) -> u64 {
        u64::from(self.at) * 8
    }

    /// At the list's submission (`submission`: the submissions made
    /// before it): its copy runs if `due`, or if it was let run earlier in
    /// the same submission, which names the list twice. Both runs read the
    /// one word, so the second decision must not take the copy from the
    /// first (VIctory #1500, by A6thGraderFr, handles that case too).
    /// Returns whether the copy runs.
    fn decide(&self, due: bool, submission: u64) -> bool {
        let this = submission + 1;
        let runs = due || self.ran_in.load(Ordering::Relaxed) == this;
        if runs {
            self.ran_in.store(this, Ordering::Relaxed);
        }
        self.set(runs);
        runs
    }

    /// Before the list runs: whether its copy runs.
    fn set(&self, runs: bool) {
        // SAFETY: this word of the mapped buffer is ours; the list that
        // reads it is not running (its submission is being prepared).
        unsafe {
            self.of
                .data
                .add(self.at as usize)
                .write_volatile(u64::from(runs))
        };
    }
}

impl Drop for Predicate {
    fn drop(&mut self) {
        self.of
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(self.at);
    }
}

/// A buffer-to-texture copy into subresource `sub` of `dst` at `at`, from
/// title buffer `src` laid out as `footprint` (title format), when `dst`'s
/// host format stands in for the footprint's: recorded here, the title
/// bytes converted into an upload buffer at submission. False when the
/// formats agree (the caller copies as it is).
#[allow(clippy::too_many_arguments)] // the copy's own arguments
pub(crate) fn copy_converted(
    list: &HostList,
    dst: This,
    sub: u32,
    at: [u32; 3],
    src: This,
    footprint: &crate::resource::PlacedFootprint,
    source_box: *const [u32; 6],
) -> bool {
    let Some(to) = crate::resource::host_resource(dst) else {
        return false;
    };
    // SAFETY: a live resource.
    let host_format = unsafe { to.GetDesc() }.Format;
    let Some(convert) =
        crate::resource::format::conversion(footprint.format(), host_format.0 as u32)
    else {
        return false;
    };
    let Some(convert) = convert else {
        once!(
            format = footprint.format(),
            "buffer-to-texture copy into a stand-in without a conversion dropped"
        );
        return true;
    };
    let (Some(host), Some(bytes), Some((data, size))) = (
        host(),
        footprint.bytes(),
        crate::resource::buffer_bytes(src),
    ) else {
        return true;
    };
    if footprint
        .offset
        .checked_add(bytes)
        .is_none_or(|end| end > size)
    {
        tracing::warn!(
            offset = footprint.offset,
            bytes,
            size,
            "copy outside its buffer"
        );
        return true;
    }
    let (width, height, depth, title_pitch) = footprint.shape();
    let (from, to_bytes) = convert.bytes();
    let lines = height as usize * depth as usize;
    let host_pitch = (width as usize * to_bytes).next_multiple_of(256);
    let host_bytes = host_pitch * lines.saturating_sub(1) + width as usize * to_bytes;
    let Some((buffer, mapped)) = list.upload(host, host_bytes as u64) else {
        return true;
    };
    // At submission, when the title has written the buffer.
    let (title, mapped) = (data as usize + footprint.offset as usize, mapped as usize);
    let read = title_pitch as usize * lines.saturating_sub(1) + width as usize * from;
    list.defer_host((title, read), move || {
        let mut texels = vec![0u8; width as usize * from];
        for line in 0..lines {
            let row = (title + line * title_pitch as usize) as *const u8;
            if !crate::host::read_into(row, &mut texels) {
                texels.fill(0);
            }
            // SAFETY: row `line` of the mapped upload buffer, `width`
            // texels of the host format.
            let out = unsafe {
                std::slice::from_raw_parts_mut(
                    (mapped + line * host_pitch) as *mut u8,
                    width as usize * to_bytes,
                )
            };
            convert.row(&texels, out);
        }
    });
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, dst, COPY_DEST);
    gpu_write(list, dst, false);
    let to_location = D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(to) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: sub,
        },
    };
    let from_location = D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(&buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 0,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: host_format,
                    Width: width,
                    Height: height,
                    Depth: depth,
                    RowPitch: host_pitch as u32,
                },
            },
        },
    };
    let source_box = (!source_box.is_null()).then(|| source_box.cast::<D3D12_BOX>());
    // SAFETY: an open list; both locations name live resources (the upload
    // buffer lives until the list is reset); the box is the title's (the
    // host's layout, in texels of either format).
    unsafe {
        list.list.CopyTextureRegion(
            &to_location,
            at[0],
            at[1],
            at[2],
            &from_location,
            source_box,
        )
    };
    true
}

impl Staging {
    /// The title memory the texture is read from: start and end.
    pub(crate) fn range(&self) -> (usize, usize) {
        let start = self.memory as usize;
        (start, start + self.layout.size as usize)
    }

    /// The staging buffer, if it has one now.
    fn buffer(&self) -> Option<ID3D12Resource> {
        let upload = self.upload.lock().unwrap_or_else(|e| e.into_inner());
        upload.as_ref().map(|u| u.buffer.clone())
    }

    /// The staging buffer's mapping, if it has one now.
    fn mapped(&self) -> Option<*mut u8> {
        let upload = self.upload.lock().unwrap_or_else(|e| e.into_inner());
        upload.as_ref().map(|u| u.data)
    }

    /// Whether host copy `target` holds what was detiled last, as decided
    /// at the submissions so far (a copy into it from that detiling ran or
    /// was found not needed).
    fn holds(&self, target: Option<&Weak<crate::resource::HostTexture>>) -> bool {
        let Some(target) = target else {
            return false;
        };
        let written = self.written.load(Ordering::Acquire);
        let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        copied
            .as_ref()
            .is_some_and(|(w, t)| *w == written && Weak::ptr_eq(t, target))
    }

    /// Makes its staging buffer again if it was released, within `room`
    /// bytes: the bytes made (0 if it had one). The new one is blank.
    fn make_upload(&self, room: u64) -> Result<u64, Staged> {
        let mut upload = self.upload.lock().unwrap_or_else(|e| e.into_inner());
        if upload.is_some() {
            return Ok(0);
        }
        if self.size > room {
            return Err(Staged::NoRoom(self.size));
        }
        let host = host().ok_or(Staged::NotNow)?;
        let Some((buffer, data)) = host.upload_buffer(self.size) else {
            once!(
                size = self.size,
                "no upload memory for a texture's staging buffer made again: asked again at its next upload"
            );
            return Err(Staged::NotNow);
        };
        crate::host::name(&buffer, || {
            format!("staging of title memory {:?} (made again)", self.memory)
        });
        STAGED.fetch_add(self.size, Ordering::Relaxed);
        *upload = Some(Upload {
            buffer,
            data,
            size: self.size,
        });
        self.blank.store(true, Ordering::Release);
        Ok(self.size)
    }

    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all). `before_write` runs
    /// just before the staging buffer is overwritten: an earlier
    /// submission's GPU copy may still read it. The safety net takes its
    /// turn first (counted with the check).
    pub(crate) fn refresh(&self, before_write: impl FnOnce()) -> usize {
        net_walk();
        if self.host_owned.load(Ordering::Relaxed) {
            0
        } else {
            self.refresh_now(before_write)
        }
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self, before_write: impl FnOnce()) -> usize {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        // A blank buffer a copy is due from (`upload`): detiled whatever the
        // memory's checks say, as at its first check.
        if self.force.swap(false, Ordering::AcqRel) {
            last.asked = 0;
            last.hash = None;
        }
        #[cfg(feature = "diagnostics")]
        {
            let submission = crate::timestamps::submissions();
            self.seen.checks.fetch_add(1, Ordering::Relaxed);
            if last.checked.replace(submission) == Some(submission) {
                self.seen.again.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some((now, written)) = crate::watch::writes(start, start + size, false) {
            if last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return 0;
            }
            last.asked = now;
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            last.read.resize(size, 0);
            crate::host::copy_mapped(last.read.as_mut_ptr(), self.memory, size, false);
            before_write();
            self.detile(&last.read);
            self.written.fetch_add(1, Ordering::Release);
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
                self.seen.changed.fetch_add(1, Ordering::Relaxed);
            }
            return size;
        }
        // View memory: guarded, and hashed only after a write (or where a
        // block cannot be guarded, which every asking reports written).
        // Whether the guards reported a write (the memory most likely
        // changed), or found the memory clean though the safety net suspects
        // it or `verify` asks (a change is then one they missed).
        let (mut reported, mut unseen) = (false, false);
        if guards()
            && let Some((now, written)) = crate::watch::writes(start, start + size, true)
        {
            let clean = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            // A write they report is hashed below anyway.
            let suspect = self.suspect.swap(false, Ordering::Relaxed);
            if clean {
                #[cfg(feature = "diagnostics")]
                self.seen.clean.fetch_add(1, Ordering::Relaxed);
                if !suspect && !verify_guards() {
                    // Answered by the guards: the safety net's to hash.
                    self.sampled.store(true, Ordering::Relaxed);
                    return 0;
                }
                unseen = true;
            } else {
                reported = true;
            }
            // Hashed below: nothing for the safety net until the next check.
            self.sampled.store(false, Ordering::Relaxed);
        }
        if !committed(self.memory, size) {
            #[cfg(feature = "diagnostics")]
            tracing::debug!(
                memory = ?self.memory,
                size,
                desc = ?self.layout.desc,
                "a sampled texture's memory is not committed"
            );
            return 0;
        }
        // Memory the guards report written most likely changed: its
        // snapshot is taken at once. Otherwise it is hashed in place first,
        // and copied only if that tells a change.
        if !reported {
            // SAFETY: committed memory of the texture's layout size.
            let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
            if last.hash == Some(self.hash(tiled)) {
                return size;
            }
        }
        // Hashed and detiled from one snapshot (module doc), taken through
        // the copy, which stands pages unmapped since the check above. The
        // room is shared, and not held during the wait.
        let mut snapshot = std::mem::take(&mut *SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()));
        snapshot.resize(size, 0);
        crate::host::copy_mapped(snapshot.as_mut_ptr(), self.memory, size, false);
        let hash = self.hash(&snapshot);
        if last.hash != Some(hash) {
            last.hash = Some(hash);
            if unseen {
                self.missed(size);
            }
            before_write();
            self.detile(&snapshot);
            self.written.fetch_add(1, Ordering::Release);
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                self.seen.changed.fetch_add(1, Ordering::Relaxed);
            }
        }
        *SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()) = snapshot;
        size
    }

    /// The hash of `bytes`, this texture's memory or its snapshot.
    fn hash(&self, bytes: &[u8]) -> blake3::Hash {
        #[cfg(test)]
        HASHED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(self.id);
        #[cfg(feature = "diagnostics")]
        {
            crate::copystat::add(crate::copystat::C::TexHashed, bytes.len() as u64);
            self.seen
                .hashed
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        blake3::hash(bytes)
    }

    /// A change its guards missed, found by the safety net or by `verify`:
    /// counted, and logged the first time for this texture.
    fn missed(&self, size: usize) {
        let n = MISSED.fetch_add(1, Ordering::Relaxed) + 1;
        if self.missed.fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::warn!(
                memory = ?self.memory,
                size,
                desc = ?self.layout.desc,
                missed = n,
                "a texture changed that its guards found clean (a writer that opens no guard): \
                 refreshed"
            );
        }
    }

    /// At submission, once the staging buffer is up to date: whether a copy
    /// into `target` has to run for it to hold the staging buffer's bytes.
    /// It is then taken to hold them (the copy runs in submission order,
    /// before any later list's).
    fn copy_due(&self, target: &Weak<crate::resource::HostTexture>) -> bool {
        let written = self.written.load(Ordering::Acquire);
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        let due = !copied
            .as_ref()
            .is_some_and(|(w, t)| *w == written && Weak::ptr_eq(t, target));
        *copied = Some((written, target.clone()));
        due
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer, which
    /// is not blank from then on (a subresource that could not be detiled
    /// stays as it was, as before buffers were released).
    fn detile(&self, tiled: &[u8]) {
        // A list holding this state holds the buffer: there is one.
        let Some(data) = self.mapped() else {
            return;
        };
        match self.convert {
            Some(convert) => self.detile_converted(tiled, convert, data),
            None => self.detile_plain(tiled, data),
        }
        self.blank.store(false, Ordering::Release);
    }

    /// [`Staging::detile`] into `data`, the mapped staging buffer, in the
    /// host format.
    fn detile_plain(&self, tiled: &[u8], data: *mut u8) {
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = (i as u32 % self.mips, i as u32 / self.mips);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = pitch * rows as usize * f.Depth as usize;
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear =
                unsafe { std::slice::from_raw_parts_mut(data.add(footprint.Offset as usize), len) };
            if let Err(e) =
                self.layout
                    .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
    }
}

impl Staging {
    /// As [`Staging::detile`], into a stand-in format: each subresource
    /// detiled in the title's format, then converted row by row into
    /// `data`, the mapped staging buffer.
    fn detile_converted(&self, tiled: &[u8], convert: Convert, data: *mut u8) {
        let (from, to) = convert.bytes();
        let mut title = Vec::new();
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = (i as u32 % self.mips, i as u32 / self.mips);
            let f = footprint.Footprint;
            let width = f.Width as usize;
            let (pitch, lines) = (width * from, rows as usize * f.Depth as usize);
            title.clear();
            title.resize(pitch * lines, 0);
            if let Err(e) =
                self.layout
                    .detile(tiled, mip, slice, &mut title, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
            let host_pitch = f.RowPitch as usize;
            for (line, texels) in title.chunks_exact(pitch).enumerate() {
                // SAFETY: row `line` of the subresource's part of the mapped
                // staging buffer, `width` texels of the host format.
                let row = unsafe {
                    std::slice::from_raw_parts_mut(
                        data.add(footprint.Offset as usize + line * host_pitch),
                        width * to,
                    )
                };
                convert.row(texels, row);
            }
        }
    }
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// The staging state of a title texture, made on first use; why there is
/// none if it cannot be made.
fn staging(resource: This, texture: &ID3D12Resource) -> Result<Arc<Staging>, Staged> {
    staging_within(resource, texture, u64::MAX).map(|(s, _)| s)
}

/// [`staging`], made only within `room` bytes ([`Staged::NoRoom`] past
/// them), with the bytes made (0 if it was there). A state whose staging
/// buffer was released ([`release_staging`]) gets one again, blank.
fn staging_within(
    resource: This,
    texture: &ID3D12Resource,
    room: u64,
) -> Result<(Arc<Staging>, u64), Staged> {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(BTreeMap::new);
    if let Some(s) = map.get(&(resource as usize)) {
        let made = s.make_upload(room)?;
        return Ok((s.clone(), made));
    }
    let (memory, desc) = crate::resource::texture_memory(resource).ok_or(Staged::Never)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return Err(Staged::Never);
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return Err(Staged::Never);
    }
    let host = host().ok_or(Staged::NotNow)?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let convert = match crate::resource::format::conversion(desc.format(), d.Format.0 as u32) {
        Some(None) => {
            once!(
                ?desc,
                "texture contents in a format whose stand-in has no conversion not uploaded"
            );
            return Err(Staged::Never);
        }
        Some(convert) => convert,
        None => None,
    };
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    if total > room {
        return Err(Staged::NoRoom(total));
    }
    let Some((buffer, data)) = host.upload_buffer(total) else {
        once!(
            ?desc,
            total,
            "no upload memory for a texture's staging buffer: asked again at its next upload"
        );
        return Err(Staged::NotNow);
    };
    crate::host::name(&buffer, || {
        format!(
            "staging of {resource:?} {}x{}x{} f{}",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0
        )
    });
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    // Under a target of its own, one line a texture: which title memory
    // gets uploaded at all, through tables or by index
    // (`XTP_LOG=info,xtp_gpu::texture_memory=debug`).
    tracing::debug!(
        target: "xtp_gpu::texture_memory",
        memory = ?memory,
        width = d.Width,
        height = d.Height,
        format = d.Format.0,
        bytes = layout.size,
        "a texture the title fills gets a staging buffer"
    );
    let s = Arc::new(Staging {
        upload: Mutex::new(Some(Upload {
            buffer,
            data,
            size: total,
        })),
        blank: AtomicBool::new(true),
        force: AtomicBool::new(false),
        recorded: AtomicU64::new(SWEEPS.load(Ordering::Relaxed)),
        size: total,
        footprints,
        rows,
        layout,
        convert,
        mips: u32::from(d.MipLevels),
        memory,
        last: Mutex::default(),
        written: AtomicU64::new(0),
        copied: Mutex::new(None),
        sampled: AtomicBool::new(false),
        suspect: AtomicBool::new(false),
        missed: AtomicU64::new(0),
        host_owned: AtomicBool::new(false),
        #[cfg(test)]
        id: IDS.fetch_add(1, Ordering::Relaxed),
        #[cfg(feature = "diagnostics")]
        seen: Seen::default(),
    });
    STAGED.fetch_add(s.size, Ordering::Relaxed);
    map.insert(resource as usize, s.clone());
    Ok((s, total))
}

/// Every few seconds (diagnostics): per frame, the textures checked, how
/// often, the bytes hashed and the changes, the staging copies recorded and
/// those that ran, the safety net's work, and the textures that took most
/// hashing.
#[cfg(feature = "diagnostics")]
fn summary_due() {
    static NEXT: Mutex<Option<(Instant, u64)>> = Mutex::new(None);
    let (frames, seconds) = {
        let mut next = NEXT.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let frame = crate::frame::current();
        match *next {
            None => {
                *next = Some((now, frame));
                return;
            }
            Some((at, _)) if now.duration_since(at) < Duration::from_secs(5) => return,
            Some((at, from)) => {
                *next = Some((now, frame));
                (
                    frame.saturating_sub(from).max(1) as f64,
                    now.duration_since(at).as_secs_f64(),
                )
            }
        }
    };
    let take = |a: &AtomicU64| a.swap(0, Ordering::Relaxed) as f64 / frames;
    let mb = |bytes: f64| bytes / 1e6;
    /// One texture's line, per frame.
    struct Row {
        checks: f64,
        again: f64,
        clean: f64,
        hashed_mb: f64,
        changes: f64,
        staging: u64,
        memory: usize,
        desc: String,
    }
    let mut rows: Vec<Row> = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .flat_map(|m| m.values())
            .map(|s| Row {
                checks: take(&s.seen.checks),
                again: take(&s.seen.again),
                clean: take(&s.seen.clean),
                hashed_mb: mb(take(&s.seen.hashed)),
                changes: take(&s.seen.changed),
                staging: s.size,
                memory: s.memory as usize,
                desc: format!(
                    "{}x{}x{}, {} mips, {}-byte elements of {} texels square, {:?}, {:.2} MB in \
                     title memory",
                    s.layout.desc.width,
                    s.layout.desc.height,
                    s.layout.desc.depth,
                    s.mips,
                    s.layout.desc.element_bytes,
                    s.layout.desc.block,
                    s.layout.desc.swizzle,
                    s.layout.size as f64 / 1e6
                ),
            })
            .filter(|r| r.checks > 0.0)
            .collect()
    };
    let [skipped, ran] = [0, 1].map(|i| (take(&COPIES[i]), mb(take(&COPIED[i]))));
    let missed = MISSED.load(Ordering::Relaxed);
    let sum = |f: fn(&Row) -> f64| -> f64 { rows.iter().map(f).sum() };
    tracing::info!(
        "texture checks per frame: {} textures checked {:.0} times ({:.0} again in a submission \
         that had checked them, {:.0} clean by their guards), {:.1} MB hashed, {:.1} changes; \
         staging copies {:.0} ran ({:.1} MB), {:.0} skipped ({:.1} MB)",
        rows.len(),
        sum(|r| r.checks),
        sum(|r| r.again),
        sum(|r| r.clean),
        sum(|r| r.hashed_mb),
        sum(|r| r.changes),
        ran.0,
        ran.1,
        skipped.0,
        skipped.1
    );
    let [net_mb, net_ms] = [mb(take(&NET_SEEN[0])), take(&NET_SEEN[1]) / 1e3];
    let laps = NET_SEEN[2].swap(0, Ordering::Relaxed);
    tracing::info!(
        "texture safety net per frame: {net_mb:.2} MB hashed in {net_ms:.2} ms (budget {:.1} MB); \
         {laps} laps of the textures in {seconds:.1} s; changes the guards missed so far \
         {missed}",
        mb(net_budget() as f64)
    );
    rows.sort_by(|a, b| {
        b.hashed_mb
            .total_cmp(&a.hashed_mb)
            .then(b.checks.total_cmp(&a.checks))
    });
    for r in rows.iter().take(12) {
        tracing::info!(
            "  texture at {:#x} ({}, staging {:.2} MB): {:.1} checks/frame ({:.1} again, {:.1} \
             clean), {:.1} MB hashed, {:.2} changes",
            r.memory,
            r.desc,
            r.staging as f64 / 1e6,
            r.checks,
            r.again,
            r.clean,
            r.hashed_mb,
            r.changes
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::tests_support::{committed, read_back, sampled};

    const COPY_DEST: u32 = 0x400;

    /// The decisions made for `staging` so far, in order.
    fn decided(staging: &Arc<Staging>) -> Vec<bool> {
        decided_id(staging.id)
    }

    /// The decisions made so far for the staging state of id `key`.
    fn decided_id(key: u64) -> Vec<bool> {
        DECIDED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(k, _)| *k == key)
            .map(|&(_, ran)| ran)
            .collect()
    }

    /// Runs one host list per entry of `lists` in one submission, each
    /// sampling `resource` (its staging copy recorded as a draw does).
    fn sample_in(host: &crate::host::Host, resource: This, lists: usize) {
        let texture = crate::resource::host_resource(resource).expect("a host copy");
        let made: Vec<HostList> = (0..lists).map(|_| host.new_list().unwrap()).collect();
        for list in &made {
            upload(list, resource, texture);
            list.close();
        }
        let runs: Vec<&HostList> = made.iter().collect();
        host.execute(&runs);
        host.drain();
    }

    /// A texture the title fills is copied into its host copy by the first
    /// list that samples it, and not by the others, nor by later lists
    /// until its memory changes: the GPU skips their copies (a staging
    /// buffer written behind its back does not reach the host copy), and a
    /// change is copied by the next list.
    #[test]
    fn a_texture_is_copied_once_until_its_memory_changes() {
        let test = "texture::tests::a_texture_is_copied_once_until_its_memory_changes";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        if !predication() {
            return;
        }
        let resource = committed(&sampled(16, 16, 28));
        let started = std::time::Instant::now();
        while crate::resource::host_resource(resource).is_none() {
            assert!(
                started.elapsed().as_secs() < 5,
                "the host copy never became resident"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let texture = crate::resource::host_resource(resource).unwrap();
        let (memory, _) = crate::resource::texture_memory(resource).unwrap();
        let size = staging(resource, texture).unwrap().layout.size as usize;
        let fill = |byte: u8| {
            // SAFETY: the texture's own title memory, `size` bytes.
            unsafe { memory.cast_mut().write_bytes(byte, size) }
        };
        let texels = || -> Vec<u8> {
            let read = read_back(host, texture, COPY_DEST);
            (0..16)
                .flat_map(|y| read[y * 256..y * 256 + 64].to_vec())
                .collect()
        };
        let staging = staging(resource, texture).unwrap();

        // Two lists in one submission: the first copies, the second not.
        fill(0x11);
        sample_in(host, resource, 2);
        assert_eq!(decided(&staging), [true, false]);
        assert!(texels().iter().all(|&b| b == 0x11));

        // The memory did not change: the next list's copy is skipped on
        // the GPU, so what is in the staging buffer does not get through.
        // SAFETY: the mapped staging buffer, `size` bytes; nothing runs.
        unsafe {
            staging
                .mapped()
                .unwrap()
                .write_bytes(0x22, staging.size as usize)
        };
        sample_in(host, resource, 1);
        assert_eq!(decided(&staging), [true, false, false]);
        assert!(
            texels().iter().all(|&b| b == 0x11),
            "a skipped copy ran: {:x?}",
            &texels()[..16]
        );

        // A change is detiled and copied by the next list.
        fill(0x33);
        sample_in(host, resource, 2);
        assert_eq!(decided(&staging), [true, false, false, true, false]);
        assert!(texels().iter().all(|&b| b == 0x33));
        type Release = extern "system" fn(This) -> u32;
        assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
    }

    /// A copy into another host copy is due even though the staging buffer
    /// did not change (the title's resource got a new host copy), and so
    /// is one into the first again after it.
    #[test]
    fn another_host_copy_is_copied_into() {
        let test = "texture::tests::another_host_copy_is_copied_into";
        let Some(_gpu) = crate::testgpu::device(test) else {
            return;
        };
        let [first, second] = [0, 1].map(|_| committed(&sampled(16, 16, 28)));
        let started = std::time::Instant::now();
        while [first, second]
            .iter()
            .any(|&r| crate::resource::host_copy(r).is_none())
        {
            assert!(
                started.elapsed().as_secs() < 5,
                "the host copies never became resident"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let [a, b] = [first, second].map(|r| crate::resource::host_copy(r).unwrap());
        let staging = staging(first, &a.texture).unwrap();
        let [a, b] = [&a, &b].map(Arc::downgrade);
        assert!(staging.copy_due(&a), "never copied");
        assert!(!staging.copy_due(&a), "copied, not changed");
        assert!(staging.copy_due(&b), "another host copy");
        assert!(staging.copy_due(&a), "the first one got the other's turn");
        staging.written.fetch_add(1, Ordering::Release);
        assert!(staging.copy_due(&a), "written again");
        assert!(!staging.copy_due(&a));
        type Release = extern "system" fn(This) -> u32;
        for r in [first, second] {
            assert_eq!(crate::object::slot::<Release>(r, 0x10)(r), 0);
        }
    }

    /// The times `staging` was hashed so far.
    fn hashes(staging: &Arc<Staging>) -> usize {
        let key = staging.id;
        HASHED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|&&k| k == key)
            .count()
    }

    /// A texture in view memory (physical pages, which cannot be
    /// write-watched) is guarded: checks without a write in between take no
    /// hash and copy nothing; a write by the CPU (which faults), or one the
    /// writer reports (as xmem does after DirectStorage writes through a
    /// view of its own), is hashed, detiled and copied.
    #[test]
    fn a_texture_in_a_view_is_hashed_only_after_a_write() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
        };
        let test = "texture::tests::a_texture_in_a_view_is_hashed_only_after_a_write";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        if !guards() || !predication() {
            return;
        }
        // Guards are the process's: one test that guards views at a time
        // (the watch's own tests count what is guarded).
        let _serial = crate::watch::tests::serial();
        const LEN: usize = 64 << 10;
        // SAFETY: a fresh pagefile section mapped twice, read-write: the
        // title's view and a writer's own (as xmem's is).
        let (view, other) = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                LEN as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, LEN).Value as usize;
            let other = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, LEN).Value as usize;
            windows_sys::Win32::Foundation::CloseHandle(section);
            assert!(view != 0 && other != 0);
            (view, other)
        };
        let resource =
            crate::resource::tests_support::at_address(view as u64, &sampled(16, 16, 28));
        let started = std::time::Instant::now();
        while crate::resource::host_resource(resource).is_none() {
            assert!(
                started.elapsed().as_secs() < 5,
                "the host copy never became resident"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let texture = crate::resource::host_resource(resource).unwrap();
        let staging = staging(resource, texture).unwrap();
        let size = staging.layout.size as usize;
        assert!(size <= LEN);
        let texels = || -> Vec<u8> {
            let read = read_back(host, texture, COPY_DEST);
            (0..16)
                .flat_map(|y| read[y * 256..y * 256 + 64].to_vec())
                .collect()
        };
        // SAFETY: bytes of the title's view, as its CPU writes them (a
        // fault into the guards' handler where guarded).
        let fill = |at: usize, byte: u8| unsafe { (at as *mut u8).write_bytes(byte, size) };

        fill(view, 0x44);
        sample_in(host, resource, 1);
        assert_eq!((hashes(&staging), decided(&staging)), (1, vec![true]));
        assert!(texels().iter().all(|&b| b == 0x44));

        // Nothing written: no hash, no copy.
        sample_in(host, resource, 2);
        assert_eq!(
            (hashes(&staging), decided(&staging)),
            (1, vec![true, false, false])
        );

        // The CPU writes through the guarded view.
        fill(view, 0x55);
        sample_in(host, resource, 1);
        assert_eq!(hashes(&staging), 2);
        assert_eq!(decided(&staging).last(), Some(&true));
        assert!(texels().iter().all(|&b| b == 0x55));

        // A writer through its own view, which reports what it wrote.
        fill(other, 0x66);
        crate::watch::writing(view, size);
        sample_in(host, resource, 1);
        assert_eq!(hashes(&staging), 3);
        assert_eq!(decided(&staging).last(), Some(&true));
        assert!(texels().iter().all(|&b| b == 0x66));

        type Release = extern "system" fn(This) -> u32;
        assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
        // Its guards go back (other tests count what is guarded); the views
        // stay mapped until the tests end: the watch's journal keeps the
        // pages these checks found written, which a test that allocates
        // fresh memory at the same address would read as its own.
        crate::watch::title_memory_remapped(view, LEN);
        let _ = other;
    }

    /// A list named twice in one submission runs its copy in both runs if
    /// the first needs it: both read its one predicate word. Submitted
    /// again in a later submission, nothing changed, it does not: the
    /// submissions are counted with timestamps off too (the default here).
    #[test]
    fn a_list_named_twice_in_a_submission_copies() {
        crate::testgpu::known_layer_errors(&[553], "#45");
        let test = "texture::tests::a_list_named_twice_in_a_submission_copies";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        if !predication() {
            return;
        }
        let resource = committed(&sampled(16, 16, 28));
        let started = std::time::Instant::now();
        while crate::resource::host_resource(resource).is_none() {
            assert!(
                started.elapsed().as_secs() < 5,
                "the host copy never became resident"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let texture = crate::resource::host_resource(resource).unwrap();
        let (memory, _) = crate::resource::texture_memory(resource).unwrap();
        let staging = staging(resource, texture).unwrap();
        let size = staging.layout.size as usize;
        // SAFETY: the texture's own title memory, `size` bytes.
        unsafe { memory.cast_mut().write_bytes(0x11, size) };
        sample_in(host, resource, 1);
        // SAFETY: as above.
        unsafe { memory.cast_mut().write_bytes(0x77, size) };
        let list = host.new_list().unwrap();
        upload(&list, resource, texture);
        list.close();
        host.execute(&[&list, &list]);
        host.drain();
        assert_eq!(decided(&staging), [true, true, true]);
        let read = read_back(host, texture, COPY_DEST);
        let texels: Vec<u8> = (0..16)
            .flat_map(|y| read[y * 256..y * 256 + 64].to_vec())
            .collect();
        assert!(texels.iter().all(|&b| b == 0x77), "{:x?}", &texels[..16]);
        // The same recording in a submission of its own, the memory as it
        // was: told from a second run of the last one by the count.
        host.execute(&[&list]);
        host.drain();
        assert_eq!(decided(&staging), [true, true, true, false]);
        type Release = extern "system" fn(This) -> u32;
        assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
    }

    #[test]
    fn switches_turn_off_with_0_off_or_false() {
        for v in ["0", "off", "OFF", "false", "False", " 0 "] {
            assert!(off(v), "{v:?}");
            assert_eq!(guard_mode_of(Some(v), true), GuardMode::Off, "{v:?}");
            assert_eq!(net_budget_of(Some(v)), 0, "{v:?}");
        }
        for v in ["1", "on", "yes", "", "verify"] {
            assert!(!off(v), "{v:?}");
        }
        assert_eq!(guard_mode_of(None, true), GuardMode::On);
        assert_eq!(guard_mode_of(Some("1"), false), GuardMode::On);
        // `verify` only where its counters are: on otherwise (and warned).
        assert_eq!(guard_mode_of(Some("verify"), true), GuardMode::Verify);
        assert_eq!(guard_mode_of(Some("Verify"), true), GuardMode::Verify);
        assert_eq!(guard_mode_of(Some("verify"), false), GuardMode::On);
        assert_eq!(net_budget_of(None), 2 << 20);
        assert_eq!(net_budget_of(Some("8")), 8 << 20);
        assert_eq!(net_budget_of(Some("lots")), 2 << 20);
    }

    /// The safety net's budget comes once a frame, whatever is spent of it,
    /// and once a second where frames do not advance; its laps come two
    /// seconds apart at least.
    #[test]
    fn the_net_is_given_its_budget_once_a_frame() {
        let t0 = Instant::now();
        let mut a = Allowance::default();
        a.renew(100, 7, t0);
        assert_eq!(a.left, 100);
        a.left = 30;
        // The same frame: what is left stays.
        a.renew(100, 7, t0 + Duration::from_millis(500));
        assert_eq!(a.left, 30);
        // The next frame: a budget, not what was left on top.
        a.renew(100, 8, t0 + Duration::from_millis(600));
        assert_eq!(a.left, 100);
        a.left = 0;
        // No frame for a second: a budget all the same.
        a.renew(100, 8, t0 + Duration::from_millis(1_500));
        assert_eq!(a.left, 0);
        a.renew(100, 8, t0 + Duration::from_millis(1_600));
        assert_eq!(a.left, 100);
        // Laps: the first at once, the next one not before `NET_LAP`.
        assert!(lap_due(None, t0));
        assert!(!lap_due(Some(t0), t0 + NET_LAP / 2));
        assert!(lap_due(Some(t0), t0 + NET_LAP));
    }

    /// The next frame for the safety net, a lap after the last.
    fn net_next_frame() {
        NET_LATER[0].fetch_add(1, Ordering::Relaxed);
        NET_LATER[1].fetch_add(NET_LAP.as_millis() as u64, Ordering::Relaxed);
    }

    /// Two read-write views of one fresh pagefile section of `len` bytes:
    /// the title's and a writer's own (as xmem's is).
    fn section_views(len: usize) -> (usize, usize) {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
        };
        // SAFETY: a fresh pagefile section mapped twice, read-write.
        unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                len as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
            let other = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
            windows_sys::Win32::Foundation::CloseHandle(section);
            assert!(view != 0 && other != 0);
            (view, other)
        }
    }

    /// What the safety net's work costs a frame, out of the game: its
    /// budget read through the kernel a slice at a time and hashed, over a
    /// 64 MiB view (more than the CPU's caches, as the title's texture pool
    /// is), against hashing it in place; and a check's snapshot (copied,
    /// then hashed) against the hash in place it replaces where the guards
    /// report a write. Best of three passes. Run with
    /// `cargo test --release -p xtp-gpu -- --ignored --nocapture net_costs`.
    #[test]
    #[ignore = "benchmark"]
    fn net_costs() {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        const LEN: usize = 64 << 20;
        let (view, other) = section_views(LEN);
        // SAFETY: the view, `LEN` bytes, ours alone.
        let memory = unsafe { std::slice::from_raw_parts_mut(view as *mut u8, LEN) };
        for (i, b) in memory.iter_mut().enumerate() {
            *b = (i.wrapping_mul(2_654_435_761) >> 13) as u8;
        }
        let budget = net_budget_of(None) as usize;
        let rounds = LEN / budget;
        // Milliseconds per budget's bytes, the best of three passes over
        // the view.
        let time = |f: &mut dyn FnMut(usize)| -> f64 {
            (0..3)
                .map(|_| {
                    let started = Instant::now();
                    for r in 0..rounds {
                        f(view + r * budget);
                    }
                    started.elapsed().as_secs_f64() * 1e3 / rounds as f64
                })
                .fold(f64::INFINITY, f64::min)
        };
        let mut read = vec![0u8; NET_SLICE];
        let net = time(&mut |at| {
            let mut hasher = blake3::Hasher::new();
            for off in (0..budget).step_by(NET_SLICE) {
                assert!(crate::host::read_into((at + off) as *const u8, &mut read));
                hasher.update(&read);
            }
            std::hint::black_box(hasher.finalize());
        });
        let in_place = time(&mut |at| {
            // SAFETY: inside the view.
            let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, budget) };
            std::hint::black_box(blake3::hash(bytes));
        });
        let mut snapshot = vec![0u8; budget];
        let snap = time(&mut |at| {
            crate::host::copy_mapped(snapshot.as_mut_ptr(), at as *const u8, budget, false);
            std::hint::black_box(blake3::hash(&snapshot));
        });
        eprintln!(
            "texture safety net, per {} MiB (its budget a frame): {net:.3} ms read through the \
             kernel and hashed ({:.0} MB/s), {in_place:.3} ms hashed in place; a check's \
             snapshot, copied and hashed: {snap:.3} ms",
            budget >> 20,
            budget as f64 / 1e3 / net
        );
        for at in [view, other] {
            // SAFETY: views mapped above, not used after.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut _,
                })
            };
        }
    }

    /// A title texture over the title's view `view`, once its host copy is
    /// resident.
    fn texture_in(view: usize) -> (This, &'static ID3D12Resource) {
        let resource =
            crate::resource::tests_support::at_address(view as u64, &sampled(16, 16, 28));
        let started = std::time::Instant::now();
        loop {
            if let Some(texture) = crate::resource::host_resource(resource) {
                return (resource, texture);
            }
            assert!(
                started.elapsed().as_secs() < 5,
                "the host copy never became resident"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// The first rows of `texture` (16x16, 4 bytes a texel), as the host
    /// copy holds them.
    fn texels_of(host: &crate::host::Host, texture: &ID3D12Resource) -> Vec<u8> {
        let read = read_back(host, texture, COPY_DEST);
        (0..16)
            .flat_map(|y| read[y * 256..y * 256 + 64].to_vec())
            .collect()
    }

    /// A check hashes the memory it detiles: the title writing during the
    /// wait for an earlier copy, then writing back the bytes that were
    /// there, leaves the host copy with what the memory holds (the hash
    /// taken before the wait matched the bytes written back, and the
    /// staging buffer kept the write made during it).
    #[test]
    fn a_texture_is_hashed_as_it_is_detiled() {
        let test = "texture::tests::a_texture_is_hashed_as_it_is_detiled";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        if !guards() || !predication() {
            return;
        }
        let _serial = crate::watch::tests::serial();
        const LEN: usize = 64 << 10;
        let (view, other) = section_views(LEN);
        let (resource, texture) = texture_in(view);
        let staging = staging(resource, texture).unwrap();
        let size = staging.layout.size as usize;
        assert!(size <= LEN);
        // SAFETY: bytes of the title's view, as its CPU writes them (a
        // fault into the guards' handler where guarded).
        let fill = |byte: u8| unsafe { (view as *mut u8).write_bytes(byte, size) };

        fill(0x11);
        sample_in(host, resource, 1);
        assert!(texels_of(host, texture).iter().all(|&b| b == 0x11));

        // Written: the next check hashes and detiles, and while it waits
        // for an earlier copy the title writes again.
        fill(0x22);
        staging.refresh(|| fill(0x33));
        // Then writes back what the check took.
        fill(0x22);
        sample_in(host, resource, 1);
        let texels = texels_of(host, texture);
        assert!(texels.iter().all(|&b| b == 0x22), "{:x?}", &texels[..16]);

        type Release = extern "system" fn(This) -> u32;
        assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
        // As in `a_texture_in_a_view_is_hashed_only_after_a_write`.
        crate::watch::title_memory_remapped(view, LEN);
        let _ = other;
    }

    /// A write no guard sees (through another view of the title's pages,
    /// which nothing guards nor reports) is found by the safety net within
    /// a frame's budget: the texture is refreshed at the check after its
    /// walk, the change counted and logged as missed, and found clean again
    /// after.
    #[test]
    fn a_write_no_guard_saw_is_found_by_the_safety_net() {
        let test = "texture::tests::a_write_no_guard_saw_is_found_by_the_safety_net";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        if !guards() || !predication() || net_budget() == 0 {
            return;
        }
        let _serial = crate::watch::tests::serial();
        const LEN: usize = 64 << 10;
        let (view, other) = section_views(LEN);
        let (resource, texture) = texture_in(view);
        let staging = staging(resource, texture).unwrap();
        let size = staging.layout.size as usize;
        // The budget covers it: what it hashes is the guarded textures
        // checked since, and the other tests' that guard textures take the
        // same lock as this one.
        assert!(size as u64 <= net_budget());
        // SAFETY: bytes of the title's view, or the other one, `size` of
        // them.
        let fill = |at: usize, byte: u8| unsafe { (at as *mut u8).write_bytes(byte, size) };

        fill(view, 0x44);
        sample_in(host, resource, 1);
        assert!(texels_of(host, texture).iter().all(|&b| b == 0x44));
        // Clean: no hash at the check.
        let hashed = hashes(&staging);
        sample_in(host, resource, 1);
        assert_eq!(hashes(&staging), hashed);

        // Through the other view, reported to no one.
        fill(other, 0x99);
        let missed = MISSED.load(Ordering::Relaxed);
        // Each check in a frame of its own a lap after the last, the net's
        // budget given anew. The walk at a check hashes the texture if a
        // check found it clean
        // since the net last hashed it: the last one did, unless another
        // test's walk (every check walks) hashed it before the write; then
        // this check finds it clean and the next walk hashes it. Changed,
        // it is refreshed by the check that walk comes before.
        let mut frames = 0;
        while !texels_of(host, texture).iter().all(|&b| b == 0x99) {
            frames += 1;
            assert!(frames <= 2, "not refreshed in {} frames", frames - 1);
            net_next_frame();
            sample_in(host, resource, 1);
        }
        assert_eq!(staging.missed.load(Ordering::Relaxed), 1);
        assert!(MISSED.load(Ordering::Relaxed) > missed);
        assert_eq!(decided(&staging).last(), Some(&true));

        // Hashed again by the net, it is not suspect: its checks stay clean.
        let hashed = hashes(&staging);
        for _ in 0..2 {
            net_next_frame();
            sample_in(host, resource, 1);
        }
        assert_eq!(hashes(&staging), hashed);
        assert_eq!(staging.missed.load(Ordering::Relaxed), 1);

        type Release = extern "system" fn(This) -> u32;
        assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
        crate::watch::title_memory_remapped(view, LEN);
    }

    /// The look a pass over the textures shaders pick by index takes before
    /// it records an upload (`memory_change`): a texture without a staging
    /// buffer is due (nothing was uploaded from it: a staging buffer that
    /// could not be made is not "clean"); one never detiled is due; one in
    /// view memory its guards find clean is not, and is once the CPU wrote
    /// it, until a check has detiled the write (asking takes nothing from
    /// the check); one in memory neither guarded nor write-watched only the
    /// check's hash tells; one a GPU write was submitted into has nothing to
    /// upload, once that write's list is submitted and not before.
    #[test]
    fn a_look_before_an_upload_tells_written_memory() {
        let test = "texture::tests::a_look_before_an_upload_tells_written_memory";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        if !guards() || verify_guards() || !predication() {
            return;
        }
        let _serial = crate::watch::tests::serial();
        const LEN: usize = 64 << 10;
        let (view, other) = section_views(LEN);
        let (resource, texture) = texture_in(view);
        assert_eq!(memory_change(resource), Change::Yes, "never staged");
        let staging = staging(resource, texture).unwrap();
        assert_eq!(memory_change(resource), Change::Yes, "never detiled");
        let size = staging.layout.size as usize;
        // SAFETY: bytes of the title's view, as its CPU writes them (a
        // fault into the guards' handler).
        let fill = |byte: u8| unsafe { (view as *mut u8).write_bytes(byte, size) };
        fill(0x11);
        sample_in(host, resource, 1);
        assert_eq!(memory_change(resource), Change::No, "clean");
        fill(0x22);
        assert_eq!(memory_change(resource), Change::Yes, "written");
        assert_eq!(memory_change(resource), Change::Yes, "still, until a check");
        sample_in(host, resource, 1);
        assert!(texels_of(host, texture).iter().all(|&b| b == 0x22));
        assert_eq!(memory_change(resource), Change::No, "detiled");

        // A committed texture: memory of its own, which neither covers.
        let plain = committed(&sampled(16, 16, 28));
        let started = Instant::now();
        while crate::resource::host_resource(plain).is_none() {
            assert!(started.elapsed().as_secs() < 5, "never resident");
            std::thread::sleep(Duration::from_millis(1));
        }
        let own = super::staging(plain, crate::resource::host_resource(plain).unwrap()).unwrap();
        sample_in(host, plain, 1);
        assert_eq!(memory_change(plain), Change::Unknown(own.layout.size));
        // One the host copies into keeps the host's contents, from the
        // copy's submission on.
        let list = host.new_list().unwrap();
        gpu_write(&list, plain, true);
        list.close();
        assert_eq!(memory_change(plain), Change::Unknown(own.layout.size));
        host.execute(&[&list]);
        host.drain();
        assert_eq!(memory_change(plain), Change::No);
        assert_eq!(staged_bytes(plain), None, "its staging buffer goes");

        type Release = extern "system" fn(This) -> u32;
        for r in [resource, plain] {
            assert_eq!(crate::object::slot::<Release>(r, 0x10)(r), 0);
        }
        crate::watch::title_memory_remapped(view, LEN);
        let _ = other;
    }

    /// What the look before an upload costs (`memory_change`), out of the
    /// game: 1,024 textures in view memory, as the title's texture pool is
    /// (guarded), each 64 KiB from the next, staged and detiled once, then
    /// looked at with none written (best of three rounds), and after the
    /// CPU wrote each. Run with
    /// `cargo test --release -p xtp-gpu -- --ignored --nocapture memory_change_costs`.
    #[test]
    #[ignore = "benchmark"]
    fn memory_change_costs() {
        let test = "texture::tests::memory_change_costs";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        if !guards() || verify_guards() {
            return;
        }
        let _serial = crate::watch::tests::serial();
        const N: usize = 1024;
        const STEP: usize = 64 << 10;
        let (view, other) = section_views(N * STEP);
        let textures: Vec<(This, &ID3D12Resource)> =
            (0..N).map(|i| texture_in(view + i * STEP)).collect();
        // One list stages and uploads them all.
        let list = host.new_list().unwrap();
        for &(resource, texture) in &textures {
            upload(&list, resource, texture);
        }
        list.close();
        host.execute(&[&list]);
        host.drain();
        let round = || -> (f64, usize) {
            let started = Instant::now();
            let changed = textures
                .iter()
                .filter(|(r, _)| memory_change(*r) != Change::No)
                .count();
            (started.elapsed().as_secs_f64() * 1e6 / N as f64, changed)
        };
        let clean = (0..3).map(|_| round()).fold((f64::INFINITY, 0), |best, r| {
            (best.0.min(r.0), best.1.max(r.1))
        });
        for i in 0..N {
            // SAFETY: the first byte of each texture's memory, in the view.
            unsafe { ((view + i * STEP) as *mut u8).write_volatile(0x5a) };
        }
        let written = round();
        eprintln!(
            "the look before an upload, {N} guarded textures: {:.2} us each clean ({} not clean), \
             {:.2} us each written ({} found)",
            clean.0, clean.1, written.0, written.1
        );
        type Release = extern "system" fn(This) -> u32;
        for (r, _) in textures {
            crate::object::slot::<Release>(r, 0x10)(r);
        }
        crate::watch::title_memory_remapped(view, N * STEP);
        let _ = other;
    }

    /// A copy location as a title passes one (D3D12's layout): a resource,
    /// the kind (0 a subresource index, 1 a placed footprint), then the
    /// index or the footprint.
    #[repr(C)]
    struct Location {
        resource: This,
        kind: u32,
        offset: u64,
        format: u32,
        width: u32,
        height: u32,
        depth: u32,
        row_pitch: u32,
    }

    /// A 2×2 RGBA8 block of `texel`s, in a title buffer (rows 256 bytes apart).
    fn block_of(texel: u32) -> crate::testgpu::TitleBuffer {
        let buffer = crate::testgpu::TitleBuffer::new(1 << 12);
        buffer.set_words(|i| if i % 64 < 2 && i < 128 { texel } else { 0 });
        buffer
    }

    /// Records, in `list` as the title does, the copy of `block` (a 2×2
    /// block in a title buffer) to texel `(x, y)` of subresource 0 of `dst`.
    fn copy_block(
        list: &crate::testgpu::TitleList,
        dst: This,
        (x, y): (u32, u32),
        block: &crate::testgpu::TitleBuffer,
    ) {
        let to = Location {
            resource: dst,
            kind: 0,
            offset: 0,
            format: 0,
            width: 0,
            height: 0,
            depth: 0,
            row_pitch: 0,
        };
        let from = Location {
            resource: block.resource(),
            kind: 1,
            offset: 0,
            format: 28,
            width: 2,
            height: 2,
            depth: 1,
            row_pitch: 256,
        };
        type Copy = extern "system" fn(
            This,
            *const Location,
            u32,
            u32,
            u32,
            *const Location,
            *const [u32; 6],
        );
        let this = list.this();
        crate::object::slot::<Copy>(this, 0x80)(this, &to, x, y, 0, &from, std::ptr::null());
    }

    /// The 16 texels of a 4×4 RGBA8 host copy, row by row.
    fn texels_4x4(host: &crate::host::Host, texture: &ID3D12Resource) -> Vec<u32> {
        let read = read_back(host, texture, COPY_DEST);
        (0..16)
            .map(|i| {
                let at = (i / 4) * 256 + (i % 4) * 4;
                u32::from_le_bytes(read[at..at + 4].try_into().unwrap())
            })
            .collect()
    }

    /// A fresh 4×4 RGBA8 texture the title fills with `background`, once
    /// its host copy is resident.
    fn texture_4x4(background: u8) -> (This, &'static ID3D12Resource) {
        let resource = committed(&sampled(4, 4, 28));
        let started = Instant::now();
        while crate::resource::host_resource(resource).is_none() {
            assert!(started.elapsed().as_secs() < 5, "never resident");
            std::thread::sleep(Duration::from_millis(1));
        }
        let (memory, desc) = crate::resource::texture_memory(resource).unwrap();
        let size = crate::resource::texture_layout(&desc).unwrap().size as usize;
        // SAFETY: the texture's own title memory, `size` bytes.
        unsafe { memory.cast_mut().write_bytes(background, size) };
        (resource, crate::resource::host_resource(resource).unwrap())
    }

    /// What native D3D12 holds after copies of 2×2 blocks of the given
    /// texels at the given places, in submission order, over `background`.
    fn expected(background: u32, copies: &[((u32, u32), u32)]) -> Vec<u32> {
        let mut texels = vec![background; 16];
        for &((x, y), texel) in copies {
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                texels[((y + dy) * 4 + x + dx) as usize] = texel;
            }
        }
        texels
    }

    /// A 2D RGBA8 texture of `slices` array slices and `mips` mips, `width`
    /// square, that shaders sample (the layout library's pick).
    fn sampled_chain(width: u64, slices: u16, mips: u16) -> crate::resource::ResourceDesc {
        let desc = sampled(width, width as u32, 28);
        // SAFETY: a description is 56 bytes, 14 words; word 7 holds the
        // array size (low half) and the mip count (high half).
        let mut words: [u32; 14] = unsafe { std::mem::transmute_copy(&desc) };
        words[7] = u32::from(slices) | u32::from(mips) << 16;
        // SAFETY: as above, the same layout back.
        unsafe { std::mem::transmute_copy(&words) }
    }

    /// Data that arrives after a texture's upload in a late mip only (the
    /// smallest, in the mip tail) or in another array slice only is found
    /// by the look before an upload and detiled into that subresource of
    /// the staging buffer. The cases of victory-fork #559 (NickJBc), whose
    /// fingerprint of three windows missed them; here the guards cover
    /// every page of the texture, so no window is needed.
    #[test]
    fn late_mips_and_other_slices_are_found_changed() {
        let test = "texture::tests::late_mips_and_other_slices_are_found_changed";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        if !guards() || verify_guards() || !predication() {
            return;
        }
        let _serial = crate::watch::tests::serial();
        const LEN: usize = 1 << 20;
        // A 256-wide chain of 9 mips (1x1 last), then an array of 6 slices.
        for (case, desc, (mip, slice)) in [
            ("the last mip", sampled_chain(256, 1, 9), (8, 0)),
            ("slice 3 of 6", sampled_chain(64, 6, 1), (0, 3)),
        ] {
            let (view, other) = section_views(LEN);
            let resource = crate::resource::tests_support::at_address(view as u64, &desc);
            let started = Instant::now();
            while crate::resource::host_resource(resource).is_none() {
                assert!(started.elapsed().as_secs() < 5, "never resident");
                std::thread::sleep(Duration::from_millis(1));
            }
            let texture = crate::resource::host_resource(resource).unwrap();
            let staging = staging(resource, texture).unwrap();
            let size = staging.layout.size as usize;
            assert!(size <= LEN, "{case}: {size}");
            // SAFETY: bytes of the title's view, as its CPU writes them.
            unsafe { (view as *mut u8).write_bytes(0x11, size) };
            sample_in(host, resource, 1);
            assert_eq!(memory_change(resource), Change::No, "{case}: clean");
            // The element (0, 0) of that mip and slice only.
            let at = staging.layout.offset(mip, 0, 0, slice).unwrap() as usize;
            assert!(at + 4 <= size, "{case}: {at}");
            // SAFETY: inside the texture's memory, in the view.
            unsafe { ((view + at) as *mut u32).write_unaligned(0x7777_7777) };
            assert_eq!(memory_change(resource), Change::Yes, "{case}: written");
            let before = staging.written.load(Ordering::Acquire);
            sample_in(host, resource, 1);
            assert!(
                staging.written.load(Ordering::Acquire) > before,
                "{case}: detiled"
            );
            // D3D12's subresource order: mips of slice 0, then of slice 1...
            let sub = (slice * staging.mips + mip) as usize;
            let offset = staging.footprints[sub].Offset as usize;
            // SAFETY: the mapped staging buffer; nothing writes it now.
            let texel = unsafe {
                staging
                    .mapped()
                    .unwrap()
                    .add(offset)
                    .cast::<u32>()
                    .read_unaligned()
            };
            assert_eq!(texel, 0x7777_7777, "{case}: subresource {sub}");
            assert_eq!(memory_change(resource), Change::No, "{case}: clean again");
            type Release = extern "system" fn(This) -> u32;
            assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
            crate::watch::title_memory_remapped(view, LEN);
            let _ = other;
        }
    }

    /// Texture writes follow submission order, not recording order
    /// (watch.md item 263; reimplemented after andrewpozdnakov7-jpg's test
    /// on VIctory #1891). A 4×4 RGBA8 texture the title fills with a
    /// background; two lists copy overlapping 2×2 blocks, A at (0, 0) and
    /// B at (1, 1), recorded A, B or B, A and submitted A, B or B, A:
    /// native D3D12 keeps the background where neither copied and the
    /// block submitted last where both did, whatever the recording order.
    /// Then a list that samples the texture (its upload) and one that
    /// copies A into it, in the four orders: sampling writes nothing, so
    /// the texture ends as the background with A over it. Each case on a
    /// texture of its own; the cases that differ from native are listed
    /// together.
    ///
    /// Before `gpu_write` (on WARP, 2026-11-25), 4 of 4 two-copy cases lost
    /// the background (a partial copy did not first bring the title bytes)
    /// and 3 of 4 sample-and-copy cases differed (the copy counted from its
    /// recording: a sampling list recorded after it never uploaded the
    /// background, one recorded before it but submitted after overwrote
    /// the copy). The decisions at submission need predicates.
    #[test]
    fn texture_writes_follow_submission_order() {
        use crate::testgpu::TitleList;
        let test = "texture::tests::texture_writes_follow_submission_order";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        if !predication() {
            return;
        }
        let (background, a, b) = (0x1111_1111u32, 0xaaaa_aaaa, 0xbbbb_bbbb);
        let (block_a, block_b) = (block_of(a), block_of(b));
        let mut wrong = Vec::new();
        let release = |r: This| {
            type Release = extern "system" fn(This) -> u32;
            crate::object::slot::<Release>(r, 0x10)(r);
        };
        // Two copies.
        for (recorded_a_first, submitted_a_first) in
            [(true, true), (false, true), (true, false), (false, false)]
        {
            let (resource, texture) = texture_4x4(0x11);
            let (la, lb) = (TitleList::new(), TitleList::new());
            let mut record = [(&la, (0, 0), &block_a), (&lb, (1, 1), &block_b)];
            if !recorded_a_first {
                record.reverse();
            }
            for (list, at, block) in record {
                copy_block(list, resource, at, block);
            }
            let (first, second) = if submitted_a_first {
                (&la, &lb)
            } else {
                (&lb, &la)
            };
            first.run();
            second.run();
            let submitted = if submitted_a_first {
                [((0, 0), a), ((1, 1), b)]
            } else {
                [((1, 1), b), ((0, 0), a)]
            };
            let got = texels_4x4(host, texture);
            let want = expected(background, &submitted);
            if got != want {
                wrong.push(format!(
                    "copies recorded {} submitted {}: {} of 16 as native, got {got:08x?}",
                    if recorded_a_first { "A, B" } else { "B, A" },
                    if submitted_a_first { "A, B" } else { "B, A" },
                    got.iter().zip(&want).filter(|(g, w)| g == w).count()
                ));
            }
            release(resource);
        }
        // A sampling list and a copying one.
        for (recorded_s_first, submitted_s_first) in
            [(true, true), (false, true), (true, false), (false, false)]
        {
            let (resource, texture) = texture_4x4(0x11);
            let (ls, lc) = (TitleList::new(), TitleList::new());
            let sample = || upload(ls.host(), resource, texture);
            if recorded_s_first {
                sample();
                copy_block(&lc, resource, (0, 0), &block_a);
            } else {
                copy_block(&lc, resource, (0, 0), &block_a);
                sample();
            }
            let (first, second) = if submitted_s_first {
                (&ls, &lc)
            } else {
                (&lc, &ls)
            };
            first.run();
            second.run();
            let got = texels_4x4(host, texture);
            let want = expected(background, &[((0, 0), a)]);
            if got != want {
                wrong.push(format!(
                    "sample and copy recorded {} submitted {}: {} of 16 as native, got {got:08x?}",
                    if recorded_s_first { "S, C" } else { "C, S" },
                    if submitted_s_first { "S, C" } else { "C, S" },
                    got.iter().zip(&want).filter(|(g, w)| g == w).count()
                ));
            }
            release(resource);
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// A pass's first upload makes a texture's staging buffer only within
    /// the room it has: past it none is made, the texture is not taken for
    /// uploaded (its look still finds it due), and it is made once there is
    /// room; one that has its staging buffer needs no room (a full staging
    /// memory leaves it served); one a GPU write was submitted into needs
    /// none at all.
    #[test]
    fn staging_buffers_are_made_within_their_room() {
        let test = "texture::tests::staging_buffers_are_made_within_their_room";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let (resource, texture) = texture_4x4(0x11);
        let Staged::NoRoom(bytes) = stage(resource, texture, 16) else {
            panic!("{:?}", stage(resource, texture, 16));
        };
        assert!(bytes > 16, "{bytes}");
        assert_eq!(staged_bytes(resource), None, "none made");
        assert_eq!(memory_change(resource), Change::Yes, "still due");
        assert_eq!(stage(resource, texture, bytes), Staged::Ready(bytes));
        assert_eq!(staged_bytes(resource), Some(bytes));
        assert_eq!(stage(resource, texture, 0), Staged::Ready(0), "had one");
        let list = host.new_list().unwrap();
        gpu_write(&list, resource, true);
        list.close();
        host.execute(&[&list]);
        host.drain();
        assert_eq!(stage(resource, texture, u64::MAX), Staged::Host);
        assert_eq!(staged_bytes(resource), None);
        type Release = extern "system" fn(This) -> u32;
        assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
    }

    /// A staging buffer goes once no list holds its texture's state (the
    /// lists that copied from it ran and were reset: the fence passed) and
    /// no upload of it was recorded for `idle` sweeps, not before; its
    /// bytes are counted released. Its next upload makes it again, blank:
    /// unchanged memory is neither detiled nor copied (the host copy has
    /// its bytes), a change is detiled and copied as before, and a host
    /// copy not known to hold the bytes gets them detiled and copied.
    #[test]
    fn staging_buffers_go_once_their_last_copy_ran() {
        let test = "texture::tests::staging_buffers_go_once_their_last_copy_ran";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        if !predication() {
            return;
        }
        const IDLE: u64 = 1000;
        let (resource, texture) = texture_4x4(0x11);
        let key = resource as usize;
        // Other tests' textures are in the same map: only this one goes.
        let sweep = || release_staging_of(IDLE, usize::MAX, |r| r == key);
        // Sweeps until it goes: the buffers and bytes released.
        let sweep_out = || {
            let mut released = (0, 0);
            for _ in 0..=IDLE {
                let (n, b) = sweep();
                released = (released.0 + n, released.1 + b);
            }
            released
        };
        let state = || {
            let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
            map.as_ref().unwrap().get(&key).unwrap().clone()
        };
        let id = staging(resource, texture).unwrap().id;
        let bytes = staged_bytes(resource).expect("a staging buffer");
        let (memory, _) = crate::resource::texture_memory(resource).unwrap();
        let size = state().layout.size as usize;

        // Recorded and not run: the list holds it, however long it waits.
        let list = host.new_list().unwrap();
        upload(&list, resource, texture);
        list.close();
        assert_eq!(sweep_out(), (0, 0), "held by a list");
        assert_eq!(staged_bytes(resource), Some(bytes));
        host.execute(&[&list]);
        host.drain();
        drop(list);
        assert_eq!(decided_id(id), [true]);
        // Held by nothing once a list that sampled it lately ran and went,
        // its last upload recorded within `IDLE` sweeps: kept; then
        // released.
        sample_in(host, resource, 1);
        assert_eq!(decided_id(id), [true, false]);
        assert_eq!(sweep(), (0, 0), "recorded lately");
        assert_eq!(sweep_out(), (1, bytes), "released");
        assert_eq!(staged_bytes(resource), None);

        // Unchanged: made again, neither detiled nor copied.
        let written = state().written.load(Ordering::Acquire);
        sample_in(host, resource, 1);
        assert_eq!(staged_bytes(resource), Some(bytes), "made again");
        assert_eq!(decided_id(id), [true, false, false]);
        assert_eq!(state().written.load(Ordering::Acquire), written);
        assert!(state().blank.load(Ordering::Acquire));
        assert!(texels_4x4(host, texture).iter().all(|&t| t == 0x1111_1111));

        // Changed: detiled and copied.
        // SAFETY: the texture's own title memory, `size` bytes.
        unsafe { memory.cast_mut().write_bytes(0x33, size) };
        sample_in(host, resource, 1);
        assert_eq!(decided_id(id), [true, false, false, true]);
        assert!(texels_4x4(host, texture).iter().all(|&t| t == 0x3333_3333));

        // Released again; a host copy not known to hold the last detiling
        // (no copy decided into it) gets it detiled and copied.
        assert_eq!(sweep_out(), (1, bytes), "released again");
        *state().copied.lock().unwrap() = None;
        sample_in(host, resource, 1);
        assert_eq!(decided_id(id), [true, false, false, true, true]);
        assert!(!state().blank.load(Ordering::Acquire));
        assert!(texels_4x4(host, texture).iter().all(|&t| t == 0x3333_3333));
        type Release = extern "system" fn(This) -> u32;
        assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
    }
}
