use crate::dxbc::{self, Container, FourCC};
use crate::emit::{HostShader, translate_program};
use crate::native::{HwStage, NativeProgram, StageIo};

fn pixel(db_shader_control: u32) -> HostShader {
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        float_mode: 0,
        vgprs: 1,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            rsrc2: 0,
            db_shader_control,
            patches: vec![],
        },
        code: 0xBF81_0000u32.to_le_bytes().to_vec(), // s_endpgm
    };
    let bytes = dxbc::build(&[]);
    translate_program(&Container::parse(&bytes).unwrap(), &native).unwrap()
}

#[test]
fn depth_before_shader_sets_only_the_dxil_entry_flag() {
    // Même programme, sans écriture UAV. Seul le bit 12 commande le test anticipé.
    let neighbors = (1 << 11) | (1 << 13);
    let plain = pixel(neighbors);
    let early = pixel(neighbors | (1 << 12));
    assert_eq!(plain.dxil, pixel(0).dxil);
    let plain_parts = Container::parse(&plain.dxil).unwrap();
    let early_parts = Container::parse(&early.dxil).unwrap();
    for part in &plain_parts.parts {
        let other = early_parts.part(part.fourcc).unwrap().data;
        if part.fourcc == FourCC::DXIL {
            assert_ne!(part.data, other);
        } else {
            assert_eq!(part.data, other);
        }
    }
    #[cfg(windows)]
    if let Some(ir) = crate::emit::smem_tests::disassembly(&early) {
        assert!(ir.contains("!{i32 0, i64 8}"), "{ir}");
    }
}
