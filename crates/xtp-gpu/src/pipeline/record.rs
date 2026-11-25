//! The graphics and mesh pipelines sessions made, kept beside the pipeline
//! library so that `xtp precook` makes them again before the next session
//! ([`super::precook`]). After a translator change every host pipeline has
//! a new name in the library (its name hashes the translated code), and the
//! precook could make only the compute ones, which need their container
//! alone: the others need the title's graphics state, which only a session
//! sees. A Series S session then dropped some 970 draws a frame "being
//! made" while its world loaded (run `20261125-022031`).
//!
//! - What is kept: each title pipeline with a graphics state that a
//!   pipeline worker made or tried to make (a first draw with it, or a start
//!   at creation from the early list), whatever became of it: its shaders'
//!   containers and the subobjects of the title's stream as the title gave
//!   them, in its order. A subobject without a pointer is kept byte for
//!   byte, the input layout with its semantic names; the root signature is
//!   ours (the translations' bindings define it) and the stream output,
//!   cached blob and view instancing subobjects are not taken in. The
//!   precook gives them to `host::GraphicsState::read` again, so that the
//!   state, its description, the host pipeline's name in the library and
//!   the pipeline's early key are the session's, as this build reads them
//!   (the depth bounds too, once `read` takes them in).
//! - Not kept per translator: a description is the title's, the same for
//!   every translator, and a file per translator would be empty exactly
//!   when the precook needs it, after a change. The precook skips what no
//!   longer translates.
//! - Compute pipelines are not kept: the precook makes every compute
//!   program of the pack from its container alone.
//! - The file, [`FILE`]: versioned ([`MAGIC`]) and append-only, a list of
//!   entries: a container, by its BLAKE3, or a pipeline, by its early key,
//!   each once. An entry is written whole by one call and carries its
//!   length and the start of its digest. The part of an entry a run left
//!   when it ended while writing is cut off before the next entry; an entry
//!   that does not read back (its digest) is skipped by the precook. The
//!   file stops growing at [`MAX_BYTES`]. Workers write it, never the
//!   title's threads.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::{
    D3D12_INPUT_CLASSIFICATION, D3D12_INPUT_ELEMENT_DESC, D3D12_INPUT_LAYOUT_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::PCSTR;

use super::Shader;
use super::host::GraphicsState;

/// The file, in the pipeline cache's folder.
pub(crate) const FILE: &str = "recorded-pipelines.bin";
/// The file's first bytes: its format, version 1. A file of another
/// version is left as it is, and nothing is kept in it.
const MAGIC: &[u8; 8] = b"XTPREC\x00\x01";
/// The file stops growing at this size (a session's pipelines take a few
/// megabytes, their containers some ten).
const MAX_BYTES: u64 = 1 << 30;

/// Entry tags: a shader container, a pipeline.
const CONTAINER: u8 = b'S';
const PIPELINE: u8 = b'P';
/// An entry's head: its tag, its payload's length (u32) and the first
/// [`CHECK`] bytes of the payload's BLAKE3.
const CHECK: usize = 8;
const HEAD: usize = 1 + 4 + CHECK;

/// The stages a kept pipeline's shaders are of, by their number in the
/// file.
const STAGES: [&str; 8] = [
    "vertex",
    "pixel",
    "domain",
    "hull",
    "geometry",
    "compute",
    "amplification",
    "mesh",
];

/// The input layout's subobject type (its elements are behind a pointer).
const INPUT_LAYOUT: u32 = 12;

/// Whether a subobject of `kind` is kept byte for byte: of a known size
/// (`super::subobject`) and without pointers. Root signature (0), shaders
/// (1 to 6, 23, 24), stream output (7), input layout (12, kept apart),
/// cached blob (19) and view instancing (22) hold pointers.
fn as_is(kind: u32) -> bool {
    matches!(kind, 8..=11 | 13..=18 | 20 | 21)
}

/// The graphics subobjects of a title's pipeline stream, as kept: each its
/// type (u8), its length (u16) and its data, in the stream's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stream(Box<[u8]>);

/// A stream's subobjects being taken in, at the title's creation call
/// ([`Capture::take`]).
#[derive(Debug, Default)]
pub(crate) struct Capture {
    bytes: Vec<u8>,
    /// No subobject was left out (the stream is kept only then).
    whole: bool,
}

impl Capture {
    pub(crate) fn new() -> Capture {
        Capture {
            whole: true,
            ..Capture::default()
        }
    }

    /// Takes in one subobject of the title's stream: its type `kind`, at
    /// `at`, `size` bytes with its type and padding (`super::subobject`).
    ///
    /// # Safety
    /// `at` points at a whole subobject of type `kind` and `size` bytes.
    pub(crate) unsafe fn take(&mut self, kind: u32, at: *const u8, size: usize) {
        if kind == INPUT_LAYOUT {
            // SAFETY: an input layout subobject holds its description at +8.
            let layout = unsafe { at.add(8).cast::<D3D12_INPUT_LAYOUT_DESC>().read_unaligned() };
            if layout.pInputElementDescs.is_null() {
                // `GraphicsState::read` takes nothing in either.
                return;
            }
            // SAFETY: the title passes this many elements.
            let elements = unsafe {
                std::slice::from_raw_parts(layout.pInputElementDescs, layout.NumElements as usize)
            };
            let mut data = (elements.len() as u32).to_le_bytes().to_vec();
            for e in elements {
                // SAFETY: each element names its semantic with a C string.
                let name = unsafe { CStr::from_ptr(e.SemanticName.0.cast()) }.to_bytes();
                let Ok(len) = u16::try_from(name.len()) else {
                    self.whole = false;
                    return;
                };
                data.extend(len.to_le_bytes());
                data.extend(name);
                for v in [
                    e.SemanticIndex,
                    e.Format.0 as u32,
                    e.InputSlot,
                    e.AlignedByteOffset,
                    e.InputSlotClass.0 as u32,
                    e.InstanceDataStepRate,
                ] {
                    data.extend(v.to_le_bytes());
                }
            }
            self.push(kind, &data);
        } else if as_is(kind) && size > 4 {
            // SAFETY: the subobject's data follows its type, `size` in all.
            let data = unsafe { std::slice::from_raw_parts(at.add(4), size - 4) };
            self.push(kind, data);
        }
    }

    fn push(&mut self, kind: u32, data: &[u8]) {
        let (Ok(kind), Ok(len)) = (u8::try_from(kind), u16::try_from(data.len())) else {
            self.whole = false;
            return;
        };
        self.bytes.push(kind);
        self.bytes.extend(len.to_le_bytes());
        self.bytes.extend(data);
    }

    /// The stream as kept; `None` if a subobject could not be kept.
    pub(crate) fn finish(self) -> Option<Stream> {
        self.whole.then(|| Stream(self.bytes.into_boxed_slice()))
    }
}

impl Stream {
    /// The graphics state the stream describes: each subobject given to
    /// `GraphicsState::read` again, in the stream's order, as the title's
    /// creation call gave it; `None` if one does not decode.
    pub(crate) fn state(&self) -> Option<GraphicsState> {
        let mut g = GraphicsState::default();
        let mut r = Reader(&self.0);
        while !r.0.is_empty() {
            let kind = u32::from(r.u8()?);
            let len = usize::from(r.u16()?);
            let data = r.bytes(len)?;
            if kind == INPUT_LAYOUT {
                let (_names, elements) = input_layout(data)?;
                let mut sub = vec![0u8; 24];
                sub[..4].copy_from_slice(&kind.to_le_bytes());
                let desc = D3D12_INPUT_LAYOUT_DESC {
                    pInputElementDescs: elements.as_ptr(),
                    NumElements: elements.len() as u32,
                };
                // SAFETY: 16 bytes at +8 of a 24-byte buffer.
                unsafe {
                    sub.as_mut_ptr()
                        .add(8)
                        .cast::<D3D12_INPUT_LAYOUT_DESC>()
                        .write_unaligned(desc);
                }
                // SAFETY: a whole input layout subobject, its elements and
                // their names alive while `read` copies them.
                unsafe { g.read(kind, sub.as_ptr()) };
                continue;
            }
            let (size, None) = super::subobject(kind)? else {
                return None;
            };
            if !as_is(kind) || len + 4 != size {
                return None;
            }
            let mut sub = kind.to_le_bytes().to_vec();
            sub.extend(data);
            // SAFETY: a whole subobject of this kind: its type, then its
            // `size - 4` bytes of data.
            unsafe { g.read(kind, sub.as_ptr()) };
        }
        Some(g)
    }
}

/// An input layout as kept: its names, and its elements pointing at them.
fn input_layout(data: &[u8]) -> Option<(Vec<CString>, Vec<D3D12_INPUT_ELEMENT_DESC>)> {
    let mut r = Reader(data);
    let n = r.u32()? as usize;
    // D3D12 takes 32 elements at most.
    if n > 64 {
        return None;
    }
    let mut names = Vec::with_capacity(n);
    let mut fields = Vec::with_capacity(n);
    for _ in 0..n {
        let len = usize::from(r.u16()?);
        names.push(CString::new(r.bytes(len)?).ok()?);
        let mut f = [0u32; 6];
        for v in &mut f {
            *v = r.u32()?;
        }
        fields.push(f);
    }
    if !r.0.is_empty() {
        return None;
    }
    let elements = names
        .iter()
        .zip(&fields)
        .map(|(name, f)| D3D12_INPUT_ELEMENT_DESC {
            SemanticName: PCSTR(name.as_ptr().cast()),
            SemanticIndex: f[0],
            Format: DXGI_FORMAT(f[1] as i32),
            InputSlot: f[2],
            AlignedByteOffset: f[3],
            InputSlotClass: D3D12_INPUT_CLASSIFICATION(f[4] as i32),
            InstanceDataStepRate: f[5],
        })
        .collect();
    Some((names, elements))
}

/// Little-endian fields read off the front of a byte slice.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.0.len() {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.bytes(N)?.try_into().ok()
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.array()?))
    }
}

/// An entry of the file: its head, then `payload`.
fn entry(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEAD + payload.len());
    out.push(tag);
    out.extend((payload.len() as u32).to_le_bytes());
    out.extend(&blake3::hash(payload).as_bytes()[..CHECK]);
    out.extend(payload);
    out
}

/// A pipeline's key as kept: its early key's 32 hexadecimal digits.
fn key_bytes(key: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    if key.len() != 32 {
        return None;
    }
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(key.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A pipeline entry's payload: its key, its shaders (stage number and
/// container BLAKE3 each), then its stream.
fn pipeline_payload(key: &[u8; 16], shaders: &[(u8, [u8; 32])], stream: &Stream) -> Vec<u8> {
    let mut out = key.to_vec();
    out.push(shaders.len() as u8);
    for (stage, hash) in shaders {
        out.push(*stage);
        out.extend(hash);
    }
    out.extend(&*stream.0);
    out
}

/// The session's file, open for appending, and what it holds.
struct Writer {
    file: File,
    path: PathBuf,
    end: u64,
    containers: HashSet<[u8; 32]>,
    pipelines: HashSet<[u8; 16]>,
    /// Nothing more is written (a write failed, or the file is full).
    stopped: bool,
}

impl Writer {
    /// The file at `path`, made if there is none, with the part of an
    /// entry a run left at its end cut off.
    fn open(path: &Path) -> std::io::Result<Writer> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let len = file.metadata()?.len();
        let mut writer = Writer {
            file,
            path: path.to_owned(),
            end: MAGIC.len() as u64,
            containers: HashSet::new(),
            pipelines: HashSet::new(),
            stopped: false,
        };
        let mut magic = [0u8; MAGIC.len()];
        let head = len.min(MAGIC.len() as u64) as usize;
        writer.file.read_exact(&mut magic[..head])?;
        if magic[..head] != MAGIC[..head] {
            return Err(std::io::Error::other(
                "another format (another version of XTP): nothing is kept in it",
            ));
        }
        if head < MAGIC.len() {
            // New, or its magic cut short.
            writer.file.set_len(0)?;
            writer.file.seek(SeekFrom::Start(0))?;
            writer.file.write_all(MAGIC)?;
            return Ok(writer);
        }
        let mut reader = BufReader::new(&writer.file);
        let mut at = MAGIC.len() as u64;
        while at + HEAD as u64 <= len {
            let mut head = [0u8; HEAD];
            reader.read_exact(&mut head)?;
            let n = u64::from(u32::from_le_bytes(
                head[1..5].try_into().unwrap_or_default(),
            ));
            let end = at + HEAD as u64 + n;
            if end > len {
                break;
            }
            match head[0] {
                CONTAINER if n >= 32 => {
                    let mut hash = [0u8; 32];
                    reader.read_exact(&mut hash)?;
                    writer.containers.insert(hash);
                    reader.seek_relative(n as i64 - 32)?;
                }
                PIPELINE if n >= 16 => {
                    let mut key = [0u8; 16];
                    reader.read_exact(&mut key)?;
                    writer.pipelines.insert(key);
                    reader.seek_relative(n as i64 - 16)?;
                }
                _ => break,
            }
            at = end;
        }
        drop(reader);
        if at < len {
            tracing::info!(
                bytes = len - at,
                path = %path.display(),
                "recorded pipelines: the end of an entry left unfinished is cut off"
            );
            writer.file.set_len(at)?;
        }
        writer.file.seek(SeekFrom::Start(at))?;
        writer.end = at;
        Ok(writer)
    }

    /// Appends the entries in `bytes`, or stops writing if that fails or
    /// the file would pass [`MAX_BYTES`]: whether they were written.
    fn append(&mut self, bytes: &[u8]) -> bool {
        if self.stopped {
            return false;
        }
        if self.end + bytes.len() as u64 > MAX_BYTES {
            tracing::warn!(
                path = %self.path.display(),
                bytes = self.end,
                "recorded pipelines: the file is full; no more pipelines are kept"
            );
            self.stopped = true;
            return false;
        }
        match self.file.write_all(bytes) {
            Ok(()) => {
                self.end += bytes.len() as u64;
                true
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.path.display(),
                    "recorded pipelines: not written; no more pipelines are kept"
                );
                // The next run cuts off what this one left half written.
                self.stopped = true;
                false
            }
        }
    }
}

/// The session's file, opened on first use; `None` without a pipeline
/// cache folder, or if it cannot be.
fn writer() -> Option<&'static Mutex<Writer>> {
    static WRITER: OnceLock<Option<Mutex<Writer>>> = OnceLock::new();
    WRITER
        .get_or_init(|| {
            let path = super::library::dir()?.join(FILE);
            match Writer::open(&path) {
                Ok(w) => {
                    tracing::info!(
                        pipelines = w.pipelines.len(),
                        containers = w.containers.len(),
                        bytes = w.end,
                        "recorded pipelines: kept so far"
                    );
                    Some(Mutex::new(w))
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        "recorded pipelines: no file; this run's pipelines are not kept"
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Pipelines kept since the start.
static KEPT: AtomicU64 = AtomicU64::new(0);

/// Pipelines kept since the start (for the library's line).
pub(crate) fn kept() -> u64 {
    KEPT.load(Ordering::Relaxed)
}

/// `shaders` as a pipeline entry lists them: stage number, container
/// BLAKE3; `None` for a stage the file has no number for.
fn listed(shaders: &[Arc<Shader>]) -> Option<Vec<(u8, [u8; 32])>> {
    shaders
        .iter()
        .map(|s| {
            let stage = STAGES.iter().position(|&n| n == s.stage)?;
            Some((stage as u8, *blake3::hash(&s.container).as_bytes()))
        })
        .collect()
}

impl Writer {
    /// Keeps the pipeline of early key `key`, of `shaders` (`listed` as
    /// [`listed`] lists them) and `stream`, unless it is kept already,
    /// after those of its containers that are not: whether it was written
    /// now.
    fn keep(
        &mut self,
        key: [u8; 16],
        shaders: &[Arc<Shader>],
        listed: &[(u8, [u8; 32])],
        stream: &Stream,
    ) -> bool {
        if self.pipelines.contains(&key) {
            return false;
        }
        // Its containers first: a pipeline entry is only read back with
        // them.
        let mut bytes = Vec::new();
        let mut new = Vec::new();
        for ((_, hash), s) in listed.iter().zip(shaders) {
            if !self.containers.contains(hash) && !new.contains(hash) {
                let mut payload = hash.to_vec();
                payload.extend(&s.container);
                bytes.extend(entry(CONTAINER, &payload));
                new.push(*hash);
            }
        }
        bytes.extend(entry(PIPELINE, &pipeline_payload(&key, listed, stream)));
        if !self.append(&bytes) {
            return false;
        }
        self.containers.extend(new);
        self.pipelines.insert(key);
        true
    }
}

/// A title pipeline with the graphics state `stream` was made, or tried:
/// its description is kept, under its early `key`, unless it is already,
/// with those of its shaders' containers that are not.
pub(crate) fn made(key: &str, shaders: &[Arc<Shader>], stream: &Stream) {
    let Some(key) = key_bytes(key) else {
        return;
    };
    let Some(writer) = writer() else {
        return;
    };
    let lock = || writer.lock().unwrap_or_else(|e| e.into_inner());
    if lock().pipelines.contains(&key) {
        return;
    }
    // Hashed outside the lock: a container takes up to some 100 KiB.
    let Some(listed) = listed(shaders) else {
        return;
    };
    if lock().keep(key, shaders, &listed, stream) {
        KEPT.fetch_add(1, Ordering::Relaxed);
    }
}

/// A kept pipeline: its shaders (stage, container BLAKE3) and its stream.
pub(crate) struct Kept {
    pub(crate) shaders: Vec<(&'static str, [u8; 32])>,
    pub(crate) stream: Stream,
}

/// What the file holds, as the precook reads it.
#[derive(Default)]
pub(crate) struct Contents {
    pub(crate) pipelines: Vec<Kept>,
    containers: HashMap<[u8; 32], Vec<u8>>,
    /// Entries that do not read back (their digest, or what they hold).
    pub(crate) damaged: usize,
    /// The file's size.
    pub(crate) bytes: u64,
}

impl Contents {
    /// The container whose BLAKE3 is `hash`, if the file holds it.
    pub(crate) fn container(&self, hash: &[u8; 32]) -> Option<&[u8]> {
        self.containers.get(hash).map(Vec::as_slice)
    }
}

/// Reads the file in `dir`: empty if there is none.
pub(crate) fn read(dir: &Path) -> Result<Contents, String> {
    let path = dir.join(FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Contents::default()),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// What the file's `bytes` hold.
fn parse(bytes: &[u8]) -> Result<Contents, String> {
    let mut out = Contents {
        bytes: bytes.len() as u64,
        ..Contents::default()
    };
    if bytes.is_empty() {
        return Ok(out);
    }
    let mut r = Reader(bytes);
    if r.bytes(MAGIC.len()) != Some(MAGIC) {
        return Err("another format (another version of XTP)".into());
    }
    while r.0.len() >= HEAD {
        let tag = r.u8().unwrap_or_default();
        let n = r.u32().unwrap_or_default() as usize;
        let check = r.array::<CHECK>().unwrap_or_default();
        let Some(payload) = r.bytes(n) else {
            break;
        };
        if blake3::hash(payload).as_bytes()[..CHECK] != check {
            out.damaged += 1;
            continue;
        }
        let mut p = Reader(payload);
        match tag {
            CONTAINER => match p.array::<32>() {
                Some(hash) => {
                    out.containers.insert(hash, p.0.to_vec());
                }
                None => out.damaged += 1,
            },
            PIPELINE => match pipeline(&mut p) {
                Some(kept) => out.pipelines.push(kept),
                None => out.damaged += 1,
            },
            _ => out.damaged += 1,
        }
    }
    // The part of an entry a run left when it ended while writing.
    if !r.0.is_empty() {
        out.damaged += 1;
    }
    Ok(out)
}

/// A pipeline entry's payload ([`pipeline_payload`]).
fn pipeline(p: &mut Reader<'_>) -> Option<Kept> {
    let _key = p.array::<16>()?;
    let n = p.u8()?;
    let shaders = (0..n)
        .map(|_| Some((*STAGES.get(usize::from(p.u8()?))?, p.array::<32>()?)))
        .collect::<Option<Vec<_>>>()?;
    Some(Kept {
        shaders,
        stream: Stream(p.0.into()),
    })
}

/// For tests: keeps a pipeline in the file in `dir` as a session's worker
/// does ([`made`]): whether it was written.
#[cfg(test)]
pub(crate) fn keep_in(dir: &Path, key: &str, shaders: &[Arc<Shader>], stream: &Stream) -> bool {
    let mut w = Writer::open(&dir.join(FILE)).expect("the file");
    w.keep(
        key_bytes(key).expect("a key"),
        shaders,
        &listed(shaders).expect("known stages"),
        stream,
    )
}

/// For tests: the stream of `subobjects`, each its type and the data after
/// it.
#[cfg(test)]
pub(crate) fn stream_of(subobjects: &[(u32, Vec<u8>)]) -> Stream {
    let mut c = Capture::new();
    for (kind, data) in subobjects {
        c.push(*kind, data);
    }
    c.finish().expect("short subobjects")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::S_OK;

    /// A stream as BTP 6 builds it (`super::super::tests`'s layout:
    /// title+0x86f090): root signature, a vertex and a pixel shader, blend,
    /// sample mask, rasterizer, depth-stencil 1, sample description, input
    /// layout (two elements), topology, render target and depth formats.
    /// The blend's and depth's padding hold garbage, as a title's may.
    struct TitleStream {
        bytes: Vec<u8>,
        _vs: Vec<u8>,
        _ps: Vec<u8>,
        _names: [CString; 2],
        _elements: Vec<D3D12_INPUT_ELEMENT_DESC>,
    }

    fn title_stream(salt: u8) -> TitleStream {
        let vs = vec![1, 2, 3, salt];
        let ps = vec![4u8; 5];
        let names = [c"POSITION".to_owned(), c"TEXCOORD".to_owned()];
        let elements = vec![
            D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(names[0].as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT(6), // R32G32B32_FLOAT
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION(0),
                InstanceDataStepRate: 0,
            },
            D3D12_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(names[1].as_ptr().cast()),
                SemanticIndex: 1,
                Format: DXGI_FORMAT(34), // R16G16_FLOAT
                InputSlot: 1,
                AlignedByteOffset: u32::MAX,
                InputSlotClass: D3D12_INPUT_CLASSIFICATION(1),
                InstanceDataStepRate: 1,
            },
        ];
        let mut s = vec![0xccu8; 0x2b0];
        for (at, kind) in [
            (0x0, 0),
            (0x10, 1),
            (0x28, 2),
            (0x40, 3),
            (0x58, 4),
            (0x70, 5),
            (0x88, 8),
            (0x1d8, 9),
            (0x1e0, 10),
            (0x210, 21),
            (0x250, 17),
            (0x260, 12),
            (0x278, 14),
            (0x280, 15),
            (0x2a8, 16),
        ] {
            words(&mut s, at, &[kind, 0]);
        }
        // No root signature; the vertex and pixel shaders' code, no domain,
        // hull or geometry shader.
        quad(&mut s, 0x8, 0);
        quad(&mut s, 0x18, vs.as_ptr() as u64);
        quad(&mut s, 0x20, vs.len() as u64);
        quad(&mut s, 0x30, ps.as_ptr() as u64);
        quad(&mut s, 0x38, ps.len() as u64);
        for at in [0x48, 0x50, 0x60, 0x68, 0x78, 0x80] {
            quad(&mut s, at, 0);
        }
        // Blend: alpha to coverage `salt`, no independent blend; target 0
        // blends one + one (the other targets' bytes are garbage).
        words(&mut s, 0x8c, &[u32::from(salt), 0]);
        words(&mut s, 0x94, &[1, 0, 2, 2, 1, 2, 1, 1, 4, 0x0f]);
        // Sample mask, then a rasterizer: solid, no culling.
        words(&mut s, 0x1dc, &[u32::MAX]);
        words(&mut s, 0x1e4, &[3, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        // Depth-stencil 1: depth on, writes, less-equal, no stencil; its
        // masks then garbage padding, the faces, depth bounds on.
        let d = 0x214;
        words(&mut s, d, &[1, 1, 4, 0]);
        s[d + 16..d + 20].copy_from_slice(&[0xff, 0xff, 0xab, 0xcd]);
        words(&mut s, d + 20, &[1, 1, 1, 8, 1, 1, 1, 8, 1]);
        // Sample description: one sample.
        words(&mut s, 0x254, &[1, 0]);
        // Input layout.
        quad(&mut s, 0x268, elements.as_ptr() as u64);
        quad(&mut s, 0x270, elements.len() as u64);
        // Topology type 3 (triangles), one render target of format 28,
        // depth format 45 (D24_UNORM_S8_UINT).
        words(&mut s, 0x27c, &[3]);
        words(&mut s, 0x284, &[28, 0, 0, 0, 0, 0, 0, 0, 1]);
        words(&mut s, 0x2ac, &[45]);
        TitleStream {
            bytes: s,
            _vs: vs,
            _ps: ps,
            _names: names,
            _elements: elements,
        }
    }

    fn words(s: &mut [u8], at: usize, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            s[at + 4 * i..at + 4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
    }

    fn quad(s: &mut [u8], at: usize, v: u64) {
        s[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// The title's pipeline from `stream`, through the device method.
    fn create(stream: &TitleStream) -> &'static super::super::Pipeline {
        let desc = super::super::StreamDesc {
            size: stream.bytes.len(),
            stream: stream.bytes.as_ptr(),
        };
        let mut p = std::ptr::null();
        let hr = super::super::create_stream(
            std::ptr::null(),
            &desc,
            &super::super::PIPELINE_IIDS[0],
            &mut p,
        );
        assert_eq!(hr, S_OK);
        super::super::pipeline(p)
    }

    /// The state rebuilt from what was kept is the one the title's stream
    /// gave: the same description (so the same name in the library) and
    /// the same early key, the input layout's names included.
    #[test]
    fn a_kept_stream_gives_the_titles_state_again() {
        let title = title_stream(0);
        let p = create(&title);
        let made = p.host.graphics.as_deref().unwrap();
        let stream = p.host.stream.as_ref().expect("the stream is kept");
        let again = stream.state().expect("it decodes");
        assert_eq!(again.describe(), made.describe());
        assert!(made.describe().contains("TEXCOORD"));
        let shaders = p.shaders.clone();
        assert_eq!(
            super::super::make::key(&shaders, Some(&again)),
            p.host.key,
            "the same early key"
        );
        // Another blend is another state.
        let other = title_stream(1);
        let q = create(&other);
        assert_ne!(
            q.host.stream.as_ref().unwrap().state().unwrap().describe(),
            made.describe()
        );
    }

    /// A stream whose subobject does not decode gives no state.
    #[test]
    fn damaged_streams_give_no_state() {
        let mut c = Capture::new();
        c.push(9, &[1, 2, 3]); // a sample mask of three bytes
        assert!(c.finish().unwrap().state().is_none());
        let mut c = Capture::new();
        c.push(0, &[0; 12]); // a root signature
        assert!(c.finish().unwrap().state().is_none());
        assert!(Stream(Box::new([12, 4, 0, 9, 0, 0, 0])).state().is_none());
        let mut c = Capture::new();
        c.push(9, &[0; 70_000]);
        assert!(c.finish().is_none(), "too long to keep");
        assert!(Capture::new().finish().unwrap().state().is_some());
    }

    #[test]
    fn keys_are_kept_as_bytes() {
        let key = "00112233445566778899aabbccddeeff";
        assert_eq!(
            key_bytes(key),
            Some([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ])
        );
        assert_eq!(key_bytes("0011"), None);
        assert_eq!(key_bytes(&"zz".repeat(16)), None);
    }

    fn shader(stage: &'static str, bytes: &[u8]) -> Arc<Shader> {
        super::super::shader(stage, bytes)
    }

    /// What a session keeps, a precook reads back: each pipeline once (by
    /// its key, also across runs), each container once, in a file a run
    /// left half written as well; another version's file is left alone.
    #[test]
    fn pipelines_are_kept_once_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let title = title_stream(7);
        let stream = {
            let p = create(&title);
            p.host.stream.clone().unwrap()
        };
        let (vs, ps) = (
            shader("vertex", b"record vs"),
            shader("pixel", b"record ps"),
        );
        let ps2 = shader("pixel", b"record ps 2");
        let keep = |w: &mut Writer, n: u8, shaders: &[Arc<Shader>]| {
            let key = key_bytes(&format!("{n:02x}").repeat(16)).unwrap();
            w.keep(key, shaders, &listed(shaders).unwrap(), &stream)
        };
        {
            let mut w = Writer::open(&path).unwrap();
            assert!(keep(&mut w, 1, &[vs.clone(), ps.clone()]));
            assert!(!keep(&mut w, 1, &[vs.clone(), ps.clone()]), "kept once");
            assert!(keep(&mut w, 2, &[vs.clone(), ps2.clone()]));
        }
        let read = super::read(dir.path()).unwrap();
        assert_eq!((read.pipelines.len(), read.damaged), (2, 0));
        assert_eq!(read.containers.len(), 3, "the vertex shader once");
        let first = &read.pipelines[0];
        assert_eq!(first.shaders[0].0, "vertex");
        assert_eq!(read.container(&first.shaders[1].1), Some(&b"record ps"[..]));
        assert_eq!(first.stream, stream);

        // A run that ended while writing: its half entry is cut off when
        // the next run opens the file, which knows what it holds.
        let whole = std::fs::metadata(&path).unwrap().len();
        let mut half = entry(PIPELINE, &pipeline_payload(&[9; 16], &[], &stream));
        half.truncate(half.len() / 2);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&half)
            .unwrap();
        assert_eq!(super::read(dir.path()).unwrap().damaged, 1);
        {
            let mut w = Writer::open(&path).unwrap();
            assert_eq!(w.end, whole, "cut off");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), whole);
            assert_eq!((w.pipelines.len(), w.containers.len()), (2, 3));
            assert!(
                !keep(&mut w, 2, &[vs.clone(), ps2.clone()]),
                "kept by the run before"
            );
            assert!(keep(&mut w, 3, std::slice::from_ref(&ps2)));
        }
        let read = super::read(dir.path()).unwrap();
        assert_eq!((read.pipelines.len(), read.damaged), (3, 0));
        assert_eq!(read.containers.len(), 3);

        // A damaged entry is skipped; the others still read.
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        let parsed = parse(&bytes).unwrap();
        assert_eq!((parsed.pipelines.len(), parsed.damaged), (2, 1));

        // Another version's file.
        std::fs::write(&path, b"XTPREC\x00\x02 and more").unwrap();
        assert!(Writer::open(&path).is_err());
        assert!(super::read(dir.path()).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"XTPREC\x00\x02 and more",
            "left as it is"
        );
        // No file: nothing kept.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(super::read(empty.path()).unwrap().pipelines.len(), 0);
    }
}
