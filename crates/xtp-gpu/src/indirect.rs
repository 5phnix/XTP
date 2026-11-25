//! Indirect execution's arguments, bounded before the GPU reads them. The
//! title's own compute passes write most of them; a pass we drop or
//! translate wrongly leaves whatever the memory held, and a draw of four
//! billion vertices or a dispatch past the host's limits runs until the
//! host resets the display driver. So every record is clamped on the GPU,
//! just before its execution, by a compute shader of ours (HLSL compiled by
//! the system's compiler; a 5.1 shader with its root signature inside).
//!
//! Each list also has a budget (2^27 vertex instances or threads, groups
//! counted as 64 threads, mesh groups as 128): records past it run empty,
//! so a list of thousands of garbage records cannot run for seconds
//! either. One group charges it in record order (VIctory #1510): the first
//! record that does not fit, and every record after it in the list, run
//! empty. A word that every thread adds to wraps once enough rejected
//! records have charged it, and lets work in again.
//!
//! shortcut: one group walks every record, 64 at a time, so the clamp's
//! time grows with the records a title passes (43 ms for 2^20 records on
//! an RTX 4080, by #1510's author).
//!
//! Indirect rectangle-list draws follow the clamp with an expansion on the
//! GPU ([`crate::rectangles`]).
//!
//! Idea from VIctory #1820 (report by Exopus-0x65f7ad0).
//! Le GPU produit des commandes hôtes (base XYZ, dimensions XYZ) dans
//! l’ordre du titre. Le budget porte sur le dispatch entier.
//! Avec XTP_DISPATCH_COUNTS=1, chaque bilan de trame regroupe par shader les
//! dimensions originales, les limites atteintes et les rejets de budget.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

use crate::host::{HostList, host, transition};

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), UAV(u0), UAV(u1), UAV(u2), UAV(u3), UAV(u4)"
cbuffer Record : register(b0) { uint kind; uint stride; uint records; uint flags; };
RWByteAddressBuffer args : register(u0);
RWByteAddressBuffer budget : register(u1);
RWByteAddressBuffer pieces : register(u2);
RWByteAddressBuffer countBuffer : register(u3);
RWByteAddressBuffer stats : register(u4);
static const uint BUDGET = 1u << 27;
static const uint LIMIT = 65535;
groupshared uint costs[64];
groupshared uint sizes[64];
groupshared uint starts[64];
groupshared uint metrics[16];

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint lane : SV_GroupIndex) {
    uint active = records;
    if (flags & 1) active = min(active, countBuffer.Load(0));
    bool measure = (flags & 2) != 0;
    if (measure && lane < 16) metrics[lane] = 0;
    GroupMemoryBarrierWithGroupSync();
    uint left = 0, written = 0;
    if (lane == 0) left = BUDGET - min(budget.Load(0), BUDGET);
    for (uint tile = 0; tile < records; tile += 64) {
        uint n = min(records - tile, 64u);
        uint at = (tile + lane) * stride;
        uint cost = 0, size = 0;
        uint3 g = 0;
        if (tile + lane < active) {
            if (kind <= 1) {
                uint2 raw = args.Load2(at);
                uint vertices = min(raw.x, 1u << 24);
                uint instances = min(raw.y, (1u << 26) / max(vertices, 1u));
                if (measure) {
                    InterlockedMax(metrics[8], raw.x);
                    InterlockedMax(metrics[9], raw.y);
                    if (vertices != raw.x) InterlockedAdd(metrics[10], 1);
                    if (instances != raw.y) InterlockedAdd(metrics[11], 1);
                    if (any(raw != uint2(vertices, instances))) InterlockedAdd(metrics[4], 1);
                }
                args.Store2(at, uint2(vertices, instances));
                cost = vertices * instances;
            } else {
                g = args.Load3(at);
                if (measure) {
                    InterlockedMax(metrics[0], g.x);
                    InterlockedMax(metrics[1], g.y);
                    InterlockedMax(metrics[2], g.z);
                    if (any(g > LIMIT)) InterlockedAdd(metrics[3], 1);
                }
                if (kind == 10 && measure && any(g == 0)) InterlockedAdd(metrics[14], 1);
                if (all(g != 0)) {
                    uint weight = kind == 2 ? 64 : 128;
                    if (measure && g.x > (1u << 20) / g.y / g.z) InterlockedAdd(metrics[12], 1);
                    // Division avant multiplication, même pour trois UINT_MAX.
                    if (g.x > BUDGET / weight / g.y / g.z) {
                        if (measure) InterlockedAdd(metrics[5], 1);
                    } else {
                        cost = g.x * g.y * g.z * weight;
                        uint3 tiles = (g - 1) / LIMIT + 1;
                        size = tiles.x * tiles.y * tiles.z;
                    }
                }
            }
        }
        costs[lane] = cost;
        sizes[lane] = size;
        GroupMemoryBarrierWithGroupSync();
        if (lane == 0) {
            for (uint i = 0; i < n; i++) {
                starts[i] = written;
                if (costs[i] > left) {
                    left = 0;
                    costs[i] = 0xffffffff;
                    if (measure) InterlockedAdd(metrics[6], 1);
                } else {
                    left -= costs[i];
                    written += sizes[i];
                }
            }
        }
        GroupMemoryBarrierWithGroupSync();
        if (costs[lane] == 0xffffffff) {
            if (kind <= 1) args.Store(at, 0);
        } else if (size != 0) {
            // Deux axes > LIMIT dépasseraient déjà le budget de toute la liste.
            uint3 step = uint3(g.x > LIMIT, g.y > LIMIT, g.z > LIMIT) * LIMIT;
            for (uint j = 0; j < size; j++) {
                uint3 base = step * j;
                uint dst = 4 + 24 * (starts[lane] + j);
                pieces.Store3(dst, base);
                pieces.Store3(dst + 12, min(g - base, LIMIT));
            }
        }
        GroupMemoryBarrierWithGroupSync();
    }
    if (lane == 0) {
        budget.Store(0, BUDGET - left);
        if (kind > 1) pieces.Store(0, written);
        if (measure) {
            stats.Store4(0, uint4(metrics[0], metrics[1], metrics[2], metrics[3]));
            stats.Store4(16, uint4(metrics[4], metrics[5], metrics[6], active));
            stats.Store4(32, uint4(metrics[8], metrics[9], metrics[10], metrics[11]));
            stats.Store3(48, uint3(metrics[12], BUDGET - left, metrics[14]));
        }
    }
}
"#;

/// Résultats écrits par le GPU avant et pendant le bridage.
#[cfg(feature = "diagnostics")]
#[derive(Clone)]
pub(crate) enum Sample {
    Indirect {
        _buffer: ID3D12Resource,
        data: usize,
        hash: String,
    },
    Direct {
        groups: [u32; 3],
        hash: String,
    },
}

#[cfg(feature = "diagnostics")]
fn measuring() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("XTP_DISPATCH_COUNTS").is_ok_and(|v| v == "1"))
}

#[cfg(feature = "diagnostics")]
pub(crate) fn note_direct(list: &HostList, pipeline: usize, groups: [u32; 3]) {
    if measuring() {
        let hash = crate::pipeline::dispatch_hash(pipeline as _).unwrap_or_default();
        list.dispatch_sample(Sample::Direct { groups, hash });
    }
}

#[cfg(feature = "diagnostics")]
#[derive(Debug, Default)]
struct Counts {
    max: [u32; 3],
    oversized: u64,
    clamped: u64,
    zeroed: u64,
    past_budget: u64,
    records: u64,
    direct: u64,
    max_vertices: u32,
    max_instances: u32,
    vertex_limit: u64,
    instance_limit: u64,
    over_million_groups: u64,
    max_list_cost: u32,
    empty_mesh: u64,
}

#[cfg(feature = "diagnostics")]
static COUNTS: std::sync::Mutex<std::collections::BTreeMap<String, Counts>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(feature = "diagnostics")]
impl Sample {
    pub(crate) fn report(&self) {
        let mut counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
        let (data, hash) = match self {
            Self::Direct { groups, hash } => {
                let c = counts.entry(hash.clone()).or_default();
                c.direct += 1;
                c.oversized += u64::from(groups.iter().any(|&n| n > LIMIT));
                for (high, n) in c.max.iter_mut().zip(groups) {
                    *high = (*high).max(*n);
                }
                return;
            }
            Self::Indirect { data, hash, .. } => (*data, hash),
        };
        // SAFETY: le tampon mappé est conservé par l’enregistrement soumis.
        let read = |i| unsafe { (data as *const u32).add(i).read_volatile() };
        let c = counts.entry(hash.clone()).or_default();
        for (i, v) in c.max.iter_mut().enumerate() {
            *v = (*v).max(read(i));
        }
        c.oversized += u64::from(read(3));
        c.clamped += u64::from(read(4));
        c.zeroed += u64::from(read(5));
        c.past_budget += u64::from(read(6));
        c.records += u64::from(read(7));
        c.max_vertices = c.max_vertices.max(read(8));
        c.max_instances = c.max_instances.max(read(9));
        c.vertex_limit += u64::from(read(10));
        c.instance_limit += u64::from(read(11));
        c.over_million_groups += u64::from(read(12));
        c.max_list_cost = c.max_list_cost.max(read(13));
        c.empty_mesh += u64::from(read(14));
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) fn log_counts() {
    let counts = std::mem::take(&mut *COUNTS.lock().unwrap_or_else(|e| e.into_inner()));
    for (hash, counts) in counts {
        tracing::info!(%hash, ?counts, "dispatch and draw counts per shader");
    }
}

/// Les compteurs de diagnostic occupent 64 octets par exécution dans une page de la liste.
#[derive(Default)]
pub(crate) struct Samples {
    #[cfg(feature = "diagnostics")]
    page: Option<(ID3D12Resource, usize, usize)>,
}

impl Samples {
    pub(crate) fn next(&mut self, list: &HostList, pipeline: usize) -> Option<u64> {
        #[cfg(feature = "diagnostics")]
        {
            if measuring() {
                const PAGE: usize = 65536;
                if self.page.as_ref().is_none_or(|p| p.2 == PAGE) {
                    let (buffer, data) = host()?.shared_buffer(PAGE as u64)?;
                    self.page = Some((buffer, data as usize, 0));
                }
                let (buffer, data, offset) = self.page.as_mut()?;
                let hash = crate::pipeline::dispatch_hash(pipeline as _).unwrap_or_default();
                list.dispatch_sample(Sample::Indirect {
                    _buffer: buffer.clone(),
                    data: *data + *offset,
                    hash,
                });
                // SAFETY: l’enregistrement soumis conserve la page et chaque plage est distincte.
                let gpu = unsafe { buffer.GetGPUVirtualAddress() } + *offset as u64;
                *offset += 64;
                return Some(gpu);
            }
        }
        let _ = (list, pipeline);
        None
    }
}

struct Clamp {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Clamp {}
// SAFETY: as above.
unsafe impl Sync for Clamp {}

fn clamp() -> Option<&'static Clamp> {
    static CLAMP: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    CLAMP
        .get_or_init(|| match make() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no argument clamp: indirect execution dropped");
                None
            }
        })
        .as_ref()
}

fn make() -> windows::core::Result<Clamp> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("clamp"),
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
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!("argument clamp: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes; the root signature is inside the shader.
    unsafe {
        let bytes =
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Clamp { root, pso })
    }
}

const LIMIT: u32 = 65535;
const RECORD_BYTES: u32 = 24;
// Chaque morceau supplémentaire consomme au moins LIMIT groupes du budget.
const MAX_GROUPS: u32 = (1 << 27) / 64;
const EXTRA: u32 = MAX_GROUPS / LIMIT;

/// Mémoire réutilisée par les dispatches d’une même liste.
pub(crate) struct Expanded {
    pub(crate) buffer: ID3D12Resource,
    pub(crate) capacity: u32,
}

impl Expanded {
    pub(crate) fn reserve<'a>(
        held: &'a mut Option<Self>,
        list: &HostList,
        records: u32,
    ) -> Option<&'a Self> {
        // Une commande produite contient au moins un groupe accepté.
        let needed = records.saturating_add(EXTRA).min(MAX_GROUPS);
        if held.as_ref().is_none_or(|s| s.capacity < needed) {
            // 4 octets pour le compteur et 12 de marge pour la signature sans base.
            let bytes = needed.checked_mul(RECORD_BYTES)?.checked_add(16)?;
            let (buffer, _) = host()?.shared_buffer(u64::from(bytes))?;
            list.keep(&buffer);
            *held = Some(Self {
                buffer,
                capacity: needed,
            });
        }
        held.as_ref()
    }
}

/// Balaye les seules commandes actives, en conservant le budget de la liste.
/// Les dessins sont bridés en place, les dispatches écrits dans « expanded ».
pub(crate) fn rewrite(
    list: &HostList,
    gpu: u64,
    (kind, stride, records): (u32, u32, u32),
    count: Option<u64>,
    expanded: Option<&Expanded>,
    stats: Option<u64>,
) -> bool {
    let (Some(c), Some(budget)) = (clamp(), list.budget()) else {
        return false;
    };
    let min_stride = match kind {
        0 => 16,
        1 => 20,
        2 | 10 if expanded.is_some() => 12,
        _ => return false,
    };
    if stride < min_stride
        || !gpu.is_multiple_of(4)
        || !stride.is_multiple_of(4)
        || stride.checked_mul(records).is_none()
        || count.is_some_and(|v| !v.is_multiple_of(4))
    {
        return false;
    }
    let flags = u32::from(count.is_some()) | (u32::from(stats.is_some()) << 1);
    let constants = [kind, stride, records, flags];
    let l = &list.list;
    // SAFETY: chaque vue utilisée désigne un tampon gardé par la liste.
    unsafe {
        l.ResourceBarrier(&[uav_barrier()]);
        l.SetComputeRootSignature(&c.root);
        l.SetPipelineState(&c.pso);
        l.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
        l.SetComputeRootUnorderedAccessView(1, gpu);
        l.SetComputeRootUnorderedAccessView(2, budget);
        l.SetComputeRootUnorderedAccessView(
            3,
            expanded.map_or(budget, |s| s.buffer.GetGPUVirtualAddress()),
        );
        l.SetComputeRootUnorderedAccessView(4, count.unwrap_or(budget));
        l.SetComputeRootUnorderedAccessView(5, stats.unwrap_or(budget));
        l.Dispatch(1, 1, 1);
        l.ResourceBarrier(&[uav_barrier()]);
    }
    true
}

pub(crate) fn ready(list: &HostList, buffer: &ID3D12Resource) {
    // SAFETY: la liste est ouverte et le tampon a été préparé en UAV.
    unsafe {
        list.list.ResourceBarrier(&[transition(
            buffer,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
        )])
    }
}

/// La signature des morceaux installe trois constantes avant la commande.
/// Pour un programme sans lecture du groupe, on saute ces douze octets.
pub(crate) fn signature(
    kind: u32,
    root: &ID3D12RootSignature,
    base: Option<(u32, u32)>,
) -> Option<ID3D12CommandSignature> {
    let mut args = [D3D12_INDIRECT_ARGUMENT_DESC::default(); 2];
    if let Some((param, offset)) = base {
        args[0].Type = D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT;
        args[0].Anonymous.Constant = D3D12_INDIRECT_ARGUMENT_DESC_0_1 {
            RootParameterIndex: param,
            DestOffsetIn32BitValues: offset,
            Num32BitValuesToSet: 3,
        };
    }
    args[1].Type = D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32);
    let args = &args[usize::from(base.is_none())..];
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: RECORD_BYTES,
        NumArgumentDescs: args.len() as u32,
        pArgumentDescs: args.as_ptr(),
        NodeMask: 0,
    };
    let mut signature = None;
    // SAFETY: les descriptions et la racine restent valables pendant la création.
    unsafe {
        host()?
            .device
            .CreateCommandSignature(&desc, base.map(|_| root), &mut signature)
    }
    .ok()?;
    signature
}

pub(crate) fn direct(
    list: &HostList,
    groups: [u32; 3],
    base: Option<(u32, u32)>,
    mesh: bool,
    mut run: impl FnMut([u32; 3]),
) {
    let mut extent = groups.map(|n| n.min(LIMIT));
    if groups.contains(&0) {
        run(extent);
        return;
    }
    if mesh {
        // DispatchMesh limite également le produit XYZ d’une commande.
        const MAX_MESH_GROUPS: u32 = (1 << 22) - 1;
        extent[1] = extent[1].min(MAX_MESH_GROUPS / extent[0]);
        extent[2] = extent[2].min(MAX_MESH_GROUPS / extent[0] / extent[1]);
    }
    if groups == extent {
        run(groups);
        return;
    }
    for z in (0..groups[2]).step_by(extent[2] as usize) {
        for y in (0..groups[1]).step_by(extent[1] as usize) {
            for x in (0..groups[0]).step_by(extent[0] as usize) {
                let origin = [x, y, z];
                if let Some((param, offset)) = base {
                    // SAFETY: trois dwords réservés dans la racine du programme.
                    unsafe {
                        if mesh {
                            list.list.SetGraphicsRoot32BitConstants(
                                param,
                                3,
                                origin.as_ptr().cast(),
                                offset,
                            );
                        } else {
                            list.list.SetComputeRoot32BitConstants(
                                param,
                                3,
                                origin.as_ptr().cast(),
                                offset,
                            );
                        }
                    }
                }
                run(std::array::from_fn(|i| {
                    (groups[i] - origin[i]).min(extent[i])
                }));
            }
        }
    }
}

/// After the execution: the buffer back to unordered access, where the
/// title's passes keep using it.
pub(crate) fn done(list: &HostList, buffer: &ID3D12Resource) {
    // SAFETY: an open list and a live buffer.
    unsafe {
        list.list.ResourceBarrier(&[transition(
            buffer,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )])
    };
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::testgpu::device;

    /// The clamp compiles with the system's compiler and the host takes it.
    #[test]
    fn argument_clamp_builds() {
        if super::host().is_none() {
            return; // no host GPU here
        }
        assert!(super::clamp().is_some());
    }

    /// Clamps `records` records of `kind`, filled by repeating `words`,
    /// `calls` times in one list: each record's first word after (zero when
    /// it ran empty). One more record follows them and must stay as
    /// written. `None` without a host GPU (`test`'s skip is reported).
    fn clamped(test: &str, kind: u32, words: &[u32], records: u32, calls: u32) -> Option<Vec<u32>> {
        let gpu = device(test)?;
        let host = gpu.host();
        // Words in a record of `kind`.
        let per: usize = match kind {
            0 => 4,
            1 => 5,
            _ => 3,
        };
        let spec = (kind, per as u32 * 4, records);
        let all = per * (records as usize + 1);
        let (buffer, data) = host.shared_buffer(all as u64 * 4).expect("a shared buffer");
        let data = data.cast::<u32>();
        let list = host.new_list().expect("a host list");
        // SAFETY: our mapped buffer, `all` words, written before the list
        // runs; a live buffer the test holds until the list has run.
        unsafe {
            for at in 0..all {
                data.add(at).write_volatile(words[at % words.len()]);
            }
            let gpu = buffer.GetGPUVirtualAddress();
            for _ in 0..calls {
                assert!(super::rewrite(&list, gpu, spec, None, None, None));
            }
        }
        list.close();
        host.execute(&[&list]);
        host.drain();
        // SAFETY: as above, read once the list has run.
        unsafe {
            for at in all - per..all {
                assert_eq!(
                    data.add(at).read_volatile(),
                    words[at % words.len()],
                    "the word {at} past the records"
                );
            }
            Some(
                (0..records as usize)
                    .map(|i| data.add(i * per).read_volatile())
                    .collect(),
            )
        }
    }

    /// Rejected records do not wrap the budget: of 128 garbage draws (2^26
    /// vertex instances each once clamped) two run, where a wrapping add let four.
    #[test]
    fn rejected_records_do_not_wrap_the_budget() {
        let test = "indirect::tests::rejected_records_do_not_wrap_the_budget";
        let Some(draws) = clamped(test, 0, &[u32::MAX], 128, 1) else {
            return;
        };
        assert_eq!(draws.iter().filter(|&&count| count != 0).count(), 2);
    }

    /// Chaque dispatch mesh de 70 000 groupes consomme deux commandes et son coût entier.
    /// Le compte GPU exclut la dernière ligne. Une dimension corrompue ne déborde pas.
    #[test]
    fn mesh_pieces_share_the_original_budget() {
        let test = "indirect::tests::mesh_pieces_share_the_original_budget";
        let Some(gpu) = crate::testgpu::device(test) else {
            return;
        };
        let host = gpu.host();
        let list = host.new_list().unwrap();
        let (args, data) = host.shared_buffer(16 * 18).unwrap();
        let (out, written) = host.shared_buffer(16 + 24 * 64).unwrap();
        let (stats, counts) = host.shared_buffer(64).unwrap();
        let scratch = super::Expanded {
            buffer: out,
            capacity: 64,
        };
        // SAFETY: les tampons mappés appartiennent au test jusqu’à drain().
        unsafe {
            let words = std::slice::from_raw_parts_mut(data.cast::<u32>(), 18 * 4);
            for record in words.as_chunks_mut::<4>().0 {
                record.copy_from_slice(&[70_000, 1, 1, 0]);
            }
            words[..3].fill(u32::MAX);
            words[3] = 17;
            // An active empty record and one beyond the count. Only the first is measured.
            words[4 * 16 + 1] = 0;
            words[4 * 17 + 1] = 0;
            let gpu = args.GetGPUVirtualAddress();
            assert!(super::rewrite(
                &list,
                gpu,
                (10, 16, 18),
                Some(gpu + 12),
                Some(&scratch),
                Some(stats.GetGPUVirtualAddress())
            ));
            list.close();
            host.execute(&[&list]);
            host.drain();
            let count = written.cast::<u32>().read_volatile();
            assert_eq!(count, 28);
            let pieces =
                std::slice::from_raw_parts(written.cast::<u32>().add(1), count as usize * 6);
            for &pair in pieces.as_chunks::<12>().0 {
                assert_eq!(pair, [0, 0, 0, 65535, 1, 1, 65535, 0, 0, 4465, 1, 1]);
            }
            let metrics = std::slice::from_raw_parts(counts.cast::<u32>(), 8);
            assert_eq!(metrics, [u32::MAX, u32::MAX, u32::MAX, 17, 0, 1, 1, 17]);
            assert_eq!(counts.cast::<u32>().add(14).read_volatile(), 1);
            assert_eq!(words[4], 70000);
        }
    }

    /// The budget goes to the first records, in order: of 200 draws of
    /// 3 * 2^18 vertices the first 170 run (it ends inside the third tile);
    /// small records that all fit all run.
    #[test]
    fn the_budget_goes_to_the_first_records() {
        let test = "indirect::tests::the_budget_goes_to_the_first_records";
        let Some(draws) = clamped(test, 0, &[3 << 18, 1, 0, 0], 200, 1) else {
            return;
        };
        assert!(draws[..170].iter().all(|&count| count != 0));
        assert!(draws[170..].iter().all(|&count| count == 0));
        let small = clamped(test, 0, &[1], 1 << 16, 1).expect("a host GPU");
        assert!(small.iter().all(|&count| count == 1));
    }

    /// A record that does not fit ends the budget: after two draws of
    /// 3 * 2^24 vertex instances a third takes what is left, and the small
    /// ones after it run empty.
    #[test]
    fn a_record_that_does_not_fit_ends_the_budget() {
        let test = "indirect::tests::a_record_that_does_not_fit_ends_the_budget";
        let mut words = [1 << 24, 3, 0, 0].repeat(3);
        words.extend([3, 1, 0, 0].repeat(6));
        let Some(draws) = clamped(test, 0, &words, 8, 1) else {
            return;
        };
        let ran: Vec<bool> = draws.iter().map(|&count| count != 0).collect();
        assert_eq!(ran, [true, true, false, false, false, false, false, false]);
    }

    /// The budget is the list's: a second execution of the same records
    /// finds it spent, and they all run empty.
    #[test]
    fn a_spent_budget_stays_spent() {
        let test = "indirect::tests::a_spent_budget_stays_spent";
        let Some(twice) = clamped(test, 1, &[u32::MAX], 64, 2) else {
            return;
        };
        assert!(twice.iter().all(|&count| count == 0));
    }
}
