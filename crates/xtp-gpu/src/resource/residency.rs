//! Host textures made resident without waiting for good.
//!
//! A world session stalled in `CreateCommittedResource`, its thread in
//! `D3DKMTWaitForSynchronizationObjectFromCpu` with the GPU idle: the
//! runtime waits, with no time limit, for the kernel to make the new
//! texture resident (2026-11-24, `docs/analysis/dark-world.md` §7). Host
//! textures are therefore made not resident
//! (`D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT`), then made resident here:
//! - first with `D3D12_RESIDENCY_FLAG_DENY_OVERBUDGET`, which fails at once
//!   past the process's video memory budget instead of waiting;
//! - past the budget (logged), again without it, so the system can page
//!   other memory out as a resident creation would;
//! - each time waiting for the residency fence at most [`TIMEOUT`]; a
//!   texture not resident by then is refused (logged), which the title
//!   sees as a texture without a host copy (null views), not as a hang.
//!
//! `XTP_TEXTURE_RESIDENCY=direct` makes host textures resident at creation,
//! as before.

use std::sync::Mutex;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

/// How long a texture may take to become resident.
pub(super) const TIMEOUT: Duration = Duration::from_secs(5);

/// Whether host textures are made resident at creation (the old path).
pub(super) fn direct() -> bool {
    static DIRECT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DIRECT.get_or_init(|| {
        let direct = asks_direct(std::env::var("XTP_TEXTURE_RESIDENCY").ok().as_deref());
        if direct {
            tracing::info!("host textures made resident at creation (XTP_TEXTURE_RESIDENCY)");
        }
        direct
    })
}

fn asks_direct(value: Option<&str>) -> bool {
    matches!(value, Some("direct" | "0"))
}

/// The heap flags a host texture is created with.
pub(super) fn heap_flags() -> D3D12_HEAP_FLAGS {
    if direct() {
        D3D12_HEAP_FLAG_NONE
    } else {
        D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT
    }
}

/// The fence residency signals, and the last value asked for.
struct Fence {
    fence: ID3D12Fence,
    next: u64,
}

// SAFETY: D3D12 fences are free-threaded; the value is under the lock.
unsafe impl Send for Fence {}

static FENCE: Mutex<Option<Fence>> = Mutex::new(None);

/// Why a texture is not resident.
#[derive(Debug)]
pub(super) enum Failed {
    /// D3D12 refused (the error), even past the budget check.
    Refused(windows::core::Error),
    /// Still not resident after [`TIMEOUT`].
    TimedOut,
}

/// Makes `texture`, created not resident on `device`, resident; returns
/// whether it went past the budget on the way.
pub(super) fn make_resident(
    device: &ID3D12Device,
    texture: &ID3D12Resource,
) -> Result<bool, Failed> {
    let device3: ID3D12Device3 = device.cast().map_err(Failed::Refused)?;
    let pageable: ID3D12Pageable = texture.cast().map_err(Failed::Refused)?;
    let objects = [Some(pageable)];
    let deny = enqueue(
        device,
        &device3,
        D3D12_RESIDENCY_FLAG_DENY_OVERBUDGET,
        &objects,
    );
    match deny {
        Ok(()) => Ok(false),
        Err(Failed::Refused(e)) if e.code() == windows::Win32::Foundation::E_OUTOFMEMORY => {
            enqueue(device, &device3, D3D12_RESIDENCY_FLAG_NONE, &objects).map(|()| true)
        }
        Err(other) => Err(other),
    }
}

/// One residency request, waited for at most [`TIMEOUT`].
fn enqueue(
    device: &ID3D12Device,
    device3: &ID3D12Device3,
    flags: D3D12_RESIDENCY_FLAGS,
    objects: &[Option<ID3D12Pageable>],
) -> Result<(), Failed> {
    let (fence, value) = {
        let mut guard = FENCE.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            // SAFETY: plain creation.
            let fence =
                unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.map_err(Failed::Refused)?;
            *guard = Some(Fence { fence, next: 0 });
        }
        let f = guard.as_mut().ok_or(Failed::TimedOut)?;
        f.next += 1;
        (f.fence.clone(), f.next)
    };
    // SAFETY: live objects; the fence is ours.
    unsafe { device3.EnqueueMakeResident(flags, objects, &fence, value) }
        .map_err(Failed::Refused)?;
    // SAFETY: a fresh event, closed below; the fence is live.
    unsafe {
        if fence.GetCompletedValue() >= value {
            return Ok(());
        }
        let event = CreateEventW(None, false, false, None).map_err(Failed::Refused)?;
        let waited = fence
            .SetEventOnCompletion(value, event)
            .map_err(Failed::Refused)
            .map(|()| WaitForSingleObject(event, TIMEOUT.as_millis() as u32));
        let _ = CloseHandle(event);
        match waited? {
            WAIT_OBJECT_0 => Ok(()),
            _ if fence.GetCompletedValue() >= value => Ok(()),
            _ => Err(Failed::TimedOut),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_residency_is_asked_by_name_or_zero() {
        assert!(asks_direct(Some("direct")));
        assert!(asks_direct(Some("0")));
        assert!(!asks_direct(Some("1")));
        assert!(!asks_direct(None));
    }

    /// A texture made not resident becomes resident and holds what is
    /// cleared into it (on the test device: WARP on the shared PC).
    #[test]
    fn a_texture_made_not_resident_becomes_usable() {
        let test = "resource::residency::tests::a_texture_made_not_resident_becomes_usable";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT,
                    &desc,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    None,
                    &mut texture,
                )
                .unwrap();
        }
        let texture = texture.unwrap();
        assert!(
            !make_resident(&host.device, &texture).unwrap(),
            "within budget"
        );
        // Usable: cleared, then read back.
        let list = host.new_list().unwrap();
        let rtv = host.rtv(&texture, None);
        // SAFETY: an open list and a fresh view of a resident texture.
        unsafe {
            list.list
                .ClearRenderTargetView(rtv, &[1.0, 0.0, 1.0, 1.0], None);
        }
        list.close();
        host.execute(&[&list]);
        let texels = super::super::tests_support::read_back(host, &texture, 0x4);
        assert_eq!(texels[..4], [255, 0, 255, 255]);
        assert_eq!(host.log_messages(), None);
    }
}
