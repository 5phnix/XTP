//! `amd_fidelityfx_loader_xbox.dll` and `amd_fidelityfx_upscaler_xbox.dll`:
//! AMD's FidelityFX upscaler (FSR4), which BTP 6 runs on Series S only.
//! Its calls are answered here, and none of AMD's code runs.
//!
//! The title imports four calls from the loader (`ffxCreateContext`,
//! `ffxDestroyContext`, `ffxQuery`, `ffxDispatch`), which loads the
//! upscaler; both DLLs export those and `ffxConfigure`. The descriptions
//! keep the PC FidelityFX API's layouts (`ffx_api.h`, `ffx_upscale.h`): a
//! 16-byte header (type, next) and the fields. What BTP 6 1.0.2.1 passes
//! (its calls at title+0x28ef2cc, +0x28ef30f, +0x28ef625, +0x28f03db,
//! +0x28f0651 and +0x28ef512):
//! - Create: the upscale description (0x10000: flags, the largest render
//!   and output sizes), chained to the console backend's (0x1000002, the
//!   device at +0x10) and a version's (0x1000b). The result is not read.
//! - Query 0x10004, the jitter phase count `(render width, display width,
//!   out)`, right after. The frame counter is taken modulo it at
//!   title+0x28f03e8 (a division by zero when it stays 0).
//! - Query 0x10005 each frame, the jitter offset `(index, phase count, out
//!   x, out y)`: the camera is moved by what it writes, into locals the
//!   title does not initialise.
//! - Query 0x10008, the upscaler's GPU memory `(out)`: the title adds what
//!   it writes, again into uninitialised locals, to a memory total.
//! - Dispatch 0x10001 each frame: `ffxDispatchDescUpscale` (432 bytes),
//!   with the title's command list and resources.
//!
//! Queries are answered as AMD's open FSR 2 and 3 code answers them. A
//! dispatch records a bilinear upscale of the colour input into the output
//! ([`xtp_gpu::list::upscale`]). Depth, motion vectors, exposure and the
//! masks are not read. `XTP_FSR4=1` leaves the title AMD's DLLs instead.
//!
//! Ported from Marvisak/VIctory#1284 (head f96ad82) by AwkGrep256: the
//! module standing in for both DLLs, the five exports, the description
//! layouts, and logging unknown description types once. #1284 passes the
//! calls on to AMD's PC loader; here they are answered without it.
#![allow(non_snake_case)]
// Title code passes pointers the real DLL would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::mem::offset_of;

use crate::{HleModule, hle_export};

/// `ffxReturnCode_t`.
type ReturnCode = u32;
const OK: ReturnCode = 0;
const ERROR: ReturnCode = 1;
const ERROR_UNKNOWN_DESCTYPE: ReturnCode = 2;
const ERROR_PARAMETER: ReturnCode = 6;

/// `ffxApiHeader`: a description's type and the next in its chain.
#[repr(C)]
pub struct Header {
    kind: u64,
    next: *const Header,
}

const CREATE_UPSCALE: u64 = 0x0001_0000;
const DISPATCH_UPSCALE: u64 = 0x0001_0001;
const QUERY_JITTER_PHASE_COUNT: u64 = 0x0001_0004;
const QUERY_JITTER_OFFSET: u64 = 0x0001_0005;
const QUERY_GPU_MEMORY: u64 = 0x0001_0008;
/// The console backend's description (the device at +0x10) and the
/// upscaler version's: chained to the title's create description.
const CONSOLE_BACKEND: u64 = 0x0100_0002;
const UPSCALE_VERSION: u64 = 0x0001_000b;

/// `ffxCreateContextDescUpscale`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CreateUpscale {
    kind: u64,
    next: *const Header,
    flags: u32,
    max_render: [u32; 2],
    max_upscale: [u32; 2],
    message: *const c_void,
}
const _: () = assert!(size_of::<CreateUpscale>() == 48);

/// `ffxQueryDescUpscaleGetJitterPhaseCount`.
#[repr(C)]
struct QueryPhaseCount {
    header: Header,
    render_width: u32,
    display_width: u32,
    out: *mut i32,
}
const _: () = assert!(offset_of!(QueryPhaseCount, out) == 0x18);

/// `ffxQueryDescUpscaleGetJitterOffset`.
#[repr(C)]
struct QueryJitterOffset {
    header: Header,
    index: i32,
    phase_count: i32,
    out_x: *mut f32,
    out_y: *mut f32,
}
const _: () = assert!(offset_of!(QueryJitterOffset, out_y) == 0x20);

/// `ffxQueryDescUpscaleGetGPUMemoryUsage`: the total and the aliasable
/// bytes (`FfxApiEffectMemoryUsage`).
#[repr(C)]
struct QueryGpuMemory {
    header: Header,
    out: *mut [u64; 2],
}

/// `FfxApiResource`: the resource (the title's `ID3D12Resource`), its
/// description, its state.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Resource {
    resource: *const c_void,
    description: [u32; 8],
    state: u32,
}
const _: () = assert!(size_of::<Resource>() == 48);

/// `ffxDispatchDescUpscale`; its two `bool`s as bytes (the title writes
/// them with byte stores).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DispatchUpscale {
    kind: u64,
    next: *const Header,
    command_list: *const c_void,
    /// Colour, depth, motion vectors, exposure, reactive, transparency and
    /// composition, output.
    resources: [Resource; 7],
    jitter: [f32; 2],
    motion_vector_scale: [f32; 2],
    render_size: [u32; 2],
    upscale_size: [u32; 2],
    sharpening: u8,
    sharpness: f32,
    frame_time_delta: f32,
    pre_exposure: f32,
    reset: u8,
    /// Near, far, vertical field of view, metres per view-space unit.
    camera: [f32; 4],
    flags: u32,
}
const _: () = assert!(size_of::<DispatchUpscale>() == 432);
// Where BTP 6 writes them (title+0x28f016e..+0x28f03cc).
const _: () = assert!(offset_of!(DispatchUpscale, command_list) == 0x10);
const _: () = assert!(offset_of!(DispatchUpscale, resources) == 0x18);
const _: () = assert!(offset_of!(DispatchUpscale, jitter) == 0x168);
const _: () = assert!(offset_of!(DispatchUpscale, render_size) == 0x178);
const _: () = assert!(offset_of!(DispatchUpscale, sharpening) == 0x188);
const _: () = assert!(offset_of!(DispatchUpscale, reset) == 0x198);
const _: () = assert!(offset_of!(DispatchUpscale, flags) == 0x1ac);
const COLOR: usize = 0;
const OUTPUT: usize = 6;

/// Whether this module stands in for AMD's DLLs: unless `XTP_FSR4` is
/// `1`, which leaves the title AMD's.
pub(crate) fn stands_in() -> bool {
    stands_in_for(std::env::var("XTP_FSR4").ok().as_deref())
}

fn stands_in_for(fsr4: Option<&str>) -> bool {
    fsr4 != Some("1")
}

/// FidelityFX's jitter phase count for a render width and a display width,
/// as AMD's open FSR 2 and 3 code computes it
/// (`ffxFsr3UpscalerGetJitterPhaseCount`): 8 × (display ÷ render)², cut to
/// an integer. `None` for a zero width.
fn phase_count(render_width: u32, display_width: u32) -> Option<i32> {
    const BASE: f32 = 8.0;
    if render_width == 0 || display_width == 0 {
        return None;
    }
    let ratio = display_width as f32 / render_width as f32;
    Some((BASE * ratio.powf(2.0)) as i32)
}

/// The Halton sequence's element `index` in `base`, as AMD's `halton`.
fn halton(index: i32, base: i32) -> f32 {
    let (mut f, mut result) = (1.0f32, 0.0f32);
    let mut i = index;
    while i > 0 {
        f /= base as f32;
        result += f * (i % base) as f32;
        i /= base;
    }
    result
}

/// FidelityFX's jitter offset of frame `index` in a sequence of `phases`,
/// in render pixels (`ffxFsr3UpscalerGetJitterOffset`): Halton (2, 3) of
/// `index % phases + 1`, centred on 0. `None` for no phases.
fn jitter_offset(index: i32, phases: i32) -> Option<(f32, f32)> {
    if phases <= 0 {
        return None;
    }
    let i = index % phases + 1;
    Some((halton(i, 2) - 0.5, halton(i, 3) - 0.5))
}

/// Logs the first time `call` sees description type `kind`.
fn note(call: &'static str, kind: u64) {
    static SEEN: std::sync::Mutex<Vec<(&str, u64)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&(call, kind)) {
        seen.push((call, kind));
        tracing::info!("ffx: {call} with description type {kind:#x}");
    }
}

/// What a context remembers: the largest sizes it was made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Context {
    max_render: [u32; 2],
    max_upscale: [u32; 2],
}

/// The contexts made and not yet destroyed (their addresses), so that a
/// stale or foreign one is never freed.
static CONTEXTS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// The upscale description in a create chain, read from the title.
///
/// # Safety
///
/// `h` is null or the start of a chain of valid descriptions.
unsafe fn upscale_in(mut h: *const Header) -> Option<CreateUpscale> {
    let mut upscale = None;
    while !h.is_null() {
        // SAFETY: the caller's chain.
        let kind = unsafe { (*h).kind };
        match kind {
            // SAFETY: a description of this type.
            CREATE_UPSCALE => upscale = Some(unsafe { h.cast::<CreateUpscale>().read() }),
            CONSOLE_BACKEND | UPSCALE_VERSION => {}
            _ => note("ffxCreateContext", kind),
        }
        // SAFETY: as above.
        h = unsafe { (*h).next };
    }
    upscale
}

pub extern "C" fn ffxCreateContext(
    context: *mut *mut c_void,
    desc: *const Header,
    _memory: *const c_void,
) -> ReturnCode {
    if context.is_null() {
        return ERROR_PARAMETER;
    }
    // SAFETY: the title's chain of descriptions (null is tolerated).
    let Some(upscale) = (unsafe { upscale_in(desc) }) else {
        tracing::warn!("ffx: a context without an upscale description refused");
        return ERROR_PARAMETER;
    };
    let made = Box::into_raw(Box::new(Context {
        max_render: upscale.max_render,
        max_upscale: upscale.max_upscale,
    }));
    CONTEXTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(made as usize);
    // SAFETY: checked non-null; the title's out pointer.
    unsafe { context.write(made.cast()) };
    tracing::info!(
        render = ?upscale.max_render,
        upscale = ?upscale.max_upscale,
        flags = format_args!("{:#x}", upscale.flags),
        "ffx: upscale context (bilinear, AMD's upscaler not loaded)"
    );
    OK
}

pub extern "C" fn ffxDestroyContext(
    context: *mut *mut c_void,
    _memory: *const c_void,
) -> ReturnCode {
    if context.is_null() {
        return ERROR_PARAMETER;
    }
    // SAFETY: the title's context pointer.
    let at = unsafe { *context } as usize;
    let mut contexts = CONTEXTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(i) = contexts.iter().position(|&c| c == at) else {
        return ERROR_PARAMETER;
    };
    contexts.swap_remove(i);
    drop(contexts);
    // SAFETY: made by `ffxCreateContext` and not freed since.
    drop(unsafe { Box::from_raw(at as *mut Context) });
    // SAFETY: as above.
    unsafe { context.write(std::ptr::null_mut()) };
    OK
}

/// Nothing to configure: a bilinear upscale has no settings.
pub extern "C" fn ffxConfigure(_context: *mut *mut c_void, desc: *const Header) -> ReturnCode {
    if desc.is_null() {
        return ERROR_PARAMETER;
    }
    // SAFETY: the title's description.
    note("ffxConfigure", unsafe { (*desc).kind });
    OK
}

pub extern "C" fn ffxQuery(_context: *mut *mut c_void, desc: *mut Header) -> ReturnCode {
    if desc.is_null() {
        return ERROR_PARAMETER;
    }
    // SAFETY: the title's description.
    let kind = unsafe { (*desc).kind };
    match kind {
        QUERY_JITTER_PHASE_COUNT => {
            // SAFETY: a description of this type.
            let q = unsafe { &*desc.cast::<QueryPhaseCount>() };
            let Some(count) = phase_count(q.render_width, q.display_width) else {
                return ERROR_PARAMETER;
            };
            if q.out.is_null() {
                return ERROR_PARAMETER;
            }
            // SAFETY: checked non-null; the title's out pointer.
            unsafe { q.out.write(count) };
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::info!(
                    render = q.render_width,
                    display = q.display_width,
                    count,
                    "ffx: jitter phase count"
                );
            });
            OK
        }
        QUERY_JITTER_OFFSET => {
            // SAFETY: a description of this type.
            let q = unsafe { &*desc.cast::<QueryJitterOffset>() };
            let Some((x, y)) = jitter_offset(q.index, q.phase_count) else {
                return ERROR_PARAMETER;
            };
            if q.out_x.is_null() || q.out_y.is_null() {
                return ERROR_PARAMETER;
            }
            // SAFETY: checked non-null; the title's out pointers.
            unsafe {
                q.out_x.write(x);
                q.out_y.write(y);
            }
            OK
        }
        QUERY_GPU_MEMORY => {
            // SAFETY: a description of this type.
            let q = unsafe { &*desc.cast::<QueryGpuMemory>() };
            if q.out.is_null() {
                return ERROR_PARAMETER;
            }
            // The upscale takes no GPU memory of the title's.
            // SAFETY: checked non-null; the title's out pointer.
            unsafe { q.out.write([0, 0]) };
            OK
        }
        _ => {
            note("ffxQuery", kind);
            ERROR_UNKNOWN_DESCTYPE
        }
    }
}

/// The context at `*context`, if it is one of ours.
fn context_at(context: *mut *mut c_void) -> Option<Context> {
    if context.is_null() {
        return None;
    }
    // SAFETY: the title's context pointer.
    let at = unsafe { *context } as usize;
    let contexts = CONTEXTS.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: made by `ffxCreateContext` and not destroyed (it is listed).
    contexts
        .contains(&at)
        .then(|| unsafe { *(at as *const Context) })
}

/// What a dispatch description asks of [`xtp_gpu::list::upscale`].
fn upscale_of(d: &DispatchUpscale) -> xtp_gpu::list::Upscale {
    xtp_gpu::list::Upscale {
        list: d.command_list.cast(),
        color: d.resources[COLOR].resource.cast(),
        output: d.resources[OUTPUT].resource.cast(),
        render: d.render_size,
        size: d.upscale_size,
        jitter: d.jitter,
    }
}

pub extern "C" fn ffxDispatch(context: *mut *mut c_void, desc: *const Header) -> ReturnCode {
    if desc.is_null() {
        return ERROR_PARAMETER;
    }
    // SAFETY: the title's description.
    let kind = unsafe { (*desc).kind };
    if kind != DISPATCH_UPSCALE {
        note("ffxDispatch", kind);
        return ERROR_UNKNOWN_DESCTYPE;
    }
    // SAFETY: a description of this type.
    let d = unsafe { desc.cast::<DispatchUpscale>().read() };
    let u = upscale_of(&d);
    if u.list.is_null() || u.color.is_null() || u.output.is_null() {
        return ERROR_PARAMETER;
    }
    // AMD's upscaler refuses sizes past the context's; the bilinear pass
    // takes any, so it only says so.
    if let Some(c) = context_at(context)
        && (u.render.iter().zip(c.max_render).any(|(&r, m)| r > m)
            || u.size.iter().zip(c.max_upscale).any(|(&s, m)| s > m))
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!(?c, render = ?u.render, size = ?u.size, "ffx: a dispatch past its context's sizes"));
    }
    let ran = xtp_gpu::list::upscale(&u);
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| {
        tracing::info!(
            render = ?d.render_size,
            upscale = ?d.upscale_size,
            jitter = ?d.jitter,
            color = ?d.resources[COLOR].description,
            output = ?d.resources[OUTPUT].description,
            states = ?d.resources.map(|r| r.state),
            ran,
            "ffx: first dispatch, a bilinear upscale"
        );
    });
    if ran { OK } else { ERROR }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ffx",
        dlls: &[
            "amd_fidelityfx_loader_xbox.dll",
            "amd_fidelityfx_upscaler_xbox.dll",
        ],
        exports: vec![
            hle_export!(ffxCreateContext),
            hle_export!(ffxDestroyContext),
            hle_export!(ffxConfigure),
            hle_export!(ffxQuery),
            hle_export!(ffxDispatch),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amds_dlls_stay_out_unless_fsr4_is_asked_for() {
        assert!(stands_in_for(None));
        assert!(stands_in_for(Some("0")));
        assert!(stands_in_for(Some("")));
        assert!(!stands_in_for(Some("1")));
    }

    /// 720p to 1080p, the title's Series S sizes: 18 phases, the divisor
    /// that stayed 0 at title+0x28f03e8. Other ratios as AMD's formula.
    #[test]
    fn the_phase_count_is_8_times_the_squared_ratio() {
        assert_eq!(phase_count(1280, 1920), Some(18));
        assert_eq!(phase_count(1920, 3840), Some(32));
        assert_eq!(phase_count(1920, 1920), Some(8));
        // 8 × (2560 ÷ 1706)² = 18.01...: cut, not rounded.
        assert_eq!(phase_count(1706, 2560), Some(18));
        assert_eq!(phase_count(0, 1920), None);
    }

    /// Halton (2, 3) from 1, centred: the first offsets of FSR 2's sample
    /// code, and a full cycle of 18 that repeats and stays inside half a
    /// pixel.
    #[test]
    fn jitter_offsets_follow_halton_2_3() {
        let near = |(x, y): (f32, f32), (a, b): (f32, f32)| {
            assert!(
                (x - a).abs() < 1e-6 && (y - b).abs() < 1e-6,
                "{x},{y} vs {a},{b}"
            );
        };
        near(jitter_offset(0, 18).unwrap(), (0.0, -1.0 / 6.0));
        near(jitter_offset(1, 18).unwrap(), (-0.25, 1.0 / 6.0));
        near(jitter_offset(2, 18).unwrap(), (0.25, -7.0 / 18.0));
        near(jitter_offset(3, 18).unwrap(), (-0.375, -1.0 / 18.0));
        for i in 0..18 {
            let (x, y) = jitter_offset(i, 18).unwrap();
            assert!(x.abs() < 0.5 && y.abs() < 0.5);
            assert_eq!(jitter_offset(i + 18, 18), jitter_offset(i, 18));
        }
        assert_eq!(jitter_offset(5, 0), None);
    }

    fn header(kind: u64) -> Header {
        Header {
            kind,
            next: std::ptr::null(),
        }
    }

    /// The three queries the title makes, laid out as it lays them out,
    /// get their answers written; any other type is refused.
    #[test]
    fn the_titles_queries_are_answered() {
        let mut count = 0i32;
        let mut q = QueryPhaseCount {
            header: header(QUERY_JITTER_PHASE_COUNT),
            render_width: 1280,
            display_width: 1920,
            out: &mut count,
        };
        assert_eq!(ffxQuery(std::ptr::null_mut(), (&raw mut q).cast()), OK);
        assert_eq!(count, 18);

        let (mut x, mut y) = (f32::NAN, f32::NAN);
        let mut q = QueryJitterOffset {
            header: header(QUERY_JITTER_OFFSET),
            index: 1,
            phase_count: count,
            out_x: &mut x,
            out_y: &mut y,
        };
        assert_eq!(ffxQuery(std::ptr::null_mut(), (&raw mut q).cast()), OK);
        assert_eq!((x, y), jitter_offset(1, 18).unwrap());

        // Garbage before, as the title's uninitialised locals.
        let mut usage = [u64::MAX; 2];
        let mut q = QueryGpuMemory {
            header: header(QUERY_GPU_MEMORY),
            out: &mut usage,
        };
        assert_eq!(ffxQuery(std::ptr::null_mut(), (&raw mut q).cast()), OK);
        assert_eq!(usage, [0, 0]);

        let mut other = header(6);
        assert_eq!(
            ffxQuery(std::ptr::null_mut(), &raw mut other),
            ERROR_UNKNOWN_DESCTYPE
        );
        // A zero width would divide by zero in AMD's formula.
        let mut q = QueryPhaseCount {
            header: header(QUERY_JITTER_PHASE_COUNT),
            render_width: 0,
            display_width: 1920,
            out: &mut count,
        };
        assert_eq!(
            ffxQuery(std::ptr::null_mut(), (&raw mut q).cast()),
            ERROR_PARAMETER
        );
    }

    /// The create chain as the title builds it (upscale, console backend,
    /// version) makes a context that destroy frees once.
    #[test]
    fn contexts_are_made_from_the_titles_chain_and_freed_once() {
        let version = Header {
            kind: UPSCALE_VERSION,
            next: std::ptr::null(),
        };
        let backend = Header {
            kind: CONSOLE_BACKEND,
            next: &version,
        };
        let create = CreateUpscale {
            kind: CREATE_UPSCALE,
            next: &backend,
            flags: 0x29,
            max_render: [1280, 720],
            max_upscale: [1920, 1080],
            message: std::ptr::null(),
        };
        let mut context = std::ptr::null_mut();
        let chain = (&raw const create).cast::<Header>();
        assert_eq!(ffxCreateContext(&mut context, chain, std::ptr::null()), OK);
        assert!(!context.is_null());
        assert_eq!(
            context_at(&mut context),
            Some(Context {
                max_render: [1280, 720],
                max_upscale: [1920, 1080],
            })
        );
        let mut same = context;
        assert_eq!(ffxDestroyContext(&mut same, std::ptr::null()), OK);
        assert!(same.is_null());
        assert_eq!(context_at(&mut context), None);
        // The title's copy is stale now: not freed twice.
        assert_eq!(
            ffxDestroyContext(&mut context, std::ptr::null()),
            ERROR_PARAMETER
        );
        // A chain without an upscale description makes none.
        assert_eq!(
            ffxCreateContext(&mut context, &backend, std::ptr::null()),
            ERROR_PARAMETER
        );
    }

    /// A dispatch description as the title fills it: the list at +0x10, the
    /// colour input and the output among the seven resources, the jitter
    /// and both sizes reach the upscale.
    #[test]
    fn a_dispatch_hands_its_list_textures_sizes_and_jitter_to_the_upscale() {
        let resource = |at: usize, state: u32| Resource {
            resource: at as *const c_void,
            description: [0; 8],
            state,
        };
        let mut resources = [resource(0, 0); 7];
        resources[COLOR] = resource(0x1000, 0xc);
        resources[1] = resource(0x2000, 0xc);
        resources[OUTPUT] = resource(0x7000, 0x2);
        let d = DispatchUpscale {
            kind: DISPATCH_UPSCALE,
            next: std::ptr::null(),
            command_list: 0x9000 as *const c_void,
            resources,
            jitter: [0.25, -0.125],
            motion_vector_scale: [1280.0, 720.0],
            render_size: [1280, 720],
            upscale_size: [1920, 1080],
            sharpening: 0,
            sharpness: 0.0,
            frame_time_delta: 33.0,
            pre_exposure: 1.0,
            reset: 0,
            camera: [0.1, 1000.0, 1.0, 1.0],
            flags: 0,
        };
        let u = upscale_of(&d);
        assert_eq!(
            (u.list as usize, u.color as usize, u.output as usize),
            (0x9000, 0x1000, 0x7000)
        );
        assert_eq!(
            (u.render, u.size, u.jitter),
            ([1280, 720], [1920, 1080], [0.25, -0.125])
        );
        // Without a list nothing is recorded.
        let none = DispatchUpscale {
            command_list: std::ptr::null(),
            ..d
        };
        assert_eq!(
            ffxDispatch(std::ptr::null_mut(), (&raw const none).cast()),
            ERROR_PARAMETER
        );
        let mut other = header(0x0001_0006);
        assert_eq!(
            ffxDispatch(std::ptr::null_mut(), &raw mut other),
            ERROR_UNKNOWN_DESCTYPE
        );
    }
}
