//! Host memory made resident without a title thread waiting on VidMm.
//!
//! World sessions stalled in resource creation, the thread in
//! `D3DKMTWaitForSynchronizationObjectFromCpu` with the GPU idle: past about
//! 9.5 GB resident on the PC's RTX 4080, VidMm admits about one new resident
//! allocation a second, whatever its size (2026-11-24, task 6bb65399,
//! `docs/analysis/vram.md`). Host textures and the texture pool's heaps are
//! therefore created not resident (`D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT`)
//! and asked to become resident here:
//! - with `D3D12_RESIDENCY_FLAG_DENY_OVERBUDGET`: never past the process's
//!   video memory budget; a refusal is logged and asked again from
//!   [`RETRY`] later, never given up;
//! - each request with a fence of its own: requests may finish in any
//!   order, so one fence for all could say a texture is resident when only
//!   a later request is (a GPU page fault);
//! - not waited for: [`Residency::is_ready`] says whether it is there yet.
//!   What is not resident yet is not handed to the GPU: `host_resource`
//!   answers `None` for it (null views, no draws through it) until then.
//!
//! `XTP_TEXTURE_RESIDENCY=direct` makes committed host textures resident at
//! creation, as before 2026-11-24.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, E_OUTOFMEMORY};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

/// How long after a refusal residency is asked again.
pub(super) const RETRY: Duration = Duration::from_secs(1);

/// Whether committed host textures are made resident at creation (the old
/// path).
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

/// The heap flags a committed host texture is created with.
pub(super) fn heap_flags() -> D3D12_HEAP_FLAGS {
    if direct() {
        D3D12_HEAP_FLAG_NONE
    } else {
        D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT
    }
}

/// Residency requests made and refused since the start (frame log, tests).
static ASKED: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// Requests made and refused since the start.
pub(super) fn counts() -> (u64, u64) {
    (
        ASKED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
    )
}

/// Whether some host memory (a texture, a pool heap) is resident, and if
/// not, how it gets there.
pub(crate) struct Residency {
    ready: AtomicBool,
    state: Mutex<State>,
}

enum State {
    Ready,
    /// Asked: its own fence reaches 1 once resident.
    Asked {
        fence: ID3D12Fence,
    },
    /// Refused (past the budget, or an error, logged then): asked again
    /// from `since` + [`RETRY`].
    Refused {
        device: ID3D12Device,
        objects: Vec<Option<ID3D12Pageable>>,
        since: Instant,
    },
}

// SAFETY: D3D12 devices, fences and pageables are free-threaded; the state
// is behind its lock.
unsafe impl Send for State {}

impl std::fmt::Debug for Residency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Residency")
            .field("ready", &self.ready.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Residency {
    /// Memory resident from the start.
    pub(crate) fn resident() -> Residency {
        Residency {
            ready: AtomicBool::new(true),
            state: Mutex::new(State::Ready),
        }
    }

    /// Asks `device` to make `objects` (created not resident) resident,
    /// within the budget; not waited for.
    pub(crate) fn ask(device: &ID3D12Device, objects: Vec<Option<ID3D12Pageable>>) -> Residency {
        let state = match enqueue(device, &objects) {
            Ok(fence) => State::Asked { fence },
            Err(why) => refused(device, objects, why),
        };
        Residency {
            ready: AtomicBool::new(false),
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the memory is resident now. A request refused [`RETRY`] ago
    /// or more is made again (not waited for).
    pub(crate) fn is_ready(&self) -> bool {
        if self.ready.load(Ordering::Acquire) {
            return true;
        }
        let mut state = self.lock();
        match &*state {
            State::Ready => true,
            // SAFETY: a plain query of a live fence.
            State::Asked { fence } if unsafe { fence.GetCompletedValue() } >= 1 => {
                *state = State::Ready;
                self.ready.store(true, Ordering::Release);
                true
            }
            State::Asked { .. } => false,
            State::Refused { since, .. } if since.elapsed() < RETRY => false,
            State::Refused {
                device, objects, ..
            } => {
                let (device, objects) = (device.clone(), objects.clone());
                *state = match enqueue(&device, &objects) {
                    Ok(fence) => State::Asked { fence },
                    Err(why) => refused(&device, objects, why),
                };
                false
            }
        }
    }

    /// Whether the last request was refused (it is asked again later).
    pub(crate) fn refused(&self) -> bool {
        matches!(*self.lock(), State::Refused { .. })
    }

    /// Waits at most `timeout` for the memory to be resident; whether it is.
    /// Nothing waits for a refused request.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        if self.is_ready() {
            return true;
        }
        let fence = match &*self.lock() {
            State::Asked { fence } => fence.clone(),
            _ => return false,
        };
        // SAFETY: a fresh event, closed below; the fence is live.
        unsafe {
            if let Ok(event) = CreateEventW(None, false, false, None) {
                if fence.SetEventOnCompletion(1, event).is_ok() {
                    WaitForSingleObject(event, timeout.as_millis().min(u32::MAX as u128) as u32);
                }
                let _ = CloseHandle(event);
            }
        }
        self.is_ready()
    }

    /// A request waiting on `fence` (tests, which signal it themselves).
    #[cfg(test)]
    pub(crate) fn asked(fence: ID3D12Fence) -> Residency {
        Residency {
            ready: AtomicBool::new(false),
            state: Mutex::new(State::Asked { fence }),
        }
    }
}

/// The refused state for `objects`, logged (the first refusals, then every
/// power of two) with the video memory budget.
fn refused(device: &ID3D12Device, objects: Vec<Option<ID3D12Pageable>>, why: String) -> State {
    let n = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 8 || n.is_power_of_two() {
        let (budget, usage) = crate::dxgi::video_memory(device).unwrap_or_default();
        tracing::warn!(
            n,
            why,
            budget_mib = budget >> 20,
            usage_mib = usage >> 20,
            "host memory not made resident: asked again in a second, unused meanwhile"
        );
    }
    State::Refused {
        device: device.clone(),
        objects,
        since: Instant::now(),
    }
}

/// One request, with a fence of its own, signalled to 1 once `objects`
/// are resident.
fn enqueue(
    device: &ID3D12Device,
    objects: &[Option<ID3D12Pageable>],
) -> Result<ID3D12Fence, String> {
    ASKED.fetch_add(1, Ordering::Relaxed);
    let device3: ID3D12Device3 = device
        .cast()
        .map_err(|e| format!("no ID3D12Device3 ({e})"))?;
    // SAFETY: plain creation.
    let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .map_err(|e| format!("no fence ({e})"))?;
    // SAFETY: live objects; the fence is ours.
    unsafe {
        device3.EnqueueMakeResident(D3D12_RESIDENCY_FLAG_DENY_OVERBUDGET, objects, &fence, 1)
    }
    .map_err(|e| {
        if e.code() == E_OUTOFMEMORY {
            "past the video memory budget".to_owned()
        } else {
            format!("refused ({})", e.code())
        }
    })?;
    Ok(fence)
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

    /// A 4x4 render target created not resident.
    fn not_resident(device: &ID3D12Device) -> ID3D12Resource {
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
            device
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
        texture.unwrap()
    }

    fn pageable(texture: &ID3D12Resource) -> Vec<Option<ID3D12Pageable>> {
        vec![Some(texture.cast().unwrap())]
    }

    /// A texture made not resident becomes resident and holds what is
    /// cleared into it (on the test device: WARP on the shared PC).
    #[test]
    fn a_texture_made_not_resident_becomes_usable() {
        let test = "resource::residency::tests::a_texture_made_not_resident_becomes_usable";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let texture = not_resident(&host.device);
        let residency = Residency::ask(&host.device, pageable(&texture));
        assert!(residency.wait(Duration::from_secs(5)), "within budget");
        assert!(!residency.refused());
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
    }

    /// Requests finish in any order, each on its own fence: one whose
    /// fence is not signalled stays not ready while later ones are, from
    /// threads at once.
    #[test]
    fn each_request_waits_on_its_own_fence() {
        let test = "resource::residency::tests::each_request_waits_on_its_own_fence";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let fences: Vec<ID3D12Fence> = (0..8)
            // SAFETY: plain creation.
            .map(|_| unsafe { host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.unwrap())
            .collect();
        let requests: Vec<Residency> = fences.iter().map(|f| Residency::asked(f.clone())).collect();
        /// A fence for another thread to signal.
        struct Signal(ID3D12Fence);
        // SAFETY: D3D12 fences are free-threaded.
        unsafe impl Send for Signal {}
        // Signalled last to first, from as many threads.
        std::thread::scope(|s| {
            for (i, fence) in fences.iter().enumerate().rev() {
                let requests = &requests;
                let fence = Signal(fence.clone());
                s.spawn(move || {
                    let fence = fence;
                    // SAFETY: a CPU signal of our own fence.
                    unsafe { fence.0.Signal(1) }.unwrap();
                    assert!(requests[i].is_ready());
                });
                // Every request before `i` is still waiting.
                for (j, r) in requests.iter().enumerate().take(i) {
                    assert!(!r.is_ready(), "request {j} ready before its fence");
                }
            }
        });
        assert!(requests.iter().all(Residency::is_ready));
    }

    /// Waiting gives up after its time, the request stays (not refused),
    /// and is ready once its fence is reached.
    #[test]
    fn a_wait_that_expires_leaves_the_request_pending() {
        let test = "resource::residency::tests::a_wait_that_expires_leaves_the_request_pending";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        // SAFETY: plain creation.
        let fence: ID3D12Fence =
            unsafe { host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.unwrap();
        let r = Residency::asked(fence.clone());
        let started = Instant::now();
        assert!(!r.wait(Duration::from_millis(50)));
        assert!(started.elapsed() >= Duration::from_millis(40));
        assert!(!r.refused() && !r.is_ready());
        // SAFETY: a CPU signal of our own fence.
        unsafe { fence.Signal(1) }.unwrap();
        assert!(r.is_ready());
        assert!(r.wait(Duration::ZERO));
    }

    /// A refused request is asked again once [`RETRY`] has passed, on the
    /// next look, and then becomes resident.
    #[test]
    fn a_refused_request_is_asked_again() {
        let test = "resource::residency::tests::a_refused_request_is_asked_again";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let texture = not_resident(&host.device);
        let r = Residency {
            ready: AtomicBool::new(false),
            state: Mutex::new(State::Refused {
                device: host.device.clone(),
                objects: pageable(&texture),
                since: Instant::now(),
            }),
        };
        assert!(!r.is_ready(), "not before its retry time");
        assert!(r.refused());
        if let State::Refused { since, .. } = &mut *r.lock() {
            *since -= RETRY;
        }
        assert!(!r.is_ready() || !r.refused(), "asked again");
        assert!(!r.refused());
        assert!(r.wait(Duration::from_secs(5)));
    }
}
