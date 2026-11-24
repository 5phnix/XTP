#![cfg(windows)]

#[test]
fn warns_before_falling_back_from_unusable_manifest_entries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Fallback.exe"), b"synthetic non-PE").unwrap();
    std::fs::write(
        dir.path().join("MicrosoftGame.config"),
        r#"<Game><ExecutableList><Executable Name="missing.exe"/>
            <Executable Name="notes.txt"/></ExecutableList></Game>"#,
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_xtp"))
        .current_dir(dir.path())
        .args(["run", "."])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning:") && stderr.contains("MicrosoftGame.config"),
        "{stderr}"
    );
    assert!(
        stderr.contains("missing.exe") && stderr.contains("notes.txt"),
        "{stderr}"
    );
}
