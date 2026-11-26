//! Which imports do titles need, and where does the loader bind them?

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use xtp_hle::{HleResolver, HostPolicy};
use xtp_loader::census::{Class, Counts};
use xtp_loader::{Census, StubDb};
use xtp_pe::{ImageInfo, ImportSymbol};

use crate::survey::quote;

#[derive(Default)]
struct DllStats {
    functions: BTreeMap<ImportSymbol, Class>,
    images: BTreeSet<PathBuf>,
}

type ImportList = BTreeMap<(String, ImportSymbol), BTreeSet<PathBuf>>;

#[derive(Default)]
struct Report {
    images: usize,
    unreadable: Vec<(PathBuf, String)>,
    dlls: BTreeMap<String, DllStats>,
    missing: ImportList,
    stubbed: ImportList,
    unverified: ImportList,
}

impl Report {
    fn add(&mut self, dir: &Path, census: Census) {
        self.images += census.images.len();
        self.unreadable.extend(
            census
                .unreadable
                .into_iter()
                .map(|(image, error)| (dir.join(image), error)),
        );
        for import in census.imports {
            let image = dir.join(import.image);
            let dll = import.dll.to_ascii_lowercase();
            let stats = self.dlls.entry(dll.clone()).or_default();
            let class = stats
                .functions
                .entry(import.symbol.clone())
                .or_insert(import.class);
            if binding_order(import.class) > binding_order(*class) {
                *class = import.class;
            }
            let list = match import.class {
                Class::Missing => Some(&mut self.missing),
                Class::Stubbed(_) => Some(&mut self.stubbed),
                Class::Unverified => Some(&mut self.unverified),
                _ => None,
            };
            if let Some(list) = list {
                list.entry((dll, import.symbol))
                    .or_default()
                    .insert(image.clone());
            }
            stats.images.insert(image);
        }
    }
}

/// Count a pair once, under its last binding outcome across the inputs.
fn binding_order(class: Class) -> u8 {
    match class {
        Class::Hle => 0,
        Class::Host => 1,
        Class::Title => 2,
        Class::Unverified => 3,
        Class::Stubbed(_) => 4,
        Class::Missing => 5,
    }
}

fn counts<'a>(classes: impl Iterator<Item = &'a Class>) -> Counts {
    let mut counts = Counts::default();
    for class in classes {
        match class {
            Class::Hle => counts.hle += 1,
            Class::Host => counts.host += 1,
            Class::Title => counts.title += 1,
            Class::Unverified => counts.unverified += 1,
            Class::Stubbed(_) => counts.stubbed += 1,
            Class::Missing => counts.missing += 1,
        }
    }
    counts
}

pub fn run(paths: Vec<PathBuf>, strict: bool, list_missing: bool, json: bool) -> Result<()> {
    let report = scan(crate::default_paths(paths)?, strict);
    if json {
        print!("{}", to_json(&report));
    } else {
        print!("{}", to_text(&report, list_missing));
    }
    Ok(())
}

fn scan(paths: Vec<PathBuf>, strict: bool) -> Report {
    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });
    let wanted = |dll: &str| crate::run::title_dll(&resolver.host, dll);
    let stubs = StubDb::builtin();
    let mut report = Report::default();
    for path in paths {
        let (dir, census) = if path.is_dir() {
            (
                path.as_path(),
                Census::of_folder(&path, &resolver, &wanted, stubs),
            )
        } else {
            let dir = path.parent().unwrap_or(Path::new(""));
            (dir, of_file(&path, &resolver, stubs))
        };
        match census {
            Ok(census) => report.add(dir, census),
            Err(error) => report.unreadable.push((path, error.to_string())),
        }
    }
    report
}

fn of_file(path: &Path, resolver: &HleResolver, stubs: &StubDb) -> Result<Census> {
    let info = ImageInfo::parse(&std::fs::read(path)?)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    Ok(Census::of_images(
        &[(name.into_owned(), info)],
        resolver,
        &|_| false,
        stubs,
    ))
}

fn to_text(report: &Report, list_missing: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} images scanned, {} unreadable",
        report.images,
        report.unreadable.len()
    );
    for (path, error) in &report.unreadable {
        let _ = writeln!(out, "  skipped {}: {error}", path.display());
    }
    let _ = writeln!(
        out,
        "\n{:<56} {:>6} {:>6} {:>6} {:>6} {:>10} {:>8} {:>8} {:>7}",
        "dll", "funcs", "hle", "host", "title", "unverified", "stubbed", "missing", "images"
    );
    for (dll, stats) in &report.dlls {
        text_counts(&mut out, dll, counts(stats.functions.values()));
        let _ = writeln!(out, " {:>7}", stats.images.len());
    }
    let totals = counts(
        report
            .dlls
            .values()
            .flat_map(|stats| stats.functions.values()),
    );
    text_counts(&mut out, "TOTAL", totals);
    out.push('\n');

    if list_missing {
        for (label, list) in [
            ("missing", &report.missing),
            ("stubbed", &report.stubbed),
            ("unverified", &report.unverified),
        ] {
            let _ = writeln!(out, "\n{label} (by number of images using it)");
            for ((dll, sym), users) in sorted_rows(list) {
                let _ = writeln!(out, "  {:>4}  {dll}!{sym}", users.len());
            }
        }
    }
    out
}

fn text_counts(out: &mut String, dll: &str, c: Counts) {
    let _ = write!(
        out,
        "{dll:<56} {:>6} {:>6} {:>6} {:>6} {:>10} {:>8} {:>8}",
        c.total(),
        c.hle,
        c.host,
        c.title,
        c.unverified,
        c.stubbed,
        c.missing
    );
}

fn sorted_rows(list: &ImportList) -> Vec<(&(String, ImportSymbol), &BTreeSet<PathBuf>)> {
    let mut rows: Vec<_> = list.iter().collect();
    rows.sort_by(|a, b| {
        b.1.len()
            .cmp(&a.1.len())
            .then_with(|| a.0.0.cmp(&b.0.0))
            .then_with(|| a.0.1.to_string().cmp(&b.0.1.to_string()))
            .then_with(|| a.0.1.cmp(&b.0.1))
    });
    rows
}

/// Stable JSON census report: totals, per-DLL counts, and unresolved or unverified pairs.
fn to_json(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{{");
    let _ = writeln!(out, "  \"format\": \"xtp-census 1\",");
    let _ = writeln!(out, "  \"images\": {},", report.images);
    let _ = writeln!(out, "  \"unreadable\": {},", report.unreadable.len());
    let totals = counts(
        report
            .dlls
            .values()
            .flat_map(|stats| stats.functions.values()),
    );
    let _ = writeln!(out, "  \"totals\": {{{}}},", json_counts(totals));
    if report.dlls.is_empty() {
        let _ = writeln!(out, "  \"dlls\": [],");
    } else {
        let _ = writeln!(out, "  \"dlls\": [");
        for (i, (dll, stats)) in report.dlls.iter().enumerate() {
            let comma = if i + 1 < report.dlls.len() { "," } else { "" };
            let _ = writeln!(
                out,
                "    {{\"dll\": {}, {}, \"images\": {}}}{comma}",
                quote(dll),
                json_counts(counts(stats.functions.values())),
                stats.images.len(),
            );
        }
        let _ = writeln!(out, "  ],");
    }
    json_list(&mut out, "missing", &report.missing, ",");
    json_list(&mut out, "stubbed", &report.stubbed, ",");
    json_list(&mut out, "unverified", &report.unverified, "");
    let _ = writeln!(out, "}}");
    out
}

fn json_counts(c: Counts) -> String {
    format!(
        "\"functions\": {}, \"hle\": {}, \"host\": {}, \"title\": {}, \"unverified\": {}, \"stubbed\": {}, \"missing\": {}",
        c.total(),
        c.hle,
        c.host,
        c.title,
        c.unverified,
        c.stubbed,
        c.missing
    )
}

fn json_list(out: &mut String, label: &str, list: &ImportList, comma: &str) {
    let rows = sorted_rows(list);
    if rows.is_empty() {
        let _ = writeln!(out, "  \"{label}\": []{comma}");
    } else {
        let _ = writeln!(out, "  \"{label}\": [");
        for (i, ((dll, sym), users)) in rows.iter().enumerate() {
            let row_comma = if i + 1 < rows.len() { "," } else { "" };
            let _ = writeln!(
                out,
                "    {{\"dll\": {}, \"symbol\": {}, \"images\": {}}}{row_comma}",
                quote(dll),
                quote(&sym.to_string()),
                users.len(),
            );
        }
        let _ = writeln!(out, "  ]{comma}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xtp_pe::{ExportInfo, ImportEntry, ImportModule, ImportSymbol};

    fn name(name: &str) -> ImportSymbol {
        ImportSymbol::Name {
            hint: 0,
            name: name.into(),
        }
    }

    fn image(dll: bool, imports: Vec<ImportModule>, exports: &[&str]) -> ImageInfo {
        ImageInfo {
            machine: 0x8664,
            characteristics: if dll { 0x2000 } else { 0 },
            timestamp: 0,
            subsystem: 0,
            dll_characteristics: 0,
            image_base: 0x1_4000_0000,
            size_of_image: 0x10000,
            entry_rva: 0,
            stack_reserve: 0,
            stack_commit: 0,
            sections: Vec::new(),
            imports,
            delay_imports: Vec::new(),
            exports: exports
                .iter()
                .enumerate()
                .map(|(i, name)| ExportInfo {
                    name: Some((*name).into()),
                    ordinal: 1 + i as u16,
                    rva: Some(0x2000),
                    forward: None,
                })
                .collect(),
            tls: None,
            has_relocations: true,
            has_exception_table: true,
            has_load_config: false,
            codeview: None,
        }
    }

    fn imports(dll: &str, symbols: impl IntoIterator<Item = ImportSymbol>) -> ImportModule {
        ImportModule {
            dll: dll.into(),
            entries: symbols
                .into_iter()
                .enumerate()
                .map(|(i, symbol)| ImportEntry {
                    symbol,
                    iat_slot_rva: 0x1000 + 8 * i as u32,
                })
                .collect(),
        }
    }

    #[test]
    fn reports_the_loader_classes() {
        let exe = image(
            false,
            vec![
                imports("Engine.dll", [name("Start"), name("Gone")]),
                imports(
                    "oleaut32.dll",
                    [ImportSymbol::Ordinal(4), ImportSymbol::Ordinal(6)],
                ),
                imports("xgameruntime.dll", [name("XGameRuntimeInitialize")]),
            ],
            &[],
        );
        let images = [
            ("sample.exe".into(), exe),
            ("engine.dll".into(), image(true, vec![], &["Start"])),
            (
                "oleaut32.dll".into(),
                image(true, vec![], &["SysAllocStringLen"]),
            ),
        ];
        let resolver = HleResolver::new(HostPolicy::none());
        let wanted = |dll: &str| crate::run::title_dll(&resolver.host, dll);
        let stubs = StubDb::builtin();
        let mut report = Report::default();
        report.add(
            Path::new("first"),
            Census::of_images(&images, &resolver, &wanted, stubs),
        );

        let json = to_json(&report);
        assert!(json.contains(
            r#""functions": 5, "hle": 1, "host": 0, "title": 2, "unverified": 0, "stubbed": 1, "missing": 1"#
        ));
        assert!(json.contains(r#""dll": "engine.dll", "symbol": "Gone", "images": 1"#));
        let text = to_text(&report, true);
        let (_, lists) = text.split_once("\nmissing (").unwrap();
        let (missing, stubbed) = lists.split_once("\nstubbed (").unwrap();
        assert!(missing.contains("engine.dll!Gone"));
        assert!(!missing.contains("engine.dll!Start"));
        assert!(!missing.contains("oleaut32.dll!"));
        assert!(stubbed.contains("oleaut32.dll!#6"));
        assert!(!stubbed.contains("oleaut32.dll!#4"));
        assert!(!to_text(&report, false).contains("engine.dll!Gone"));

        // A second title with the same image name has no DLL to serve Start.
        report.add(
            Path::new("second"),
            Census::of_images(&images[..1], &resolver, &wanted, stubs),
        );
        let engine = &report.dlls["engine.dll"];
        assert_eq!(engine.images.len(), 2);
        assert_eq!(counts(engine.functions.values()).missing, 2);
        assert_eq!(
            report.missing[&("engine.dll".into(), name("Gone"))].len(),
            2
        );
        assert_eq!(
            report.missing[&("engine.dll".into(), name("Start"))].len(),
            1
        );
    }

    #[test]
    fn title_forwarders_are_listed_as_unverified_even_in_strict_mode() {
        let names = ["Present", "Absent", "Cycle"];
        let exe = image(false, vec![imports("engine.dll", names.map(name))], &[]);
        let mut engine = image(true, vec![], &names);
        for (export, forward) in
            engine
                .exports
                .iter_mut()
                .zip(["target.Start", "absent.Start", "engine.Cycle"])
        {
            export.rva = None;
            export.forward = Some(forward.into());
        }
        let images = [
            ("sample.exe".into(), exe),
            ("engine.dll".into(), engine),
            ("target.dll".into(), image(true, vec![], &["Start"])),
        ];
        for host in [HostPolicy::default(), HostPolicy::none()] {
            let resolver = HleResolver::new(host);
            let wanted = |dll: &str| crate::run::title_dll(&resolver.host, dll);
            let mut report = Report::default();
            report.add(
                Path::new("synthetic"),
                Census::of_images(&images, &resolver, &wanted, StubDb::builtin()),
            );

            let c = counts(report.dlls["engine.dll"].functions.values());
            assert_eq!(
                c,
                Counts {
                    unverified: 3,
                    ..Counts::default()
                }
            );
            assert_eq!((c.total(), c.resolved()), (3, 0));
            assert!(report.missing.is_empty());
            assert!(report.stubbed.is_empty());
            assert_eq!(report.unverified.len(), 3);
            let text = to_text(&report, true);
            let (_, unverified) = text.split_once("\nunverified (").unwrap();
            for alias in names {
                assert!(unverified.contains(&format!("engine.dll!{alias}")));
                assert_eq!(
                    report.unverified[&("engine.dll".into(), name(alias))].len(),
                    1
                );
            }
            let json = to_json(&report);
            assert!(json.contains(r#""unverified": 3"#));
            assert!(json.contains(r#""missing": []"#));
            assert!(json.contains(r#""stubbed": []"#));
            assert!(json.contains(r#""unverified": ["#));
            assert_eq!(json.matches("\"symbol\":").count(), 3);
        }
    }

    #[test]
    fn an_ordinal_is_not_a_literal_hash_name() {
        let exe = image(
            false,
            vec![imports(
                "engine.dll",
                [ImportSymbol::Ordinal(1), name("#1")],
            )],
            &[],
        );
        let mut images = [
            ("sample.exe".into(), exe),
            ("engine.dll".into(), image(true, vec![], &["Start"])),
        ];
        let resolver = HleResolver::new(HostPolicy::none());
        let wanted = |dll: &str| crate::run::title_dll(&resolver.host, dll);
        let census = Census::of_images(&images, &resolver, &wanted, StubDb::builtin());
        assert_eq!(
            census
                .imports
                .iter()
                .filter(|i| i.class == Class::Title)
                .count(),
            1,
        );
        assert_eq!(
            census
                .imports
                .iter()
                .filter(|i| i.class == Class::Missing)
                .count(),
            1,
        );
        let mut report = Report::default();
        report.add(Path::new("synthetic"), census);
        let c = counts(report.dlls["engine.dll"].functions.values());
        assert_eq!((c.total(), c.title, c.missing), (2, 1, 1));

        images[1].1.exports[0].rva = None;
        images[1].1.exports[0].forward = Some("absent.Start".into());
        let mut report = Report::default();
        report.add(
            Path::new("synthetic"),
            Census::of_images(&images, &resolver, &wanted, StubDb::builtin()),
        );
        let c = counts(report.dlls["engine.dll"].functions.values());
        assert_eq!(
            (c.total(), c.unverified, c.missing, c.resolved()),
            (2, 1, 1, 0)
        );
        assert_eq!(report.unverified.len(), 1);
        assert!(
            report
                .unverified
                .contains_key(&("engine.dll".into(), ImportSymbol::Ordinal(1)))
        );
        assert_eq!(report.missing.len(), 1);
        assert!(
            report
                .missing
                .contains_key(&("engine.dll".into(), name("#1")))
        );
    }

    #[test]
    fn lists_keep_symbol_kinds_but_ignore_name_hints() {
        let exe = image(
            false,
            vec![imports(
                "engine.dll",
                [
                    ImportSymbol::Ordinal(1),
                    name("#1"),
                    ImportSymbol::Name {
                        hint: u16::MAX,
                        name: "#1".into(),
                    },
                ],
            )],
            &[],
        );
        let resolver = HleResolver::new(HostPolicy::none());
        let census = Census::of_images(
            &[("sample.exe".into(), exe)],
            &resolver,
            &|_| false,
            StubDb::builtin(),
        );
        let mut report = Report::default();
        report.add(Path::new("synthetic"), census);

        let c = counts(report.dlls["engine.dll"].functions.values());
        assert_eq!((c.total(), c.missing), (2, 2));
        assert_eq!(report.missing.len(), 2);
        assert!(report.missing.values().all(|users| users.len() == 1));
        let json = to_json(&report);
        assert_eq!(json.matches("\"symbol\": \"#1\"").count(), 2);
        assert!(json.contains("\"format\": \"xtp-census 1\""));
        assert_eq!(to_text(&report, true).matches("engine.dll!#1").count(), 2);
    }

    #[test]
    fn typed_symbols_keep_lexical_display_order() {
        let mut list = ImportList::new();
        for symbol in [
            name("Alpha"),
            ImportSymbol::Ordinal(2),
            ImportSymbol::Ordinal(10),
        ] {
            list.insert(("engine.dll".into(), symbol), BTreeSet::new());
        }
        let symbols: Vec<_> = sorted_rows(&list)
            .into_iter()
            .map(|((_, symbol), _)| symbol.to_string())
            .collect();
        assert_eq!(symbols, ["#10", "#2", "Alpha"]);
    }

    #[test]
    fn scans_only_the_requested_files() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        let nested = subdir.join("nested.dll");
        let top = dir.path().join("top.dll");
        std::fs::write(&top, b"MZ broken").unwrap();
        std::fs::write(&nested, b"MZ broken").unwrap();
        let report = scan(vec![dir.path().to_owned(), nested.clone()], false);
        assert_eq!(report.unreadable.len(), 2);
        assert_eq!(report.unreadable[0].0, top);
        assert_eq!(report.unreadable[1].0, nested);
    }

    #[test]
    fn empty_census_json_emits_empty_arrays() {
        let out = to_json(&Report::default());
        assert!(out.contains(r#""format": "xtp-census 1""#));
        assert!(out.contains(r#""dlls": []"#));
        assert!(out.contains(r#""missing": []"#));
        assert!(out.contains(r#""stubbed": []"#));
        assert!(out.contains(r#""unverified": []"#));
    }
}
