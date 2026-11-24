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
#[derive(Clone)]
struct Task {
    context: usize,
    callback: usize,
    owner: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl Task {
    fn new(context: *mut c_void, callback: Callback) -> Task {
        Task {
            context: context as usize,
            callback: callback as usize,
            owner: None,
        }
    }

    /// Keeps runtime-owned context alive through dispatch, or drops it if
    /// submission fails or the queue closes without dispatching the task.
    fn owned<T: Send + Sync + 'static>(context: T, callback: Callback) -> Task {
        let owner = Arc::new(context);
        Task {
            context: Arc::as_ptr(&owner) as usize,
            callback: callback as usize,
            owner: Some(owner),
        }
    }

    fn run(self, canceled: bool) {
        let _owner = self.owner;
        // SAFETY: built from a `Callback` in our constructors.
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

#[derive(Default)]
struct Completion {
    result: Option<(HRESULT, usize)>,
    providers: Vec<std::thread::ThreadId>,
    defer: bool,
    publishing: bool,
    done: bool,
}

/// Active until cleanup or result retrieval. Completion state outlives the
/// provider registration, so waiters can still find it during cleanup.
struct Call {
    provider: Provider,
    context: usize,
    identity: usize,
    done: Mutex<Completion>,
    finished: Condvar,
}

impl Call {
    fn wait(&self) -> HRESULT {
        let done = lock(&self.done);
        let done = self
            .finished
            .wait_while(done, |d| !d.done)
            .unwrap_or_else(|e| e.into_inner());
        done.result.unwrap().0
    }

    /// Records the result (the first one wins), wakes waiters and queues
    /// the completion routine on the block's completion port. A call that
    /// is over already is left alone, and so is `block`: the title may have
    /// freed it, or begun another call on it, since.
    fn complete(&self, block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
        if result == E_PENDING {
            return;
        }
        let mut done = lock(&self.done);
        if done.result.is_some() {
            return;
        }
        done.result = Some((result, required_size));
        // Completing from within a provider remains synchronous when no
        // other thread is using its context. That provider must stop using
        // the block/context after completing, as before. Other providers
        // must return before cleanup can safely run.
        let current = std::thread::current().id();
        done.defer = done.providers.iter().any(|id| *id != current);
        self.publish(block, done);
    }

    fn publish(&self, block: *mut AsyncBlock, mut done: MutexGuard<'_, Completion>) {
        let Some((result, required_size)) = done.result else {
            return;
        };
        if (done.defer && !done.providers.is_empty()) || done.publishing {
            return;
        }
        done.publishing = true;
        drop(done);
        extern "system" fn routine(block: *mut c_void, _canceled: bool) {
            let block = block.cast::<AsyncBlock>();
            // SAFETY: the title keeps the block until its completion
            // routine has run.
            if let Some(routine) = unsafe { (*block).callback } {
                routine(block);
            }
        }
        // Read before publishing: once the status is set, the title may free
        // the block.
        // SAFETY: the call is in flight, so the title's block is valid.
        let (handle, callback) = unsafe { ((*block).queue, (*block).callback) };
        // Own the queue before publishing: the caller may close its handle
        // once it observes completion, before we submit the callback.
        let completion_queue = callback.and_then(|_| queue(handle));
        let cleanup = result < 0 || required_size == 0;
        if cleanup {
            // Transfer visibility atomically, but remove the provider before
            // cleanup. Reentrant provider operations must find no active call.
            let mut registry = lock(&REGISTRY);
            if let Some(call) = registry.remove_active(block, self) {
                registry.completing.insert(block as usize, call);
            }
            drop(registry);
            self.cleanup(block);
        }
        // Begin cannot reuse this address until publication has finished.
        // No callbacks run under the registry or completion mutex.
        let mut registry = lock(&REGISTRY);
        let (status, size) = words(block);
        size.store(required_size, Ordering::Release);
        status.store(result as u32 as usize, Ordering::Release);
        // Publication's last block access. A configured completion routine
        // keeps its existing lifetime guarantee (see `routine` above).
        if cleanup
            && registry
                .completing
                .get(&(block as usize))
                .is_some_and(|c| std::ptr::eq(c.as_ref(), self))
        {
            registry.completing.remove(&(block as usize));
        }
        drop(registry);
        let mut done = lock(&self.done);
        done.done = true;
        self.finished.notify_all();
        drop(done);
        if callback.is_some() {
            let task = Task::new(block.cast(), routine);
            match completion_queue {
                Some(queue) if queue.submit(COMPLETION, 0, task.clone()) == S_OK => {}
                _ => task.run(true),
            }
        }
    }

    fn cleanup(&self, block: *mut AsyncBlock) {
        CLEANING.with(|blocks| blocks.borrow_mut().push(block as usize));
        let _marker = CleanupMarker;
        self.provider(OP_CLEANUP, block, 0, std::ptr::null_mut());
    }

    /// Admit only pending provider operations. A completion racing another
    /// provider thread is published when the last provider returns instead
    /// of cleaning up a context that cancellation might still be using.
    /// A provider requesting deferred completion must return, rather than
    /// block on its own status: publication depends on that return. A
    /// non-waiting query is safe and may still report E_PENDING.
    fn invoke(&self, op: u32, block: *mut AsyncBlock) -> Option<HRESULT> {
        let mut done = lock(&self.done);
        if done.result.is_some() {
            return None;
        }
        let current = std::thread::current().id();
        done.providers.push(current);
        drop(done);
        let hr = self.provider(op, block, 0, std::ptr::null_mut());
        let mut done = lock(&self.done);
        let index = done.providers.iter().position(|id| *id == current).unwrap();
        done.providers.swap_remove(index);
        self.publish(block, done);
        Some(hr)
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

#[derive(Default)]
struct Registry {
    active: HashMap<usize, Arc<Call>>,
    /// Wait-only entries: providers are inaccessible and begin is refused
    /// until cleanup and publication finish. Both maps share one lock.
    completing: HashMap<usize, Arc<Call>>,
}
impl Registry {
    fn remove_active(&mut self, block: *mut AsyncBlock, expected: &Call) -> Option<Arc<Call>> {
        if self
            .active
            .get(&(block as usize))
            .is_some_and(|c| std::ptr::eq(c.as_ref(), expected))
        {
            self.active.remove(&(block as usize))
        } else {
            None
        }
    }
}
static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Default::default);
thread_local! {
    static CLEANING: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}
struct CleanupMarker;
impl Drop for CleanupMarker {
    fn drop(&mut self) {
        CLEANING.with(|blocks| {
            blocks.borrow_mut().pop();
        });
    }
}

fn call(block: *mut AsyncBlock) -> Option<Arc<Call>> {
    lock(&REGISTRY).active.get(&(block as usize)).cloned()
}

fn waiting_call(block: *mut AsyncBlock) -> Option<Arc<Call>> {
    let registry = lock(&REGISTRY);
    registry
        .active
        .get(&(block as usize))
        .or_else(|| registry.completing.get(&(block as usize)))
        .cloned()
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

fn release(block: *mut AsyncBlock, expected: &Call) {
    // Unlocked before the provider runs: it may call back into us.
    let removed = lock(&REGISTRY).remove_active(block, expected);
    if let Some(call) = removed {
        call.cleanup(block);
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
    let mut registry = lock(&REGISTRY);
    if block.is_null()
        || registry.active.contains_key(&(block as usize))
        || registry.completing.contains_key(&(block as usize))
    {
        return Err(E_INVALIDARG);
    }
    let (status, size) = words(block);
    status.store(E_PENDING as u32 as usize, Ordering::Release);
    size.store(0, Ordering::Release);
    let call = Arc::new(Call {
        provider,
        context: context as usize,
        identity: identity as usize,
        done: Mutex::default(),
        finished: Condvar::new(),
    });
    registry.active.insert(block as usize, call.clone());
    drop(registry);
    // A failed begin fails the call, so the completion routine still runs.
    if let Some(hr) = call.invoke(OP_BEGIN, block)
        && hr < 0
    {
        call.complete(block, hr, 0);
    }
    Ok(call)
}

/// Queues the provider's `DoWork` on the block's work port.
pub fn XAsyncSchedule(block: *mut AsyncBlock, delay_ms: u32) -> HRESULT {
    extern "system" fn work(context: *mut c_void, canceled: bool) {
        // SAFETY: Task::owned retains this context for the whole callback.
        let (address, call) = unsafe { &*context.cast::<(usize, Arc<Call>)>() };
        let block = *address as *mut AsyncBlock;
        let hr = if canceled {
            E_ABORT
        } else {
            let Some(hr) = call.invoke(OP_DO_WORK, block) else {
                return;
            };
            hr
        };
        if hr != E_PENDING {
            call.complete(block, hr, 0);
        }
    }
    let Some(call) = call(block) else {
        return E_INVALIDARG;
    };
    let done = lock(&call.done);
    if done.result.is_some() {
        return E_INVALIDARG;
    }
    // SAFETY: a block in flight is valid.
    let handle = unsafe { (*block).queue };
    let queue = queue(handle);
    drop(done);
    match queue {
        Some(queue) => queue.submit(WORK, delay_ms, Task::owned((block as usize, call), work)),
        None => E_INVALIDARG,
    }
}

/// Completes the call on the block; see [`Call::complete`].
pub fn XAsyncComplete(block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    if let Some(call) = call(block) {
        call.complete(block, result, required_size);
    }
}

/// A blocking query stays with one call and waits through cleanup. Cleanup
/// must not join a thread blocking on the same block: the API cannot infer
/// this dependency, which is a provider bug. Such threads must use
/// `wait = false`. A reentrant query of a block undergoing cleanup on this
/// thread returns its current status instead, including in nested cleanup.
pub fn XAsyncGetStatus(block: *mut AsyncBlock, wait: bool) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    if wait && CLEANING.with(|blocks| blocks.borrow().contains(&(block as usize))) {
        return status(block);
    }
    if wait && let Some(call) = waiting_call(block) {
        return call.wait();
    }
    status(block)
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
        if call.invoke(OP_CANCEL, block).is_none() {
            return;
        }
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
    release(block, &call);
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

    /// Isolate deadlock-sensitive tests and verify that the exact child ran.
    /// XTP_XASYNC_TEST is the child-only recursion guard set by this helper.
    fn bounded_child(name: &str) -> bool {
        if std::env::var("XTP_XASYNC_TEST").as_deref() == Ok(name) {
            println!("xasync child: {name}");
            return true;
        }
        let output = tempfile::tempfile().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                &format!("modules::xasync::tests::{name}"),
                "--exact",
                "--nocapture",
            ])
            .env("XTP_XASYNC_TEST", name)
            .stdout(output.try_clone().unwrap())
            .stderr(output.try_clone().unwrap())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        use std::io::{Read, Seek};
        let mut output = output;
        output.rewind().unwrap();
        let mut text = String::new();
        output.read_to_string(&mut text).unwrap();
        assert!(
            text.contains(&format!("xasync child: {name}")),
            "child did not execute: {text}"
        );
        assert!(
            status.is_some_and(|s| s.success()),
            "child failed or exceeded 20s: {text}"
        );
        false
    }

    #[test]
    fn status_waiters_arriving_during_error_cleanup_receive_the_error() {
        if !bounded_child("status_waiters_arriving_during_error_cleanup_receive_the_error") {
            return;
        }
        use std::sync::mpsc::{TryRecvError, sync_channel};
        struct Cleanup {
            entered: std::sync::mpsc::SyncSender<()>,
            resume: Mutex<std::sync::mpsc::Receiver<()>>,
            count: AtomicU32,
            timed_out: AtomicBool,
        }
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            if op == OP_CLEANUP {
                // SAFETY: the test retains the block and context until all threads join.
                let data = unsafe { &*data };
                // SAFETY: context points to the test's Cleanup.
                let cleanup = unsafe { &*data.context.cast::<Cleanup>() };
                cleanup.count.fetch_add(1, Ordering::SeqCst);
                let _ = cleanup.entered.send(());
                if lock(&cleanup.resume)
                    .recv_timeout(Duration::from_secs(10))
                    .is_err()
                {
                    cleanup.timed_out.store(true, Ordering::SeqCst);
                }
            }
            S_OK
        }
        let (entered, cleaning) = sync_channel(1);
        let (resume, permit) = sync_channel(1);
        let cleanup: &Cleanup = Box::leak(Box::new(Cleanup {
            entered,
            resume: Mutex::new(permit),
            count: AtomicU32::new(0),
            timed_out: AtomicBool::new(false),
        }));
        // A failed assertion must not invalidate memory still used by a worker.
        let block: *mut AsyncBlock = Box::leak(Box::new(AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }));
        assert_eq!(
            XAsyncBegin(
                block,
                (cleanup as *const Cleanup).cast_mut().cast(),
                std::ptr::null(),
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        let retained = call(block).unwrap();
        let address = block as usize;
        let completing =
            std::thread::spawn(move || XAsyncComplete(address as *mut AsyncBlock, E_ABORT, 0));
        cleaning
            .recv_timeout(Duration::from_secs(5))
            .expect("completion did not enter cleanup");
        assert!(
            call(block).is_none(),
            "provider registration must be removed before cleanup"
        );
        assert_eq!(
            XAsyncGetStatus(block, false),
            E_PENDING,
            "cleanup must precede publication"
        );
        let mut waiters = Vec::new();
        let mut results = Vec::new();
        for _ in 0..3 {
            let baseline = Arc::strong_count(&retained);
            let (sent, received) = sync_channel(1);
            waiters.push(std::thread::spawn(move || {
                let hr = XAsyncGetStatus(address as *mut AsyncBlock, true);
                sent.send(hr).unwrap();
            }));
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            // An early return proves the old bug; a retained Call proves
            // the query found this call while cleanup is still paused.
            // No timeout or scheduling delay is used as evidence of waiting.
            let early = loop {
                match received.try_recv() {
                    Ok(hr) => break Some(hr),
                    Err(TryRecvError::Empty) if Arc::strong_count(&retained) > baseline => {
                        break None;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(e) => panic!("waiter disconnected: {e}"),
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "waiter neither returned nor retained its call"
                );
                std::thread::yield_now();
            };
            results.push((early, received));
        }
        // Retaining the call must not let these operations reenter
        // the provider while its context is being cleaned up.
        XAsyncComplete(block, S_OK, 8);
        XAsyncCancel(block);
        assert_eq!(XAsyncSchedule(block, 0), E_INVALIDARG);
        resume.send(()).unwrap();
        completing.join().unwrap();
        let results: Vec<_> = results
            .into_iter()
            .map(|(early, received)| {
                early.unwrap_or_else(|| {
                    received
                        .recv_timeout(Duration::from_secs(5))
                        .expect("waiter was not notified")
                })
            })
            .collect();
        for waiter in waiters {
            waiter.join().unwrap();
        }
        assert!(
            !cleanup.timed_out.load(Ordering::SeqCst),
            "cleanup permit timed out"
        );
        assert_eq!(cleanup.count.load(Ordering::SeqCst), 1);
        assert!(waiting_call(block).is_none());
        assert_eq!(
            results, [E_ABORT; 3],
            "waiting queries must receive the final error"
        );
    }

    #[test]
    fn cleanup_can_query_status_and_join_a_nonwaiting_query() {
        if !bounded_child("cleanup_can_query_status_and_join_a_nonwaiting_query") {
            return;
        }
        static NESTED: AtomicBool = AtomicBool::new(false);
        extern "system" fn nested(op: u32, data: *const ProviderData) -> HRESULT {
            if op == OP_CLEANUP {
                // SAFETY: both blocks remain valid until their cleanup returns.
                let data = unsafe { &*data };
                NESTED.store(
                    XAsyncGetStatus(data.block, true) == E_PENDING
                        && XAsyncGetStatus(data.context.cast(), true) == E_PENDING,
                    Ordering::SeqCst,
                );
            }
            S_OK
        }
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            if op == OP_CLEANUP {
                // SAFETY: the test leaks its block and flag until the child exits.
                let data = unsafe { &*data };
                let direct = XAsyncGetStatus(data.block, true);
                let pending = XAsyncGetStatus(data.block, false);
                let inner: *mut AsyncBlock = Box::leak(Box::new(AsyncBlock {
                    queue: std::ptr::null(),
                    context: std::ptr::null_mut(),
                    callback: None,
                    internal: [0; 4],
                }));
                let begun = XAsyncBegin(
                    inner,
                    data.block.cast(),
                    std::ptr::null(),
                    std::ptr::null(),
                    Some(nested),
                );
                XAsyncComplete(inner, E_ABORT, 0);
                let restored = XAsyncGetStatus(data.block, true) == E_PENDING;
                XAsyncComplete(data.block, S_OK, 0);
                XAsyncCancel(data.block);
                let address = data.block as usize;
                let (sent, received) = std::sync::mpsc::sync_channel(1);
                let worker = std::thread::spawn(move || {
                    let block = address as *mut AsyncBlock;
                    XAsyncComplete(block, S_OK, 0);
                    XAsyncCancel(block);
                    let observed = (
                        XAsyncGetStatus(block, false),
                        XAsyncSchedule(block, 0),
                        call(block).is_none(),
                    );
                    let _ = sent.send(observed);
                });
                let observed = received.recv_timeout(Duration::from_secs(5));
                // The subprocess watchdog also bounds join if a regression
                // wedges an operation before the worker can send its result.
                let joined = worker.join().is_ok();
                // SAFETY: context points to the test's leaked AtomicBool.
                unsafe { &*data.context.cast::<AtomicBool>() }.store(
                    direct == E_PENDING
                        && pending == E_PENDING
                        && observed == Ok((E_PENDING, E_INVALIDARG, true))
                        && joined
                        && begun == S_OK
                        && NESTED.load(Ordering::SeqCst)
                        && restored,
                    Ordering::SeqCst,
                );
            }
            S_OK
        }
        let observed: &AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let block: *mut AsyncBlock = Box::leak(Box::new(AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }));
        assert_eq!(
            XAsyncBegin(
                block,
                (observed as *const AtomicBool).cast_mut().cast(),
                std::ptr::null(),
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        XAsyncComplete(block, E_ABORT, 0);
        assert!(
            observed.load(Ordering::SeqCst),
            "cleanup must return pending for a reentrant wait and permit a joined non-waiting query without exposing the provider"
        );
        assert_eq!(XAsyncGetStatus(block, true), E_ABORT);
        assert!(waiting_call(block).is_none());
        assert!(
            CLEANING.with(|blocks| blocks.borrow().is_empty()),
            "nested cleanup markers were not restored"
        );
    }

    #[test]
    fn a_wait_keeps_the_call_reused_by_its_completion_callback() {
        if !bounded_child("a_wait_keeps_the_call_reused_by_its_completion_callback") {
            return;
        }
        static BEGUN: AtomicBool = AtomicBool::new(false);
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            if op == OP_DO_WORK {
                // SAFETY: the runtime passes valid provider data.
                XAsyncComplete(unsafe { (*data).block }, E_ABORT, 0);
            }
            S_OK
        }
        extern "system" fn completed(block: *mut AsyncBlock) {
            // SAFETY: this test retains the block until both calls are over.
            unsafe { (*block).callback = None };
            BEGUN.store(
                XAsyncBegin(
                    block,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    Some(provider),
                ) == S_OK,
                Ordering::SeqCst,
            );
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 3, &mut q), S_OK);
        let block: *mut AsyncBlock = Box::leak(Box::new(AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: Some(completed),
            internal: [0; 4],
        }));
        let old = begin(block, std::ptr::null_mut(), std::ptr::null(), provider).unwrap();
        let old_released = Arc::downgrade(&old);
        let (resume, permit) = std::sync::mpsc::sync_channel(1);
        // Lookup has selected the old call. Force the rest of the wait path
        // to run after its callback has begun the new call at this address.
        let waiter = std::thread::spawn(move || {
            permit
                .recv_timeout(Duration::from_secs(5))
                .expect("reuse callback did not finish");
            old.wait()
        });
        assert_eq!(XAsyncSchedule(block, 0), S_OK);
        assert_eq!(XAsyncSchedule(block, 0), S_OK);
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert!(BEGUN.load(Ordering::SeqCst));
        assert_eq!(XAsyncGetStatus(block, false), E_PENDING);
        assert!(
            XTaskQueueDispatch(q, WORK, 0),
            "old queued work was not dispatched"
        );
        assert_eq!(
            XAsyncGetStatus(block, false),
            E_PENDING,
            "old queued work reached the new call"
        );
        resume.send(()).unwrap();
        assert_eq!(
            waiter.join().unwrap(),
            E_ABORT,
            "the wait followed the new call"
        );
        assert!(
            old_released.upgrade().is_none(),
            "dispatched work retained the old call"
        );
        XAsyncComplete(block, S_OK, 0);
        assert_eq!(XAsyncGetStatus(block, true), S_OK);
        assert!(waiting_call(block).is_none());
        let last = begin(block, std::ptr::null_mut(), std::ptr::null(), provider).unwrap();
        let discarded = Arc::downgrade(&last);
        assert_eq!(XAsyncSchedule(block, 0), S_OK);
        last.complete(block, E_ABORT, 0);
        drop(last);
        assert!(
            discarded.upgrade().is_some(),
            "queued work must retain its context"
        );
        XTaskQueueCloseHandle(q);
        assert!(
            discarded.upgrade().is_none(),
            "closing the queue leaked undispatched work"
        );
    }

    #[test]
    fn completion_waits_for_an_active_cancel_provider() {
        if !bounded_child("completion_waits_for_an_active_cancel_provider") {
            return;
        }
        use std::sync::mpsc::sync_channel;
        struct Cancel {
            entered: std::sync::mpsc::SyncSender<()>,
            resume: Mutex<std::sync::mpsc::Receiver<()>>,
            cleanups: AtomicU32,
            valid: AtomicBool,
            requests: AtomicU32,
            deferred: AtomicBool,
        }
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the test keeps data's context alive through both joins.
            let data = unsafe { &*data };
            // SAFETY: context points to the test's Cancel.
            let cancel = unsafe { &*data.context.cast::<Cancel>() };
            if op == OP_CANCEL {
                if cancel.requests.fetch_add(1, Ordering::SeqCst) != 0 {
                    XAsyncComplete(data.block, E_ABORT, 0);
                    // Do not block here: this invocation is one of the
                    // providers whose return allows publication.
                    cancel.deferred.store(
                        XAsyncGetStatus(data.block, false) == E_PENDING,
                        Ordering::SeqCst,
                    );
                    return S_OK;
                }
                let _ = cancel.entered.send(());
                let resumed = lock(&cancel.resume)
                    .recv_timeout(Duration::from_secs(10))
                    .is_ok();
                cancel.valid.store(
                    resumed && cancel.cleanups.load(Ordering::SeqCst) == 0,
                    Ordering::SeqCst,
                );
                // Supported reentrancy: a provider may complete its own call.
                XAsyncComplete(data.block, S_OK, 0);
            } else if op == OP_CLEANUP {
                cancel.cleanups.fetch_add(1, Ordering::SeqCst);
                XAsyncComplete(data.block, S_OK, 0);
                XAsyncCancel(data.block);
            }
            S_OK
        }
        let (entered, waiting) = sync_channel(1);
        let (resume, permit) = sync_channel(1);
        let cancel: &Cancel = Box::leak(Box::new(Cancel {
            entered,
            resume: Mutex::new(permit),
            cleanups: AtomicU32::new(0),
            valid: AtomicBool::new(false),
            requests: AtomicU32::new(0),
            deferred: AtomicBool::new(false),
        }));
        let block: *mut AsyncBlock = Box::leak(Box::new(AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }));
        assert_eq!(
            XAsyncBegin(
                block,
                (cancel as *const Cancel).cast_mut().cast(),
                std::ptr::null(),
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        let address = block as usize;
        let cancelling = std::thread::spawn(move || XAsyncCancel(address as *mut AsyncBlock));
        waiting
            .recv_timeout(Duration::from_secs(5))
            .expect("cancel provider did not start");
        let completing = std::thread::spawn(move || XAsyncCancel(address as *mut AsyncBlock));
        completing.join().unwrap();
        assert!(
            cancel.deferred.load(Ordering::SeqCst),
            "a provider's non-waiting query must observe deferred completion without forming a cycle"
        );
        assert_eq!(XAsyncGetStatus(block, false), E_PENDING);
        assert_eq!(cancel.cleanups.load(Ordering::SeqCst), 0);
        resume.send(()).unwrap();
        cancelling.join().unwrap();
        assert!(
            cancel.valid.load(Ordering::SeqCst),
            "cleanup overlapped the cancel provider or the permit timed out"
        );
        assert_eq!(cancel.cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(
            XAsyncGetStatus(block, true),
            E_ABORT,
            "first completion wins"
        );
        assert!(waiting_call(block).is_none());
    }

    #[test]
    fn completion_retains_its_queue_before_publishing_status() {
        if !bounded_child("completion_retains_its_queue_before_publishing_status") {
            return;
        }
        struct Context {
            queue: std::sync::Weak<Queue>,
            retained: AtomicBool,
            completed: AtomicU32,
        }
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            if op == OP_CLEANUP {
                // SAFETY: the test retains its block and context through completion.
                let data = unsafe { &*data };
                // SAFETY: context points to the test's leaked Context.
                let context = unsafe { &*data.context.cast::<Context>() };
                let retained = context.queue.strong_count() >= 2;
                context.retained.store(retained, Ordering::SeqCst);
                // Close the caller's reference only when runtime ownership is
                // established. The original implementation fails safely below,
                // rather than incrementing an Arc through a dangling handle.
                if retained {
                    // SAFETY: the block is still pending and valid during cleanup.
                    XTaskQueueCloseHandle(unsafe { (*data.block).queue });
                }
            }
            S_OK
        }
        extern "system" fn completed(block: *mut AsyncBlock) {
            // SAFETY: the test keeps both objects alive until this callback returns.
            let context = unsafe { &*(*block).context.cast::<Context>() };
            context.completed.fetch_add(1, Ordering::SeqCst);
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 3, &mut q), S_OK);
        let weak = Arc::downgrade(&queue(q).unwrap());
        let context: &Context = Box::leak(Box::new(Context {
            queue: weak,
            retained: AtomicBool::new(false),
            completed: AtomicU32::new(0),
        }));
        let pointer = (context as *const Context).cast_mut().cast();
        let block: *mut AsyncBlock = Box::leak(Box::new(AsyncBlock {
            queue: q,
            context: pointer,
            callback: Some(completed),
            internal: [0; 4],
        }));
        assert_eq!(
            XAsyncBegin(
                block,
                pointer,
                std::ptr::null(),
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        XAsyncComplete(block, E_ABORT, 0);
        let retained = context.retained.load(Ordering::SeqCst);
        if !retained {
            XTaskQueueCloseHandle(q);
        }
        assert_eq!(context.completed.load(Ordering::SeqCst), 1);
        assert!(
            context.queue.upgrade().is_none(),
            "completion leaked its queue reference"
        );
        assert!(
            retained,
            "completion must own the queue before cleanup and status publication"
        );
    }

    #[test]
    fn zero_size_completion_can_free_the_block_in_its_callback() {
        if !bounded_child("zero_size_completion_can_free_the_block_in_its_callback") {
            return;
        }
        struct Counts {
            cleanups: AtomicU32,
            callbacks: AtomicU32,
            inline: AtomicBool,
            work: AtomicU32,
        }
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid data; Counts lives until begin returns.
            let data = unsafe { &*data };
            if op == OP_BEGIN {
                XAsyncSchedule(data.block, 0);
                XAsyncComplete(data.block, S_OK, 0);
                // SAFETY: Counts is retained by the test even after
                // the immediate callback has freed the block.
                let counts = unsafe { &*data.context.cast::<Counts>() };
                counts.inline.store(
                    counts.callbacks.load(Ordering::SeqCst) == 1,
                    Ordering::SeqCst,
                );
                XAsyncComplete(data.block, E_ABORT, 8);
            } else if op == OP_DO_WORK {
                // SAFETY: Counts is retained by this test until dispatch ends.
                unsafe { &*data.context.cast::<Counts>() }
                    .work
                    .fetch_add(1, Ordering::SeqCst);
            } else if op == OP_CLEANUP {
                // SAFETY: the context points to this test's Counts.
                unsafe { &*data.context.cast::<Counts>() }
                    .cleanups
                    .fetch_add(1, Ordering::SeqCst);
                XAsyncComplete(data.block, E_ABORT, 0);
                XAsyncCancel(data.block);
            }
            S_OK
        }
        extern "system" fn completed(block: *mut AsyncBlock) {
            // SAFETY: the test allocated this block; this is its only callback.
            let block = unsafe { Box::from_raw(block) };
            // SAFETY: the test retains Counts until this immediate callback returns.
            let counts = unsafe { &*block.context.cast::<Counts>() };
            let mut size = usize::MAX;
            let mut used = usize::MAX;
            let block_ptr = (&*block as *const AsyncBlock).cast_mut();
            let valid = XAsyncGetStatus(block_ptr, true) == S_OK
                && XAsyncGetResultSize(block_ptr, &mut size) == S_OK
                && size == 0
                && XAsyncGetResult(
                    block_ptr,
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    &mut used,
                ) == S_OK
                && used == 0
                && counts.cleanups.load(Ordering::SeqCst) == 1;
            counts
                .callbacks
                .fetch_add(u32::from(valid), Ordering::SeqCst);
        }
        let counts = Counts {
            cleanups: AtomicU32::new(0),
            callbacks: AtomicU32::new(0),
            inline: AtomicBool::new(false),
            work: AtomicU32::new(0),
        };
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 3, &mut q), S_OK);
        let block: *mut AsyncBlock = Box::leak(Box::new(AsyncBlock {
            queue: q,
            context: (&raw const counts).cast_mut().cast(),
            callback: Some(completed),
            internal: [0; 4],
        }));
        let old = begin(
            block,
            (&raw const counts).cast_mut().cast(),
            std::ptr::null(),
            provider,
        )
        .unwrap();
        assert_eq!(counts.callbacks.load(Ordering::SeqCst), 1);
        assert!(
            counts.inline.load(Ordering::SeqCst),
            "immediate completion must remain reentrant"
        );
        old.complete(block, E_ABORT, 0); // The address is now freed: no dereference is allowed.
        assert_eq!(old.wait(), S_OK);
        assert!(old.invoke(OP_CANCEL, block).is_none());
        assert_eq!(counts.cleanups.load(Ordering::SeqCst), 1);
        assert!(call(block).is_none());
        assert!(
            XTaskQueueDispatch(q, WORK, 0),
            "old work was not dispatched"
        );
        assert_eq!(
            counts.work.load(Ordering::SeqCst),
            0,
            "old work invoked a provider after the block was freed"
        );
        XTaskQueueCloseHandle(q);
    }

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
        if !bounded_child("a_cancelled_pending_call_leaves_the_reused_block_alone") {
            return;
        }
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
            wait_first
                .recv_timeout(long)
                .expect("first worker was not released");
            Ok(b"first!".to_vec())
        })
        .expect("begun");
        assert_eq!(Arc::strong_count(&slot), 3, "us, the call, the worker");
        let first_call = call(block).unwrap();
        assert_eq!(XAsyncGetStatus(block, false), E_PENDING);

        XAsyncCancel(block);
        assert_eq!(XAsyncGetStatus(block, false), E_ABORT);
        assert!(call(block).is_none(), "cleanup ran: the call is over");
        assert_eq!(Arc::strong_count(&slot), 2, "the worker still holds it");
        assert!(XTaskQueueDispatch(q, COMPLETION, 0));
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 1);

        let (go_second, wait_second) = channel::<()>();
        let hr = pending_with(block, second, move || {
            wait_second
                .recv_timeout(long)
                .expect("second worker was not released");
            Ok(b"second".to_vec())
        });
        assert_eq!(hr, S_OK);
        first_call.complete(block, E_ABORT, 0);
        assert_eq!(first_call.wait(), E_ABORT);
        release(block, &first_call);
        assert_eq!(XAsyncGetStatus(block, false), E_PENDING);

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
        assert!(XTaskQueueDispatch(q, COMPLETION, 5_000));
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
