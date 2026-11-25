//! Generate the GFX10 instruction encoding conformance corpus from LLVM MC's
//! public AMDGPU assembly tests (`llvm/test/MC/AMDGPU/*.s`, Apache-2.0 WITH
//! LLVM-exception).
//!
//! Run with `cargo xtask gfx10-corpus` (fetches the pinned LLVM release tag)
//! or `cargo xtask gfx10-corpus --llvm-dir <path/to/llvm/test/MC/AMDGPU>`.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

const LLVM_TAG: &str = "llvmorg-19.1.7";
const RAW_BASE: &str =
    "https://raw.githubusercontent.com/llvm/llvm-project/llvmorg-19.1.7/llvm/test/MC/AMDGPU";

/// Max bytes per generated `.txt` shard (well below `xtask policy`'s 512 KiB
/// limit).
const MAX_SHARD_BYTES: usize = 384 * 1024;

/// Curated GFX10 / GFX10.3 MC assembly test files and their SHA-256 hashes.
const LLVM_FILES: &[(&str, &str)] = &[
    (
        "exp-gfx10.s",
        "0527392f6b9e3425abb608e07ce70d169b7426caf672604c356f1e910c8cbacc",
    ),
    (
        "gfx1011_dlops.s",
        "0cec73f1ae8052e3678698dacd22321fcf7152abb66382be5880f0d7734623fe",
    ),
    (
        "gfx1013.s",
        "dbbb1f77cf86092507f47a5bfb8d45dc040e94fc779acc7c3bd6332abe8ea0e0",
    ),
    (
        "gfx1030_new.s",
        "936237c47c32d9735537bd263800a585eb3a394f111ddd9c7bc95b8375d7bb5a",
    ),
    (
        "gfx10_asm_dpp16.s",
        "0a18a55fe0f501cea9e47dcbbdd4df7a58a2101a0bbb60d0bf8ab2a45a38fa93",
    ),
    (
        "gfx10_asm_dpp8.s",
        "15a0985d273169f56c9e37892e9b6e6a55e413ab7420b3c5148f8f42a75ec7ac",
    ),
    (
        "gfx10_asm_ds.s",
        "8c462ab9691e1043aa85e832e1b2afae504f7bb817237021a0c96cf85a311e43",
    ),
    (
        "gfx10_asm_flat.s",
        "bf866e4f8972212cd161a07eb943263c66cf288a89fca91e7613bfb9d725c16d",
    ),
    (
        "gfx10_asm_mimg.s",
        "566deaba5937a083c5f1946b9b6533c4f37c86ef47f6f03a4b74286d42af9503",
    ),
    (
        "gfx10_asm_mubuf.s",
        "3f236cec0f51812014dd48d60b0b7aa5a1bf592dd2cacaf22510b22e809aa7f8",
    ),
    (
        "gfx10_asm_smem.s",
        "f52132ec99c3ebe50147cf998df55b651964e8cad0efb0c6ea42668847e07c08",
    ),
    (
        "gfx10_asm_sop.s",
        "ea9ed73781994f9b05ef3e07dfd0983d6bdfd617a70d5f467e8c976e750c49b0",
    ),
    (
        "gfx10_asm_vop1.s",
        "68954a299b345c4ab8516e3acb4da6d67f70bf06dd4f35232ae0d80b6f0f9dce",
    ),
    (
        "gfx10_asm_vop2.s",
        "ba8265579515782f66af207153aff2779624a3021ac89636f7dc5b702c8895d1",
    ),
    (
        "gfx10_asm_vop3.s",
        "166ba14de355b3784b64bebc67f40b1c7635510bf3b00d01e6648b0fa6934ad6",
    ),
    (
        "gfx10_asm_vopc.s",
        "e562f3952a2885ba2a96b2368fac59bbe30558a20021666b812e395a271b94b3",
    ),
    (
        "gfx10_asm_vopc_e64.s",
        "64e4390623ee298e7c48663869b722188d3fdd9fc2864db1ecd0d78bd87c8270",
    ),
    (
        "gfx10_asm_vopc_sdwa.s",
        "fbfc5f1ed33f300cb8a7d67da00c51f6a91c98aa239ca239ed54a0b7b9ef391f",
    ),
    (
        "gfx10_asm_vopcx.s",
        "6166070a711d54726e158de2cc44459e03ce675c1632a12d30475017cb0aecf0",
    ),
    (
        "mtbuf-gfx10.s",
        "1cb248616876c066db286d516135bfd27dad800d33288fa6c79fab2e804e9e4d",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    mnemonic: String,
    words: Vec<u32>,
}

pub fn run(root: &Path, llvm_dir: Option<&Path>, out_dir: Option<&Path>, full: bool) -> Result<()> {
    let out_dir = out_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join("crates/xtp-shader/tests/data"));
    let mut entries = BTreeSet::new();
    for &(file, expected_hash) in LLVM_FILES {
        let bytes = match llvm_dir {
            Some(dir) => std::fs::read(dir.join(file))
                .with_context(|| format!("reading {}", dir.join(file).display()))?,
            None => fetch_llvm_file(file)?,
        };
        let hash = sha256(&bytes).with_context(|| format!("hashing {file}"))?;
        anyhow::ensure!(
            hash == expected_hash,
            "SHA-256 mismatch for {file} at {LLVM_TAG}: expected {expected_hash}, got {hash}"
        );
        let text = std::str::from_utf8(&bytes).with_context(|| format!("non-UTF-8 {file}"))?;
        let before = entries.len();
        parse_llvm_mc(text, &mut entries);
        println!(
            "  {file:<26} +{} unique encodings (total {})",
            entries.len() - before,
            entries.len()
        );
    }

    if entries.is_empty() {
        bail!("no GFX10 encodings extracted");
    }

    if !full {
        entries = reduce_entries(entries);
    }
    let header = shard_header(entries.len(), full);
    let shards = split_shards(&header, &entries, MAX_SHARD_BYTES);
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let mut written = BTreeSet::new();
    for (i, shard) in shards.iter().enumerate() {
        let path = out_dir.join(format!("gfx10-llvm-mc-{:02}.txt", i + 1));
        std::fs::write(&path, shard).with_context(|| format!("writing {}", path.display()))?;
        written.insert(path);
    }

    // Remove shards left over from a larger corpus after writing the new ones.
    for entry in std::fs::read_dir(&out_dir)? {
        let path = entry?.path();
        if !written.contains(&path)
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("gfx10-llvm-mc-") && n.ends_with(".txt"))
        {
            std::fs::remove_file(&path)?;
        }
    }

    println!(
        "wrote {} unique GFX10 encodings across {} shard(s) in {}",
        entries.len(),
        shards.len(),
        out_dir.display()
    );
    Ok(())
}

fn fetch_llvm_file(file: &str) -> Result<Vec<u8>> {
    let url = format!("{RAW_BASE}/{file}");
    let out = Command::new("curl")
        .args(["-fsSL", &url])
        .output()
        .with_context(|| format!("running curl for {url}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("curl failed for {url}: {stderr}");
    }
    Ok(out.stdout)
}

fn sha256(bytes: &[u8]) -> Result<String> {
    // Hash the same bytes the parser uses, whether read locally or by curl.
    let mut command = if cfg!(windows) {
        let mut powershell = Command::new("powershell.exe");
        powershell.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference = 'Stop'; \
             (Get-FileHash -Algorithm SHA256 -InputStream ([Console]::OpenStandardInput())).Hash",
        ]);
        powershell
    } else {
        let mut shasum = Command::new("shasum");
        shasum.args(["-a", "256"]);
        shasum
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting SHA-256 command")?;
    let sent = child.stdin.take().expect("piped stdin").write_all(bytes);
    let output = child.wait_with_output()?;
    anyhow::ensure!(
        output.status.success(),
        "SHA-256 command failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    sent.context("sending bytes to SHA-256 command")?;
    let hash = std::str::from_utf8(&output.stdout)?
        .split_whitespace()
        .next()
        .context("missing SHA-256 digest")?;
    Ok(hash.to_ascii_lowercase())
}

fn reduce_entries(entries: BTreeSet<Entry>) -> BTreeSet<Entry> {
    let mut seen = BTreeSet::new();
    entries
        .iter()
        .filter(|entry| {
            seen.insert((
                entry.mnemonic.as_str(),
                entry.words.len(),
                decoder_selectors(&entry.words),
            ))
        })
        .cloned()
        .collect()
}

/// Keep the encoding family and the selectors used by `gfx10::instruction`
/// for extra dwords and split opcodes. Ordinary operand values share a key.
fn decoder_selectors(words: &[u32]) -> (u32, u32) {
    let w = words[0];
    let w1 = words.get(1).copied().unwrap_or_default();
    if w >> 31 == 0 {
        let family = match w >> 25 {
            tag @ (0x3E | 0x3F) => tag,
            _ => 0,
        };
        let selector = match w & 0x1FF {
            src @ (0xFF | 0xF9 | 0xFA | 0xE9 | 0xEA) => src,
            _ => 0,
        };
        return (0x100 | family, selector);
    }
    if w >> 30 == 0b10 {
        let src0_literal = u32::from(w & 0xFF == 0xFF);
        let src1_literal = u32::from((w >> 8) & 0xFF == 0xFF) << 1;
        return match w >> 23 {
            0x17D => (0x17D, src0_literal),
            0x17E => (0x17E, src0_literal | src1_literal),
            0x17F => (0x17F, 0),
            _ if w >> 28 == 0b1011 => (0xB, 0),
            _ => (0x2, src0_literal | src1_literal),
        };
    }
    let family = w >> 26;
    let selector = match family {
        0x33 | 0x35 => [0, 9, 18]
            .into_iter()
            .enumerate()
            .map(|(i, shift)| u32::from((w1 >> shift) & 0x1FF == 0xFF) << i)
            .sum(),
        0x37 => (w >> 14) & 3,
        0x38 => (w >> 25) & 1,
        0x3A => (w1 >> 21) & 1,
        // Bits 2:1 count NSA dwords. Bit 0 extends the opcode.
        0x3C => w & 7,
        _ => 0,
    };
    (family, selector)
}

fn shard_header(entry_count: usize, full: bool) -> String {
    let command = if full {
        "gfx10-corpus --full"
    } else {
        "gfx10-corpus"
    };
    let mut header = format!(
        "# Generated by `cargo xtask {command}`.\n\
         # SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception\n\
         # Part of the LLVM Project, see https://llvm.org/LICENSE.txt\n\
         # Copyright (c) LLVM Project contributors; see LICENSE-LLVM.txt.\n\
         # Extracted and reformatted from llvm/test/MC/AMDGPU/ at {LLVM_TAG}\n\
         # (mnemonic and encoding only), deduplicated and sorted.\n\
         # Corpus entries: {entry_count}\n\
         # Format: <mnemonic> <word0_hex> [<word1_hex> ...]\n"
    );
    if !full {
        header.push_str("# Reduced by mnemonic, dword count and decoder selectors.\n");
    }
    header.push_str("# SHA-256 for each llvm/test/MC/AMDGPU/ source file at the pinned tag:\n");
    for (file, hash) in LLVM_FILES {
        let _ = writeln!(header, "# {file} {hash}");
    }
    header
}

fn split_shards(header: &str, entries: &BTreeSet<Entry>, max_bytes: usize) -> Vec<String> {
    let mut shards = Vec::new();
    let mut buf = header.to_owned();
    for entry in entries {
        let mut line = entry.mnemonic.clone();
        for w in &entry.words {
            let _ = write!(line, " {w:08x}");
        }
        line.push('\n');
        if buf.len() + line.len() > max_bytes && buf.len() > header.len() {
            shards.push(buf);
            buf = header.to_owned();
        }
        buf.push_str(&line);
    }
    if buf.len() > header.len() {
        shards.push(buf);
    }
    shards
}

fn parse_llvm_mc(text: &str, out: &mut BTreeSet<Entry>) {
    let mut last_mnemonic: Option<String> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let comment = line
            .strip_prefix("//")
            .or_else(|| line.strip_prefix(';'))
            .map(str::trim);

        if let Some(comment) = comment {
            if let Some((prefix, rest)) = comment.split_once(':') {
                let prefix = prefix.trim();
                if is_gfx10_prefix(prefix) {
                    let (before_enc, after_enc) = if let Some(enc_pos) = rest.find("encoding:") {
                        (
                            rest[..enc_pos].trim().trim_end_matches(';').trim(),
                            rest[enc_pos + "encoding:".len()..].trim(),
                        )
                    } else if rest.trim_start().starts_with('[') {
                        ("", rest.trim())
                    } else {
                        ("", "")
                    };
                    if !after_enc.is_empty()
                        && let Some(words) = parse_encoding_bytes(after_enc)
                    {
                        let mnemonic = first_mnemonic(before_enc)
                            .map(str::to_ascii_lowercase)
                            .or_else(|| last_mnemonic.clone());
                        if let Some(mnemonic) = mnemonic {
                            out.insert(Entry { mnemonic, words });
                        }
                    }
                }
            }
            continue;
        }

        // Skip directives and labels.
        if line.starts_with('.') || line.ends_with(':') {
            continue;
        }
        if let Some(mnem) = first_mnemonic(line) {
            last_mnemonic = Some(mnem.to_ascii_lowercase());
        }
    }
}

fn is_gfx10_prefix(prefix: &str) -> bool {
    let upper = prefix.to_ascii_uppercase();
    upper.split(',').all(|prefix| {
        let mut tokens = prefix.split('-');
        matches!(tokens.next(), Some("GFX10" | "CHECK" | "W32" | "W64"))
            && matches!(tokens.next(), None | Some("NEXT" | "SAME"))
            && tokens.next().is_none()
    })
}

fn first_mnemonic(s: &str) -> Option<&str> {
    let tok = s.split_whitespace().next()?;
    // Valid AMDGPU mnemonics start with an ASCII letter and contain only
    // `[a-zA-Z0-9_]`.
    if tok.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        && tok.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        Some(tok)
    } else {
        None
    }
}

fn parse_encoding_bytes(s: &str) -> Option<Vec<u32>> {
    let open = s.find('[')?;
    let close = s[open + 1..].find(']')?;
    let inside = &s[open + 1..open + 1 + close];
    let mut bytes = Vec::new();
    for part in inside.split(',') {
        let tok = part.trim();
        let hex = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X"))?;
        if hex.is_empty() || hex.len() > 2 {
            return None;
        }
        bytes.push(u8::from_str_radix(hex, 16).ok()?);
    }
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let words = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&c| u32::from_le_bytes(c))
        .collect();
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inline_and_bare_encoding_lines() {
        let sample = r#"
// RUN: llvm-mc -triple=amdgcn -mcpu=gfx1030 -show-encoding %s | FileCheck --check-prefix=GFX10 %s

global_load_dword_addtid v1, s[2:3] offset:16
// GFX10: encoding: [0x10,0x80,0x58,0xdc,0x00,0x00,0x02,0x01]

image_load v[0:3], v0, s[0:7] dmask:0xf dim:SQ_RSRC_IMG_1D unorm
; GFX10: image_load v[0:3], v0, s[0:7] dmask:0xf dim:SQ_RSRC_IMG_1D unorm ; encoding: [0x00,0x1f,0x00,0xf0,0x00,0x00,0x00,0x00]

v_mov_b32_dpp v5, v1 quad_perm:[3,2,1,0] row_mask:0x0 bank_mask:0x0
// GFX10: [0xfa,0x02,0x0a,0x7e,0x01,0x1b,0x00,0x00]

// Relocation fixups must be skipped.
s_branch foo
// GFX10: encoding: [A,A,0x82,0xbf]

// Non-GFX10 prefixes must be skipped.
v_add_f32 v0, v1, v2
// GFX9: encoding: [0x01,0x05,0x00,0x02]
"#;
        let mut entries = BTreeSet::new();
        parse_llvm_mc(sample, &mut entries);
        assert_eq!(entries.len(), 3);
        assert!(entries.contains(&Entry {
            mnemonic: "global_load_dword_addtid".into(),
            words: vec![0xdc58_8010, 0x0102_0000],
        }));
        assert!(entries.contains(&Entry {
            mnemonic: "image_load".into(),
            words: vec![0xf000_1f00, 0x0000_0000],
        }));
        assert!(entries.contains(&Entry {
            mnemonic: "v_mov_b32_dpp".into(),
            words: vec![0x7e0a_02fa, 0x0000_1b01],
        }));
    }

    #[test]
    fn matches_only_positive_gfx10_prefix_tokens() {
        for prefix in ["GFX10", "CHECK", "W32", "w64-next", "GFX10,W32-SAME"] {
            assert!(is_gfx10_prefix(prefix), "{prefix}");
        }
        for prefix in [
            "",
            "GFX100",
            "GFX10ERR",
            "GFX10-ERR",
            "NOT-GFX10",
            "GFX10-NOT",
            "FIXME-GFX10-V_FMA_F16",
            "GFX10,GFX11",
            "GFX10,GFX9",
            "W64-ERR",
            "RUN",
        ] {
            assert!(!is_gfx10_prefix(prefix), "{prefix}");
        }
    }

    #[test]
    fn first_mnemonic_accepts_an_opcode_token() {
        assert_eq!(
            first_mnemonic(" \tV_MOV_B32_e32 v0, v1"),
            Some("V_MOV_B32_e32")
        );
        assert_eq!(first_mnemonic("s_endpgm"), Some("s_endpgm"));
        for text in [
            "", " \t", "[0x12]", "{...}", "0xff", "label:", ".text", "v-mov",
        ] {
            assert_eq!(first_mnemonic(text), None, "{text}");
        }
        let mut entries = BTreeSet::new();
        parse_llvm_mc(
            "v_mov_b32 v0, v1\n// GFX10: v_mov_b32_e32 v0, v1 ; encoding: [0x01,0x03,0x00,0x7e]",
            &mut entries,
        );
        assert_eq!(entries.first().unwrap().mnemonic, "v_mov_b32_e32");
    }

    fn entry(mnemonic: &str, words: &[u32]) -> Entry {
        Entry {
            mnemonic: mnemonic.into(),
            words: words.to_vec(),
        }
    }

    #[test]
    fn reduction_keeps_deterministic_representatives() {
        let kept = [
            entry("v_add_f32", &[0x1]),
            entry("v_sub_f32", &[0x0400_0001]),
            entry("v_add_f32", &[0x1, 0]),
            entry("v_add_f32", &[0xf9, 0]),
            entry("v_add_f32", &[0xfa, 0]),
            entry("v_add_f32", &[0xe9, 0]),
            entry("v_add_f32", &[0xea, 0]),
            entry("v_add_f32", &[0xff, 0]),
            entry("v_add_f32", &[0xd400_0000, 0]),
        ];
        let redundant = [
            entry("v_add_f32", &[0x2]),
            entry("v_add_f32", &[0x1ff]),
            entry("v_add_f32", &[0x2ff, 0]),
        ];
        let input: Vec<_> = kept.iter().chain(&redundant).cloned().collect();
        let expected = kept.into_iter().collect();
        assert_eq!(reduce_entries(input.iter().cloned().collect()), expected);
        assert_eq!(reduce_entries(input.into_iter().rev().collect()), expected);
    }

    #[test]
    fn keeps_literal_positions_and_split_opcode_bits() {
        for (base, variant) in [
            ([0x8000_00ff, 0], [0x8000_ff00, 0]),
            ([0xd400_0000, 0xff], [0xd400_0000, 0xff << 9]),
            ([0xcc00_0000, 0xff], [0xcc00_0000, 0xff << 18]),
            ([0xf000_0000, 0], [0xf000_0006, 0]),
            ([0xf000_0000, 0], [0xf000_0001, 0]),
            ([0xe000_0000, 0], [0xe200_0000, 0]),
            ([0xe800_0000, 0], [0xe800_0000, 1 << 21]),
        ] {
            assert_ne!(decoder_selectors(&base), decoder_selectors(&variant));
        }
    }

    #[test]
    fn shards_repeat_the_header_without_splitting_entries() {
        let header = "# header\n";
        let entries = BTreeSet::from([entry("a", &[0]), entry("b", &[1]), entry("c", &[2, 3])]);
        let first = "# header\na 00000000\nb 00000001\n";
        let second = "# header\nc 00000002 00000003\n";
        assert_eq!(split_shards(header, &entries, first.len()), [first, second]);
        assert!(split_shards(header, &BTreeSet::new(), first.len()).is_empty());
    }
}
