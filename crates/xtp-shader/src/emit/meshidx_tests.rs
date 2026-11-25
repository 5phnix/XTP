//! Ported from Marvisak/VIctory#2245 (2e26c46a6) by andrewpozdnakov7-jpg.

use super::Target;
use super::host::Host;
use super::mesh_tests::{
    CULL, EDGE_FLAGS, TRIANGLE, draw_flat_on, translate_flat_mesh, triangle_mesh,
};

#[test]
fn repeated_exports_emit_indices_once_in_dxil() {
    let native = triangle_mesh(&[TRIANGLE | CULL, TRIANGLE]);
    let (shader, _) = translate_flat_mesh(&native, Target::ANY);
    if let Some(ir) = super::smem_tests::disassembly(&shader) {
        assert_eq!(ir.matches("call void @dx.op.emitIndices(").count(), 1);
    }
}

#[test]
fn warp_draws_the_last_primitive_export() {
    let Ok(host) = Host::new(true) else {
        eprintln!("WARP SM 6.6 indisponible, rendu ignoré");
        return;
    };
    let cases: [(&[u32], usize); 6] = [
        (&[TRIANGLE], 256),
        (&[TRIANGLE | EDGE_FLAGS], 256),
        (&[TRIANGLE | EDGE_FLAGS | CULL], 0),
        (&[], 0),
        (&[TRIANGLE, TRIANGLE | CULL], 0),
        (&[TRIANGLE | CULL, TRIANGLE | EDGE_FLAGS], 256),
    ];
    for (exports, expected) in cases {
        let (_, _, pixels) = draw_flat_on(&host, &triangle_mesh(exports), (16, 16));
        let lit = pixels.iter().filter(|p| p[3] != 0.0).count();
        assert_eq!(lit, expected, "exports {exports:#x?}");
        if expected > 0 {
            assert!(pixels.iter().all(|p| *p == [1.0, 0.5, 0.25, 1.0]));
        }
    }
}
