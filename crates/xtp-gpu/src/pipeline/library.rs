//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with are listed by a key
//!    known when the title creates them (their shader containers and
//!    graphics state); the next run hands those to the workers at
//!    creation, during loading.
//! 3. Beside them, the whole descriptions of the graphics and mesh
//!    pipelines the title creates ([`super::record`]), from which `xtp
//!    precook` makes them into the library: those of the whole game after
//!    one session, and again after a translator change.
//!
//! Loads and stores of different pipelines run side by side: the library
//! is thread-safe and synchronises itself, but for two loads of the same
//! pipeline at once, which "may modify the state of that pipeline within
//! the library in a non-thread-safe manner" (D3D12's remarks on
//! `CreatePipelineLibrary`): one pipeline's name is used by one thread at a
//! time ([`NAMES`]). Serialising takes the library alone, so that its size
//! cannot change between asking for it and writing. With every call
//! serialised, the workers made some 35 pipelines a second, most of them
//! loads (a Series S session of 2026-11-25, `20261125-022031`: 1,465
//! loaded and 607 made, some 400 ms each on 14 workers), and its world
//! dropped some 900 draws a frame "being made" for a minute.
//!
//! - A save counts as done once its file is written: the pipelines it
//!   holds stop counting as added then, not before, so that a save that
//!   fails is made again by the next one.
//! - Generations: a library cannot drop an entry, and every translator
//!   change adds a name for each pipeline whose code changed, beside the
//!   old one. `xtp precook` compacts oversized files ([`rebuild`]): the
//!   library starts empty, what it asks for is taken from the generation
//!   before when that has it (loaded there, stored here) and made
//!   otherwise, and the save replaces the file with the new generation
//!   alone. What only sessions asked for is made again when one asks.
//! - The names the library holds: D3D12 cannot list them, so every save
//!   writes them beside the file ([`NAMES_FILE`]), with the file's size and
//!   BLAKE3, which the next process checks before it trusts them. A library
//!   read without them (an older file, or one saved by a process that died
//!   before writing them) learns them as they load.
//! - The precook does not load a pipeline the library is known to hold
//!   ([`without_loading`]): its work is to have each pipeline in the file,
//!   and a session loads it when it needs it. That round trip was the whole
//!   cost of an unchanged pipeline after a translator change.
//! - A name the library holds but whose entry does not load for its
//!   description (StorePipeline refuses a name it holds, so before this the
//!   pipeline was compiled again in every session) gets the pipeline under
//!   an alternate name ([`SLOTS`]), which the next session loads.
//! - Such a name is remembered as broken in the names file: the precook
//!   answers a pipeline by its names only when one of them is held and not
//!   known to fail, and otherwise makes it (under the next name), so that
//!   a pipeline a load refused is not counted ready again. What no process
//!   has loaded yet cannot be known to fail without a load: the session
//!   that first finds such an entry makes the pipeline beside it and says
//!   so in the names file, and the precooks after it count it no more.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HSTRING, Interface, PCWSTR};

static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keeps the pipeline library and the early list in `dir` from now on.
pub fn use_pipeline_cache(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// The folder the pipeline cache is kept in, once set.
pub(crate) fn dir() -> Option<&'static std::path::Path> {
    DIR.get().map(PathBuf::as_path)
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);
/// Host pipelines the precook found in the library's names and did not
/// load ([`without_loading`]), since the start.
pub(crate) static KNOWN: AtomicU64 = AtomicU64::new(0);
/// Microseconds pipelines waited for the library (their name, or a save),
/// and spent loading from it, since the start (the library's line).
static LOCK_US: AtomicU64 = AtomicU64::new(0);
static LOAD_US: AtomicU64 = AtomicU64::new(0);

struct Library {
    library: ID3D12PipelineLibrary1,
    /// The blob the library was made from: it must outlive the library.
    _blob: Vec<u8>,
    /// Changes since the last save, including replacing a generation.
    added: AtomicU64,
    /// While a new generation is made ([`rebuild`]): the one before, read
    /// only.
    previous: Option<Generation>,
    /// The names this library holds, as far as this process knows.
    names: RwLock<Names>,
    /// Names learned since the names file was last written.
    names_changed: AtomicBool,
    /// The library file this library holds the same pipelines as: the one
    /// it was read from (`_blob`, accepted: hashed when first needed) or
    /// the one last saved. `None` with `_blob` not the file's (empty, or
    /// rejected by the host).
    file: Mutex<Option<FileId>>,
    blob_is_file: bool,
}

/// A generation of the library read only, the blob it was made from, and
/// the names it holds if its names file was its own.
struct Generation {
    library: ID3D12PipelineLibrary1,
    _blob: Vec<u8>,
    names: Option<Names>,
}

impl Library {
    /// A library of `blob` (read from the file when `blob_is_file`), with
    /// the names known to be in it.
    fn new(
        library: ID3D12PipelineLibrary1,
        blob: Vec<u8>,
        names: Names,
        file: Option<FileId>,
        blob_is_file: bool,
        previous: Option<Generation>,
    ) -> Library {
        Library {
            library,
            _blob: blob,
            added: AtomicU64::new(0),
            previous,
            names: RwLock::new(names),
            names_changed: AtomicBool::new(false),
            file: Mutex::new(file),
            blob_is_file,
        }
    }

    fn names(&self) -> std::sync::RwLockReadGuard<'_, Names> {
        self.names.read().unwrap_or_else(|e| e.into_inner())
    }

    /// `name` is held: stored by this process, refused by StorePipeline as
    /// one the library holds, or loaded.
    fn hold(&self, name: &str) {
        let mut names = self.names.write().unwrap_or_else(|e| e.into_inner());
        if names.held.insert(name.to_owned()) {
            self.names_changed.store(true, Ordering::Relaxed);
        }
    }

    /// A load of `name` for the description of that name loaded (`ok`), or
    /// failed: the name is held and mended, or marked broken if it is known
    /// to be held (a load also fails for a name the library does not hold,
    /// which a library read without its names may not know of).
    fn loaded(&self, name: &str, ok: bool) {
        let mut names = self.names.write().unwrap_or_else(|e| e.into_inner());
        let changed = if ok {
            let held = names.held.insert(name.to_owned());
            names.broken.remove(name) | held
        } else {
            names.held.contains(name) && names.broken.insert(name.to_owned())
        };
        if changed {
            self.names_changed.store(true, Ordering::Relaxed);
        }
    }

    /// Stores `pso` under the first of `slots` (its names, [`slot`]) that
    /// does not hold an entry this pipeline's description fails to load
    /// from: `failed` says the slots whose load failed here. A slot the
    /// library holds that was not tried holds this pipeline already (stored
    /// meanwhile by another thread: a name is its description's hash), and
    /// nothing more is stored; one that was tried and failed holds another
    /// entry, and the next slot is tried. A name StorePipeline refuses is
    /// held, by the same rules.
    fn store(
        &self,
        slots: &[(String, HSTRING)],
        failed: &[bool; SLOTS],
        pso: &ID3D12PipelineState,
    ) {
        for ((name, wide), &failed) in slots.iter().zip(failed) {
            if self.names().held.contains(name) {
                if failed {
                    continue;
                }
                return;
            }
            // SAFETY: a live pipeline; a name already stored is refused,
            // which leaves the library as it was.
            match unsafe { self.library.StorePipeline(PCWSTR(wide.as_ptr()), pso) } {
                Ok(()) => {
                    self.added.fetch_add(1, Ordering::Relaxed);
                    self.hold(name);
                    return;
                }
                Err(e) if e.code() == windows::Win32::Foundation::E_INVALIDARG => {
                    self.hold(name);
                    if !failed {
                        return;
                    }
                    // Held, and its load failed here: another entry.
                    self.loaded(name, false);
                }
                Err(e) => {
                    tracing::debug!(name, error = %e, "host pipeline not stored");
                    return;
                }
            }
        }
        tracing::debug!(
            name = slots[0].0,
            "host pipeline not stored: each of its names is held"
        );
    }
}

/// A library file: its size and BLAKE3, which say whether a names file is
/// that file's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    bytes: u64,
    blake3: [u8; 32],
}

impl FileId {
    fn of(bytes: &[u8]) -> FileId {
        FileId {
            bytes: bytes.len() as u64,
            blake3: *blake3::hash(bytes).as_bytes(),
        }
    }
}

/// The names a library holds, as far as a process knows them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Names {
    held: HashSet<String>,
    /// Of `held`, the names whose entry did not load for the description
    /// of that name when a process last loaded it (another description's
    /// entry, or one the driver refuses).
    broken: HashSet<String>,
    /// `held` is every name the library holds: it started empty, or its
    /// names file said so. Otherwise a name not in it may still be there.
    complete: bool,
}

/// The names file beside the library, and its first line. Version 2 marks
/// broken names ([`Names::broken`]); version 1, which did not, is read too.
const NAMES_FILE: &str = "host-pipelines.names";
const NAMES_FORMAT: &str = "xtp pipeline names 2";
const NAMES_FORMAT_1: &str = "xtp pipeline names 1";
/// After a broken name in the names file.
const BROKEN: &str = " broken";

impl Names {
    /// The names of a library that holds none.
    fn empty() -> Names {
        Names {
            complete: true,
            ..Names::default()
        }
    }

    /// The names of a library read without its names file: none known.
    fn unknown() -> Names {
        Names::default()
    }

    /// Whether the library holds some name of the pipeline named `name`
    /// that is not known to fail to load ([`Names::broken`]): what the
    /// precook answers without loading ([`without_loading`]).
    fn ready_any(&self, name: &str) -> bool {
        (0..SLOTS)
            .map(|k| slot(name, k))
            .any(|n| self.held.contains(&n) && !self.broken.contains(&n))
    }

    /// The names file of a library file `file` holding these names.
    fn text(&self, file: &FileId) -> String {
        let mut held: Vec<&str> = self.held.iter().map(String::as_str).collect();
        held.sort_unstable();
        let mut text = format!(
            "{NAMES_FORMAT}\nlibrary {} {} {}\n",
            file.bytes,
            blake3::Hash::from(file.blake3).to_hex(),
            if self.complete { "complete" } else { "partial" }
        );
        for name in held {
            text.push_str(name);
            if self.broken.contains(name) {
                text.push_str(BROKEN);
            }
            text.push('\n');
        }
        text
    }

    /// The names a names file reading `text` gives the library file of
    /// `blob`; `None` if it is another file's (or not a names file).
    fn read(text: &str, blob: &[u8]) -> Option<(Names, FileId)> {
        let mut lines = text.lines();
        let marks = match lines.next()? {
            NAMES_FORMAT => true,
            NAMES_FORMAT_1 => false,
            _ => return None,
        };
        let mut head = lines.next()?.strip_prefix("library ")?.split(' ');
        let bytes: u64 = head.next()?.parse().ok()?;
        let hash = head.next()?;
        let complete = match head.next()? {
            "complete" => true,
            "partial" => false,
            _ => return None,
        };
        if head.next().is_some() || bytes != blob.len() as u64 {
            return None;
        }
        // Hashed only once the size agrees: some 0.5 s per GiB.
        let file = FileId::of(blob);
        (blake3::Hash::from(file.blake3).to_hex().as_str() == hash).then_some(())?;
        let mut names = Names {
            complete,
            ..Names::default()
        };
        for line in lines {
            let (name, broken) = match line.strip_suffix(BROKEN) {
                Some(name) if marks => (name, true),
                _ => (line, false),
            };
            is_name(name).then_some(())?;
            if broken {
                names.broken.insert(name.to_owned());
            }
            names.held.insert(name.to_owned());
        }
        Some((names, file))
    }
}

/// Whether `text` is a pipeline's name in the library ([`name`]) or one of
/// its alternates ([`slot`]).
fn is_name(text: &str) -> bool {
    let (name, k) = match text.split_once('~') {
        Some((name, k)) => (
            name,
            k.parse::<usize>().ok().filter(|k| (1..SLOTS).contains(k)),
        ),
        None => (text, Some(0)),
    };
    k.is_some() && name.len() == 32 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The names a pipeline may be stored under: its own, then alternates for
/// when the entry of its own name does not load for its description.
const SLOTS: usize = 4;

/// The `k`th name of the pipeline named `name` (`0`: its own).
fn slot(name: &str, k: usize) -> String {
    if k == 0 {
        name.to_owned()
    } else {
        format!("{name}~{k}")
    }
}

thread_local! {
    /// Set by [`without_loading`] on the precook's threads.
    static SKIP: Cell<bool> = const { Cell::new(false) };
}

/// A pipeline of this process, answered for a pipeline the library holds
/// that [`without_loading`] does not load ([`found_in`] keeps the first it
/// makes or loads).
struct StandIn(ID3D12PipelineState);
// SAFETY: D3D12 objects are free-threaded, and this one is only held.
unsafe impl Send for StandIn {}
// SAFETY: as above.
unsafe impl Sync for StandIn {}
static STAND_IN: OnceLock<StandIn> = OnceLock::new();

/// Runs `make` (a host pipeline's creation, `HostPipeline::new`) with every
/// pipeline the library's names hold answered without loading it: the
/// pipeline `make` gets is then another one of this process (the first it
/// made or loaded), so only the precook calls this, as it keeps nothing it
/// makes. A pipeline the names do not hold is loaded or made as always.
/// The precook's work is to have each pipeline in the library file; a
/// session loads it, by its name, when it needs it.
pub(crate) fn without_loading<R>(make: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            SKIP.set(self.0);
        }
    }
    let _restore = Restore(SKIP.replace(true));
    make()
}

// SAFETY: D3D12 objects are free-threaded. The library synchronises its
// calls itself but for loads of one pipeline, which the name locks
// ([`NAMES`]) keep apart, and serialising is made with the library's write
// lock held, alone.
unsafe impl Send for Library {}
// SAFETY: as above.
unsafe impl Sync for Library {}

/// Locks by a pipeline's name: one thread at a time loads or stores a
/// pipeline of a name (one of these locks: a few names share one).
static NAMES: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];

/// The name lock of `name`.
fn name_lock(name: &str) -> &'static Mutex<()> {
    let h = name.bytes().fold(0usize, |h, b| {
        h.wrapping_mul(31).wrapping_add(usize::from(b))
    });
    &NAMES[h % NAMES.len()]
}

const LIBRARY_FILE: &str = "host-pipelines.bin";
const EARLY_FILE: &str = "early-pipelines.txt";
/// Title pipelines listed for an early start, at most.
const EARLY_MAX: usize = 8192;

/// The library is opened on first use: once opened, [`rebuild`] comes too
/// late.
static OPENED: AtomicBool = AtomicBool::new(false);
static REBUILD: AtomicBool = AtomicBool::new(false);

fn library() -> Option<&'static RwLock<Library>> {
    static LIBRARY: OnceLock<Option<RwLock<Library>>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            OPENED.store(true, Ordering::SeqCst);
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let read = |file| DIR.get().and_then(|d| std::fs::read(d.join(file)).ok());
            let blob = read(LIBRARY_FILE).unwrap_or_default();
            let names = read(NAMES_FILE).and_then(|t| String::from_utf8(t).ok());
            let library = if REBUILD.load(Ordering::SeqCst) {
                new_generation(&device, blob, names.as_deref())
            } else {
                open(&device, blob, names.as_deref())
            };
            if library.is_none() {
                tracing::warn!("no host pipeline library: pipelines are not kept");
            }
            library.map(RwLock::new)
        })
        .as_ref()
}

/// Makes the library a new generation, the file's library its generation
/// before (see the module): asked before the library's first use (`xtp
/// precook`'s start), else too late, which it says.
pub(crate) fn rebuild() -> bool {
    REBUILD.store(true, Ordering::SeqCst);
    !OPENED.load(Ordering::SeqCst)
}

/// The library file's size, if there is one.
pub(crate) fn file_bytes() -> Option<u64> {
    let dir = DIR.get()?;
    std::fs::metadata(dir.join(LIBRARY_FILE))
        .ok()
        .map(|m| m.len())
}

/// The host's library of `bytes`, which must outlive it.
fn make(device: &ID3D12Device1, bytes: &[u8]) -> windows::core::Result<ID3D12PipelineLibrary1> {
    // SAFETY: the callers keep the bytes beside the library.
    unsafe {
        device
            .CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)?
            .cast()
    }
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage), with the names `names` (the names
/// file's text) says it holds if they are that blob's.
fn open(device: &ID3D12Device1, blob: Vec<u8>, names: Option<&str>) -> Option<Library> {
    if !blob.is_empty() {
        match make(device, &blob) {
            Ok(library) => {
                let read = names.and_then(|text| Names::read(text, &blob));
                tracing::info!(
                    bytes = blob.len(),
                    names = read.as_ref().map(|(n, _)| n.held.len()),
                    "host pipeline library loaded"
                );
                let (names, file) = match read {
                    Some((names, file)) => (names, Some(file)),
                    None => (Names::unknown(), None),
                };
                return Some(Library::new(library, blob, names, file, true, None));
            }
            Err(e) => {
                tracing::info!(error = %e, "host pipeline library rejected; starting an empty one")
            }
        }
    } else {
        tracing::info!("no host pipeline library yet; starting an empty one");
    }
    let library = make(device, &[]).ok()?;
    Some(Library::new(
        library,
        Vec::new(),
        Names::empty(),
        None,
        false,
        None,
    ))
}

/// An empty library, the one in `blob` its generation before (none if the
/// host rejects it), with the names `names` says it holds.
fn new_generation(device: &ID3D12Device1, blob: Vec<u8>, names: Option<&str>) -> Option<Library> {
    let previous = if blob.is_empty() {
        None
    } else {
        match make(device, &blob) {
            Ok(library) => Some(Generation {
                names: names
                    .and_then(|text| Names::read(text, &blob))
                    .map(|(n, _)| n),
                library,
                _blob: blob,
            }),
            Err(e) => {
                tracing::info!(error = %e, "host pipeline library rejected: the new generation starts from nothing");
                None
            }
        }
    };
    tracing::info!(
        before = previous.as_ref().map_or(0, |g| g._blob.len()),
        "host pipeline library: a new generation, from the one before"
    );
    let library = make(device, &[]).ok()?;
    let library = Library::new(library, Vec::new(), Names::empty(), None, false, previous);
    // Even a generation with no surviving pipelines replaces its file.
    library.added.store(1, Ordering::Relaxed);
    Some(library)
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let mut h = blake3::Hasher::new();
    for part in [kind.as_bytes(), root]
        .into_iter()
        .chain(code.iter().copied())
        .chain([state.as_bytes()])
    {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex()[..32].to_owned()
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored.
pub(crate) fn pipeline(
    name: &str,
    load: impl Fn(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    let Some(library) = library() else {
        MADE.fetch_add(1, Ordering::Relaxed);
        return create();
    };
    found_in(library, name, load, create).map(|(pso, _)| pso)
}

/// Where a pipeline came from ([`found_in`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Library,
    /// The generation before (stored into the library).
    Previous,
    Made,
    /// Held by the library and not loaded ([`without_loading`]): the
    /// pipeline answered is a stand-in.
    Known,
}

/// [`pipeline`] in `library`, and where it came from.
///
/// A pipeline is looked for under each of its names ([`slot`]) the library
/// is known to hold, and under its own name when the library's names are
/// not all known. Made, it is stored under the first name the library does
/// not hold: an entry of its name that does not load for this description
/// keeps its name, and the pipeline goes beside it, where the next session
/// finds it.
fn found_in(
    library: &RwLock<Library>,
    name: &str,
    load: impl Fn(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<(ID3D12PipelineState, Source)> {
    if SKIP.get()
        && let Some(stand_in) = STAND_IN.get()
        && library
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .names()
            .ready_any(name)
    {
        KNOWN.fetch_add(1, Ordering::Relaxed);
        return Ok((stand_in.0.clone(), Source::Known));
    }
    let slots: Vec<(String, HSTRING)> = (0..SLOTS)
        .map(|k| {
            let name = slot(name, k);
            let wide = HSTRING::from(&name);
            (name, wide)
        })
        .collect();
    let since = |t: std::time::Instant| t.elapsed().as_micros() as u64;
    // The name's lock, then the library's (a save takes it alone).
    let take = || {
        let asked = std::time::Instant::now();
        let held = name_lock(name).lock().unwrap_or_else(|e| e.into_inner());
        let l = library.read().unwrap_or_else(|e| e.into_inner());
        LOCK_US.fetch_add(since(asked), Ordering::Relaxed);
        (held, l)
    };
    // The slots worth a load in a library of `names`: those it holds, or
    // all of them when `names` may miss some (a library read without its
    // names file: a load that finds no entry costs little).
    let tried = |names: Option<&Names>| -> Vec<usize> {
        (0..SLOTS)
            .filter(|&k| names.is_none_or(|n| !n.complete || n.held.contains(&slots[k].0)))
            .collect()
    };
    // The slots of this library whose load failed: their entries are other
    // descriptions', and a store goes past them ([`Library::store`]).
    let mut failed = [false; SLOTS];
    let loaded = {
        let (_name, l) = take();
        let started = std::time::Instant::now();
        let mut loaded = None;
        let ours = tried(Some(&l.names()));
        for k in ours {
            match load(&l.library, PCWSTR(slots[k].1.as_ptr())) {
                Ok(pso) => {
                    // Learned, when the names were not all known, and
                    // mended if a load failed before.
                    l.loaded(&slots[k].0, true);
                    loaded = Some((pso, Source::Library));
                    break;
                }
                Err(_) => {
                    failed[k] = true;
                    l.loaded(&slots[k].0, false);
                }
            }
        }
        if loaded.is_none()
            && let Some(previous) = &l.previous
        {
            for k in tried(previous.names.as_ref()) {
                if let Ok(pso) = load(&previous.library, PCWSTR(slots[k].1.as_ptr())) {
                    l.store(&slots, &failed, &pso);
                    loaded = Some((pso, Source::Previous));
                    break;
                }
            }
        }
        LOAD_US.fetch_add(since(started), Ordering::Relaxed);
        loaded
    };
    if let Some(found) = loaded {
        LOADED.fetch_add(1, Ordering::Relaxed);
        keep_stand_in(&found.0);
        return Ok(found);
    }
    // Compiled outside the locks: compiles take up to seconds.
    let pso = create()?;
    MADE.fetch_add(1, Ordering::Relaxed);
    let (_name, l) = take();
    l.store(&slots, &failed, &pso);
    keep_stand_in(&pso);
    Ok((pso, Source::Made))
}

/// For tests: whether `pso` is the stand-in [`without_loading`] answers
/// for a pipeline the library's names hold.
#[cfg(test)]
pub(crate) fn is_stand_in(pso: &ID3D12PipelineState) -> bool {
    STAND_IN.get().is_some_and(|s| s.0 == *pso)
}

/// For tests: whether this process has a pipeline library (none: every
/// pipeline is made, and [`without_loading`] answers none by its name).
#[cfg(test)]
pub(crate) fn has_library() -> bool {
    library().is_some()
}

/// Keeps `pso` as the stand-in [`without_loading`] answers, if none is.
fn keep_stand_in(pso: &ID3D12PipelineState) {
    if STAND_IN.get().is_none() {
        let _ = STAND_IN.set(StandIn(pso.clone()));
    }
}

/// What a save did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Saved {
    /// Nothing to write (nothing added, the early list as it was), or not
    /// yet time to, or no library or cache folder.
    Nothing,
    Written,
    /// The reason; what was to be written still is.
    Failed(String),
}

/// Writes the library if pipelines were added since the last save, and the
/// early list with it or if it changed, once `SAVE_EVERY` has passed (or
/// `now`). The library is serialised only when it holds something new: it
/// takes the lock every load waits for, and the cache's library had grown
/// to 2.3 GB (2026-11-25), which a session wrote again every 20 s while it
/// drew new pipelines that the library already held.
///
/// One save at a time ([`write_library`]): a worker's gives way to one
/// under way, one asked `now` (at the end of a run or of a precook) waits
/// for it.
pub(crate) fn save(now: bool) -> Saved {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
    let Some(dir) = DIR.get() else {
        return Saved::Nothing;
    };
    let Some(library) = library() else {
        return if REBUILD.load(Ordering::SeqCst) {
            Saved::Failed("the host could not create the replacement pipeline library".into())
        } else {
            Saved::Nothing
        };
    };
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if !now && last.is_some_and(|t| t.elapsed() < SAVE_EVERY) {
            return Saved::Nothing;
        }
        *last = Some(std::time::Instant::now());
    }
    let saved = write_library(library, dir, now, |bytes| {
        // Under the save's lock: whether the list changed since the last
        // save that wrote it.
        let used_changed = USED_CHANGED.swap(false, Ordering::Relaxed);
        if bytes.is_none() && !used_changed {
            return Ok(false);
        }
        let used: Vec<String> = USED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flatten()
            .cloned()
            .collect();
        if let Err(e) = write_atomically(&dir.join(EARLY_FILE), used.join("\n").as_bytes()) {
            if used_changed {
                USED_CHANGED.store(true, Ordering::Relaxed);
            }
            return Err(e);
        }
        let (created, unkept, distinct) = super::record::created_counts();
        tracing::info!(
            library = bytes.is_some(),
            bytes = bytes.unwrap_or(0),
            early = used.len(),
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            known = KNOWN.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            created,
            created_unkept = unkept,
            created_distinct = distinct,
            recorded = super::record::kept(),
            lock_wait_ms = LOCK_US.load(Ordering::Relaxed) / 1000,
            load_ms = LOAD_US.load(Ordering::Relaxed) / 1000,
            "host pipeline library saved"
        );
        Ok(true)
    });
    if let Saved::Failed(e) = &saved {
        tracing::warn!(error = %e, "host pipeline library not saved");
    }
    saved
}

/// Writes `library` into `dir` if pipelines were added to it, then runs
/// `then` with its size (`None`: none written), which says whether it
/// wrote something too. The pipelines the library holds stop counting as
/// added once both are done, not before.
///
/// One at a time, from what it serialises to that count: two at once would
/// both write what was added before them, then both stop counting it (the
/// count would wrap), and share the temporary files. With `wait`, it waits
/// for one under way; without, it gives way to it ([`Saved::Nothing`]).
fn write_library(
    library: &RwLock<Library>,
    dir: &std::path::Path,
    wait: bool,
    then: impl FnOnce(Option<usize>) -> std::io::Result<bool>,
) -> Saved {
    static SAVING: Mutex<()> = Mutex::new(());
    let _saving = if wait {
        SAVING.lock().unwrap_or_else(|e| e.into_inner())
    } else {
        match SAVING.try_lock() {
            Ok(saving) => saving,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Saved::Nothing,
        }
    };
    let path = dir.join(LIBRARY_FILE);
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let serialised = (|| -> std::io::Result<_> {
        std::fs::create_dir_all(dir)?;
        let (mapped, added, names) = {
            // Alone: no store between the size and the bytes, nor between
            // the bytes and the names they hold.
            let l = library.write().unwrap_or_else(|e| e.into_inner());
            let added = l.added.load(Ordering::Relaxed);
            if added == 0 {
                // The file holds what the library holds: the names learned
                // since (loads of a library read without them) are its.
                let names = l
                    .names_changed
                    .swap(false, Ordering::Relaxed)
                    .then(|| l.names().clone());
                drop(l);
                if let Some(names) = names {
                    write_names(library, dir, &names, None);
                }
                return Ok(None);
            }
            l.names_changed.store(false, Ordering::Relaxed);
            let names = l.names().clone();
            (LibraryFile::serialize(&tmp, &l.library)?, added, names)
        };
        let size = mapped.size;
        mapped.flush()?;
        // SAFETY: the view holds `size` bytes the library wrote, and lives
        // until `mapped` is dropped below.
        let file =
            FileId::of(unsafe { std::slice::from_raw_parts(mapped.view.Value.cast(), size) });
        drop(mapped);
        std::fs::rename(&tmp, &path)?;
        write_names(library, dir, &names, Some(file));
        Ok(Some((size, added)))
    })();
    let serialised = match serialised {
        Ok(result) => result,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Saved::Failed(e.to_string());
        }
    };
    let written = then(serialised.map(|(size, _)| size));
    match (written, serialised) {
        (Err(e), _) => Saved::Failed(e.to_string()),
        (Ok(_), Some((_, added))) => {
            // Those stored meanwhile still count, for the next save.
            let l = library.read().unwrap_or_else(|e| e.into_inner());
            l.added.fetch_sub(added, Ordering::Relaxed);
            Saved::Written
        }
        (Ok(true), None) => Saved::Written,
        (Ok(false), None) => Saved::Nothing,
    }
}

/// Writes the names file of the library file in `dir`: `names`, held by the
/// file just written (`file`, its identity), or by the file the library was
/// read from or last saved to (`None`). The names stay to write again if it
/// fails, or if that file is not known (a library that did not read back),
/// and a file whose names are not written is read without them: what the
/// names file says is always its library file's.
fn write_names(
    library: &RwLock<Library>,
    dir: &std::path::Path,
    names: &Names,
    file: Option<FileId>,
) {
    let l = library.read().unwrap_or_else(|e| e.into_inner());
    let file = {
        let mut known = l.file.lock().unwrap_or_else(|e| e.into_inner());
        if file.is_some() {
            *known = file;
        } else if known.is_none() && l.blob_is_file {
            *known = Some(FileId::of(&l._blob));
        }
        *known
    };
    let written =
        file.map(|file| write_atomically(&dir.join(NAMES_FILE), names.text(&file).as_bytes()));
    match written {
        Some(Ok(())) => {}
        Some(Err(e)) => {
            tracing::warn!(error = %e, "host pipeline names not written");
            l.names_changed.store(true, Ordering::Relaxed);
        }
        None => l.names_changed.store(true, Ordering::Relaxed),
    }
}

/// D3D12 needs one contiguous output buffer. A writable view of the
/// temporary file avoids a second library-sized allocation in the heap.
struct LibraryFile {
    file: std::fs::File,
    view: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    size: usize,
}

impl LibraryFile {
    fn serialize(
        path: &std::path::Path,
        library: &ID3D12PipelineLibrary1,
    ) -> std::io::Result<Self> {
        use std::os::windows::{
            fs::OpenOptionsExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        };
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
        };

        // SAFETY: a live library under its exclusive lock.
        let size = unsafe { library.GetSerializedSize() };
        if size > isize::MAX as usize {
            return Err(std::io::Error::other(
                "pipeline library is too large to map",
            ));
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(path)?;
        file.set_len(size as u64)?;
        // SAFETY: an unnamed mapping of this file, extended to exactly size.
        let section = unsafe {
            CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                0,
                std::ptr::null(),
            )
        };
        if section.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the newly created handle is ours; the view retains its
        // section independently once this handle closes.
        let section = unsafe { OwnedHandle::from_raw_handle(section) };
        // SAFETY: the section covers the entire file and permits writes.
        let view = unsafe { MapViewOfFile(section.as_raw_handle(), FILE_MAP_WRITE, 0, 0, size) };
        if view.Value.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mapped = Self { file, view, size };
        // SAFETY: the exclusively opened file has one writable view of size bytes.
        unsafe {
            let bytes = std::slice::from_raw_parts_mut(view.Value.cast(), size);
            library.Serialize(bytes).map_err(std::io::Error::other)?;
        }
        Ok(mapped)
    }

    fn flush(&self) -> std::io::Result<()> {
        use windows_sys::Win32::System::Memory::FlushViewOfFile;
        // SAFETY: this view and its file stay open through the flush.
        if unsafe { FlushViewOfFile(self.view.Value, self.size) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.file.sync_all()
    }
}

impl Drop for LibraryFile {
    fn drop(&mut self) {
        // SAFETY: this object owns the sole view and unmaps it exactly once.
        unsafe { windows_sys::Win32::System::Memory::UnmapViewOfFile(self.view) };
    }
}

/// Writes `bytes` to a temporary file beside `path`, then renames it over
/// `path`: a reader never sees a part. One save at a time uses it
/// ([`save`]).
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Title pipeline keys drawn or dispatched with (this run and the last).
static USED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

fn used() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    if used.is_none() {
        let listed = DIR
            .get()
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let set: HashSet<String> = listed
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        tracing::info!(pipelines = set.len(), "title pipelines to start early");
        *used = Some(set);
    }
    used
}

/// Whether the title pipeline with `key` was drawn with in an earlier run.
pub(crate) fn wanted_early(key: &str) -> bool {
    used().as_ref().is_some_and(|s| s.contains(key))
}

/// The title pipeline with `key` was drawn or dispatched with.
pub(crate) fn note_used(key: &str) {
    let mut used = used();
    let set = used.get_or_insert_with(HashSet::new);
    if set.len() < EARLY_MAX && set.insert(key.to_owned()) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        assert_eq!(a, name("graphics", b"root", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let test = "pipeline::library::tests::a_rejected_blob_gives_an_empty_library";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let device = gpu.device.cast::<ID3D12Device1>().expect("ID3D12Device1");
        // The layer reports the blob it cannot read; the library falls back.
        let library = crate::testgpu::expect_layer_errors(|| {
            open(&device, b"not a pipeline library".repeat(16), None).expect("an empty library")
        });
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.library.GetSerializedSize() } > 0);
        assert!(library._blob.is_empty());
    }

    /// Host pipelines of a few names, each asked for by several threads at
    /// once: every ask gets its pipeline (loads of one name kept apart,
    /// stores beside loads), most from the library, which still serialises
    /// after.
    #[test]
    fn pipelines_are_loaded_and_stored_side_by_side() {
        let test = "pipeline::library::tests::pipelines_are_loaded_and_stored_side_by_side";
        let Some(_gpu) = crate::testgpu::device(test) else {
            return;
        };
        if library().is_none() {
            return;
        }
        let shaders: Vec<_> = [1, 2, 4, 8]
            .map(|n| {
                super::super::precook::tests::translated(
                    "compute",
                    xtp_shader::HwStage::Cs,
                    Some(&format!("[numthreads({n}, 1, 1)] void main() {{}}")),
                    windows::core::s!("cs_5_1"),
                    format!("library test: compute {n}").as_bytes(),
                )
            })
            .into();
        let loaded = LOADED.load(Ordering::Relaxed);
        std::thread::scope(|s| {
            for t in 0..8 {
                let shaders = &shaders;
                s.spawn(move || {
                    for k in 0..12 {
                        let shader = &shaders[(t + k) % shaders.len()];
                        let made = super::super::host::HostPipeline::new(
                            std::slice::from_ref(shader),
                            None,
                        );
                        assert!(made.is_some(), "a pipeline for {shader:?}");
                    }
                });
            }
        });
        // 96 asks of 4 pipelines: each compiled by at most the 8 threads.
        assert!(LOADED.load(Ordering::Relaxed) - loaded >= 96 - 4 * 8);
        let l = library()
            .unwrap()
            .write()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: a plain query on a live library, alone.
        assert!(unsafe { l.library.GetSerializedSize() } > 0);
    }

    /// A compute kernel of `n` threads whose code holds its root signature.
    struct Kernel {
        code: Vec<u8>,
        root: ID3D12RootSignature,
    }

    fn kernel(device: &ID3D12Device, n: u32) -> Kernel {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        let source = format!("[RootSignature(\"\")] [numthreads({n}, 1, 1)] void main() {{}}");
        let mut blob = None;
        // SAFETY: the source lives through compilation and the blob owns
        // its bytes.
        let code = unsafe {
            D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                windows::core::PCSTR::null(),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut blob,
                None,
            )
            .unwrap();
            let blob = blob.unwrap();
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
                .to_vec()
        };
        // SAFETY: the code holds the root signature.
        let root = unsafe { device.CreateRootSignature(0, &code) }.unwrap();
        Kernel { code, root }
    }

    /// `k`'s pipeline named `name` in `library`, and where it came from.
    fn ask(library: &RwLock<Library>, device: &ID3D12Device, k: &Kernel, name: &str) -> Source {
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            // SAFETY: a copy of a live root signature, not released by the
            // description.
            pRootSignature: unsafe { std::mem::transmute_copy(&k.root) },
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: k.code.as_ptr().cast(),
                BytecodeLength: k.code.len(),
            },
            ..Default::default()
        };
        found_in(
            library,
            name,
            // SAFETY: the description points at live data.
            |l, name| unsafe { l.LoadComputePipeline(name, &desc) },
            // SAFETY: as above.
            || unsafe { device.CreateComputePipelineState(&desc) },
        )
        .unwrap()
        .1
    }

    /// A save that fails leaves the library's pipelines to save: the next
    /// save writes them, and only then do they stop counting as added.
    #[test]
    fn a_failed_save_is_made_again() {
        let test = "pipeline::library::tests::a_failed_save_is_made_again";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let device = host.device.cast::<ID3D12Device1>().unwrap();
        let library = RwLock::new(open(&device, Vec::new(), None).unwrap());
        library.read().unwrap().added.store(2, Ordering::Relaxed);
        let dir = tempfile::tempdir().unwrap();
        // A file where the folder should be.
        let not_a_folder = dir.path().join("pipelines");
        std::fs::write(&not_a_folder, b"in the way").unwrap();
        assert!(matches!(
            write_library(&library, &not_a_folder, true, |_| Ok(true)),
            Saved::Failed(_)
        ));
        assert_eq!(library.read().unwrap().added.load(Ordering::Relaxed), 2);
        // The early list not written: neither is the save.
        let good = dir.path().join("cache");
        assert!(matches!(
            write_library(&library, &good, true, |_| Err(std::io::Error::other(
                "full"
            ))),
            Saved::Failed(_)
        ));
        assert_eq!(library.read().unwrap().added.load(Ordering::Relaxed), 2);
        let mut size = None;
        assert_eq!(
            write_library(&library, &good, true, |bytes| {
                size = bytes;
                Ok(true)
            }),
            Saved::Written
        );
        assert_eq!(library.read().unwrap().added.load(Ordering::Relaxed), 0);
        let written = std::fs::metadata(good.join(LIBRARY_FILE)).unwrap().len();
        assert_eq!(Some(written as usize), size);
        assert_eq!(
            write_library(&library, &good, true, |_| Ok(false)),
            Saved::Nothing,
            "nothing new"
        );
    }

    /// Two saves at once: one asked to wait writes after the one under way,
    /// which counts what it wrote first, so that what was added stops
    /// counting once and the count does not wrap; one that does not wait
    /// gives way.
    #[test]
    fn saves_at_once_count_what_they_wrote_once() {
        let test = "pipeline::library::tests::saves_at_once_count_what_they_wrote_once";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let device = host.device.cast::<ID3D12Device1>().unwrap();
        let library = RwLock::new(open(&device, Vec::new(), None).unwrap());
        library.read().unwrap().added.store(2, Ordering::Relaxed);
        let dir = tempfile::tempdir().unwrap();
        let (inside, go) = (std::sync::Barrier::new(2), std::sync::Barrier::new(2));
        std::thread::scope(|s| {
            let first = s.spawn(|| {
                write_library(&library, dir.path(), true, |_| {
                    inside.wait();
                    go.wait();
                    Ok(true)
                })
            });
            inside.wait();
            // The first is between its write and its count.
            assert_eq!(
                write_library(&library, dir.path(), false, |_| Ok(true)),
                Saved::Nothing,
                "gives way"
            );
            let second = s.spawn(|| write_library(&library, dir.path(), true, |_| Ok(false)));
            go.wait();
            assert_eq!(first.join().unwrap(), Saved::Written);
            assert_eq!(
                second.join().unwrap(),
                Saved::Nothing,
                "nothing new by then"
            );
        });
        assert_eq!(library.read().unwrap().added.load(Ordering::Relaxed), 0);
    }

    /// A new generation takes what is asked for from the one before, and
    /// keeps only that: what was not asked for is made again later.
    #[test]
    fn a_new_generation_keeps_what_is_asked_for() {
        let test = "pipeline::library::tests::a_new_generation_keeps_what_is_asked_for";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let device = host.device.cast::<ID3D12Device1>().unwrap();
        let (a, b) = (kernel(&host.device, 3), kernel(&host.device, 5));
        let dir = tempfile::tempdir().unwrap();
        let saved = |library: &RwLock<Library>| {
            assert_eq!(
                write_library(library, dir.path(), true, |_| Ok(true)),
                Saved::Written
            );
            std::fs::read(dir.path().join(LIBRARY_FILE)).unwrap()
        };
        let old = RwLock::new(open(&device, Vec::new(), None).unwrap());
        assert_eq!(ask(&old, &host.device, &a, "a"), Source::Made);
        assert_eq!(ask(&old, &host.device, &b, "b"), Source::Made);
        // Even a matching name must not return a pipeline whose
        // description has different bytecode.
        let mismatch = crate::testgpu::expect_layer_errors(|| ask(&old, &host.device, &b, "a"));
        assert_eq!(mismatch, Source::Made);
        assert_eq!(ask(&old, &host.device, &a, "a"), Source::Library);
        let new = RwLock::new(new_generation(&device, saved(&old), None).unwrap());
        assert_eq!(ask(&new, &host.device, &a, "a"), Source::Previous);
        assert_eq!(ask(&new, &host.device, &a, "a"), Source::Library);
        let next = RwLock::new(open(&device, saved(&new), None).unwrap());
        assert_eq!(ask(&next, &host.device, &a, "a"), Source::Library);
        assert_eq!(
            ask(&next, &host.device, &b, "b"),
            Source::Made,
            "not asked for"
        );
        let empty = RwLock::new(new_generation(&device, saved(&next), None).unwrap());
        let reopened = RwLock::new(open(&device, saved(&empty), None).unwrap());
        assert_eq!(
            ask(&reopened, &host.device, &a, "a"),
            Source::Made,
            "nothing survived"
        );
    }

    /// A names file is its library file's: read back with the names it was
    /// written with, and refused for another file (another size, or other
    /// bytes of the same size), another format or a line that is no name.
    #[test]
    fn names_are_read_back_for_their_own_file_only() {
        let blob = b"a serialised pipeline library".repeat(8);
        let a = "0123456789abcdef0123456789abcdef";
        let names = Names {
            held: [a.to_owned(), format!("{a}~1")].into(),
            broken: [a.to_owned()].into(),
            complete: true,
        };
        let file = FileId::of(&blob);
        let text = names.text(&file);
        assert_eq!(Names::read(&text, &blob), Some((names.clone(), file)));
        // A file of the first version marks none broken, and its marks are
        // no names.
        let first = text.replace(NAMES_FORMAT, NAMES_FORMAT_1);
        assert_eq!(Names::read(&first, &blob), None, "a mark in version 1");
        let first = first.replace(BROKEN, "");
        let unmarked = Names {
            broken: HashSet::new(),
            ..names.clone()
        };
        assert_eq!(Names::read(&first, &blob), Some((unmarked, file)));
        let partial = Names {
            complete: false,
            ..names.clone()
        };
        assert_eq!(
            Names::read(&partial.text(&file), &blob).map(|(n, _)| n),
            Some(partial)
        );
        let mut other = blob.clone();
        other[3] ^= 1;
        assert_eq!(Names::read(&text, &other), None, "other bytes");
        assert_eq!(Names::read(&text, &blob[1..]), None, "another size");
        assert_eq!(Names::read(&format!("{text}{a}~4\n"), &blob), None);
        assert_eq!(Names::read(&format!("{text}not a name\n"), &blob), None);
        assert_eq!(
            Names::read(&text.replace(NAMES_FORMAT, "xtp pipeline names 0"), &blob),
            None
        );
        assert_eq!(Names::read("", &blob), None);
        assert!(is_name(a) && is_name(&format!("{a}~3")));
        assert!(!is_name(&format!("{a}~0")) && !is_name(&a.to_uppercase()) && !is_name("a"));
        assert!(names.ready_any(a) && !names.ready_any(&a.replace('0', "f")));
        // Ready by a name held and not broken only.
        let only_broken = Names {
            held: [a.to_owned()].into(),
            ..names.clone()
        };
        assert!(!only_broken.ready_any(a), "its one name is broken");
        assert_eq!(
            Names::read(&format!("{text}{a}~2{BROKEN}{BROKEN}\n"), &blob),
            None
        );
    }

    /// An entry of a pipeline's name that does not load for its
    /// description (here: another kernel stored under that name) used to
    /// make the pipeline again each time, since StorePipeline refuses a
    /// name the library holds. The pipeline is stored under the next name
    /// instead, and found there from then on, after a save too; the names
    /// file says which names the saved library holds.
    #[test]
    fn a_name_that_does_not_load_gets_the_next_one() {
        let test = "pipeline::library::tests::a_name_that_does_not_load_gets_the_next_one";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let device = host.device.cast::<ID3D12Device1>().unwrap();
        let (a, b) = (kernel(&host.device, 3), kernel(&host.device, 5));
        let name = "00112233445566778899aabbccddeeff";
        let library = RwLock::new(open(&device, Vec::new(), None).unwrap());
        assert_eq!(ask(&library, &host.device, &a, name), Source::Made);
        let other =
            || crate::testgpu::expect_layer_errors(|| ask(&library, &host.device, &b, name));
        assert_eq!(other(), Source::Made, "the entry of its name is another's");
        assert_eq!(other(), Source::Library, "found under the next name");
        assert_eq!(ask(&library, &host.device, &a, name), Source::Library);
        let held = library.read().unwrap().names().clone();
        assert_eq!(held.held, [name.to_owned(), slot(name, 1)].into());
        assert!(held.complete, "started empty");

        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            write_library(&library, dir.path(), true, |_| Ok(true)),
            Saved::Written
        );
        let blob = std::fs::read(dir.path().join(LIBRARY_FILE)).unwrap();
        let text = std::fs::read_to_string(dir.path().join(NAMES_FILE)).unwrap();
        assert_eq!(Names::read(&text, &blob), Some((held, FileId::of(&blob))));
        let reopened = RwLock::new(open(&device, blob, Some(&text)).unwrap());
        let other =
            || crate::testgpu::expect_layer_errors(|| ask(&reopened, &host.device, &b, name));
        assert_eq!(other(), Source::Library, "repaired for the next process");
        assert_eq!(
            reopened.read().unwrap().added.load(Ordering::Relaxed),
            0,
            "nothing made"
        );
    }

    /// The precook does not load a pipeline the library's names hold
    /// ([`without_loading`]); one they do not hold is made and stored as
    /// always. A library read without its names learns those that load,
    /// and a save writes the names learned.
    #[test]
    fn the_precook_does_not_load_what_the_names_hold() {
        let test = "pipeline::library::tests::the_precook_does_not_load_what_the_names_hold";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let device = host.device.cast::<ID3D12Device1>().unwrap();
        let (a, b) = (kernel(&host.device, 3), kernel(&host.device, 5));
        let (name_a, name_b) = (
            "a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0",
            "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0",
        );
        let dir = tempfile::tempdir().unwrap();
        let library = RwLock::new(open(&device, Vec::new(), None).unwrap());
        assert_eq!(ask(&library, &host.device, &a, name_a), Source::Made);
        assert_eq!(
            write_library(&library, dir.path(), true, |_| Ok(true)),
            Saved::Written
        );
        let blob = std::fs::read(dir.path().join(LIBRARY_FILE)).unwrap();
        let text = std::fs::read_to_string(dir.path().join(NAMES_FILE)).unwrap();

        // Read with its names: held, so not loaded; another is made. (The
        // count is the process's: other tests' precooks, the replay's,
        // may add to it meanwhile. `Source::Known` says this ask's.)
        let known = KNOWN.load(Ordering::Relaxed);
        let named = RwLock::new(open(&device, blob.clone(), Some(&text)).unwrap());
        assert_eq!(
            without_loading(|| ask(&named, &host.device, &a, name_a)),
            Source::Known
        );
        assert!(KNOWN.load(Ordering::Relaxed) > known, "counted");
        assert_eq!(
            without_loading(|| ask(&named, &host.device, &b, name_b)),
            Source::Made
        );
        assert_eq!(
            ask(&named, &host.device, &a, name_a),
            Source::Library,
            "a session loads it"
        );

        // Read without its names (an older file): loaded once, then known.
        let unnamed = RwLock::new(open(&device, blob, None).unwrap());
        assert_eq!(
            without_loading(|| ask(&unnamed, &host.device, &a, name_a)),
            Source::Library
        );
        assert_eq!(
            without_loading(|| ask(&unnamed, &host.device, &a, name_a)),
            Source::Known
        );
        // Nothing added: the save writes the names learned alone, for the
        // file as it is.
        std::fs::remove_file(dir.path().join(NAMES_FILE)).unwrap();
        assert_eq!(
            write_library(&unnamed, dir.path(), true, |_| Ok(false)),
            Saved::Nothing
        );
        let blob = std::fs::read(dir.path().join(LIBRARY_FILE)).unwrap();
        let text = std::fs::read_to_string(dir.path().join(NAMES_FILE)).unwrap();
        let (learned, _) = Names::read(&text, &blob).expect("the file's names");
        assert_eq!(
            (learned.held, learned.complete),
            ([name_a.to_owned()].into(), false)
        );
    }

    /// The Codex review of 12556d6: the precook answered a pipeline by any
    /// name the library held, before the driver, even one whose entry does
    /// not load for its description. A load that fails now marks the name
    /// broken, in the names file too, and the precook does not answer by a
    /// broken name: it makes the pipeline under the next one, by which it
    /// answers from then on.
    #[test]
    fn the_precook_does_not_answer_by_a_broken_name() {
        let test = "pipeline::library::tests::the_precook_does_not_answer_by_a_broken_name";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let device = host.device.cast::<ID3D12Device1>().unwrap();
        let (a, b) = (kernel(&host.device, 3), kernel(&host.device, 7));
        let name = "c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0";
        let dir = tempfile::tempdir().unwrap();
        let saved = |library: &RwLock<Library>| {
            let wrote = write_library(library, dir.path(), true, |_| Ok(true));
            assert!(!matches!(wrote, Saved::Failed(_)), "{wrote:?}");
            let blob = std::fs::read(dir.path().join(LIBRARY_FILE)).unwrap();
            let text = std::fs::read_to_string(dir.path().join(NAMES_FILE)).unwrap();
            (blob, text)
        };
        // The library's entry of the name is another description's.
        let library = RwLock::new(open(&device, Vec::new(), None).unwrap());
        assert_eq!(ask(&library, &host.device, &a, name), Source::Made);
        let (blob, text) = saved(&library);

        // A session's load of `b` by that name fails, and its compile too
        // (nothing goes under the next name): the names file says broken.
        let session = RwLock::new(open(&device, blob.clone(), Some(&text)).unwrap());
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            // SAFETY: a copy of a live root signature, not released by the
            // description.
            pRootSignature: unsafe { std::mem::transmute_copy(&b.root) },
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: b.code.as_ptr().cast(),
                BytecodeLength: b.code.len(),
            },
            ..Default::default()
        };
        let refused = crate::testgpu::expect_layer_errors(|| {
            found_in(
                &session,
                name,
                // SAFETY: the description points at live data.
                |l, name| unsafe { l.LoadComputePipeline(name, &desc) },
                || Err(windows::Win32::Foundation::E_FAIL.into()),
            )
        });
        assert!(refused.is_err());
        let (blob, text) = saved(&session);
        let (names, _) = Names::read(&text, &blob).expect("its names");
        assert_eq!(names.broken, [name.to_owned()].into(), "{text}");
        assert!(!names.ready_any(name));

        // The precook does not answer by it: it loads, then makes `b`,
        // under the next name.
        let precook = RwLock::new(open(&device, blob, Some(&text)).unwrap());
        let made = crate::testgpu::expect_layer_errors(|| {
            without_loading(|| ask(&precook, &host.device, &b, name))
        });
        assert_eq!(made, Source::Made, "not answered by a broken name");
        assert_eq!(
            without_loading(|| ask(&precook, &host.device, &b, name)),
            Source::Known,
            "by the next name"
        );
        let (blob, text) = saved(&precook);
        let (names, _) = Names::read(&text, &blob).expect("its names");
        assert_eq!(
            (names.held, names.broken),
            (
                [name.to_owned(), slot(name, 1)].into(),
                [name.to_owned()].into()
            )
        );

        // A session after it loads `b` by the next name; `a` still loads
        // by its own, which mends it.
        let next = RwLock::new(open(&device, blob, Some(&text)).unwrap());
        let found = crate::testgpu::expect_layer_errors(|| ask(&next, &host.device, &b, name));
        assert_eq!(found, Source::Library);
        assert_eq!(ask(&next, &host.device, &a, name), Source::Library);
        assert!(next.read().unwrap().names().broken.is_empty(), "mended");
    }
}
