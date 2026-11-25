use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// One host postbuild slot. Only the completion thread reads its mapped bytes.
#[derive(Clone)]
pub(crate) struct CompactedSize {
    _page: ID3D12Resource,
    mapped: usize,
    size: Arc<AtomicU64>,
}

impl CompactedSize {
    pub(crate) fn complete(&self) {
        // SAFETY: the retained page contains this aligned word. The host signalled
        // the submission fence, and list replay waits before rewriting the slot.
        let bytes = unsafe { (self.mapped as *const u64).read_volatile() };
        self.size.store(bytes, Ordering::Release);
    }
}

#[derive(Default)]
pub(crate) struct SizePage {
    page: Option<(ID3D12Resource, usize)>,
    used: usize,
}

impl SizePage {
    pub(crate) fn reserve(&mut self) -> Option<(u64, CompactedSize)> {
        const PAGE_BYTES: usize = 64 << 10;
        if self.page.is_none() || self.used == PAGE_BYTES {
            let (page, data) = crate::host::made()?.shared_buffer(PAGE_BYTES as u64)?;
            self.page = Some((page, data as usize));
            self.used = 0;
        }
        let (page, data) = self.page.as_ref()?;
        let offset = self.used;
        self.used += 8;
        // SAFETY: the slot is eight-byte aligned and inside the live page.
        let address = unsafe { page.GetGPUVirtualAddress() } + offset as u64;
        Some((
            address,
            CompactedSize {
                _page: page.clone(),
                mapped: data + offset,
                size: Arc::default(),
            },
        ))
    }
}

pub(super) fn request(
    list: &HostList,
) -> Option<(
    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC,
    Arc<AtomicU64>,
)> {
    let (gpu, sample) = list.rt_compacted_size()?;
    let desc = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC {
        DestBuffer: gpu,
        InfoType: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_COMPACTED_SIZE,
    };
    Some((desc, sample.size))
}
