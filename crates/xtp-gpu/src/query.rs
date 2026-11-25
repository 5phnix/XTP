//! Title query heaps and their host occlusion and pipeline-statistics queries.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUERY_HEAP, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x0d9658ae,
        0xed45,
        0x469e,
        [0xa6, 0x1d, 0x97, 0x0e, 0xc5, 0x83, 0xca, 0xb4],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Desc {
    kind: u32,
    count: u32,
    nodes: u32,
}

#[repr(C)]
struct Heap {
    object: Object,
    desc: Desc,
    #[cfg(windows)]
    host: Option<windows::Win32::Graphics::Direct3D12::ID3D12QueryHeap>,
}

/// Device method.
pub(crate) extern "system" fn create(
    _: This,
    desc: *const Desc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    tracing::debug!(?desc, "query heap");
    let heap = Box::new(Heap {
        object: Object::new(&FUNCTIONS),
        desc,
        #[cfg(windows)]
        host: make_host(desc),
    });
    object::hand_out(QUERY_HEAP, heap, iid, &HEAP_IIDS, out)
}

#[cfg(windows)]
fn make_host(desc: Desc) -> Option<windows::Win32::Graphics::Direct3D12::ID3D12QueryHeap> {
    use windows::Win32::Graphics::Direct3D12::*;
    let kind = match desc.kind {
        0 => D3D12_QUERY_HEAP_TYPE_OCCLUSION,
        2 => D3D12_QUERY_HEAP_TYPE_PIPELINE_STATISTICS,
        _ => return None,
    };
    let mut heap = None;
    // SAFETY: the output belongs to us and the descriptor lives through the call.
    unsafe {
        crate::host::host()?.device.CreateQueryHeap(
            &D3D12_QUERY_HEAP_DESC {
                Type: kind,
                Count: desc.count,
                NodeMask: 0,
            },
            &mut heap,
        )
    }
    .ok()?;
    heap
}

#[cfg(windows)]
pub(crate) fn host_heap<'a>(
    this: This,
    kind: u32,
    first: u32,
    count: u32,
) -> Option<&'a windows::Win32::Graphics::Direct3D12::ID3D12QueryHeap> {
    // SAFETY: a title query heap passed to one of its command-list methods.
    let heap = unsafe { this.cast::<Heap>().as_ref()? };
    if !matches!((heap.desc.kind, kind), (0, 0 | 1) | (2, 3))
        || count == 0
        || first.checked_add(count)? > heap.desc.count
    {
        return None;
    }
    heap.host.as_ref()
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUERY_HEAP,
        iid,
        object::is(iid, &HEAP_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Heap>(this)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::object::slot;
    use crate::testgpu::{Kernel, Pass, TitleBuffer, TitleList};

    #[test]
    fn queries_count_only_work_between_the_marks() {
        let test = "query::tests::queries_count_only_work_between_the_marks";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let kernel = Kernel::new(host);
        let scratch = TitleBuffer::new(256);
        for (heap_kind, kind, words) in [(0, 0, 2), (0, 1, 2), (2, 3, 22)] {
            let mut heap = std::ptr::null();
            let desc = Desc {
                kind: heap_kind,
                count: 2,
                nodes: 0,
            };
            assert_eq!(
                create(std::ptr::null(), &desc, &HEAP_IIDS[0], &mut heap),
                crate::object::S_OK
            );
            let list = TitleList::new();
            let out = TitleBuffer::new(256);
            out.set_words(|_| 0xface);
            let mark = |offset, index| {
                slot::<extern "system" fn(This, This, u32, u32)>(list.this(), offset)(
                    list.this(),
                    heap,
                    kind,
                    index,
                )
            };
            mark(0xe0, 0);
            mark(0xe8, 0);
            mark(0xe0, 1);
            if kind == 3 {
                kernel.dispatch(
                    list.host(),
                    Pass::Fill,
                    scratch.address(),
                    scratch.address(),
                );
            }
            mark(0xe8, 1);
            slot::<extern "system" fn(This, This, u32, u32, u32, This, u64)>(list.this(), 0xf0)(
                list.this(),
                heap,
                kind,
                0,
                2,
                out.resource(),
                8,
            );
            release(heap);
            list.run();
            let result = out.words();
            let mut expected = vec![0; 2 * words];
            if kind == 3 {
                expected[words + 20] = 64;
            } // CSInvocations of the second query.
            assert_eq!(&result[2..2 + expected.len()], expected);
            assert_eq!((result[1], result[2 + expected.len()]), (0xface, 0xface));
        }
    }
}

/// IUnknown 3, object 4, device child 1.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<QUERY_HEAP, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};
