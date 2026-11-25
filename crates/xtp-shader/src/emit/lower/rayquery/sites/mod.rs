//! The walks [`super::splice`] replaces: one file per program, named by the
//! first 8 hex digits of its hash as the logs give it, each with its site
//! described by hand from the program's disassembly (never committed) and
//! proven by play at translation ([`super::proof`]).
//!
//! A new site is a new file here, its `mod` line and its entry in
//! [`SITES`], each entry under a comment of its own so that one site is one
//! line.

use super::Site;

#[path = "32ed072b.rs"]
mod p32ed072b;

/// The walks to splice, by the fingerprint of their program's code
/// ([`super::fingerprint`]).
pub(super) const SITES: &[(u64, Site)] = &[
    // `32ed072b9f845048`: the local lights' tiled traced shadows.
    p32ed072b::SITE,
];
