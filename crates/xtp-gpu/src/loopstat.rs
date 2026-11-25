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
//! draws and dispatches bind the root UAV at that slot's dword
//! ([`address`]): `base + slot × 4` in one buffer of [`SLOTS`] dwords, in
//! system memory the CPU maps. After each present, once every submission
//! made so far has finished, the dwords are read, and what each one gained
//! is that frame's ([`presented`]); every few seconds, with the frame line
//! (`XTP_FRAME_LINE`), a line sums the frames since the last one up and
//! names the pipelines that stopped the most lanes ([`line`]). With
//! `xtp_gpu::loopstat=debug`, a line for each frame where a lane stopped.
//!
//! shortcut: what a frame counts is what was read once its present's
//! submission had finished: work of the next frame that had finished by
//! then counts in it, so the split between frames is approximate; the
//! totals are exact.
//!
//! shortcut: the buffer is in the common state, promoted to unordered
//! access wherever a list binds it, which lists on other queues may do at
//! the same time; the GPU's atomics on it are the only writes.
//!
//! [`HostShader::loop_counter`]: xtp_shader::emit::HostShader::loop_counter
//! [`LOOP_COUNTER_SPACE`]: xtp_shader::emit::LOOP_COUNTER_SPACE

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

/// Dwords in the buffer: one per counting pipeline (256 KiB).
pub(crate) const SLOTS: u32 = 1 << 16;

/// The slot of every pipeline past [`SLOTS`], counted together.
const SHARED: u32 = 0;

/// Pipelines named on the line.
const TOP: usize = 10;

/// The counters: the buffer, its GPU address and its mapping.
struct Buffer {
    /// Kept alive for good: lists bind it without keeping it.
    _resource: ID3D12Resource,
    gpu: u64,
    data: *const u32,
}

// SAFETY: a D3D12 resource is free-threaded; `data` is a persistent mapping
// of it, only read here (the GPU's atomics write it).
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
            let Some((resource, data)) = host.shared_buffer(u64::from(SLOTS) * 4) else {
                tracing::warn!("no loop-limit counter: its buffer was not made");
                return None;
            };
            // SAFETY: the mapping covers the buffer, which no list has used.
            unsafe { std::ptr::write_bytes(data, 0, SLOTS as usize * 4) };
            crate::host::name(&resource, || "loop-limit counters".into());
            // SAFETY: a plain query of a live buffer.
            let gpu = unsafe { resource.GetGPUVirtualAddress() };
            tracing::info!(
                slots = SLOTS,
                "loop-limit counters on (XTP_SHADER_LOOP_COUNT): lanes stopped at a loop's limit or budget, per pipeline"
            );
            Some(Buffer {
                _resource: resource,
                gpu,
                data: data.cast::<u32>().cast_const(),
            })
        })
        .as_ref()
}

/// The slots' names, the counts last read and the sums since the last line.
struct Counts {
    /// By slot: the pipeline's name (slot [`SHARED`]: those past the slots).
    names: Vec<String>,
    slots: HashMap<String, u32>,
    /// By slot: its dword when last read.
    last: Vec<u32>,
    sums: Sums,
}

static COUNTS: Mutex<Option<Counts>> = Mutex::new(None);

fn counts<T>(f: impl FnOnce(&mut Counts) -> T) -> T {
    let mut counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
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
/// (`u0` in [`xtp_shader::emit::LOOP_COUNTER_SPACE`]): its slot's dword.
pub(crate) fn address(slot: u32) -> Option<u64> {
    Some(buffer()?.gpu + u64::from(slot.min(SLOTS - 1)) * 4)
}

/// Slot `slot`'s dword as it is now.
fn read_slot(buffer: &Buffer, slot: u32) -> u32 {
    // SAFETY: a slot of the mapped buffer; the GPU's atomics write it.
    unsafe {
        buffer
            .data
            .add(slot.min(SLOTS - 1) as usize)
            .read_volatile()
    }
}

/// Just after a present's submission (of frame `frame`): the counts are read
/// once every submission made so far has finished.
pub(crate) fn presented(host: &crate::host::Host, frame: u64) {
    if !xtp_shader::emit::loop_count() {
        return;
    }
    // No buffer yet: no pipeline counts.
    let Some(buffer) = BUFFER.get().and_then(Option::as_ref) else {
        return;
    };
    host.after(move || read(buffer, frame));
}

/// Reads the counters after frame `frame` (on the thread that finished its
/// submissions: no waits here).
fn read(buffer: &Buffer, frame: u64) {
    counts(|c| {
        let n = c.names.len().min(SLOTS as usize);
        c.last.resize(n, 0);
        let mut stopped = Vec::new();
        for (slot, last) in c.last.iter_mut().enumerate() {
            let now = read_slot(buffer, slot as u32);
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
    Some(read_slot(buffer()?, slot))
}

#[cfg(test)]
mod tests {
    use super::*;

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
