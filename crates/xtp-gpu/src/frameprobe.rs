//! Diagnostics: what chosen draws and dispatches read through their
//! descriptor tables, at the moment they run, and the order in which the
//! R9G9B9E5 stand-ins are decoded against their writers
//! (docs/analysis/pix-world-frame.md §11).
//!
//! `XTP_FRAME_PROBE=<program>[,<program>...]` (16 hex digits each), with
//! `XTP_CAPTURE=<folder>` and frames listed by `XTP_CAPTURE_FRAMES` or
//! `XTP_CAPTURE_WORLD`, writes `probe-<n>.txt` for each listed frame `n`
//! once its lists have run. For each op of a listed program (the first
//! [`OPS`] a frame), and each descriptor its stage reads through a table:
//! - where the descriptor is: the table's byte offset the title gave (its
//!   user data), the title slot, whether that slot is past the host heap's
//!   mirrored slots, and the host slot the shader reads (in a table made
//!   afresh in dynamic slots, `draw::State::tables`, when it has one);
//! - the view the title wrote there, and for a mirrored slot the view the
//!   host slot was last made from;
//! - for a buffer view: the title range, the memory there (private,
//!   mapped: a view of physical pages; its protection), at how many other
//!   addresses the title shows its 64 KiB page (`watch::views_of`), and the
//!   host copy it resolves to;
//! - the range's bytes three times: title memory when the op is recorded,
//!   title memory when its list is submitted (what the submission's
//!   refresh takes), and the host copy on the GPU just before the op
//!   (copied in the list where the op stands). 64 KiB at most each
//!   (`XTP_FRAME_PROBE_KIB`, up to 16 MiB; [`bytes`]), counted (non-zero
//!   bytes, the first and the last of them) and shown as words up to
//!   [`SHOWN`] bytes, and from the first non-zero byte when that lies
//!   further. A frame's read-back holds 48 MiB (`XTP_FRAME_PROBE_MIB`, up
//!   to 512; [`budget`]).
//! - for a constant view: its bytes the same three ways, all of them shown
//!   up to [`SHOWN_CONSTANTS`] (a lighting program's light colours,
//!   directions and feature switches are there; docs/analysis/lighting-terms.md);
//! - for a texture view: the host texture's size (depth or layers too) and
//!   the title address of its memory, which tells two views of one texture.
//!
//! An indirect op of a listed program also gets its argument records, read
//! the same three ways ([`arguments`]). A dispatch of a listed program
//! that is dropped for want of a host pipeline (its program does not
//! translate) is probed too, as "dropped": the tables its console program
//! says it uses, resolved from the roots the title bound ([`dropped`]), so
//! what it would have read and written shows though it never runs.
//!
//! Every listed frame also gets the R9G9B9E5 timeline (upstream watch item
//! 360): each texture an op reads through a table in title format 67 (an
//! R9G9B9E5 view), and each UAV write at the same address.
//! Each decode command counts the submissions in which its predicate
//! allowed the conversion to run.
//!
//! **`XTP_DIAG_DIR_LIGHT=<r>,<g>,<b>[,<scale>][/<r>,<g>,<b>[,<scale>]...]`**
//! (off by default; any frame, listed or not) gives the deferred lighting
//! a directional light colour of ours, to show what the image looks like
//! with one (docs/analysis/lighting-terms.md §4.3: the title's is 0). Just
//! before each op whose stage reads the light record (`s1+0`, 340 bytes)
//! and the light scale buffer (`s0+232`, 384 bytes), the record's colour
//! (bytes 300 to 311) and the scale (byte 84, 1 when not given) are written
//! into their host copies on the GPU, at that place in the list ([`dir_light`]).
//! Title memory is not written: the next submission's refresh, or the
//! scale's writer, puts the title's values back. Several colours take turns,
//! [`dir_light_frames`] frames each (`XTP_DIAG_DIR_LIGHT_FRAMES`, 4 by
//! default), in the order given, from frame 0; the log says which colour
//! each frame recorded with.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use xtp_shader::ResourceType;

use crate::host::HostList;
use crate::object::This;

/// Ops probed a frame, at most.
const OPS: usize = 96;
/// Bytes of a snapshot shown as words.
const SHOWN: usize = 512;
/// Bytes of a constant view's snapshot shown as words: all of a constant
/// buffer the size the title's lighting reads (3,424 bytes).
const SHOWN_CONSTANTS: usize = 4096;
/// Bytes shown from a snapshot's first non-zero byte, when it lies past
/// the first [`SHOWN`].
const SHOWN_AT_FIRST: usize = 128;
/// The title format of R9G9B9E5.
const R9G9B9E5: u32 = 67;
/// Presents after a listed frame at which it is reported even if some
/// probed op's list was never submitted.
const LATE: u64 = 4;

/// A size switch in `unit` bytes: `default` when unset or not a number,
/// else clamped to `1..=max`.
fn sized(text: Option<&str>, default: u64, max: u64, unit: u64) -> u64 {
    text.and_then(|t| t.trim().parse::<u64>().ok())
        .map_or(default, |n| n.clamp(1, max))
        * unit
}

/// Bytes of a range snapshot (title memory) or copied back (host copy):
/// `XTP_FRAME_PROBE_KIB`, 64 KiB by default, 16 MiB at most.
fn bytes() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| {
        sized(
            std::env::var("XTP_FRAME_PROBE_KIB").ok().as_deref(),
            64,
            16 << 10,
            1 << 10,
        )
    })
}

/// Read-back memory a frame: `XTP_FRAME_PROBE_MIB`, 48 MiB by default, 512
/// at most.
fn budget() -> u64 {
    static BUDGET: OnceLock<u64> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        sized(
            std::env::var("XTP_FRAME_PROBE_MIB").ok().as_deref(),
            48,
            512,
            1 << 20,
        )
    })
}

/// A descriptor a stage reads through a table, as `draw::State` resolves
/// it for the op.
pub(crate) struct Table {
    pub(crate) slot: u8,
    pub(crate) offset_dwords: u32,
    pub(crate) kind: ResourceType,
    /// The title's descriptor.
    pub(crate) handle: u64,
    /// Its slot in the title's heap.
    pub(crate) index: Option<u32>,
    /// The host heap's slots mirroring the title's.
    pub(crate) mirrored: Option<u32>,
    /// The table's byte offset in its heap, as the title gave it.
    pub(crate) title_offset: Option<u32>,
    /// The byte offset the shader gets instead, for a table made afresh.
    pub(crate) fresh_offset: Option<u32>,
    /// For a mirrored slot: the view its host slot was made from.
    pub(crate) host_view: Option<String>,
}

/// A range's bytes, title memory or host copy.
struct Snapshot {
    bytes: Vec<u8>,
}

/// `bytes` as little-endian words in hex.
fn words(bytes: &[u8]) -> String {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&w| format!("{:08x}", u32::from_le_bytes(w)))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Snapshot {
    fn text(&self, len: u64) -> String {
        self.text_shown(len, SHOWN)
    }

    /// As [`Snapshot::text`], with the first `shown` bytes as words.
    fn text_shown(&self, len: u64, shown: usize) -> String {
        let b = &self.bytes;
        let nz = b.iter().filter(|&&x| x != 0).count();
        let more = if b.len() > shown { " …" } else { "" };
        let mut out = format!(
            "non-zero {nz} of {} bytes{}",
            b.len(),
            if (b.len() as u64) < len {
                format!(" (first {} of {len})", b.len())
            } else {
                String::new()
            },
        );
        let first = b.iter().position(|&x| x != 0);
        let last = b.iter().rposition(|&x| x != 0);
        if let (Some(first), Some(last)) = (first, last) {
            let _ = write!(out, ", from +{first:#x} to +{last:#x}");
        }
        let _ = write!(out, " [{}{more}]", words(&b[..b.len().min(shown)]));
        // The first non-zero bytes, where the words above do not reach.
        if let Some(first) = first.filter(|&f| f >= shown) {
            let at = first & !15;
            let end = (at + SHOWN_AT_FIRST).min(b.len());
            let _ = write!(out, "; at +{at:#x}: [{}]", words(&b[at..end]));
        }
        out
    }
}

/// One descriptor of a probed op.
struct Read {
    text: String,
    /// The buffer range it views: title address and bytes.
    range: Option<(u64, u64)>,
    /// Bytes of its snapshots shown as words.
    shown: usize,
    at_record: Option<Snapshot>,
    at_submit: Option<Snapshot>,
    /// Where the host copy's bytes are in the read-back buffer, or why
    /// they are not.
    gpu: Result<(u64, u64), &'static str>,
}

struct Op {
    list: usize,
    /// The title submission that ran its list, once one has.
    submission: Option<u64>,
    program: String,
    /// Dropped for want of a host pipeline: its reads are what it would
    /// have read ([`dropped`]).
    dropped: bool,
    reads: Vec<Read>,
}

/// A texture an op of a listed frame reads or writes through a table.
struct TextureUse {
    list: usize,
    after: u64,
    program: String,
    address: u64,
    title_format: u32,
    host: String,
    write: bool,
}

/// What a listed frame gathers until its report.
struct Frame {
    frame: u64,
    matched: usize,
    ops: Vec<Op>,
    textures: Vec<TextureUse>,
    decodings: Vec<(u64, usize, u64, std::sync::Arc<AtomicU64>)>,
    readback: Option<ID3D12Resource>,
    used: u64,
    /// Title submissions made while it was recorded or ran: number, lists.
    submissions: Vec<(u64, Vec<usize>)>,
}

static FRAMES: Mutex<Vec<Frame>> = Mutex::new(Vec::new());
/// Title submissions so far.
static SUBMISSIONS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The program whose stage this thread prepares (`begin`).
    static PROGRAM: RefCell<String> = const { RefCell::new(String::new()) };
    /// The op this thread probed last, until the next stage is prepared:
    /// its frame, list and index, for its indirect arguments
    /// ([`arguments`]).
    static LAST: Cell<Option<(u64, usize, usize)>> = const { Cell::new(None) };
}

fn frames() -> MutexGuard<'static, Vec<Frame>> {
    FRAMES.lock().unwrap_or_else(|e| e.into_inner())
}

fn key(list: &HostList) -> usize {
    std::ptr::from_ref(list) as usize
}

/// The programs `XTP_FRAME_PROBE` lists.
fn programs() -> &'static [String] {
    static LIST: OnceLock<Vec<String>> = OnceLock::new();
    LIST.get_or_init(|| parse(&std::env::var("XTP_FRAME_PROBE").unwrap_or_default()))
}

fn parse(text: &str) -> Vec<String> {
    text.split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| h.len() == 16 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .collect()
}

/// The light record among a lighting stage's tables (table, dword offset,
/// view bytes), and where its colour is (docs/analysis/lighting-terms.md
/// §4.3).
const RECORD: (u8, u32, u64) = (1, 0, 340);
const RECORD_COLOUR: u64 = 300;
/// The light scale buffer among the same tables, and where the scale is.
const SCALE: (u8, u32, u64) = (0, 232, 384);
const SCALE_AT: u64 = 84;

/// One `XTP_DIAG_DIR_LIGHT` colour.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DirLight {
    colour: [f32; 3],
    scale: f32,
}

/// `XTP_DIAG_DIR_LIGHT`'s colours: none unless every one is three or four
/// finite numbers of 0 or more.
fn parse_dir_light(text: &str) -> Vec<DirLight> {
    text.split('/')
        .map(|one| {
            let v: Vec<f32> = one
                .split(',')
                .map(|t| t.trim().parse::<f32>())
                .collect::<Result<_, _>>()
                .ok()?;
            let (colour, scale) = match *v.as_slice() {
                [r, g, b] => ([r, g, b], 1.0),
                [r, g, b, s] => ([r, g, b], s),
                _ => return None,
            };
            let fine = |x: f32| x.is_finite() && x >= 0.0;
            (colour.into_iter().all(fine) && fine(scale)).then_some(DirLight { colour, scale })
        })
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default()
}

/// Frames each `XTP_DIAG_DIR_LIGHT` colour lasts: `XTP_DIAG_DIR_LIGHT_FRAMES`,
/// 4 by default.
fn dir_light_frames() -> u64 {
    static FRAMES: OnceLock<u64> = OnceLock::new();
    *FRAMES.get_or_init(|| {
        sized(
            std::env::var("XTP_DIAG_DIR_LIGHT_FRAMES").ok().as_deref(),
            4,
            1 << 20,
            1,
        )
    })
}

fn dir_lights() -> &'static [DirLight] {
    static LIGHTS: OnceLock<Vec<DirLight>> = OnceLock::new();
    LIGHTS.get_or_init(|| {
        let Ok(text) = std::env::var("XTP_DIAG_DIR_LIGHT") else {
            return Vec::new();
        };
        let lights = parse_dir_light(&text);
        if lights.is_empty() {
            tracing::warn!(
                text,
                "XTP_DIAG_DIR_LIGHT is not <r>,<g>,<b>[,<scale>][/...] of finite numbers of 0 or more: off"
            );
        } else {
            tracing::warn!(
                ?lights,
                frames = dir_light_frames(),
                "XTP_DIAG_DIR_LIGHT: the deferred lighting gets a directional light colour of ours"
            );
        }
        lights
    })
}

/// Whether `XTP_DIAG_DIR_LIGHT` is on.
pub(crate) fn dir_light_on() -> bool {
    !dir_lights().is_empty()
}

/// The colour of `lights` frame `frame` records with, each lasting `frames`
/// frames in turn.
fn dir_light_for(lights: &[DirLight], frames: u64, frame: u64) -> Option<DirLight> {
    let n = lights.len() as u64;
    (n > 0).then(|| lights[((frame / frames.max(1)) % n) as usize])
}

/// `draw::State::bind` with `XTP_DIAG_DIR_LIGHT` on, for a stage about to
/// be recorded in `list` with `tables`: if the stage reads the light record
/// and the light scale buffer, this frame's colour and scale are written
/// into their host copies here in the list, before the op.
pub(crate) fn dir_light(list: &HostList, tables: &[Table]) {
    let frame = crate::queue::presented() + 1;
    let Some(light) = dir_light_for(dir_lights(), dir_light_frames(), frame) else {
        return;
    };
    let at = |(slot, offset, len): (u8, u32, u64)| {
        tables
            .iter()
            .find(|t| t.slot == slot && t.offset_dwords == offset)
            .and_then(|t| crate::descriptor::view(t.handle))
            .and_then(|v| span_of(&v))
            .filter(|&(_, n)| n == len)
            .map(|(address, _)| address)
    };
    let (Some(record), Some(scale)) = (at(RECORD), at(SCALE)) else {
        return;
    };
    let [r, g, b] = light.colour;
    let colour = write_words(
        list,
        record + RECORD_COLOUR,
        &[r.to_bits(), g.to_bits(), b.to_bits()],
    );
    let scaled = write_words(list, scale + SCALE_AT, &[light.scale.to_bits()]);
    // Once a frame in the log.
    static LOGGED: AtomicU64 = AtomicU64::new(u64::MAX);
    if LOGGED.swap(frame, Ordering::Relaxed) != frame {
        tracing::info!(
            frame,
            ?light,
            record = format_args!("{record:#x}"),
            scale = format_args!("{scale:#x}"),
            colour,
            scaled,
            "XTP_DIAG_DIR_LIGHT: directional light written for the lighting"
        );
    }
}

/// `words` written at title `address` in its host copy, on the GPU at this
/// place in `list`; title memory is left as it is. False where the address
/// has no host copy.
fn write_words(list: &HostList, address: u64, words: &[u32]) -> bool {
    use windows::core::Interface as _;
    let Some(s) = crate::shadow::existing(address, 4 * words.len() as u64) else {
        return false;
    };
    let Ok(list2) = list.list.cast::<ID3D12GraphicsCommandList2>() else {
        return false;
    };
    let gpu = s.gpu(address);
    let params: Vec<D3D12_WRITEBUFFERIMMEDIATE_PARAMETER> = words
        .iter()
        .zip(0u64..)
        .map(|(&value, i)| D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
            Dest: gpu + 4 * i,
            Value: value,
        })
        .collect();
    let (common, dest) = (D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST);
    list.keep(&s.buffer);
    // SAFETY: an open list; the host copy, which the list keeps, and
    // destinations inside it. Host copies are taken as common, as
    // [`copy`] and `shadow::write` take them.
    unsafe {
        list2.ResourceBarrier(&[
            crate::host::uav_barrier(),
            crate::host::transition(&s.buffer, common, dest),
        ]);
        list2.WriteBufferImmediate(params.len() as u32, params.as_ptr(), None);
        list2.ResourceBarrier(&[crate::host::transition(&s.buffer, dest, common)]);
    }
    true
}

/// The frame being recorded, if the probe runs and lists it.
fn recording() -> Option<u64> {
    if programs().is_empty() || crate::capture::folder().is_none() {
        return None;
    }
    let frame = crate::queue::presented() + 1;
    crate::capture::listed(frame).then_some(frame)
}

fn frame_mut(frames: &mut Vec<Frame>, frame: u64) -> &mut Frame {
    if let Some(i) = frames.iter().position(|f| f.frame == frame) {
        return &mut frames[i];
    }
    frames.push(Frame {
        frame,
        matched: 0,
        ops: Vec::new(),
        textures: Vec::new(),
        decodings: Vec::new(),
        readback: None,
        used: 0,
        submissions: Vec::new(),
    });
    frames.last_mut().unwrap()
}

/// `draw::State::bind`, before a stage's tables are prepared: the program
/// the texture uses that follow ([`texture`]) belong to.
pub(crate) fn begin(program: &str) {
    if recording().is_some() {
        PROGRAM.with(|p| program.clone_into(&mut p.borrow_mut()));
        LAST.set(None);
    }
}

/// Whether a stage of `program` is probed in the frame being recorded.
pub(crate) fn wanted(program: &str) -> bool {
    recording().is_some() && programs().iter().any(|p| p == program)
}

/// `draw::State::bind`, once a stage of a probed program has its tables
/// (`tables`) for the op about to be recorded in `list`.
pub(crate) fn stage(list: &HostList, program: &str, tables: Vec<Table>) {
    push(list, program, &tables, false);
}

/// `draw::State`, for a dispatch of a probed program dropped for want of a
/// host pipeline: the tables its console program says it uses (`tables`,
/// resolved from the roots the title bound), probed as for an op that runs.
/// The copies of its host copies stand where it would have run.
pub(crate) fn dropped(list: &HostList, program: &str, tables: Vec<Table>) {
    push(list, program, &tables, true);
}

fn push(list: &HostList, program: &str, tables: &[Table], dropped: bool) {
    LAST.set(None);
    let Some(frame) = recording() else {
        return;
    };
    {
        let mut frames = frames();
        let f = frame_mut(&mut frames, frame);
        f.matched += 1;
        if f.ops.len() >= OPS {
            return;
        }
    }
    // Looked up without the lock: finding a host copy may make one, which
    // may wait.
    let found: Vec<(Read, Option<CopyFrom>)> = tables.iter().map(read).collect();
    let mut frames = frames();
    let f = frame_mut(&mut frames, frame);
    let reads = found
        .into_iter()
        .map(|(mut r, from)| {
            if let Some((s, address, len)) = from {
                r.gpu = copy(f, list, &s, address, len);
            }
            r
        })
        .collect();
    LAST.set(Some((frame, key(list), f.ops.len())));
    f.ops.push(Op {
        list: key(list),
        submission: None,
        program: program.to_owned(),
        dropped,
        reads,
    });
}

/// `draw::State::execute_indirect`, once the op's stage was probed (it ran,
/// or [`dropped`]): its `len` bytes of argument records at title `address`,
/// read as a buffer view is, added to the op this thread probed last.
pub(crate) fn arguments(list: &HostList, address: u64, len: u64) {
    let Some((frame, at, index)) = LAST.take() else {
        return;
    };
    if at != key(list) || recording() != Some(frame) {
        return;
    }
    let mut text = "indirect arguments:".to_owned();
    let (at_record, from, gpu) = range(&mut text, address, len);
    let mut frames = frames();
    let f = frame_mut(&mut frames, frame);
    let gpu = match from {
        Some((s, address, n)) => copy(f, list, &s, address, n),
        None => gpu,
    };
    if let Some(op) = f.ops.get_mut(index) {
        op.reads.push(Read {
            text,
            range: Some((address, len)),
            shown: SHOWN,
            at_record,
            at_submit: None,
            gpu,
        });
    }
}

/// A host copy, and the title range of it to copy back.
type CopyFrom = (std::sync::Arc<crate::shadow::Shadow>, u64, u64);

/// A range's bytes now, the host copy to read on the GPU, and where the
/// GPU's bytes are (or why there are none).
type Found = (
    Option<Snapshot>,
    Option<CopyFrom>,
    Result<(u64, u64), &'static str>,
);

/// The title range `address`+`len` described into `text`: its memory and
/// host copy, its bytes now, and the host copy to read on the GPU (or why
/// there is none).
fn range(text: &mut String, address: u64, len: u64) -> Found {
    let _ = write!(
        text,
        "\n      memory {address:#x}+{len:#x}: {}, its 64 KiB page shown at {} other addresses",
        memory_text(address),
        crate::watch::views_of(address as usize)
    );
    let take = len.min(bytes());
    let (from, gpu) = match crate::shadow::find(address, take) {
        Some(s) => {
            let _ = write!(
                text,
                "; host copy at +{:#x} ({} bytes to its end{})",
                s.offset(address),
                s.left(address),
                if s.carded() { ", card" } else { "" }
            );
            let n = take.min(s.left(address));
            (Some((s, address, n)), Err("not copied"))
        }
        None => {
            text.push_str("; no host copy");
            (None, Err("no host copy"))
        }
    };
    (snapshot(address, take), from, gpu)
}

/// What `t` reads, its title bytes taken now, and the host copy whose
/// bytes the GPU reads.
fn read(t: &Table) -> (Read, Option<CopyFrom>) {
    let mut text = format!("s{}+{} {:?}:", t.slot, t.offset_dwords, t.kind);
    if let Some(offset) = t.title_offset {
        let _ = write!(text, " table +{offset:#x}");
    }
    match (t.index, t.mirrored) {
        (Some(i), Some(m)) if i >= m => {
            let _ = write!(text, " title slot {i} (past the {m} mirrored)");
        }
        (Some(i), _) => {
            let _ = write!(text, " title slot {i}");
        }
        (None, _) => text.push_str(" outside the bound heaps"),
    }
    match t.fresh_offset {
        Some(offset) => {
            let slot = offset / 32 + t.offset_dwords / 8;
            let _ = write!(
                text,
                ", made afresh: the shader gets +{offset:#x}, host slot {slot}"
            );
        }
        None => text.push_str(", host slot = title slot"),
    }
    let view = crate::descriptor::view(t.handle);
    let _ = write!(text, "\n      view: {}", view_text(view.as_ref()));
    if let Some(h) = &t.host_view {
        let _ = write!(text, "\n      host slot made from: {h}");
    }
    // A small texture the title placed (the 1×1 ones the CPU fills): its
    // first texels as title memory holds them, whatever the swizzle.
    if let Some(crate::descriptor::View::Placed(_, r, d)) = &view
        && r[0] != 1
        && u64::from(r[4]) * u64::from(r[6]) <= 16
    {
        let address = u64::from(d[10]) | u64::from(d[11]) << 32;
        let _ = write!(
            text,
            "\n      texels in title memory {address:#x}: {}; {}",
            memory_text(address),
            snapshot(address, 64).map_or_else(|| "not readable".to_owned(), |s| s.text(64))
        );
    }
    let span = view.as_ref().and_then(span_of).filter(|&(_, len)| len > 0);
    let (at_record, from, gpu) = match span {
        Some((address, len)) => range(&mut text, address, len),
        None => (None, None, Err("not a buffer view")),
    };
    let constant = matches!(view, Some(crate::descriptor::View::Constant(..)));
    let r = Read {
        text,
        range: span,
        shown: if constant { SHOWN_CONSTANTS } else { SHOWN },
        at_record,
        at_submit: None,
        gpu,
    };
    (r, from)
}

/// The title memory a view reads as bytes: a buffer view's range, or a
/// constant view's (the light parameters and feature switches a lighting
/// program reads are there).
fn span_of(view: &crate::descriptor::View) -> Option<(u64, u64)> {
    match *view {
        crate::descriptor::View::Constant(address, size) => Some((address, u64::from(size))),
        ref v => crate::descriptor::host::buffer_range(v),
    }
}

/// A view in a few words.
fn view_text(view: Option<&crate::descriptor::View>) -> String {
    use crate::descriptor::View;
    match view {
        None => "none (no view of XTP's at the title's descriptor)".into(),
        Some(View::Placed(uav, r, d)) => format!(
            "placed uav={uav} dim {} {}x{} f{} layout {:#x} at {:#x}, view words {:x?}",
            r[0],
            u64::from(r[4]) | u64::from(r[5]) << 32,
            r[6],
            r[8],
            r[11],
            u64::from(d[10]) | u64::from(d[11]) << 32,
            &d[..10]
        ),
        Some(View::Shader(r, desc)) => format!(
            "resource {:#x} ({}) view {desc:x?}",
            r,
            host_text(*r as This)
        ),
        Some(View::Unordered(r, counter, desc)) => format!(
            "uav of {:#x} ({}) counter {counter:#x} view {desc:x?}",
            r,
            host_text(*r as This)
        ),
        Some(View::Constant(address, size)) => format!("constant {address:#x}+{size}"),
        Some(v) => format!("{v:?}").chars().take(80).collect(),
    }
}

/// A title resource's host object, in a few words.
fn host_text(resource: This) -> String {
    match crate::resource::host_resource(resource) {
        Some(t) => {
            // SAFETY: a live resource.
            let d = unsafe { t.GetDesc() };
            let depth = if d.DepthOrArraySize > 1 {
                format!("x{}", d.DepthOrArraySize)
            } else {
                String::new()
            };
            let at = crate::resource::texture_memory(resource)
                .map_or_else(String::new, |(data, _)| {
                    format!(", title memory {:#x}", data as usize)
                });
            format!(
                "host {}x{}{depth} f{} flags {:#x}{at}",
                d.Width, d.Height, d.Format.0, d.Flags.0
            )
        }
        None => match crate::resource::buffer_bytes(resource) {
            Some((p, len)) => format!("buffer {:#x}+{len:#x}", p as usize),
            None => "no host object".into(),
        },
    }
}

/// The title memory at `address`: its kind, state and protection.
fn memory_text(address: u64) -> String {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_IMAGE, MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE, MEMORY_BASIC_INFORMATION,
        VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size; a query-only address.
    let n = unsafe { VirtualQuery(address as *const _, &mut info, size_of_val(&info)) };
    if n == 0 {
        return "not queried".into();
    }
    let kind = match info.Type {
        MEM_PRIVATE => "private",
        MEM_MAPPED => "mapped (a view of physical pages)",
        MEM_IMAGE => "image",
        _ => "free",
    };
    let state = match info.State {
        MEM_COMMIT => "committed",
        MEM_RESERVE => "reserved",
        _ => "free",
    };
    format!(
        "{kind}, {state}, protection {:#x}, region {:#x}+{:#x} of allocation {:#x}",
        info.Protect, info.BaseAddress as usize, info.RegionSize, info.AllocationBase as usize
    )
}

/// `len` bytes of title memory at `address`, if they can be read.
fn snapshot(address: u64, len: u64) -> Option<Snapshot> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
        PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY, VirtualQuery,
    };
    let (start, end) = (address as usize, (address + len) as usize);
    let mut p = start;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size; a query-only address.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        let readable = matches!(
            info.Protect,
            PAGE_READONLY
                | PAGE_READWRITE
                | PAGE_WRITECOPY
                | PAGE_EXECUTE_READ
                | PAGE_EXECUTE_READWRITE
        );
        if n == 0 || info.State != MEM_COMMIT || !readable {
            return None;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    let mut bytes = vec![0u8; len as usize];
    // SAFETY: `len` committed, readable bytes of title memory (checked
    // just above); the title may write them meanwhile, which a diagnostic
    // accepts.
    unsafe { std::ptr::copy_nonoverlapping(start as *const u8, bytes.as_mut_ptr(), bytes.len()) };
    Some(Snapshot { bytes })
}

/// Records, in `list` where it stands, a copy of `len` bytes of the host
/// copy `s` at title `address` into the frame's read-back buffer.
fn copy(
    f: &mut Frame,
    list: &HostList,
    s: &crate::shadow::Shadow,
    address: u64,
    len: u64,
) -> Result<(u64, u64), &'static str> {
    let host = crate::host::host().ok_or("no host")?;
    if f.readback.is_none() {
        f.readback = Some(readback_buffer(&host.device).map_err(|_| "no read-back buffer")?);
    }
    let at = f.used.next_multiple_of(16);
    if at + len > budget() {
        return Err("read-back budget spent");
    }
    f.used = at + len;
    let readback = f.readback.as_ref().ok_or("no read-back buffer")?;
    let (common, source) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    // SAFETY: an open list; the host copy and our read-back buffer, which
    // the list keeps, the ranges inside both. Host copies are taken as
    // common, as `shadow` takes them (and `passdump::sample_buffer`).
    unsafe {
        list.list.ResourceBarrier(&[
            crate::host::uav_barrier(),
            crate::host::transition(&s.buffer, common, source),
        ]);
        list.list
            .CopyBufferRegion(readback, at, &s.buffer, s.offset(address), len);
        list.list
            .ResourceBarrier(&[crate::host::transition(&s.buffer, source, common)]);
    }
    list.keep(&s.buffer);
    list.keep(readback);
    Ok((at, len))
}

fn readback_buffer(device: &ID3D12Device) -> windows::core::Result<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: budget(),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; the out pointer is ours.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut buffer,
        )?;
    }
    let buffer = buffer.ok_or_else(windows::core::Error::empty)?;
    crate::host::name(&buffer, || "XTP_FRAME_PROBE read-back".to_owned());
    Ok(buffer)
}

/// `draw::readable`: an op being prepared in `list` reads (or, with
/// `write`, writes through unordered access) the title texture `resource`.
pub(crate) fn texture(list: &HostList, resource: This, write: bool) {
    let Some(frame) = recording() else {
        return;
    };
    let Some((data, desc)) = crate::resource::texture_memory(resource) else {
        return;
    };
    let title_format = desc.format();
    if !write && title_format != R9G9B9E5 {
        return;
    }
    let use_ = TextureUse {
        list: key(list),
        after: SUBMISSIONS.load(Ordering::Relaxed),
        program: PROGRAM.with(|p| p.borrow().clone()),
        address: data as u64,
        title_format,
        host: host_text(resource),
        write,
    };
    frame_mut(&mut frames(), frame).textures.push(use_);
}

/// Counts submissions that enable this command's predicate. The list can be
/// discarded or submitted again.
pub(crate) fn decoding(list: &HostList, resource: This) -> Option<std::sync::Arc<AtomicU64>> {
    let frame = recording()?;
    let (address, _) = crate::resource::texture_memory(resource)?;
    let count = std::sync::Arc::new(AtomicU64::new(0));
    frame_mut(&mut frames(), frame).decodings.push((
        address as u64,
        key(list),
        SUBMISSIONS.load(Ordering::Relaxed),
        count.clone(),
    ));
    Some(count)
}

/// `Host::submit`: a title submission of `runs`, in order, before its
/// refreshes. Takes the probed ranges' title memory for the ops of these
/// lists, and reports the frames presented since, once what was submitted
/// so far has run.
pub(crate) fn submitted(runs: &[&HostList]) {
    if programs().is_empty() {
        return;
    }
    let number = SUBMISSIONS.fetch_add(1, Ordering::Relaxed) + 1;
    let lists: Vec<usize> = runs.iter().map(|l| key(l)).collect();
    let presented = crate::queue::presented();
    let done: Vec<Frame> = {
        let mut frames = frames();
        // Before this submission is counted: what `Host::after` waits for
        // below is the submissions made so far, not this one. The title
        // records a frame's lists while the one before is presented, so
        // a frame is ready once every op it probed has had its list
        // submitted, or [`LATE`] presents after it at most.
        let (done, kept) = frames.drain(..).partition(|f: &Frame| {
            f.frame <= presented && f.ops.iter().all(|o| o.submission.is_some())
                || presented >= f.frame + LATE
        });
        *frames = kept;
        for f in frames.iter_mut() {
            f.submissions.push((number, lists.clone()));
            for op in f
                .ops
                .iter_mut()
                .filter(|o| o.submission.is_none() && lists.contains(&o.list))
            {
                op.submission = Some(number);
                for r in &mut op.reads {
                    if let Some((address, len)) = r.range {
                        r.at_submit = snapshot(address, len.min(bytes()));
                    }
                }
            }
        }
        done
    };
    if done.is_empty() {
        return;
    }
    let Some(host) = crate::host::host() else {
        return;
    };
    for f in done {
        host.after(move || write_report(f));
    }
}

fn write_report(f: Frame) {
    let Some(folder) = crate::capture::folder() else {
        return;
    };
    let gpu = f.readback.as_ref().and_then(|r| {
        let mut p = std::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: f.used as usize,
        };
        // SAFETY: our read-back buffer, written by lists that have run.
        unsafe { r.Map(0, Some(&range), Some(&mut p)) }.ok()?;
        // SAFETY: the mapping holds `used` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), f.used as usize) }.to_vec();
        // SAFETY: mapped just above; nothing written.
        unsafe { r.Unmap(0, Some(&D3D12_RANGE::default())) };
        Some(bytes)
    });
    let text = report(&f, gpu.as_deref());
    let path = folder.join(format!("probe-{}.txt", f.frame));
    let _ = std::fs::create_dir_all(folder);
    match std::fs::write(&path, text) {
        Ok(()) => tracing::info!(path = %path.display(), "frame probe written"),
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "frame probe not written"),
    }
}

/// The first submission after `after` that holds `list`.
fn submission_of(f: &Frame, list: usize, after: u64) -> Option<u64> {
    f.submissions
        .iter()
        .find(|(n, lists)| *n > after && lists.contains(&list))
        .map(|&(n, _)| n)
}

fn report(f: &Frame, gpu: Option<&[u8]>) -> String {
    let mut out = String::new();
    // Lists numbered in submission order, from 1.
    let mut numbers: HashMap<usize, usize> = HashMap::new();
    for (_, lists) in &f.submissions {
        for &l in lists {
            let n = numbers.len() + 1;
            numbers.entry(l).or_insert(n);
        }
    }
    let list_name = |list: usize, after: u64| {
        let sub = submission_of(f, list, after)
            .map_or_else(|| "not submitted".to_owned(), |n| format!("submission {n}"));
        match numbers.get(&list) {
            Some(n) => format!("L{n} ({sub})"),
            None => format!("list {list:#x} ({sub})"),
        }
    };
    let _ = writeln!(
        out,
        "XTP frame probe of frame {}: programs {:?}; {} ops probed of {} matching; title submissions {}..{}.",
        f.frame,
        programs(),
        f.ops.len(),
        f.matched,
        f.submissions.first().map_or(0, |s| s.0),
        f.submissions.last().map_or(0, |s| s.0),
    );
    let _ = writeln!(
        out,
        "Bytes: title memory when the op was recorded, when its list was submitted (before the submission's refresh), and the host copy on the GPU just before the op; words in hex, little-endian."
    );
    let _ = writeln!(out, "\n== Ops");
    for (i, op) in f.ops.iter().enumerate() {
        let _ = writeln!(
            out,
            "op {} {} {}{}",
            i + 1,
            match (op.submission, numbers.get(&op.list)) {
                (Some(s), Some(n)) => format!("L{n} (submission {s})"),
                _ => format!("list {:#x} (not submitted)", op.list),
            },
            op.program,
            if op.dropped {
                " DROPPED (no host pipeline): what it would read and write"
            } else {
                ""
            }
        );
        for r in &op.reads {
            let _ = writeln!(out, "    {}", r.text);
            let Some((_, len)) = r.range else {
                continue;
            };
            let record = r.at_record.as_ref();
            let _ = writeln!(
                out,
                "      at record: {}",
                record.map_or_else(|| "not readable".to_owned(), |s| s.text_shown(len, r.shown))
            );
            let submit = r.at_submit.as_ref();
            let same = |a: Option<&Snapshot>, b: Option<&Snapshot>| {
                a.zip(b).is_some_and(|(a, b)| a.bytes == b.bytes)
            };
            let _ = writeln!(
                out,
                "      at submit: {}",
                match submit {
                    None => "not taken".to_owned(),
                    Some(_) if same(submit, record) => "as at record".to_owned(),
                    Some(s) => s.text_shown(len, r.shown),
                }
            );
            let ran = op.submission.is_some();
            let on_gpu = match (&r.gpu, gpu) {
                (Err(why), _) => Err(*why),
                // Its list never ran: the read-back bytes are not its.
                (Ok(_), _) if !ran => Err("its list was not submitted"),
                (Ok(_), None) => Err("not read back"),
                (Ok((at, n)), Some(bytes)) => bytes
                    .get(*at as usize..(at + n) as usize)
                    .map(|b| Snapshot { bytes: b.to_vec() })
                    .ok_or("not read back"),
            };
            let _ = writeln!(
                out,
                "      on the GPU: {}",
                match &on_gpu {
                    Err(why) => (*why).to_owned(),
                    Ok(s) if same(Some(s), submit) => "as at submit".to_owned(),
                    Ok(s) => {
                        let differ = submit.map_or(0, |t| {
                            t.bytes.iter().zip(&s.bytes).filter(|(a, b)| a != b).count()
                        });
                        format!(
                            "{} ({differ} bytes differ from submit)",
                            s.text_shown(len, r.shown)
                        )
                    }
                }
            );
        }
    }
    let _ = writeln!(out, "\n== R9G9B9E5 reads and the writes at their address");
    let mut seen: Vec<u64> = Vec::new();
    for t in f.textures.iter().filter(|t| !t.write) {
        if seen.contains(&t.address) {
            continue;
        }
        seen.push(t.address);
        let _ = writeln!(
            out,
            "texture {:#x} (title format {}, {}):",
            t.address, t.title_format, t.host
        );
        let _ = writeln!(
            out,
            "  first read by {} in {}",
            t.program,
            list_name(t.list, t.after),
        );
        // Each program and list once, with how many uses.
        let mut uses: Vec<(bool, &str, usize, u64, u32, usize)> = Vec::new();
        for u in f.textures.iter().filter(|u| u.address == t.address) {
            match uses
                .iter_mut()
                .find(|x| x.0 == u.write && x.1 == u.program && x.2 == u.list && x.3 == u.after)
            {
                Some(x) => x.5 += 1,
                None => uses.push((u.write, &u.program, u.list, u.after, u.title_format, 1)),
            }
        }
        for &(write, program, list, after, format, n) in &uses {
            let _ = writeln!(
                out,
                "  {} by {program} in {} ({n} uses, title format {format})",
                if write { "written" } else { "read" },
                list_name(list, after)
            );
        }
        for (_, list, after, count) in f.decodings.iter().filter(|d| d.0 == t.address) {
            let _ = writeln!(
                out,
                "  decode in {}: {} executions",
                list_name(*list, *after),
                count.load(Ordering::Relaxed)
            );
        }
    }
    let _ = writeln!(out, "\n== Title submissions");
    for (n, lists) in &f.submissions {
        let names: Vec<String> = lists
            .iter()
            .map(|l| {
                numbers
                    .get(l)
                    .map_or_else(|| format!("{l:#x}"), |n| format!("L{n}"))
            })
            .collect();
        let _ = writeln!(out, "submission {n}: {}", names.join(" "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_parse() {
        assert_eq!(
            parse("db4835a364449a45, 1CD9BC634417A981,short,zz4835a364449a45"),
            vec!["db4835a364449a45", "1cd9bc634417a981"]
        );
        assert!(parse("").is_empty());
    }

    #[test]
    fn snapshots_count_and_show_words() {
        let s = Snapshot {
            bytes: vec![1, 0, 0, 0, 0, 0, 0, 0, 0xff, 0x80, 0, 1],
        };
        assert_eq!(
            s.text(12),
            "non-zero 4 of 12 bytes, from +0x0 to +0xb [00000001 00000000 010080ff]"
        );
        assert_eq!(
            s.text(64),
            "non-zero 4 of 12 bytes (first 12 of 64), from +0x0 to +0xb [00000001 00000000 010080ff]"
        );
        let zero = Snapshot { bytes: vec![0; 8] };
        assert_eq!(zero.text(8), "non-zero 0 of 8 bytes [00000000 00000000]");
    }

    /// Non-zero bytes past the words shown are shown from the first of
    /// them, 16-byte aligned.
    #[test]
    fn snapshots_show_their_first_non_zero_bytes() {
        let mut bytes = vec![0u8; 1024];
        bytes[0x2bc] = 7;
        bytes[0x2bf] = 1;
        let text = Snapshot { bytes }.text(1024);
        assert!(
            text.starts_with("non-zero 2 of 1024 bytes, from +0x2bc to +0x2bf [00000000"),
            "{text}"
        );
        let tail = text.split_once("; at +0x2b0: [").unwrap().1;
        let shown: Vec<&str> = tail.trim_end_matches(']').split(' ').collect();
        assert_eq!(shown.len(), SHOWN_AT_FIRST / 4);
        assert_eq!(
            &shown[..4],
            ["00000000", "00000000", "00000000", "01000007"]
        );
    }

    /// A constant view is read over its whole size, and shown in full up to
    /// [`SHOWN_CONSTANTS`] bytes.
    #[test]
    fn constant_views_are_read_and_shown_whole() {
        let view = crate::descriptor::View::Constant(0x9_9276_ed80, 3424);
        assert_eq!(span_of(&view), Some((0x9_9276_ed80, 3424)));
        let mut bytes = vec![0u8; 3424];
        bytes[3420] = 1;
        let s = Snapshot { bytes };
        let words_shown = |t: &str| {
            t.split_once(" [")
                .unwrap()
                .1
                .split(']')
                .next()
                .unwrap()
                .split(' ')
                .filter(|w| w.len() == 8)
                .count()
        };
        assert_eq!(words_shown(&s.text_shown(3424, SHOWN_CONSTANTS)), 3424 / 4);
        assert_eq!(words_shown(&s.text(3424)), SHOWN / 4);
    }

    #[test]
    fn dir_light_colours_parse() {
        let one = |r, g, b, scale| DirLight {
            colour: [r, g, b],
            scale,
        };
        assert_eq!(
            parse_dir_light("0.1,0.12,0.2"),
            vec![one(0.1, 0.12, 0.2, 1.0)]
        );
        assert_eq!(
            parse_dir_light(" 1, 1.2 ,2 ,0.5/0,0,0"),
            vec![one(1.0, 1.2, 2.0, 0.5), one(0.0, 0.0, 0.0, 1.0)]
        );
        // One bad colour turns the switch off.
        for bad in [
            "",
            "1,2",
            "1,2,3,4,5",
            "1,x,3",
            "1,2,3/-1,0,0",
            "inf,0,0",
            "1,2,3,NaN",
        ] {
            assert!(parse_dir_light(bad).is_empty(), "{bad}");
        }
    }

    /// Colours take turns, `frames` frames each, from frame 0.
    #[test]
    fn dir_light_colours_take_turns() {
        let lights: Vec<DirLight> = (0..3)
            .map(|i| DirLight {
                colour: [i as f32; 3],
                scale: 1.0,
            })
            .collect();
        let at = |frame| dir_light_for(&lights, 4, frame).unwrap().colour[0];
        let seen: Vec<f32> = [0, 3, 4, 7, 8, 11, 12].into_iter().map(at).collect();
        assert_eq!(seen, [0.0, 0.0, 1.0, 1.0, 2.0, 2.0, 0.0]);
        // Captures 4 frames apart see each colour in turn.
        let captured: Vec<f32> = [617, 621, 625].into_iter().map(at).collect();
        assert_eq!(captured, [1.0, 2.0, 0.0]);
        assert_eq!(dir_light_for(&[], 4, 7), None);
        assert_eq!(dir_light_for(&lights, 0, 2).unwrap().colour[0], 2.0);
    }

    #[test]
    fn size_switches_parse_and_clamp() {
        assert_eq!(sized(None, 64, 16 << 10, 1 << 10), 64 << 10);
        assert_eq!(sized(Some(" 2048 "), 64, 16 << 10, 1 << 10), 2 << 20);
        assert_eq!(sized(Some("0"), 64, 16 << 10, 1 << 10), 1 << 10);
        assert_eq!(sized(Some("99999999"), 64, 16 << 10, 1 << 10), 16 << 20);
        assert_eq!(sized(Some("2M"), 48, 512, 1 << 20), 48 << 20);
    }

    #[test]
    fn title_memory_reads_back() {
        let data: Vec<u8> = (0..64).collect();
        let s = snapshot(data.as_ptr() as u64, 64).unwrap();
        assert_eq!(s.bytes, data);
        assert!(snapshot(0x10, 16).is_none());
    }
}
