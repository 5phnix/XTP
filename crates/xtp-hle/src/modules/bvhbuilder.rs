//! Trace builder imports resolved to the title DLL.
//! Enable the xtp_hle::modules::bvhbuilder debug target to capture a session.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tracing::{Level, debug, enabled};
use windows::Win32::Graphics::Direct3D12::*;
use xtp_loader::{ImportResolver, Provenance, Resolution};
use xtp_pe::ImportSymbol;

type Object = *mut c_void;
type Desc = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC;
type Inputs = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS;
type Sizes = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO;
type Postbuild = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC;

#[repr(C)]
struct BatchEntry {
    build: Desc,
    postbuild_count: u32,
    postbuild: *const Postbuild,
    reserved: [u64; 2],
}

const _: () = {
    assert!(size_of::<Desc>() == 48);
    assert!(std::mem::offset_of!(BatchEntry, postbuild) == 0x38);
    assert!(size_of::<BatchEntry>() == 80);
};

/// Wraps exports found in the title DLL.
pub struct WithBuilderTrace<'a>(pub &'a dyn ImportResolver);

impl ImportResolver for WithBuilderTrace<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let resolved = self.0.resolve(dll, symbol);
        if dll.eq_ignore_ascii_case("XboxBvhBuilder.dll")
            && let ImportSymbol::Name { name, .. } = symbol
            && let Resolution::Found {
                address,
                provenance: Provenance::Title,
            } = resolved
            && let Some(hook) = bind(name, address)
        {
            return Resolution::Found {
                address: hook,
                provenance: Provenance::Hle,
            };
        }
        resolved
    }
}

// One ABI type checks the hook and calls the original export.
macro_rules! methods {
    ($($hook:ident: $abi:ty = $name:literal),+ $(,)?) => {
        $(mod $hook {
            use super::*;
            pub(super) type Abi = $abi;
            const _: Abi = super::$hook;
            pub(super) static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
            pub(super) fn original() -> Abi {
                // SAFETY: bind stores the matching export before publishing the hook.
                unsafe { std::mem::transmute::<usize, Abi>(ORIGINAL.load(Ordering::Acquire)) }
            }
        })+
        fn bind(name: &str, address: usize) -> Option<usize> {
            match name {
                $($name => {
                    $hook::ORIGINAL.store(address, Ordering::Release);
                    Some($hook as *const () as usize)
                },)+
                _ => None,
            }
        }
    };
}

methods! {
    ctor: extern "system" fn(Object, Object, u8) -> Object =
        "??0XboxBvhBuilder@@QEAA@PEAUID3D12Device11@@_N@Z",
    dtor: extern "system" fn(Object) = "??1XboxBvhBuilder@@QEAA@XZ",
    build: extern "system" fn(Object, Object, u32, *const BatchEntry) =
        "?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList6@@IPEBUD3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2@@@Z",
    copy: extern "system" fn(Object, Object, u64, u64, i32) =
        "?CopyRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAXPEAUID3D12GraphicsCommandList4@@_K1W4D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE@@@Z",
    prebuild: extern "system" fn(Object, *const Inputs, *const c_void, *mut Sizes) =
        "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAXPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS@@PEBUD3D12XBOX_XDXR_STREAM_DESC@@PEAUD3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO@@@Z",
    rehydrate: extern "system" fn(Object, Object, u32, *const Desc, *const u64) -> i32 =
        "?RehydrateRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@PEB_K@Z",
    morton: extern "system" fn(Object, u8) = "?Set64BitMortonCodes@XboxBvhBuilder@@QEAAX_N@Z",
}

fn frame() -> u64 {
    xtp_gpu::queue::presented()
}

extern "system" fn ctor(this: Object, device: Object, flag: u8) -> Object {
    debug!(frame = frame(), ?this, ?device, flag, "ctor");
    ctor::original()(this, device, flag)
}

extern "system" fn dtor(this: Object) {
    debug!(frame = frame(), ?this, "dtor");
    dtor::original()(this)
}

extern "system" fn morton(this: Object, flag: u8) {
    debug!(frame = frame(), ?this, flag, "morton");
    morton::original()(this, flag)
}

extern "system" fn copy(this: Object, list: Object, dst: u64, src: u64, mode: i32) {
    debug!(
        frame = frame(),
        ?this,
        ?list,
        dst,
        src,
        mode,
        compact = mode == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT.0,
        "copy"
    );
    copy::original()(this, list, dst, src, mode);
    // SAFETY: the DLL receives this same live title list.
    unsafe { xtp_gpu::rt::Builder.copy(list.cast(), dst, src, mode) };
}

// Sample the frame at entry. The ID joins records from one call when
// title threads record command lists concurrently.
struct Call {
    frame: u64,
    id: u64,
}

impl Call {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            frame: frame(),
            id: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }

    fn inputs(&self, entry: u32, inputs: &Inputs) {
        let Self { frame, id } = *self;
        let count = inputs.NumDescs;
        let flags = inputs.Flags.0;
        let layout = inputs.DescsLayout;
        debug!(
            frame,
            id,
            entry,
            kind = inputs.Type.0,
            flags,
            count,
            layout = layout.0,
            "inputs"
        );
        if inputs.Type != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL {
            return;
        }
        for geometry in 0..count {
            let i = geometry as usize;
            // SAFETY: BLAS inputs provide count CPU descriptors in the selected layout.
            // GPU addresses in these descriptors are only logged.
            let desc = unsafe {
                match layout {
                    D3D12_ELEMENTS_LAYOUT_ARRAY if !inputs.Anonymous.pGeometryDescs.is_null() => {
                        inputs.Anonymous.pGeometryDescs.add(i)
                    }
                    D3D12_ELEMENTS_LAYOUT_ARRAY_OF_POINTERS
                        if !inputs.Anonymous.ppGeometryDescs.is_null() =>
                    {
                        *inputs.Anonymous.ppGeometryDescs.add(i)
                    }
                    _ => break,
                }
                .as_ref()
            };
            let Some(desc) = desc else { continue };
            let flags = desc.Flags.0;
            match desc.Type {
                D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES => {
                    // SAFETY: the discriminant selects Triangles.
                    let t = unsafe { &desc.Anonymous.Triangles };
                    debug!(
                        frame,
                        id,
                        entry,
                        geometry,
                        flags,
                        vertex_format = t.VertexFormat.0,
                        index_format = t.IndexFormat.0,
                        vertex_address = t.VertexBuffer.StartAddress,
                        index_address = t.IndexBuffer,
                        vertices = t.VertexCount,
                        indices = t.IndexCount,
                        stride = t.VertexBuffer.StrideInBytes,
                        transform = t.Transform3x4 != 0,
                        "triangles"
                    );
                }
                D3D12_RAYTRACING_GEOMETRY_TYPE_PROCEDURAL_PRIMITIVE_AABBS => {
                    // SAFETY: the discriminant selects AABBs.
                    let a = unsafe { &desc.Anonymous.AABBs };
                    debug!(
                        frame,
                        id,
                        entry,
                        geometry,
                        flags,
                        boxes = a.AABBCount,
                        stride = a.AABBs.StrideInBytes,
                        "aabbs"
                    );
                }
                _ => debug!(
                    frame,
                    id,
                    entry,
                    geometry,
                    flags,
                    kind = desc.Type.0,
                    "geometry"
                ),
            }
        }
    }

    fn description(&self, entry: u32, desc: &Desc) {
        debug!(
            frame = self.frame,
            id = self.id,
            entry,
            dst = desc.DestAccelerationStructureData,
            src = desc.SourceAccelerationStructureData,
            scratch = desc.ScratchAccelerationStructureData,
            refit = desc
                .Inputs
                .Flags
                .contains(D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PERFORM_UPDATE),
            "description"
        );
        self.inputs(entry, &desc.Inputs);
    }
}

/// Title and host prebuild sizes describe different BVH formats. Ask the
/// title builder with the active inputs while their CPU arrays are live.
fn title_bytes(this: Object, desc: &Desc) -> Option<u64> {
    if prebuild::ORIGINAL.load(Ordering::Acquire) == 0 {
        return None;
    }
    let mut inputs = desc.Inputs;
    inputs.Flags &= !D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PERFORM_UPDATE;
    let mut sizes = Sizes::default();
    prebuild::original()(this, &inputs, std::ptr::null(), &mut sizes);
    Some(sizes.ResultDataMaxSizeInBytes).filter(|&bytes| bytes != 0)
}

extern "system" fn build(this: Object, list: Object, count: u32, entries: *const BatchEntry) {
    if enabled!(Level::DEBUG) {
        let call = Call::new();
        debug!(
            frame = call.frame,
            id = call.id,
            ?this,
            ?list,
            count,
            "build"
        );
        if !entries.is_null() {
            for i in 0..count {
                // SAFETY: the title supplies count live 80-byte entries.
                let e = unsafe { &*entries.add(i as usize) };
                call.description(i, &e.build);
                if !e.postbuild.is_null() {
                    for j in 0..e.postbuild_count {
                        // SAFETY: this entry owns its postbuild array.
                        let p = unsafe { &*e.postbuild.add(j as usize) };
                        debug!(
                            frame = call.frame,
                            id = call.id,
                            entry = i,
                            kind = p.InfoType.0,
                            dst = p.DestBuffer,
                            "postbuild"
                        );
                    }
                }
            }
        }
    }
    build::original()(this, list, count, entries);
    if !entries.is_null() {
        // SAFETY: the title supplies count live entries and records no intervening list commands.
        unsafe {
            let entries = std::slice::from_raw_parts(entries, count as usize);
            xtp_gpu::rt::Builder.build_batch_sized(
                list.cast(),
                entries
                    .iter()
                    .map(|entry| (&entry.build, title_bytes(this, &entry.build))),
            );
        }
    }
}

extern "system" fn prebuild(
    this: Object,
    inputs: *const Inputs,
    stream: *const c_void,
    out: *mut Sizes,
) {
    let call = enabled!(Level::DEBUG).then(Call::new);
    if let Some(call) = &call {
        debug!(frame = call.frame, id = call.id, ?this, ?stream, "prebuild");
        // SAFETY: the title keeps the inputs alive through this call.
        if let Some(inputs) = unsafe { inputs.as_ref() } {
            call.inputs(0, inputs);
        }
    }
    prebuild::original()(this, inputs, stream, out);
    if let Some(call) = call
        // SAFETY: the DLL has filled all three sizes when given an output.
        && let Some(s) = unsafe { out.as_ref() }
    {
        debug!(
            frame = call.frame,
            id = call.id,
            result = s.ResultDataMaxSizeInBytes,
            scratch = s.ScratchDataSizeInBytes,
            update_scratch = s.UpdateScratchDataSizeInBytes,
            "sizes"
        );
    }
}

extern "system" fn rehydrate(
    this: Object,
    list: Object,
    count: u32,
    descs: *const Desc,
    cpu: *const u64,
) -> i32 {
    if enabled!(Level::DEBUG) {
        let call = Call::new();
        debug!(
            frame = call.frame,
            id = call.id,
            ?this,
            ?list,
            count,
            "rehydrate"
        );
        if !descs.is_null() {
            for i in 0..count {
                // SAFETY: the title supplies count contiguous build descriptions.
                call.description(i, unsafe { &*descs.add(i as usize) });
            }
        }
    }
    let hr = rehydrate::original()(this, list, count, descs, cpu);
    if hr >= 0 && !descs.is_null() {
        let mut recorded = 0;
        for i in 0..count {
            // SAFETY: the original call accepted this list and array of count descriptions.
            let desc = unsafe { &*descs.add(i as usize) };
            // SAFETY: the title list and the active CPU inputs are live for this call.
            recorded += u32::from(unsafe {
                xtp_gpu::rt::rehydrate_sized(list.cast(), desc, title_bytes(this, desc))
            });
        }
        debug!(
            count,
            recorded,
            unavailable = count - recorded,
            "rehydrated host BLAS recordings"
        );
    }
    hr
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Resolved(Resolution);
    impl ImportResolver for Resolved {
        fn resolve(&self, _: &str, _: &ImportSymbol) -> Resolution {
            self.0
        }
    }

    #[test]
    fn rt_builder_forwards_the_fifth_argument_and_hresult() {
        extern "system" fn real(
            this: Object,
            list: Object,
            count: u32,
            desc: *const Desc,
            cpu: *const u64,
        ) -> i32 {
            if this.addr() == 8
                && list.addr() == 16
                && count == 1
                && !desc.is_null()
                && !cpu.is_null()
            {
                // SAFETY: the test inputs are live until the hook returns.
                if unsafe { (*desc).DestAccelerationStructureData == 32 && *cpu == 64 } {
                    return -42;
                }
            }
            0
        }

        let title = Resolution::Found {
            address: real as *const () as usize,
            provenance: Provenance::Title,
        };
        let symbol = ImportSymbol::Name { hint: 0, name: "?RehydrateRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAJPEAUID3D12GraphicsCommandList8@@IPEBUD3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC@@PEB_K@Z".into() };
        let r = WithBuilderTrace(&Resolved(title));
        assert_eq!(r.resolve("elsewhere.dll", &symbol), title);
        assert_eq!(
            r.resolve("XboxBvhBuilder.dll", &ImportSymbol::Ordinal(1)),
            title
        );
        assert_eq!(
            WithBuilderTrace(&Resolved(Resolution::Missing)).resolve("XboxBvhBuilder.dll", &symbol),
            Resolution::Missing
        );
        let Resolution::Found {
            address,
            provenance: Provenance::Hle,
        } = r.resolve("XBOXBVHBUILDER.DLL", &symbol)
        else {
            panic!("builder import was not intercepted");
        };
        // SAFETY: the resolver bound this symbol to the rehydrate ABI.
        let hook = unsafe { std::mem::transmute::<usize, rehydrate::Abi>(address) };
        let desc = Desc {
            DestAccelerationStructureData: 32,
            ..Default::default()
        };
        assert_eq!(hook(8 as Object, 16 as Object, 1, &desc, &64), -42);
    }
}
