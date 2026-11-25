//! The recorder and the replay end to end, on the test device.

use super::proxy::{Options, Recorder, Start};
use super::replay;
use super::store::StreamReader;
use super::wire::{Arg, Blob, Event, Fix, Page, Reader, Writer};
use crate::object::{HRESULT, Iid, S_OK, This, guid, slot};

#[test]
fn record_events_come_back_as_written() {
    let blob = Blob {
        bytes: (0..24).collect(),
        fixups: vec![
            (0, Fix::Obj),
            (
                8,
                Fix::Blob(Blob {
                    bytes: vec![7; 9],
                    fixups: Vec::new(),
                }),
            ),
            (16, Fix::Handle),
        ],
    };
    let events = vec![
        Event::Device {
            iid: [3; 16],
            version: 0x2_0004,
        },
        Event::Call {
            kind: 10,
            slot: 61,
            this: 0x1234_5678_9abc,
            args: vec![
                Arg::Word(u64::MAX),
                Arg::Obj(0),
                Arg::Addr(0x9_7b11_0000),
                Arg::Handle(5),
                Arg::Blob(blob),
                Arg::Out { kind: 6, made: 42 },
                Arg::Scratch(8),
            ],
            ret: 0x8007_0057,
            note: 9,
        },
        Event::Destroy { this: 1 },
        Event::Owned {
            this: 2,
            at: 0x10_0000,
            len: 300,
        },
        Event::Window { frame: 617 },
        Event::Fence { this: 3, value: 99 },
        Event::Pages {
            gpu: true,
            pages: vec![Page {
                at: 0x5000,
                before: 77,
                bytes: vec![1; 4096],
            }],
        },
        Event::Presented { frame: 618 },
        Event::End,
    ];
    let mut w = Writer::default();
    for e in &events {
        w.event(e);
    }
    let mut r = Reader::new(&w.out);
    let mut back = Vec::new();
    while !r.done() {
        back.push(r.event().unwrap());
    }
    assert_eq!(back, events);
    // A stream cut short is corrupt, not an event.
    let mut r = Reader::new(&w.out[..w.out.len() / 2]);
    assert!(
        std::iter::from_fn(|| Some(r.event()))
            .take(20)
            .any(|e| e.is_err())
    );
}

#[test]
fn record_frames_are_read_from_the_variable() {
    use super::proxy::parse_frames;
    assert_eq!(parse_frames("618:3"), Ok((Start::Frame(618), 3)));
    assert_eq!(parse_frames("618"), Ok((Start::Frame(618), 3)));
    assert_eq!(parse_frames(""), Ok((Start::Capture, 3)));
    assert_eq!(parse_frames("capture:2"), Ok((Start::Capture, 2)));
    assert!(parse_frames("618:0").is_err());
    assert!(parse_frames("world").is_err());
}

const RESOURCE: Iid = guid(
    0x696442be,
    0xa72e,
    0x4059,
    [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
);
const ALLOCATOR: Iid = guid(
    0x6102dee4,
    0xaf59,
    0x4b09,
    [0xb9, 0x99, 0xb4, 0x4d, 0x73, 0xf0, 0x9b, 0x24],
);
const LIST: Iid = guid(
    0x5b160d0f,
    0xac1b,
    0x4185,
    [0x8b, 0xa8, 0xb3, 0xae, 0x42, 0xa5, 0xa4, 0x55],
);
const QUEUE: Iid = guid(
    0x0ec870a6,
    0x5d7e,
    0x4c22,
    [0x8c, 0xfc, 0x5b, 0xaa, 0xe0, 0x76, 0x16, 0xed],
);
const DESCRIPTOR_HEAP: Iid = guid(
    0x8efb471d,
    0x616c,
    0x4f49,
    [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
);

/// A buffer's description as the title passes it.
#[repr(C)]
struct BufferDesc {
    dimension: u32,
    alignment: u64,
    width: u64,
    height: u32,
    depth_or_array_size: u16,
    mip_levels: u16,
    format: u32,
    sample_count: u32,
    sample_quality: u32,
    layout: u32,
    flags: u32,
}

const DEFAULT_HEAP: [u32; 5] = [1, 0, 0, 0, 0];

type Committed = extern "system" fn(
    This,
    *const u8,
    u32,
    *const u8,
    u32,
    *const u8,
    *const Iid,
    *mut This,
) -> HRESULT;
type Release = extern "system" fn(This) -> u32;

/// A committed resource made through `device`'s table, as the title makes
/// one.
fn committed(device: This, desc: *const u8) -> This {
    let mut out = std::ptr::null();
    let null = std::ptr::null();
    let made = slot::<Committed>(device, 0xd8)(
        device,
        DEFAULT_HEAP.as_ptr().cast(),
        0,
        desc,
        0,
        null,
        &RESOURCE,
        &mut out,
    );
    assert_eq!(made, S_OK);
    out
}

fn buffer(device: This, len: u64) -> (This, *mut u8) {
    let desc = BufferDesc {
        dimension: 1,
        alignment: 0,
        width: len,
        height: 1,
        depth_or_array_size: 1,
        mip_levels: 1,
        format: 0,
        sample_count: 1,
        sample_quality: 0,
        layout: 1,
        flags: 0,
    };
    let b = committed(device, (&raw const desc).cast());
    let (data, size) = crate::resource::buffer_bytes(b).expect("a buffer");
    assert_eq!(size, len);
    (b, data)
}

fn words(data: *const u8) -> Vec<u32> {
    // SAFETY: a test buffer of 256 bytes.
    let bytes = unsafe { std::slice::from_raw_parts(data, 256) };
    let (w, _) = bytes.as_chunks::<4>();
    w.iter().map(|&w| u32::from_le_bytes(w)).collect()
}

fn fill(data: *mut u8, f: impl Fn(u32) -> u32) {
    for i in 0..64 {
        // SAFETY: inside a test buffer of 256 bytes.
        unsafe { data.cast::<u32>().add(i).write_unaligned(f(i as u32)) };
    }
}

fn texels(host: &crate::host::Host, texture: This) -> Vec<u8> {
    let copy = crate::resource::host_copy(texture).expect("a host copy");
    let state = copy.state.load(std::sync::atomic::Ordering::Relaxed);
    crate::resource::tests_support::read_back(host, &copy.texture, state)
}

/// The brief's check: a tiny list recorded on the test device through the
/// recording device, then replayed, gives the same texels and buffers.
/// Two submissions with a CPU write between them; the replay's objects
/// are new ones, at other addresses, so what they hold comes from the
/// replayed calls and the captured pages.
#[test]
fn record_a_list_and_replay_it_to_the_same_texels() {
    let test = "record::tests::record_a_list_and_replay_it_to_the_same_texels";
    let Some(gpu) = crate::testgpu::device(test) else {
        return;
    };
    let host = gpu.host();
    let dir = tempfile::tempdir().unwrap();
    let recorder = Recorder::begin(Options {
        dir: dir.path().to_owned(),
        start: Start::Now,
        frames: 1,
        title_memory: false,
    })
    .unwrap();
    let device = recorder.device(&crate::device::IID);
    let null = std::ptr::null();

    // A render target and three buffers.
    let desc = crate::resource::tests_support::target(16, 16, 28);
    let target = committed(device, (&raw const desc).cast());
    let (source, source_data) = buffer(device, 256);
    let (first, first_data) = buffer(device, 256);
    let (second, second_data) = buffer(device, 256);
    // A view of the target in a descriptor heap of render target views
    // (the description, then no placement).
    let heap_desc = [2u32, 1, 0, 0, 0, 0, 0, 0];
    let mut heap = null;
    type Create = extern "system" fn(This, *const u32, *const Iid, *mut This) -> HRESULT;
    assert_eq!(
        slot::<Create>(device, 0x70)(device, heap_desc.as_ptr(), &DESCRIPTOR_HEAP, &mut heap),
        S_OK
    );
    let rtv = slot::<extern "system" fn(This) -> u64>(heap, 0x48)(heap);
    slot::<extern "system" fn(This, This, *const u8, u64)>(device, 0xa0)(
        device,
        target,
        null.cast(),
        rtv,
    );
    // A queue, an allocator and a list.
    let queue_desc = [0u32; 4];
    let mut queue = null;
    assert_eq!(
        slot::<Create>(device, 0x40)(device, queue_desc.as_ptr(), &QUEUE, &mut queue),
        S_OK
    );
    let mut allocator = null;
    type CreateAllocator = extern "system" fn(This, u32, *const Iid, *mut This) -> HRESULT;
    assert_eq!(
        slot::<CreateAllocator>(device, 0x48)(device, 0, &ALLOCATOR, &mut allocator),
        S_OK
    );
    let mut list = null;
    type CreateList =
        extern "system" fn(This, u32, u32, This, This, *const Iid, *mut This) -> HRESULT;
    assert_eq!(
        slot::<CreateList>(device, 0x60)(device, 0, 0, allocator, null, &LIST, &mut list),
        S_OK
    );

    type Clear = extern "system" fn(This, u64, *const [f32; 4], u32, *const u8);
    type Copy = extern "system" fn(This, This, u64, This, u64, u64);
    type Close = extern "system" fn(This) -> HRESULT;
    type Reset = extern "system" fn(This, This, This) -> HRESULT;
    type Execute = extern "system" fn(This, u32, *const This);
    let color = [64.0f32 / 255.0, 128.0 / 255.0, 191.0 / 255.0, 1.0];
    let run = |list: This| {
        assert_eq!(slot::<Close>(list, 0x48)(list), S_OK);
        slot::<Execute>(queue, 0x50)(queue, 1, &list);
        host.drain();
    };

    // Submission 1: the CPU's words copied by the GPU, the target cleared.
    fill(source_data, |i| i * 3 + 1);
    slot::<Clear>(list, 0x1a0)(list, rtv, &color, 0, null.cast());
    slot::<Copy>(list, 0x78)(list, first, 0, source, 0, 256);
    run(list);
    // Submission 2: new words from the CPU, copied elsewhere.
    fill(source_data, |i| 1000 - i);
    assert_eq!(slot::<Reset>(list, 0x50)(list, allocator, null), S_OK);
    slot::<Copy>(list, 0x78)(list, second, 0, source, 0, 256);
    run(list);

    let want_first: Vec<u32> = (0..64).map(|i| i * 3 + 1).collect();
    let want_second: Vec<u32> = (0..64).map(|i| 1000 - i).collect();
    assert_eq!(words(first_data), want_first, "the session's first copy");
    assert_eq!(words(second_data), want_second, "the session's second copy");
    let session_texels = texels(host, target);
    assert!(
        session_texels
            .chunks(4)
            .take(16)
            .all(|t| t == [64, 128, 191, 255])
    );
    let counts = recorder.finish().unwrap();
    assert!(counts.pages >= 2, "{counts:?}");
    assert!(counts.gpu_pages >= 2, "{counts:?}");

    // The recording holds the calls made, in order.
    let mut stream = StreamReader::open(&dir.path().join("calls.bin")).unwrap();
    let mut events = Vec::new();
    while let Some(block) = stream.block().unwrap() {
        let mut r = Reader::new(&block);
        while !r.done() {
            events.push(r.event().unwrap());
        }
    }
    assert!(matches!(events.first(), Some(Event::Device { .. })));
    assert!(matches!(events.get(1), Some(Event::Window { .. })));
    assert!(matches!(events.last(), Some(Event::End)));
    let executes = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::Call {
                    kind: 8,
                    slot: 10,
                    ..
                }
            )
        })
        .count();
    assert_eq!(executes, 2);

    // The replay, on new objects.
    let options = replay::Options {
        dir: dir.path().to_owned(),
        frames: None,
        loops: 1,
        pokes: Vec::new(),
        session_gpu_pages: false,
        own_process: false,
    };
    let summary = replay::run(&options, |_| {}).unwrap();
    let mapped = |session: This| {
        summary
            .objects
            .iter()
            .find(|&&(s, _)| s == session as u64)
            .map(|&(_, r)| r as This)
            .expect("replayed")
    };
    let (target2, first2, second2) = (mapped(target), mapped(first), mapped(second));
    assert_ne!(target2, target);
    let data = |b: This| crate::resource::buffer_bytes(b).unwrap().0;
    assert_ne!(data(first2), first_data, "the replay's buffer is its own");
    assert_eq!(words(data(first2)), want_first, "the replay's first copy");
    assert_eq!(
        words(data(second2)),
        want_second,
        "the replay's second copy"
    );
    assert_eq!(texels(host, target2), session_texels, "the replay's texels");
    let last = summary
        .frames
        .last()
        .expect("the frame the recording ended in");
    assert_eq!(last.gpu_different, 0, "{last:?}");
    assert!(last.gpu_same >= 2, "{last:?}");
    assert_eq!(last.missing, 0, "{last:?}");

    // The session's objects go (the recording has ended).
    for o in [list, allocator, queue, heap, second, first, source, target] {
        slot::<Release>(o, 0x10)(o);
    }
    for (_, o) in &summary.objects {
        let o = *o as This;
        slot::<Release>(o, 0x10)(o);
    }
}
