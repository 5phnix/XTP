//! Indirect root SRVs let each recording supply its own binding at submission.
//! A missing TLAS sets that dispatch's command count to zero.
use super::*;

pub(super) fn signature(root: &ID3D12RootSignature, param: u32) -> Option<ID3D12CommandSignature> {
    let mut srv = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW,
        ..Default::default()
    };
    srv.Anonymous.ShaderResourceView = D3D12_INDIRECT_ARGUMENT_DESC_0_3 {
        RootParameterIndex: param,
    };
    let args = [
        srv,
        D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH,
            ..Default::default()
        },
    ];
    let mut signature = None;
    // SAFETY: the descriptions match the packed upload record and live root signature.
    unsafe {
        crate::host::made()?.device.CreateCommandSignature(
            &D3D12_COMMAND_SIGNATURE_DESC {
                ByteStride: 20,
                NumArgumentDescs: args.len() as u32,
                pArgumentDescs: args.as_ptr(),
                NodeMask: 0,
            },
            root,
            &mut signature,
        )
    }
    .ok()?;
    signature
}

/// Dispatches a trace with this descriptor's TLAS at its place in submission order.
/// The caller sets the remaining root parameters and the compute PSO first.
///
/// # Safety
/// The title list and descriptor slot are live. The currently bound compute root
/// signature is `root` and `param` is its acceleration-structure root SRV.
pub unsafe fn dispatch_descriptor_tlas(
    title_list: *const Object,
    handle: u64,
    root: &ID3D12RootSignature,
    param: u32,
    groups: [u32; 3],
) -> bool {
    let Some(address) = descriptor_address(handle) else {
        return false;
    };
    let Some(list) = crate::list::host_list(title_list) else {
        return false;
    };
    dispatch(list, address, root, param, groups).is_some()
}

pub(super) fn dispatch(
    list: &HostList,
    address: u64,
    root: &ID3D12RootSignature,
    param: u32,
    groups: [u32; 3],
) -> Option<()> {
    let signature = signature(root, param)?;
    let (args, mapped) = list.upload(crate::host::made()?, 24)?;
    let mapped = mapped as usize;
    let slot = registry().trace_slot(address);
    let retained = Mutex::new(None);
    list.defer_host((0, 0), move || {
        let tlas = slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let binding = tlas.and_then(|s| Some((s.result, s.instances?.current())));
        let gpu = binding.as_ref().map_or(0, |(r, _)| r.gpu());
        // SAFETY: six aligned words in the recording's private upload. Replay
        // waits for the preceding execution before rewriting the count and SRV.
        unsafe {
            (mapped as *mut [u32; 6]).write([
                gpu as u32,
                (gpu >> 32) as u32,
                groups[0],
                groups[1],
                groups[2],
                u32::from(binding.is_some()),
            ]);
        }
        *retained.lock().unwrap_or_else(|e| e.into_inner()) = binding;
    });
    list.keep(&signature);
    // SAFETY: the retained buffer contains the SRV/dispatch and its separate count word.
    unsafe {
        list.list
            .ExecuteIndirect(&signature, 1, &args, 0, &args, 20);
    }
    Some(())
}
