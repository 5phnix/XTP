//! `xtp shader-survey`: the pack survey of `docs/analysis/shader.md` (S0).
//!
//! Every program of every record of a shader pack (the contributor's own
//! `shaders.rpf`; nothing of it enters the repository) goes through the
//! translation the runtime uses, [`emit::translate_program`], and counts
//! once: translated, or under its first blocker. Blockers are grouped as
//! the analysis's section 4 groups them, and each is in the bucket of the
//! backlog item that removes it ([`ROWS`]). Fixing a first blocker can
//! reveal a second one, so an item's real gain is the difference between
//! two surveys.
//!
//! `--json FILE` writes the same as a stable file: fixed key order, one
//! program per line with its outcome and the hash of its translation (DXIL
//! and binding tables), so two runs compare as text (`git diff --no-index
//! before.json after.json`). A change that must not alter the translation,
//! such as a refactor of the emitter, leaves the file byte-identical.
//!
//! `--host-lanes MIN-MAX` translates for a host whose waves are that many
//! lanes ([`emit::Target`]): `32-32` is NVIDIA's, where console wave64
//! programs run as two 32-lane halves or stop with their reason
//! (`docs/wave64-on-wave32.md`). Without it the translation is the one for
//! any host, and the JSON is what it was; with it the JSON also says
//! `host_lanes`, and each translation's `wave` where it requires one. The
//! report counts the wave size each translation requires, by the console's.
//!
//! The pack: the path given, else `XTP_SAMPLE_SHADERS`, else `shaders.rpf`
//! in the configured game folder. Records are found as `xtp translate`
//! finds them ([`sources`]: SGD5 records by their marker, which RPF9 packs
//! store as is, and raw containers).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use xtp_shader::emit::{self, validator::Validator};
use xtp_shader::{Container, FourCC, HwStage, ShaderError, native_programs};

use crate::shader::{MAX_FILE, Source, sources};

/// The environment variable naming a pack, for the command and the opt-in
/// test.
pub(crate) const PACK_VAR: &str = "XTP_SAMPLE_SHADERS";

/// Stages in the order of section 4's columns, then the rest.
const STAGES: [HwStage; 8] = [
    HwStage::Cs,
    HwStage::Ms,
    HwStage::Ps,
    HwStage::Vs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Hs,
    HwStage::Ls,
];

/// The cause of programs the survey does not attempt: their stages are
/// translated as one merged pair (backlog G1, G2).
const MERGED: &str = "a merged ES + GS program (not attempted)";
/// The same for tessellation stages, which the title does not use.
const TESSELLATION: &str = "a tessellation-stage program (not attempted)";

/// One row of section 4: the error texts it groups (the `what` of
/// [`ShaderError::Unsupported`], whatever step reports it) and the backlog
/// item that removes them.
pub(crate) struct Row {
    pub category: &'static str,
    pub label: &'static str,
    /// The item for compute programs (and all, without `graphics`).
    pub item: &'static str,
    /// The item for vertex, pixel and mesh programs, where it differs.
    pub graphics: Option<&'static str>,
    pub texts: &'static [&'static str],
}

const LANES: &str = "Lane masks and wave layout";
const MEMORY: &str = "Memory read as data, descriptor tracking";
const IMAGES: &str = "Images";
const PIXEL: &str = "Pixel inputs and exports";
const LDS: &str = "LDS";
const OTHER: &str = "Other";
const GEOMETRY: &str = "Merged ES + GS";
const UNCLASSIFIED: &str = "Unclassified";

/// Section 4's rows, in its order.
pub(crate) const ROWS: &[Row] = &[
    Row {
        category: LANES,
        label: "select on an untracked mask",
        item: "M1",
        graphics: None,
        texts: &["a select on an untracked mask"],
    },
    Row {
        category: LANES,
        label: "mask holding lanes elsewhere read as data",
        item: "M2",
        graphics: None,
        texts: &["a lane mask holding lanes elsewhere read as data"],
    },
    Row {
        category: LANES,
        label: "lane mask read as data (compute M2, else L4)",
        item: "M2",
        graphics: Some("L4"),
        texts: &["a lane mask read as data"],
    },
    Row {
        category: LANES,
        label: "cross-lane operation outside compute / in a mesh shader",
        item: "L4",
        graphics: None,
        texts: &[
            "a cross-lane operation outside compute",
            "a cross-lane operation in a mesh shader",
        ],
    },
    Row {
        category: LANES,
        label: "opaque / constant mask, mask join of an untracked mask",
        item: "M1",
        graphics: None,
        texts: &[
            "an opaque lane mask",
            "a constant lane mask",
            "a lane-mask join of an untracked mask",
        ],
    },
    Row {
        category: LANES,
        label: "whole-wave step not simulated",
        item: "M2",
        graphics: None,
        texts: &["a whole-wave section step the simulation does not follow"],
    },
    Row {
        category: LANES,
        label: "console wave64 on a host without 64-lane waves (--host-lanes)",
        item: "L1",
        graphics: None,
        texts: &[
            "console wave64 lanes on a host without 64-lane waves",
            "a wave64 group sharing LDS without barriers on a host without 64-lane waves",
            "a console wave size the host does not run",
            "ds_permute across the halves of a wave64 on a host without 64-lane waves",
            "a whole-wave section of a wave64 on a host without 64-lane waves",
            "a whole-wave reduction across 32-lane halves",
            "a lane mask read whole in a wave64 program split in halves",
            "a wave reduction in a wave64 program split in halves",
            "a branch the console flow does not model",
        ],
    },
    Row {
        category: LANES,
        label: "console wave64, several console waves per group (--host-lanes)",
        item: "L2",
        graphics: None,
        texts: &[
            "console wave64 lanes in a group of more than 64 threads on a host without 64-lane waves",
        ],
    },
    Row {
        category: LANES,
        label: "v_permlanex16_b32",
        item: "S3",
        graphics: None,
        texts: &["v_permlanex16_b32"],
    },
    Row {
        category: MEMORY,
        label: "scalar memory read as data (R1 classifies, R2 the rest)",
        item: "R1",
        graphics: None,
        texts: &["a scalar memory read as data"],
    },
    Row {
        category: MEMORY,
        label: "descriptor read only as data",
        item: "R2",
        graphics: None,
        texts: &["a descriptor read only as data"],
    },
    Row {
        category: MEMORY,
        label: "descriptor's address or format read as data",
        item: "R2",
        graphics: None,
        texts: &["a descriptor's address or format read as data"],
    },
    Row {
        category: MEMORY,
        label: "structured buffer of unknown stride",
        item: "R3",
        graphics: None,
        texts: &["a structured buffer of unknown stride"],
    },
    Row {
        category: MEMORY,
        label: "address not from a root parameter / from a phi",
        item: "R4",
        graphics: None,
        texts: &[
            "an address not from a root parameter",
            "an address from a phi",
            "an address read from the first lane of a per-lane value",
        ],
    },
    Row {
        category: MEMORY,
        label: "per-lane descriptor cases",
        item: "R4",
        graphics: None,
        texts: &[
            "a descriptor read from the first lane of computed VGPRs",
            "a waterfall loop whose compares do not select the access's lanes",
            "a descriptor partly joined by control flow",
            "a descriptor chosen by control flow",
            "a descriptor neither loaded nor built",
        ],
    },
    Row {
        category: IMAGES,
        label: "64-bit image atomic",
        item: "I1",
        graphics: None,
        texts: &["a 64-bit image atomic"],
    },
    Row {
        category: IMAGES,
        label: "biased sample with 16-bit coordinates, image_sample_d_cl, image_gather4_l",
        item: "S1",
        graphics: None,
        texts: &[
            "a biased image sample with 16-bit coordinates",
            "image_sample_d_cl",
            "image_gather4_l",
        ],
    },
    Row {
        category: PIXEL,
        label: "computed barycentrics, barycentrics read directly, first half of an interpolation",
        item: "P2",
        graphics: None,
        texts: &[
            "an interpolation with computed barycentrics",
            "barycentrics read directly",
            "the first half of an interpolation",
        ],
    },
    Row {
        category: PIXEL,
        label: "packed input half no element holds",
        item: "P1",
        graphics: None,
        texts: &["a packed input half no element holds"],
    },
    Row {
        category: PIXEL,
        label: "hardware SGPR input, conditional export",
        item: "S3",
        graphics: None,
        texts: &["a hardware SGPR input", "a conditional export"],
    },
    Row {
        category: LDS,
        label: "bytes/halves, unallocated, other",
        item: "S3",
        graphics: None,
        texts: &[
            "an LDS write of bytes or halves",
            "an LDS read of bytes or halves",
            "an LDS operation",
            "LDS the program does not allocate",
        ],
    },
    Row {
        category: OTHER,
        label: "v_cvt_f32_f64, s_getpc_b64",
        item: "S3",
        graphics: None,
        texts: &["v_cvt_f32_f64", "s_getpc_b64"],
    },
    Row {
        category: GEOMETRY,
        label: "merged ES + GS, not attempted (G1 pairs, G2 lowers)",
        item: "G2",
        graphics: None,
        texts: &[MERGED],
    },
];

/// The section 4 row of a blocker's text, if any.
pub(crate) fn row_of(what: &str) -> Option<&'static Row> {
    ROWS.iter().find(|r| r.texts.contains(&what))
}

/// A row's item, or its items by stage kind (`compute/graphics`).
fn item_label(r: &Row) -> String {
    match r.graphics {
        Some(g) => format!("{}/{g}", r.item),
        None => r.item.to_owned(),
    }
}

/// `(category, row, item)` of a blocker in a program of `stage`.
fn bucket(what: &str, stage: HwStage) -> (&'static str, &'static str, &'static str) {
    match row_of(what) {
        Some(r) => {
            let graphics = !matches!(stage, HwStage::Cs);
            let item = r.graphics.filter(|_| graphics).unwrap_or(r.item);
            (r.category, r.label, item)
        }
        None => (UNCLASSIFIED, "not in section 4", "?"),
    }
}

/// What became of one program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Translated: the hash of the whole translation, and with validation
    /// the validator's verdict (its first error line).
    Translated {
        output: blake3::Hash,
        check: Option<Result<(), String>>,
        /// The wave size the translation requires (`WaveSize`), if any.
        wave: Option<u32>,
    },
    /// Not translated: the step that stopped it and its blocker.
    Failed { step: &'static str, what: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Program {
    pub stage: HwStage,
    /// The console runs it in waves of 64.
    pub wave64: bool,
    pub outcome: Outcome,
}

/// One record of the pack and its programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub offset: usize,
    pub name: Option<String>,
    /// Why the record holds no program to survey, if so.
    pub problem: Option<String>,
    pub programs: Vec<Program>,
}

/// A survey of one pack.
pub(crate) struct Survey {
    pub bytes: usize,
    pub hash: blake3::Hash,
    /// Validation of every Nth record's translations, if asked.
    pub validate_every: Option<usize>,
    /// The host wave sizes translated for.
    pub target: emit::Target,
    pub records: Vec<Record>,
}

/// The pack to survey: `pack`, else `XTP_SAMPLE_SHADERS`, else
/// `shaders.rpf` in the configured game folder.
pub(crate) fn pack_path(pack: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = pack {
        return Ok(p);
    }
    if let Some(p) = std::env::var_os(PACK_VAR) {
        return Ok(p.into());
    }
    let cfg = xtp_core::Config::load()?;
    let dir = cfg.game_dir.with_context(|| {
        format!("no pack given, {PACK_VAR} unset and no game_dir configured (see xtp.example.toml)")
    })?;
    Ok(dir.join("shaders.rpf"))
}

pub fn run(
    pack: Option<PathBuf>,
    json: Option<PathBuf>,
    validate_every: Option<usize>,
    jobs: Option<usize>,
    host_lanes: Option<(u32, u32)>,
) -> Result<()> {
    let path = pack_path(pack)?;
    let target = host_lanes.map_or(emit::Target::ANY, |(lanes_min, lanes_max)| emit::Target {
        lanes_min,
        lanes_max,
        stride_table: false,
    });
    if let Some(out) = &json {
        // Names of the title's programs stay out of the repository.
        xtp_core::ensure_outside_repo(out)?;
    }
    let dll = validate_every.and(Validator::default_path());
    if validate_every.is_some() {
        Validator::load(dll.as_deref()).map_err(anyhow::Error::msg)?;
    }
    let size = std::fs::metadata(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    anyhow::ensure!(size <= MAX_FILE, "{} is larger than 2 GiB", path.display());
    let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let threads =
        jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let start = std::time::Instant::now();
    let s = survey(&data, validate_every, dll.as_deref(), threads, target);
    print!("{}", report(&s, &path.display().to_string()));
    if let Some(dll) = &dll {
        println!("validator    {}", dll.display());
    }
    println!("time         {:.1} s", start.elapsed().as_secs_f64());
    if let Some(out) = json {
        std::fs::write(&out, to_json(&s)).with_context(|| format!("writing {}", out.display()))?;
        println!("json         {}", out.display());
    }
    Ok(())
}

/// Surveys the pack in `data` on `threads` threads, translating for
/// `target`; with `validate_every`, checks the translations of every Nth
/// record with `dll`'s validator.
pub(crate) fn survey(
    data: &[u8],
    validate_every: Option<usize>,
    dll: Option<&Path>,
    threads: usize,
    target: emit::Target,
) -> Survey {
    let sources = sources(data);
    let next = AtomicUsize::new(0);
    let mut records: Vec<(usize, Record)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.clamp(1, sources.len().max(1)))
            .map(|_| {
                s.spawn(|| {
                    let mut validator = validate_every
                        .is_some()
                        .then(|| Validator::load(dll).ok())
                        .flatten();
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(source) = sources.get(i) else {
                            break;
                        };
                        let check = validate_every.is_some_and(|n| i.is_multiple_of(n.max(1)));
                        let validator = validator.as_mut().filter(|_| check);
                        out.push((i, record(source, validator, target)));
                    }
                    out
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("survey thread"))
            .collect()
    });
    records.sort_by_key(|&(i, _)| i);
    Survey {
        bytes: data.len(),
        hash: blake3::hash(data),
        validate_every,
        target,
        records: records.into_iter().map(|(_, r)| r).collect(),
    }
}

/// Translates every program of one record.
fn record(source: &Source, validator: Option<&mut Validator>, target: emit::Target) -> Record {
    let (offset, name) = match source {
        Source::Raw { offset, .. } => (*offset, None),
        Source::Packed(r) => (r.offset, r.name.map(str::to_owned)),
    };
    let mut out = Record {
        offset,
        name,
        problem: None,
        programs: Vec::new(),
    };
    let Some(bytes) = source.bytes() else {
        out.problem = Some("corrupt record".into());
        return out;
    };
    let Ok(container) = Container::parse(&bytes) else {
        out.problem = Some("not a DXBC container".into());
        return out;
    };
    let Some(dxil) = container.part(FourCC::DXIL) else {
        out.problem = Some("no DXIL part".into());
        return out;
    };
    let programs = match native_programs(dxil.data) {
        Ok(p) => p,
        Err(e) => {
            out.problem = Some(format!("native programs: {e}"));
            return out;
        }
    };
    let mut validator = validator;
    for p in &programs {
        let outcome = match p.stage {
            HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms => {
                // A panic is a translator bug: it is counted, not fatal.
                let translated = catch_unwind(AssertUnwindSafe(|| {
                    emit::translate_program_for(&container, p, target)
                }));
                match translated {
                    Ok(Ok(shader)) => Outcome::Translated {
                        output: output_hash(&shader),
                        wave: shader.wave_size(),
                        check: validator.as_deref_mut().map(|v| {
                            v.validate(&mut shader.dxil.clone())
                                .map_err(|text| first_error(&text))
                        }),
                    },
                    Ok(Err((step, e))) => Outcome::Failed {
                        step: step.name(),
                        what: what(&e),
                    },
                    Err(panic) => Outcome::Failed {
                        step: "panic",
                        what: panic
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_owned())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_default(),
                    },
                }
            }
            HwStage::Es | HwStage::Gs => Outcome::Failed {
                step: "survey",
                what: MERGED.into(),
            },
            HwStage::Hs | HwStage::Ls => Outcome::Failed {
                step: "survey",
                what: TESSELLATION.into(),
            },
        };
        out.programs.push(Program {
            stage: p.stage,
            wave64: p.wave64,
            outcome,
        });
    }
    out
}

/// A blocker's text without the code offset, so programs group by it.
fn what(e: &ShaderError) -> String {
    match e {
        ShaderError::Unsupported { what, .. } => (*what).to_owned(),
        ShaderError::Isa { reason, .. } => (*reason).to_owned(),
        e => e.to_string(),
    }
}

/// The validator's first error line.
fn first_error(text: &str) -> String {
    text.lines()
        .find(|l| l.contains("error"))
        .or_else(|| text.lines().next())
        .unwrap_or("")
        .trim()
        .to_owned()
}

/// The hash of everything a translation gives the host: the DXIL and the
/// binding tables.
pub(crate) fn output_hash(s: &emit::HostShader) -> blake3::Hash {
    let mut h = blake3::Hasher::new();
    h.update(&(s.dxil.len() as u64).to_le_bytes());
    h.update(&s.dxil);
    let tables = format!(
        "{:?} {:?} {:?} {:?} {:?}",
        s.user_data, s.samplers, s.static_samplers, s.root_cbvs, s.root_srvs
    );
    h.update(tables.as_bytes());
    h.finalize()
}

/// Counts per stage.
type PerStage = BTreeMap<HwStage, usize>;

/// Programs of one stage and console wave size, and the wave sizes their
/// translations require (`None`: any).
#[derive(Default)]
pub(crate) struct WaveCounts {
    pub programs: usize,
    pub translated: BTreeMap<Option<u32>, usize>,
}

/// The survey's counts.
#[derive(Default)]
pub(crate) struct Totals {
    pub programs: PerStage,
    pub translated: PerStage,
    /// `step: what` → programs it stops, by stage.
    pub causes: BTreeMap<(String, String), PerStage>,
    pub categories: BTreeMap<&'static str, PerStage>,
    pub rows: BTreeMap<&'static str, PerStage>,
    pub items: BTreeMap<&'static str, PerStage>,
    pub problems: BTreeMap<String, usize>,
    pub checked: usize,
    pub passed: usize,
    pub rejected: BTreeMap<String, usize>,
    /// Programs by (stage, console wave64): how many, and those translated
    /// by the wave size their translation requires.
    pub waves: BTreeMap<(HwStage, bool), WaveCounts>,
    /// The hash of every translation's hash, in pack order.
    pub digest: Option<blake3::Hash>,
}

impl Totals {
    pub(crate) fn of(s: &Survey) -> Self {
        let mut t = Totals::default();
        let mut digest = blake3::Hasher::new();
        for (i, r) in s.records.iter().enumerate() {
            if let Some(p) = &r.problem {
                *t.problems.entry(p.clone()).or_default() += 1;
            }
            for (k, p) in r.programs.iter().enumerate() {
                *t.programs.entry(p.stage).or_default() += 1;
                let waves = t.waves.entry((p.stage, p.wave64)).or_default();
                waves.programs += 1;
                match &p.outcome {
                    Outcome::Translated {
                        output,
                        check,
                        wave,
                    } => {
                        *waves.translated.entry(*wave).or_default() += 1;
                        *t.translated.entry(p.stage).or_default() += 1;
                        digest.update(&(i as u64).to_le_bytes());
                        digest.update(&(k as u64).to_le_bytes());
                        digest.update(output.as_bytes());
                        match check {
                            Some(Ok(())) => {
                                t.checked += 1;
                                t.passed += 1;
                            }
                            Some(Err(e)) => {
                                t.checked += 1;
                                *t.rejected.entry(e.clone()).or_default() += 1;
                            }
                            None => {}
                        }
                    }
                    Outcome::Failed { step, what } => {
                        let (category, row, item) = bucket(what, p.stage);
                        let key = ((*step).to_owned(), what.clone());
                        *t.causes.entry(key).or_default().entry(p.stage).or_default() += 1;
                        *t.categories
                            .entry(category)
                            .or_default()
                            .entry(p.stage)
                            .or_default() += 1;
                        *t.rows.entry(row).or_default().entry(p.stage).or_default() += 1;
                        *t.items.entry(item).or_default().entry(p.stage).or_default() += 1;
                    }
                }
            }
        }
        t.digest = Some(digest.finalize());
        t
    }
}

fn sum(of: &PerStage) -> usize {
    of.values().sum()
}

fn percent(n: usize, of: usize) -> String {
    if of == 0 {
        return "-".into();
    }
    format!("{:.1} %", n as f64 * 100.0 / of as f64)
}

/// The text report.
pub(crate) fn report(s: &Survey, path: &str) -> String {
    let t = Totals::of(s);
    let programs = sum(&t.programs);
    let translated = sum(&t.translated);
    let mut o = String::new();
    let _ = writeln!(
        o,
        "pack         {path}: {} bytes, {} records, blake3 {}",
        s.bytes,
        s.records.len(),
        s.hash
    );
    if s.target != emit::Target::ANY {
        let _ = writeln!(
            o,
            "host waves   {}-{} lanes",
            s.target.lanes_min, s.target.lanes_max
        );
    }
    for (p, n) in &t.problems {
        let _ = writeln!(o, "  {n:>7} records without programs: {p}");
    }
    let _ = writeln!(
        o,
        "translated   {translated} of {programs} programs ({}), {} failed",
        percent(translated, programs),
        programs - translated
    );
    let stages: Vec<HwStage> = STAGES
        .into_iter()
        .filter(|s| t.programs.contains_key(s))
        .collect();
    let _ = writeln!(o, "\n{:<8} {:>9} {:>11}", "stage", "programs", "translated");
    for st in &stages {
        let n = t.programs.get(st).copied().unwrap_or(0);
        let k = t.translated.get(st).copied().unwrap_or(0);
        let _ = writeln!(
            o,
            "{:<8} {n:>9} {k:>11}  {}",
            stage_name(*st),
            percent(k, n)
        );
    }

    // The wave size each translation requires, by the console's (stages
    // whose translations require one, and every console wave64 stage).
    let _ = writeln!(
        o,
        "\n{:<8} {:<8} {:>9} {:>11}  translations require",
        "stage", "console", "programs", "translated"
    );
    for ((st, wave64), w) in &t.waves {
        let k: usize = w.translated.values().sum();
        if !*wave64 && w.translated.keys().all(Option::is_none) {
            continue;
        }
        let sizes: Vec<String> = w
            .translated
            .iter()
            .map(|(size, c)| match size {
                Some(size) => format!("WaveSize({size}) {c}"),
                None => format!("any {c}"),
            })
            .collect();
        let _ = writeln!(
            o,
            "{:<8} {:<8} {:>9} {k:>11}  {}",
            stage_name(*st),
            if *wave64 { "wave64" } else { "wave32" },
            w.programs,
            sizes.join(", ")
        );
    }

    // Section 4: categories, then their rows.
    let head = |o: &mut String, title: &str| {
        let _ = write!(o, "\n{title:<84}");
        for st in &stages {
            let _ = write!(o, " {:>6}", stage_name(*st));
        }
        let _ = writeln!(o, " {:>6}  item", "total");
    };
    let line = |o: &mut String, label: &str, counts: &PerStage, item: &str| {
        let _ = write!(o, "{label:<84}");
        for st in &stages {
            match counts.get(st) {
                Some(n) => {
                    let _ = write!(o, " {n:>6}");
                }
                None => {
                    let _ = write!(o, " {:>6}", "");
                }
            }
        }
        let _ = writeln!(o, " {:>6}  {item}", sum(counts));
    };
    head(&mut o, "first blocker per program (section 4)");
    let mut categories: Vec<&str> = Vec::new();
    for r in ROWS {
        if !categories.contains(&r.category) {
            categories.push(r.category);
        }
    }
    categories.push(UNCLASSIFIED);
    for c in categories {
        let Some(counts) = t.categories.get(c) else {
            continue;
        };
        line(&mut o, c, counts, "");
        for r in ROWS.iter().filter(|r| r.category == c) {
            if let Some(counts) = t.rows.get(r.label) {
                line(&mut o, &format!("  {}", r.label), counts, &item_label(r));
            }
        }
        if c == UNCLASSIFIED {
            line(&mut o, "  not in section 4 (see the causes)", counts, "?");
        }
    }

    // Backlog items, largest first.
    head(
        &mut o,
        "by backlog item (programs whose first blocker it removes)",
    );
    let mut items: Vec<_> = t.items.iter().collect();
    items.sort_by(|a, b| sum(b.1).cmp(&sum(a.1)).then(a.0.cmp(b.0)));
    for (item, counts) in items {
        line(&mut o, &format!("  {item}"), counts, item);
    }

    // Every distinct blocker.
    head(&mut o, "causes (step: blocker)");
    let mut causes: Vec<_> = t.causes.iter().collect();
    causes.sort_by(|a, b| sum(b.1).cmp(&sum(a.1)).then(a.0.cmp(b.0)));
    for ((step, what), counts) in causes {
        let item = row_of(what).map_or("?".into(), item_label);
        line(&mut o, &format!("  {step}: {what}"), counts, &item);
    }

    if let Some(every) = s.validate_every {
        let _ = writeln!(
            o,
            "\nvalidation   {} of {} translations accepted (every {} record{})",
            t.passed,
            t.checked,
            every,
            if every == 1 { "" } else { "s" }
        );
        let mut rejected: Vec<_> = t.rejected.iter().collect();
        rejected.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (e, n) in rejected {
            let _ = writeln!(o, "  {n:>7}  {e}");
        }
    }
    let _ = writeln!(
        o,
        "\noutputs      blake3 {} ({translated} translations)",
        t.digest.map(|d| d.to_string()).unwrap_or_default()
    );
    o
}

fn stage_name(s: HwStage) -> &'static str {
    match s {
        HwStage::Ls => "LS",
        HwStage::Hs => "HS",
        HwStage::Es => "ES",
        HwStage::Gs => "GS",
        HwStage::Vs => "VS",
        HwStage::Ps => "PS",
        HwStage::Ms => "MS",
        HwStage::Cs => "CS",
    }
}

/// A JSON string.
pub(crate) fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// `{"CS": n, ..., "total": n}` in [`STAGES`] order.
fn per_stage(counts: &PerStage) -> String {
    let mut o = String::from("{");
    for st in STAGES {
        if let Some(n) = counts.get(&st) {
            let _ = write!(o, "\"{}\": {n}, ", stage_name(st));
        }
    }
    let _ = write!(o, "\"total\": {}}}", sum(counts));
    o
}

/// The survey as stable JSON: fixed key order, sorted maps, one program
/// per line.
pub(crate) fn to_json(s: &Survey) -> String {
    let t = Totals::of(s);
    let mut o = String::new();
    let _ = writeln!(o, "{{");
    let _ = writeln!(o, "  \"format\": \"xtp-shader-survey 1\",");
    // Only when asked, so that a survey for any host keeps its bytes.
    if s.target != emit::Target::ANY {
        let _ = writeln!(
            o,
            "  \"host_lanes\": \"{}-{}\",",
            s.target.lanes_min, s.target.lanes_max
        );
    }
    let _ = writeln!(
        o,
        "  \"pack\": {{\"bytes\": {}, \"records\": {}, \"blake3\": \"{}\"}},",
        s.bytes,
        s.records.len(),
        s.hash
    );
    let _ = writeln!(o, "  \"programs\": {},", per_stage(&t.programs));
    let _ = writeln!(o, "  \"translated\": {},", per_stage(&t.translated));
    let map = |o: &mut String, name: &str, m: &BTreeMap<&'static str, PerStage>| {
        let _ = writeln!(o, "  \"{name}\": {{");
        let rows: Vec<String> = m
            .iter()
            .map(|(k, v)| format!("    {}: {}", quote(k), per_stage(v)))
            .collect();
        let _ = writeln!(o, "{}", rows.join(",\n"));
        let _ = writeln!(o, "  }},");
    };
    map(&mut o, "categories", &t.categories);
    map(&mut o, "rows", &t.rows);
    map(&mut o, "items", &t.items);
    let _ = writeln!(o, "  \"causes\": {{");
    let causes: Vec<String> = t
        .causes
        .iter()
        .map(|((step, what), v)| {
            format!(
                "    {}: {}",
                quote(&format!("{step}: {what}")),
                per_stage(v)
            )
        })
        .collect();
    let _ = writeln!(o, "{}", causes.join(",\n"));
    let _ = writeln!(o, "  }},");
    let problems: Vec<String> = t
        .problems
        .iter()
        .map(|(p, n)| format!("{}: {n}", quote(p)))
        .collect();
    let _ = writeln!(o, "  \"record_problems\": {{{}}},", problems.join(", "));
    match s.validate_every {
        Some(every) => {
            let rejected: Vec<String> = t
                .rejected
                .iter()
                .map(|(e, n)| format!("{}: {n}", quote(e)))
                .collect();
            let _ = writeln!(
                o,
                "  \"validation\": {{\"every\": {every}, \"checked\": {}, \"passed\": {}, \"rejected\": {{{}}}}},",
                t.checked,
                t.passed,
                rejected.join(", ")
            );
        }
        None => {
            let _ = writeln!(o, "  \"validation\": null,");
        }
    }
    let _ = writeln!(
        o,
        "  \"outputs\": \"{}\",",
        t.digest.map(|d| d.to_string()).unwrap_or_default()
    );
    let _ = writeln!(o, "  \"list\": [");
    let mut lines = Vec::new();
    for r in &s.records {
        let name = r.name.as_deref().map_or("null".into(), quote);
        if let Some(p) = &r.problem {
            lines.push(format!(
                "    {{\"record\": \"{:#x}\", \"name\": {name}, \"problem\": {}}}",
                r.offset,
                quote(p)
            ));
        }
        for (k, p) in r.programs.iter().enumerate() {
            let outcome = match &p.outcome {
                Outcome::Translated {
                    output,
                    check,
                    wave,
                } => {
                    let check = match check {
                        None => String::new(),
                        Some(Ok(())) => ", \"validator\": \"ok\"".into(),
                        Some(Err(e)) => format!(", \"validator\": {}", quote(e)),
                    };
                    // With --host-lanes only, as the header's `host_lanes`.
                    let wave = match wave {
                        Some(w) if s.target != emit::Target::ANY => format!(", \"wave\": {w}"),
                        _ => String::new(),
                    };
                    format!("\"outcome\": \"translated\", \"output\": \"{output}\"{wave}{check}")
                }
                Outcome::Failed { step, what } => format!(
                    "\"outcome\": {}, \"item\": {}",
                    quote(&format!("{step}: {what}")),
                    quote(bucket(what, p.stage).2)
                ),
            };
            lines.push(format!(
                "    {{\"record\": \"{:#x}\", \"name\": {name}, \"program\": {k}, \"stage\": \"{}\", {outcome}}}",
                r.offset,
                stage_name(p.stage)
            ));
        }
    }
    let _ = writeln!(o, "{}", lines.join(",\n"));
    let _ = writeln!(o, "  ]");
    let _ = writeln!(o, "}}");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The translated count of the person's pack for any host (2026-11-24,
    /// with the wave64 stack: 22,485 before it): the opt-in test fails
    /// below it. Raise it when an item lands.
    const TRANSLATED_FLOOR: usize = 22_508;

    /// An SGD5 record (see `xtp_game::sgd5`) holding `payload` as two
    /// stored deflate blocks, split after its second byte so that a
    /// container's magic does not appear as is in the file (the raw scan
    /// would find it too).
    fn record_bytes(name: &str, payload: &[u8]) -> Vec<u8> {
        let mut packed = Vec::new();
        for (last, part) in [(0u8, &payload[..2]), (1, &payload[2..])] {
            let len = u16::try_from(part.len()).unwrap();
            packed.push(last);
            packed.extend_from_slice(&len.to_le_bytes());
            packed.extend_from_slice(&(!len).to_le_bytes());
            packed.extend_from_slice(part);
        }
        let n = (packed.len() + 12) as u32;
        let mut out = vec![0x11, 0x22, name.len() as u8 + 1];
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&[0, 0x32]);
        for v in [n, 0x000D_DCE7, n, payload.len() as u32] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&packed);
        out
    }

    /// A DXBC container without parts.
    fn empty_container() -> Vec<u8> {
        let mut out = b"DXBC".to_vec();
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&[1, 0, 0, 0]);
        out.extend_from_slice(&32u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    #[test]
    fn every_blocker_text_is_in_one_row() {
        let mut seen = std::collections::HashSet::new();
        for r in ROWS {
            assert!(!r.item.is_empty() && !r.texts.is_empty(), "{}", r.label);
            for t in r.texts {
                assert!(seen.insert(*t), "{t:?} is in two rows");
            }
        }
        assert_eq!(row_of(MERGED).map(|r| r.item), Some("G2"));
    }

    #[test]
    fn lane_masks_read_as_data_split_by_stage_kind() {
        assert_eq!(bucket(LANE_MASK_TEXT, HwStage::Cs).2, "M2");
        for st in [HwStage::Ps, HwStage::Vs, HwStage::Ms] {
            assert_eq!(bucket(LANE_MASK_TEXT, st).2, "L4");
        }
        assert_eq!(bucket("a select on an untracked mask", HwStage::Ps).2, "M1");
        assert_eq!(
            bucket("something new", HwStage::Cs),
            (UNCLASSIFIED, "not in section 4", "?")
        );
    }
    const LANE_MASK_TEXT: &str = "a lane mask read as data";

    /// A stream of records with decoy names and a marker whose sizes
    /// disagree: each record is counted, and why it holds no program.
    #[test]
    fn a_synthetic_stream_counts_records_and_problems() {
        let mut file = b"SGD5".to_vec();
        file.extend(record_bytes(
            "PS_Decoy_\"quoted\"_Wrapped",
            b"no container here",
        ));
        file.extend_from_slice(&[9, 0, 0, 0, 0xE7, 0xDC, 0x0D, 0, 8, 0, 0, 0]);
        let empty = empty_container();
        file.extend(record_bytes("CS_Empty_00112233_Wrapped", &empty));
        let s = survey(&file, None, None, 3, emit::Target::ANY);
        assert_eq!(s.records.len(), 2);
        assert_eq!(
            s.records[0].name.as_deref(),
            Some("PS_Decoy_\"quoted\"_Wrapped")
        );
        let t = Totals::of(&s);
        assert_eq!(sum(&t.programs), 0);
        assert_eq!(t.problems.get("not a DXBC container"), Some(&1));
        assert_eq!(t.problems.get("no DXIL part"), Some(&1));
        // Stable: the same survey gives the same text, and names are quoted.
        let json = to_json(&s);
        assert_eq!(
            json,
            to_json(&survey(&file, None, None, 1, emit::Target::ANY))
        );
        assert!(
            json.contains(r#""name": "PS_Decoy_\"quoted\"_Wrapped""#),
            "{json}"
        );
        assert!(
            json.contains(r#""record_problems": {"no DXIL part": 1, "not a DXBC container": 1}"#),
            "{json}"
        );
        assert!(report(&s, "synthetic").contains("translated   0 of 0 programs"));
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(quote("a\"b\\c\nd\u{1}"), r#""a\"b\\c\nd\u0001""#);
    }

    /// Opt-in (`XTP_SAMPLE_SHADERS`, the person's `shaders.rpf`): the pack
    /// survey keeps at least the recorded translated count, and no program
    /// panics. Slow in a debug build: `cargo test --release -p xtp survey`.
    #[test]
    fn the_pack_survey_keeps_its_floor() {
        let Some(path) = std::env::var_os(PACK_VAR) else {
            eprintln!("{PACK_VAR} unset; skipped");
            return;
        };
        let data = std::fs::read(&path).unwrap();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let s = survey(&data, None, None, threads, emit::Target::ANY);
        let t = Totals::of(&s);
        let translated = sum(&t.translated);
        let panics: Vec<_> = t
            .causes
            .keys()
            .filter(|(step, _)| step == "panic")
            .collect();
        assert!(panics.is_empty(), "translator panics: {panics:?}");
        assert!(
            t.problems.is_empty(),
            "records without programs: {:?}",
            t.problems
        );
        assert!(
            translated >= TRANSLATED_FLOOR,
            "{translated} translated, below the floor of {TRANSLATED_FLOOR}"
        );
    }
}
