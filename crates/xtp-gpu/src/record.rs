//! Frame record and replay (docs/analysis/frame-replay.md).
//!
//! **Record** (`XTP_RECORD=<dir>`, `XTP_RECORD_FRAMES=<n>[:<count>]` or
//! `capture[:<count>]`, diagnostics builds): the title gets a device whose
//! objects record their calls ([`proxy`]). From the device's creation on,
//! what makes objects and views is kept (the journal); when the recorded
//! frames start, the open lists' calls, the fences' values and a snapshot
//! of title memory; during them, every call kept ([`methods`]), and the
//! memory pages that changed before and after each submission. Files:
//! `calls.bin` (the events, [`wire`]), `memory.bin` (the snapshot) and
//! `summary.txt`, compressed in blocks ([`store`]).
//!
//! **Replay** ([`replay::run`]): the calls made again on our device in a
//! process of their own (`xtp replay`), with title memory where the session
//! had it.
#![cfg_attr(not(feature = "diagnostics"), allow(dead_code))]

mod memory;
mod methods;
mod proxy;
pub mod replay;
mod store;
#[cfg(test)]
mod tests;
mod wire;

#[cfg(feature = "diagnostics")]
pub(crate) use proxy::{hand_out_device, is_proxy_of};
pub use replay::{Frame, Options, Poke, Summary, parse_poke, read_pokes, run};

/// Free bytes on the volume of `dir`, if the system says (recordings take
/// gigabytes).
pub fn free_bytes(dir: &std::path::Path) -> Option<u64> {
    proxy::free_bytes(dir)
}
