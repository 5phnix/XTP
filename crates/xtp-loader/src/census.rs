//! Which imports of a title folder bind, and to what, without running
//! anything: a dry run of binding over every image of the folder.
//!
//! Each import is classified in binding's order (see
//! [`LoadedImage::bind_imports`](crate::LoadedImage)): the resolver (our
//! runtime, then the host), then the exports of the title DLLs the folder
//! ships, then the same again by name for an ordinal import the stub table
//! names; what is left is *stubbed* if the stub table gives it a value
//! (it keeps going under `UnimplementedPolicy::Table`) and *missing*
//! otherwise (it traps).
//!
//! [`Census::expectation`] writes the stubbed and missing lists as text
//! (public API names and counts only, never anything else from the
//! images), and [`Census::compare`] checks a later census against it, so
//! coverage can only grow unnoticed, never shrink.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use xtp_pe::{ImageInfo, ImportSymbol};

use crate::{ImportResolver, Provenance, Resolution, StubDb};

/// Where one import goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Hle,
    Host,
    /// An export of a DLL the title ships.
    Title,
    /// Unresolved; the stub table gives it this value.
    Stubbed(u64),
    /// Unresolved and not in the stub table: it traps.
    Missing,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Hle => "hle",
            Class::Host => "host",
            Class::Title => "title",
            Class::Stubbed(_) => "stubbed",
            Class::Missing => "missing",
        }
    }
}

/// One import of one image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// File name of the importing image.
    pub image: String,
    /// The DLL as the image names it.
    pub dll: String,
    /// The function, or `#n` for an ordinal import.
    pub symbol: String,
    pub class: Class,
}

impl Import {
    /// `dll!symbol`, the DLL in lower case: the key the lists use.
    pub fn key(&self) -> String {
        format!("{}!{}", self.dll.to_ascii_lowercase(), self.symbol)
    }
}

/// Counts by class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub hle: usize,
    pub host: usize,
    pub title: usize,
    pub stubbed: usize,
    pub missing: usize,
}

impl Counts {
    fn add(&mut self, class: Class) {
        match class {
            Class::Hle => self.hle += 1,
            Class::Host => self.host += 1,
            Class::Title => self.title += 1,
            Class::Stubbed(_) => self.stubbed += 1,
            Class::Missing => self.missing += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.hle + self.host + self.title + self.stubbed + self.missing
    }

    pub fn resolved(&self) -> usize {
        self.hle + self.host + self.title
    }
}

/// The classified imports of a set of images.
#[derive(Debug, Clone, Default)]
pub struct Census {
    /// The images classified, by file name, sorted.
    pub images: Vec<String>,
    /// Files that start like a PE image but are not one we can bind, with why.
    pub unreadable: Vec<(String, String)>,
    /// Every import of every image (static and delay-loaded).
    pub imports: Vec<Import>,
    /// BLAKE3 of the folder's executable, which names the build.
    pub build: Option<String>,
}

/// How a census differs from an expectation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drift {
    /// Missing now, but not listed as missing: coverage lost.
    pub regressions: Vec<String>,
    /// Listed as missing, now resolved or stubbed: the expectation can be
    /// updated.
    pub improvements: Vec<String>,
    /// The expectation is for another build (its hash), so nothing was
    /// compared.
    pub other_build: Option<String>,
}

/// An image's exports, to resolve imports of the title DLL it is.
struct Exports<'a> {
    by_name: BTreeSet<&'a str>,
    by_ordinal: BTreeSet<u16>,
}

impl<'a> Exports<'a> {
    fn of(info: &'a ImageInfo) -> Self {
        Exports {
            by_name: info
                .exports
                .iter()
                .filter_map(|e| e.name.as_deref())
                .collect(),
            by_ordinal: info.exports.iter().map(|e| e.ordinal).collect(),
        }
    }

    fn has(&self, symbol: &ImportSymbol) -> bool {
        match symbol {
            ImportSymbol::Name { name, .. } => self.by_name.contains(name.as_str()),
            ImportSymbol::Ordinal(o) => self.by_ordinal.contains(o),
        }
    }
}

impl Census {
    /// Classifies every image in `dir` (not its subfolders: the loader maps
    /// title DLLs from the title's own folder only). `wanted` says which
    /// DLL names may come from the folder, as for
    /// [`load_title_dlls`](crate::load_title_dlls).
    pub fn of_folder(
        dir: &Path,
        resolver: &dyn ImportResolver,
        wanted: &dyn Fn(&str) -> bool,
        stubs: &StubDb,
    ) -> Result<Census> {
        let mut images = Vec::new();
        let mut unreadable = Vec::new();
        let mut build = None;
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .with_context(|| format!("listing {}", dir.display()))?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .collect();
        entries.sort();
        for path in entries {
            if !starts_like_pe(&path) {
                continue;
            }
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            match ImageInfo::parse(&bytes) {
                Ok(info) => {
                    if !info.is_dll() && build.is_none() {
                        build = Some(blake3::hash(&bytes).to_hex().to_string());
                    }
                    images.push((name, info));
                }
                Err(e) => unreadable.push((name, e.to_string())),
            }
        }
        let mut census = Census::of_images(&images, resolver, wanted, stubs);
        census.unreadable = unreadable;
        census.build = build;
        Ok(census)
    }

    /// Classifies the imports of `images` (file name, parsed image), which
    /// are also the title DLLs available to each other.
    pub fn of_images(
        images: &[(String, ImageInfo)],
        resolver: &dyn ImportResolver,
        wanted: &dyn Fn(&str) -> bool,
        stubs: &StubDb,
    ) -> Census {
        let dlls: Vec<(String, Exports)> = images
            .iter()
            .filter(|(name, info)| info.is_dll() && wanted(&name.to_ascii_lowercase()))
            .map(|(name, info)| (name.to_ascii_lowercase(), Exports::of(info)))
            .collect();
        let found = |dll: &str, symbol: &ImportSymbol| -> Option<Class> {
            match resolver.resolve(dll, symbol) {
                Resolution::Found { provenance, .. } => Some(match provenance {
                    Provenance::Hle => Class::Hle,
                    Provenance::Host => Class::Host,
                    Provenance::Title => Class::Title,
                }),
                Resolution::Missing => dlls
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(dll))
                    .is_some_and(|(_, exports)| exports.has(symbol))
                    .then_some(Class::Title),
            }
        };
        let classify = |dll: &str, symbol: &ImportSymbol| -> Class {
            if let Some(class) = found(dll, symbol) {
                return class;
            }
            if let ImportSymbol::Ordinal(o) = symbol
                && let Some(name) = stubs.ordinal_name(dll, *o)
            {
                let by_name = ImportSymbol::Name {
                    hint: 0,
                    name: name.to_owned(),
                };
                if let Some(class) = found(dll, &by_name) {
                    return class;
                }
            }
            stubs
                .value(dll, symbol)
                .map_or(Class::Missing, Class::Stubbed)
        };
        let mut census = Census::default();
        for (image, info) in images {
            census.images.push(image.clone());
            for module in info.imports.iter().chain(&info.delay_imports) {
                for entry in &module.entries {
                    census.imports.push(Import {
                        image: image.clone(),
                        dll: module.dll.clone(),
                        symbol: entry.symbol.to_string(),
                        class: classify(&module.dll, &entry.symbol),
                    });
                }
            }
        }
        census.images.sort();
        census
    }

    /// Counts by class, every import of every image.
    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for import in &self.imports {
            counts.add(import.class);
        }
        counts
    }

    /// Counts by class of the imports of `image`.
    pub fn counts_of(&self, image: &str) -> Counts {
        let mut counts = Counts::default();
        for import in self.imports.iter().filter(|i| i.image == image) {
            counts.add(import.class);
        }
        counts
    }

    /// Each distinct `dll!symbol` in `class`'s kind (stubbed or missing, for
    /// example), with the images that import it.
    fn listed(&self, kind: &str) -> BTreeMap<String, (Class, BTreeSet<&str>)> {
        let mut listed: BTreeMap<String, (Class, BTreeSet<&str>)> = BTreeMap::new();
        for import in self.imports.iter().filter(|i| i.class.name() == kind) {
            listed
                .entry(import.key())
                .or_insert_with(|| (import.class, BTreeSet::new()))
                .1
                .insert(&import.image);
        }
        listed
    }

    /// Distinct `dll!symbol` keys that are missing.
    pub fn missing(&self) -> BTreeSet<String> {
        self.listed("missing").into_keys().collect()
    }

    /// Distinct `dll!symbol` keys that are stubbed.
    pub fn stubbed(&self) -> BTreeSet<String> {
        self.listed("stubbed").into_keys().collect()
    }

    /// For people: counts per image and in all, then every stubbed and
    /// missing import with the images that use it.
    pub fn report(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let row = |out: &mut String, name: &str, c: Counts| {
            let _ = writeln!(
                out,
                "{name:<40} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8}",
                c.total(),
                c.hle,
                c.host,
                c.title,
                c.stubbed,
                c.missing
            );
        };
        let _ = writeln!(
            out,
            "{} images, {} unreadable{}",
            self.images.len(),
            self.unreadable.len(),
            self.build
                .as_deref()
                .map(|b| format!(", build {b}"))
                .unwrap_or_default()
        );
        for (name, why) in &self.unreadable {
            let _ = writeln!(out, "  skipped {name}: {why}");
        }
        let _ = writeln!(
            out,
            "{:<40} {:>6} {:>6} {:>6} {:>6} {:>8} {:>8}",
            "image", "total", "hle", "host", "title", "stubbed", "missing"
        );
        for image in &self.images {
            row(&mut out, image, self.counts_of(image));
        }
        row(&mut out, "ALL", self.counts());
        for kind in ["stubbed", "missing"] {
            let listed = self.listed(kind);
            let _ = writeln!(out, "\n{kind} ({} distinct)", listed.len());
            for (key, (class, users)) in &listed {
                let users: Vec<_> = users.iter().copied().collect();
                let value = match class {
                    Class::Stubbed(v) => format!(" -> {v:#x}"),
                    _ => String::new(),
                };
                let _ = writeln!(out, "  {key}{value}  ({})", users.join(", "));
            }
        }
        out
    }

    /// The text [`Self::compare`] reads: the build, counts, and every
    /// stubbed and missing `dll!symbol`, one per line.
    pub fn expectation(&self) -> String {
        use std::fmt::Write;
        let c = self.counts();
        let mut out = String::from(
            "# Import census of a title folder (xtp-loader `Census::expectation`).\n\
             # Names and counts only. Regenerate with XTP_CENSUS_BLESS=1, see\n\
             # crates/xtp-loader/tests/census_sample.rs.\n",
        );
        if let Some(build) = &self.build {
            let _ = writeln!(out, "build {build}");
        }
        let _ = writeln!(out, "images {}", self.images.len());
        let _ = writeln!(
            out,
            "imports {} hle {} host {} title {} stubbed {} missing {}",
            c.total(),
            c.hle,
            c.host,
            c.title,
            c.stubbed,
            c.missing
        );
        for key in self.stubbed() {
            let _ = writeln!(out, "stubbed {key}");
        }
        for key in self.missing() {
            let _ = writeln!(out, "missing {key}");
        }
        out
    }

    /// Compares this census with an [`Self::expectation`] text.
    pub fn compare(&self, expected: &str) -> Drift {
        let mut build = None;
        let mut was_missing = BTreeSet::new();
        for line in expected.lines() {
            let line = line.trim();
            if let Some(b) = line.strip_prefix("build ") {
                build = Some(b.trim().to_owned());
            } else if let Some(key) = line.strip_prefix("missing ") {
                was_missing.insert(key.trim().to_owned());
            }
        }
        if let (Some(expected), Some(ours)) = (&build, &self.build)
            && expected != ours
        {
            return Drift {
                other_build: Some(expected.clone()),
                ..Drift::default()
            };
        }
        let missing = self.missing();
        Drift {
            regressions: missing.difference(&was_missing).cloned().collect(),
            improvements: was_missing.difference(&missing).cloned().collect(),
            other_build: None,
        }
    }
}

/// Cheap pre-filter: an `MZ` header, whatever the extension.
fn starts_like_pe(path: &Path) -> bool {
    let mut magic = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == b"MZ"
}

#[cfg(test)]
mod tests {
    use super::*;
    use xtp_pe::{ExportInfo, ImportEntry, ImportModule};

    fn name(n: &str) -> ImportSymbol {
        ImportSymbol::Name {
            hint: 0,
            name: n.into(),
        }
    }

    fn image(dll: bool, imports: &[(&str, ImportSymbol)], exports: &[&str]) -> ImageInfo {
        let mut modules: Vec<ImportModule> = Vec::new();
        for (i, (from, symbol)) in imports.iter().enumerate() {
            let entry = ImportEntry {
                symbol: symbol.clone(),
                iat_slot_rva: 0x1000 + 8 * i as u32,
            };
            match modules.iter_mut().find(|m| m.dll == *from) {
                Some(m) => m.entries.push(entry),
                None => modules.push(ImportModule {
                    dll: (*from).to_owned(),
                    entries: vec![entry],
                }),
            }
        }
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
            imports: modules,
            delay_imports: Vec::new(),
            exports: exports
                .iter()
                .enumerate()
                .map(|(i, n)| ExportInfo {
                    name: Some((*n).to_owned()),
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

    /// Our runtime serves `console.dll!Init` and `oleaut32.dll!SysFreeString`
    /// by name; the host serves `kernel32.dll`.
    struct Sample;

    impl ImportResolver for Sample {
        fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
            let dll = dll.to_ascii_lowercase();
            let provenance = match (dll.as_str(), symbol) {
                ("console.dll", ImportSymbol::Name { name, .. }) if name == "Init" => {
                    Provenance::Hle
                }
                ("oleaut32.dll", ImportSymbol::Name { name, .. }) if name == "SysFreeString" => {
                    Provenance::Hle
                }
                ("kernel32.dll", _) => Provenance::Host,
                _ => return Resolution::Missing,
            };
            Resolution::Found {
                address: 0x1000,
                provenance,
            }
        }
    }

    const STUBS: &str = r#"
        [ordinals."oleaut32.dll"]
        6 = "SysFreeString"
        [modules."console.dll"]
        default = "E_NOTIMPL"
    "#;

    fn sample() -> Census {
        let exe = image(
            false,
            &[
                ("console.dll", name("Init")),
                ("console.dll", name("Other")),
                ("KERNEL32.dll", name("GetTickCount")),
                ("Engine.dll", name("EngineStart")),
                ("Engine.dll", name("EngineGone")),
                ("oleaut32.dll", ImportSymbol::Ordinal(6)),
                ("oleaut32.dll", ImportSymbol::Ordinal(9)),
                ("nobody.dll", name("Nothing")),
            ],
            &[],
        );
        let engine = image(true, &[("nobody.dll", name("Nothing"))], &["EngineStart"]);
        let stubs = StubDb::parse(STUBS).unwrap();
        let images = [
            ("game.exe".to_owned(), exe),
            ("Engine.dll".to_owned(), engine),
        ];
        Census::of_images(&images, &Sample, &|_| true, &stubs)
    }

    #[test]
    fn classifies_in_binding_order() {
        let census = sample();
        let class = |key: &str| {
            census
                .imports
                .iter()
                .find(|i| i.image == "game.exe" && i.key() == key)
                .map(|i| i.class)
        };
        assert_eq!(class("console.dll!Init"), Some(Class::Hle));
        assert_eq!(
            class("console.dll!Other"),
            Some(Class::Stubbed(0x8000_4001))
        );
        assert_eq!(class("kernel32.dll!GetTickCount"), Some(Class::Host));
        assert_eq!(class("engine.dll!EngineStart"), Some(Class::Title));
        assert_eq!(class("engine.dll!EngineGone"), Some(Class::Missing));
        // Named by the table, then served by name.
        assert_eq!(class("oleaut32.dll!#6"), Some(Class::Hle));
        assert_eq!(class("oleaut32.dll!#9"), Some(Class::Missing));
        let all = census.counts();
        assert_eq!((all.hle, all.host, all.title), (2, 1, 1));
        assert_eq!((all.stubbed, all.missing, all.total()), (1, 4, 9));
        assert_eq!(census.counts_of("Engine.dll").missing, 1);
        let missing: Vec<_> = census.missing().into_iter().collect();
        assert_eq!(
            missing,
            [
                "engine.dll!EngineGone",
                "nobody.dll!Nothing",
                "oleaut32.dll!#9"
            ]
        );
        let report = census.report();
        assert!(
            report.contains("nobody.dll!Nothing  (Engine.dll, game.exe)"),
            "{report}"
        );
        assert!(
            report.contains("console.dll!Other -> 0x80004001"),
            "{report}"
        );
    }

    #[test]
    fn title_dlls_only_serve_what_wanted_allows() {
        let exe = image(false, &[("Engine.dll", name("EngineStart"))], &[]);
        let engine = image(true, &[], &["EngineStart"]);
        let images = [
            ("game.exe".to_owned(), exe),
            ("Engine.dll".to_owned(), engine),
        ];
        let stubs = StubDb::default();
        let refused = Census::of_images(&images, &Sample, &|dll| dll != "engine.dll", &stubs);
        assert_eq!(refused.counts().missing, 1);
        let taken = Census::of_images(&images, &Sample, &|_| true, &stubs);
        assert_eq!(taken.counts().title, 1);
    }

    #[test]
    fn an_expectation_catches_lost_coverage() {
        let mut census = sample();
        census.build = Some("abc".into());
        let expected = census.expectation();
        assert!(expected.contains("\nbuild abc\n"), "{expected}");
        assert!(
            expected.contains("\nstubbed console.dll!Other\n"),
            "{expected}"
        );
        assert!(
            expected.contains("\nmissing nobody.dll!Nothing\n"),
            "{expected}"
        );
        assert_eq!(census.compare(&expected), Drift::default());

        // One import resolves now, another no longer does.
        census.imports.retain(|i| i.key() != "nobody.dll!Nothing");
        let init = census
            .imports
            .iter_mut()
            .find(|i| i.key() == "console.dll!Init")
            .unwrap();
        init.class = Class::Missing;
        let drift = census.compare(&expected);
        assert_eq!(drift.regressions, ["console.dll!Init"]);
        assert_eq!(drift.improvements, ["nobody.dll!Nothing"]);

        census.build = Some("def".into());
        assert_eq!(
            census.compare(&expected).other_build.as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn a_folder_census_reads_only_pe_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readme.txt"), "not an image").unwrap();
        std::fs::write(dir.path().join("broken.dll"), b"MZ but nothing else").unwrap();
        let census = Census::of_folder(dir.path(), &Sample, &|_| true, &StubDb::default()).unwrap();
        assert!(census.images.is_empty());
        assert_eq!(census.unreadable.len(), 1);
        assert_eq!(census.unreadable[0].0, "broken.dll");
        assert_eq!(census.build, None);
    }
}
