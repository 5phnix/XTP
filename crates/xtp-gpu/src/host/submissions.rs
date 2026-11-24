//! Asynchronous submission on the host device (`XTP_TEST_ADAPTER=warp` for
//! the software adapter): what finishes when, what waits for what, and
//! what a list reset while it runs keeps.

use super::*;
use crate::queue::Written;
use crate::testgpu::{Kernel, Pass, TitleBuffer, device, pattern};

/// The tests here one at a time: a test that holds the host queue
/// ([`Gate`]) would stop one that waits inside a submission (which holds
/// the submission lock) until the gate's timer opens it. Tests elsewhere
/// wait for the GPU only after submitting, so they are only delayed.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// A fence of ours that holds the host queue: what is submitted after
/// [`Gate::close`] stays in flight on the GPU until [`Gate::open`]. Opened
/// after 20 s whatever happens, so that a test failing with the queue held
/// cannot hang the others (which are only delayed meanwhile).
pub(crate) struct Gate {
    open: Option<std::sync::mpsc::Sender<()>>,
    timer: Option<std::thread::JoinHandle<()>>,
}

impl Gate {
    pub(crate) fn close(host: &Host) -> Gate {
        // SAFETY: a fence of ours on our device and queue.
        let fence: ID3D12Fence = unsafe {
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            host.queue.Wait(&fence, 1).unwrap();
            fence
        };
        let (open, opened) = std::sync::mpsc::channel::<()>();
        let fence = Held(fence);
        let timer = std::thread::spawn(move || {
            let fence = fence;
            let _ = opened.recv_timeout(std::time::Duration::from_secs(20));
            // SAFETY: a plain signal of our fence.
            let _ = unsafe { fence.0.Signal(1) };
        });
        Gate {
            open: Some(open),
            timer: Some(timer),
        }
    }

    pub(crate) fn open(&mut self) {
        if let Some(open) = self.open.take() {
            let _ = open.send(());
        }
        if let Some(timer) = self.timer.take() {
            let _ = timer.join();
        }
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.open();
    }
}

/// The gate's fence, signalled from its timer thread.
struct Held(ID3D12Fence);

// SAFETY: D3D12 fences are free-threaded.
unsafe impl Send for Held {}

fn word(at: &u64) -> u64 {
    // SAFETY: a live word of the test's.
    unsafe { std::ptr::read_volatile(at) }
}

fn ptr(at: &u64) -> *mut u8 {
    (&raw const *at).cast_mut().cast()
}

/// Three submissions held on the GPU: nothing of theirs is done when they
/// return. Once the GPU runs them they finish in order, each one's
/// bottom-of-pipe write before the signals queued behind it and before the
/// next one's.
#[test]
fn submissions_finish_in_order_after_their_gpu_work() {
    let test = "host::submissions::submissions_finish_in_order_after_their_gpu_work";
    let _serial = serial();
    let Some(host) = device(test) else { return };
    let words: [&'static u64; 3] = [(); 3].map(|()| &*Box::leak(Box::new(0u64)));
    let lists: Vec<HostList> = (0..3).map(|_| host.new_list().unwrap()).collect();
    let seen: std::sync::Arc<Mutex<Vec<[u64; 3]>>> = std::sync::Arc::default();
    let mut gate = Gate::close(host);
    for (i, list) in lists.iter().enumerate() {
        list.defer_write(ptr(words[i]), Written::Value(i as u64 + 1), 8);
        list.close();
        host.execute(&[list]);
        let seen = seen.clone();
        host.after(move || seen.lock().unwrap().push(words.map(word)));
    }
    // Returned with the GPU held: in flight, nothing written.
    assert!(lists.iter().all(HostList::running));
    assert_eq!(words.map(word), [0; 3]);
    assert!(seen.lock().unwrap().is_empty());
    gate.open();
    host.drain();
    assert_eq!(*seen.lock().unwrap(), [[1, 0, 0], [1, 2, 0], [1, 2, 3]]);
    assert!(!lists.iter().any(HostList::running));
}

/// Title memory as the runtime reserves it, write-watched: `len` bytes at
/// the start of a 4 MiB window of their own (a window of loose memory gets
/// one host copy, `shadow::find`). Never freed: the copy outlives the test.
fn watched(len: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };
    const WINDOW: usize = 4 << 20;
    /// Kept per page by the kernel (`GetWriteWatch`).
    const MEM_WRITE_WATCH: u32 = 0x0020_0000;
    // SAFETY: a fresh reservation of our own.
    let base = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            2 * WINDOW,
            MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
            PAGE_READWRITE,
        )
    } as usize;
    assert_ne!(base, 0, "write-watched memory");
    assert!(len <= WINDOW);
    let window = base.next_multiple_of(WINDOW);
    // Touched first, as the title's memory is: the kernel reports a page
    // it materializes on a first read (a refresh) as written.
    // SAFETY: inside the reservation.
    unsafe { (window as *mut u8).write_bytes(0, WINDOW) };
    window
}

/// 64 words of title memory.
fn words_at(at: usize) -> Vec<u32> {
    // SAFETY: 256 bytes of the test's memory.
    (0..64)
        .map(|i| unsafe { std::ptr::read_volatile((at as *const u32).add(i)) })
        .collect()
}

/// What shaders write reaches title memory only after the GPU has run
/// them. A later submission reading it through the same host copy, which
/// the title has not written since, does not wait (VIctory#595's second
/// commit): the GPU gives it the first one's output in queue order.
#[test]
fn shader_writes_are_written_back_after_the_gpu_work() {
    let test = "host::submissions::shader_writes_are_written_back_after_the_gpu_work";
    let _serial = serial();
    let Some(host) = device(test) else { return };
    let kernel = Kernel::new(host);
    // On pages of their own (the title writes `out` before the first runs).
    let window = watched(4 << 20);
    let (scratch, out, copy) = (window, window + (1 << 16), window + (2 << 16));
    for i in 0..64 {
        // SAFETY: the test's memory.
        unsafe { (out as *mut u32).add(i).write_volatile(0xdead_beef) };
    }
    let (first, second) = (host.new_list().unwrap(), host.new_list().unwrap());
    // The first brings `out`'s page whole up to date, as a frame's many
    // draws do: only whole pages are known current in the copy.
    let page = crate::watch::PAGE as u64;
    let window_copy = crate::shadow::find(out as u64, page).unwrap();
    window_copy.refresh(&first, out as u64, page);
    kernel.dispatch(&first, Pass::Fill, scratch as u64, out as u64);
    first.close();
    kernel.dispatch(&second, Pass::Copy, out as u64, copy as u64);
    second.close();
    let mut gate = Gate::close(host);
    host.execute(&[&first]);
    host.execute(&[&second]);
    assert!(first.running() && second.running(), "not waited for");
    assert_eq!(words_at(out), [0xdead_beef; 64], "nothing written back yet");
    gate.open();
    host.drain();
    let want: Vec<u32> = (0..64).map(pattern).collect();
    assert_eq!(words_at(out), want);
    assert_eq!(words_at(copy), want, "the second read the first's output");
}

/// Memory the title wrote since the host copy took it is refreshed only
/// once the submission whose shaders write it has finished: the refresh
/// then brings the title's write together with the shaders'.
#[test]
fn a_refresh_of_memory_the_title_wrote_waits_for_the_shaders_writing_it() {
    let test =
        "host::submissions::a_refresh_of_memory_the_title_wrote_waits_for_the_shaders_writing_it";
    let _serial = serial();
    let Some(host) = device(test) else { return };
    let kernel = Kernel::new(host);
    let window = watched(4 << 20);
    let (scratch, out, copy) = (window, window + (1 << 16), window + (2 << 16));
    let (first, second) = (host.new_list().unwrap(), host.new_list().unwrap());
    kernel.dispatch(&first, Pass::Fill, scratch as u64, out as u64);
    first.close();
    kernel.dispatch(&second, Pass::Copy, out as u64, copy as u64);
    second.close();
    let mut gate = Gate::close(host);
    host.execute(&[&first]);
    // The title writes the page meanwhile: the second's refresh of it must
    // not take title memory before the first's output is in it.
    // SAFETY: the test's memory, past the 64 words the shaders write.
    unsafe { (out as *mut u32).add(100).write_volatile(0x5555_5555) };
    let second = Shared(second);
    let (done, submitted) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            let second = &second;
            host.execute(&[&second.0]);
            let _ = done.send(());
        });
        // Held by the first, which the GPU has not run.
        let waited = submitted.recv_timeout(std::time::Duration::from_millis(300));
        assert!(waited.is_err(), "the second waits for the first");
        gate.open();
        submitted.recv().unwrap();
    });
    host.drain();
    let want: Vec<u32> = (0..64).map(pattern).collect();
    assert_eq!(words_at(copy), want, "the first's output, not stale bytes");
    assert_eq!(words_at(out), want);
    // SAFETY: the test's memory.
    let kept = unsafe { (out as *const u32).add(100).read_volatile() };
    assert_eq!(kept, 0x5555_5555, "the title's write is kept");
}

/// A GPU buffer copy (G3) into memory an earlier submission's shaders
/// write through the same host copy waits for that one: its write-back is
/// told from a snapshot of the copy, which the copy would change. Title
/// memory ends with the copy's bytes, as the console's GPU leaves it.
#[test]
fn a_gpu_copy_over_what_earlier_shaders_write_waits_for_them() {
    let test = "host::submissions::a_gpu_copy_over_what_earlier_shaders_write_waits_for_them";
    let _serial = serial();
    let Some(host) = device(test) else { return };
    let kernel = Kernel::new(host);
    let [scratch, out, source] = [(); 3].map(|()| TitleBuffer::new(256));
    source.set_words(|i| 0x7700_0000 + i as u32);
    let (first, second) = (host.new_list().unwrap(), host.new_list().unwrap());
    kernel.dispatch(&first, Pass::Fill, scratch.address(), out.address());
    first.close();
    assert!(crate::shadow::copy(
        &second,
        out.address(),
        source.address(),
        256
    ));
    second.close();
    let mut gate = Gate::close(host);
    host.execute(&[&first]);
    let second = Shared(second);
    let (done, submitted) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            let second = &second;
            host.execute(&[&second.0]);
            let _ = done.send(());
        });
        let waited = submitted.recv_timeout(std::time::Duration::from_millis(300));
        assert!(waited.is_err(), "the copy waits for the shaders");
        gate.open();
        submitted.recv().unwrap();
    });
    host.drain();
    let want: Vec<u32> = (0..64).map(|i| 0x7700_0000 + i).collect();
    assert_eq!(out.words(), want);
}

/// Three submissions whose shaders write one host copy, the second still
/// in flight when the third is prepared: each tells what its shaders wrote
/// from the copy's bytes before its own GPU work. With one snapshot per
/// copy, the third compared with bytes from before the first, took the
/// zeros its shaders wrote for unchanged and left the title's write in
/// place of them.
#[test]
fn three_submissions_on_one_shadow_compare_with_their_own_snapshots() {
    let test =
        "host::submissions::three_submissions_on_one_shadow_compare_with_their_own_snapshots";
    let _serial = serial();
    let Some(host) = device(test) else { return };
    let kernel = Kernel::new(host);
    // One host copy (the window's), each range on a page of its own; all
    // zeros at first.
    let window = watched(4 << 20);
    let page = crate::watch::PAGE;
    let (a, b, zeros, scratch) = (window, window + page, window + 2 * page, window + 3 * page);
    let lists: Vec<HostList> = (0..3).map(|_| host.new_list().unwrap()).collect();
    kernel.dispatch(&lists[0], Pass::Fill, scratch as u64, a as u64);
    lists[0].close();
    kernel.dispatch(&lists[1], Pass::Fill, scratch as u64, b as u64);
    lists[1].close();
    let mut first_gate = Gate::close(host);
    host.execute(&[&lists[0]]);
    let mut second_gate = Gate::close(host);
    host.execute(&[&lists[1]]);
    // The first runs and is finished; the second stays in flight.
    first_gate.open();
    lists[0].settle();
    assert!(lists[1].running());
    let filled: Vec<u32> = (0..64).map(pattern).collect();
    assert_eq!(words_at(a), filled, "the first's output written back");
    // The title writes `a` again, then the third copies zeros over it.
    for i in 0..64 {
        // SAFETY: the test's memory.
        unsafe { (a as *mut u32).add(i).write_volatile(0x1111_1111) };
    }
    kernel.dispatch(&lists[2], Pass::Copy, zeros as u64, a as u64);
    lists[2].close();
    host.execute(&[&lists[2]]);
    second_gate.open();
    host.drain();
    assert_eq!(
        words_at(a),
        [0; 64],
        "the third's zeros, not the title's write"
    );
    assert_eq!(words_at(b), filled, "the second's output");
}

/// A host list another thread submits.
struct Shared(HostList);

// SAFETY: host lists are used from any thread (the title records and
// submits on many); their state is behind locks.
unsafe impl Sync for Shared {}
// SAFETY: as above.
unsafe impl Send for Shared {}

/// A list reset while its submission runs records into fresh memory: its
/// scratch memory is not reused and its allocator is another, so the
/// running submission reads what it was given. The old memory goes back
/// once that submission has finished.
#[test]
fn a_list_reset_while_it_runs_records_into_fresh_memory() {
    let test = "host::submissions::a_list_reset_while_it_runs_records_into_fresh_memory";
    let _serial = serial();
    let Some(host) = device(test) else { return };
    let list = host.new_list().unwrap();
    let (first, second) = (Box::new(0u64), Box::new(0u64));
    // Records a copy of 256 bytes of `fill` from the list's scratch memory
    // into a buffer the CPU reads.
    let record = |fill: u8| {
        let (gpu, cpu) = list.scratch(256).unwrap();
        // SAFETY: 256 bytes of the list's scratch memory.
        unsafe { cpu.write_bytes(fill, 256) };
        let chunk = list
            .scratch
            .lock()
            .unwrap()
            .chunks
            .last()
            .unwrap()
            .0
            .clone();
        let (out, data) = host.shared_buffer(256).unwrap();
        // SAFETY: an open list; live buffers, ranges inside both.
        unsafe {
            let offset = gpu - chunk.GetGPUVirtualAddress();
            list.list.CopyBufferRegion(&out, 0, &chunk, offset, 256);
        }
        list.keep(&out);
        (cpu as usize, out, data as usize)
    };
    let (scratch_a, _out_a, read_a) = record(0xaa);
    list.defer_write(ptr(&first), Written::Value(7), 8);
    list.close();
    let mut gate = Gate::close(host);
    host.execute(&[&list]);
    assert!(list.running());
    let allocator = list.allocator.lock().unwrap().as_raw();
    list.reset();
    // Its memory went to the running submission.
    assert!(!list.running());
    assert_ne!(list.allocator.lock().unwrap().as_raw(), allocator);
    let (scratch_b, _out_b, read_b) = record(0xbb);
    assert_ne!(scratch_a, scratch_b, "fresh scratch memory");
    list.defer_write(ptr(&second), Written::Value(9), 8);
    list.close();
    host.execute(&[&list]);
    gate.open();
    host.drain();
    // SAFETY: the buffers' mappings, 256 bytes each; the GPU is done.
    let (a, b) = unsafe {
        (
            std::slice::from_raw_parts(read_a as *const u8, 256),
            std::slice::from_raw_parts(read_b as *const u8, 256),
        )
    };
    assert!(
        a.iter().all(|&x| x == 0xaa),
        "the first run read its own memory"
    );
    assert!(b.iter().all(|&x| x == 0xbb));
    assert_eq!((word(&first), word(&second)), (7, 9));
    assert!(!list.running());
}

/// The dynamic descriptor slots of a list reset while it runs stay leased
/// until its submission has finished (the GPU reads its tables until
/// then). After VIctory#595's test by aycow.
#[test]
fn dynamic_descriptors_stay_leased_until_the_submission_finishes() {
    use crate::descriptor::{HeapDesc, create_heap, heap};
    use crate::object::{S_OK, This, guid, slot};
    let test = "host::submissions::dynamic_descriptors_stay_leased_until_the_submission_finishes";
    let _serial = serial();
    let Some(host) = device(test) else { return };
    #[repr(C)]
    struct ExtendedHeapDesc {
        /// Views, 8 of them, shader visible, no node mask.
        words: [u32; 4],
        address: u64,
        size: u64,
    }
    let heap_iid = guid(
        0x8efb471d,
        0x616c,
        0x4f49,
        [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
    );
    let desc = ExtendedHeapDesc {
        words: [0, 8, 1, 0],
        address: 0,
        size: 0,
    };
    let mut title_heap: This = std::ptr::null();
    let made = create_heap(
        std::ptr::null(),
        std::ptr::from_ref(&desc).cast::<HeapDesc>(),
        &heap_iid,
        &mut title_heap,
    );
    assert_eq!(made, S_OK);
    let host_heap = heap(title_heap).host().unwrap();
    // Every dynamic slot, leased by the list.
    let list = host.new_list().unwrap();
    for n in [1 << 16, 1 << 12, 1 << 8, 1 << 4, 1] {
        while let Some(first) = host_heap.dynamic(n) {
            list.keep_dynamic(title_heap as usize, first, n);
        }
    }
    assert_eq!(host_heap.dynamic(1), None);
    list.close();
    let mut gate = Gate::close(host);
    host.execute(&[&list]);
    list.reset();
    assert_eq!(host_heap.dynamic(1), None, "still leased while it runs");
    gate.open();
    host.drain();
    let after = host_heap.dynamic(1).expect("given back once it finished");
    host_heap.release(after, 1);
    let release = slot::<extern "system" fn(This) -> u32>(title_heap, 0x10);
    assert_eq!(release(title_heap), 0);
}

/// A later submission's step reading title memory an earlier one's GPU
/// work writes sees it written, as the console's GPU would order them: by
/// a bottom-of-pipe write, and by shaders.
#[test]
fn a_step_reading_what_an_earlier_submission_writes_sees_it() {
    let test = "host::submissions::a_step_reading_what_an_earlier_submission_writes_sees_it";
    let _serial = serial();
    let Some(host) = device(test) else { return };
    let (first, second) = (host.new_list().unwrap(), host.new_list().unwrap());
    let label = Box::new(0u64);
    first.defer_write(ptr(&label), Written::Value(0x1234_5678_9abc), 8);
    let kernel = Kernel::new(host);
    let (scratch, out) = (TitleBuffer::new(256), TitleBuffer::new(256));
    kernel.dispatch(&first, Pass::Fill, scratch.address(), out.address());
    first.close();
    // The second copies both at submission, on the CPU.
    let copy = Box::new(0u64);
    let words = vec![0u32; 64];
    second.defer_fill(ptr(&copy), ptr(&label), 8);
    second.defer_fill(
        words.as_ptr().cast_mut().cast(),
        out.address() as *const u8,
        256,
    );
    second.close();
    host.execute(&[&first]);
    host.execute(&[&second]);
    assert_eq!(word(&copy), 0x1234_5678_9abc);
    // SAFETY: the vector's own words, filled while the second was
    // submitted.
    let words: Vec<u32> = words
        .iter()
        .map(|w| unsafe { std::ptr::read_volatile(w) })
        .collect();
    assert_eq!(words, (0..64).map(pattern).collect::<Vec<u32>>());
    host.drain();
}

/// A queue's signal of a title fence takes effect once the submission
/// before it has finished: not while the GPU runs it, and after its
/// bottom-of-pipe write.
#[test]
fn title_fences_are_signalled_after_the_work_before_them() {
    use crate::object::{HRESULT, S_OK, This, slot};
    let test = "host::submissions::title_fences_are_signalled_after_the_work_before_them";
    let _serial = serial();
    let Some(host) = device(test) else { return };
    let null = std::ptr::null();
    let mut fence: This = null;
    let fence_iid = &crate::fence::FENCE_IIDS[0];
    assert_eq!(
        crate::fence::create(null, 0, 0, fence_iid, &mut fence),
        S_OK
    );
    let completed = slot::<extern "system" fn(This) -> u64>(fence, 0x40);
    let label = Box::new(0u64);
    let list = host.new_list().unwrap();
    list.defer_write(ptr(&label), Written::Value(3), 8);
    list.close();
    let mut gate = Gate::close(host);
    host.execute(&[&list]);
    assert_eq!(crate::queue::signal(null, fence, 1), S_OK);
    assert_eq!(completed(fence), 0, "not while the GPU runs the work");
    // The title's wait: an event set when the fence gets there.
    let event = EventForTest::new();
    let on_completion =
        slot::<extern "system" fn(This, u64, *mut std::ffi::c_void) -> HRESULT>(fence, 0x48);
    assert_eq!(on_completion(fence, 1, event.0.0), S_OK);
    assert!(!event.set_within(0), "not set before the work has run");
    gate.open();
    // The completion thread sets it, nobody draining.
    assert!(event.set_within(10_000), "set once the work has run");
    assert_eq!(completed(fence), 1);
    assert_eq!(word(&label), 3, "the write before the signal is done");
    host.drain();
    assert_eq!(
        slot::<extern "system" fn(This) -> u32>(fence, 0x10)(fence),
        0
    );
}

/// An auto-reset event for a title fence's wait.
struct EventForTest(HANDLE);

impl EventForTest {
    fn new() -> EventForTest {
        // SAFETY: plain event creation.
        EventForTest(unsafe { CreateEventW(None, false, false, None) }.unwrap())
    }

    /// Whether it is set within `ms` milliseconds.
    fn set_within(&self, ms: u32) -> bool {
        // SAFETY: our event.
        unsafe { WaitForSingleObject(self.0, ms) == windows::Win32::Foundation::WAIT_OBJECT_0 }
    }
}

impl Drop for EventForTest {
    fn drop(&mut self) {
        // SAFETY: our own handle, no longer waited on.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// `XTP_SYNC_SUBMIT`'s path: the submission is finished when it returns,
/// written back and all, with no completion thread needed.
#[test]
fn a_synchronous_submission_is_finished_when_it_returns() {
    let test = "host::submissions::a_synchronous_submission_is_finished_when_it_returns";
    let _serial = serial();
    let Some(host) = device(test) else { return };
    let kernel = Kernel::new(host);
    let (scratch, out) = (TitleBuffer::new(256), TitleBuffer::new(256));
    let list = host.new_list().unwrap();
    let label = Box::new(0u64);
    kernel.dispatch(&list, Pass::Fill, scratch.address(), out.address());
    list.defer_write(ptr(&label), Written::Value(5), 8);
    list.close();
    host.run_as(&[&list], false, true);
    assert!(!list.running());
    assert_eq!(word(&label), 5);
    assert_eq!(out.words(), (0..64).map(pattern).collect::<Vec<u32>>());
}
