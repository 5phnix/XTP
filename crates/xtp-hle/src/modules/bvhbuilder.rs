//! Observation des imports du constructeur BVH, après résolution dans la DLL du titre.
//! Activer la cible `xtp_hle::modules::bvhbuilder=debug` pour relever une session.

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

/// Enveloppe uniquement les exports effectivement trouvés dans la DLL du titre.
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

// Le même type vérifie le point d'entrée et sert à rappeler la méthode d'origine.
macro_rules! methods {
    ($($hook:ident: $abi:ty = $name:literal),+ $(,)?) => {
        $(mod $hook {
            use super::*;
            pub(super) type Abi = $abi;
            const _: Abi = super::$hook;
            pub(super) static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
            pub(super) fn original() -> Abi {
                // SAFETY: bind installe l'export de cette signature avant de publier le hook.
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
    unsafe { xtp_gpu::rt::copy(list.cast(), dst, src, mode) };
}

// Une frame est échantillonnée à l'entrée. L'identifiant relie les lignes même si
// deux threads enregistrent des listes en parallèle.
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
            // SAFETY: le contrat BLAS fournit count descriptions CPU selon le layout.
            // Les adresses GPU contenues dans ces descriptions ne sont jamais lues.
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
                    // SAFETY: le discriminant sélectionne le membre Triangles.
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
                    // SAFETY: le discriminant sélectionne le membre AABBs.
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
                // SAFETY: le titre passe count entrées CPU de 80 octets.
                let e = unsafe { &*entries.add(i as usize) };
                call.description(i, &e.build);
                if !e.postbuild.is_null() {
                    for j in 0..e.postbuild_count {
                        // SAFETY: le tableau postbuild appartient à cette entrée.
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
        for i in 0..count {
            // SAFETY: these CPU descriptors and the list belong to the current call.
            unsafe { xtp_gpu::rt::build(list.cast(), &(*entries.add(i as usize)).build) };
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
        // SAFETY: les entrées appartiennent à l'appel du titre et restent en vie.
        if let Some(inputs) = unsafe { inputs.as_ref() } {
            call.inputs(0, inputs);
        }
    }
    prebuild::original()(this, inputs, stream, out);
    if let Some(call) = call
        // SAFETY: la DLL vient d'écrire les trois tailles si une sortie est fournie.
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
                // SAFETY: cette méthode passe count descriptions standard contiguës.
                call.description(i, unsafe { &*descs.add(i as usize) });
            }
        }
    }
    let hr = rehydrate::original()(this, list, count, descs, cpu);
    if hr >= 0 && !descs.is_null() {
        let mut recorded = 0;
        for i in 0..count {
            // SAFETY: the original call accepted this list and array of count descriptions.
            recorded +=
                u32::from(unsafe { xtp_gpu::rt::rehydrate(list.cast(), &*descs.add(i as usize)) });
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
                // SAFETY: les données synthétiques sont conservées jusqu'au retour de l'appel.
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
            panic!("interception absente");
        };
        // SAFETY: le résolveur a associé ce symbole à la signature exacte de rehydrate.
        let hook = unsafe { std::mem::transmute::<usize, rehydrate::Abi>(address) };
        let desc = Desc {
            DestAccelerationStructureData: 32,
            ..Default::default()
        };
        assert_eq!(hook(8 as Object, 16 as Object, 1, &desc, &64), -42);
    }
}
