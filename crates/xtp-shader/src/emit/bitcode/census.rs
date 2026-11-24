//! Counts of a module's instructions by kind, as the lowering made it and
//! as it is written (a diagnostic): what a translated program costs before
//! the driver's compiler sees it, and what the clean-up passes ([`super::opt`])
//! take out. After fxceless's VIctory fork (commit 1684989), without its
//! counts of the lowering's guards, clamps and loop counters (the lowering
//! does not tag them here).
//!
//! `XTP_DXIL_CENSUS=<file>` (every build) appends one line per module
//! written to the file: the passes, whether the cleaned module was kept
//! (else the declared features whose use the passes would have changed),
//! and both modules' counts by [`KINDS`] (`plain.<kind>` and
//! `written.<kind>`), tab-separated `key=value`. A program the translation
//! makes in several forms counts once per form it wrote.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::{Mutex, OnceLock};

use super::{Cast, Module, Op};

/// The kinds, in the order of [`Census::counts`].
pub const KINDS: [&str; 23] = [
    "total",
    "arith",
    "math_call",
    "bitcast",
    "cast",
    "select",
    "phi",
    "extract",
    "buffer_load",
    "cbuffer_load",
    "texture_read",
    "lds_load",
    "buffer_store",
    "lds_store",
    "atomic",
    "handle",
    "annotate",
    "dimensions",
    "barrier",
    "wave_op",
    "io_other",
    "gep",
    "blocks",
];

/// Instruction counts of one module, by [`KINDS`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Census {
    pub counts: [u32; KINDS.len()],
}

#[cfg(test)]
impl Census {
    /// The count of `kind` (one of [`KINDS`]).
    pub fn get(&self, kind: &str) -> u32 {
        self.counts[at(kind)]
    }
}

fn at(kind: &str) -> usize {
    KINDS
        .iter()
        .position(|k| *k == kind)
        .expect("a census kind")
}

/// The kind of a call of the function `name`.
fn kind(name: &str) -> usize {
    let n = name.strip_prefix("dx.op.").unwrap_or(name);
    let n = n.split('.').next().unwrap_or(n);
    match n {
        "unary" | "binary" | "tertiary" | "unaryBits" | "legacyF16ToF32" | "legacyF32ToF16"
        | "isSpecialFloat" | "dot2" | "dot3" | "dot4" | "quaternary" => at("math_call"),
        "bufferLoad" | "rawBufferLoad" => at("buffer_load"),
        "cbufferLoadLegacy" | "cbufferLoad" => at("cbuffer_load"),
        "bufferStore" | "rawBufferStore" | "textureStore" => at("buffer_store"),
        "atomicBinOp" | "atomicCompareExchange" => at("atomic"),
        "createHandle" | "createHandleFromBinding" | "createHandleFromHeap" => at("handle"),
        "annotateHandle" => at("annotate"),
        "getDimensions" => at("dimensions"),
        "barrier" => at("barrier"),
        _ if n.starts_with("wave") || n.starts_with("quad") => at("wave_op"),
        _ if n.starts_with("textureLoad")
            || n.starts_with("sample")
            || n.starts_with("textureGather")
            || n.starts_with("calculateLOD") =>
        {
            at("texture_read")
        }
        _ => at("io_other"),
    }
}

impl Module {
    /// The instructions [`Module::write`] writes for this module, by kind:
    /// those of the blocks reached from the entry (and of their joins,
    /// those something reads).
    pub(crate) fn census(&self) -> Census {
        let call_kind: Vec<usize> = self.funcs.iter().map(|f| kind(&f.name)).collect();
        let live = self.live();
        let reached = self.reached();
        let mut c = Census::default();
        for (_, blk) in self.blocks.iter().enumerate().filter(|(b, _)| reached[*b]) {
            c.counts[at("blocks")] += 1;
            let phis = blk.phis.iter().filter(|&&p| live[p as usize]);
            for &i in phis.chain(&blk.body) {
                c.counts[0] += 1;
                let k = match &self.insts[i as usize].op {
                    Op::Bin(..) | Op::Cmp(..) => at("arith"),
                    Op::Cast(Cast::Bitcast, _) => at("bitcast"),
                    Op::Cast(..) => at("cast"),
                    Op::Select(..) => at("select"),
                    Op::Extract(..) => at("extract"),
                    Op::Phi(_) => at("phi"),
                    Op::Gep(..) => at("gep"),
                    Op::Load(_) => at("lds_load"),
                    Op::Store(..) => at("lds_store"),
                    Op::AtomicRmw(..) | Op::CmpXchg(..) => at("atomic"),
                    Op::Call(f, _) => call_kind[*f as usize],
                };
                c.counts[k] += 1;
            }
        }
        c
    }
}

/// The census file `XTP_DXIL_CENSUS` names, opened once (none: no census).
fn out() -> Option<&'static Mutex<std::fs::File>> {
    static OUT: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    OUT.get_or_init(|| {
        let path = std::env::var_os("XTP_DXIL_CENSUS")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Mutex::new(file))
    })
    .as_ref()
}

/// Whether modules are counted (`XTP_DXIL_CENSUS` names a file that opens).
pub(crate) fn enabled() -> bool {
    out().is_some()
}

/// The census line of a module written with `passes`: `plain` as the
/// lowering made it, `cleaned` what the passes made of it if it was kept,
/// else in `lost` the declared features whose use they would have changed
/// (`-`: none, or no passes).
pub(crate) fn line(passes: u32, plain: &Module, cleaned: Option<&Module>, lost: &[&str]) -> String {
    let before = plain.census();
    let after = cleaned.map_or_else(|| before.clone(), Module::census);
    let lost = if lost.is_empty() {
        "-".to_owned()
    } else {
        lost.join("+")
    };
    let kept = u8::from(cleaned.is_some());
    let mut line = format!("passes={passes}\tkept={kept}\tlost={lost}");
    for (name, n) in KINDS.iter().zip(before.counts) {
        let _ = write!(line, "\tplain.{name}={n}");
    }
    for (name, n) in KINDS.iter().zip(after.counts) {
        let _ = write!(line, "\twritten.{name}={n}");
    }
    line.push('\n');
    line
}

/// Appends the census line of a module written (see [`line`]) when
/// modules are counted.
pub(crate) fn note(passes: u32, plain: &Module, cleaned: Option<&Module>, lost: &[&str]) {
    let Some(out) = out() else { return };
    let line = line(passes, plain, cleaned, lost);
    // A poisoned lock still holds the file: a line is a line.
    let mut file = out
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = file.write_all(line.as_bytes());
}
