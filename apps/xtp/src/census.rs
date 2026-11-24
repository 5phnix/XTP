//! Which imports do titles need, and which do we cover?

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use xtp_hle::{HleResolver, HostPolicy};
use xtp_loader::Provenance;
use xtp_pe::{ImageInfo, ImportSymbol};

use crate::survey::quote;

#[derive(Default)]
struct DllStats {
    functions: BTreeSet<ImportSymbol>,
    hle: usize,
    host: usize,
    missing: usize,
    images: BTreeSet<PathBuf>,
}

pub fn run(paths: Vec<PathBuf>, strict: bool, list_missing: bool, json: bool) -> Result<()> {
    let paths = crate::default_paths(paths)?;
    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });

    let mut images = 0usize;
    let mut failed = Vec::new();
    let mut dlls: BTreeMap<String, DllStats> = BTreeMap::new();
    let mut missing: BTreeMap<(String, String), BTreeSet<PathBuf>> = BTreeMap::new();

    for root in &paths {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if !entry.file_type().is_file() || !looks_like_pe(path) {
                continue;
            }
            let info = match std::fs::read(path)
                .map_err(anyhow::Error::from)
                .and_then(|d| ImageInfo::parse(&d).map_err(anyhow::Error::from))
            {
                Ok(info) => info,
                Err(e) => {
                    failed.push((path.to_path_buf(), e.to_string()));
                    continue;
                }
            };
            images += 1;
            for module in info.imports.iter().chain(&info.delay_imports) {
                let key = module.dll.to_ascii_lowercase();
                let stats = dlls.entry(key.clone()).or_default();
                stats.images.insert(path.to_path_buf());
                for e in &module.entries {
                    let class = resolver.classify(&module.dll, &e.symbol);
                    if class.is_none() {
                        missing
                            .entry((key.clone(), e.symbol.to_string()))
                            .or_default()
                            .insert(path.to_path_buf());
                    }
                    // Count each dll!function once across all images.
                    if stats.functions.insert(e.symbol.clone()) {
                        match class {
                            Some(Provenance::Hle) => stats.hle += 1,
                            Some(Provenance::Host) => stats.host += 1,
                            Some(Provenance::Title) => unreachable!("census maps no title DLLs"),
                            None => stats.missing += 1,
                        }
                    }
                }
            }
        }
    }

    let (mut f, mut h, mut o, mut m) = (0, 0, 0, 0);
    for s in dlls.values() {
        f += s.functions.len();
        h += s.hle;
        o += s.host;
        m += s.missing;
    }

    if json {
        print!(
            "{}",
            to_json(images, failed.len(), (f, h, o, m), &dlls, &missing)
        );
        return Ok(());
    }

    println!("{images} images scanned, {} unreadable", failed.len());
    for (p, e) in &failed {
        println!("  skipped {}: {e}", p.display());
    }
    println!(
        "\n{:<56} {:>6} {:>6} {:>6} {:>8} {:>7}",
        "dll", "funcs", "hle", "host", "missing", "images"
    );
    for (dll, s) in &dlls {
        println!(
            "{dll:<56} {:>6} {:>6} {:>6} {:>8} {:>7}",
            s.functions.len(),
            s.hle,
            s.host,
            s.missing,
            s.images.len()
        );
    }
    println!("{:<56} {f:>6} {h:>6} {o:>6} {m:>8}", "TOTAL");

    if list_missing {
        let mut rows: Vec<_> = missing.into_iter().collect();
        rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
        println!("\nmissing (by number of images using it)");
        for ((dll, sym), users) in rows {
            println!("  {:>4}  {dll}!{sym}", users.len());
        }
    }
    Ok(())
}

/// Stable JSON census report: totals, per-DLL counts, and missing pairs.
fn to_json(
    images: usize,
    unreadable: usize,
    totals: (usize, usize, usize, usize),
    dlls: &BTreeMap<String, DllStats>,
    missing: &BTreeMap<(String, String), BTreeSet<PathBuf>>,
) -> String {
    let (functions, hle, host, missing_total) = totals;
    let mut o = String::new();
    let _ = writeln!(o, "{{");
    let _ = writeln!(o, "  \"format\": \"xtp-census 1\",");
    let _ = writeln!(o, "  \"images\": {images},");
    let _ = writeln!(o, "  \"unreadable\": {unreadable},");
    let _ = writeln!(
        o,
        "  \"totals\": {{\"functions\": {functions}, \"hle\": {hle}, \"host\": {host}, \"missing\": {missing_total}}},"
    );

    if dlls.is_empty() {
        let _ = writeln!(o, "  \"dlls\": [],");
    } else {
        let _ = writeln!(o, "  \"dlls\": [");
        for (i, (dll, s)) in dlls.iter().enumerate() {
            let comma = if i + 1 < dlls.len() { "," } else { "" };
            let _ = writeln!(
                o,
                "    {{\"dll\": {}, \"functions\": {}, \"hle\": {}, \"host\": {}, \"missing\": {}, \"images\": {}}}{comma}",
                quote(dll),
                s.functions.len(),
                s.hle,
                s.host,
                s.missing,
                s.images.len(),
            );
        }
        let _ = writeln!(o, "  ],");
    }

    let mut rows: Vec<_> = missing.iter().collect();
    rows.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
    if rows.is_empty() {
        let _ = writeln!(o, "  \"missing\": []");
    } else {
        let _ = writeln!(o, "  \"missing\": [");
        for (i, ((dll, sym), users)) in rows.iter().enumerate() {
            let comma = if i + 1 < rows.len() { "," } else { "" };
            let _ = writeln!(
                o,
                "    {{\"dll\": {}, \"symbol\": {}, \"images\": {}}}{comma}",
                quote(dll),
                quote(sym),
                users.len(),
            );
        }
        let _ = writeln!(o, "  ]");
    }
    let _ = writeln!(o, "}}");
    o
}

/// Cheap pre-filter: an `MZ` header, regardless of extension (console
/// packages do not always use .exe/.dll).
fn looks_like_pe(path: &Path) -> bool {
    let mut magic = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == b"MZ"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_census_json_emits_empty_arrays() {
        let out = to_json(0, 0, (0, 0, 0, 0), &BTreeMap::new(), &BTreeMap::new());
        assert!(out.contains(r#""format": "xtp-census 1""#));
        assert!(out.contains(r#""dlls": []"#));
        assert!(out.contains(r#""missing": []"#));
    }
}
