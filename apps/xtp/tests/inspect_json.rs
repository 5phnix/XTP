//! Integration tests for `xtp inspect --json` and `xtp census --json` on the
//! synthetic `hle-probe` fixture (no game files required).
#![cfg(windows)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn build_probe() -> PathBuf {
    let manifest = workspace_root().join("tests/fixtures/hle-probe/Cargo.toml");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hle-probe-json");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .status()
        .expect("running cargo");
    assert!(status.success(), "building hle-probe failed");
    target.join("debug").join("hle_probe.dll")
}

/// Minimal strict JSON value + parser so tests validate syntax and structure
/// without adding a crate dependency.
#[derive(Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(i64),
    Str(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    fn as_object(&self) -> &BTreeMap<String, Json> {
        match self {
            Json::Object(map) => map,
            other => panic!("expected JSON object, got {other:?}"),
        }
    }

    fn as_array(&self) -> &[Json] {
        match self {
            Json::Array(arr) => arr,
            other => panic!("expected JSON array, got {other:?}"),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Json::Str(s) => s,
            other => panic!("expected JSON string, got {other:?}"),
        }
    }

    fn as_i64(&self) -> i64 {
        match self {
            Json::Number(n) => *n,
            other => panic!("expected JSON number, got {other:?}"),
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn parse(input: &'a str) -> Json {
        let mut p = Self {
            bytes: input.as_bytes(),
            pos: 0,
        };
        let value = p.value();
        p.skip_ws();
        assert_eq!(p.pos, p.bytes.len(), "trailing bytes after JSON value");
        value
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\n' | b'\r' | b'\t') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&mut self) -> u8 {
        self.skip_ws();
        *self.bytes.get(self.pos).expect("unexpected EOF in JSON")
    }

    fn take(&mut self, expected: u8) {
        let got = self.peek();
        assert_eq!(
            got, expected,
            "expected {:?}, got {:?} at byte {}",
            expected as char, got as char, self.pos
        );
        self.pos += 1;
    }

    fn value(&mut self) -> Json {
        match self.peek() {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Json::Str(self.string()),
            b't' => {
                assert!(self.bytes[self.pos..].starts_with(b"true"));
                self.pos += 4;
                Json::Bool(true)
            }
            b'f' => {
                assert!(self.bytes[self.pos..].starts_with(b"false"));
                self.pos += 5;
                Json::Bool(false)
            }
            b'n' => {
                assert!(self.bytes[self.pos..].starts_with(b"null"));
                self.pos += 4;
                Json::Null
            }
            b'-' | b'0'..=b'9' => self.number(),
            other => panic!("unexpected byte {:?} at {}", other as char, self.pos),
        }
    }

    fn object(&mut self) -> Json {
        self.take(b'{');
        let mut map = BTreeMap::new();
        if self.peek() == b'}' {
            self.pos += 1;
            return Json::Object(map);
        }
        loop {
            let key = self.string();
            self.take(b':');
            let val = self.value();
            assert!(map.insert(key, val).is_none(), "duplicate JSON key");
            match self.peek() {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    break;
                }
                other => panic!("expected ',' or '}}', got {:?}", other as char),
            }
        }
        Json::Object(map)
    }

    fn array(&mut self) -> Json {
        self.take(b'[');
        let mut items = Vec::new();
        if self.peek() == b']' {
            self.pos += 1;
            return Json::Array(items);
        }
        loop {
            items.push(self.value());
            match self.peek() {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    break;
                }
                other => panic!("expected ',' or ']', got {:?}", other as char),
            }
        }
        Json::Array(items)
    }

    fn string(&mut self) -> String {
        self.take(b'"');
        let mut out = String::new();
        while let Some(&b) = self.bytes.get(self.pos) {
            self.pos += 1;
            match b {
                b'"' => return out,
                b'\\' => {
                    let esc = *self.bytes.get(self.pos).expect("EOF in escape");
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
                                .expect("unicode escape");
                            self.pos += 4;
                            let code = u32::from_str_radix(hex, 16).expect("hex codepoint");
                            out.push(char::from_u32(code).expect("valid char"));
                        }
                        other => panic!("unsupported escape \\{}", other as char),
                    }
                }
                other => out.push(other as char),
            }
        }
        panic!("unterminated JSON string");
    }

    fn number(&mut self) -> Json {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while matches!(self.bytes.get(self.pos), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).expect("number utf8");
        Json::Number(text.parse().expect("valid integer"))
    }
}

#[test]
fn inspect_and_census_json_on_hle_probe() {
    let probe = build_probe();
    let xtp = env!("CARGO_BIN_EXE_xtp");

    let out = Command::new(xtp)
        .args(["inspect", "--json"])
        .arg(&probe)
        .output()
        .expect("running xtp inspect --json");
    assert!(
        out.status.success(),
        "xtp inspect --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let doc = Parser::parse(&stdout);
    let obj = doc.as_object();
    assert_eq!(obj["format"].as_str(), "xtp-inspect 1");
    assert!(obj["file"].as_str().ends_with("hle_probe.dll"));
    assert_eq!(obj["machine"].as_str(), "0x8664");
    assert!(obj["image_base"].as_str().starts_with("0x"));

    let imports = obj["imports"].as_array();
    let has_hle_xgr = imports.iter().any(|item| {
        let m = item.as_object();
        m["dll"].as_str().eq_ignore_ascii_case("xgameruntime.dll")
            && m["resolution"].as_str() == "hle"
    });
    assert!(
        has_hle_xgr,
        "expected xgameruntime.dll as hle in inspect --json: {stdout}"
    );

    let has_missing_probe = imports.iter().any(|item| {
        let m = item.as_object();
        m["dll"].as_str().contains("probe-missing") && m["resolution"].as_str() == "missing"
    });
    assert!(
        has_missing_probe,
        "expected missing probe DLL as missing in inspect --json: {stdout}"
    );

    let census_out = Command::new(xtp)
        .args(["census", "--json"])
        .arg(&probe)
        .output()
        .expect("running xtp census --json");
    assert!(
        census_out.status.success(),
        "xtp census --json failed: {}",
        String::from_utf8_lossy(&census_out.stderr)
    );
    let census_stdout = String::from_utf8(census_out.stdout).expect("utf8 stdout");
    let census = Parser::parse(&census_stdout);
    let cobj = census.as_object();
    assert_eq!(cobj["format"].as_str(), "xtp-census 1");
    assert_eq!(cobj["images"].as_i64(), 1);
    let totals = cobj["totals"].as_object();
    assert!(totals["hle"].as_i64() >= 1);
    assert!(totals["missing"].as_i64() >= 1);
    let missing_list = cobj["missing"].as_array();
    assert!(
        missing_list
            .iter()
            .any(|item| item.as_object()["dll"].as_str().contains("probe-missing")),
        "expected missing probe DLL in census --json: {census_stdout}"
    );
}
