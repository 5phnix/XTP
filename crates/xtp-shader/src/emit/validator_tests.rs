//! What dxil.dll refuses in the containers the emitter writes (W6).

use super::bitcode::{Attr, Cast, Term};
use super::container::SigElement;
use super::shader::Shader;
use super::validator::Validator;

#[allow(clippy::too_many_arguments)]
fn el(
    name: &str,
    index: u32,
    kind: u8,
    comp: u8,
    interp: u8,
    row: Option<u32>,
    col: u8,
    cols: u8,
) -> SigElement {
    SigElement {
        name: name.into(),
        index,
        kind,
        comp,
        interp,
        row,
        col,
        cols,
        usage: 0,
        half: false,
    }
}

fn target() -> SigElement {
    let mut t = el("SV_Target", 0, 16, 9, 0, Some(0), 0, 4);
    t.usage = 0xf;
    t
}

/// A pixel shader reading channels `reads` ((element, column)) of `inputs`
/// into SV_Target0's channels in turn.
fn pixel(mut inputs: Vec<SigElement>, reads: &[(usize, u8)]) -> Vec<u8> {
    let mut s = Shader::new(true);
    let b = s.entry;
    let mut vals = Vec::new();
    for &(id, col) in reads {
        let e = &mut inputs[id];
        e.usage |= 1 << col;
        let int = e.comp != 9;
        let (ty, suffix) = if int {
            (s.t.i32, ".i32")
        } else {
            (s.t.f32, ".f32")
        };
        let undef = s.m.undef(s.t.i32);
        let args = [s.m.i32(id as u32), s.m.i32(0), s.m.i8(col), undef];
        let v = s.op(b, &format!("loadInput{suffix}"), Attr::ReadNone, ty, 4, &args);
        let f32t = s.t.f32;
        vals.push(if int {
            s.m.cast(b, Cast::Bitcast, v, f32t)
        } else {
            v
        });
    }
    s.inputs = inputs;
    for col in 0..4u8 {
        let v = match vals.get(usize::from(col)) {
            Some(&v) => v,
            None => s.m.f32(0),
        };
        let args = [s.m.i32(0), s.m.i32(0), s.m.i8(col), v];
        let void = s.t.void;
        s.op(b, "storeOutput.f32", Attr::NoUnwind, void, 5, &args);
    }
    s.outputs = vec![target()];
    s.m.terminate(b, Term::Ret);
    s.finish()
}

/// A vertex shader writing every channel of `outputs` (zeros).
fn vertex(outputs: Vec<SigElement>) -> Vec<u8> {
    let mut s = Shader::new(false);
    let b = s.entry;
    for (id, e) in outputs.iter().enumerate() {
        for col in 0..e.cols {
            let (suffix, zero) = if e.comp == 9 {
                (".f32", s.m.f32(0))
            } else {
                (".i32", s.m.i32(0))
            };
            let args = [s.m.i32(id as u32), s.m.i32(0), s.m.i8(col), zero];
            let void = s.t.void;
            s.op(
                b,
                &format!("storeOutput{suffix}"),
                Attr::NoUnwind,
                void,
                5,
                &args,
            );
        }
    }
    s.outputs = outputs
        .into_iter()
        .map(|mut e| {
            e.usage = (1 << e.cols) - 1;
            e
        })
        .collect();
    s.m.terminate(b, Term::Ret);
    s.finish()
}

fn position() -> SigElement {
    el("SV_Position", 0, 3, 9, 4, Some(0), 0, 4)
}

/// Prints what dxil.dll says of each variant (exploration, not a check).
#[test]
#[ignore = "exploration: run with --ignored --nocapture"]
fn explore_validator_rules() {
    let path = Validator::default_path();
    eprintln!("dxil.dll: {path:?}");
    let mut v = Validator::load(path.as_deref()).expect("dxil.dll");
    let mut say = |what: &str, mut c: Vec<u8>| {
        let r = v.validate(&mut c);
        eprintln!(
            "{what}: {}",
            match r {
                Ok(()) => "ok".to_owned(),
                Err(e) => e.replace('\n', " | "),
            }
        );
    };
    let pos = || el("SV_Position", 0, 3, 9, 4, Some(0), 0, 4);
    let u = |index, interp, col| el("TEXCOORD", index, 0, 5, interp, Some(1), col, 1);
    let f = |index, interp, col| el("TEXCOORD", index, 0, 9, interp, Some(1), col, 1);
    say("V1 ps [u32 c unread, f32 lin unread]", pixel(vec![pos(), u(0, 1, 0), f(1, 2, 1)], &[]));
    say("V2 ps [u32 c read, f32 c unread]", pixel(vec![pos(), u(0, 1, 0), f(1, 1, 1)], &[(1, 0)]));
    say("V3 ps [f32 lin read, u32 lin unread]", pixel(vec![pos(), f(0, 2, 0), u(1, 2, 1)], &[(1, 0)]));
    say("V4 ps [f32 lin read, u32 undef unread]", pixel(vec![pos(), f(0, 2, 0), u(1, 0, 1)], &[(1, 0)]));
    say("V5 ps [u32 undef unread, f32 lin read]", pixel(vec![pos(), u(0, 0, 0), f(1, 2, 1)], &[(2, 0)]));
    say("V6 ps u32 lin read alone", pixel(vec![pos(), u(0, 2, 0)], &[(1, 0)]));
    say("V6b ps u32 lin unread alone", pixel(vec![pos(), u(0, 2, 0)], &[]));
    say("V7 ps f32 undef read alone", pixel(vec![pos(), f(0, 0, 0)], &[(1, 0)]));
    say("V7b ps f32 undef unread alone", pixel(vec![pos(), f(0, 0, 0)], &[]));
    say("V7c ps u32 undef unread alone", pixel(vec![pos(), u(0, 0, 0)], &[]));
    say("V8 vs [u32 lin, f32 lin]", vertex(vec![pos(), u(0, 2, 0), f(1, 2, 1)]));
    say("V9 vs [u32 undef, f32 undef]", vertex(vec![pos(), u(0, 0, 0), f(1, 0, 1)]));
    say("V9b vs [u32 c, f32 undef]", vertex(vec![pos(), u(0, 1, 0), f(1, 0, 1)]));
    say("V9c vs [f32 lin, u32 undef]", vertex(vec![pos(), f(0, 2, 0), u(1, 0, 1)]));
    say("V10 vs [u32 c, f32 c]", vertex(vec![pos(), u(0, 1, 0), f(1, 1, 1)]));
    say("V10b vs [u32 c, f32 lin]", vertex(vec![pos(), u(0, 1, 0), f(1, 2, 1)]));
    say("V10c vs pos undef", vertex(vec![el("SV_Position", 0, 3, 9, 0, Some(0), 0, 4)]));
    say("V10d vs pos lin", vertex(vec![el("SV_Position", 0, 3, 9, 2, Some(0), 0, 4)]));
    // Row order: an element at column 3 listed before one at column 0.
    say("V11 vs order [(1,3) then (1,0)]", vertex(vec![pos(), f(0, 2, 3), el("TEXCOORD", 1, 0, 9, 2, Some(1), 0, 3)]));
    say("V11b ps order [(1,3) then (1,0)]", pixel(vec![pos(), f(0, 2, 3), el("TEXCOORD", 1, 0, 9, 2, Some(1), 0, 3)], &[(1, 0), (2, 0)]));
    // Front face beside an unread float.
    say("V12 ps [f32 lin unread, front face c]", pixel(vec![pos(), f(0, 2, 0), el("SV_IsFrontFace", 0, 13, 5, 1, Some(1), 1, 1)], &[]));
    say("V12b ps [f32 c unread, front face c]", pixel(vec![pos(), f(0, 1, 0), el("SV_IsFrontFace", 0, 13, 5, 1, Some(1), 1, 1)], &[]));
    // Centroid and sample beside linear.
    say("V13 ps [f32 lin read, f32 centroid unread]", pixel(vec![pos(), f(0, 2, 0), f(1, 3, 1)], &[(1, 0)]));
    say("V14 ps [f32 noperspective read, f32 lin unread]", pixel(vec![pos(), f(0, 4, 0), f(1, 2, 1)], &[(1, 0)]));
}

#[test]
fn signature_writers_see_position() {
    // Placeholder so the module has a check until the real ones land.
    let c = vertex(vec![position()]);
    assert_eq!(&c[..4], b"DXBC");
}
