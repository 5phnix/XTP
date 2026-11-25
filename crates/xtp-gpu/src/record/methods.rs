//! The methods a recording keeps, by object kind and table slot: for each,
//! a typed wrapper that records the call and forwards it to the object's
//! own table ([`super::proxy`]), what its arguments are for the replay
//! (one [`Arg`] per parameter, in order), and the call the replay makes
//! with them.
//!
//! Slots are the older tables' (2020-era layout); lists and queues created
//! by a newer device have later slots for some methods ([`newer_list`],
//! [`newer_queue`]). Methods that only answer questions (descriptions,
//! sizes, feature queries) or only matter to profilers are not kept.

use super::wire::{Arg, Blob, Fix};
use crate::object::{self, This};

/// Object kinds, as `crate::object` numbers them.
pub(crate) const DEVICE: u8 = object::DEVICE as u8;
pub(crate) const HEAP: u8 = object::HEAP as u8;
pub(crate) const RESOURCE: u8 = object::RESOURCE as u8;
pub(crate) const FENCE: u8 = object::FENCE as u8;
pub(crate) const QUEUE: u8 = object::QUEUE as u8;
pub(crate) const ALLOCATOR: u8 = object::ALLOCATOR as u8;
pub(crate) const LIST: u8 = object::LIST as u8;
pub(crate) const DESCRIPTOR_HEAP: u8 = object::DESCRIPTOR_HEAP as u8;
pub(crate) const ROOT_SIGNATURE: u8 = object::ROOT_SIGNATURE as u8;
pub(crate) const QUERY_HEAP: u8 = object::QUERY_HEAP as u8;
pub(crate) const COMMAND_SIGNATURE: u8 = object::COMMAND_SIGNATURE as u8;
pub(crate) const PIPELINE: u8 = object::PIPELINE as u8;

/// Slots in each kind's table (`crate::object::Table` sizes).
pub(crate) fn table_len(kind: u8) -> Option<usize> {
    Some(match kind {
        DEVICE | LIST => 160,
        QUEUE => 36,
        RESOURCE => 32,
        HEAP | FENCE | ALLOCATOR | DESCRIPTOR_HEAP | ROOT_SIGNATURE | QUERY_HEAP
        | COMMAND_SIGNATURE | PIPELINE => 24,
        _ => return None,
    })
}

/// A list slot in the newer layout (`crate::list`: two methods more before
/// ExecuteIndirect).
pub(crate) const fn newer_list(slot: usize) -> usize {
    if slot >= 59 { slot + 2 } else { slot }
}

/// A queue slot in the newer layout (`crate::queue`: two methods more
/// before Signal).
pub(crate) const fn newer_queue(slot: usize) -> usize {
    if slot >= 14 { slot + 2 } else { slot }
}

/// When a method is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Always: objects and views the recorded frames may use, made at any
    /// time before them.
    Journal,
    /// A list's commands: kept since the list's last reset before the
    /// recorded frames, all of them during.
    List,
    /// Only during the recorded frames.
    Window,
    /// Only during the recorded frames, with the memory captured before
    /// and after (submissions, page mappings).
    Capture,
    /// A present: captured as submissions are, and where the recorded
    /// frames start and end.
    Present,
}

pub(crate) const JOURNAL: u8 = 0;
pub(crate) const LISTED: u8 = 1;
pub(crate) const WINDOW: u8 = 2;
pub(crate) const CAPTURE: u8 = 3;
pub(crate) const PRESENTS: u8 = 4;

pub(crate) const fn role(r: u8) -> Role {
    match r {
        JOURNAL => Role::Journal,
        LISTED => Role::List,
        WINDOW => Role::Window,
        CAPTURE => Role::Capture,
        _ => Role::Present,
    }
}

/// A value passed in a register or stack slot, as the replay keeps it.
pub(crate) trait Word: Sized {
    fn word(self) -> u64;
    fn from_word(w: u64) -> Self;
}

macro_rules! words {
    ($($t:ty),*) => {$(
        impl Word for $t {
            fn word(self) -> u64 {
                self as u64
            }
            fn from_word(w: u64) -> Self {
                w as $t
            }
        }
    )*};
}
words!(u8, u16, u32, u64, usize, i32);

impl Word for f32 {
    fn word(self) -> u64 {
        u64::from(self.to_bits())
    }
    fn from_word(w: u64) -> Self {
        f32::from_bits(w as u32)
    }
}

impl Word for () {
    fn word(self) -> u64 {
        0
    }
    fn from_word(_: u64) -> Self {}
}

impl<T> Word for *const T {
    fn word(self) -> u64 {
        self as u64
    }
    fn from_word(w: u64) -> Self {
        w as *const T
    }
}

impl<T> Word for *mut T {
    fn word(self) -> u64 {
        self as u64
    }
    fn from_word(w: u64) -> Self {
        w as *mut T
    }
}

// What the captures build from the title's arguments. The title passes
// pointers the method itself reads, the same number of bytes; they are
// read the same way here, before the method runs.

fn w(v: impl Word) -> Arg {
    Arg::Word(v.word())
}

fn obj(p: This) -> Arg {
    Arg::Obj(p as u64)
}

fn addr(a: u64) -> Arg {
    Arg::Addr(a)
}

fn handle(h: u64) -> Arg {
    Arg::Handle(h)
}

/// An out pointer that gets an object of `kind`; the call fills in what
/// the session made ([`super::proxy`]).
fn out(kind: u8, p: *mut This) -> Arg {
    Arg::Out {
        kind,
        made: p as u64,
    }
}

/// `len` bytes at `p`, or none.
fn bytes(p: *const u8, len: usize) -> Option<Blob> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the title passes `len` readable bytes (see above).
    let b = unsafe { std::slice::from_raw_parts(p, len) };
    Some(Blob {
        bytes: b.to_vec(),
        fixups: Vec::new(),
    })
}

fn arg(b: Option<Blob>) -> Arg {
    b.map_or(Arg::Word(0), Arg::Blob)
}

fn copy<T>(p: *const T, count: usize) -> Arg {
    arg(bytes(p.cast(), count * size_of::<T>()))
}

fn interface(p: *const object::Iid) -> Arg {
    copy(p, 1)
}

impl Blob {
    fn word(&self, at: usize) -> u64 {
        self.bytes
            .get(at..at + 8)
            .map_or(0, |b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
    }

    fn half(&self, at: usize) -> u32 {
        self.bytes
            .get(at..at + 4)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
    }

    fn fix(mut self, at: usize, fix: Fix) -> Blob {
        if at + 8 <= self.bytes.len() {
            self.fixups.push((at as u32, fix));
        }
        self
    }

    /// `fix` on the word at `at` of each of `count` elements `stride`
    /// bytes apart.
    fn fix_each(mut self, count: usize, stride: usize, at: usize, fix: impl Fn() -> Fix) -> Blob {
        for i in 0..count {
            self = self.fix(i * stride + at, fix());
        }
        self
    }

    /// The pointer at `at` points at `len` bytes, copied too (with their
    /// own fixups from `more`).
    fn nested(self, at: usize, len: usize, more: impl FnOnce(Blob) -> Blob) -> Blob {
        let p = self.word(at) as *const u8;
        match bytes(p, len) {
            Some(inner) => self.fix(at, Fix::Blob(more(inner))),
            None => self,
        }
    }
}

/// A NUL-terminated string's bytes, the NUL included.
fn c_string(p: *const u8) -> Option<Blob> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated string (as above).
    let len = unsafe { std::ffi::CStr::from_ptr(p.cast()) }
        .to_bytes_with_nul()
        .len();
    bytes(p, len)
}

fn with_string(b: Blob, at: usize) -> Blob {
    match c_string(b.word(at) as *const u8) {
        Some(s) => b.fix(at, Fix::Blob(s)),
        None => b,
    }
}

/// Input elements (32 bytes, the semantic name first).
fn input_elements(b: Blob, at: usize, count: usize) -> Blob {
    b.nested(at, count * 32, |e| {
        (0..count).fold(e, |e, i| with_string(e, i * 32))
    })
}

/// A pipeline stream's subobjects (`crate::pipeline`): what they point at.
fn stream(b: Blob) -> Blob {
    let mut b = b;
    let mut at = 0;
    while at + 8 <= b.bytes.len() {
        let kind = b.half(at);
        let Some((size, shader)) = crate::pipeline::recorded_subobject(kind) else {
            break;
        };
        if at + size > b.bytes.len() {
            break;
        }
        b = match kind {
            _ if shader => {
                let len = b.word(at + 16) as usize;
                b.nested(at + 8, len, |c| c)
            }
            0 => b.fix(at + 8, Fix::Obj),
            12 => {
                let count = b.half(at + 16) as usize;
                input_elements(b, at + 8, count)
            }
            7 => {
                let (entries, strides) = (b.half(at + 16) as usize, b.half(at + 32) as usize);
                b.nested(at + 8, entries * 24, |e| {
                    (0..entries).fold(e, |e, i| with_string(e, i * 24 + 8))
                })
                .nested(at + 24, strides * 4, |s| s)
            }
            19 => {
                let len = b.word(at + 16) as usize;
                b.nested(at + 8, len, |c| c)
            }
            22 => {
                let count = b.half(at + 8) as usize;
                b.nested(at + 16, count * 8, |c| c)
            }
            _ => b,
        };
        at += size;
    }
    b
}

/// Rectangles (16 bytes each).
fn rects(count: u32, p: *const u8) -> Arg {
    arg(bytes(p, count as usize * 16))
}

/// A copy location (48 bytes, the resource first).
fn location(p: *const u8) -> Arg {
    arg(bytes(p, 48).map(|b| b.fix(0, Fix::Obj)))
}

macro_rules! methods {
    ($(
        $name:ident ( $($a:ident : $t:ty),* ) $(-> $r:ty)? => $capture:expr;
    )*) => {$(
        pub(crate) mod $name {
            #[allow(unused_imports)]
            use super::*;
            pub(crate) type F = extern "system" fn(This $(, $t)*) $(-> $r)?;

            /// Records the call (as role `R` of kind `K` at slot `S`) and
            /// runs it.
            pub(crate) extern "system" fn wrap<const K: u8, const S: usize, const R: u8>(
                this: This $(, $a: $t)*
            ) $(-> $r)? {
                let original = super::super::proxy::original(this, S);
                // SAFETY: the object's own table holds this method at `S`.
                let original = unsafe { std::mem::transmute::<*const (), F>(original) };
                let capture: fn($($t),*) -> Vec<Arg> = $capture;
                super::super::proxy::call(
                    K,
                    S,
                    super::role(R),
                    this,
                    || capture($($a),*),
                    || original(this $(, $a)*),
                )
            }

            /// Calls `f` (this method) with the replay's words.
            ///
            /// # Safety
            /// `f` is this method; the words are the replay's arguments
            /// for it, pointers into memory it keeps alive.
            pub(crate) unsafe fn replay(f: *const (), this: This, words: &[u64]) -> u64 {
                // SAFETY: as the caller says.
                let f = unsafe { std::mem::transmute::<*const (), F>(f) };
                #[allow(unused_mut, unused_variables)]
                let mut words = words.iter().copied();
                $( let $a: $t = Word::from_word(words.next().unwrap_or(0)); )*
                Word::word(f(this $(, $a)*))
            }
        }
    )*};
}

type Iid = *const object::Iid;
type Out = *mut This;
type Bytes = *const u8;

methods! {
    // The device: objects.
    create_queue(desc: Bytes, iid: Iid, made: Out) -> i32 =>
        |desc, i, o| vec![arg(bytes(desc, 16)), interface(i), out(QUEUE, o)];
    create_allocator(kind: u32, iid: Iid, made: Out) -> i32 =>
        |k, i, o| vec![w(k), interface(i), out(ALLOCATOR, o)];
    create_graphics(desc: Bytes, iid: Iid, made: Out) -> i32 => |desc, i, o| {
        // The root signature, then five stages' (code, size).
        let d = bytes(desc, 88).map(|b| {
            (0..5).fold(b.fix(0, Fix::Obj), |b, s| {
                let len = b.word(16 + 16 * s) as usize;
                b.nested(8 + 16 * s, len, |c| c)
            })
        });
        vec![arg(d), interface(i), out(PIPELINE, o)]
    };
    create_compute(desc: Bytes, iid: Iid, made: Out) -> i32 => |desc, i, o| {
        let d = bytes(desc, 24).map(|b| {
            let len = b.word(16) as usize;
            b.fix(0, Fix::Obj).nested(8, len, |c| c)
        });
        vec![arg(d), interface(i), out(PIPELINE, o)]
    };
    create_compute_x(desc: Bytes, extensions: u32, extended: Bytes, iid: Iid, made: Out) -> i32 =>
        |desc, n, _ext: Bytes, i, o| {
            let d = bytes(desc, 24).map(|b| {
                let len = b.word(16) as usize;
                b.fix(0, Fix::Obj).nested(8, len, |c| c)
            });
            // The console extensions are not read (`crate::pipeline`).
            vec![arg(d), w(n), w(0u64), interface(i), out(PIPELINE, o)]
        };
    create_stream(desc: Bytes, iid: Iid, made: Out) -> i32 => |desc, i, o| {
        let d = bytes(desc, 16).map(|b| {
            let len = b.word(0) as usize;
            b.nested(8, len, stream)
        });
        vec![arg(d), interface(i), out(PIPELINE, o)]
    };
    create_list(nodes: u32, kind: u32, allocator: This, pipeline: This, iid: Iid, made: Out) -> i32 =>
        |n, k, a, p, i, o| vec![w(n), w(k), obj(a), obj(p), interface(i), out(LIST, o)];
    create_list_x(desc: Bytes, allocator: This, pipeline: This, iid: Iid, made: Out) -> i32 =>
        |desc, a, p, i, o| vec![arg(bytes(desc, 16)), obj(a), obj(p), interface(i), out(LIST, o)];
    create_descriptor_heap(desc: Bytes, iid: Iid, made: Out) -> i32 =>
        // The description, then what may be a placement (read as a hint).
        |desc, i, o| vec![arg(bytes(desc, 32)), interface(i), out(DESCRIPTOR_HEAP, o)];
    create_root(nodes: u32, blob: Bytes, size: usize, iid: Iid, made: Out) -> i32 =>
        |n, b, s, i, o| vec![w(n), arg(bytes(b, s)), w(s), interface(i), out(ROOT_SIGNATURE, o)];
    create_committed(heap: Bytes, flags: u32, desc: Bytes, state: u32, clear: Bytes, iid: Iid, made: Out) -> i32 =>
        |h, f, d, s, c, i, o| vec![
            arg(bytes(h, 20)), w(f), arg(bytes(d, 56)), w(s), arg(bytes(c, 20)), interface(i), out(RESOURCE, o),
        ];
    create_committed3(
        heap: Bytes, flags: u32, desc: Bytes, layout: u32, clear: Bytes, protected: This,
        castable_count: u32, castable: *const u32, iid: Iid, made: Out
    ) -> i32 => |h, f, d, l, c, p, n, formats, i, o| vec![
        arg(bytes(h, 20)), w(f), arg(bytes(d, 56)), w(l), arg(bytes(c, 20)), obj(p),
        w(n), copy(formats, n as usize), interface(i), out(RESOURCE, o),
    ];
    create_heap(desc: Bytes, iid: Iid, made: Out) -> i32 =>
        |d, i, o| vec![arg(bytes(d, 48)), interface(i), out(HEAP, o)];
    create_placed(heap: This, offset: u64, desc: Bytes, state: u32, clear: Bytes, iid: Iid, made: Out) -> i32 =>
        |h, off, d, s, c, i, o| vec![
            obj(h), w(off), arg(bytes(d, 56)), w(s), arg(bytes(c, 20)), interface(i), out(RESOURCE, o),
        ];
    create_at_address(address: u64, desc: Bytes, layout: u32, clear: Bytes, iid: Iid, made: Out) -> i32 =>
        |a, d, l, c, i, o| vec![
            addr(a), arg(bytes(d, 56)), w(l), arg(bytes(c, 20)), interface(i), out(RESOURCE, o),
        ];
    create_fence(initial: u64, flags: u32, iid: Iid, made: Out) -> i32 =>
        |v, f, i, o| vec![w(v), w(f), interface(i), out(FENCE, o)];
    create_query_heap(desc: Bytes, iid: Iid, made: Out) -> i32 =>
        |d, i, o| vec![arg(bytes(d, 12)), interface(i), out(QUERY_HEAP, o)];
    create_signature(desc: Bytes, root: This, iid: Iid, made: Out) -> i32 => |d, r, i, o| {
        // (stride, count, arguments of 16 bytes, node mask).
        let d = bytes(d, 24).map(|b| {
            let count = b.half(4) as usize;
            b.nested(8, count * 16, |a| a)
        });
        vec![arg(d), obj(r), interface(i), out(COMMAND_SIGNATURE, o)]
    };
    register_page_pool(address: u64, pages: u32, handle: *mut usize) -> i32 =>
        |a, n, _h: *mut usize| vec![addr(a), w(n), Arg::Scratch(8)];
    unregister_page_pool(handle: usize) -> i32 => |h| vec![w(h)];

    // The device: views and descriptor copies.
    create_cbv(desc: Bytes, at: u64) => |d, h| vec![arg(bytes(d, 16).map(|b| b.fix(0, Fix::Addr))), handle(h)];
    create_srv(resource: This, desc: Bytes, at: u64) =>
        |r, d, h| vec![obj(r), arg(bytes(d, 40)), handle(h)];
    create_uav(resource: This, counter: This, desc: Bytes, at: u64) =>
        |r, c, d, h| vec![obj(r), obj(c), arg(bytes(d, 40)), handle(h)];
    create_rtv(resource: This, desc: Bytes, at: u64) =>
        |r, d, h| vec![obj(r), arg(bytes(d, 24)), handle(h)];
    create_dsv(resource: This, desc: Bytes, at: u64) =>
        |r, d, h| vec![obj(r), arg(bytes(d, 24)), handle(h)];
    create_sampler(desc: Bytes, at: u64) => |d, h| vec![arg(bytes(d, 52)), handle(h)];
    create_sampler_x(desc: Bytes, at: u64) => |d, h| vec![arg(bytes(d, 108)), handle(h)];
    create_placed_view(resource: Bytes, desc: Bytes, at: u64) =>
        |r, d, h| vec![arg(bytes(r, 56).map(|b| b.fix(0, Fix::Addr))), arg(bytes(d, 64)), handle(h)];
    copy_descriptors(
        dst_ranges: u32, dst_starts: *const u64, dst_sizes: *const u32,
        src_ranges: u32, src_starts: *const u64, src_sizes: *const u32, kind: u32
    ) => |dn, ds, dz, sn, ss, sz, k| {
        let starts = |n: u32, p: *const u64| {
            arg(bytes(p.cast(), n as usize * 8).map(|b| b.fix_each(n as usize, 8, 0, || Fix::Handle)))
        };
        vec![
            w(dn), starts(dn, ds), copy(dz, dn as usize),
            w(sn), starts(sn, ss), copy(sz, sn as usize), w(k),
        ]
    };
    copy_descriptors_simple(count: u32, dst: u64, src: u64, kind: u32) =>
        |n, d, s, k| vec![w(n), handle(d), handle(s), w(k)];

    // Queues.
    execute(count: u32, lists: *const This) => |n, l: *const This| vec![
        w(n), arg(bytes(l.cast(), n as usize * 8).map(|b| b.fix_each(n as usize, 8, 0, || Fix::Obj))),
    ];
    execute_x(count: u32, lists: *const This, token: u64, flags: u32) =>
        |n, l: *const This, t, f| vec![
            w(n),
            arg(bytes(l.cast(), n as usize * 8).map(|b| b.fix_each(n as usize, 8, 0, || Fix::Obj))),
            w(t), w(f),
        ];
    queue_signal(fence: This, value: u64) -> i32 => |f, v| vec![obj(f), w(v)];
    queue_signal2_x(fence: This, value: u64, bottom: Bytes, top: Bytes) -> i32 =>
        // The sync settings are not read (`crate::queue`).
        |f, v, _b: Bytes, _t: Bytes| vec![obj(f), w(v), w(0u64), w(0u64)];
    queue_wait(fence: This, value: u64) -> i32 => |f, v| vec![obj(f), w(v)];
    present(planes: u32, parameters: Bytes, rest: Bytes) -> i32 => |n, p, _r: Bytes| {
        // Planes of 0x48 bytes: token, resource count at 8, resources at 0x10.
        let stride = crate::dxgi::planes::STRIDE;
        let planes = bytes(p, n as usize * stride).map(|b| {
            (0..n as usize).fold(b, |b, i| {
                let count = b.half(i * stride + 8) as usize;
                b.nested(i * stride + 0x10, count * 8, |r| r.fix_each(count, 8, 0, || Fix::Obj))
            })
        });
        vec![w(n), arg(planes), w(0u64)]
    };
    copy_page_mappings(count: u32, batches: Bytes, pool: u64, pool_pages: u32, flags: u32) -> i32 =>
        |n, b, p, pp, f| {
            // Batches of (destination, range count, ranges of 12 bytes).
            let batches = bytes(b, n as usize * 24).map(|b| {
                (0..n as usize).fold(b, |b, i| {
                    let count = b.half(i * 24 + 8) as usize;
                    b.fix(i * 24, Fix::Addr).nested(i * 24 + 16, count * 12, |r| r)
                })
            });
            vec![w(n), arg(batches), addr(p), w(pp), w(f)]
        };

    // Fences and allocators.
    fence_signal(value: u64) -> i32 => |v| vec![w(v)];
    allocator_reset() -> i32 => || vec![];

    // Lists.
    close() -> i32 => || vec![];
    reset(allocator: This, pipeline: This) -> i32 => |a, p| vec![obj(a), obj(p)];
    reset_x(allocator: This, pipeline: This, flags: u32) -> i32 => |a, p, f| vec![obj(a), obj(p), w(f)];
    draw(vertices: u32, instances: u32, first: u32, first_instance: u32) =>
        |a, b, c, d| vec![w(a), w(b), w(c), w(d)];
    draw_indexed(indices: u32, instances: u32, first: u32, base: i32, first_instance: u32) =>
        |a, b, c, d, e| vec![w(a), w(b), w(c), w(d), w(e)];
    dispatch(x: u32, y: u32, z: u32) => |x, y, z| vec![w(x), w(y), w(z)];
    dispatch_x(x: u32, y: u32, z: u32, flags: u32) => |x, y, z, f| vec![w(x), w(y), w(z), w(f)];
    dispatch_mesh_x(x: u32, flags: u64, indirect: u64, payload: u64) =>
        |x, f, i, p| vec![w(x), w(f), addr(i), addr(p)];
    copy_buffer_region(dst: This, dst_offset: u64, src: This, src_offset: u64, len: u64) =>
        |d, a, s, b, n| vec![obj(d), w(a), obj(s), w(b), w(n)];
    copy_texture_region(dst: Bytes, x: u32, y: u32, z: u32, src: Bytes, source_box: Bytes) =>
        |d, x, y, z, s, b| vec![location(d), w(x), w(y), w(z), location(s), arg(bytes(b, 24))];
    copy_texture_region_x(dst: Bytes, x: u32, y: u32, z: u32, src: Bytes, source_box: Bytes, flags: u32) =>
        |d, x, y, z, s, b, f| vec![location(d), w(x), w(y), w(z), location(s), arg(bytes(b, 24)), w(f)];
    copy_resource(dst: This, src: This) => |d, s| vec![obj(d), obj(s)];
    copy_resource_x(dst: This, src: This, flags: u32) => |d, s, f| vec![obj(d), obj(s), w(f)];
    resolve(dst: This, dst_sub: u32, src: This, src_sub: u32, format: u32) =>
        |d, a, s, b, f| vec![obj(d), w(a), obj(s), w(b), w(f)];
    topology(value: u32) => |v| vec![w(v)];
    viewports(count: u32, items: Bytes) => |n, p| vec![w(n), arg(bytes(p, n as usize * 24))];
    scissors(count: u32, items: Bytes) => |n, p| vec![w(n), rects(n, p)];
    blend_factor(factor: Bytes) => |p| vec![arg(bytes(p, 16))];
    stencil_ref(value: u32) => |v| vec![w(v)];
    set_pipeline(pipeline: This) => |p| vec![obj(p)];
    barrier(count: u32, barriers: Bytes) -> u32 => |n, p| {
        // Barriers of 32 bytes: (type, flags, resource, second, last); an
        // aliasing barrier's second is a resource too.
        let b = bytes(p, n as usize * 32).map(|b| {
            (0..n as usize).fold(b, |b, i| {
                let b = b.fix(i * 32 + 8, Fix::Obj);
                if b.half(i * 32) == 1 { b.fix(i * 32 + 16, Fix::Obj) } else { b }
            })
        });
        vec![w(n), arg(b)]
    };
    barrier_groups(count: u32, groups: Bytes) =>
        // The groups are not read (`crate::list`).
        |n, _g: Bytes| vec![w(n), w(0u64)];
    query(heap: This, kind: u32, index: u32) => |h, k, i| vec![obj(h), w(k), w(i)];
    resolve_queries(heap: This, kind: u32, first: u32, count: u32, buffer: This, offset: u64) =>
        |h, k, f, n, b, o| vec![obj(h), w(k), w(f), w(n), obj(b), w(o)];
    heaps(count: u32, items: *const This) => |n, p: *const This| vec![
        w(n), arg(bytes(p.cast(), n as usize * 8).map(|b| b.fix_each(n as usize, 8, 0, || Fix::Obj))),
    ];
    root_signature(signature: This) => |s| vec![obj(s)];
    table(slot: u32, at: u64) => |s, h| vec![w(s), handle(h)];
    constant(slot: u32, value: u32, offset: u32) => |s, v, o| vec![w(s), w(v), w(o)];
    constants(slot: u32, count: u32, values: *const u32, offset: u32) =>
        |s, n, v, o| vec![w(s), w(n), copy(v, n as usize), w(o)];
    view(slot: u32, at: u64) => |s, a| vec![w(s), addr(a)];
    index_buffer(view: Bytes) => |v| vec![arg(bytes(v, 16).map(|b| b.fix(0, Fix::Addr)))];
    vertex_buffers(start: u32, count: u32, views: Bytes) => |s, n, v| vec![
        w(s), w(n), arg(bytes(v, n as usize * 16).map(|b| b.fix_each(n as usize, 16, 0, || Fix::Addr))),
    ];
    targets(count: u32, handles: Bytes, one_range: i32, depth: Bytes) => |n, h, one, d| {
        let n_handles = if one != 0 { 1 } else { n as usize };
        vec![
            w(n),
            arg(bytes(h, n_handles * 8).map(|b| b.fix_each(n_handles, 8, 0, || Fix::Handle))),
            w(one),
            arg(bytes(d, 8).map(|b| b.fix(0, Fix::Handle))),
        ]
    };
    clear_dsv(at: u64, flags: u32, depth: f32, stencil: u8, count: u32, rects: Bytes) =>
        |h, f, d, s, n, r| vec![handle(h), w(f), w(d), w(s), w(n), self::rects(n, r)];
    clear_rtv(at: u64, color: Bytes, count: u32, rects: Bytes) =>
        |h, c, n, r| vec![handle(h), arg(bytes(c, 16)), w(n), self::rects(n, r)];
    clear_uav(gpu: u64, cpu: u64, resource: This, values: Bytes, count: u32, rects: Bytes) =>
        |g, c, res, v, n, r| vec![handle(g), handle(c), obj(res), arg(bytes(v, 16)), w(n), self::rects(n, r)];
    execute_indirect(signature: This, max: u32, arguments: This, offset: u64, count: This, count_offset: u64) =>
        |s, m, a, o, c, co| vec![obj(s), w(m), obj(a), w(o), obj(c), w(co)];
    copy_memory(dst: u64, src: u64, len: u64, flags: u32) =>
        |d, s, n, f| vec![addr(d), addr(s), w(n), w(f)];
    depth_bounds(min: f32, max: f32) => |a, b| vec![w(a), w(b)];
    write32(at: u64, value: u32, flush: u32, flags: u32) => |a, v, f, g| vec![addr(a), w(v), w(f), w(g)];
    write64(at: u64, value: u64, flush: u32, flags: u32) => |a, v, f, g| vec![addr(a), w(v), w(f), w(g)];
    wait_value(at: u64, comparison: u32, reference: u64) => |a, c, r| vec![addr(a), w(c), w(r)];
    counted() => || vec![];
    allocate_upload(size: u64, alignment: u64) -> *mut u8 => |s, a| vec![w(s), w(a)];
}

/// Where a method sits and how it is kept.
pub(crate) struct Place {
    pub(crate) kind: u8,
    /// Its slot in the older layout and in the newer one.
    pub(crate) slots: [usize; 2],
    pub(crate) wrap: [*const (); 2],
    pub(crate) replay: unsafe fn(*const (), This, &[u64]) -> u64,
    pub(crate) name: &'static str,
}

// SAFETY: function addresses only.
unsafe impl Sync for Place {}

macro_rules! place {
    ($kind:ident $old:literal => $new:expr, $m:ident, $role:ident) => {
        Place {
            kind: $kind,
            slots: [$old, $new],
            wrap: [
                $m::wrap::<$kind, $old, $role> as *const (),
                $m::wrap::<$kind, { $new }, $role> as *const (),
            ],
            replay: $m::replay,
            name: stringify!($m),
        }
    };
    ($kind:ident $slot:literal, $m:ident, $role:ident) => {
        place!($kind $slot => $slot, $m, $role)
    };
}

/// Every method kept.
pub(crate) static PLACES: &[Place] = &[
    place!(DEVICE 8, create_queue, JOURNAL),
    place!(DEVICE 9, create_allocator, JOURNAL),
    place!(DEVICE 10, create_graphics, JOURNAL),
    place!(DEVICE 11, create_compute, JOURNAL),
    place!(DEVICE 12, create_list, JOURNAL),
    place!(DEVICE 14, create_descriptor_heap, JOURNAL),
    place!(DEVICE 16, create_root, JOURNAL),
    place!(DEVICE 17, create_cbv, JOURNAL),
    place!(DEVICE 18, create_srv, JOURNAL),
    place!(DEVICE 19, create_uav, JOURNAL),
    place!(DEVICE 20, create_rtv, JOURNAL),
    place!(DEVICE 21, create_dsv, JOURNAL),
    place!(DEVICE 22, create_sampler, JOURNAL),
    place!(DEVICE 23, copy_descriptors, JOURNAL),
    place!(DEVICE 24, copy_descriptors_simple, JOURNAL),
    place!(DEVICE 27, create_committed, JOURNAL),
    place!(DEVICE 28, create_heap, JOURNAL),
    place!(DEVICE 29, create_placed, JOURNAL),
    place!(DEVICE 36, create_fence, JOURNAL),
    place!(DEVICE 39, create_query_heap, JOURNAL),
    place!(DEVICE 41, create_signature, JOURNAL),
    place!(DEVICE 48, create_queue, JOURNAL),
    place!(DEVICE 61, create_sampler_x, JOURNAL),
    place!(DEVICE 62, create_compute_x, JOURNAL),
    place!(DEVICE 70, create_placed_view, JOURNAL),
    place!(DEVICE 71, create_placed_view, JOURNAL),
    place!(DEVICE 73, create_list_x, JOURNAL),
    place!(DEVICE 74, register_page_pool, JOURNAL),
    place!(DEVICE 75, unregister_page_pool, JOURNAL),
    place!(DEVICE 76, create_committed, JOURNAL),
    place!(DEVICE 86, create_stream, JOURNAL),
    place!(DEVICE 127, create_committed3, JOURNAL),
    place!(DEVICE 133, create_at_address, JOURNAL),
    place!(QUEUE 10, execute, CAPTURE),
    place!(QUEUE 14 => newer_queue(14), queue_signal, WINDOW),
    place!(QUEUE 15 => newer_queue(15), queue_wait, WINDOW),
    place!(QUEUE 29 => newer_queue(29), copy_page_mappings, CAPTURE),
    place!(QUEUE 30 => newer_queue(30), present, PRESENTS),
    place!(QUEUE 32 => newer_queue(32), queue_signal2_x, WINDOW),
    place!(QUEUE 33 => newer_queue(33), execute_x, CAPTURE),
    place!(FENCE 10, fence_signal, WINDOW),
    place!(ALLOCATOR 8, allocator_reset, WINDOW),
    place!(LIST 9, close, LISTED),
    place!(LIST 10, reset, LISTED),
    place!(LIST 12, draw, LISTED),
    place!(LIST 13, draw_indexed, LISTED),
    place!(LIST 14, dispatch, LISTED),
    place!(LIST 15, copy_buffer_region, LISTED),
    place!(LIST 16, copy_texture_region, LISTED),
    place!(LIST 17, copy_resource, LISTED),
    place!(LIST 19, resolve, LISTED),
    place!(LIST 20, topology, LISTED),
    place!(LIST 21, viewports, LISTED),
    place!(LIST 22, scissors, LISTED),
    place!(LIST 23, blend_factor, LISTED),
    place!(LIST 24, stencil_ref, LISTED),
    place!(LIST 25, set_pipeline, LISTED),
    place!(LIST 26, barrier, LISTED),
    place!(LIST 28, query, LISTED),
    place!(LIST 29, query, LISTED),
    place!(LIST 30, resolve_queries, LISTED),
    place!(LIST 32, heaps, LISTED),
    place!(LIST 33, root_signature, LISTED),
    place!(LIST 34, root_signature, LISTED),
    place!(LIST 35, table, LISTED),
    place!(LIST 36, table, LISTED),
    place!(LIST 37, constant, LISTED),
    place!(LIST 38, constant, LISTED),
    place!(LIST 39, constants, LISTED),
    place!(LIST 40, constants, LISTED),
    place!(LIST 41, view, LISTED),
    place!(LIST 42, view, LISTED),
    place!(LIST 43, view, LISTED),
    place!(LIST 44, view, LISTED),
    place!(LIST 45, view, LISTED),
    place!(LIST 46, view, LISTED),
    place!(LIST 47, index_buffer, LISTED),
    place!(LIST 48, vertex_buffers, LISTED),
    place!(LIST 50, targets, LISTED),
    place!(LIST 51, clear_dsv, LISTED),
    place!(LIST 52, clear_rtv, LISTED),
    place!(LIST 53, clear_uav, LISTED),
    place!(LIST 54, clear_uav, LISTED),
    place!(LIST 59 => newer_list(59), execute_indirect, LISTED),
    place!(LIST 60 => newer_list(60), copy_memory, LISTED),
    place!(LIST 68 => newer_list(68), depth_bounds, LISTED),
    place!(LIST 69 => newer_list(69), write32, LISTED),
    place!(LIST 70 => newer_list(70), write64, LISTED),
    place!(LIST 71 => newer_list(71), wait_value, LISTED),
    place!(LIST 72 => newer_list(72), wait_value, LISTED),
    place!(LIST 73 => newer_list(73), counted, LISTED),
    place!(LIST 78 => newer_list(78), dispatch_x, LISTED),
    place!(LIST 83 => newer_list(83), copy_texture_region_x, LISTED),
    place!(LIST 84 => newer_list(84), copy_resource_x, LISTED),
    place!(LIST 98 => newer_list(98), dispatch_mesh_x, LISTED),
    place!(LIST 107 => newer_list(107), depth_bounds, LISTED),
    place!(LIST 124 => newer_list(124), counted, LISTED),
    place!(LIST 127 => newer_list(127), dispatch, LISTED),
    place!(LIST 137 => newer_list(137), reset_x, LISTED),
    place!(LIST 146 => 146, barrier_groups, LISTED),
    place!(LIST 148 => 148, allocate_upload, LISTED),
];

/// The method of `kind` at `slot` in layout `newer`, if kept.
pub(crate) fn find(kind: u8, newer: bool, slot: usize) -> Option<&'static Place> {
    PLACES
        .iter()
        .find(|p| p.kind == kind && p.slots[usize::from(newer)] == slot && p.applies(newer))
}

impl Place {
    /// Newer-only list methods (146, 148) have no slot in the older layout.
    pub(crate) fn applies(&self, newer: bool) -> bool {
        newer || !(self.kind == LIST && self.slots[0] >= 140)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_places_are_unique_per_kind_and_layout() {
        for newer in [false, true] {
            let mut seen = std::collections::HashSet::new();
            for p in PLACES.iter().filter(|p| p.applies(newer)) {
                let slot = p.slots[usize::from(newer)];
                assert!(seen.insert((p.kind, slot)), "{} twice at {slot}", p.name);
                assert!(
                    slot < table_len(p.kind).unwrap(),
                    "{} past its table",
                    p.name
                );
            }
        }
        assert_eq!(
            find(LIST, true, 61).map(|p| p.name),
            Some("execute_indirect")
        );
        assert_eq!(
            find(LIST, false, 59).map(|p| p.name),
            Some("execute_indirect")
        );
        assert!(find(LIST, false, 146).is_none());
        assert_eq!(find(QUEUE, true, 32).map(|p| p.name), Some("present"));
        assert_eq!(find(QUEUE, false, 10).map(|p| p.name), Some("execute"));
    }

    #[test]
    fn record_streams_copy_what_their_subobjects_point_at() {
        // A root signature subobject, then a vertex shader of 4 bytes.
        let code = [1u8, 2, 3, 4];
        let mut s = Vec::new();
        s.extend_from_slice(&0u32.to_le_bytes());
        s.extend_from_slice(&[0; 4]);
        s.extend_from_slice(&0x1234_5678u64.to_le_bytes());
        s.extend_from_slice(&1u32.to_le_bytes());
        s.extend_from_slice(&[0; 4]);
        s.extend_from_slice(&(code.as_ptr() as u64).to_le_bytes());
        s.extend_from_slice(&(code.len() as u64).to_le_bytes());
        let b = stream(Blob {
            bytes: s,
            fixups: Vec::new(),
        });
        assert_eq!(b.fixups.len(), 2);
        assert_eq!(b.fixups[0], (8, Fix::Obj));
        match &b.fixups[1] {
            (24, Fix::Blob(c)) => assert_eq!(c.bytes, code),
            other => panic!("{other:?}"),
        }
    }
}
