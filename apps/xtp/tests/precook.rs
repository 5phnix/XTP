//! `xtp precook` end to end on a synthetic pack (no game files): a console
//! compute program, which translates and gets a host pipeline, and a
//! container without a program, which the translator refuses. On the
//! software adapter (WARP) unless `XTP_TEST_ADAPTER=hardware`: the PC's GPU
//! is for game sessions.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A console compute program in 32-lane waves that only
/// ends (`s_endpgm`), as the console compiler stores it: FSSM record, XBCH
/// zlib blob, XDAT index in the `DXIL` part (`xtp_shader::native`).
fn compute_container(threads: u32) -> Vec<u8> {
    const RECORD: usize = 0x54;
    const STAGE_DATA: usize = 0x40;
    let code = 0xBF81_0000u32.to_le_bytes(); // s_endpgm
    // The payload: the slot count, the stages' record offsets (only the
    // compute one, at 0x60, present), then the record, its stage data and
    // its code.
    let mut payload = 6u64.to_le_bytes().to_vec();
    payload.resize(0x68, 0);
    for slot in [0x08, 0x10, 0x18, 0x20, 0x28, 0x30, 0x48, 0x60] {
        payload[slot..slot + 8].copy_from_slice(&0xDEAD_BEEF_DEAD_BEEFu64.to_le_bytes());
    }
    let record = payload.len();
    payload[0x60..0x68].copy_from_slice(&(record as u64).to_le_bytes());
    let mut r = vec![0u8; RECORD + STAGE_DATA];
    let code_at = (record + RECORD + STAGE_DATA) as u64;
    r[0..8].copy_from_slice(&code_at.to_le_bytes());
    r[8..16].copy_from_slice(&(code.len() as u64).to_le_bytes());
    r[0x14..0x18].copy_from_slice(&(1u32 << 5).to_le_bytes()); // wave32
    r[0x50..0x54].copy_from_slice(&(4u32 | 8 << 20).to_le_bytes()); // VGPRs, SGPRs
    let stage = RECORD;
    r[stage + 0x28..stage + 0x34].copy_from_slice(&[threads, 1, 1].map(u32::to_le_bytes).concat());
    payload.extend_from_slice(&r);
    payload.extend_from_slice(&code);

    let mut fssm = b"FSSM".to_vec();
    fssm.extend_from_slice(&0x101u32.to_le_bytes());
    fssm.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    fssm.extend_from_slice(&payload);
    let stream = miniz_oxide::deflate::compress_to_vec_zlib(&fssm, 6);
    let mut xbch = b"XBCH".to_vec();
    xbch.extend_from_slice(&3u32.to_le_bytes());
    xbch.extend_from_slice(&(stream.len() as u64).to_le_bytes());
    xbch.extend_from_slice(&(fssm.len() as u64).to_le_bytes());
    xbch.extend_from_slice(&stream);

    // The DXIL part: the program header (compute, shader model 6.6), the
    // XDAT index of one native blob, the blob.
    let mut dxil = vec![0u8; 0x18];
    dxil[0..4].copy_from_slice(&(5u32 << 16 | 6 << 4 | 6).to_le_bytes());
    dxil.extend_from_slice(b"XDAT");
    dxil.extend_from_slice(&1u32.to_le_bytes());
    dxil.extend_from_slice(&0x24u32.to_le_bytes());
    let blob_at = dxil.len() + 12;
    for v in [0x0d, blob_at as u32, xbch.len() as u32] {
        dxil.extend_from_slice(&v.to_le_bytes());
    }
    dxil.extend_from_slice(&xbch);
    container(&[(b"DXIL", &dxil)])
}

/// A DXBC container holding `parts`.
fn container(parts: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let header = 0x20 + 4 * parts.len();
    let mut body = Vec::new();
    let mut offsets = Vec::new();
    for (fourcc, data) in parts {
        offsets.push((header + body.len()) as u32);
        body.extend_from_slice(*fourcc);
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(data);
    }
    let mut out = b"DXBC".to_vec();
    out.extend_from_slice(&[0; 16]);
    out.extend_from_slice(&[1, 0, 0, 0]);
    out.extend_from_slice(&((header + body.len()) as u32).to_le_bytes());
    out.extend_from_slice(&(parts.len() as u32).to_le_bytes());
    for o in offsets {
        out.extend_from_slice(&o.to_le_bytes());
    }
    out.extend_from_slice(&body);
    out
}

/// An SGD5 record holding `payload` as stored deflate blocks, split after
/// its second byte so that a container's magic does not appear as is.
fn record(name: &str, payload: &[u8]) -> Vec<u8> {
    let mut packed = Vec::new();
    let mut parts = vec![(0u8, &payload[..2])];
    let rest: Vec<&[u8]> = payload[2..].chunks(0xffff).collect();
    for (i, part) in rest.iter().enumerate() {
        parts.push((u8::from(i + 1 == rest.len()), part));
    }
    for (last, part) in parts {
        let len = u16::try_from(part.len()).unwrap();
        packed.push(last);
        packed.extend_from_slice(&len.to_le_bytes());
        packed.extend_from_slice(&(!len).to_le_bytes());
        packed.extend_from_slice(part);
    }
    let n = (packed.len() + 12) as u32;
    let mut out = vec![0x11, 0x22, name.len() as u8 + 1];
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&[0, 0x3c]);
    for v in [n, 0x000D_DCE7, n, payload.len() as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&packed);
    out
}

/// The figure after `label` at the start of a line of `out`.
fn figure(out: &str, label: &str) -> Option<u64> {
    let line = out.lines().find(|l| l.starts_with(label))?;
    line[label.len()..].split_whitespace().next()?.parse().ok()
}

struct Run {
    out: String,
    err: String,
}

/// Runs `exe precook <pack> --no-wait <args>` with a cache of its own in
/// `dir`, on the test adapter; `None` (the test skipped) without a device.
fn precook(exe: &Path, dir: &Path, pack: &Path, args: &[&str]) -> Option<Run> {
    precook_with_settings(exe, dir, pack, args, &[])
}

fn precook_with_settings(
    exe: &Path,
    dir: &Path,
    pack: &Path,
    args: &[&str],
    settings: &[(&str, &str)],
) -> Option<Run> {
    let adapter = match std::env::var("XTP_TEST_ADAPTER").as_deref() {
        Ok("none") => {
            xtp_testkit::skip(xtp_testkit::reason::NO_GPU);
            return None;
        }
        // The first hardware adapter, as in a run.
        Ok("hardware") => "",
        _ => "Microsoft Basic Render Driver",
    };
    let output = Command::new(exe)
        .arg("precook")
        .arg(pack)
        .arg("--no-wait")
        .args(args)
        .current_dir(dir)
        .env("XTP_CACHE_DIR", dir.join("cache"))
        .env("XTP_DATA_DIR", dir.join("data"))
        .env("XTP_ADAPTER", adapter)
        .env_remove("XTP_CONFIG")
        .env_remove("XTP_SESSION_LOCK")
        .env_remove("XTP_HOST_LANES")
        .env_remove("XTP_LOG")
        .env_remove("RUST_LOG")
        .envs(settings.iter().copied())
        .output()
        .expect("running xtp precook");
    let run = Run {
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    if !output.status.success() && run.err.contains("no host D3D12 device") {
        xtp_testkit::skip(xtp_testkit::reason::NO_GPU);
        return None;
    }
    assert!(
        output.status.success(),
        "xtp precook failed\n{}\n{}",
        run.out,
        run.err
    );
    Some(run)
}

/// The first pass translates the compute program, keeps the refusal and
/// makes the host pipeline; the next one finds the cache up to date; a
/// forced one finds everything in the cache and the library; and another
/// executable whose bytes differ outside the translator (a trailing byte:
/// what any change to the HLE or a log line does to xtp.exe) finds the
/// same key and the same entries.
#[test]
fn precook_fills_the_cache_once_for_every_build_of_the_translator() {
    let dir = tempfile::tempdir().unwrap();
    let compute = compute_container(64);
    let refused = container(&[]);
    let mut pack = b"SGD5".to_vec();
    pack.extend(record("CS_Synthetic_0011aabb_Wrapped", &compute));
    pack.extend(record("PS_NoProgram_2233ccdd_Wrapped", &refused));
    pack.extend(record("CS_Synthetic_again_Wrapped", &compute));
    let pack_path = dir.path().join("shaders.rpf");
    std::fs::write(&pack_path, &pack).unwrap();
    let xtp = PathBuf::from(env!("CARGO_BIN_EXE_xtp"));
    let list = dir.path().join("list.txt");
    // The title pipelines earlier sessions start early: the precook saves
    // the library beside them, and must keep them.
    let pipelines = dir.path().join("cache/pipelines");
    std::fs::create_dir_all(&pipelines).unwrap();
    let early = "a title pipeline key\nanother one";
    std::fs::write(pipelines.join("early-pipelines.txt"), early).unwrap();

    let Some(first) = precook(
        &xtp,
        dir.path(),
        &pack_path,
        &["--list", list.to_str().unwrap()],
    ) else {
        return;
    };
    let out = &first.out;
    for phase in ["translation  ", "compute PSOs ", "library save "] {
        let seconds = out
            .lines()
            .find_map(|line| line.strip_prefix(phase))
            .and_then(|time| time.split_once(" s"))
            .and_then(|(seconds, _)| seconds.parse::<f64>().ok());
        assert!(seconds.is_some_and(|s| s >= 0.0), "{phase}\n{out}");
    }
    assert!(
        out.contains("containers   2 in 3 records (1 repeated)"),
        "{out}\n{}",
        first.err
    );
    assert_eq!(figure(out, "translated"), Some(1), "{out}");
    assert_eq!(figure(out, "from cache"), Some(0), "{out}");
    assert_eq!(figure(out, "refused"), Some(1), "{out}");
    assert!(
        out.contains(
            "compute      1 programs: 1 host pipelines made, 0 from the library, 0 without one"
        ),
        "{out}\n{}",
        first.err
    );
    let listed = std::fs::read_to_string(&list).unwrap();
    let hash = &blake3::hash(&compute).to_hex()[..16];
    assert!(
        listed
            .lines()
            .any(|l| l.contains("CS_Synthetic_0011aabb_Wrapped")
                && l.contains(hash)
                && l.ends_with("Cs ready")),
        "{listed}"
    );
    assert!(
        listed.contains("refused: malformed shader: no DXIL part"),
        "{listed}"
    );
    assert!(pipelines.join("host-pipelines.bin").is_file(), "{out}");
    let kept = std::fs::read_to_string(pipelines.join("early-pipelines.txt")).unwrap();
    let mut kept: Vec<&str> = kept.lines().collect();
    kept.sort();
    assert_eq!(
        kept,
        ["a title pipeline key", "another one"],
        "the early list is kept"
    );

    let second = precook(&xtp, dir.path(), &pack_path, &[]).unwrap();
    assert!(second.out.contains("up to date"), "{}", second.out);
    assert_eq!(figure(&second.out, "translated"), None, "no pass");

    let forced = precook(&xtp, dir.path(), &pack_path, &["--force"]).unwrap();
    let out = &forced.out;
    assert_eq!(figure(out, "translated"), Some(0), "{out}");
    assert_eq!(figure(out, "from cache"), Some(1), "{out}");
    assert_eq!(figure(out, "refused"), Some(1), "the refusal, kept: {out}");
    assert!(
        out.contains("1 programs: 0 host pipelines made, 1 from the library"),
        "{out}"
    );

    // Another build of the same translator.
    let other = dir.path().join("xtp-other.exe");
    let mut bytes = std::fs::read(&xtp).unwrap();
    bytes.push(0);
    std::fs::write(&other, bytes).unwrap();
    let same = precook(&other, dir.path(), &pack_path, &[]).unwrap();
    assert!(
        same.out.contains("up to date"),
        "the same key: {}",
        same.out
    );
    let shared = precook(&other, dir.path(), &pack_path, &["--force"]).unwrap();
    assert_eq!(figure(&shared.out, "translated"), Some(0), "{}", shared.out);
    assert_eq!(figure(&shared.out, "from cache"), Some(1), "{}", shared.out);
}

/// A new translation key with one changed DXIL. A loop-limit change gives
/// the two loop-free programs a new key while preserving their DXIL. Changing
/// the group size in one record then changes just that program's DXIL.
#[test]
fn precook_keeps_pipelines_across_translation_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shaders.rpf");
    let xtp = Path::new(env!("CARGO_BIN_EXE_xtp"));
    let write_pack = |threads| {
        let mut pack = b"SGD5".to_vec();
        pack.extend(record("CS_First_Wrapped", &compute_container(32)));
        pack.extend(record("CS_Second_Wrapped", &compute_container(threads)));
        std::fs::write(&path, pack).unwrap();
    };
    let run = |limit| {
        precook_with_settings(
            xtp,
            dir.path(),
            &path,
            &[],
            &[("XTP_SHADER_LOOP_LIMIT", limit)],
        )
    };
    write_pack(64);
    let Some(first) = run("128") else { return };
    assert_eq!(figure(&first.out, "translated"), Some(2), "{}", first.out);
    let library = dir.path().join("cache/pipelines/host-pipelines.bin");
    assert!(library.is_file(), "{}", first.out);

    write_pack(128);
    let changed = run("256").unwrap().out;
    assert_eq!(figure(&changed, "translated"), Some(2), "{changed}");
    assert!(!changed.contains("a new generation"), "{changed}");
    assert!(
        changed.contains("1 host pipelines made, 1 from the library"),
        "{changed}"
    );

    // The replaced pipeline is still available to a session using the old build.
    write_pack(64);
    let original = run("128").unwrap().out;
    assert_eq!(figure(&original, "from cache"), Some(2), "{original}");
    assert!(
        original.contains("0 host pipelines made, 2 from the library"),
        "{original}"
    );

    // A driver-rejected file is replaced with a usable library.
    std::fs::write(library, b"invalid pipeline library").unwrap();
    let repaired = run("128").unwrap().out;
    assert!(
        repaired.contains("2 host pipelines made, 0 from the library"),
        "{repaired}"
    );
    let forced = precook_with_settings(
        xtp,
        dir.path(),
        &path,
        &["--force"],
        &[("XTP_SHADER_LOOP_LIMIT", "128")],
    )
    .unwrap()
    .out;
    assert!(
        forced.contains("0 host pipelines made, 2 from the library"),
        "{forced}"
    );
}
