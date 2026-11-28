//! Windows' timeout detection and recovery (TDR) settings for the host GPU,
//! read once when a hardware host device is created.
//!
//! Adapted from `codec-d/cool_fox` (`crates/victory-gpu/src/tdr.rs`,
//! `avatarmanz/VIctory-7900XTX`, `GPL-3.0-or-later`).
//!
//! With `TdrLevel` 0 a GPU that stops answering is never reset: a slow or
//! hung world frame freezes the whole desktop, and the runtime never sees a
//! device removal it could report. With 1 the timeout triggers a bugcheck.
//! Both are often set by hand to stop slow frames from resetting; raising
//! `TdrDelay` gives slow frames room without disabling recovery.

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};

/// Where Windows keeps the display driver TDR settings.
const KEY: &str = r"SYSTEM\CurrentControlSet\Control\GraphicsDrivers";

/// The TDR values in the registry; `None` when unset and Windows uses its
/// default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tdr {
    /// 0 off, 1 bugcheck, 2 recover to VGA, 3 recover (the default).
    pub(crate) level: Option<u32>,
    /// Seconds the GPU may take before counting as hung (default 2).
    pub(crate) delay: Option<u32>,
    /// Seconds a driver DDI call may take (default 5).
    pub(crate) ddi_delay: Option<u32>,
}

impl Tdr {
    pub(crate) fn read() -> Self {
        Tdr {
            level: dword("TdrLevel"),
            delay: dword("TdrDelay"),
            ddi_delay: dword("TdrDdiDelay"),
        }
    }

    /// Warning when the configured `TdrLevel` would freeze the desktop or
    /// bugcheck instead of delivering device removal to the runtime.
    pub(crate) fn warning(&self) -> Option<&'static str> {
        match self.level {
            Some(0) => Some(
                "TdrLevel is 0: Windows never resets a hung GPU, so a GPU hang freezes \
                 the whole PC instead of ending the run with a device removal; delete \
                 TdrLevel (or set it to 3) and raise TdrDelay to give slow frames room",
            ),
            Some(1) => Some(
                "TdrLevel is 1: a GPU timeout stops Windows with a blue screen instead \
                 of ending the run with a device removal; delete TdrLevel (or set it \
                 to 3) and raise TdrDelay to give slow frames room",
            ),
            _ => None,
        }
    }
}

/// A DWORD value under [`KEY`]; `None` when missing or not a DWORD.
fn dword(name: &str) -> Option<u32> {
    let key = wide(KEY);
    let name = wide(name);
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `key` and `name` are NUL-terminated UTF-16 strings that outlive
    // the call, and `value` has room for the `size` bytes a DWORD takes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut value).cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS).then_some(value)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn shown(value: Option<u32>) -> String {
    value.map_or_else(|| "default".to_owned(), |v| v.to_string())
}

/// Logs the host TDR settings and warns when a GPU hang would freeze the PC
/// or trigger a bugcheck instead of returning device removal.
pub(crate) fn log() {
    let tdr = Tdr::read();
    tracing::info!(
        level = %shown(tdr.level),
        delay = %shown(tdr.delay),
        ddi_delay = %shown(tdr.ddi_delay),
        "GPU timeout detection (TdrLevel, TdrDelay, TdrDdiDelay)"
    );
    if let Some(warning) = tdr.warning() {
        tracing::warn!("{warning}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(level: Option<u32>) -> Tdr {
        Tdr {
            level,
            ..Tdr::default()
        }
    }

    #[test]
    fn off_and_bugcheck_warn() {
        assert!(level(Some(0)).warning().unwrap().contains("freezes"));
        assert!(level(Some(1)).warning().unwrap().contains("blue screen"));
    }

    #[test]
    fn recovery_and_the_default_do_not_warn() {
        for l in [None, Some(2), Some(3)] {
            assert_eq!(level(l).warning(), None, "TdrLevel {l:?}");
        }
    }

    #[test]
    fn delay_alone_does_not_warn() {
        let tdr = Tdr {
            level: Some(3),
            delay: Some(60),
            ddi_delay: Some(60),
        };
        assert_eq!(tdr.warning(), None);
    }

    #[test]
    fn a_missing_value_reads_as_the_default() {
        assert_eq!(dword("XtpTestNoSuchValue"), None);
    }

    #[test]
    fn shown_names_the_default() {
        assert_eq!(shown(None), "default");
        assert_eq!(shown(Some(60)), "60");
    }
}
