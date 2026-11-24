//! `xtp precook`'s side in this crate: a shader container of the title's
//! pack made ready the way the title's first use of it makes it, through
//! the same calls, so that a session after a translator change starts with
//! its translations, and its compute pipelines, already made.
//!
//! - Every container is translated for the host's wave sizes
//!   ([`crate::device::host_target`]) by [`crate::shader_cache`]'s
//!   `host_shader`, the function the title's pipelines call: same
//!   translation, same key, same entry. The title finds it on first use.
//! - A compute program's host pipeline is made from its translation alone
//!   (its root signature is the translation's bindings:
//!   `host::HostPipeline::new`), so it is made here too, by that function,
//!   which stores it into the host pipeline library as a first dispatch
//!   does; [`finish`] saves the library.
//! - Graphics and mesh pipelines need the title's graphics state as well.
//!   No run records it (the library keeps the compiled pipelines, and
//!   `early-pipelines.txt` only the keys of the title pipelines used), so
//!   they are made in the session, as before.

use std::sync::atomic::Ordering;

use xtp_shader::{Container, FourCC, HwStage};

/// What became of one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Its translation is in the cache (made now or found there: [`counts`]
    /// tells how many of each), and for a compute program with pipelines
    /// asked, its host pipeline in the library.
    Ready,
    /// The translator refuses it: the reason, kept in the cache as the
    /// title's first use would keep it.
    Refused(String),
    /// A compute program that gets no host pipeline (the reasons:
    /// [`refusals`]).
    NoPipeline,
}

/// What the translations are made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// The wave sizes translated for, as (min, max) lanes.
    pub lanes: (u32, u32),
    /// The key the translations are kept under (this build's translator
    /// and settings, and the target), in hexadecimal.
    pub key: String,
}

/// Makes the host device (the title's adapter: `XTP_ADAPTER` as in a run)
/// and says what translations are made for; why not, without a device.
pub fn start() -> Result<Ready, String> {
    crate::host::created().as_ref().map_err(Clone::clone)?;
    let target = crate::device::host_target();
    let key = crate::shader_cache::host_key();
    Ok(Ready {
        lanes: (target.lanes_min, target.lanes_max),
        key: key.iter().map(|b| format!("{b:02x}")).collect(),
    })
}

/// The stage of the program in `container` that a translation takes (its
/// first vertex, pixel, mesh or compute program, as `translate_for` takes
/// it); `None` if it holds none.
pub fn stage(container: &[u8]) -> Option<HwStage> {
    let c = Container::parse(container).ok()?;
    let part = c.part(FourCC::DXIL)?;
    xtp_shader::native_programs(part.data)
        .ok()?
        .into_iter()
        .map(|p| p.stage)
        .find(|s| matches!(s, HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs))
}

/// Precooks `container`, whose program is of `stage` ([`stage`]): its
/// translation into the shader cache and, with `pipelines`, a compute
/// program's host pipeline into the library.
pub fn container(container: &[u8], stage: Option<HwStage>, pipelines: bool) -> Outcome {
    let target = crate::device::host_target();
    if !(pipelines && stage == Some(HwStage::Cs)) {
        return match crate::shader_cache::host_shader(container, target) {
            Ok(_) => Outcome::Ready,
            Err(reason) => Outcome::Refused(reason),
        };
    }
    // As a compute pipeline the title creates: the shared container, then
    // its host pipeline, which translates it through the cache first.
    let shader = super::shader("compute", container);
    if super::host::HostPipeline::new(std::slice::from_ref(&shader), None).is_some() {
        return Outcome::Ready;
    }
    match shader.translated.get() {
        // The translator refused it: its reason, from the refusal just
        // kept (refusals count neither as made nor as found).
        Some(None) => match crate::shader_cache::host_shader(container, target) {
            Err(reason) => Outcome::Refused(reason),
            Ok(_) => Outcome::NoPipeline,
        },
        _ => Outcome::NoPipeline,
    }
}

/// What the precook made and found since the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// Shaders translated, and found in the cache (refusals are neither).
    pub translated: u64,
    pub cached: u64,
    /// Host pipelines compiled, and loaded from the library.
    pub made: u64,
    pub loaded: u64,
}

/// The counts so far.
pub fn counts() -> Counts {
    let (cached, translated) = crate::shader_cache::counts();
    Counts {
        translated,
        cached,
        made: super::library::MADE.load(Ordering::Relaxed),
        loaded: super::library::LOADED.load(Ordering::Relaxed),
    }
}

/// The host pipelines refused so far, by reason, most first.
pub fn refusals() -> Option<String> {
    super::refusals()
}

/// Saves the pipeline library if pipelines were added. The list of title
/// pipelines to start early is read first: no title pipeline was created
/// here, so nothing else read it, and the save writes back what it holds.
pub fn finish() {
    let _ = super::library::wanted_early("");
    super::library::save(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A container's stage is its translated program's; none without one.
    /// (`apps/xtp/tests/precook.rs` precooks a real compute program.)
    #[test]
    fn stages_are_read_from_the_container() {
        assert_eq!(stage(b"not a container"), None);
        assert_eq!(stage(&dxbc(&[])), None, "no DXIL part");
        assert_eq!(stage(&dxbc(&[(b"DXIL", &[0; 8])])), None, "no program");
    }

    /// A DXBC container holding `parts`.
    fn dxbc(parts: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let header = 0x20 + 4 * parts.len();
        let mut body = Vec::new();
        let mut offsets = Vec::new();
        for (fourcc, data) in parts {
            offsets.push((header + body.len()) as u32);
            body.extend(*fourcc);
            body.extend((data.len() as u32).to_le_bytes());
            body.extend(*data);
        }
        let mut out = b"DXBC".to_vec();
        out.extend([0; 16]);
        out.extend(1u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(((header + body.len()) as u32).to_le_bytes());
        out.extend((parts.len() as u32).to_le_bytes());
        for o in offsets {
            out.extend(o.to_le_bytes());
        }
        out.extend(body);
        out
    }
}
