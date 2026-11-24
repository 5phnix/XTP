//! Fingerprints this crate's sources into `XTP_SHADER_SOURCES`
//! (`xtp_shader::SOURCES`, src/fingerprint.rs): the shader cache keys
//! translations by the translator that made them, not by the whole
//! executable.

#[path = "src/fingerprint.rs"]
mod fingerprint;

fn main() {
    let root = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    // What else decides the code: the features, the target, and whether
    // debug assertions and overflow checks are on.
    let mut settings: Vec<String> = std::env::vars()
        .filter(|(name, _)| name.starts_with("CARGO_FEATURE_"))
        .map(|(name, _)| name)
        .collect();
    for name in [
        "TARGET",
        "CARGO_CFG_DEBUG_ASSERTIONS",
        "CARGO_CFG_OVERFLOW_CHECKS",
    ] {
        if let Some(value) = std::env::var_os(name) {
            settings.push(format!("{name}={}", value.to_string_lossy()));
        }
    }
    settings.sort();
    let print = fingerprint::of(&root, &settings).expect("reading the crate's sources");
    println!("cargo:rustc-env=XTP_SHADER_SOURCES={print}");
    // A directory: cargo looks at every file in it.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
}
