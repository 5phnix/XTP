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
//! `XTP_TEXTURE_GUARDS=0` hashes at every check, as before: a world frame
//! checked the same textures about a hundred times
//! (`docs/analysis/gpu-frame.md`).
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
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

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
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    buffer: ID3D12Resource,
    /// Its bytes.
    size: u64,
    data: *mut u8,
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

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    set.get_or_insert_with(Default::default)
        .insert(resource as usize);
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
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
/// has not got the staging buffer's bytes yet (decided at submission, see
/// the module). Leaves the texture in the copy destination state.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    #[cfg(feature = "diagnostics")]
    summary_due();
    let host_written = HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&(resource as usize)));
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written || !list.first_use(resource as usize) {
        return;
    }
    let Some(staging) = staging(resource, texture) else {
        return;
    };
    // The host copy `texture` is: what the copy writes (the list holds it
    // through its state below).
    let target = crate::resource::host_copy(resource)
        .filter(|c| c.texture.as_raw() == texture.as_raw())
        .map(|c| Arc::downgrade(&c));
    const COPY_DEST: u32 = 0x400;
    crate::list::ensure_state(list, resource, COPY_DEST);
    // Without a predicate (none left, or predication off) the copy runs.
    let predicate = target.as_ref().and_then(|_| Predicate::take());
    let timed = crate::timestamps::command(list, crate::timestamps::What::TextureUpload, None);
    if let Some(p) = &predicate {
        // SAFETY: an open list; the predicate's buffer lives for the run.
        unsafe {
            list.list
                .SetPredication(&p.of.buffer, p.offset(), D3D12_PREDICATION_OP_EQUAL_ZERO)
        };
    }
    // The staging buffer lives as long as `staging`, which the deferred
    // refresh below holds until the list is reset.
    for (i, footprint) in staging.footprints.iter().enumerate() {
        #[cfg(feature = "diagnostics")]
        {
            let f = footprint.Footprint;
            // SAFETY: live resources.
            let (gpu, d) = unsafe { (staging.buffer.GetGPUVirtualAddress(), texture.GetDesc()) };
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
            pResource: unsafe { std::mem::transmute_copy(&staging.buffer) },
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
    // Then, for this list: whether its copy runs.
    let bytes = staging.size;
    list.defer_host((0, 0), move || {
        let due = target.as_ref().is_none_or(|t| staging.copy_due(t));
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
            .push((Arc::as_ptr(&staging) as usize, runs));
    });
}

/// Each copy decided in tests: its staging buffer and whether it ran.
#[cfg(test)]
static DECIDED: Mutex<Vec<(usize, bool)>> = Mutex::new(Vec::new());

/// Each hash taken in tests: its staging buffer.
#[cfg(test)]
static HASHED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// GPU copies of staging buffers recorded, by whether they ran: count and
/// bytes (staging sizes), since the last summary.
static COPIES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static COPIED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether texture memory in views is guarded, a hash taken only after a
/// write (`XTP_TEXTURE_GUARDS`, on unless `0`).
fn guards() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XTP_TEXTURE_GUARDS").is_none_or(|v| v != "0"))
}

/// Whether a check the guards find clean is hashed too, and a change they
/// missed counted and logged (`XTP_TEXTURE_GUARDS=verify`, diagnostics): a
/// writer that opens no guard.
#[cfg(feature = "diagnostics")]
fn verify_guards() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XTP_TEXTURE_GUARDS").is_some_and(|v| v == "verify"))
}

/// Changes the guards missed (`verify_guards`), since the start.
#[cfg(feature = "diagnostics")]
static MISSED: AtomicU64 = AtomicU64::new(0);

/// Whether texture copies run under a predicate (`XTP_TEXTURE_PREDICATION`,
/// on unless `0`).
fn predication() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("XTP_TEXTURE_PREDICATION").is_none_or(|v| v != "0"))
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
    host_written(dst);
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

    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all). `before_write` runs
    /// just before the staging buffer is overwritten: an earlier
    /// submission's GPU copy may still read it.
    pub(crate) fn refresh(&self, before_write: impl FnOnce()) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let read = self.refresh_now(before_write);
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self, before_write: impl FnOnce()) -> usize {
        let size = self.layout.size as usize;
        let start = self.memory as usize;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
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
        if guards()
            && let Some((now, written)) = crate::watch::writes(start, start + size, true)
        {
            let clean = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            if clean {
                #[cfg(feature = "diagnostics")]
                self.seen.clean.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                let verify = verify_guards();
                #[cfg(not(feature = "diagnostics"))]
                let verify = false;
                if !verify {
                    return 0;
                }
                // Verified: hashed below like an unguarded check, a change
                // counted as missed.
                #[cfg(feature = "diagnostics")]
                if committed(self.memory, size) {
                    // SAFETY: committed memory of the texture's layout size.
                    let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
                    if last.hash.is_some_and(|h| h != blake3::hash(tiled)) {
                        let n = MISSED.fetch_add(1, Ordering::Relaxed) + 1;
                        if n.is_power_of_two() {
                            tracing::warn!(
                                memory = ?self.memory,
                                size,
                                missed = n,
                                "a texture changed that its guards found clean"
                            );
                        }
                    }
                }
            }
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
        // SAFETY: committed memory of the texture's layout size.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let hash = blake3::hash(tiled);
        #[cfg(test)]
        HASHED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(std::ptr::from_ref(self) as usize);
        #[cfg(feature = "diagnostics")]
        {
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            self.seen.hashed.fetch_add(size as u64, Ordering::Relaxed);
        }
        if last.hash == Some(hash) {
            return size;
        }
        last.hash = Some(hash);
        // A title write during the wait is detiled too; the next check sees
        // the memory differ from the hash and detiles it again.
        before_write();
        self.detile(tiled);
        self.written.fetch_add(1, Ordering::Release);
        #[cfg(feature = "diagnostics")]
        {
            crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
            self.seen.changed.fetch_add(1, Ordering::Relaxed);
        }
        size
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

    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        if let Some(convert) = self.convert {
            self.detile_converted(tiled, convert);
            return;
        }
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
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
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
    /// detiled in the title's format, then converted row by row.
    fn detile_converted(&self, tiled: &[u8], convert: Convert) {
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
                        self.data.add(footprint.Offset as usize + line * host_pitch),
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

/// The staging state of a title texture, made on first use.
fn staging(resource: This, texture: &ID3D12Resource) -> Option<Arc<Staging>> {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize)) {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let convert = match crate::resource::format::conversion(desc.format(), d.Format.0 as u32) {
        Some(None) => {
            once!(
                ?desc,
                "texture contents in a format whose stand-in has no conversion not uploaded"
            );
            return None;
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
    let (buffer, data) = host.upload_buffer(total)?;
    crate::host::name(&buffer, || {
        format!(
            "staging of {resource:?} {}x{}x{} f{}",
            d.Width, d.Height, d.DepthOrArraySize, d.Format.0
        )
    });
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    let s = Arc::new(Staging {
        buffer,
        size: total,
        data,
        footprints,
        rows,
        layout,
        convert,
        mips: u32::from(d.MipLevels),
        memory,
        last: Mutex::default(),
        written: AtomicU64::new(0),
        copied: Mutex::new(None),
        #[cfg(feature = "diagnostics")]
        seen: Seen::default(),
    });
    map.insert(resource as usize, s.clone());
    Some(s)
}

/// Every few seconds (diagnostics): per frame, the textures checked, how
/// often, the bytes hashed and the changes, the staging copies recorded and
/// those that ran, and the textures that took most hashing.
#[cfg(feature = "diagnostics")]
fn summary_due() {
    static NEXT: Mutex<Option<(std::time::Instant, u64)>> = Mutex::new(None);
    let frames = {
        let mut next = NEXT.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        let frame = crate::frame::current();
        match *next {
            None => {
                *next = Some((now, frame));
                return;
            }
            Some((at, _)) if now.duration_since(at) < std::time::Duration::from_secs(5) => return,
            Some((_, from)) => {
                *next = Some((now, frame));
                frame.saturating_sub(from).max(1) as f64
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
                    "{}x{}x{}, {} mips, {}-byte elements of {} texels square, {:?}, {:.2} MB in                      title memory",
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
        "texture checks per frame: {} textures checked {:.0} times ({:.0} again in a submission          that had checked them, {:.0} clean by their guards), {:.1} MB hashed, {:.1} changes;          staging copies {:.0} ran ({:.1} MB), {:.0} skipped ({:.1} MB); changes the guards          missed so far {missed} (verified only with XTP_TEXTURE_GUARDS=verify)",
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
    rows.sort_by(|a, b| {
        b.hashed_mb
            .total_cmp(&a.hashed_mb)
            .then(b.checks.total_cmp(&a.checks))
    });
    for r in rows.iter().take(12) {
        tracing::info!(
            "  texture at {:#x} ({}, staging {:.2} MB): {:.1} checks/frame ({:.1} again, {:.1}              clean), {:.1} MB hashed, {:.2} changes",
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
        let key = Arc::as_ptr(staging) as usize;
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
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
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
        unsafe { staging.data.write_bytes(0x22, staging.size as usize) };
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
        assert_eq!(host.log_messages(), None);
        type Release = extern "system" fn(This) -> u32;
        assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
    }

    /// A copy into another host copy is due even though the staging buffer
    /// did not change (the title's resource got a new host copy), and so
    /// is one into the first again after it.
    #[test]
    fn another_host_copy_is_copied_into() {
        let test = "texture::tests::another_host_copy_is_copied_into";
        if crate::testgpu::device(test).is_none() {
            return;
        }
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
        let key = Arc::as_ptr(staging) as usize;
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
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        if !guards() || !predication() {
            return;
        }
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
        assert_eq!(host.log_messages(), None);

        type Release = extern "system" fn(This) -> u32;
        assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
        // The views stay mapped until the tests end: the watch's journal
        // keeps the pages these checks found written, which a test that
        // allocates fresh memory at the same address would read as its own.
        let _ = other;
    }

    /// A list named twice in one submission runs its copy in both runs if
    /// the first needs it: both read its one predicate word.
    #[test]
    fn a_list_named_twice_in_a_submission_copies() {
        let test = "texture::tests::a_list_named_twice_in_a_submission_copies";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
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
        type Release = extern "system" fn(This) -> u32;
        assert_eq!(crate::object::slot::<Release>(resource, 0x10)(resource), 0);
    }
}
