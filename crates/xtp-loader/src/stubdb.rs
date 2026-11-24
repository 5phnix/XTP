//! What an import nobody implements returns, per DLL and per function.
//!
//! Under [`UnimplementedPolicy::Table`](crate::UnimplementedPolicy) an
//! unresolved import returns the value this table gives it, and traps like
//! [`UnimplementedPolicy::Trap`](crate::UnimplementedPolicy) if the table
//! does not list it: only what someone decided is safe to stub keeps the
//! title going. The table also names ordinal imports, so a resolver that
//! serves a function by name serves its ordinal import too (see
//! [`WithOrdinalNames`]).
//!
//! The built-in table is `data/stubs.toml` (our own values, from the public
//! headers' return types); [`StubDb::parse`] reads one in the same format:
//!
//! ```toml
//! # Ordinal imports, by name.
//! [ordinals."oleaut32.dll"]
//! 4 = "SysAllocStringLen"
//!
//! # `default` for every function of the DLL (a trailing `*` matches a
//! # prefix, for versioned API sets), `functions` for single ones.
//! [modules."xgameruntime.dll"]
//! default = "E_NOTIMPL"
//! functions = { XGameRuntimeIsFeatureAvailable = 0 }
//! ```
//!
//! A value is an integer (`0x80004001`, `-1`) or one of the names in
//! [`named_value`].

use std::collections::HashMap;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use xtp_pe::ImportSymbol;

use crate::{ImportResolver, Resolution};

/// The values one DLL's (or one family of DLLs') stubs return.
#[derive(Debug, Clone, Default)]
struct ModuleStubs {
    /// Lower case; a trailing `*` matches a prefix.
    pattern: String,
    default: Option<u64>,
    /// By function name, or `#n` for an ordinal import.
    functions: HashMap<String, u64>,
}

impl ModuleStubs {
    /// How closely the pattern names `dll` (lower case): an exact name
    /// beats any prefix, a longer prefix a shorter one.
    fn specificity(&self, dll: &str) -> Option<usize> {
        match self.pattern.strip_suffix('*') {
            Some(prefix) => dll.starts_with(prefix).then_some(prefix.len()),
            None => (dll == self.pattern).then_some(usize::MAX),
        }
    }
}

/// The stub table: return values and ordinal names.
#[derive(Debug, Clone, Default)]
pub struct StubDb {
    /// (lower-case DLL, ordinal) → function name.
    ordinals: HashMap<(String, u16), String>,
    modules: Vec<ModuleStubs>,
}

static BUILTIN: LazyLock<StubDb> = LazyLock::new(|| {
    StubDb::parse(include_str!("../data/stubs.toml"))
        .expect("data/stubs.toml parses (checked by a test)")
});

/// A value name the table may use instead of a number.
pub fn named_value(name: &str) -> Option<u64> {
    let hresult = |v: u32| Some(u64::from(v));
    match name {
        "S_OK" | "FALSE" | "NULL" => Some(0),
        "S_FALSE" | "TRUE" => Some(1),
        "E_NOTIMPL" => hresult(0x8000_4001),
        "E_FAIL" => hresult(0x8000_4005),
        "E_ABORT" => hresult(0x8000_4004),
        "E_INVALIDARG" => hresult(0x8007_0057),
        "E_OUTOFMEMORY" => hresult(0x8007_000E),
        "E_ACCESSDENIED" => hresult(0x8007_0005),
        "INVALID_HANDLE_VALUE" => Some(u64::MAX),
        _ => None,
    }
}

fn value(v: &toml::Value) -> Result<u64> {
    match v {
        toml::Value::Integer(i) => Ok(*i as u64),
        toml::Value::String(s) => named_value(s).with_context(|| format!("unknown value {s:?}")),
        other => bail!("a value is an integer or a name, not {other}"),
    }
}

impl StubDb {
    /// `data/stubs.toml`.
    pub fn builtin() -> &'static StubDb {
        &BUILTIN
    }

    /// Reads a table in the format of `data/stubs.toml`.
    pub fn parse(text: &str) -> Result<StubDb> {
        let table: toml::Table = text.parse().context("parsing the stub table")?;
        let mut db = StubDb::default();
        for (key, item) in &table {
            let Some(entries) = item.as_table() else {
                bail!("`{key}` is not a table");
            };
            match key.as_str() {
                "ordinals" => {
                    for (dll, names) in entries {
                        let names = names
                            .as_table()
                            .with_context(|| format!("ordinals.{dll:?} is not a table"))?;
                        for (ordinal, name) in names {
                            let ordinal: u16 = ordinal
                                .parse()
                                .with_context(|| format!("ordinal {ordinal:?} of {dll}"))?;
                            let name = name
                                .as_str()
                                .with_context(|| format!("{dll} #{ordinal}: not a name"))?;
                            db.ordinals
                                .insert((dll.to_ascii_lowercase(), ordinal), name.to_owned());
                        }
                    }
                }
                "modules" => {
                    for (dll, stubs) in entries {
                        let stubs = stubs
                            .as_table()
                            .with_context(|| format!("modules.{dll:?} is not a table"))?;
                        let mut module = ModuleStubs {
                            pattern: dll.to_ascii_lowercase(),
                            ..ModuleStubs::default()
                        };
                        for (field, v) in stubs {
                            match field.as_str() {
                                "default" => {
                                    module.default =
                                        Some(value(v).with_context(|| format!("{dll} default"))?);
                                }
                                "functions" => {
                                    let functions = v.as_table().with_context(|| {
                                        format!("modules.{dll:?}.functions is not a table")
                                    })?;
                                    for (name, v) in functions {
                                        let v =
                                            value(v).with_context(|| format!("{dll}!{name}"))?;
                                        module.functions.insert(name.clone(), v);
                                    }
                                }
                                other => bail!("modules.{dll:?}: unknown field `{other}`"),
                            }
                        }
                        db.modules.push(module);
                    }
                }
                other => bail!("unknown section `{other}`"),
            }
        }
        Ok(db)
    }

    /// The name of `dll`'s export `ordinal`, if the table knows it.
    pub fn ordinal_name(&self, dll: &str, ordinal: u16) -> Option<&str> {
        self.ordinals
            .get(&(dll.to_ascii_lowercase(), ordinal))
            .map(String::as_str)
    }

    /// What a stub for `dll!symbol` returns, or `None` if the table does
    /// not list it. A function's own entry beats any DLL default; among
    /// either, the entry that names `dll` most closely wins. An ordinal
    /// import matches `#n` or the name [`Self::ordinal_name`] gives it.
    pub fn value(&self, dll: &str, symbol: &ImportSymbol) -> Option<u64> {
        let dll = dll.to_ascii_lowercase();
        let mut matching: Vec<(usize, &ModuleStubs)> = self
            .modules
            .iter()
            .filter_map(|m| m.specificity(&dll).map(|s| (s, m)))
            .collect();
        matching.sort_by_key(|&(specificity, _)| std::cmp::Reverse(specificity));
        let keys: Vec<String> = match symbol {
            ImportSymbol::Name { name, .. } => vec![name.clone()],
            ImportSymbol::Ordinal(o) => self
                .ordinal_name(&dll, *o)
                .map(str::to_owned)
                .into_iter()
                .chain([format!("#{o}")])
                .collect(),
        };
        let function = matching
            .iter()
            .find_map(|(_, m)| keys.iter().find_map(|k| m.functions.get(k).copied()));
        function.or_else(|| matching.iter().find_map(|(_, m)| m.default))
    }
}

/// Resolves through `inner`; an ordinal import `inner` cannot resolve is
/// asked again by the name `names` gives it. Binding wraps every resolver
/// in this with the built-in table, and the census does the same.
pub struct WithOrdinalNames<'a> {
    pub inner: &'a dyn ImportResolver,
    pub names: &'a StubDb,
}

impl ImportResolver for WithOrdinalNames<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let found = self.inner.resolve(dll, symbol);
        if found != Resolution::Missing {
            return found;
        }
        let ImportSymbol::Ordinal(ordinal) = symbol else {
            return found;
        };
        match self.names.ordinal_name(dll, *ordinal) {
            Some(name) => {
                let by_name = ImportSymbol::Name {
                    hint: 0,
                    name: name.to_owned(),
                };
                self.inner.resolve(dll, &by_name)
            }
            None => found,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provenance;

    fn name(n: &str) -> ImportSymbol {
        ImportSymbol::Name {
            hint: 0,
            name: n.into(),
        }
    }

    const SAMPLE: &str = r##"
        [ordinals."OleAut32.dll"]
        4 = "SysAllocStringLen"

        [modules."console-hresult.dll"]
        default = "E_NOTIMPL"
        functions = { IsAvailable = 0, Count = 3 }

        [modules."api-ms-win-sample-*"]
        default = -1

        [modules."api-ms-win-sample-l1-1-0.dll"]
        functions = { Exact = 7 }

        [modules."oleaut32.dll"]
        functions = { SysAllocStringLen = 0, "#6" = 0 }
    "##;

    #[test]
    fn the_builtin_table_parses() {
        let db = StubDb::parse(include_str!("../data/stubs.toml")).unwrap();
        assert_eq!(
            db.ordinal_name("oleaut32.dll", 4),
            Some("SysAllocStringLen")
        );
        assert_eq!(db.ordinal_name("oleaut32.dll", 6), Some("SysFreeString"));
    }

    #[test]
    fn a_listed_hresult_module_returns_e_notimpl() {
        let db = StubDb::parse(SAMPLE).unwrap();
        assert_eq!(
            db.value("Console-HRESULT.dll", &name("Anything")),
            Some(0x8000_4001)
        );
        assert_eq!(db.value("unlisted.dll", &name("Anything")), None);
    }

    #[test]
    fn a_function_entry_overrides_its_module_default() {
        let db = StubDb::parse(SAMPLE).unwrap();
        assert_eq!(
            db.value("console-hresult.dll", &name("IsAvailable")),
            Some(0)
        );
        assert_eq!(db.value("console-hresult.dll", &name("Count")), Some(3));
        // A prefix pattern's default, an exact entry's function, and the
        // prefix default again for the exact DLL's other functions.
        assert_eq!(
            db.value("api-ms-win-sample-l1-2-0.dll", &name("F")),
            Some(u64::MAX)
        );
        assert_eq!(
            db.value("api-ms-win-sample-l1-1-0.dll", &name("Exact")),
            Some(7)
        );
        assert_eq!(
            db.value("api-ms-win-sample-l1-1-0.dll", &name("Other")),
            Some(u64::MAX)
        );
    }

    #[test]
    fn ordinal_imports_match_by_name_or_number() {
        let db = StubDb::parse(SAMPLE).unwrap();
        assert_eq!(db.value("oleaut32.dll", &ImportSymbol::Ordinal(4)), Some(0));
        assert_eq!(db.value("oleaut32.dll", &ImportSymbol::Ordinal(6)), Some(0));
        assert_eq!(db.value("oleaut32.dll", &ImportSymbol::Ordinal(9)), None);
    }

    #[test]
    fn bad_tables_are_refused() {
        for bad in [
            "[modules.\"a.dll\"]\ndefault = \"E_SOMETHING\"",
            "[modules.\"a.dll\"]\nreturn = 0",
            "[ordinals.\"a.dll\"]\nfour = \"F\"",
            "[stubs]\n",
            "[modules.\"a.dll\"]\ndefault = 1.5",
        ] {
            assert!(StubDb::parse(bad).is_err(), "{bad}");
        }
    }

    struct ByName;

    impl ImportResolver for ByName {
        fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
            match symbol {
                ImportSymbol::Name { name, .. }
                    if dll.eq_ignore_ascii_case("oleaut32.dll") && name == "SysAllocStringLen" =>
                {
                    Resolution::Found {
                        address: 0x1234,
                        provenance: Provenance::Hle,
                    }
                }
                _ => Resolution::Missing,
            }
        }
    }

    #[test]
    fn an_ordinal_import_resolves_by_the_name_the_map_gives() {
        let db = StubDb::parse(SAMPLE).unwrap();
        let resolver = WithOrdinalNames {
            inner: &ByName,
            names: &db,
        };
        let found = Resolution::Found {
            address: 0x1234,
            provenance: Provenance::Hle,
        };
        assert_eq!(
            resolver.resolve("OLEAUT32.dll", &ImportSymbol::Ordinal(4)),
            found
        );
        // Not in the map, or nobody serves the name: still missing.
        assert_eq!(
            resolver.resolve("oleaut32.dll", &ImportSymbol::Ordinal(7)),
            Resolution::Missing
        );
        let empty = StubDb::default();
        let unnamed = WithOrdinalNames {
            inner: &ByName,
            names: &empty,
        };
        assert_eq!(
            unnamed.resolve("oleaut32.dll", &ImportSymbol::Ordinal(4)),
            Resolution::Missing
        );
    }
}
