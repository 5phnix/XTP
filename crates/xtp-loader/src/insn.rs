//! Decoders for console-only instructions the crash handler does the job
//! of (`crash::emulate_amd_only`) on CPUs that lack them. Pure, so they are
//! tested on any CPU; the handler applies what they find to the saved
//! context.

/// `rdpru` (`0f 01 fd`) reads the processor register `ecx` names: 0 is
/// MPERF, 1 is APERF, the counters titles divide to measure the effective
/// clock. Both count at the host's TSC rate here (`tsc`), so the ratio
/// reads as the base clock. Any other register reads as absent (`None`):
/// zero. The flags are left alone, as a Zen 4 leaves them (measured).
pub(crate) fn rdpru(ecx: u32, tsc: u64) -> Option<u64> {
    matches!(ecx, 0 | 1).then_some(tsc)
}

/// `rdpid r64` (`f3 [rex] 0f c7 /7`, a register operand), which writes the
/// processor's id: the register it writes and the instruction's length.
pub(crate) fn rdpid(code: &[u8]) -> Option<(u8, usize)> {
    let (rex, at) = match *code.get(1)? {
        rex @ 0x40..=0x4f => (rex, 2),
        _ => (0, 1),
    };
    if *code.first()? != 0xf3 || *code.get(at..at + 2)? != [0x0f, 0xc7] {
        return None;
    }
    let modrm = *code.get(at + 2)?;
    if modrm >> 6 != 3 || (modrm >> 3) & 7 != 7 {
        return None;
    }
    Some((modrm & 7 | (rex & 1) << 3, at + 3))
}

/// How a `clzero` loop decides to go round again after `cmp rax, limit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Again {
    /// `jb`: while `rax < limit`.
    Below,
    /// `jbe`: while `rax <= limit`.
    BelowOrEqual,
    /// `jne`: until `rax == limit`.
    NotEqual,
}

/// A `clzero` loop, one cache line a pass (`limit` any other register):
///
/// ```text
/// top: clzero              ; 0f 01 fc, zeroes the line at rax
///      add  rax, 0x40      ; 48 83 c0 40
///      cmp  rax, limit     ; REX.W 39 /r, or REX.W 3b /r
///      jb | jbe | jne top  ; 72 | 76 | 75, rel8
/// ```
///
/// Each pass costs an exception on a CPU without `clzero` (about a
/// microsecond), so the handler zeroes the whole range in one, as it does
/// `clwb` loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClzeroLoop {
    /// The register `rax` is compared with.
    pub(crate) limit: u8,
    pub(crate) again: Again,
    /// Bytes from `clzero` to the instruction after the jump.
    pub(crate) len: usize,
}

/// The loop `code` starts with, if it is one.
pub(crate) fn clzero_loop(code: &[u8]) -> Option<ClzeroLoop> {
    if *code.get(..7)? != [0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40] {
        return None;
    }
    let &[rex, op, modrm] = code.get(7..10)? else {
        return None;
    };
    if modrm >> 6 != 3 {
        return None;
    }
    let (reg, rm) = ((modrm >> 3) & 7, modrm & 7);
    // `rax` is the operand no REX bit extends; the limit takes the other.
    let limit = match (rex, op) {
        (0x48 | 0x4c, 0x39) if rm == 0 => reg | (rex & 4) << 1,
        (0x48 | 0x49, 0x3b) if reg == 0 => rm | (rex & 1) << 3,
        _ => return None,
    };
    let again = match *code.get(10)? {
        0x72 => Again::Below,
        0x76 => Again::BelowOrEqual,
        0x75 => Again::NotEqual,
        _ => return None,
    };
    let len = 12;
    if i64::from(*code.get(11)? as i8) != -(len as i64) || limit == 0 {
        return None;
    }
    Some(ClzeroLoop { limit, again, len })
}

impl ClzeroLoop {
    /// How many passes the loop makes when it reaches `clzero` with `rax`
    /// and `limit` (the faulting pass included), or `None` if it would run
    /// through the end of the address space.
    pub(crate) fn passes(&self, rax: u64, limit: u64) -> Option<u64> {
        let passes = match self.again {
            Again::BelowOrEqual if rax <= limit => (limit - rax) / 64 + 1,
            Again::Below if rax < limit => (limit - rax).div_ceil(64),
            Again::BelowOrEqual | Again::Below => 1,
            Again::NotEqual => {
                let distance = limit.wrapping_sub(rax);
                if distance == 0 || !distance.is_multiple_of(64) {
                    return None;
                }
                distance / 64
            }
        };
        rax.checked_add(passes.checked_mul(64)?)?;
        Some(passes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdpru_reads_mperf_and_aperf_only() {
        assert_eq!(rdpru(0, 1234), Some(1234));
        assert_eq!(rdpru(1, 1234), Some(1234));
        assert_eq!(rdpru(2, 1234), None);
        assert_eq!(rdpru(u32::MAX, 1234), None);
    }

    #[test]
    fn rdpid_takes_any_register() {
        assert_eq!(rdpid(&[0xf3, 0x0f, 0xc7, 0xf8]), Some((0, 4))); // rax
        assert_eq!(rdpid(&[0xf3, 0x0f, 0xc7, 0xfb]), Some((3, 4))); // rbx
        assert_eq!(rdpid(&[0xf3, 0x41, 0x0f, 0xc7, 0xf9]), Some((9, 5))); // r9
        assert_eq!(rdpid(&[0xf3, 0x48, 0x0f, 0xc7, 0xf8]), Some((0, 5))); // REX.W
        assert_eq!(rdpid(&[0x0f, 0xc7, 0xf8]), None); // rdseed
        assert_eq!(rdpid(&[0xf3, 0x0f, 0xc7, 0xf0]), None); // /6
        assert_eq!(rdpid(&[0xf3, 0x0f, 0xc7, 0x38]), None); // a memory operand
        assert_eq!(rdpid(&[0xf3, 0x0f, 0xc7]), None); // cut off
    }

    #[test]
    fn clzero_loops_by_register_and_condition() {
        // cmp rax, rcx (39 /r), jbe
        let a = [
            0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x48, 0x39, 0xc8, 0x76, 0xf4,
        ];
        let expected = ClzeroLoop {
            limit: 1,
            again: Again::BelowOrEqual,
            len: 12,
        };
        assert_eq!(clzero_loop(&a), Some(expected));
        // cmp rax, r9 (REX.R, 39 /r), jb
        let b = [
            0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x4c, 0x39, 0xc8, 0x72, 0xf4,
        ];
        let b = clzero_loop(&b).unwrap();
        assert_eq!((b.limit, b.again), (9, Again::Below));
        // cmp rax, r10 (REX.B, 3b /r), jne
        let c = [
            0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x49, 0x3b, 0xc2, 0x75, 0xf4,
        ];
        let c = clzero_loop(&c).unwrap();
        assert_eq!((c.limit, c.again), (10, Again::NotEqual));
        // Another stride, a compare of another register, a jump elsewhere,
        // a cut-off loop: not one.
        for bad in [
            &[
                0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x20, 0x48, 0x39, 0xc8, 0x76, 0xf4,
            ][..],
            &[
                0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x48, 0x39, 0xd1, 0x76, 0xf4,
            ],
            &[
                0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x48, 0x39, 0xc8, 0x76, 0xf0,
            ],
            &[
                0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x48, 0x39, 0xc0, 0x76, 0xf4,
            ],
            &[0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x48, 0x39, 0xc8],
        ] {
            assert_eq!(clzero_loop(bad), None, "{bad:02x?}");
        }
    }

    #[test]
    fn clzero_loop_passes() {
        let with = |again| ClzeroLoop {
            limit: 1,
            again,
            len: 12,
        };
        let (jbe, jb, jne) = (
            with(Again::BelowOrEqual),
            with(Again::Below),
            with(Again::NotEqual),
        );
        // Four lines from 0x1000: jbe stops past 0x10c0, jb at 0x1100,
        // jne at exactly 0x1100.
        assert_eq!(jbe.passes(0x1000, 0x10c0), Some(4));
        assert_eq!(jbe.passes(0x1005, 0x10c4), Some(3));
        assert_eq!(jbe.passes(0x1005, 0x10c5), Some(4));
        assert_eq!(jb.passes(0x1000, 0x1100), Some(4));
        assert_eq!(jb.passes(0x1000, 0x10c1), Some(4));
        assert_eq!(jne.passes(0x1000, 0x1100), Some(4));
        // At or past the limit already: the faulting pass only.
        assert_eq!(jbe.passes(0x2000, 0x1000), Some(1));
        assert_eq!(jb.passes(0x1000, 0x1000), Some(1));
        // jne that never meets its limit, or wraps around.
        assert_eq!(jne.passes(0x1000, 0x1010), None);
        assert_eq!(jne.passes(0x1000, 0x1000), None);
        assert_eq!(jbe.passes(u64::MAX - 0x40, u64::MAX), None);
    }
}
