//! `xtp precook`'s side in this crate: a shader container of the title's
//! pack, and a graphics or mesh pipeline an earlier session made, made
//! ready the way the title's first use of it makes it, through the same
//! calls, so that a session after a translator change starts with its
//! translations and its host pipelines already made.
//!
//! - Every container is translated for the host's wave sizes
//!   ([`crate::device::host_target`]) by [`crate::shader_cache`]'s
//!   `host_shader`, the function the title's pipelines call: same
//!   translation, same key, same entry. The title finds it on first use.
//! - A compute program's host pipeline is made from its translation alone
//!   (its root signature is the translation's bindings:
//!   `host::HostPipeline::new`), so it is made here too, by that function,
//!   which stores it into the host pipeline library as a first dispatch
//!   does; [`finish`] saves the library.
//! - Graphics and mesh pipelines need the title's graphics state as well,
//!   which only a session sees: sessions keep the description of each one
//!   the title creates ([`super::record`]), and [`replay`] makes it again as
//!   the session did, by the same function with the same shaders and state,
//!   into the same library entry. A description whose shaders do not
//!   translate any more is refused at once, as in a session. The file of
//!   descriptions is rewritten here once it grows past its bound
//!   (`record::compact`).
//! - The pack's effects ([`super::effects`]) name the shaders of each pass,
//!   not its state: their shader sets ([`ShaderSet`]) say how much of the
//!   title the kept descriptions cover ([`Recorded::shader_sets`]).
//! - What a whole pass depends on besides the pack and the translator, for
//!   the precook's note: the kept descriptions and the library file
//!   ([`cache`]).
//! - After a translator change, or once the library file has grown past
//!   [`LIBRARY_REBUILD_BYTES`], the precook makes the library a new
//!   generation ([`rebuild_library`]): what it makes or finds again, and
//!   nothing of the translators before.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use xtp_shader::{Container, FourCC, HwStage};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

/// What became of one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Its translation is in the cache (made now or found there: [`counts`]
    /// tells how many of each), and for a compute program with pipelines
    /// asked, its host pipeline in the library.
    Ready,
    /// The translator refuses it: the reason, kept in the cache as the
    /// title's first use would keep it.
    Refused(String),
    /// A compute program that gets no host pipeline (the reasons:
    /// [`refusals`]).
    NoPipeline,
}

/// What the translations are made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// The wave sizes translated for, as (min, max) lanes.
    pub lanes: (u32, u32),
    /// The key the translations are kept under (this build's translator
    /// and settings, and the target), in hexadecimal.
    pub key: String,
}

/// Makes the host device (the title's adapter: `XTP_ADAPTER` as in a run)
/// and says what translations are made for; why not, without a device.
pub fn start() -> Result<Ready, String> {
    crate::host::created().as_ref().map_err(Clone::clone)?;
    let target = crate::device::host_target();
    let key = crate::shader_cache::host_key();
    Ok(Ready {
        lanes: (target.lanes_min, target.lanes_max),
        key: key.iter().map(|b| format!("{b:02x}")).collect(),
    })
}

/// The stage of the program in `container` that a translation takes (its
/// first vertex, pixel, mesh or compute program, as `translate_for` takes
/// it); `None` if it holds none.
pub fn stage(container: &[u8]) -> Option<HwStage> {
    let c = Container::parse(container).ok()?;
    let part = c.part(FourCC::DXIL)?;
    xtp_shader::native_programs(part.data)
        .ok()?
        .into_iter()
        .map(|p| p.stage)
        .find(|s| matches!(s, HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs))
}

/// Precooks `container`, whose program is of `stage` ([`stage`]): its
/// translation into the shader cache and, with `pipelines`, a compute
/// program's host pipeline into the library.
pub fn container(container: &[u8], stage: Option<HwStage>, pipelines: bool) -> Outcome {
    let target = crate::device::host_target();
    if !(pipelines && stage == Some(HwStage::Cs)) {
        return match crate::shader_cache::host_shader(container, target) {
            Ok(_) => Outcome::Ready,
            Err(reason) => Outcome::Refused(reason),
        };
    }
    // As a compute pipeline the title creates: the shared container, then
    // its host pipeline, which translates it through the cache first.
    let shader = super::shader("compute", container);
    if super::host::HostPipeline::new(std::slice::from_ref(&shader), None).is_some() {
        return Outcome::Ready;
    }
    match shader.translated.get() {
        // The translator refused it: its reason, from the refusal just
        // kept (refusals count neither as made nor as found).
        Some(None) => match crate::shader_cache::host_shader(container, target) {
            Err(reason) => Outcome::Refused(reason),
            Ok(_) => Outcome::NoPipeline,
        },
        _ => Outcome::NoPipeline,
    }
}

/// What the precook made and found since the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// Shaders translated, and found in the cache (refusals are neither).
    pub translated: u64,
    pub cached: u64,
    /// Host pipelines compiled, and loaded from the library.
    pub made: u64,
    pub loaded: u64,
}

/// The counts so far.
pub fn counts() -> Counts {
    let (cached, translated) = crate::shader_cache::counts();
    Counts {
        translated,
        cached,
        made: super::library::MADE.load(Ordering::Relaxed),
        loaded: super::library::LOADED.load(Ordering::Relaxed),
    }
}

/// The host pipelines refused so far, by reason, most first.
pub fn refusals() -> Option<String> {
    super::refusals()
}

/// A library file past this size is made a new generation by the next
/// precook (the cache's had grown to 2.3 GB on 2026-11-25).
pub const LIBRARY_REBUILD_BYTES: u64 = 2 << 30;

/// What a precook's pipelines depend on besides the pack and the
/// translator: the descriptions sessions kept and the library file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cache {
    /// The BLAKE3 of the file of kept descriptions, in hexadecimal; `none`
    /// without one.
    pub recorded: String,
    /// The library file's size (0: none).
    pub library: u64,
}

/// The cache's state now ([`Cache`]); an empty one without a pipeline
/// cache folder.
pub fn cache() -> Result<Cache, String> {
    let Some(dir) = super::library::dir() else {
        return Ok(Cache {
            recorded: "none".into(),
            library: 0,
        });
    };
    Ok(Cache {
        recorded: digest_in(dir)?,
        library: super::library::file_bytes().unwrap_or(0),
    })
}

/// The BLAKE3 of the file of kept descriptions in `dir` now, in
/// hexadecimal; `none` without one.
fn digest_in(dir: &std::path::Path) -> Result<String, String> {
    let path = dir.join(super::record::FILE);
    match std::fs::File::open(&path) {
        Ok(file) => {
            let mut hasher = blake3::Hasher::new();
            hasher
                .update_reader(file)
                .map_err(|e| format!("reading {}: {e}", path.display()))?;
            Ok(hasher.finalize().to_hex().to_string())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("none".into()),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

/// Makes the library a new generation (see the module): to be asked
/// before any pipeline is made; false if that is too late.
pub fn rebuild_library() -> bool {
    super::library::rebuild()
}

/// The graphics and mesh pipelines sessions kept ([`super::record`]), each
/// once, ready to make again ([`replay`]).
pub struct Recorded {
    pipelines: Vec<Description>,
    /// The shader sets of the descriptions that read back, each once.
    pub shader_sets: HashSet<ShaderSet>,
    /// Kept descriptions that are not made again: the entries that do not
    /// read back, those whose state does not decode, those whose
    /// containers are not kept, and repeats (descriptions this build reads
    /// as the same pipeline).
    pub damaged: usize,
    pub missing: usize,
    pub repeated: usize,
    /// The file's size, and where it is (none without a pipeline cache).
    pub bytes: u64,
    pub path: Option<std::path::PathBuf>,
    /// The BLAKE3 of the file as read, in hexadecimal (`none` without
    /// one): what [`replay`] makes is this file's, whatever a session
    /// appends to it meanwhile, so that a note says this one.
    pub digest: String,
    /// The file was rewritten first, past its bound: its size before, and
    /// the descriptions that went.
    pub compacted: Option<(u64, usize)>,
    /// The file was of an earlier version, started again: its size.
    pub earlier: Option<u64>,
    /// The file is past its bound but was not rewritten: another process
    /// (a session) has it open.
    pub not_compacted: bool,
}

/// A kept pipeline as the session made it: its shaders, and its graphics
/// state read again from the title's stream.
struct Description {
    shaders: Vec<Arc<Shader>>,
    state: GraphicsState,
}

/// A pipeline's shader set: its containers' BLAKE3, in order, whatever
/// their stages (the pack's effects put a mesh program in the vertex
/// list).
pub type ShaderSet = Vec<[u8; 32]>;

/// The shader set of `containers`.
pub fn shader_set<'a>(containers: impl IntoIterator<Item = &'a [u8; 32]>) -> ShaderSet {
    let mut set: ShaderSet = containers.into_iter().copied().collect();
    set.sort_unstable();
    set
}

// SAFETY: the graphics state's input elements point into its own names,
// and it is only read once made (as `make::Pending`'s).
unsafe impl Sync for Description {}

impl Recorded {
    /// How many descriptions there are to make again.
    pub fn len(&self) -> usize {
        self.pipelines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pipelines.is_empty()
    }
}

/// The pipelines sessions kept in the pipeline cache's folder, each once
/// under this build's key; none without a cache or a file. Its containers
/// join the shaders a session would share ([`super::shader`]).
pub fn recorded() -> Result<Recorded, String> {
    match super::library::dir() {
        Some(dir) => recorded_in(dir, super::record::COMPACT_AT, super::record::COMPACT_TO),
        None => Ok(Recorded::none()),
    }
}

impl Recorded {
    /// No descriptions, from no file.
    fn none() -> Recorded {
        Recorded {
            pipelines: Vec::new(),
            shader_sets: HashSet::new(),
            damaged: 0,
            missing: 0,
            repeated: 0,
            bytes: 0,
            path: None,
            digest: "none".into(),
            compacted: None,
            earlier: None,
            not_compacted: false,
        }
    }
}

/// [`recorded`] of the file in `dir`, rewritten first down to `to` bytes
/// if it is larger than `at` (or started again if it is of an earlier
/// version), unless another process has it open.
fn recorded_in(dir: &std::path::Path, at: u64, to: u64) -> Result<Recorded, String> {
    let mut out = Recorded::none();
    let mut file = super::record::read(dir)?;
    if file.bytes > at || file.earlier {
        match super::record::compact(dir, to)? {
            Some(c) => {
                tracing::info!(
                    before = c.before,
                    after = c.after,
                    kept = c.kept,
                    dropped = c.dropped,
                    earlier = c.earlier,
                    "recorded pipelines: rewritten"
                );
                if c.earlier {
                    out.earlier = Some(c.before);
                } else {
                    out.compacted = Some((c.before, c.dropped));
                }
            }
            None => {
                tracing::info!(
                    bytes = file.bytes,
                    "recorded pipelines: not rewritten, another process has the file open"
                );
                out.not_compacted = true;
            }
        }
        file = super::record::read(dir)?;
    }
    out.path = Some(dir.join(super::record::FILE));
    out.bytes = file.bytes;
    out.digest = file.digest.clone();
    out.damaged = file.damaged;
    let mut seen = HashSet::new();
    for kept in &file.pipelines {
        out.shader_sets
            .insert(shader_set(kept.shaders.iter().map(|(_, hash)| hash)));
        let Some(state) = kept.stream.state() else {
            out.damaged += 1;
            continue;
        };
        let Some(shaders) = kept
            .shaders
            .iter()
            .map(|(stage, hash)| Some(super::shader(stage, file.container(hash)?)))
            .collect::<Option<Vec<_>>>()
        else {
            out.missing += 1;
            continue;
        };
        if !seen.insert(super::make::key(&shaders, Some(&state))) {
            out.repeated += 1;
            continue;
        }
        out.pipelines.push(Description { shaders, state });
    }
    Ok(out)
}

/// What became of one kept pipeline made again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replayed {
    /// Its host pipeline is in the library: made now or found there
    /// ([`counts`] tells how many of each).
    Ready,
    /// One of its shaders does not translate (the translator's refusal,
    /// kept in the shader cache).
    Untranslated,
    /// The host refused it, or a console program is refused before any
    /// translation (the reasons: [`refusals`]).
    Refused,
}

/// Makes the host pipeline of `recorded`'s pipeline `i` into the library,
/// as the session's first draw with it would: the same function, shaders
/// and state.
pub fn replay(recorded: &Recorded, i: usize) -> Replayed {
    let d = &recorded.pipelines[i];
    if HostPipeline::new(&d.shaders, Some(&d.state)).is_some() {
        return Replayed::Ready;
    }
    if d.shaders
        .iter()
        .any(|s| matches!(s.translated.get(), Some(None)))
    {
        Replayed::Untranslated
    } else {
        Replayed::Refused
    }
}

/// Saves the pipeline library if pipelines were added; why not, if the
/// save fails (what it was to write is written by the next save). The list
/// of title pipelines to start early is read first: no title pipeline was
/// created here, so nothing else read it, and the save writes back what it
/// holds.
pub fn finish() -> Result<(), String> {
    let _ = super::library::wanted_early("");
    match super::library::save(true) {
        super::library::Saved::Failed(e) => Err(e),
        super::library::Saved::Nothing | super::library::Saved::Written => Ok(()),
    }
}

#[cfg(test)]
pub(in crate::pipeline) mod tests {
    use super::*;

    /// A container's stage is its translated program's; none without one.
    /// (`apps/xtp/tests/precook.rs` precooks a real compute program.)
    #[test]
    fn stages_are_read_from_the_container() {
        assert_eq!(stage(b"not a container"), None);
        assert_eq!(stage(&dxbc(&[])), None, "no DXIL part");
        assert_eq!(stage(&dxbc(&[(b"DXIL", &[0; 8])])), None, "no program");
    }

    /// A DXBC container holding `parts`.
    fn dxbc(parts: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let header = 0x20 + 4 * parts.len();
        let mut body = Vec::new();
        let mut offsets = Vec::new();
        for (fourcc, data) in parts {
            offsets.push((header + body.len()) as u32);
            body.extend(*fourcc);
            body.extend((data.len() as u32).to_le_bytes());
            body.extend(*data);
        }
        let mut out = b"DXBC".to_vec();
        out.extend([0; 16]);
        out.extend(1u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(((header + body.len()) as u32).to_le_bytes());
        out.extend((parts.len() as u32).to_le_bytes());
        for o in offsets {
            out.extend(o.to_le_bytes());
        }
        out.extend(body);
        out
    }

    /// A shader of `container`'s bytes, shared as a session shares it
    /// ([`super::super::shader`]), whose translation is already made:
    /// `source` compiled by FXC for `profile`, or, with no source, the
    /// translator's refusal. The translator is not this test's business
    /// (`apps/xtp/tests/precook.rs` translates a real program).
    pub(in crate::pipeline) fn translated(
        stage: &'static str,
        kind: HwStage,
        source: Option<&str>,
        profile: windows::core::PCSTR,
        container: &[u8],
    ) -> Arc<Shader> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        let translated = source.map(|source| {
            let mut blob = None;
            // SAFETY: the source lives through compilation and the blob
            // owns its bytes.
            let dxil = unsafe {
                D3DCompile(
                    source.as_ptr().cast(),
                    source.len(),
                    windows::core::PCSTR::null(),
                    None,
                    None,
                    windows::core::s!("main"),
                    profile,
                    0,
                    0,
                    &mut blob,
                    None,
                )
                .unwrap();
                let blob = blob.unwrap();
                std::slice::from_raw_parts(
                    blob.GetBufferPointer().cast::<u8>(),
                    blob.GetBufferSize(),
                )
                .to_vec()
            };
            super::super::Translated {
                host: xtp_shader::emit::HostShader {
                    stage: kind,
                    dxil,
                    group_base: None,
                    user_data: vec![],
                    samplers: vec![],
                    static_samplers: vec![],
                    root_cbvs: vec![],
                    root_srvs: vec![],
                    stride_table: false,
                    run_time_buffers: 0,
                },
                used: vec![],
                wave64: false,
            }
        });
        let shader = super::super::shader(stage, container);
        assert!(
            shader.translated.set(translated).is_ok(),
            "a container of this test's own"
        );
        shader
    }

    fn words(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// The file of kept descriptions is rewritten once past its bound,
    /// before it is read: within room, nothing goes; without, all.
    #[test]
    fn the_kept_file_is_rewritten_past_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let stream = super::super::record::stream_of(&[(14, words(&[3]))]);
        let shaders = vec![
            super::super::shader("vertex", b"precook bound: vertex"),
            super::super::shader("pixel", b"precook bound: pixel"),
        ];
        assert!(super::super::record::keep_in(dir.path(), &shaders, &stream));
        let before = std::fs::metadata(dir.path().join(super::super::record::FILE))
            .unwrap()
            .len();
        let r = recorded_in(dir.path(), u64::MAX, u64::MAX).unwrap();
        assert_eq!((r.compacted, r.len()), (None, 1));
        let r = recorded_in(dir.path(), 0, u64::MAX).unwrap();
        assert_eq!((r.compacted, r.len()), (Some((before, 0)), 1));
        let r = recorded_in(dir.path(), 0, 0).unwrap();
        assert_eq!((r.compacted, r.len()), (Some((before, 1)), 0));
    }

    /// The descriptions made again are those of the file as read: its
    /// digest is that file's, not the one a session appends to meanwhile
    /// (the note says what was made, and the next precook makes the rest).
    #[test]
    fn the_digest_is_the_files_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(super::super::record::FILE);
        let none = recorded_in(dir.path(), u64::MAX, u64::MAX).unwrap();
        assert_eq!((none.len(), none.digest.as_str()), (0, "none"));
        let stream = super::super::record::stream_of(&[(14, words(&[3]))]);
        let keep = |pixel: &[u8]| {
            let shaders = vec![
                super::super::shader("vertex", b"precook digest: vertex"),
                super::super::shader("pixel", pixel),
            ];
            assert!(super::super::record::keep_in(dir.path(), &shaders, &stream));
        };
        keep(b"precook digest: pixel a");
        let read = recorded_in(dir.path(), u64::MAX, u64::MAX).unwrap();
        let digest = blake3::hash(&std::fs::read(&path).unwrap())
            .to_hex()
            .to_string();
        assert_eq!((read.len(), read.digest.as_str()), (1, digest.as_str()));
        // A session appends after the read.
        keep(b"precook digest: pixel b");
        assert_eq!(read.digest, digest, "the file as read");
        assert_ne!(digest_in(dir.path()).unwrap(), digest, "the file now");
    }

    /// A graphics pipeline a session kept is made again into the library
    /// by the same function, with the state read again from its stream:
    /// compiled the first time, found in the library the next. Two
    /// descriptions this build reads as one pipeline are made once; one
    /// whose shader no longer translates is refused at once.
    #[test]
    fn kept_graphics_pipelines_are_precooked_into_the_library() {
        let test =
            "pipeline::precook::tests::kept_graphics_pipelines_are_precooked_into_the_library";
        let Some(_gpu) = crate::testgpu::device(test) else {
            return;
        };
        let vs = translated(
            "vertex",
            HwStage::Vs,
            Some(
                "float4 main(uint id : SV_VertexID) : SV_Position { return float4(id, 0, 0, 1); }",
            ),
            windows::core::s!("vs_5_1"),
            b"precook test: vertex",
        );
        let ps = translated(
            "pixel",
            HwStage::Ps,
            Some("float4 main() : SV_Target { return float4(0.25, 0.5, 0.75, 1); }"),
            windows::core::s!("ps_5_1"),
            b"precook test: pixel",
        );
        let refused = translated(
            "pixel",
            HwStage::Ps,
            None,
            windows::core::PCSTR::null(),
            b"precook test: refused pixel",
        );
        // Triangles into one R8G8B8A8_UNORM target, no depth, no culling.
        let stream = super::super::record::stream_of(&[
            (10, words(&[3, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0])),
            (11, words(&[0, 1, 2, 0, 0xffff, 1, 1, 1, 8, 1, 1, 1, 8])),
            (14, words(&[3])),
            (15, words(&[28, 0, 0, 0, 0, 0, 0, 0, 1])),
        ]);
        // The same with a node mask, which this build does not read.
        let masked = super::super::record::stream_of(&[
            (10, words(&[3, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0])),
            (11, words(&[0, 1, 2, 0, 0xffff, 1, 1, 1, 8, 1, 1, 1, 8])),
            (14, words(&[3])),
            (15, words(&[28, 0, 0, 0, 0, 0, 0, 0, 1])),
            (18, words(&[1])),
        ]);
        let shaders = vec![vs.clone(), ps.clone()];
        let dir = tempfile::tempdir().unwrap();
        let keep = super::super::record::keep_in;
        assert!(keep(dir.path(), &shaders, &stream));
        assert!(!keep(dir.path(), &shaders, &stream), "kept once");
        assert!(keep(dir.path(), &shaders, &masked));
        let other = vec![vs.clone(), refused.clone()];
        assert!(keep(dir.path(), &other, &stream));

        let recorded = recorded_in(dir.path(), u64::MAX, u64::MAX).unwrap();
        assert_eq!(
            (
                recorded.len(),
                recorded.repeated,
                recorded.missing,
                recorded.damaged
            ),
            (2, 1, 0, 0)
        );
        assert!(
            Arc::ptr_eq(&recorded.pipelines[0].shaders[0], &vs),
            "shared"
        );
        let made = counts().made;
        assert_eq!(replay(&recorded, 0), Replayed::Ready);
        assert!(counts().made > made, "compiled");
        let loaded = counts().loaded;
        assert_eq!(replay(&recorded, 0), Replayed::Ready);
        if super::super::library::available() {
            assert!(counts().loaded > loaded, "found in the library");
        }
        assert_eq!(replay(&recorded, 1), Replayed::Untranslated);
    }
}
