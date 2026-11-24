//! XTaskQueue and XAsync: the runtime's callback queues and async-call
//! blocks, served as the task-queue provider's object from `QueryApiImpl`.
//!
//! As documented for the public API: a queue has a work port and a
//! completion port, each dispatched manually (the title calls dispatch),
//! on a thread pool, serialised on one thread, or immediately on the
//! submitting thread. A null queue handle means the process queue. An
//! async block's system area holds its status and result size, so both
//! stay readable after our state for the call is gone.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use super::xgameruntime::{Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing};

/// The task-queue provider; it is also the interface id titles ask for.
pub const PROVIDER: Guid = Guid {
    data1: 0x073b_7dcb,
    data2: 0x1fcf,
    data3: 0x4030,
    data4: [0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28],
};

pub const E_PENDING: HRESULT = 0x8000_000A_u32 as i32;
pub const E_ABORT: HRESULT = 0x8000_4004_u32 as i32;
pub const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
pub const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
pub const E_NOT_SUFFICIENT_BUFFER: HRESULT = 0x8007_007A_u32 as i32;
pub const E_ILLEGAL_METHOD_CALL: HRESULT = 0x8000_000E_u32 as i32;

const WORK: u32 = 0;
const COMPLETION: u32 = 1;
const INFINITE: u32 = u32::MAX;

type Callback = extern "system" fn(*mut c_void, bool);
type MonitorCallback = extern "system" fn(*mut c_void, *const Queue, u32);
type TerminatedCallback = extern "system" fn(*mut c_void);
type CompletionRoutine = extern "system" fn(*mut AsyncBlock);
type Provider = extern "system" fn(u32, *const ProviderData) -> HRESULT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Manual,
    ThreadPool,
    Serialized,
    Immediate,
}

impl Mode {
    fn from_raw(mode: u32) -> Option<Mode> {
        [
            Mode::Manual,
            Mode::ThreadPool,
            Mode::Serialized,
            Mode::Immediate,
        ]
        .get(mode as usize)
        .copied()
    }
}

/// A title callback and its context (addresses, so tasks can cross threads).
#[derive(Clone, Copy)]
struct Task {
    context: usize,
    callback: usize,
}

impl Task {
    fn new(context: *mut c_void, callback: Callback) -> Task {
        Task {
            context: context as usize,
            callback: callback as usize,
        }
    }

    fn run(self, canceled: bool) {
        // SAFETY: built from a `Callback` in `new`.
        let callback: Callback = unsafe { std::mem::transmute(self.callback) };
        callback(self.context as *mut c_void, canceled);
    }
}

struct Port {
    mode: Mode,
    tasks: Mutex<VecDeque<Task>>,
    ready: Condvar,
}

impl Port {
    fn new(mode: Mode) -> Arc<Port> {
        let port = Arc::new(Port {
            mode,
            tasks: Mutex::default(),
            ready: Condvar::new(),
        });
        if mode == Mode::Serialized {
            // The worker ends once it holds the last reference (every queue
            // using the port is closed) and nothing is left to run.
            let worker = port.clone();
            std::thread::spawn(move || {
                loop {
                    match worker.take(1000, || false) {
                        Some(task) => task.run(false),
                        None if Arc::strong_count(&worker) == 1 => break,
                        None => {}
                    }
                }
            });
        }
        port
    }

    fn submit(&self, task: Task) {
        match self.mode {
            Mode::Immediate => task.run(false),
            Mode::ThreadPool => POOL.submit(task),
            Mode::Manual | Mode::Serialized => {
                lock(&self.tasks).push_back(task);
                self.ready.notify_one();
            }
        }
    }

    /// The next queued task, waiting up to `timeout_ms`, or until `ended`
    /// (checked whenever the port is woken) says no task will come.
    fn take(&self, timeout_ms: u32, ended: impl Fn() -> bool) -> Option<Task> {
        let waiting = |t: &mut VecDeque<Task>| t.is_empty() && !ended();
        let tasks = lock(&self.tasks);
        let mut tasks = if timeout_ms == INFINITE {
            self.ready
                .wait_while(tasks, waiting)
                .unwrap_or_else(|e| e.into_inner())
        } else {
            let timeout = Duration::from_millis(timeout_ms.into());
            self.ready
                .wait_timeout_while(tasks, timeout, waiting)
                .unwrap_or_else(|e| e.into_inner())
                .0
        };
        tasks.pop_front()
    }
}

/// The thread pool behind thread-pool ports: idle workers are reused, and a
/// new one starts whenever more tasks wait than workers idle, so a callback
/// that blocks on another never starves it. Workers stay for reuse.
struct Pool {
    /// Queued tasks and the number of idle workers.
    state: Mutex<(VecDeque<Task>, usize)>,
    ready: Condvar,
}

static POOL: Pool = Pool {
    state: Mutex::new((VecDeque::new(), 0)),
    ready: Condvar::new(),
};

impl Pool {
    fn submit(&'static self, task: Task) {
        let mut state = lock(&self.state);
        state.0.push_back(task);
        if state.0.len() > state.1 {
            std::thread::spawn(move || self.work());
        } else {
            self.ready.notify_one();
        }
    }

    fn work(&self) {
        let mut state = lock(&self.state);
        loop {
            while state.0.is_empty() {
                state.1 += 1;
                state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
                state.1 -= 1;
            }
            let task = state.0.pop_front();
            drop(state);
            if let Some(task) = task {
                task.run(false);
            }
            state = lock(&self.state);
        }
    }
}

/// A queue handle is `Arc::into_raw` of this; duplicating and closing a
/// handle add and drop a reference.
pub struct Queue {
    ports: [Arc<Port>; 2],
    monitors: Mutex<Vec<(u64, usize, usize)>>,
    terminated: AtomicBool,
}

impl Queue {
    fn new(ports: [Arc<Port>; 2]) -> Arc<Queue> {
        Arc::new(Queue {
            ports,
            monitors: Mutex::default(),
            terminated: AtomicBool::new(false),
        })
    }

    fn submit(self: &Arc<Self>, port: u32, delay_ms: u32, task: Task) -> HRESULT {
        if port > COMPLETION {
            return E_INVALIDARG;
        }
        if self.terminated.load(Ordering::Acquire) {
            return E_ABORT;
        }
        if delay_ms == 0 {
            self.deliver(port, task);
        } else {
            let queue = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay_ms.into()));
                queue.deliver(port, task);
            });
        }
        S_OK
    }

    fn deliver(self: &Arc<Self>, port: u32, task: Task) {
        self.ports[port as usize].submit(task);
        // Copied out: a monitor may unregister itself.
        let monitors = lock(&self.monitors).clone();
        for (_, context, callback) in monitors {
            // SAFETY: stored from a `MonitorCallback` in `register_monitor`.
            let callback: MonitorCallback = unsafe { std::mem::transmute(callback) };
            callback(context as *mut c_void, Arc::as_ptr(self), port);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The process queue: `None` until first used (then a thread-pool queue),
/// `Some(None)` once the title clears it.
static PROCESS_QUEUE: Mutex<Option<Option<Arc<Queue>>>> = Mutex::new(None);

fn process_queue() -> Option<Arc<Queue>> {
    lock(&PROCESS_QUEUE)
        .get_or_insert_with(|| {
            Some(Queue::new([
                Port::new(Mode::ThreadPool),
                Port::new(Mode::ThreadPool),
            ]))
        })
        .clone()
}

/// A new reference to the queue behind `handle` (null: the process queue).
fn queue(handle: *const Queue) -> Option<Arc<Queue>> {
    if handle.is_null() {
        return process_queue();
    }
    // SAFETY: a handle we returned, which the title has not closed.
    unsafe {
        Arc::increment_strong_count(handle);
        Some(Arc::from_raw(handle))
    }
}

pub fn XTaskQueueCreate(work: u32, completion: u32, out: *mut *const Queue) -> HRESULT {
    let (Some(work), Some(completion)) = (Mode::from_raw(work), Mode::from_raw(completion)) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let queue = Queue::new([Port::new(work), Port::new(completion)]);
    // SAFETY: checked non-null; the title passes room for a handle.
    unsafe { out.write(Arc::into_raw(queue)) };
    S_OK
}

/// A port handle points at the port, which its queue keeps alive.
pub fn XTaskQueueGetPort(handle: *const Queue, port: u32, out: *mut *const c_void) -> HRESULT {
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    if port > COMPLETION || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::as_ptr(&queue.ports[port as usize]).cast()) };
    S_OK
}

pub fn XTaskQueueCreateComposite(
    work: *const c_void,
    completion: *const c_void,
    out: *mut *const Queue,
) -> HRESULT {
    if work.is_null() || completion.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let port = |p: *const c_void| {
        // SAFETY: a port handle from `XTaskQueueGetPort`, whose queue is open.
        unsafe {
            Arc::increment_strong_count(p.cast::<Port>());
            Arc::from_raw(p.cast::<Port>())
        }
    };
    let queue = Queue::new([port(work), port(completion)]);
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::into_raw(queue)) };
    S_OK
}

pub fn XTaskQueueDuplicateHandle(handle: *const Queue, out: *mut *const Queue) -> HRESULT {
    match queue(handle) {
        Some(queue) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(Arc::into_raw(queue)) };
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

pub fn XTaskQueueCloseHandle(handle: *const Queue) {
    if !handle.is_null() {
        // SAFETY: a handle we returned; this drops its reference.
        unsafe { Arc::decrement_strong_count(handle) };
    }
}

/// Runs one queued callback of a manual port; false if none came in time,
/// or none can come: the queue was terminated (which ends a wait under way,
/// as a dispatcher thread's loop expects).
pub fn XTaskQueueDispatch(handle: *const Queue, port: u32, timeout_ms: u32) -> bool {
    let Some(queue) = queue(handle) else {
        return false;
    };
    let Some(port) = queue.ports.get(port as usize) else {
        return false;
    };
    if port.mode != Mode::Manual {
        return false;
    }
    match port.take(timeout_ms, || queue.terminated.load(Ordering::Acquire)) {
        Some(task) => {
            task.run(false);
            true
        }
        None => false,
    }
}

pub fn XTaskQueueSubmitDelayedCallback(
    handle: *const Queue,
    port: u32,
    delay_ms: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    match (queue(handle), callback) {
        (Some(queue), Some(callback)) => queue.submit(port, delay_ms, Task::new(context, callback)),
        _ => E_INVALIDARG,
    }
}

/// Cancels what is still queued (callbacks see `canceled`), refuses new
/// work, then calls `callback`.
pub fn XTaskQueueTerminate(
    handle: *const Queue,
    _wait: bool,
    context: *mut c_void,
    callback: Option<TerminatedCallback>,
) -> HRESULT {
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    queue.terminated.store(true, Ordering::Release);
    for port in &queue.ports {
        let pending: Vec<Task> = lock(&port.tasks).drain(..).collect();
        // Dispatches waiting on this queue end: nothing more will come.
        // (Those of other queues on the port see theirs is open and wait
        // on.) After the lock above, so none misses both flag and wake.
        port.ready.notify_all();
        for task in pending {
            task.run(true);
        }
    }
    if let Some(callback) = callback {
        callback(context);
    }
    S_OK
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub fn XTaskQueueRegisterMonitor(
    handle: *const Queue,
    context: *mut c_void,
    callback: Option<MonitorCallback>,
    token: *mut u64,
) -> HRESULT {
    let (Some(queue), Some(callback)) = (queue(handle), callback) else {
        return E_INVALIDARG;
    };
    if token.is_null() {
        return E_INVALIDARG;
    }
    let id = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    lock(&queue.monitors).push((id, context as usize, callback as usize));
    // SAFETY: checked non-null.
    unsafe { token.write(id) };
    S_OK
}

pub fn XTaskQueueUnregisterMonitor(handle: *const Queue, token: u64) {
    if let Some(queue) = queue(handle) {
        lock(&queue.monitors).retain(|m| m.0 != token);
    }
}

/// A new handle to the process queue, if there is one.
pub fn XTaskQueueGetCurrentProcessTaskQueue(out: *mut *const Queue) -> bool {
    let found = process_queue();
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(found.clone().map_or(std::ptr::null(), Arc::into_raw)) };
    }
    found.is_some()
}

pub fn XTaskQueueSetCurrentProcessTaskQueue(handle: *const Queue) {
    let new = if handle.is_null() {
        None
    } else {
        queue(handle)
    };
    *lock(&PROCESS_QUEUE) = Some(new);
}

/// An async block: what the title fills in, then the system area.
#[repr(C)]
pub struct AsyncBlock {
    pub queue: *const Queue,
    pub context: *mut c_void,
    pub callback: Option<CompletionRoutine>,
    /// Ours: [unused, status, result size, unused].
    pub internal: [usize; 4],
}

#[repr(C)]
pub struct ProviderData {
    pub block: *mut AsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

const OP_BEGIN: u32 = 0;
const OP_DO_WORK: u32 = 1;
const OP_GET_RESULT: u32 = 2;
const OP_CANCEL: u32 = 3;
const OP_CLEANUP: u32 = 4;

/// One call in flight, until its result is taken or it fails. Until `done`,
/// it is the one `CALLS` holds for its block (only completing it, with
/// `done` locked, or taking its result, once done, removes it from there).
struct Call {
    provider: Provider,
    context: usize,
    identity: usize,
    done: Mutex<bool>,
}

/// Status publication, including after a call has left `CALLS` during cleanup.
static STATUS_CHANGED: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

impl Call {
    /// Records the result (the first one wins), wakes waiters and queues
    /// the completion routine on the block's completion port. A call that
    /// is over already is left alone, and so is `block`: the title may have
    /// freed it, or begun another call on it, since.
    fn complete(&self, block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
        extern "system" fn routine(block: *mut c_void, _canceled: bool) {
            let block = block.cast::<AsyncBlock>();
            // SAFETY: the title keeps the block until its completion
            // routine has run.
            if let Some(routine) = unsafe { (*block).callback } {
                routine(block);
            }
        }
        if result == E_PENDING {
            return;
        }
        let mut done = lock(&self.done);
        if *done {
            return;
        }
        // Read before publishing: once the status is set, the title may free
        // the block.
        // SAFETY: the call is in flight, so the title's block is valid.
        let (handle, callback) = unsafe { ((*block).queue, (*block).callback) };
        let (status, size) = words(block);
        if result < 0 || required_size == 0 {
            release(block);
        }
        let changed = lock(&STATUS_CHANGED.0);
        size.store(required_size, Ordering::Release);
        status.store(result as u32 as usize, Ordering::Release);
        *done = true;
        STATUS_CHANGED.1.notify_all();
        drop(changed);
        drop(done);
        if callback.is_some() {
            let task = Task::new(block.cast(), routine);
            match queue(handle) {
                Some(queue) if queue.submit(COMPLETION, 0, task) == S_OK => {}
                _ => task.run(true),
            }
        }
    }

    fn provider(
        &self,
        op: u32,
        block: *mut AsyncBlock,
        size: usize,
        buffer: *mut c_void,
    ) -> HRESULT {
        let data = ProviderData {
            block,
            buffer_size: size,
            buffer,
            context: self.context as *mut c_void,
        };
        (self.provider)(op, &data)
    }
}

/// Calls by block address.
static CALLS: LazyLock<Mutex<HashMap<usize, Arc<Call>>>> = LazyLock::new(Default::default);

fn call(block: *mut AsyncBlock) -> Option<Arc<Call>> {
    lock(&CALLS).get(&(block as usize)).cloned()
}

/// The block's status and result-size words.
fn words(block: *mut AsyncBlock) -> (&'static AtomicUsize, &'static AtomicUsize) {
    // SAFETY: the title's block outlives the call; its system area is ours.
    unsafe {
        let internal = std::ptr::addr_of_mut!((*block).internal).cast::<AtomicUsize>();
        (&*internal.add(1), &*internal.add(2))
    }
}

fn status(block: *mut AsyncBlock) -> HRESULT {
    words(block).0.load(Ordering::Acquire) as u32 as i32
}

fn release(block: *mut AsyncBlock) {
    // Unlocked before the provider runs: it may call back into us.
    let removed = lock(&CALLS).remove(&(block as usize));
    if let Some(call) = removed {
        call.provider(OP_CLEANUP, block, 0, std::ptr::null_mut());
    }
}

pub fn XAsyncBegin(
    block: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    _identity_name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    match provider.map(|provider| begin(block, context, identity, provider)) {
        Some(Ok(_)) => S_OK,
        Some(Err(hr)) => hr,
        None => E_INVALIDARG,
    }
}

/// Begins a call on `block` and returns it, for a caller that completes it
/// later: through the call, so that once it is over (cancelled, say) the
/// block and whatever call it holds by then are left alone.
fn begin(
    block: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    provider: Provider,
) -> Result<Arc<Call>, HRESULT> {
    if block.is_null() || call(block).is_some() {
        return Err(E_INVALIDARG);
    }
    let (status, size) = words(block);
    status.store(E_PENDING as u32 as usize, Ordering::Release);
    size.store(0, Ordering::Release);
    let call = Arc::new(Call {
        provider,
        context: context as usize,
        identity: identity as usize,
        done: Mutex::new(false),
    });
    lock(&CALLS).insert(block as usize, call.clone());
    // A failed begin fails the call, so the completion routine still runs.
    let hr = call.provider(OP_BEGIN, block, 0, std::ptr::null_mut());
    if hr < 0 {
        call.complete(block, hr, 0);
    }
    Ok(call)
}

/// Queues the provider's `DoWork` on the block's work port.
pub fn XAsyncSchedule(block: *mut AsyncBlock, delay_ms: u32) -> HRESULT {
    extern "system" fn work(block: *mut c_void, canceled: bool) {
        let block = block.cast::<AsyncBlock>();
        let Some(call) = call(block) else { return };
        let hr = if canceled {
            E_ABORT
        } else {
            call.provider(OP_DO_WORK, block, 0, std::ptr::null_mut())
        };
        if hr != E_PENDING {
            XAsyncComplete(block, hr, 0);
        }
    }
    if block.is_null() || call(block).is_none() {
        return E_INVALIDARG;
    }
    // SAFETY: a block in flight is valid.
    let handle = unsafe { (*block).queue };
    match queue(handle) {
        Some(queue) => queue.submit(WORK, delay_ms, Task::new(block.cast(), work)),
        None => E_INVALIDARG,
    }
}

/// Completes the call on the block; see [`Call::complete`].
pub fn XAsyncComplete(block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    if let Some(call) = call(block) {
        call.complete(block, result, required_size);
    }
}

pub fn XAsyncGetStatus(block: *mut AsyncBlock, wait: bool) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    if !wait {
        return status(block);
    }
    let mut changed = lock(&STATUS_CHANGED.0);
    loop {
        let result = status(block);
        if result != E_PENDING {
            return result;
        }
        // The status lock also covers calls removed during cleanup, so the
        // next publication cannot be missed between this check and the wait.
        changed = STATUS_CHANGED
            .1
            .wait(changed)
            .unwrap_or_else(|e| e.into_inner());
    }
}

pub fn XAsyncGetResultSize(block: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    if block.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    // Any nonnegative completion carries a result, not just S_OK: S_FALSE
    // is a success code too and a provider may return data with it.
    if hr >= 0 {
        // SAFETY: checked non-null.
        unsafe { out.write(words(block).1.load(Ordering::Acquire)) };
    }
    hr
}

/// Asks the provider to cancel; the call fails with `E_ABORT` unless the
/// provider completed it first.
pub fn XAsyncCancel(block: *mut AsyncBlock) {
    if let Some(call) = call(block) {
        call.provider(OP_CANCEL, block, 0, std::ptr::null_mut());
        // This call: if the provider completed it, the title may have begun
        // another on the block since.
        call.complete(block, E_ABORT, 0);
    }
}

/// Has the provider write the result into `buffer`, then ends the call.
pub fn XAsyncGetResult(
    block: *mut AsyncBlock,
    identity: *const c_void,
    buffer_size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    let size = words(block).1.load(Ordering::Acquire);
    if hr < 0 || size == 0 {
        if hr >= 0 && !used.is_null() {
            // SAFETY: checked non-null.
            unsafe { used.write(0) };
        }
        return hr;
    }
    let Some(call) = call(block) else {
        return E_ILLEGAL_METHOD_CALL;
    };
    if call.identity != identity as usize {
        return E_INVALIDARG;
    }
    if buffer_size < size || buffer.is_null() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    let hr = call.provider(OP_GET_RESULT, block, buffer_size, buffer);
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(size) };
    }
    release(block);
    hr
}

/// Starts a call on `block` that is already over: it completes with `hr`
/// and no result data, through the block's completion port as usual.
pub fn finished(block: *mut AsyncBlock, identity: *const c_void, hr: HRESULT) -> HRESULT {
    finished_with(
        block,
        identity,
        if hr < 0 { Err(hr) } else { Ok(Vec::new()) },
    )
}

/// Like [`finished`], with the bytes `XAsyncGetResult` hands out for
/// `identity` (an error has none).
pub fn finished_with(
    block: *mut AsyncBlock,
    identity: *const c_void,
    result: Result<Vec<u8>, HRESULT>,
) -> HRESULT {
    type Data = Result<Vec<u8>, HRESULT>;
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; the context is the
        // boxed result until cleanup frees it.
        let data = unsafe { &*data };
        let result = data.context.cast::<Data>();
        match op {
            // SAFETY: boxed in `finished_with`; nothing runs after cleanup.
            OP_CLEANUP => drop(unsafe { Box::from_raw(result) }),
            OP_BEGIN => {
                // SAFETY: as above. Copied out: completing may clean up.
                let (hr, len) = match unsafe { &*result } {
                    Ok(bytes) => (S_OK, bytes.len()),
                    Err(hr) => (*hr, 0),
                };
                XAsyncComplete(data.block, hr, len);
            }
            // SAFETY: as above; the runtime checked the buffer's size.
            OP_GET_RESULT => unsafe {
                if let Ok(bytes) = &*result {
                    data.buffer
                        .cast::<u8>()
                        .copy_from(bytes.as_ptr(), bytes.len());
                }
            },
            _ => {}
        }
        S_OK
    }
    let context = Box::into_raw(Box::new(result));
    let hr = XAsyncBegin(
        block,
        context.cast(),
        identity,
        std::ptr::null(),
        Some(provider),
    );
    if hr < 0 {
        // Refused before the call existed, so no cleanup will come.
        // SAFETY: still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// Like [`finished_with`], but the result comes from `work`, run on a
/// thread of ours; the call stays pending until it returns (a dialog, say).
/// If the title cancels the call first, it is over at once: `work` still
/// runs to its end, but what it returns is dropped, and neither the block
/// nor a call the title has begun on it since is touched.
pub fn pending_with(
    block: *mut AsyncBlock,
    identity: *const c_void,
    work: impl FnOnce() -> Result<Vec<u8>, HRESULT> + Send + 'static,
) -> HRESULT {
    match pending(block, identity, work) {
        Ok(_) => S_OK,
        Err(hr) => hr,
    }
}

/// Where the result of a [`pending_with`] call waits for `XAsyncGetResult`.
/// The call (its provider context) and its worker thread each hold a
/// reference, since either may end first: the call when it is cancelled,
/// the worker when its work is done.
type Slot = Mutex<Vec<u8>>;

/// [`pending_with`], returning the call's slot.
fn pending(
    block: *mut AsyncBlock,
    identity: *const c_void,
    work: impl FnOnce() -> Result<Vec<u8>, HRESULT> + Send + 'static,
) -> Result<Arc<Slot>, HRESULT> {
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; the context is the
        // call's reference to its slot (`Arc::into_raw` in `pending`), which
        // only cleanup, the call's last operation, gives up.
        let data = unsafe { &*data };
        let slot = data.context.cast_const().cast::<Slot>();
        match op {
            // SAFETY: as above, once; the worker holds a reference of its own.
            OP_CLEANUP => drop(unsafe { Arc::from_raw(slot) }),
            // SAFETY: as above; the runtime checked the buffer's size against
            // the length the worker completed the call with, after it had
            // put the bytes in the slot.
            OP_GET_RESULT => unsafe {
                let bytes = lock(&*slot);
                data.buffer
                    .cast::<u8>()
                    .copy_from(bytes.as_ptr(), bytes.len());
            },
            _ => {}
        }
        S_OK
    }
    let slot = Arc::new(Slot::default());
    let context = Arc::into_raw(slot.clone());
    let call = match begin(block, context.cast_mut().cast(), identity, provider) {
        Ok(call) => call,
        Err(hr) => {
            // SAFETY: refused before the call existed; the reference is still
            // ours.
            drop(unsafe { Arc::from_raw(context) });
            return Err(hr);
        }
    };
    let (block, result) = (block as usize, slot.clone());
    std::thread::spawn(move || {
        let (hr, len) = match work() {
            Ok(bytes) => {
                let len = bytes.len();
                // Our reference: the slot lives even if the call is over.
                *lock(&result) = bytes;
                (S_OK, len)
            }
            Err(hr) => (hr, 0),
        };
        // Through the call, not the block: a cancelled call is over, so this
        // late result is dropped without touching the block, which the title
        // may have freed or begun another call on.
        call.complete(block as *mut AsyncBlock, hr, len);
    });
    Ok(slot)
}

/// The result of a call begun with [`finished_with`] for `identity`, as a
/// `T` written through `out`.
pub fn result<T>(block: *mut AsyncBlock, identity: *const c_void, out: *mut T) -> HRESULT {
    XAsyncGetResult(
        block,
        identity,
        size_of::<T>(),
        out.cast(),
        std::ptr::null_mut(),
    )
}

/// The bytes of a value, for [`finished_with`].
pub fn bytes<T: Copy>(value: T) -> Vec<u8> {
    // SAFETY: a `Copy` value is plain data; padding bytes are never read.
    unsafe { std::slice::from_raw_parts((&raw const value).cast::<u8>(), size_of::<T>()) }.to_vec()
}

type Work = extern "system" fn(*mut AsyncBlock) -> HRESULT;

/// Runs `work` on the block's work port as an async call of its own; its
/// return value is the call's result.
pub fn XAsyncRun(block: *mut AsyncBlock, work: Option<Work>) -> HRESULT {
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                // SAFETY: the context is the `Work` given to `XAsyncRun`.
                let work = unsafe { std::mem::transmute::<*mut c_void, Work>(data.context) };
                work(data.block)
            }
            _ => S_OK,
        }
    }
    let Some(work) = work else {
        return E_INVALIDARG;
    };
    let identity = XAsyncRun as *const c_void;
    XAsyncBegin(
        block,
        work as *mut c_void,
        identity,
        std::ptr::null(),
        Some(provider),
    )
}

// The object `QueryApiImpl` returns. Each method takes the object first,
// then the API's arguments; slot offsets as titles call them.

pub static OBJECT: Object = Object::new("task queue", &VTABLE);

/// Its interface id is the provider's.
pub const INTERFACES: &[Interface] = &[(PROVIDER, PROVIDER, &OBJECT)];

type This = *const Object;
extern "system" fn m_get_status(_: This, b: *mut AsyncBlock, wait: u8) -> HRESULT {
    XAsyncGetStatus(b, wait != 0)
}
extern "system" fn m_get_result_size(_: This, b: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    XAsyncGetResultSize(b, out)
}
extern "system" fn m_cancel(_: This, b: *mut AsyncBlock) {
    XAsyncCancel(b)
}
extern "system" fn m_run(_: This, b: *mut AsyncBlock, work: Option<Work>) -> HRESULT {
    XAsyncRun(b, work)
}
extern "system" fn m_begin(
    _: This,
    b: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    XAsyncBegin(b, context, identity, name, provider)
}
extern "system" fn m_schedule(_: This, b: *mut AsyncBlock, delay: u32) -> HRESULT {
    XAsyncSchedule(b, delay)
}
extern "system" fn m_complete(_: This, b: *mut AsyncBlock, hr: HRESULT, size: usize) {
    XAsyncComplete(b, hr, size)
}
extern "system" fn m_get_result(
    _: This,
    b: *mut AsyncBlock,
    identity: *const c_void,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    XAsyncGetResult(b, identity, size, buffer, used)
}
extern "system" fn m_create(
    _: This,
    work: u32,
    completion: u32,
    out: *mut *const Queue,
) -> HRESULT {
    XTaskQueueCreate(work, completion, out)
}
extern "system" fn m_create_composite(
    _: This,
    work: *const c_void,
    completion: *const c_void,
    out: *mut *const Queue,
) -> HRESULT {
    XTaskQueueCreateComposite(work, completion, out)
}
extern "system" fn m_get_port(
    _: This,
    q: *const Queue,
    port: u32,
    out: *mut *const c_void,
) -> HRESULT {
    XTaskQueueGetPort(q, port, out)
}
extern "system" fn m_duplicate(_: This, q: *const Queue, out: *mut *const Queue) -> HRESULT {
    XTaskQueueDuplicateHandle(q, out)
}
extern "system" fn m_dispatch(_: This, q: *const Queue, port: u32, timeout: u32) -> bool {
    XTaskQueueDispatch(q, port, timeout)
}
extern "system" fn m_close(_: This, q: *const Queue) {
    XTaskQueueCloseHandle(q)
}
extern "system" fn m_submit(
    _: This,
    q: *const Queue,
    port: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    XTaskQueueSubmitDelayedCallback(q, port, 0, context, callback)
}
extern "system" fn m_submit_delayed(
    _: This,
    q: *const Queue,
    port: u32,
    delay: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    XTaskQueueSubmitDelayedCallback(q, port, delay, context, callback)
}
extern "system" fn m_terminate(
    _: This,
    q: *const Queue,
    wait: u8,
    context: *mut c_void,
    callback: Option<TerminatedCallback>,
) -> HRESULT {
    XTaskQueueTerminate(q, wait != 0, context, callback)
}
extern "system" fn m_register_monitor(
    _: This,
    q: *const Queue,
    context: *mut c_void,
    callback: Option<MonitorCallback>,
    token: *mut u64,
) -> HRESULT {
    XTaskQueueRegisterMonitor(q, context, callback, token)
}
extern "system" fn m_unregister_monitor(_: This, q: *const Queue, token: u64) {
    XTaskQueueUnregisterMonitor(q, token)
}
extern "system" fn m_get_process_queue(_: This, out: *mut *const Queue) -> bool {
    XTaskQueueGetCurrentProcessTaskQueue(out)
}
extern "system" fn m_set_process_queue(_: This, q: *const Queue) {
    XTaskQueueSetCurrentProcessTaskQueue(q)
}
/// The runtime's debug check that the caller is not a time-critical thread,
/// which every runtime call in a title makes first.
extern "system" fn m_check_thread(_: This) {}

static VTABLE: Methods<31> = Methods([
    missing::<0> as *const (),         // 0x00 QueryInterface
    add_ref as *const (),              // 0x08 AddRef
    add_ref as *const (),              // 0x10 Release: the object is static
    m_get_status as *const (),         // 0x18
    m_get_result_size as *const (),    // 0x20
    m_cancel as *const (),             // 0x28
    m_run as *const (),                // 0x30
    m_begin as *const (),              // 0x38
    missing::<8> as *const (),         // 0x40
    m_schedule as *const (),           // 0x48
    m_complete as *const (),           // 0x50
    m_get_result as *const (),         // 0x58
    m_create as *const (),             // 0x60
    m_create_composite as *const (),   // 0x68
    m_get_port as *const (),           // 0x70
    m_duplicate as *const (),          // 0x78
    m_dispatch as *const (),           // 0x80
    m_close as *const (),              // 0x88
    m_submit as *const (),             // 0x90
    m_submit_delayed as *const (),     // 0x98
    missing::<20> as *const (),        // 0xa0 register a wait handle
    missing::<21> as *const (),        // 0xa8 unregister it
    m_terminate as *const (),          // 0xb0
    m_register_monitor as *const (),   // 0xb8
    m_unregister_monitor as *const (), // 0xc0
    m_get_process_queue as *const (),  // 0xc8
    m_set_process_queue as *const (),  // 0xd0
    missing::<27> as *const (),        // 0xd8 thread time-sensitivity
    missing::<28> as *const (),        // 0xe0
    m_check_thread as *const (),       // 0xe8
    missing::<30> as *const (),        // 0xf0
]);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn slot<F: Copy>(offset: usize) -> F {
        crate::modules::xgameruntime::method(&OBJECT, offset)
    }

    static RAN: AtomicU32 = AtomicU32::new(0);
    extern "system" fn count(context: *mut c_void, canceled: bool) {
        RAN.fetch_add(
            if canceled { 100 } else { context as u32 },
            Ordering::SeqCst,
        );
    }

    #[test]
    fn manual_ports_run_callbacks_when_dispatched() {
        type Create = extern "system" fn(This, u32, u32, *mut *const Queue) -> HRESULT;
        type Submit = extern "system" fn(This, *const Queue, u32, *mut c_void, Callback) -> HRESULT;
        type Dispatch = extern "system" fn(This, *const Queue, u32, u32) -> bool;
        type Close = extern "system" fn(This, *const Queue);
        let this: This = &OBJECT;
        let mut q = std::ptr::null();
        assert_eq!(slot::<Create>(0x60)(this, 0, 0, &mut q), S_OK);
        assert_eq!(
            slot::<Submit>(0x90)(this, q, COMPLETION, 5 as *mut c_void, count),
            S_OK
        );
        assert_eq!(RAN.load(Ordering::SeqCst), 0, "manual: nothing runs yet");
        assert!(
            !slot::<Dispatch>(0x80)(this, q, WORK, 0),
            "work port is empty"
        );
        assert!(slot::<Dispatch>(0x80)(this, q, COMPLETION, 0));
        assert_eq!(RAN.load(Ordering::SeqCst), 5);
        assert!(!slot::<Dispatch>(0x80)(this, q, COMPLETION, 0));
        slot::<Close>(0x88)(this, q);
    }

    /// A provider that schedules its work and returns a u64 result.
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                XAsyncComplete(data.block, S_OK, 8);
                S_OK
            }
            OP_GET_RESULT => {
                // SAFETY: at least 8 bytes, checked by the runtime.
                unsafe { data.buffer.cast::<u64>().write(data.context as u64) };
                S_OK
            }
            _ => S_OK,
        }
    }

    static COMPLETED: AtomicU32 = AtomicU32::new(0);
    extern "system" fn completed(_: *mut AsyncBlock) {
        COMPLETED.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn async_calls_complete_and_return_results() {
        let mut q = std::ptr::null();
        // Work on a thread pool, completion dispatched by hand.
        assert_eq!(XTaskQueueCreate(1, 0, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: Some(completed),
            internal: [0; 4],
        };
        let identity = 0x1d as *const c_void;
        assert_eq!(
            XAsyncBegin(
                &mut block,
                42 as *mut c_void,
                identity,
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, true), S_OK);
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_OK);
        assert_eq!(size, 8);
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 0, "completion is manual");
        assert!(XTaskQueueDispatch(q, COMPLETION, INFINITE));
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 1);

        let mut value = 0u64;
        let mut small = 0u32;
        let out = (&mut small as *mut u32).cast();
        assert_eq!(
            XAsyncGetResult(&mut block, identity, 4, out, std::ptr::null_mut()),
            E_NOT_SUFFICIENT_BUFFER
        );
        let out = (&mut value as *mut u64).cast();
        let mut used = 0;
        assert_eq!(
            XAsyncGetResult(&mut block, identity, 8, out, &mut used),
            S_OK
        );
        assert_eq!((value, used), (42, 8));
        assert!(call(&mut block).is_none(), "the call ends with its result");
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn a_nonnegative_completion_still_returns_its_payload() {
        const S_FALSE: HRESULT = 1;
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            match op {
                OP_BEGIN => XAsyncSchedule(data.block, 0),
                OP_DO_WORK => {
                    XAsyncComplete(data.block, S_FALSE, 8);
                    S_OK
                }
                OP_GET_RESULT => {
                    // SAFETY: the caller checked eight bytes.
                    unsafe { data.buffer.cast::<u64>().write(0x1234) };
                    S_OK
                }
                _ => S_OK,
            }
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let identity = 0x77 as *const c_void;
        assert_eq!(
            XAsyncBegin(
                &mut block,
                std::ptr::null_mut(),
                identity,
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert_eq!(XAsyncGetStatus(&mut block, false), S_FALSE);
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_FALSE);
        assert_eq!(size, 8);
        let mut value = 0u64;
        let mut used = 0;
        assert_eq!(
            XAsyncGetResult(
                &mut block,
                identity,
                8,
                (&mut value as *mut u64).cast(),
                &mut used
            ),
            S_OK
        );
        assert_eq!((value, used), (0x1234, 8));
        assert!(
            call(&raw mut block).is_none(),
            "the call ends with its result"
        );
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn run_returns_the_work_result() {
        extern "system" fn work(_: *mut AsyncBlock) -> HRESULT {
            E_ABORT
        }
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        assert_eq!(
            slot::<extern "system" fn(This, *mut AsyncBlock, Option<Work>) -> HRESULT>(0x30)(
                &OBJECT,
                &mut block,
                Some(work)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, true), E_ABORT);
    }

    #[test]
    fn a_status_wait_outlasts_provider_cleanup() {
        use std::sync::mpsc::{Receiver, Sender, channel};
        type Cleanup = (Sender<()>, Receiver<()>);
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            match op {
                OP_BEGIN => XAsyncSchedule(data.block, 0),
                OP_DO_WORK => E_ABORT,
                OP_CLEANUP => {
                    // SAFETY: the test's channel pair outlives this worker.
                    let (started, release) = unsafe { &*data.context.cast::<Cleanup>() };
                    started.send(()).unwrap();
                    let _ = release.recv_timeout(Duration::from_secs(10));
                    S_OK
                }
                _ => S_OK,
            }
        }
        let long = Duration::from_secs(10);
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let cleanup: Cleanup = (started_tx, release_rx);
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        // Leaked on failure so a late worker cannot dereference a freed block.
        let block = Box::leak(Box::new(AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }));
        assert_eq!(
            XAsyncBegin(
                block,
                (&raw const cleanup).cast_mut().cast(),
                0x88 as *const c_void,
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        let queue = q as usize;
        let (work_tx, work_rx) = channel();
        std::thread::spawn(move || {
            let ran = XTaskQueueDispatch(queue as *const Queue, WORK, 0);
            work_tx.send(ran).unwrap();
        });
        started_rx
            .recv_timeout(long)
            .expect("provider cleanup started");
        let (status_tx, status_rx) = channel();
        let address = block as *mut AsyncBlock as usize;
        std::thread::spawn(move || {
            status_tx
                .send(XAsyncGetStatus(address as *mut AsyncBlock, true))
                .unwrap();
        });
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            release_tx.send(()).unwrap();
        });
        let result = status_rx
            .recv_timeout(long)
            .expect("status waiter finished");
        assert!(
            work_rx
                .recv_timeout(long)
                .expect("work dispatcher finished")
        );
        release.join().unwrap();
        XTaskQueueCloseHandle(q);
        assert_eq!(result, E_ABORT);
    }

    #[test]
    fn pool_callbacks_may_wait_for_each_other() {
        static SECOND: AtomicBool = AtomicBool::new(false);
        static FIRST: AtomicBool = AtomicBool::new(false);
        extern "system" fn waits(_: *mut c_void, _: bool) {
            let start = std::time::Instant::now();
            while !SECOND.load(Ordering::SeqCst) && start.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            FIRST.store(SECOND.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        extern "system" fn releases(_: *mut c_void, _: bool) {
            SECOND.store(true, Ordering::SeqCst);
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(1, 1, &mut q), S_OK);
        let none = std::ptr::null_mut();
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, none, Some(waits)),
            S_OK
        );
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, none, Some(releases)),
            S_OK
        );
        let start = std::time::Instant::now();
        while !FIRST.load(Ordering::SeqCst) && start.elapsed().as_secs() < 10 {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(FIRST.load(Ordering::SeqCst));
        XTaskQueueCloseHandle(q);
    }

    /// A dispatcher thread's loop, `while (XTaskQueueDispatch(queue, port,
    /// INFINITE));`, ends when its queue is terminated: nothing can be
    /// submitted to it any more, so nothing else would ever wake it.
    #[test]
    fn terminate_ends_a_waiting_dispatch() {
        static RAN: AtomicU32 = AtomicU32::new(0);
        extern "system" fn ran(_: *mut c_void, _: bool) {
            RAN.fetch_add(1, Ordering::SeqCst);
        }
        /// Dispatches `queue`'s work until a dispatch returns false, then
        /// says so.
        fn dispatcher(queue: *const Queue) -> std::sync::mpsc::Receiver<()> {
            let (done, ended) = std::sync::mpsc::channel();
            let queue = queue as usize;
            std::thread::spawn(move || {
                while XTaskQueueDispatch(queue as *const Queue, WORK, INFINITE) {}
                let _ = done.send(());
            });
            ended
        }
        let run_one = |queue, total| {
            let none = std::ptr::null_mut();
            assert_eq!(
                XTaskQueueSubmitDelayedCallback(queue, WORK, 0, none, Some(ran)),
                S_OK
            );
            for _ in 0..500 {
                if RAN.load(Ordering::SeqCst) == total {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("callback {total} did not run");
        };
        let (long, short) = (Duration::from_secs(5), Duration::from_millis(200));
        let none = std::ptr::null_mut();

        // Two queues on the same manual ports.
        let (mut a, mut b) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueCreate(0, 0, &mut a), S_OK);
        let (mut work, mut completion) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueGetPort(a, WORK, &mut work), S_OK);
        assert_eq!(XTaskQueueGetPort(a, COMPLETION, &mut completion), S_OK);
        assert_eq!(XTaskQueueCreateComposite(work, completion, &mut b), S_OK);

        let ended = dispatcher(a);
        run_one(a, 1);
        // The other queue's end is not this one's: the wait goes on and
        // still runs what comes in.
        assert_eq!(XTaskQueueTerminate(b, false, none, None), S_OK);
        assert!(
            ended.recv_timeout(short).is_err(),
            "ended with another queue"
        );
        run_one(a, 2);
        // Its own queue's end is.
        assert_eq!(XTaskQueueTerminate(a, false, none, None), S_OK);
        assert!(
            ended.recv_timeout(long).is_ok(),
            "still waiting on a terminated queue"
        );
        // And a dispatch that starts after the end returns at once.
        assert!(!XTaskQueueDispatch(a, WORK, INFINITE));
        XTaskQueueCloseHandle(a);
        XTaskQueueCloseHandle(b);
    }

    /// Sends on its channel when dropped: kept in a thread-local of a
    /// worker thread, it says the thread has ended, its last act included.
    struct Exit(std::sync::mpsc::Sender<()>);
    impl Drop for Exit {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    thread_local! {
        static EXIT: std::cell::RefCell<Option<Exit>> = const { std::cell::RefCell::new(None) };
    }

    /// The title cancels a `pending_with` call whose work is still running
    /// (a dialog on screen), and its cleanup runs; the title then begins a
    /// second call on the same block. When the first work ends at last, its
    /// result must reach neither the block nor the second call, and go only
    /// to memory still alive. (Before the fix, cleanup freed the state the
    /// worker then wrote to, and the late result completed the second call.)
    #[test]
    fn a_cancelled_pending_call_leaves_the_reused_block_alone() {
        use std::sync::mpsc::channel;
        static COMPLETED: AtomicU32 = AtomicU32::new(0);
        extern "system" fn completed(_: *mut AsyncBlock) {
            COMPLETED.fetch_add(1, Ordering::SeqCst);
        }
        // Unlike values: the linker could fold like ones into one address.
        static FIRST: u8 = 1;
        static SECOND: u8 = 2;
        let (first, second) = ((&raw const FIRST).cast(), (&raw const SECOND).cast());
        let long = Duration::from_secs(10);
        let mut q = std::ptr::null();
        // Completion dispatched by hand, to see each one.
        assert_eq!(XTaskQueueCreate(1, 0, &mut q), S_OK);
        // Leaked: a worker left blocked by a failed assertion keeps a valid
        // block.
        let block: *mut AsyncBlock = Box::leak(Box::new(AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: Some(completed),
            internal: [0; 4],
        }));

        let (go_first, wait_first) = channel::<()>();
        let (exited, first_ended) = channel();
        // The counter: who holds the first call's slot (we keep a reference
        // to look at it).
        let slot = pending(block, first, move || {
            EXIT.with(|e| *e.borrow_mut() = Some(Exit(exited)));
            let _ = wait_first.recv();
            Ok(b"first!".to_vec())
        })
        .expect("begun");
        assert_eq!(Arc::strong_count(&slot), 3, "us, the call, the worker");
        assert_eq!(XAsyncGetStatus(block, false), E_PENDING);

        XAsyncCancel(block);
        assert_eq!(XAsyncGetStatus(block, false), E_ABORT);
        assert!(call(block).is_none(), "cleanup ran: the call is over");
        assert_eq!(Arc::strong_count(&slot), 2, "the worker still holds it");
        assert!(XTaskQueueDispatch(q, COMPLETION, 0));
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 1);

        let (go_second, wait_second) = channel::<()>();
        let hr = pending_with(block, second, move || {
            let _ = wait_second.recv();
            Ok(b"second".to_vec())
        });
        assert_eq!(hr, S_OK);

        // The first work ends; its thread is gone once `first_ended` hears.
        go_first.send(()).unwrap();
        first_ended
            .recv_timeout(long)
            .expect("the first worker ended");
        // It wrote its result into its own slot, alive until it let go.
        assert_eq!(Arc::strong_count(&slot), 1, "the worker let go");
        assert_eq!(*lock(&slot), b"first!");
        assert_eq!(
            XAsyncGetStatus(block, false),
            E_PENDING,
            "the first call's late result completed the second"
        );
        assert!(
            !XTaskQueueDispatch(q, COMPLETION, 0),
            "a completion was queued for the second call"
        );
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 1);

        // The canary: the second call's result is its own. (Its state was
        // made just after the first one's cleanup, where a state freed then
        // would have been, and the first worker wrote after that.)
        go_second.send(()).unwrap();
        assert_eq!(XAsyncGetStatus(block, true), S_OK);
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(block, &mut size), S_OK);
        assert_eq!(size, 6);
        let (mut out, mut used) = ([0u8; 6], 0);
        let buffer = out.as_mut_ptr().cast();
        assert_eq!(XAsyncGetResult(block, second, 6, buffer, &mut used), S_OK);
        assert_eq!((&out, used), (b"second", 6));
        assert!(XTaskQueueDispatch(q, COMPLETION, 0));
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 2);
        assert!(call(block).is_none(), "the call ends with its result");
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn terminate_cancels_what_is_queued() {
        static TERMINATED: AtomicBool = AtomicBool::new(false);
        extern "system" fn terminated(_: *mut c_void) {
            TERMINATED.store(true, Ordering::SeqCst);
        }
        extern "system" fn note(context: *mut c_void, canceled: bool) {
            // SAFETY: the test passes a pointer to its flag.
            unsafe { *context.cast::<bool>() = canceled };
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut canceled = false;
        let flag = (&mut canceled as *mut bool).cast();
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, flag, Some(note)),
            S_OK
        );
        assert_eq!(
            XTaskQueueTerminate(q, true, std::ptr::null_mut(), Some(terminated)),
            S_OK
        );
        assert!(canceled && TERMINATED.load(Ordering::SeqCst));
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, flag, Some(note)),
            E_ABORT
        );
        XTaskQueueCloseHandle(q);
    }
}
