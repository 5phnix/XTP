//! Host CPU preflight check before starting a title.
//!
//! Xbox Series consoles use an AMD Zen 2 CPU, so titles are compiled assuming
//! its baseline instruction-set extensions are always present. Checking them
//! before execution gives a clear error instead of an illegal-instruction crash
//! deep inside title code.
//!
//! Console-only or AMD-specific extensions that `xtp-loader` emulates in its
//! exception handler (`CLZERO`, `MONITORX`/`MWAITX`, `RDPRU`, `RDPID`, `CLWB`,
//! `SSE4a`) are intentionally omitted here.

use anyhow::Result;

/// Environment variable that bypasses the host CPU feature check when set to `"1"`.
pub(crate) const SKIP_ENV: &str = "XTP_SKIP_CPU_CHECK";

/// Console CPU features required by titles and not emulated by `xtp-loader`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CpuFeature {
    Sse41,
    Sse42,
    Popcnt,
    Avx,
    Avx2,
    Fma,
    F16c,
    Bmi1,
    Bmi2,
    Lzcnt,
    Movbe,
}

impl CpuFeature {
    /// Every required feature in display order.
    pub(crate) const REQUIRED: &'static [Self] = &[
        Self::Sse41,
        Self::Sse42,
        Self::Popcnt,
        Self::Avx,
        Self::Avx2,
        Self::Fma,
        Self::F16c,
        Self::Bmi1,
        Self::Bmi2,
        Self::Lzcnt,
        Self::Movbe,
    ];

    /// Human-readable ISA name shown in diagnostics.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Sse41 => "SSE4.1",
            Self::Sse42 => "SSE4.2",
            Self::Popcnt => "POPCNT",
            Self::Avx => "AVX",
            Self::Avx2 => "AVX2",
            Self::Fma => "FMA",
            Self::F16c => "F16C",
            Self::Bmi1 => "BMI1",
            Self::Bmi2 => "BMI2",
            Self::Lzcnt => "LZCNT",
            Self::Movbe => "MOVBE",
        }
    }
}

/// Verify that the host CPU supports every required console feature, unless
/// `XTP_SKIP_CPU_CHECK=1` is set.
#[cfg(windows)]
pub(crate) fn check_cpu() -> Result<()> {
    let skip = std::env::var(SKIP_ENV).is_ok_and(|v| v == "1");
    check_cpu_with(skip, host_has_feature)
}

/// Core check with injectable skip flag and feature probe, for testing.
pub(crate) fn check_cpu_with(skip: bool, probe: impl FnMut(CpuFeature) -> bool) -> Result<()> {
    if skip {
        eprintln!("warning: skipping CPU feature check ({SKIP_ENV}=1)");
        return Ok(());
    }
    let missing = missing_features(probe);
    if let Some(message) = format_missing(&missing) {
        anyhow::bail!("{message}");
    }
    Ok(())
}

/// Return the required console CPU features that `probe` reports as absent.
pub(crate) fn missing_features(mut probe: impl FnMut(CpuFeature) -> bool) -> Vec<CpuFeature> {
    CpuFeature::REQUIRED
        .iter()
        .copied()
        .filter(|&feature| !probe(feature))
        .collect()
}

/// Format a user-facing error listing `missing` features, or `None` if empty.
pub(crate) fn format_missing(missing: &[CpuFeature]) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    let names: Vec<&str> = missing.iter().map(|f| f.name()).collect();
    Some(format!(
        "this CPU lacks console features the title requires: {}\n\
         (features emulated by XTP are not listed; set {SKIP_ENV}=1 to run anyway)",
        names.join(", ")
    ))
}

#[cfg(all(windows, any(target_arch = "x86", target_arch = "x86_64")))]
fn host_has_feature(feature: CpuFeature) -> bool {
    match feature {
        CpuFeature::Sse41 => std::arch::is_x86_feature_detected!("sse4.1"),
        CpuFeature::Sse42 => std::arch::is_x86_feature_detected!("sse4.2"),
        CpuFeature::Popcnt => std::arch::is_x86_feature_detected!("popcnt"),
        CpuFeature::Avx => std::arch::is_x86_feature_detected!("avx"),
        CpuFeature::Avx2 => std::arch::is_x86_feature_detected!("avx2"),
        CpuFeature::Fma => std::arch::is_x86_feature_detected!("fma"),
        CpuFeature::F16c => std::arch::is_x86_feature_detected!("f16c"),
        CpuFeature::Bmi1 => std::arch::is_x86_feature_detected!("bmi1"),
        CpuFeature::Bmi2 => std::arch::is_x86_feature_detected!("bmi2"),
        CpuFeature::Lzcnt => std::arch::is_x86_feature_detected!("lzcnt"),
        CpuFeature::Movbe => std::arch::is_x86_feature_detected!("movbe"),
    }
}

#[cfg(all(windows, not(any(target_arch = "x86", target_arch = "x86_64"))))]
fn host_has_feature(_: CpuFeature) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_features_present_produces_no_message() {
        let missing = missing_features(|_| true);
        assert!(missing.is_empty());
        assert_eq!(format_missing(&missing), None);
        assert!(check_cpu_with(false, |_| true).is_ok());
    }

    #[test]
    fn missing_avx2_is_named_in_error() {
        let missing = missing_features(|f| f != CpuFeature::Avx2);
        assert_eq!(missing, [CpuFeature::Avx2]);

        let message = format_missing(&missing).expect("missing AVX2 should produce a message");
        assert!(message.contains("AVX2"), "message: {message}");
        assert!(message.contains(SKIP_ENV), "message: {message}");

        let err = check_cpu_with(false, |f| f != CpuFeature::Avx2).unwrap_err();
        assert!(err.to_string().contains("AVX2"), "err: {err}");
    }

    #[test]
    fn multiple_missing_features_are_all_listed() {
        let missing = missing_features(|f| {
            !matches!(f, CpuFeature::Avx2 | CpuFeature::Fma | CpuFeature::Bmi2)
        });
        assert_eq!(
            missing,
            [CpuFeature::Avx2, CpuFeature::Fma, CpuFeature::Bmi2]
        );

        let message = format_missing(&missing).expect("missing features should produce a message");
        assert!(message.contains("AVX2, FMA, BMI2"), "message: {message}");
    }

    #[test]
    fn skip_flag_bypasses_missing_features() {
        assert!(check_cpu_with(true, |_| false).is_ok());
    }

    #[test]
    fn emulated_features_are_not_required() {
        let names: Vec<&str> = CpuFeature::REQUIRED.iter().map(|f| f.name()).collect();
        for emulated in [
            "CLZERO", "MONITORX", "MWAITX", "RDPRU", "RDPID", "CLWB", "SSE4a",
        ] {
            assert!(
                !names.contains(&emulated),
                "emulated feature {emulated} should not be required"
            );
        }
    }
}
