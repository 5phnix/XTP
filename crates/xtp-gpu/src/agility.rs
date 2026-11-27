//! Optional D3D12 Agility SDK loader (`docs/analysis/gpu.md`, G22).
//!
//! When `D3D12\D3D12Core.dll` sits beside the executable, host devices and
//! adapter probes use `ID3D12SDKConfiguration1::CreateDeviceFactory` targeting
//! **exactly 1.614.0** (`SDK_VERSION = 614`). When that folder or DLL is
//! absent — or if the system runtime lacks `ID3D12SDKConfiguration1` or
//! rejects a mismatched DLL (`D3D12_ERROR_INVALID_REDIST`, `0x887E0003`) — XTP
//! logs the fallback and uses the Windows inbox D3D12 runtime instead.

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::{
    CLSID_D3D12SDKConfiguration, D3D12CreateDevice, D3D12GetDebugInterface, D3D12GetInterface,
    ID3D12Device, ID3D12DeviceFactory, ID3D12SDKConfiguration1,
};
use windows::core::{GUID, IUnknown, Interface, PCSTR, Param};

/// `Microsoft.Direct3D.D3D12` **exactly 1.614.0** (`D3D12SDKVersion`).
///
/// The Agility SDK loader requires an exact version match: placing another
/// `D3D12Core.dll` version (for example `1.620.x`) beside the executable fails
/// `CreateDeviceFactory` with `D3D12_ERROR_INVALID_REDIST` (`0x887E0003`).
pub(crate) const SDK_VERSION: u32 = 614;

const SDK_DIR_NAME: &str = "D3D12";
const CORE_DLL_NAME: &str = "D3D12Core.dll";
const D3D12_ERROR_INVALID_REDIST: i32 = 0x887E_0003_u32 as i32;

#[derive(Debug)]
enum Outcome {
    Loaded(ID3D12DeviceFactory),
    Missing(PathBuf),
    Failed { dir: PathBuf, reason: String },
}

fn sdk_dir_beside(exe: &Path) -> Option<PathBuf> {
    exe.parent().map(|dir| dir.join(SDK_DIR_NAME))
}

fn sdk_path_cstr(dir: &Path) -> Result<CString, String> {
    let mut text = dir
        .to_str()
        .ok_or_else(|| format!("Agility SDK path {} is not valid UTF-8", dir.display()))?
        .to_owned();
    if !text.ends_with(['\\', '/']) {
        text.push('\\');
    }
    CString::new(text)
        .map_err(|_| format!("Agility SDK path {} contains a NUL byte", dir.display()))
}

fn load_in(dir: &Path) -> Outcome {
    load_version_in(dir, SDK_VERSION)
}

fn load_version_in(dir: &Path, sdk_version: u32) -> Outcome {
    if !dir.join(CORE_DLL_NAME).is_file() {
        return Outcome::Missing(dir.to_path_buf());
    }
    let path = match sdk_path_cstr(dir) {
        Ok(path) => path,
        Err(reason) => {
            return Outcome::Failed {
                dir: dir.to_path_buf(),
                reason,
            };
        }
    };
    // SAFETY: `D3D12GetInterface` queries the process D3D12 configuration
    // singleton, and `CreateDeviceFactory` takes a valid NUL-terminated
    // directory string without mutating global D3D12 SDK selection.
    unsafe {
        let mut config: Option<ID3D12SDKConfiguration1> = None;
        let config = match D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut config)
            .and_then(|()| config.ok_or_else(windows::core::Error::empty))
        {
            Ok(config) => config,
            Err(e) => {
                return Outcome::Failed {
                    dir: dir.to_path_buf(),
                    reason: format!("ID3D12SDKConfiguration1 unavailable ({e})"),
                };
            }
        };
        match config
            .CreateDeviceFactory::<_, ID3D12DeviceFactory>(sdk_version, PCSTR(path.as_ptr().cast()))
        {
            Ok(factory) => Outcome::Loaded(factory),
            Err(e) if e.code().0 == D3D12_ERROR_INVALID_REDIST => Outcome::Failed {
                dir: dir.to_path_buf(),
                reason: format!(
                    "D3D12_ERROR_INVALID_REDIST ({e}): {CORE_DLL_NAME} must be \
                     Microsoft.Direct3D.D3D12 exactly 1.614.0 (SDK version {sdk_version})"
                ),
            },
            Err(e) => Outcome::Failed {
                dir: dir.to_path_buf(),
                reason: format!("CreateDeviceFactory failed ({e})"),
            },
        }
    }
}

/// The loaded Agility SDK device factory when `D3D12\D3D12Core.dll` (exactly
/// `1.614.0`) was loaded beside the executable, or `None` when falling back to
/// the Windows inbox D3D12 runtime.
fn factory() -> Option<&'static ID3D12DeviceFactory> {
    static FACTORY: OnceLock<Option<ID3D12DeviceFactory>> = OnceLock::new();
    FACTORY
        .get_or_init(|| {
            let exe = match std::env::current_exe() {
                Ok(exe) => exe,
                Err(e) => {
                    tracing::debug!(error = %e, "could not resolve executable path for Agility SDK");
                    return None;
                }
            };
            let dir = sdk_dir_beside(&exe)?;
            match load_in(&dir) {
                Outcome::Loaded(factory) => {
                    tracing::info!(
                        version = SDK_VERSION,
                        path = %dir.display(),
                        "loaded D3D12 Agility SDK (exactly 1.614.0)"
                    );
                    Some(factory)
                }
                Outcome::Missing(dir) => {
                    tracing::debug!(
                        path = %dir.display(),
                        "no D3D12\\D3D12Core.dll beside executable; using inbox D3D12 runtime"
                    );
                    None
                }
                Outcome::Failed { dir, reason } => {
                    tracing::warn!(
                        path = %dir.display(),
                        "{reason}; falling back to inbox D3D12 runtime"
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// Whether the D3D12 Agility SDK factory was loaded for this process.
pub(crate) fn is_loaded() -> bool {
    factory().is_some()
}

/// Creates or probes a feature-level 12.0 D3D12 device via `factory` when
/// present, or the inbox `D3D12CreateDevice` otherwise.
///
/// # Safety
/// `adapter` and `result` must satisfy `D3D12CreateDevice` /
/// `ID3D12DeviceFactory::CreateDevice` preconditions (`result` may be null to
/// probe support).
unsafe fn create_device_with<P>(
    factory: Option<&ID3D12DeviceFactory>,
    adapter: P,
    result: *mut Option<ID3D12Device>,
) -> windows::core::Result<()>
where
    P: Param<IUnknown>,
{
    if let Some(factory) = factory {
        // SAFETY: caller guarantees `adapter` and `result` are valid.
        unsafe { factory.CreateDevice(adapter, D3D_FEATURE_LEVEL_12_0, result) }
    } else {
        // SAFETY: caller guarantees `adapter` and `result` are valid.
        unsafe { D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_0, result) }
    }
}

/// Creates or probes a feature-level 12.0 D3D12 device using the loaded
/// Agility SDK factory when active, or the inbox D3D12 runtime otherwise.
///
/// # Safety
/// `adapter` and `result` must satisfy `D3D12CreateDevice` /
/// `ID3D12DeviceFactory::CreateDevice` preconditions (`result` may be null to
/// probe support).
pub(crate) unsafe fn create_device<P>(
    adapter: P,
    result: *mut Option<ID3D12Device>,
) -> windows::core::Result<()>
where
    P: Param<IUnknown>,
{
    // SAFETY: forwarded from caller preconditions.
    unsafe { create_device_with(factory(), adapter, result) }
}

/// Obtains a D3D12 configuration interface (`ID3D12Debug`, DRED settings,
/// etc.) from the loaded Agility SDK factory when active, or from the inbox
/// `D3D12GetDebugInterface` otherwise.
///
/// # Safety
/// `clsid` must correspond to `T` and remain valid for the duration of the
/// call.
pub(crate) unsafe fn configuration_interface<T: Interface>(
    clsid: &GUID,
) -> windows::core::Result<T> {
    if let Some(factory) = factory() {
        // SAFETY: caller ensures `clsid` matches `T`.
        return unsafe { factory.GetConfigurationInterface(clsid) };
    }
    let mut out: Option<T> = None;
    // SAFETY: `out` is a valid out-pointer for `D3D12GetDebugInterface`.
    unsafe { D3D12GetDebugInterface(&mut out)? };
    out.ok_or_else(windows::core::Error::empty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::*;

    #[test]
    fn sdk_dir_and_path_formatting() {
        let dir = sdk_dir_beside(Path::new(r"C:\xtp\target\release\xtp.exe"))
            .expect("executable path should have a parent directory");
        assert_eq!(dir, PathBuf::from(r"C:\xtp\target\release\D3D12"));

        assert_eq!(
            sdk_path_cstr(&dir).unwrap().to_bytes(),
            br"C:\xtp\target\release\D3D12\"
        );
        assert_eq!(
            sdk_path_cstr(Path::new(r"C:\xtp\D3D12\"))
                .unwrap()
                .to_bytes(),
            br"C:\xtp\D3D12\"
        );
    }

    #[test]
    fn missing_sdk_folder_falls_back_to_inbox_runtime() {
        let test = "agility::tests::missing_sdk_folder_falls_back_to_inbox_runtime";
        let missing = tempfile::tempdir().expect("temp dir");
        assert!(matches!(load_in(missing.path()), Outcome::Missing(_)));

        let Some(software) = crate::testgpu::software().unwrap_or(Ok(false)).ok() else {
            // `XTP_TEST_ADAPTER=none` asks for no device creation; the missing-
            // folder fallback classification above still ran without GPU access.
            if let Some(Err(reason)) = crate::testgpu::software() {
                crate::testgpu::skip(test, &reason);
            }
            return;
        };

        // SAFETY: plain DXGI and D3D12 device creation on the inbox fallback.
        let device: windows::core::Result<ID3D12Device> = unsafe {
            let adapter: Option<IUnknown> = if software {
                let factory: IDXGIFactory4 = match CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))
                {
                    Ok(f) => f,
                    Err(e) => {
                        crate::testgpu::skip(test, &format!("no DXGI factory ({e})"));
                        return;
                    }
                };
                match factory.EnumWarpAdapter::<IUnknown>() {
                    Ok(a) => Some(a),
                    Err(e) => {
                        crate::testgpu::skip(test, &format!("no WARP adapter ({e})"));
                        return;
                    }
                }
            } else {
                crate::dxgi::host_adapter()
            };
            let mut dev = None;
            create_device_with(None, adapter.as_ref(), &mut dev)
                .and_then(|()| dev.ok_or_else(windows::core::Error::empty))
        };
        match device {
            Ok(_) => {}
            Err(e) => crate::testgpu::skip(test, &format!("no inbox D3D12 device ({e})")),
        }
    }

    #[test]
    fn invalid_redist_dll_falls_back_cleanly() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(CORE_DLL_NAME), b"not a valid PE image")
            .expect("write placeholder DLL");
        // Asking for a version above any OS inbox runtime forces D3D12 to load
        // and reject the corrupt DLL on disk rather than keeping inbox D3D12.
        assert!(
            matches!(
                load_version_in(dir.path(), u32::MAX),
                Outcome::Failed { .. }
            ),
            "a corrupt or mismatched D3D12Core.dll should fail gracefully so XTP can fall back"
        );
        // At SDK_VERSION (614), an OS whose inbox runtime is >= 614 keeps its
        // inbox runtime (`Loaded`), while an older OS fails cleanly (`Failed`).
        assert!(!matches!(load_in(dir.path()), Outcome::Missing(_)));
    }
}
