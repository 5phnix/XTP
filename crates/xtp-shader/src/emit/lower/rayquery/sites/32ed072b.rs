//! `32ed072b9f845048`, the local lights' tiled traced shadows (family A,
//! `docs/analysis/rt-dxr.md` section 4.3): one ray per light of a tile, whose
//! t the deferred lighting (`1cd9bc634417a981`) reads as the light's shadow
//! distance.
//!
//! Described by hand from its disassembly (never committed):
//!
//! * the walk is the loop from 0x170 to its exit at 0xcc8; the check of the
//!   structure's root before it branches to 0xcc4 when it reads empty;
//! * the ray is in v1-v3 (origin), v4 (t-max) and v6, v9, v11 (the
//!   direction, normalised), with t-min 0;
//! * the walk keeps instances whose mask has a bit of 0xd5, takes every
//!   triangle as opaque and leaves back-facing ones out;
//! * the code after the walk reads whether it hit (v7, a node pointer), its
//!   t (v4), its iteration count (v14, against 0xfffe) and EXEC, which the
//!   walk narrows and restores;
//! * the structure's buffer descriptor is at 0x20 of the table in user SGPR
//!   5.

use super::super::{Kind, Site, Source, ray_flags};
use crate::ir::{EXEC, Reg};

/// The site, by the fingerprint of the program's code.
pub(super) const SITE: (u64, Site) = (
    0x0487_AF5C_D93B_33BD,
    Site {
        header: 0x170,
        exit: 0xcc8,
        skip: Some(0xcc4),
        origin: [1, 2, 3],
        direction: [6, 9, 11],
        t_max: 4,
        t_min: 0,
        mask: 0xd5,
        flags: ray_flags::FORCE_OPAQUE | ray_flags::CULL_BACK_FACING_TRIANGLES,
        results: &[(7, Kind::Hit), (4, Kind::T), (14, Kind::Count)],
        kept: &[Reg::S(EXEC)],
        cap: Some(0xfffe),
        tlas: Source {
            user_sgpr: 5,
            offset: 0x20,
        },
    },
);
