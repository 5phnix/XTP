//! The proof by play (`lower/rayquery/proof.rs`, `play.rs`) against what
//! it must refuse: the counterexamples of the Codex review of eb7d7d8, each
//! a change to the family A program of `rayquery_tests` (this module's
//! parent, whose program and helpers it plays) that the proof accepted
//! before.

use super::{family_a, family_a_with, ir, planted};
use crate::emit::lower::rayquery::proof::{self, Prover};
use crate::emit::lower::rayquery::splice;
use crate::emit::lower::rayquery::world::{Root, World};

/// What the proof of `native`'s walk at `site` says: its report, or why it
/// refused.
fn proven(
    native: &crate::native::NativeProgram,
    site: &crate::emit::lower::rayquery::Site,
) -> Result<proof::Report, &'static str> {
    proof::prove(&ir(native), native, site).map_err(|f| f.why)
}

/// The words the code after the walk stores are compared bit for bit, not
/// as floats within a tolerance (point 2): the walk's hit register holds a
/// node pointer, the query's a 1, and a program that stores it raw stores
/// other words (33 and 1 read as floats are both near 0).
#[test]
fn rayquery_proof_compares_stored_words_exactly() {
    let (native, site) = family_a();
    assert!(proven(&native, &site).is_ok());
    let (native, site) = family_a_with(|t| {
        t.replace(
            "    buffer_store_dword v1, v2, s[8:11], 0 offen\n",
            "    buffer_store_dword v1, v2, s[8:11], 0 offen\n    \
             buffer_store_dword v8, v2, s[8:11], 0 offen offset:128\n",
        )
    });
    assert_eq!(
        proven(&native, &site).map(|_| ()),
        Err("a spliced program that stores other values than its walk")
    );
}

/// A lane counts as past its cap only once it has made the cap's
/// intersections (point 3): a walk that forces its count register past the
/// cap and zeroes t in lanes 0-11, at the end of each of its rounds, hid
/// their wrong distances from the comparison.
#[test]
fn rayquery_proof_refuses_a_cap_the_walk_never_reached() {
    let (native, site) = family_a_with(|t| {
        t.replace(
            "next:\n",
            "next:\n    \
             v_mbcnt_lo_u32_b32 v42, -1, 0\n    \
             v_cmp_le_u32 vcc_lo, 12, v42\n    \
             v_cndmask_b32 v9, lit(0x00010000), v9, vcc_lo\n    \
             v_cndmask_b32 v4, 0, v4, vcc_lo\n",
        )
    });
    assert_eq!(
        proven(&native, &site).map(|_| ()),
        Err("a ray-tracing walk that claims its cap without reaching it")
    );
}

/// The lanes that leave the walk are the lanes the query answered (point
/// 6). Here EXEC is widened to the whole wave between the empty-structure
/// check and the walk: on an empty structure the console's program leaves
/// by the skip path in its one lane, while the spliced one queries in 32
/// and goes on in those. Every stored word is the same (a miss is FLT_MAX
/// in all lanes), which the comparison accepted.
#[test]
fn rayquery_proof_compares_the_lanes_that_leave_the_walk() {
    let (native, site) = family_a_with(|t| {
        t.replace(
            "    s_cbranch_scc0 @skip\n",
            "    s_cbranch_scc0 @skip\n    s_mov_b32 exec_lo, -1\n",
        )
    });
    let p = ir(&native);
    let spliced = splice(p.clone(), &site).unwrap();
    let prover = Prover::new(&site, &native, &p, &spliced).unwrap();
    // Lane 0 alone in the tile, an empty structure.
    let plant = planted([1.0, 1.0, 0.0], [0.0, 0.0, 1.0], 100.0);
    let played = prover
        .play(0, 0, Root::Empty, &plant, &mut |_, _| World::empty())
        .unwrap_or_else(|f| panic!("{f:?}"));
    assert_eq!(played.guest.len(), played.host.len());
    assert_eq!(
        prover
            .compare(0, &played, &mut proof::Report::default())
            .map_err(|f| f.why),
        Err("a ray-tracing walk whose lanes at its exit are not the query's")
    );
}
