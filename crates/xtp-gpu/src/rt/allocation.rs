//! AS slices share a permanently AS-state buffer. Its physical allocation
//! stays charged until the last registry, BLAS map or recording reference goes.
use super::*;
use crate::host::buffers::{Ranges, RtReservation};
use std::sync::Weak;

const PAGE: u64 = 1 << 20;
const ALIGN: u64 = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BYTE_ALIGNMENT as u64;

struct Page {
    resource: ID3D12Resource,
    free: Mutex<Ranges>,
    _reservation: RtReservation,
}

pub(crate) struct ResultBuffer {
    page: Arc<Page>,
    offset: u64,
    pub(super) bytes: u64,
}

impl ResultBuffer {
    pub(super) fn new(rec: &Recorder<'_>, bytes: u64) -> Option<Arc<Self>> {
        static PAGES: Mutex<Vec<Weak<Page>>> = Mutex::new(Vec::new());
        let len = bytes.checked_next_multiple_of(ALIGN)?;
        if len == 0 {
            return None;
        }
        let small = len <= PAGE / 2;
        if small {
            let mut pages = PAGES.lock().unwrap_or_else(|e| e.into_inner());
            pages.retain(|p| p.strong_count() != 0);
            for page in pages.iter().filter_map(Weak::upgrade) {
                let offset = page
                    .free
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take(len);
                if let Some(offset) = offset {
                    return Some(Arc::new(Self {
                        page,
                        offset,
                        bytes,
                    }));
                }
            }
        }
        let size = if small {
            PAGE
        } else {
            len.checked_next_multiple_of(65536)?
        };
        let started = tracing::enabled!(tracing::Level::DEBUG).then(std::time::Instant::now);
        let (resource, reservation) =
            RtReservation::allocate(rec.device, size, || committed_result(rec.device, size))?;
        if let Some(started) = started {
            tracing::debug!(
                bytes = size,
                state = D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE.0,
                us = started.elapsed().as_micros() as u64,
                "host DXR buffer allocation"
            );
        }
        let mut free = Ranges::new(size);
        let offset = free.take(len)?;
        let page = Arc::new(Page {
            resource,
            free: Mutex::new(free),
            _reservation: reservation,
        });
        if small {
            PAGES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Arc::downgrade(&page));
        }
        Some(Arc::new(Self {
            page,
            offset,
            bytes,
        }))
    }

    pub(super) fn gpu(&self) -> u64 {
        // SAFETY: this slice holds the page and its aligned range.
        unsafe { self.page.resource.GetGPUVirtualAddress() + self.offset }
    }
}

impl Drop for ResultBuffer {
    fn drop(&mut self) {
        self.page
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .give(self.offset, self.bytes.next_multiple_of(ALIGN));
    }
}

fn committed_result(device: &ID3D12Device5, bytes: u64) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut resource = None;
    // SAFETY: one committed, shader-writable buffer with a state matching its use.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
            None,
            &mut resource,
        )
    }
    .ok()?;
    resource
}
