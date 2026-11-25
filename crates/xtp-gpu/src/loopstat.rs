//! The loop-limit counter (S12), behind `XTP_SHADER_LOOP_COUNT=1`
//! ([`xtp_shader::emit::loop_count`], off by default): how many lanes of
//! each pipeline leave a loop at XTP's limit or budget, per frame.
//!
//! A translated shader that can stop a lane at a loop's limit (the loop
//! limit, a spin wait's, a counted loop's) or at the loops' budget adds the
//! lanes it stops, with an atomic, to the first dword of a root UAV, `u0` in
//! [`LOOP_COUNTER_SPACE`] ([`HostShader::loop_counter`]). A host pipeline
//! with such a shader gets a slot here ([`slot`]), named as host pipelines
//! are (by their shaders' hashes, as the timestamps' programs), and its
//! draws and dispatches bind the root UAV at that slot's counter
//! ([`address`]): `base + slot × 16` in one buffer of [`SLOTS`] counters,
//! in system memory the CPU maps. A root UAV is a raw view, whose address
//! D3D12 wants 16-byte aligned: so each counter takes [`STRIDE`] bytes,
//! its first dword alone counting.
//!
//! The present's list copies the counters into a snapshot of its own
//! ([`record`]), after every list the frame submitted to the host queue,
//! the one queue there is. Once that copy has finished, the snapshot is
//! read, and what each counter gained since the frame before is that
//! frame's ([`presented`]): work of the next frame, which may already be
//! counting on the live buffer then, does not count in it. Every few
//! seconds, with the frame line (`XTP_FRAME_LINE`), a line sums the frames
//! since the last one up and names the pipelines that stopped the most
//! lanes ([`line`]). With `xtp_gpu::loopstat=debug`, a line for each frame
//! where a lane stopped.
//!
//! shortcut: the buffer is in the common state, promoted to unordered
//! access wherever a list binds it and to a copy source in the present's
//! list; the GPU's atomics on it are the only writes.
//!
//! [`HostShader::loop_counter`]: xtp_shader::emit::HostShader::loop_counter
//! [`LOOP_COUNTER_SPACE`]: xtp_shader::emit::LOOP_COUNTER_SPACE

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::Win32::Graphics::Direct3D12::{
    D3D12_RAW_UAV_SRV_BYTE_ALIGNMENT, ID3D12GraphicsCommandList, ID3D12Resource,
};

/// Counters in the buffer: one per counting pipeline (1 MiB).
pub(crate) const SLOTS: u32 = 1 << 16;

/// Bytes per counter: the alignment D3D12 wants of a raw view's address
/// (`D3D12_RAW_UAV_SRV_BYTE_ALIGNMENT`), which a root UAV is. The shader
/// adds to the first dword; the other three stay 0.
const STRIDE: u64 = D3D12_RAW_UAV_SRV_BYTE_ALIGNMENT as u64;

/// The buffer's bytes, and a snapshot's.
const BYTES: u64 = SLOTS as u64 * STRIDE;

/// The slot of every pipeline past [`SLOTS`], counted together.
const SHARED: u32 = 0;

/// Pipelines named on the line.
const TOP: usize = 10;

/// Where slot `slot`'s counter starts, in the buffer and in a snapshot.
fn offset(slot: u32) -> u64 {
    u64::from(slot.min(SLOTS - 1)) * STRIDE
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The counters: the buffer, its GPU address and its mapping.
struct Buffer {
    /// Kept alive for good: lists bind it without keeping it.
    resource: ID3D12Resource,
    gpu: u64,
    /// Its mapping, which tests read (frames read a snapshot).
    #[cfg(test)]
    data: *const u8,
}

// SAFETY: a D3D12 resource is free-threaded; `data` is a persistent mapping
// of it, only read by tests (the GPU's atomics write it).
unsafe impl Send for Buffer {}
// SAFETY: as above.
unsafe impl Sync for Buffer {}

static BUFFER: OnceLock<Option<Buffer>> = OnceLock::new();

/// The buffer, made on first use once there is a host device (`None` if it
/// could not be made, logged once).
fn buffer() -> Option<&'static Buffer> {
    if let Some(made) = BUFFER.get() {
        return made.as_ref();
    }
    let host = crate::host::made()?;
    BUFFER
        .get_or_init(|| {
            let Some((resource, data)) = host.shared_buffer(BYTES) else {
                tracing::warn!("no loop-limit counter: its buffer was not made");
                return None;
            };
            // SAFETY: the mapping covers the buffer, which no list has used.
            unsafe { std::ptr::write_bytes(data, 0, BYTES as usize) };
            crate::host::name(&resource, || "loop-limit counters".into());
            // SAFETY: a plain query of a live buffer.
            let gpu = unsafe { resource.GetGPUVirtualAddress() };
            tracing::info!(
                slots = SLOTS,
                "loop-limit counters on (XTP_SHADER_LOOP_COUNT): lanes stopped at a loop's limit or budget, per pipeline"
            );
            Some(Buffer {
                resource,
                gpu,
                #[cfg(test)]
                data: data.cast_const(),
            })
        })
        .as_ref()
}

/// The slots' names, the counts last read and the sums since the last line.
struct Counts {
    /// By slot: the pipeline's name (slot [`SHARED`]: those past the slots).
    names: Vec<String>,
    slots: HashMap<String, u32>,
    /// By slot: its counter when last read.
    last: Vec<u32>,
    sums: Sums,
}

static COUNTS: Mutex<Option<Counts>> = Mutex::new(None);

fn counts<T>(f: impl FnOnce(&mut Counts) -> T) -> T {
    let mut counts = lock(&COUNTS);
    f(counts.get_or_insert_with(|| Counts {
        names: vec!["pipelines past the counter's slots".to_owned()],
        slots: HashMap::new(),
        last: Vec::new(),
        sums: Sums::default(),
    }))
}

/// The counter slot of the pipeline named `name` (its shaders' hashes, as
/// `HostPipeline::name`), the same for every pipeline of that name; the
/// shared slot once [`SLOTS`] are taken. `None` without a buffer: the
/// pipeline cannot run its counting shaders then.
pub(crate) fn slot(name: &str) -> Option<u32> {
    buffer()?;
    Some(counts(|c| {
        if let Some(&slot) = c.slots.get(name) {
            return slot;
        }
        let slot = c.names.len() as u32;
        if slot >= SLOTS {
            if slot == SLOTS {
                tracing::warn!(
                    slots = SLOTS,
                    "loop-limit counters: every slot taken; later pipelines count together"
                );
                // Warned once: the next ones find the name list longer.
                c.names.push(String::new());
            }
            return SHARED;
        }
        c.names.push(name.to_owned());
        c.slots.insert(name.to_owned(), slot);
        slot
    }))
}

/// The GPU address a pipeline's draws and dispatches bind as its counter
/// (`u0` in [`xtp_shader::emit::LOOP_COUNTER_SPACE`]): its slot's, 16-byte
/// aligned as a raw view's must be.
pub(crate) fn address(slot: u32) -> Option<u64> {
    Some(buffer()?.gpu + offset(slot))
}

/// Slot `slot`'s counter as it is now in `data`, a mapping of the buffer or
/// of a snapshot.
fn read_slot(data: *const u8, slot: u32) -> u32 {
    // SAFETY: a counter of a mapped buffer of [`BYTES`], 16-byte aligned;
    // the GPU's atomics, or a finished copy, write it.
    unsafe {
        data.add(offset(slot) as usize)
            .cast::<u32>()
            .read_volatile()
    }
}

/// A copy of the counters' first `slots` slots, made by a list: what they
/// were once the lists before it on the host queue had run.
pub(crate) struct Snapshot {
    resource: ID3D12Resource,
    data: *const u8,
    /// The slots copied: those named when the copy was recorded.
    slots: u32,
}

// SAFETY: a D3D12 resource is free-threaded; `data` is a persistent mapping
// of it, read once its copy has finished.
unsafe impl Send for Snapshot {}

/// Snapshots read already, for the next presents (as many as frames were
/// in flight at most).
static FREE: Mutex<Vec<Snapshot>> = Mutex::new(Vec::new());

/// The snapshot the present being made copies into ([`record`]), until
/// [`presented`] hands it to the read.
static RECORDED: Mutex<Option<Snapshot>> = Mutex::new(None);

/// Records, in `list`, a copy of the counters named so far into a snapshot
/// (`None` without counters, or if no snapshot could be made, logged once).
/// The snapshot must outlive the list's run.
fn snapshot(host: &crate::host::Host, list: &ID3D12GraphicsCommandList) -> Option<Snapshot> {
    let buffer = BUFFER.get()?.as_ref()?;
    let free = lock(&FREE).pop();
    let mut snapshot = match free {
        Some(s) => s,
        None => {
            let Some((resource, data)) = host.shared_buffer(BYTES) else {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::warn!("loop-limit counters: no snapshot buffer; frames go unread");
                });
                return None;
            };
            crate::host::name(&resource, || "loop-limit counters, a frame's".into());
            Snapshot {
                resource,
                data: data.cast_const(),
                slots: 0,
            }
        }
    };
    snapshot.slots = counts(|c| c.names.len().min(SLOTS as usize) as u32);
    // SAFETY: an open list; both buffers are in the common state between
    // submissions (promoted to copy source and destination here) and live
    // until the copy has run: the counters for good, the snapshot held by
    // whoever reads it once the list has run.
    unsafe {
        list.CopyBufferRegion(
            &snapshot.resource,
            0,
            &buffer.resource,
            0,
            u64::from(snapshot.slots) * STRIDE,
        );
    }
    Some(snapshot)
}

/// In the present's list `list`, after every list of the frame on the host
/// queue and before the present's submission: the frame's counters are
/// copied, to be read by [`presented`] once the list has run.
pub(crate) fn record(host: &crate::host::Host, list: &ID3D12GraphicsCommandList) {
    if !xtp_shader::emit::loop_count() {
        return;
    }
    let Some(snapshot) = snapshot(host, list) else {
        return;
    };
    // A snapshot left by a present that was not made: free again once
    // whatever was submitted so far has run.
    if let Some(left) = lock(&RECORDED).replace(snapshot) {
        host.after(move || lock(&FREE).push(left));
    }
}

/// Just after a present's submission (of frame `frame`), which copied the
/// counters ([`record`]): they are read from that copy once every
/// submission made so far has finished.
pub(crate) fn presented(host: &crate::host::Host, frame: u64) {
    if !xtp_shader::emit::loop_count() {
        return;
    }
    // No buffer yet, or no snapshot: nothing to read.
    let Some(snapshot) = lock(&RECORDED).take() else {
        return;
    };
    host.after(move || {
        read(&snapshot, frame);
        lock(&FREE).push(snapshot);
    });
}

/// Reads frame `frame`'s counters from `snapshot` (on the thread that
/// finished its submissions: no waits here). Slots named after the copy
/// count from the next frame on.
fn read(snapshot: &Snapshot, frame: u64) {
    counts(|c| {
        let n = snapshot.slots as usize;
        if c.last.len() < n {
            c.last.resize(n, 0);
        }
        let mut stopped = Vec::new();
        for (slot, last) in c.last[..n].iter_mut().enumerate() {
            let now = read_slot(snapshot.data, slot as u32);
            let gained = now.wrapping_sub(*last);
            *last = now;
            if gained > 0 {
                stopped.push((slot as u32, gained));
            }
        }
        c.sums.add(&stopped);
        if !stopped.is_empty() {
            let lanes: u64 = stopped.iter().map(|&(_, n)| u64::from(n)).sum();
            let named: Vec<String> = stopped
                .iter()
                .map(|&(slot, n)| format!("{} {n}", c.names[slot as usize]))
                .collect();
            tracing::debug!(
                frame,
                lanes,
                pipelines = stopped.len(),
                "loop limits in a frame: {}",
                named.join(", ")
            );
        }
    });
}

/// What the frames read since the last line add up to.
#[derive(Debug, Default, Clone, PartialEq)]
struct Sums {
    frames: u64,
    /// Frames where some lane stopped.
    frames_stopped: u64,
    /// The most lanes, and pipelines, that stopped in one frame.
    most_lanes: u64,
    most_pipelines: u64,
    /// By slot: the lanes stopped.
    lanes: HashMap<u32, u64>,
}

impl Sums {
    /// One frame, which stopped `stopped`'s lanes (slot, lanes).
    fn add(&mut self, stopped: &[(u32, u32)]) {
        self.frames += 1;
        let lanes: u64 = stopped.iter().map(|&(_, n)| u64::from(n)).sum();
        if lanes > 0 {
            self.frames_stopped += 1;
        }
        self.most_lanes = self.most_lanes.max(lanes);
        self.most_pipelines = self.most_pipelines.max(stopped.len() as u64);
        for &(slot, n) in stopped {
            *self.lanes.entry(slot).or_default() += u64::from(n);
        }
    }
}

/// With the frame line: the frames read since the last one, summed up.
pub(crate) fn line() {
    if !xtp_shader::emit::loop_count() {
        return;
    }
    let text = counts(|c| {
        let sums = std::mem::take(&mut c.sums);
        // Before the first read (no counting pipeline yet), nothing.
        (sums.frames > 0).then(|| text(&sums, &c.names, c.slots.len()))
    });
    if let Some(text) = text {
        tracing::info!("{text}");
    }
}

/// The line for `sums`, the slots named `names`, of `counting` pipelines.
fn text(sums: &Sums, names: &[String], counting: usize) -> String {
    let lanes: u64 = sums.lanes.values().sum();
    let mut most: Vec<(&u32, &u64)> = sums.lanes.iter().collect();
    most.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let named: Vec<String> = most
        .iter()
        .take(TOP)
        .map(|&(&slot, &n)| {
            let name = names.get(slot as usize).map_or("?", String::as_str);
            format!("{name} {n}")
        })
        .collect();
    format!(
        "loop limits over {} frames: {:.1} lanes per frame stopped at a loop's limit or \
         budget, in {} of {counting} counting pipelines; {} frames with stopped lanes, at most \
         {} lanes and {} pipelines in one{}{}",
        sums.frames,
        lanes as f64 / sums.frames.max(1) as f64,
        sums.lanes.len(),
        sums.frames_stopped,
        sums.most_lanes,
        sums.most_pipelines,
        if named.is_empty() { "" } else { "; most: " },
        named.join(", ")
    )
}

/// The lanes slot `slot` has counted so far (tests).
#[cfg(test)]
pub(crate) fn counted(slot: u32) -> Option<u32> {
    Some(read_slot(buffer()?.data, slot))
}

/// [`snapshot`], recorded in `list` by a test.
#[cfg(test)]
pub(crate) fn snapshot_in(
    host: &crate::host::Host,
    list: &ID3D12GraphicsCommandList,
) -> Option<Snapshot> {
    snapshot(host, list)
}

#[cfg(test)]
impl Snapshot {
    /// The lanes slot `slot` had counted when the copy ran (`None` for a
    /// slot named after it was recorded).
    pub(crate) fn counted(&self, slot: u32) -> Option<u32> {
        (slot < self.slots).then(|| read_slot(self.data, slot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each slot's counter starts on 16 bytes, as a root UAV's address
    /// must (a raw view's): all four residues of a slot modulo 4, the
    /// shared slot, the last one and those past it. Counters do not
    /// overlap, and the buffer holds them all.
    #[test]
    fn every_loop_counter_slot_is_16_byte_aligned() {
        for slot in [0, 1, 2, 3, 4, 5, 6, 7, SLOTS - 1, SLOTS, u32::MAX] {
            assert_eq!(offset(slot) % 16, 0, "slot {slot}");
            assert!(offset(slot) + 4 <= BYTES, "slot {slot}");
        }
        assert_eq!(offset(SHARED), 0);
        for slot in 0..8 {
            assert!(offset(slot + 1) >= offset(slot) + 4, "slot {slot}");
        }
        assert_eq!(STRIDE, 16);
    }

    /// The line sums the frames up and names the pipelines that stopped
    /// the most lanes, most first; frames without stopped lanes count in
    /// the frames, not in the rest.
    #[test]
    fn the_loop_limit_line_sums_frames_and_names_the_pipelines() {
        let mut sums = Sums::default();
        sums.add(&[(1, 3), (2, 65_536)]);
        sums.add(&[]);
        sums.add(&[(1, 1)]);
        sums.add(&[]);
        assert_eq!((sums.frames, sums.frames_stopped), (4, 2), "{sums:?}");
        assert_eq!((sums.most_lanes, sums.most_pipelines), (65_539, 2));
        let names = [
            String::new(),
            "[compute 776d835da5ffa646]".to_owned(),
            "[vertex a67d0bcde30cead3, pixel a467d848ace5810d]".to_owned(),
        ];
        let line = text(&sums, &names, 7);
        assert!(
            line.starts_with(
                "loop limits over 4 frames: 16385.0 lanes per frame stopped at a loop's limit or \
                 budget, in 2 of 7 counting pipelines; 2 frames with stopped lanes, at most 65539 \
                 lanes and 2 pipelines in one; most: [vertex a67d0bcde30cead3, pixel \
                 a467d848ace5810d] 65536, [compute 776d835da5ffa646] 4"
            ),
            "{line}"
        );
        // Nothing stopped: the frames alone.
        let mut none = Sums::default();
        none.add(&[]);
        assert!(
            text(&none, &names, 7).ends_with("at most 0 lanes and 0 pipelines in one"),
            "{}",
            text(&none, &names, 7)
        );
    }
}
