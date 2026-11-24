//! Indirect rectangle-list draws (the console's topology 17,
//! `ExecuteIndirect` kind 0), expanded on the GPU. A direct rectangle draw
//! expands its vertices on the CPU ([`crate::primitive::expand_rects`]);
//! an indirect one knows its vertex counts only on the GPU. So after
//! [`crate::indirect::clamp_uav`] bounds each record, two compute passes
//! of ours run before `ExecuteIndirect`:
//!
//! 1. `rect_rewrite` reads each live record (records at or past the
//!    title's count, records with no instance and records whose source
//!    vertices `start..start + 3 * rects` reach past a bound per-vertex
//!    vertex buffer draw nothing), allocates `rects * 6` vertices from the
//!    list's scratch with an atomic counter, and writes a rewritten record
//!    into the list's own arguments buffer. The title's arguments are left
//!    as the clamp made them.
//! 2. `rect_expand` runs once per per-vertex input slot and writes
//!    `(v0, v1, v2, v0, v2, v3)` into that slot's scratch region, with
//!    `v3 = v0 + v2 - v1` for the formats and in-stride components that
//!    [`crate::primitive::expand_rects`] computes, the same way (formats,
//!    conversions, rounding: a test compares both byte for byte).
//!
//! `ExecuteIndirect` then reads only the list's own buffers: our arguments,
//! in which records at or past the title's count are empty draws (so no
//! count buffer, and no title buffer changes state), and the expanded
//! vertices. A list's scratch ([`Scratch`]) is reused by each of its
//! indirect rectangle draws, and by later lists once its recording has
//! gone.
//!
//! Both passes are HLSL compiled at run time by the system's `D3DCompile`.
//! They read buffers as 32-bit words and take their root signatures from
//! code, which Wine's compiler (vkd3d-shader) accepts too.
//!
//! Ported from Marvisak/VIctory#346 (head 8ef6bfe) by BastienRodz, with
//! fuzeofficlal's half rounding (`primitive.rs`). Changes in the port:
//! - the list's memory is draw state's ([`crate::draw`]'s list memory),
//!   not the host list's: one scratch per recording, pooled;
//! - the allocation counter is the arguments buffer's first word, zeroed
//!   in the list before each draw's rewrite pass (`WriteBufferImmediate`),
//!   not by the CPU at submission: two rectangle draws of one list each
//!   start from zero, whichever order the GPU sees the CPU's writes in;
//! - a source vertex buffer is read 4 bytes short of its host copy's end:
//!   an unaligned read takes the next word, and a root view has no bounds;
//! - the fourth corner is computed `precise` (no reassociation), as the
//!   CPU does it.
//!
//! shortcut: the vertex scratch holds 8 MiB ([`SCRATCH_BYTES`]), shared by
//! the records of one draw; records whose expanded vertices do not fit draw
//! nothing.
//!
//! shortcut: as in [`crate::primitive`], the fourth corner is computed from
//! vertex inputs, not vertex shader outputs; formats unknown to
//! [`crate::primitive::components`] and components past the vertex stride
//! keep the first vertex's bytes. A pipeline without per-vertex inputs (one
//! that makes its corners from the vertex id) gets `6 * rects` vertices from
//! 0, as a direct rectangle draw does. With inputs, the vertex id is the
//! expanded one, from an allocated start whose order between records may
//! change from run to run. Vertex strides not a multiple of 4 are padded to
//! 4 bytes in the scratch (and bound with that stride), so that no two
//! threads share a word.
//!
//! The rewrite pass bounds counts and instances itself, as the clamp does,
//! so it stays safe whether or not the clamp wrote its results in place.

use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::{Interface, s};

use crate::host::{HostList, host, transition, uav_barrier};
use crate::pipeline::host::Input;
use crate::primitive;
use crate::shadow::Shadow;

/// A list's vertex scratch, shared by the records of one draw.
pub(crate) const SCRATCH_BYTES: u64 = 8 << 20;

/// The records one 64-thread-group pass takes at most.
const MAX_RECORDS: u32 = 65535 * 64;

/// Thread groups per dispatch dimension (D3D12's limit).
const MAX_GROUPS_PER_DIM: u32 = 65535;

/// The arguments buffer's first bytes: the allocation counter (one word,
/// the rest padding); the rewritten records follow.
const COUNTER_BYTES: u64 = 16;

/// The arguments buffer's smallest size: 4,095 records.
const ARGS_MIN: u64 = 64 << 10;

/// Scratches kept for later lists when no list holds them.
const POOLED: usize = 16;

/// One per-vertex input element, as `rect_expand` reads it (8 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Element {
    /// Byte offset in the source (and destination) vertex.
    pub(crate) offset: u32,
    /// Low byte: [`primitive::Kind::code`]; bits 8 on: the components
    /// inside the vertex stride (1 to 4).
    pub(crate) packed: u32,
}

/// One per-vertex slot's region in the vertex scratch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotLayout {
    pub(crate) slot: u32,
    /// From the scratch's start, 16-byte aligned.
    pub(crate) dst_offset: u64,
    /// The source stride rounded up to 4 bytes.
    pub(crate) dst_stride: u32,
    /// The region's bytes: the vertex capacity times `dst_stride`.
    pub(crate) dst_size: u32,
}

/// The element table of `slot` with vertices of `stride` bytes, as
/// [`primitive::expand_rects`] reads them, and whether the pipeline has
/// any per-vertex input in that slot (even of formats it does not know: the
/// slot is expanded then, with `v3 = v0`).
pub(crate) fn slot_elements(inputs: &[Input], slot: u32, stride: u32) -> (bool, Vec<Element>) {
    let mut per_vertex = false;
    let mut out = Vec::new();
    let stride = stride as usize;
    for e in inputs {
        if e.slot != slot || e.per_instance {
            continue;
        }
        per_vertex = true;
        let Some((n, kind)) = primitive::components(e.format) else {
            continue;
        };
        let inside = stride.saturating_sub(e.offset as usize) / kind.bytes();
        let count = n.min(inside) as u32;
        if count == 0 {
            continue;
        }
        out.push(Element {
            offset: e.offset,
            packed: kind.code() | (count << 8),
        });
    }
    (per_vertex, out)
}

/// Disjoint 16-byte aligned regions in `capacity_bytes` for every
/// per-vertex `(slot, source stride)`, all of one vertex capacity: the
/// capacity (whole rectangles) and the regions. `None` if a stride is zero
/// or not one rectangle fits.
pub(crate) fn scratch_layout(
    slots: &[(u32, u32)],
    capacity_bytes: u64,
) -> Option<(u32, Vec<SlotLayout>)> {
    if slots.is_empty() {
        return Some((0, Vec::new()));
    }
    let mut dst_strides = Vec::with_capacity(slots.len());
    let mut total_stride: u64 = 0;
    for &(_, src_stride) in slots {
        if src_stride == 0 {
            return None;
        }
        let dst_stride = src_stride.checked_next_multiple_of(4)?;
        dst_strides.push(dst_stride);
        total_stride = total_stride.checked_add(u64::from(dst_stride))?;
    }
    // Without the padding between regions at most this many fit; the
    // padding (under 16 bytes a region) costs a few rectangles at most, so
    // step down until the padded layout fits.
    let mut capacity = ((capacity_bytes / total_stride / 6) * 6).min(u64::from(u32::MAX) / 6 * 6);
    let layouts = loop {
        if capacity < 6 {
            return None;
        }
        if let Some(layouts) = place(slots, &dst_strides, capacity, capacity_bytes) {
            break layouts;
        }
        capacity -= 6;
    };
    Some((capacity as u32, layouts))
}

/// The slots' regions of `verts` vertices each, 16-byte aligned one after
/// the other, if they end within `capacity_bytes`.
fn place(
    slots: &[(u32, u32)],
    dst_strides: &[u32],
    verts: u64,
    capacity_bytes: u64,
) -> Option<Vec<SlotLayout>> {
    let mut cursor = 0u64;
    let mut layouts = Vec::with_capacity(slots.len());
    for (&(slot, _), &dst_stride) in slots.iter().zip(dst_strides) {
        let dst_offset = cursor.next_multiple_of(16);
        let dst_size = verts.checked_mul(u64::from(dst_stride))?;
        let end = dst_offset.checked_add(dst_size)?;
        if end > capacity_bytes {
            return None;
        }
        layouts.push(SlotLayout {
            slot,
            dst_offset,
            dst_stride,
            dst_size: u32::try_from(dst_size).ok()?,
        });
        cursor = end;
    }
    Some(layouts)
}

/// The vertex bound all per-vertex slots share: a record's `start..start +
/// 3 * rects` lies in every slot when it lies in `0..` this. `bounds` are
/// (bytes, stride); none: no bound.
pub(crate) fn max_source_vertices(bounds: &[(u64, u32)]) -> u32 {
    bounds
        .iter()
        .map(|&(size, stride)| {
            if stride == 0 {
                0
            } else {
                (size / u64::from(stride)).min(u64::from(u32::MAX)) as u32
            }
        })
        .min()
        .unwrap_or(u32::MAX)
}

/// A dispatch grid `(x, y)` of at least `records` groups, each dimension
/// within D3D12's limit.
pub(crate) fn dispatch_grid(records: u32) -> (u32, u32) {
    if records == 0 {
        return (0, 0);
    }
    let x = records.min(MAX_GROUPS_PER_DIM);
    (x, records.div_ceil(x))
}

const REWRITE_SOURCE: &str = r#"
cbuffer Params : register(b0) {
    uint records;
    uint src_stride;
    uint has_count;
    uint max_src_verts;
    uint vert_capacity;
    uint need_verts;
    uint pad0;
    uint pad1;
};
// Buffers as 32-bit words, and no root signature attribute: Wine's system
// compiler (vkd3d-shader) has no Load on RWByteAddressBuffer and ignores the
// attribute, so the root signatures are made in code.
RWStructuredBuffer<uint> src_args : register(u0);
RWStructuredBuffer<uint> count_buf : register(u1);
RWStructuredBuffer<uint> alloc : register(u2);
RWStructuredBuffer<uint> dst_args : register(u3);

void store4(uint at, uint4 v) {
    dst_args[at] = v.x;
    dst_args[at + 1u] = v.y;
    dst_args[at + 2u] = v.z;
    dst_args[at + 3u] = v.w;
}

[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint r = id.x;
    if (r >= records) return;
    uint dst_at = r * 4u;
    if (has_count != 0u) {
        uint live = count_buf[0];
        if (r >= live) {
            store4(dst_at, uint4(0u, 0u, 0u, 0u));
            return;
        }
    }
    uint at = r * (src_stride / 4u);
    uint4 rec = uint4(src_args[at], src_args[at + 1u], src_args[at + 2u], src_args[at + 3u]);
    // The clamp's bounds again: 2^24 vertices, 2^26 vertex instances.
    rec.x = min(rec.x, 1u << 24);
    rec.y = min(rec.y, (1u << 26) / max(rec.x, 1u));
    uint rects = rec.x / 3u;
    uint src_verts = rects * 3u;
    uint out_verts = rects * 6u;
    uint out_start = 0u;
    if (rects == 0u || rec.y == 0u || rec.z > max_src_verts || src_verts > max_src_verts - rec.z) {
        out_verts = 0u;
    } else if (need_verts != 0u) {
        if (out_verts > vert_capacity) {
            out_verts = 0u;
        } else {
            uint before;
            InterlockedAdd(alloc[0], out_verts, before);
            if (before >= vert_capacity || out_verts > vert_capacity - before) {
                out_verts = 0u;
            } else {
                out_start = before;
            }
        }
    }
    store4(dst_at, uint4(out_verts, rec.y, out_start, rec.w));
}
"#;

const EXPAND_SOURCE: &str = r#"
cbuffer Params : register(b0) {
    uint records;
    uint src_stride;
    uint grid_x;
    uint vb_src_bias;
    uint vb_src_stride;
    uint vb_dst_stride;
    uint elem_count;
    uint pad0;
};
// Buffers as 32-bit words (see the rewrite pass); byte positions are
// turned into word indices here.
StructuredBuffer<uint> elems : register(t0);
RWStructuredBuffer<uint> src_args : register(u0);
RWStructuredBuffer<uint> dst_args : register(u1);
RWStructuredBuffer<uint> src_vb : register(u2);
RWStructuredBuffer<uint> dst_vb : register(u3);

uint load_u8(uint pos) {
    uint w = src_vb[(pos & ~3u) >> 2u];
    return (w >> ((pos & 3u) * 8u)) & 0xffu;
}

uint load_u16(uint pos) {
    uint rem = pos & 3u;
    uint w0 = src_vb[(pos & ~3u) >> 2u];
    if (rem <= 2u) {
        return (w0 >> (rem * 8u)) & 0xffffu;
    }
    uint w1 = src_vb[((pos & ~3u) + 4u) >> 2u];
    return ((w0 >> 24u) | (w1 << 8u)) & 0xffffu;
}

uint load_u32(uint pos) {
    uint rem = pos & 3u;
    uint w0 = src_vb[(pos & ~3u) >> 2u];
    if (rem == 0u) {
        return w0;
    }
    uint w1 = src_vb[((pos & ~3u) + 4u) >> 2u];
    return (w0 >> (rem * 8u)) | (w1 << ((4u - rem) * 8u));
}

void store_u8(uint pos, uint val) {
    uint aligned = pos & ~3u;
    uint shift = (pos & 3u) * 8u;
    uint w = dst_vb[aligned >> 2u];
    w = (w & ~(0xffu << shift)) | ((val & 0xffu) << shift);
    dst_vb[aligned >> 2u] = w;
}

void store_u16(uint pos, uint val) {
    uint rem = pos & 3u;
    if (rem <= 2u) {
        uint aligned = pos & ~3u;
        uint shift = rem * 8u;
        uint w = dst_vb[aligned >> 2u];
        w = (w & ~(0xffffu << shift)) | ((val & 0xffffu) << shift);
        dst_vb[aligned >> 2u] = w;
    } else {
        store_u8(pos, val & 0xffu);
        store_u8(pos + 1u, (val >> 8u) & 0xffu);
    }
}

void store_u32(uint pos, uint val) {
    uint rem = pos & 3u;
    if (rem == 0u) {
        dst_vb[pos >> 2u] = val;
    } else {
        uint aligned = pos & ~3u;
        uint shift = rem * 8u;
        uint inv = (4u - rem) * 8u;
        uint mask = (1u << shift) - 1u;
        uint w0 = dst_vb[aligned >> 2u];
        w0 = (w0 & mask) | (val << shift);
        dst_vb[aligned >> 2u] = w0;
        uint w1 = dst_vb[(aligned + 4u) >> 2u];
        w1 = (w1 & ~mask) | (val >> inv);
        dst_vb[(aligned + 4u) >> 2u] = w1;
    }
}

void copy_vertex(uint src_off, uint dst_off) {
    uint full = vb_src_stride & ~3u;
    for (uint b = 0u; b < full; b += 4u) {
        dst_vb[(dst_off + b) >> 2u] = load_u32(src_off + b);
    }
    uint rem = vb_src_stride & 3u;
    if (rem != 0u) {
        uint w = 0u;
        for (uint k = 0u; k < rem; k++) {
            w |= load_u8(src_off + full + k) << (k * 8u);
        }
        dst_vb[(dst_off + full) >> 2u] = w;
    }
}

float half_to_f32(uint h) {
    uint sgn = (h >> 15u) << 31u;
    uint ex = (h >> 10u) & 0x1fu;
    uint man = h & 0x3ffu;
    if (ex == 0u) {
        if (man == 0u) return asfloat(sgn);
        float v = (float)man * (1.0f / 16777216.0f);
        return sgn != 0u ? -v : v;
    }
    if (ex == 31u) {
        return asfloat(sgn | 0x7f800000u | (man << 13u));
    }
    return asfloat(sgn | ((ex + 112u) << 23u) | (man << 13u));
}

uint f32_to_half(float v) {
    uint bits = asuint(v);
    uint sgn = (bits >> 16u) & 0x8000u;
    int ex = (int)((bits >> 23u) & 0xffu) - 127 + 15;
    uint man = bits & 0x7fffffu;
    if (ex >= 31) return sgn | 0x7c00u;
    if (ex <= 0) {
        // Half subnormals, rounded to even, as primitive.rs makes them.
        if (ex < -10) return sgn;
        uint sig = (1u << 23) | man;
        uint shift = (uint)(14 - ex);
        uint halfway = 1u << (shift - 1u);
        uint q = sig >> shift;
        uint r = sig & ((1u << shift) - 1u);
        bool up = r > halfway || (r == halfway && (q & 1u) != 0u);
        return sgn | (q + (up ? 1u : 0u));
    }
    // To nearest, halfway cases to even; added, not or-ed: a rounding carry
    // out of the mantissa raises the exponent, as in primitive.rs.
    uint kept = (man >> 13u) & 1u;
    return sgn | (((uint)ex << 10u) + ((man + 0x0fffu + kept) >> 13u));
}

float read_comp(uint kind, uint pos) {
    if (kind == 0u) return asfloat(load_u32(pos));
    if (kind == 1u) return half_to_f32(load_u16(pos));
    if (kind == 2u) return (float)load_u16(pos) / 65535.0f;
    return (float)load_u8(pos) / 255.0f;
}

void write_comp(uint kind, uint pos, float v) {
    if (kind == 0u) {
        store_u32(pos, asuint(v));
    } else if (kind == 1u) {
        store_u16(pos, f32_to_half(v));
    } else if (kind == 2u) {
        store_u16(pos, (uint)(saturate(v) * 65535.0f + 0.5f));
    } else {
        store_u8(pos, (uint)(saturate(v) * 255.0f + 0.5f));
    }
}

[numthreads(64, 1, 1)]
void main(uint3 gid : SV_GroupID, uint3 gtid : SV_GroupThreadID) {
    uint r = gid.y * grid_x + gid.x;
    if (r >= records) return;
    uint out_verts = dst_args[r * 4u];
    uint rects = out_verts / 6u;
    if (rects == 0u) return;
    uint dst_start = dst_args[r * 4u + 2u];
    uint src_start = src_args[r * (src_stride / 4u) + 2u];

    for (uint rect = gtid.x; rect < rects; rect += 64u) {
        uint v0_src = vb_src_bias + (src_start + rect * 3u) * vb_src_stride;
        uint v1_src = v0_src + vb_src_stride;
        uint v2_src = v1_src + vb_src_stride;
        uint d = (dst_start + rect * 6u) * vb_dst_stride;

        copy_vertex(v0_src, d);
        copy_vertex(v1_src, d + vb_dst_stride);
        copy_vertex(v2_src, d + vb_dst_stride * 2u);
        copy_vertex(v0_src, d + vb_dst_stride * 3u);
        copy_vertex(v2_src, d + vb_dst_stride * 4u);
        uint v3_dst = d + vb_dst_stride * 5u;
        copy_vertex(v0_src, v3_dst);

        for (uint e = 0u; e < elem_count; e++) {
            uint2 desc = uint2(elems[e * 2u], elems[e * 2u + 1u]);
            uint offset = desc.x;
            uint kind = desc.y & 0xffu;
            uint comp_count = desc.y >> 8u;
            uint comp_bytes = kind == 0u ? 4u : (kind == 3u ? 1u : 2u);
            for (uint c = 0u; c < comp_count; c++) {
                uint at = offset + c * comp_bytes;
                float a = read_comp(kind, v0_src + at);
                float b = read_comp(kind, v1_src + at);
                float f = read_comp(kind, v2_src + at);
                // In the CPU's order, not reassociated.
                precise float v3 = a + f - b;
                write_comp(kind, v3_dst + at, v3);
            }
        }
    }
}
"#;

struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

struct Shaders {
    rewrite: Pass,
    expand: Pass,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Shaders {}
// SAFETY: as above.
unsafe impl Sync for Shaders {}

fn shaders() -> Option<&'static Shaders> {
    static SHADERS: std::sync::OnceLock<Option<Shaders>> = std::sync::OnceLock::new();
    SHADERS
        .get_or_init(|| match make() {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "no rectangle expander: indirect rectangles dropped");
                None
            }
        })
        .as_ref()
}

fn compile_blob(
    name: windows::core::PCSTR,
    source: &str,
) -> Result<ID3DBlob, (windows::core::Error, String)> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            name,
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        let mut text = String::new();
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            text = String::from_utf8_lossy(bytes).into_owned();
            tracing::warn!("rectangle expander: {text}");
        }
        return Err((e, text));
    }
    code.ok_or_else(|| (windows::core::Error::empty(), String::new()))
}

/// A pass's root signature: eight root constants at `b0`, then one root
/// descriptor per entry of `views`, registers counted per kind from 0.
fn root_signature(
    device: &ID3D12Device,
    views: &[D3D12_ROOT_PARAMETER_TYPE],
) -> windows::core::Result<ID3D12RootSignature> {
    let mut params = vec![D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            Constants: D3D12_ROOT_CONSTANTS {
                ShaderRegister: 0,
                RegisterSpace: 0,
                Num32BitValues: 8,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    }];
    let (mut srv, mut uav) = (0, 0);
    for &kind in views {
        let register = if kind == D3D12_ROOT_PARAMETER_TYPE_SRV {
            &mut srv
        } else {
            &mut uav
        };
        params.push(D3D12_ROOT_PARAMETER {
            ParameterType: kind,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: *register,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        });
        *register += 1;
    }
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: params.len() as u32,
        pParameters: params.as_ptr(),
        ..Default::default()
    };
    let mut blob: Option<ID3DBlob> = None;
    // SAFETY: a description of root constants and root descriptors that
    // lives through the call; the out pointer is ours.
    unsafe {
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None)?;
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        device.CreateRootSignature(0, bytes)
    }
}

fn compile_pass(
    device: &ID3D12Device,
    name: windows::core::PCSTR,
    source: &str,
    views: &[D3D12_ROOT_PARAMETER_TYPE],
) -> windows::core::Result<Pass> {
    let code = compile_blob(name, source).map_err(|(e, _)| e)?;
    let root = root_signature(device, views)?;
    // SAFETY: the blob's own bytes, and a root signature of ours.
    unsafe {
        let bytes =
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Pass { root, pso })
    }
}

fn make() -> windows::core::Result<Shaders> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    const UAV: D3D12_ROOT_PARAMETER_TYPE = D3D12_ROOT_PARAMETER_TYPE_UAV;
    const SRV: D3D12_ROOT_PARAMETER_TYPE = D3D12_ROOT_PARAMETER_TYPE_SRV;
    let rewrite = compile_pass(device, s!("rect_rewrite"), REWRITE_SOURCE, &[UAV; 4])?;
    let expand = compile_pass(
        device,
        s!("rect_expand"),
        EXPAND_SOURCE,
        &[SRV, UAV, UAV, UAV, UAV],
    )?;
    Ok(Shaders { rewrite, expand })
}

/// What one recording's indirect rectangle draws are expanded into: the
/// vertex scratch, and the arguments buffer (the allocation counter, then
/// the rewritten records). Taken by a list's draw state on its first such
/// draw, given back ([`give_back`]) once the recording has gone.
pub(crate) struct Scratch {
    vertices: ID3D12Resource,
    args: ID3D12Resource,
    args_len: u64,
    /// The states the list's commands so far leave them in: common when a
    /// recording starts (buffers decay to it when a submission ends).
    vertices_state: D3D12_RESOURCE_STATES,
    args_state: D3D12_RESOURCE_STATES,
    /// Whether the host list keeps them (until its reset).
    kept: bool,
}

// SAFETY: D3D12 resources are free-threaded.
unsafe impl Send for Scratch {}

/// Scratches no list holds.
static POOL: Mutex<Vec<Scratch>> = Mutex::new(Vec::new());

/// Gives a recording's scratch back for later lists: its submissions have
/// run, so its buffers are back in the common state.
pub(crate) fn give_back(mut scratch: Scratch) {
    scratch.vertices_state = D3D12_RESOURCE_STATE_COMMON;
    scratch.args_state = D3D12_RESOURCE_STATE_COMMON;
    scratch.kept = false;
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    if pool.len() < POOLED {
        pool.push(scratch);
    }
}

/// A buffer of `size` bytes in GPU memory that shaders can write, in the
/// common state.
fn uav_buffer(size: u64, what: &str) -> Option<ID3D12Resource> {
    let host = host()?;
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
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
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a buffer.
    let made = unsafe {
        host.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut buffer,
        )
    };
    if let Err(e) = made {
        tracing::warn!(error = %e, size, "no {what} for indirect rectangles");
        return None;
    }
    let buffer = buffer?;
    crate::host::name(&buffer, || format!("rectangle {what} {size:#x}"));
    Some(buffer)
}

/// The list's scratch, with room for `args_len` bytes of arguments: the one
/// it holds, one from [`POOL`], or a new one.
fn take_scratch<'a>(
    list: &HostList,
    held: &'a mut Option<Scratch>,
    args_len: u64,
) -> Option<&'a mut Scratch> {
    if held.is_none() {
        let pooled = POOL.lock().unwrap_or_else(|e| e.into_inner()).pop();
        *held = match pooled {
            Some(s) => Some(s),
            None => Some(Scratch {
                vertices: uav_buffer(SCRATCH_BYTES, "vertices")?,
                args: uav_buffer(ARGS_MIN, "arguments")?,
                args_len: ARGS_MIN,
                vertices_state: D3D12_RESOURCE_STATE_COMMON,
                args_state: D3D12_RESOURCE_STATE_COMMON,
                kept: false,
            }),
        };
    }
    let s = held.as_mut()?;
    if !s.kept {
        list.keep(&s.vertices);
        list.keep(&s.args);
        s.kept = true;
    }
    if s.args_len < args_len {
        // The list keeps the smaller one until its reset (it may have used it).
        let len = args_len.next_power_of_two();
        s.args = uav_buffer(len, "arguments")?;
        s.args_len = len;
        s.args_state = D3D12_RESOURCE_STATE_COMMON;
        list.keep(&s.args);
    }
    Some(s)
}

/// Moves `buffer` from `*state` to `to` (nothing if it is there).
fn to_state(
    barriers: &mut Vec<D3D12_RESOURCE_BARRIER>,
    buffer: &ID3D12Resource,
    state: &mut D3D12_RESOURCE_STATES,
    to: D3D12_RESOURCE_STATES,
) {
    if *state != to {
        barriers.push(transition(buffer, *state, to));
        *state = to;
    }
}

/// An expanded indirect rectangle draw, ready for `ExecuteIndirect`.
pub(crate) struct Prepared {
    /// The rewritten records (stride 16, no count buffer) and where they
    /// start.
    pub(crate) args: ID3D12Resource,
    pub(crate) offset: u64,
    /// The expanded per-vertex slots.
    views: Vec<(u32, D3D12_VERTEX_BUFFER_VIEW)>,
}

impl Prepared {
    /// Binds the expanded per-vertex slots, over what the draw's own
    /// preparation bound there.
    pub(crate) fn bind_vertices(&self, list: &HostList) {
        for (slot, view) in &self.views {
            // SAFETY: an open list; the view names the list's scratch.
            unsafe { list.list.IASetVertexBuffers(*slot, Some(&[*view])) };
        }
    }
}

/// A source vertex slot of the draw.
struct Source {
    slot: u32,
    shadow: Arc<Shadow>,
    address: u64,
    size: u32,
    stride: u32,
    elements: Vec<Element>,
}

/// Clamps an indirect rectangle draw's `max` records of `stride` bytes at
/// title address `arguments` (host copy `args`), the record count at
/// `count` if the title gives one, and records the passes that rewrite
/// them into `held` (the list's scratch) with the vertices of the
/// per-vertex slots of `inputs` (`vertices`: the bound views, (address,
/// bytes, stride) by slot) expanded. On success the compute root signature
/// and pipeline are ours: the caller binds its draw, then
/// [`Prepared::bind_vertices`], then runs `ExecuteIndirect` on
/// [`Prepared::args`] with stride 16 and no count buffer. On error nothing
/// was recorded.
pub(crate) fn expand(
    list: &HostList,
    held: &mut Option<Scratch>,
    inputs: &[Input],
    vertices: &[(u64, u32, u32); 32],
    (args, arguments, stride, max): (&Arc<Shadow>, u64, u32, u32),
    count: Option<(&Arc<Shadow>, u64)>,
) -> Result<Prepared, &'static str> {
    if stride < 16 || !stride.is_multiple_of(4) || !args.gpu(arguments).is_multiple_of(4) {
        return Err("rectangle records short or not 4-byte aligned");
    }
    if max > MAX_RECORDS {
        return Err("too many indirect rectangle records");
    }
    if let Some((s, at)) = count
        && !s.gpu(at).is_multiple_of(4)
    {
        return Err("rectangle count not 4-byte aligned");
    }
    let mut sources = Vec::new();
    let mut bounds = Vec::new();
    for slot in 0..32u32 {
        let (address, size, slot_stride) = vertices[slot as usize];
        let (per_vertex, elements) = slot_elements(inputs, slot, slot_stride);
        if !per_vertex {
            continue;
        }
        if address == 0 || slot_stride == 0 || size == 0 {
            return Err("rectangle vertex buffer not bound");
        }
        let Some(shadow) = crate::shadow::find(address, u64::from(size)) else {
            return Err("rectangle vertex buffer outside title buffers");
        };
        // An unaligned read takes the next word: 4 bytes short of the host
        // copy's end, past which a root view reads anything.
        let readable = u64::from(size)
            .min(shadow.left(address).saturating_sub(4))
            .min(u64::from(u32::MAX - 3));
        bounds.push((readable, slot_stride));
        sources.push(Source {
            slot,
            shadow,
            address,
            size,
            stride: slot_stride,
            elements,
        });
    }
    let strides: Vec<(u32, u32)> = sources.iter().map(|s| (s.slot, s.stride)).collect();
    let Some((capacity, layouts)) = scratch_layout(&strides, SCRATCH_BYTES) else {
        return Err("rectangle vertices larger than the scratch");
    };
    let max_src_verts = max_source_vertices(&bounds);
    let Some(passes) = shaders() else {
        return Err("no rectangle expander");
    };
    let records_len = u64::from(max.max(1)) * 16;
    let Some(scratch) = take_scratch(list, held, COUNTER_BYTES + records_len) else {
        return Err("no scratch for rectangles");
    };
    // The element tables, written now; the GPU reads them when it runs.
    let mut tables = Vec::with_capacity(sources.len());
    for s in &sources {
        let bytes = (s.elements.len().max(1) * size_of::<Element>()) as u64;
        let Some((gpu, data)) = list.scratch(bytes) else {
            return Err("no upload memory for rectangle elements");
        };
        // SAFETY: fresh upload memory of the list, `bytes` long.
        unsafe {
            std::ptr::copy_nonoverlapping(
                s.elements.as_ptr().cast::<u8>(),
                data,
                s.elements.len() * size_of::<Element>(),
            );
        }
        tables.push(gpu);
    }
    let l2 = list
        .list
        .cast::<ID3D12GraphicsCommandList2>()
        .map_err(|_| "no immediate writes on the host list")?;

    // The title's buffers, as the title left them at this place.
    args.refresh(list, arguments, u64::from(stride) * u64::from(max.max(1)));
    if let Some((s, at)) = count {
        s.refresh(list, at, 4);
    }
    for s in &sources {
        s.shadow.refresh(list, s.address, u64::from(s.size));
    }
    let src_args = args.gpu(arguments);
    if !crate::indirect::clamp_uav(list, src_args, (0, stride, max)) {
        return Err("not bounded (no clamp)");
    }

    // SAFETY: live buffers.
    let (args_gpu, vertices_gpu) = unsafe {
        (
            scratch.args.GetGPUVirtualAddress(),
            scratch.vertices.GetGPUVirtualAddress(),
        )
    };
    let (counter, records) = (args_gpu, args_gpu + COUNTER_BYTES);
    let count_gpu = count.map_or(src_args, |(s, at)| s.gpu(at));
    let rewrite = [
        max,
        stride,
        u32::from(count.is_some()),
        max_src_verts,
        capacity,
        u32::from(!sources.is_empty()),
        0,
        0,
    ];
    let mut barriers = Vec::new();
    let copy_dest = D3D12_RESOURCE_STATE_COPY_DEST;
    let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
    to_state(
        &mut barriers,
        &scratch.args,
        &mut scratch.args_state,
        copy_dest,
    );
    to_state(
        &mut barriers,
        &scratch.vertices,
        &mut scratch.vertices_state,
        uav,
    );
    let l = &list.list;
    // SAFETY: an open list; our passes and buffers the list keeps (the
    // title's host copies through their refresh).
    unsafe {
        if !barriers.is_empty() {
            l.ResourceBarrier(&barriers);
        }
        // This draw's allocations start from zero.
        l2.WriteBufferImmediate(
            1,
            &D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
                Dest: counter,
                Value: 0,
            },
            None,
        );
        barriers.clear();
        to_state(&mut barriers, &scratch.args, &mut scratch.args_state, uav);
        l.ResourceBarrier(&barriers);
        l.SetComputeRootSignature(&passes.rewrite.root);
        l.SetPipelineState(&passes.rewrite.pso);
        l.SetComputeRoot32BitConstants(0, 8, rewrite.as_ptr().cast(), 0);
        l.SetComputeRootUnorderedAccessView(1, src_args);
        l.SetComputeRootUnorderedAccessView(2, count_gpu);
        l.SetComputeRootUnorderedAccessView(3, counter);
        l.SetComputeRootUnorderedAccessView(4, records);
        l.Dispatch(max.div_ceil(64), 1, 1);
    }
    let mut views = Vec::with_capacity(sources.len());
    if !sources.is_empty() {
        let (grid_x, grid_y) = dispatch_grid(max);
        // SAFETY: as above.
        unsafe {
            // The rewritten records, for the expansion.
            l.ResourceBarrier(&[uav_barrier()]);
            l.SetComputeRootSignature(&passes.expand.root);
            l.SetPipelineState(&passes.expand.pso);
            l.SetComputeRootUnorderedAccessView(2, src_args);
            l.SetComputeRootUnorderedAccessView(3, records);
            for ((s, layout), table) in sources.iter().zip(&layouts).zip(tables) {
                let src = s.shadow.gpu(s.address);
                let to = vertices_gpu + layout.dst_offset;
                let expand = [
                    max,
                    stride,
                    grid_x,
                    (src & 3) as u32,
                    s.stride,
                    layout.dst_stride,
                    s.elements.len() as u32,
                    0,
                ];
                l.SetComputeRoot32BitConstants(0, 8, expand.as_ptr().cast(), 0);
                l.SetComputeRootShaderResourceView(1, table);
                l.SetComputeRootUnorderedAccessView(4, src & !3);
                l.SetComputeRootUnorderedAccessView(5, to);
                l.Dispatch(grid_x, grid_y, 1);
                views.push((
                    layout.slot,
                    D3D12_VERTEX_BUFFER_VIEW {
                        BufferLocation: to,
                        SizeInBytes: layout.dst_size,
                        StrideInBytes: layout.dst_stride,
                    },
                ));
            }
        }
    }
    barriers.clear();
    let (arguments_state, vertex_state) = (
        D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
        D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER,
    );
    to_state(
        &mut barriers,
        &scratch.args,
        &mut scratch.args_state,
        arguments_state,
    );
    if !sources.is_empty() {
        to_state(
            &mut barriers,
            &scratch.vertices,
            &mut scratch.vertices_state,
            vertex_state,
        );
    }
    // SAFETY: an open list and buffers it keeps.
    unsafe { l.ResourceBarrier(&barriers) };
    Ok(Prepared {
        args: scratch.args.clone(),
        offset: COUNTER_BYTES,
        views,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(slot: u32, offset: u32, format: u32, per_instance: bool) -> Input {
        Input {
            slot,
            offset,
            format,
            per_instance,
        }
    }

    #[test]
    fn slot_elements_filter_and_stop_at_the_stride() {
        let inputs = [
            input(0, 0, 16, false),  // R32G32_FLOAT, 8 bytes
            input(0, 8, 28, false),  // R8G8B8A8_UNORM, 4 bytes
            input(0, 4, 2, false),   // R32G32B32A32_FLOAT, 2 components inside 12 bytes
            input(0, 12, 28, false), // past the stride: none
            input(0, 0, 42, false),  // R32_UINT, unknown: none
            input(1, 0, 16, true),   // per instance: ignored
            input(2, 0, 42, false),  // unknown only: per vertex, no element
        ];
        let (per_vertex, elements) = slot_elements(&inputs, 0, 12);
        assert!(per_vertex);
        let element = |offset, kind: primitive::Kind, n: u32| Element {
            offset,
            packed: kind.code() | n << 8,
        };
        assert_eq!(
            elements,
            [
                element(0, primitive::Kind::F32, 2),
                element(8, primitive::Kind::Unorm8, 4),
                element(4, primitive::Kind::F32, 2),
            ]
        );
        assert_eq!(slot_elements(&inputs, 1, 16), (false, Vec::new()));
        assert_eq!(slot_elements(&inputs, 2, 4), (true, Vec::new()));
    }

    #[test]
    fn scratch_layouts_align_and_fit() {
        assert_eq!(scratch_layout(&[], 1024), Some((0, Vec::new())));
        assert_eq!(scratch_layout(&[(0, 0)], 1024), None);
        assert_eq!(scratch_layout(&[(0, 16)], 64), None, "6 * 16 > 64");

        // Strides 6 (padded to 8) and 12.
        let (cap, layouts) = scratch_layout(&[(0, 6), (3, 12)], 1024).unwrap();
        assert_eq!(cap % 6, 0);
        assert!(cap >= 6);
        assert_eq!(layouts.len(), 2);
        assert_eq!((layouts[0].slot, layouts[0].dst_offset), (0, 0));
        assert_eq!(layouts[0].dst_stride, 8);
        assert_eq!(layouts[0].dst_size, cap * 8);
        assert_eq!(layouts[1].slot, 3);
        assert_eq!(layouts[1].dst_offset % 16, 0);
        assert!(layouts[1].dst_offset >= u64::from(layouts[0].dst_size));
        assert_eq!(layouts[1].dst_stride, 12);
        assert_eq!(layouts[1].dst_size, cap * 12);
        assert!(layouts[1].dst_offset + u64::from(layouts[1].dst_size) <= 1024);

        // An exact fit, with no padding needed: 6 vertices of 8 bytes, then
        // 6 of 12 (48 is 16-byte aligned).
        let (cap, layouts) = scratch_layout(&[(0, 8), (1, 12)], 120).unwrap();
        assert_eq!((cap, layouts[1].dst_offset), (6, 48));
        // Padding counted where needed: 24 bytes, aligned to 32, 24 more.
        assert_eq!(scratch_layout(&[(0, 4), (1, 4)], 55), None);
        let (cap, layouts) = scratch_layout(&[(0, 4), (1, 4)], 56).unwrap();
        assert_eq!((cap, layouts[1].dst_offset), (6, 32));
    }

    #[test]
    fn the_source_bound_is_the_smallest_slots() {
        assert_eq!(max_source_vertices(&[]), u32::MAX);
        assert_eq!(max_source_vertices(&[(120, 12), (80, 16)]), 5);
        assert_eq!(max_source_vertices(&[(100, 0), (80, 16)]), 0);
    }

    #[test]
    fn dispatch_grids_stay_within_the_limit() {
        assert_eq!(dispatch_grid(0), (0, 0));
        assert_eq!(dispatch_grid(1), (1, 1));
        assert_eq!(dispatch_grid(65535), (65535, 1));
        let (x, y) = dispatch_grid(65536);
        assert!(x <= 65535 && y <= 65535);
        assert!(u64::from(x) * u64::from(y) >= 65536);
    }

    /// Both passes compile with the system's compiler and the host takes
    /// them.
    #[test]
    fn the_rectangle_expander_builds() {
        let test = "rectangles::tests::the_rectangle_expander_builds";
        if crate::testgpu::device(test).is_none() {
            return;
        }
        assert!(shaders().is_some());
    }

    /// One vertex of the byte-for-byte test (24 bytes): R32G32_FLOAT at 0,
    /// R16G16_FLOAT at 8, R16G16_UNORM at 12, R8G8B8A8_UNORM at 16, a word
    /// of an unknown format at 20 (keeps v0's bytes); an element at 24 is
    /// past the stride.
    fn vertex(x: f32, y: f32, u: u16, v: u16, n: [u16; 2], rgba: [u8; 4], tag: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(STRIDE);
        out.extend(x.to_le_bytes());
        out.extend(y.to_le_bytes());
        out.extend(u.to_le_bytes());
        out.extend(v.to_le_bytes());
        out.extend(n[0].to_le_bytes());
        out.extend(n[1].to_le_bytes());
        out.extend(rgba);
        out.extend(tag.to_le_bytes());
        out
    }
    const STRIDE: usize = 24;

    /// Three rectangles: a full-screen one; one with clamped colours,
    /// negative halves and a half subnormal fourth corner (2^-16); one
    /// whose u fourth corner is 1 + 2^-11, halfway between two halves (the
    /// even 0x3c00).
    fn rectangle_vertices() -> Vec<u8> {
        [
            vertex(-1.0, 1.0, 0, 0, [0, 10000], [0, 50, 200, 255], 0x1122_3344),
            vertex(-1.0, -1.0, 0, 0x3c00, [10000, 30000], [100; 4], 0x5566_7788),
            vertex(
                1.0,
                -1.0,
                0x3c00,
                0x3c00,
                [40000, 50000],
                [150, 200, 50, 255],
                0x99aa_bbcc,
            ),
            vertex(
                0.25,
                0.5,
                0xb800,
                0x0400,
                [50000, 1000],
                [220, 10, 128, 64],
                0xdead_beef,
            ),
            vertex(
                0.25,
                -0.5,
                0xbc00,
                0x0600,
                [10000, 20000],
                [50, 200, 250, 200],
                0xcafe_babe,
            ),
            vertex(
                2.5,
                -0.5,
                0xc000,
                0x0300,
                [60000, 5000],
                [200, 20, 10, 100],
                0x0102_0304,
            ),
            vertex(
                10.0,
                20.0,
                0x3c00,
                0x4200,
                [12345, 54321],
                [10, 20, 30, 40],
                0xaabb_ccdd,
            ),
            vertex(
                10.0,
                30.0,
                0,
                0x4400,
                [22222, 11111],
                [50, 60, 70, 80],
                0x1234_5678,
            ),
            vertex(
                40.0,
                30.0,
                0x1000,
                0x4000,
                [33333, 22222],
                [90, 80, 70, 60],
                0x8765_4321,
            ),
        ]
        .concat()
    }

    fn rectangle_inputs() -> Vec<Input> {
        vec![
            input(0, 0, 16, false),
            input(0, 8, 34, false),
            input(0, 12, 35, false),
            input(0, 16, 28, false),
            input(0, 20, 42, false),
            input(0, 24, 28, false),
        ]
    }

    /// The whole expansion as a draw records it, on title buffers: clamp,
    /// counter, rewrite and expansion, twice in one list (the second draw's
    /// allocations start from zero again), compared with
    /// [`primitive::expand_rects`] byte for byte. Records: two rectangles
    /// from vertex 0; one from vertex 6 with 3 instances from 7; one past
    /// the vertex buffer; one without instances; one past the title's
    /// count (4).
    #[test]
    fn expansion_on_the_gpu_matches_the_cpus() {
        use crate::testgpu::{TitleBuffer, TitleList};
        let test = "rectangles::tests::expansion_on_the_gpu_matches_the_cpus";
        let Some(host) = crate::testgpu::device(test) else {
            return;
        };
        let source = rectangle_vertices();
        let records: [[u32; 4]; 5] = [
            [6, 1, 0, 0],
            [3, 3, 6, 7],
            [3, 1, 8, 0],
            [3, 0, 0, 0],
            [3, 1, 0, 0],
        ];
        let args = TitleBuffer::new(256);
        let count = TitleBuffer::new(256);
        let vertices = TitleBuffer::new(4096);
        let words: Vec<u32> = records.iter().flatten().copied().collect();
        args.set_words(|i| words.get(i).copied().unwrap_or(0));
        count.set_words(|i| if i == 0 { 4 } else { 0 });
        vertices.set_words(|i| {
            source
                .get(i * 4..i * 4 + 4)
                .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
        });
        let (args_copy, count_copy) = (args.shadow(), count.shadow());
        let _vertex_copy = vertices.shadow();
        let inputs = rectangle_inputs();
        let mut bound = [(0u64, 0u32, 0u32); 32];
        bound[0] = (vertices.address(), source.len() as u32, STRIDE as u32);

        let list = TitleList::new();
        let l = list.host();
        let mut held = None;
        let draw = |held: &mut Option<Scratch>| {
            expand(
                l,
                held,
                &inputs,
                &bound,
                (&args_copy, args.address(), 16, 5),
                Some((&count_copy, count.address())),
            )
            .expect("expanded")
        };
        let first = draw(&mut held);
        let second = draw(&mut held);
        assert_eq!(first.offset, COUNTER_BYTES);
        assert_eq!(first.views.len(), 1);
        assert_eq!(first.views[0].0, 0);
        assert_eq!(first.views[0].1.StrideInBytes, STRIDE as u32);
        let scratch = held.as_mut().expect("the list's scratch");
        // Read back what the second draw left.
        let (out_args, out_args_data) = host.shared_buffer(256).expect("readback");
        let (out_vertices, out_vertices_data) = host.shared_buffer(4096).expect("readback");
        let mut barriers = Vec::new();
        let source_state = D3D12_RESOURCE_STATE_COPY_SOURCE;
        to_state(
            &mut barriers,
            &scratch.args,
            &mut scratch.args_state,
            source_state,
        );
        to_state(
            &mut barriers,
            &scratch.vertices,
            &mut scratch.vertices_state,
            source_state,
        );
        // SAFETY: an open list; live buffers within their sizes.
        unsafe {
            l.list.ResourceBarrier(&barriers);
            l.list
                .CopyBufferRegion(&out_args, 0, &scratch.args, 0, COUNTER_BYTES + 5 * 16);
            l.list
                .CopyBufferRegion(&out_vertices, 0, &scratch.vertices, 0, 18 * STRIDE as u64);
        }
        list.run();
        // SAFETY: the list has run; both are mapped buffers of these sizes.
        let (counter, rewritten, expanded) = unsafe {
            let words = std::slice::from_raw_parts(out_args_data.cast::<u32>(), 4 + 5 * 4);
            let rewritten: Vec<[u32; 4]> = words[4..]
                .chunks(4)
                .map(|c| c.try_into().unwrap())
                .collect();
            (
                words[0],
                rewritten,
                std::slice::from_raw_parts(out_vertices_data, 18 * STRIDE).to_vec(),
            )
        };
        assert_eq!(
            counter, 18,
            "12 + 6 vertices, from zero for the second draw"
        );
        assert_eq!(rewritten[0][0], 12);
        assert_eq!(rewritten[0][1], 1);
        assert_eq!(rewritten[0][3], 0);
        assert_eq!(rewritten[1][0], 6);
        assert_eq!(rewritten[1][1], 3);
        assert_eq!(rewritten[1][3], 7);
        assert_eq!(rewritten[2][0], 0, "past the vertex buffer");
        assert_eq!(rewritten[3][0], 0, "no instance");
        assert_eq!(rewritten[4], [0; 4], "past the count");
        let elements: Vec<(u32, u32)> = inputs.iter().map(|i| (i.offset, i.format)).collect();
        for (record, from, rects) in [(0, 0, 2), (1, 6, 1)] {
            let mut expected = vec![0u8; rects * 6 * STRIDE];
            let src = &source[from * STRIDE..(from + rects * 3) * STRIDE];
            primitive::expand_rects(src, &mut expected, STRIDE, &elements);
            let at = rewritten[record][2] as usize * STRIDE;
            assert_eq!(
                &expanded[at..at + rects * 6 * STRIDE],
                &expected[..],
                "record {record}"
            );
        }
        drop(second);
        if let Some(s) = held.take() {
            give_back(s);
        }
    }
}
