//! One frame in flight (`XTP_FRAMES_IN_FLIGHT`, docs/analysis/frames-in-flight.md):
//! the title prepares frame N+1 while the GPU runs frame N. A present no
//! longer finishes every submission before it returns; the frame before it
//! is finished instead, before its copy is recorded ([`super::Host::flip`]),
//! and so is the frame presented last before a flip wait ends
//! (`crate::frame::wait`), as a console's flip follows the GPU. The title
//! is then never more than one frame ahead of the GPU.
//!
//! Ported from simnJS/AnyPS5 (7b85afc1aa: a flip completes once its work is
//! submitted and the previous flip's work is done; 4328d9d796: the title
//! never more than one frame ahead of the GPU, after eight frames ahead let
//! a title rewrite tables the GPU had not read yet).
//!
//! `XTP_FRAMES_IN_FLIGHT=ab` alternates the two within one run, to compare
//! them on the same scene: off in phase A, on in phase B, through
//! [`xtp_core::ab`] (phases of 20 s unless told, `ab:<seconds>`, the first
//! 3 s of each not measured, `ab:<seconds>:<settle>`; a line per phase and
//! the frame lines tagged with theirs).

use std::sync::LazyLock;

use xtp_core::ab;

/// `XTP_FRAMES_IN_FLIGHT`, read once: `0` off, `1` on, `ab…` off and on by
/// A/B phase ([`ab::switch`]). Without it: off. It held 150 s of the
/// Series S world, but gains nothing while a frame's waits are inside its
/// submissions rather than at its present
/// (docs/analysis/frames-in-flight.md §5).
static IN_FLIGHT: LazyLock<ab::Switch<bool>> = LazyLock::new(|| {
    let switch = ab::switch("XTP_FRAMES_IN_FLIGHT", false);
    let mode = match (switch.alternates(), switch.get()) {
        (true, _) => "off in A/B phase A, on in phase B",
        (false, true) => "on",
        (false, false) => "off",
    };
    tracing::info!("frames in flight: {mode}");
    switch
});

/// Whether a present lets its frame run on while the title prepares the
/// next one (in an A/B run, as its phase says: it changes at presents).
pub(crate) fn ahead() -> bool {
    IN_FLIGHT.get()
}

/// Submissions in flight at most: a submission past them waits for the
/// oldest first. A world frame of BTP 6 makes 20 to 40 (run of
/// 2026-11-24 18:11), so one frame in flight needs room for two.
pub(crate) fn cap() -> usize {
    if ahead() { 128 } else { 16 }
}
