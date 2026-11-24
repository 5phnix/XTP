//! Translated shaders kept on disk (decision 0003): under the configured
//! cache folder, keyed by the container's content, by the XTP build that
//! translated it (the BLAKE3 of our executable), so a rebuilt translator
//! never serves an old translation, and by the wave sizes it was made for
//! (VIctory #223: a translation for a 32-lane host is not a 64-lane one's).
//! Entries go through `xtp-pipeline`'s content-addressed cache, which
//! writes them atomically; an entry that does not read back is translated
//! again.
//!
//! The entry is our own encoding of [`HostShader`]: a field added to it
//! must be added to `encode` and `decode` too (entries of older builds
//! are ignored anyway).

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use xtp_pipeline::{Cache, Converter};
use xtp_shader::emit::{HostShader, StaticSampler, Target, UserData};
use xtp_shader::{HwStage, RootType};

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Keeps translated shaders in `dir` from now on.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no shader cache"),
    }
}

/// Shaders found in the cache and translated since the start.
pub(crate) fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// What identifies this build: the BLAKE3 of our executable file and of
/// the translation settings it runs with (the loop limit, which a
/// diagnostics build can change at run time).
fn build() -> &'static [u8; 32] {
    static BUILD: OnceLock<[u8; 32]> = OnceLock::new();
    BUILD.get_or_init(|| {
        let exe = std::env::current_exe().and_then(std::fs::read);
        let limit = xtp_shader::emit::loop_limit();
        if limit != xtp_shader::emit::DEFAULT_LOOP_LIMIT {
            tracing::warn!(limit, "XTP_SHADER_LOOP_LIMIT: translated loops stop sooner");
        }
        match exe {
            Ok(mut bytes) => {
                bytes.extend_from_slice(&limit.to_le_bytes());
                *blake3::hash(&bytes).as_bytes()
            }
            Err(e) => {
                // Without it no entry can be trusted: a fresh key per run.
                tracing::warn!(error = %e, "cannot read our executable; shader cache unused");
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                *blake3::hash(&now.as_nanos().to_le_bytes()).as_bytes()
            }
        }
    })
}

/// The translation of a console shader container for a host that runs
/// `target`'s wave sizes, from the cache when there is one.
pub(crate) fn host_shader(container: &[u8], target: Target) -> Result<HostShader, String> {
    let translate =
        |c: &[u8]| xtp_shader::emit::translate_for(c, target).map_err(|e| e.to_string());
    match CACHE.get() {
        Some(cache) => kept(cache, container, target, translate),
        None => translate(container),
    }
}

/// `translate`'s translation of `container` for `target` from `cache`,
/// under this build's key for that target; made and stored on a miss.
/// The target that keys the entry is the one `translate` is for: the
/// caller passes both.
pub(crate) fn kept(
    cache: &Cache,
    container: &[u8],
    target: Target,
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
) -> Result<HostShader, String> {
    cached(cache, &key(build(), target), container, translate)
}

/// The part of an entry's key before the container: the build, and the
/// wave sizes translated for (fixed sizes, so no two pairs run together).
fn key(build: &[u8; 32], target: Target) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(build);
    h.update(&target.key());
    *h.finalize().as_bytes()
}

/// The cache entry's converter: the key ([`key`]) and the container in,
/// the encoded translation out. `ran` tells a miss from a hit.
struct Translate<F> {
    translate: F,
    ran: AtomicBool,
}

impl<F: Fn(&[u8]) -> Result<HostShader, String> + Send + Sync> Converter for Translate<F> {
    fn id(&self) -> &'static str {
        "shader.dxil"
    }
    fn version(&self) -> u32 {
        // The build fingerprint in the input does the versioning.
        1
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.ran.store(true, Ordering::Relaxed);
        let shader = (self.translate)(&input[32..]).map_err(anyhow::Error::msg)?;
        Ok(encode(&shader))
    }
}

fn cached(
    cache: &Cache,
    key: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
) -> Result<HostShader, String> {
    let input = [&key[..], container].concat();
    let converter = Translate {
        translate,
        ran: AtomicBool::new(false),
    };
    for _ in 0..2 {
        let path = cache
            .get_or_convert(&converter, &input)
            .map_err(|e| format!("{e:#}"))?;
        if let Some(shader) = std::fs::read(&path).ok().and_then(|b| decode(&b)) {
            let (counter, what) = if converter.ran.load(Ordering::Relaxed) {
                (&MISSES, "translated and cached")
            } else {
                (&HITS, "from the cache")
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(path = %path.display(), "shader {what}");
            return Ok(shader);
        }
        tracing::debug!(path = %path.display(), "unreadable shader cache entry; translating again");
        let _ = std::fs::remove_file(&path);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    (converter.translate)(container)
}

/// For tests: [`wave64_program`] with its lane reads.
#[cfg(test)]
pub(crate) fn wave64_in_half_program() -> (xtp_shader::Container<'static>, xtp_shader::NativeProgram)
{
    wave64_program(true)
}

/// For tests: a console wave64 compute program of 64 threads that stores
/// one dword per thread, dword 64 g + t for thread t of group g, through a
/// raw UAV whose table is user data 0, with an empty container. With
/// `in_half`, its lanes read only inside their 32-lane half (a `ds_swizzle`
/// xor 16, then a DPP row mirror) and thread t stores t ^ 31: translated
/// for a host without 64-lane waves it runs as two 32-lane waves and
/// declares `WaveSize(32)`, else `WaveSize(64)`. Without, thread t stores
/// t and the translation runs in waves of any size. The code is the wave64
/// port's own test program (`emit/split_tests.rs`): ported from
/// Marvisak/VIctory#233 (d1fecaa) by r4tedx.
#[cfg(test)]
pub(crate) fn wave64_program(
    in_half: bool,
) -> (xtp_shader::Container<'static>, xtp_shader::NativeProgram) {
    use xtp_shader::native::{ComputeIo, RootParam, StageIo};
    let threads = 64;
    let reads: &[u32] = if in_half {
        &[
            0xD8D4_401F,
            0x0100_0000, // ds_swizzle_b32 v1, v0 offset:swizzle(BITMASK_PERM, and 0x1f, xor 0x10)
            0x7E04_02FA,
            0xFF01_4001, // v_mov_b32_dpp v2, v1 row_mirror row_mask:0xf bank_mask:0xf
        ]
    } else {
        &[0x7E04_0300] // v_mov_b32 v2, v0
    };
    let words = [
        &[
            0xBE82_0300, // s_mov_b32 s2, s0
            0xBE83_0380, // s_mov_b32 s3, 0
            0xF408_0101,
            0xFA00_0000, // s_load_dwordx4 s[4:7], s[2:3], null
            0x9308_FF01,
            threads * 4, // s_mul_i32 s8, s1, threads * 4
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        ][..],
        reads,
        &[
            0x3406_0082, // v_lshlrev_b32 v3, 2, v0
            0x4A06_0608, // v_add_nc_u32 v3, s8, v3
            0xE070_1000,
            0x8001_0203, // buffer_store_dword v2, v3, s[4:7], 0 offen
            0xBF81_0000, // s_endpgm
        ],
    ]
    .concat();
    let native = xtp_shader::NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 8,
        // A UAV table in user SGPR 0; the group id in s1.
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: Vec::new(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1 | 1 << 7,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let container = xtp_shader::Container {
        digest: [0; 16],
        version: (1, 0),
        parts: Vec::new(),
    };
    (container, native)
}

const MAGIC: &[u8; 4] = b"VSH1";
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

/// Little-endian words and length-prefixed lists.
fn encode(s: &HostShader) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, s.dxil.len() as u32);
    out.extend(&s.dxil);
    word(&mut out, s.user_data.len() as u32);
    for u in &s.user_data {
        for v in [u.index, u32::from(u.slot), root_code(u.root), u.dword] {
            word(&mut out, v);
        }
    }
    word(&mut out, s.samplers.len() as u32);
    for sampler in &s.samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, s.static_samplers.len() as u32);
    for t in &s.static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [&s.root_cbvs, &s.root_srvs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    out
}

fn decode(bytes: &[u8]) -> Option<HostShader> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn word(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn list(&mut self) -> Option<Vec<u8>> {
            let n = self.word()? as usize;
            Some(self.take(n)?.to_vec())
        }
    }
    let mut r = Reader(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    let stage = *STAGES.get(r.word()? as usize)?;
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    r.0.is_empty().then_some(HostShader {
        stage,
        dxil,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader() -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC and the rest".to_vec(),
            user_data: vec![UserData {
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
        }
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(cached(&cache, &[8; 32], container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        for e in walk(dir.path()) {
            std::fs::write(e, b"VSH1 broken").unwrap();
        }
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);

        // Failures are not stored.
        let fail = |_: &[u8]| Err::<HostShader, _>("unsupported".to_owned());
        assert!(cached(&cache, &build, b"other", fail).is_err());
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                files.extend(walk(&e.path()));
            } else {
                files.push(e.path());
            }
        }
        files
    }

    /// Translations are kept by the wave sizes they were made for: one
    /// container's wave64 program, translated for an NVIDIA host (two
    /// 32-lane waves, `WaveSize(32)`) and for any host (`WaveSize(64)`),
    /// is two entries, each found again by its own target.
    #[cfg(windows)]
    #[test]
    fn translations_are_kept_by_target() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let (c, native) = wave64_in_half_program();
        let nvidia = Target {
            lanes_min: 32,
            lanes_max: 32,
        };
        let runs = AtomicU64::new(0);
        let get = |target: Target| {
            let translate = |_: &[u8]| {
                runs.fetch_add(1, Ordering::Relaxed);
                xtp_shader::emit::translate_program_for(&c, &native, target)
                    .map_err(|(_, e)| e.to_string())
            };
            let shader = cached(&cache, &key(&build, target), container, translate).unwrap();
            crate::pipeline::host::Needs::of(&shader.dxil)
                .unwrap()
                .lanes
        };
        assert_eq!(get(nvidia), (32, 32));
        assert_eq!(get(Target::ANY), (64, 64));
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        assert_eq!(get(nvidia), (32, 32), "from its own entry");
        assert_eq!(get(Target::ANY), (64, 64));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        let rdna = Target {
            lanes_min: 32,
            lanes_max: 64,
        };
        assert_ne!(key(&build, nvidia), key(&build, rdna));
        assert_ne!(key(&build, rdna), key(&build, Target::ANY));
        assert_ne!(key(&build, nvidia), key(&[8; 32], nvidia));
        assert_eq!(key(&build, nvidia), key(&build, nvidia));
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let bytes = encode(&shader());
        assert_eq!(decode(&bytes), Some(shader()));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_eq!(decode(b"VSH0"), None);
    }
}
