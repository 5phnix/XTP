//! The proof by play (`docs/analysis/rt-dxr.md` section 5.3): a walk is
//! spliced only once the program, played with the walk and played with the
//! host query in its place, gives the same answers on synthetic worlds.
//!
//! Each world is one wave of the program, from its entry, on inputs of the
//! play's own ([`Play`]): every user SGPR points at a table whose 16-byte
//! slots are buffer descriptors, every buffer holds positive floats whose
//! halves are sane f16s too (so a ray reads as one in any record layout),
//! except the descriptor the site names as the structure's
//! ([`Site::tlas`]), which points at the world. The guest is played to the
//! walk's header; the world is then built around the rays it holds there
//! (`world::around`), laid out in the console's format, and the guest goes
//! on through the walk and to its end. The spliced program is played from
//! its entry on the same inputs, its query answered by DXR's rules on the
//! same world ([`World::query`]). The proof holds when, lane by lane:
//!
//! * at the walk's exit, each result the site names is what the host
//!   answered, by its kind ([`Kind`]); the registers the site keeps hold
//!   what they held on entry;
//! * both programs store the same values at the same addresses.
//!
//! Lanes where the console and DXR are known to differ are left out and
//! counted: a candidate at t-min or t-max, a walk past its iteration cap
//! (`Kind::T`, `Kind::Count`), and rays that graze a triangle's edge, where
//! float rounding decides. A walk that reads memory outside its world, an
//! instruction the machine has no model of, or worlds that leave the walk
//! unexercised refuse the proof.

use std::collections::HashMap;

use super::play::{BvhRay, Env, Fail, LANES, Machine, QueryRay, Wave, lanes};
use super::world::{self, Answer, Layout, Ray, Rng, Root, World, root_pointer};
use super::{Kind, Site, Why, splice};
use crate::ir::{EXEC, Inst, Program, Reg};
use crate::native::{NativeProgram, StageIo};

/// User-data tables: 1 MiB each from here, by user SGPR.
const TABLES: u64 = 0x1000_0000;
/// The buffers the tables' descriptors point at, 64 MiB each.
const BUFFERS: u64 = 0x40_0000_0000;
/// The world: its TLAS here, its BLASes after it (base over 256 fits in a
/// dword, as the walk keeps it).
const WORLD: u64 = 0x80_0000_0000;
const WORLD_END: u64 = WORLD + 0x10_0000_0000;
/// Records of the play's buffers, and their count.
const STRIDE: u32 = 32;
const RECORDS: u32 = 0x10_0000;
/// The worlds a proof plays.
pub(crate) const WORLDS: u64 = 40;
/// Instructions one play may run.
const BUDGET: u64 = 4_000_000;

/// What the play saw.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub worlds: u64,
    /// Lanes whose results were compared, and of those, hits.
    pub lanes: usize,
    pub hits: usize,
    /// Lanes left out: a candidate at t-min or t-max, a walk past its cap,
    /// a ray grazing an edge.
    pub edges: usize,
    pub capped: usize,
    pub fragile: usize,
    /// Instructions both machines ran.
    pub instructions: u64,
}

/// Why a proof failed: in which world, at which instruction, and what was
/// seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub seed: u64,
    pub offset: usize,
    pub why: Why,
    pub detail: String,
}

/// A descriptor of a buffer at `base`: 32-byte records, a 32-bit float
/// format with its channels in order.
fn descriptor(base: u64, stride: u32, records: u32) -> [u32; 4] {
    let dst_sel = 4 | 5 << 3 | 6 << 6 | 7 << 9;
    [
        base as u32,
        (base >> 32) as u32 & 0xFFFF | stride << 16,
        records,
        dst_sel | 22 << 12,
    ]
}

/// A positive float in [2^-7, 2^9) whose two halves are sane f16s, in
/// [1, 4) and [0.25, 8).
fn sane(h: u64) -> u32 {
    let exponent = 120 + (h & 15) as u32;
    let high = (h >> 4 & 0x7F) as u32;
    let low_exponent = 13 + (h >> 11) as u32 % 5;
    let low = low_exponent << 10 | (h >> 16 & 0x3FF) as u32;
    exponent << 23 | high << 16 | low
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 33)).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    z = (z ^ (z >> 33)).wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    z ^ (z >> 33)
}

/// One play's world outside the wave.
pub(crate) struct Play<'w> {
    seed: u64,
    /// The structure's descriptor: user SGPR, byte offset in its table.
    tlas: (u8, u32),
    /// The TLAS header's root pointer, before the world is laid out.
    root: u32,
    layout: Option<&'w Layout>,
    world: Option<&'w World>,
    /// The guest walk's byte range, where reads must be the world's.
    walk: Option<(usize, usize)>,
    /// Memory the test planted (records it controls).
    pub planted: HashMap<u64, u32>,
    pub stores: Vec<(usize, u64, u32)>,
    pub answers: [Option<Answer>; LANES],
}

impl<'w> Play<'w> {
    pub fn new(seed: u64, site: &Site, root: u32) -> Self {
        Play {
            seed,
            tlas: (site.tlas.user_sgpr, u32::from(site.tlas.offset)),
            root,
            layout: None,
            world: None,
            walk: None,
            planted: HashMap::new(),
            stores: Vec::new(),
            answers: [None; LANES],
        }
    }

    /// The buffer a table slot's descriptor points at.
    pub fn buffer(user_sgpr: u8, offset: u32) -> u64 {
        BUFFERS + (u64::from(user_sgpr) * 256 + u64::from(offset / 16 % 256)) * 0x400_0000
    }
}

impl Env for Play<'_> {
    fn load(&mut self, inst: &Inst, at: u64) -> Result<u32, Why> {
        if let Some(&x) = self.planted.get(&at) {
            return Ok(x);
        }
        if (WORLD..WORLD_END).contains(&at) {
            if at == WORLD + 0x30 {
                return Ok(self.root);
            }
            return self
                .layout
                .and_then(|l| l.memory.get(&at).copied())
                .ok_or("the play reads world memory that was never planted");
        }
        if self
            .walk
            .is_some_and(|(a, b)| (a..b).contains(&inst.machine.offset))
        {
            return Err("the walk reads memory outside its world");
        }
        if (TABLES..TABLES + 0x40 * 0x10_0000).contains(&at) {
            let k = ((at - TABLES) / 0x10_0000) as u8;
            let offset = ((at - TABLES) % 0x10_0000) as u32;
            let slot = offset & !15;
            let d = if (k, slot) == self.tlas {
                descriptor(WORLD, 0, 0x1000_0000)
            } else {
                descriptor(Self::buffer(k, slot), STRIDE, RECORDS)
            };
            return Ok(d[(offset / 4 % 4) as usize]);
        }
        if (BUFFERS..WORLD).contains(&at) {
            return Ok(sane(mix(at ^ self.seed.rotate_left(17))));
        }
        Err("a read outside the play's memory")
    }

    fn store(&mut self, _: &Inst, lane: usize, at: u64, value: u32) -> Result<(), Why> {
        if !(BUFFERS..WORLD).contains(&at) {
            return Err("a store outside the play's buffers");
        }
        self.stores.push((lane, at, value));
        Ok(())
    }

    fn intersect(&mut self, base: u64, ray: &BvhRay) -> Result<[u32; 4], Why> {
        self.layout
            .ok_or("an intersection without a world")?
            .intersect(base, ray)
    }

    fn query(&mut self, lane: usize, ray: &QueryRay) -> Result<(bool, f32), Why> {
        let answer = self.world.ok_or("a query without a world")?.query(ray);
        self.answers[lane] = Some(answer);
        Ok((answer.hit, answer.t))
    }
}

/// A wave of `native` at its entry, group `group`: user SGPRs at the play's
/// tables, the group id, thread ids, EXEC full, the rest noise.
fn wave(p: &Program, native: &NativeProgram, seed: u64, group: u32) -> Result<Wave, Why> {
    let StageIo::Cs(io) = &native.io else {
        return Err("a proof by play of a program that is not a compute program");
    };
    if io.threads != [LANES as u32, 1, 1] {
        return Err("a proof by play of a group that is not one wave of 32");
    }
    let mut rng = Rng::new(mix(seed ^ 0x5741_5645));
    let mut noise = || rng.bits() as u32;
    let mut w = Wave {
        s: std::array::from_fn(|_| noise()),
        scc: false,
        v: (0..usize::from(p.vgprs).max(1))
            .map(|_| std::array::from_fn(|_| noise()))
            .collect(),
        lds: (0..(io.lds_bytes() / 4).max(1)).map(|_| noise()).collect(),
    };
    for k in 0..io.user_sgprs() {
        w.s[usize::from(k)] = (TABLES + u64::from(k) * 0x10_0000) as u32;
    }
    let (ids, info) = io.system_sgprs();
    for (n, id) in ids.iter().enumerate() {
        if let Some(s) = id {
            w.s[usize::from(*s)] = if n == 0 { group } else { 0 };
        }
    }
    if let Some(s) = info {
        w.s[usize::from(s)] = 0;
    }
    for k in 0..io.thread_id_vgprs() {
        w.v[usize::from(k)] = std::array::from_fn(|l| if k == 0 { l as u32 } else { 0 });
    }
    w.s[usize::from(EXEC)] = u32::MAX;
    w.s[usize::from(EXEC) + 1] = 0;
    Ok(w)
}

/// Lane `l`'s ray where the walk starts, from the site's registers.
fn ray_of(w: &Wave, site: &Site, l: usize) -> Ray {
    let f = |r: u8| f32::from_bits(w.v[usize::from(r)][l]);
    Ray {
        origin: site.origin.map(f),
        direction: site.direction.map(f),
        t_max: f(site.t_max),
    }
}

/// The values a walk leaves, lane by lane, where the code after it starts.
#[derive(Debug, Clone)]
struct Exit {
    exec: u32,
    regs: Vec<(u8, Kind, [u32; LANES])>,
}

/// One world played both ways: the guest's exit and stores, the spliced
/// program's answers and stores.
pub(crate) struct Played {
    pub world: World,
    guest_exit: Option<Exit>,
    entry_exec: u32,
    pub guest: Vec<(usize, u64, u32)>,
    pub host: Vec<(usize, u64, u32)>,
    pub answers: [Option<Answer>; LANES],
    pub instructions: u64,
}

/// The two machines of a proof and where the walk is.
pub(crate) struct Prover<'a> {
    site: &'a Site,
    native: &'a NativeProgram,
    guest_p: &'a Program,
    host_p: &'a Program,
    guest: Machine<'a>,
    host: Machine<'a>,
    header: usize,
    exit: usize,
    skip: Option<usize>,
}

fn block_at(p: &Program, offset: usize) -> Result<usize, Why> {
    let mut at = p
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.offset == offset);
    match (at.next(), at.next()) {
        (Some((b, _)), None) => Ok(b),
        _ => Err("a ray-tracing walk whose header or exit is not one block"),
    }
}

fn fail(seed: u64, f: Fail) -> Failure {
    Failure {
        seed,
        offset: f.offset,
        why: f.why,
        detail: String::new(),
    }
}

impl<'a> Prover<'a> {
    pub fn new(
        site: &'a Site,
        native: &'a NativeProgram,
        guest_p: &'a Program,
        host_p: &'a Program,
    ) -> Result<Self, Why> {
        Ok(Prover {
            site,
            native,
            guest_p,
            host_p,
            guest: Machine::new(guest_p, native.float_mode)?,
            host: Machine::new(host_p, native.float_mode)?,
            header: block_at(guest_p, site.header)?,
            exit: block_at(guest_p, site.exit)?,
            skip: site.skip.map(|s| block_at(guest_p, s)).transpose()?,
        })
    }

    /// Plays world `seed` of thread group `group`, of root `root`, both
    /// ways; `plant` puts the test's own records in memory, `build` makes
    /// the world from the rays at the walk's header.
    pub fn play(
        &self,
        seed: u64,
        group: u32,
        root: Root,
        plant: &HashMap<u64, u32>,
        build: &mut dyn FnMut(&[(usize, Ray)], &mut Rng) -> World,
    ) -> Result<Played, Failure> {
        let site = self.site;
        let mut rng = Rng::new(mix(seed.wrapping_add(0x574F_524C_44)));
        let mut budget = BUDGET;
        let err = |why| Failure {
            seed,
            offset: site.header,
            why,
            detail: String::new(),
        };
        // The guest, to the walk.
        let mut env = Play::new(seed, site, root_pointer(root));
        env.planted = plant.clone();
        env.walk = Some((site.header, site.exit));
        let mut w = wave(self.guest_p, self.native, seed, group).map_err(err)?;
        let stops = |b: usize| b == self.header || b == self.exit || Some(b) == self.skip;
        let at = self
            .guest
            .run(&mut w, &mut env, 0, stops, &mut budget)
            .map_err(|f| fail(seed, f))?;
        let entry_exec = w.exec();
        // No lane reached the walk (none in the tile mask, or the structure
        // read empty): the world stays empty, as the spliced program finds
        // it if it queries.
        let walked = at == Some(self.header);
        let world = if walked {
            let rays: Vec<(usize, Ray)> = lanes(entry_exec)
                .map(|l| (l, ray_of(&w, site, l)))
                .collect();
            build(&rays, &mut rng)
        } else {
            World::empty()
        };
        let layout = Layout::new(&world, WORLD, &mut rng);
        if walked && (world.root != root || layout.root != env.root) {
            return Err(err("a world whose root is not the one planted"));
        }
        env.layout = Some(&layout);
        env.world = Some(&world);
        // Through the walk, then to the end.
        let mut guest_exit = None;
        if let Some(b) = at {
            let at = if b == self.exit {
                Some(b)
            } else {
                self.guest
                    .run(&mut w, &mut env, b, |b| b == self.exit, &mut budget)
                    .map_err(|f| fail(seed, f))?
            };
            if let Some(b) = at {
                guest_exit = Some(Exit {
                    exec: w.exec(),
                    regs: site
                        .results
                        .iter()
                        .map(|&(r, k)| (r, k, w.v[usize::from(r)]))
                        .collect(),
                });
                env.walk = None;
                self.guest
                    .run(&mut w, &mut env, b, |_| false, &mut budget)
                    .map_err(|f| fail(seed, f))?;
            }
        }
        let guest = std::mem::take(&mut env.stores);
        let guest_instructions = BUDGET - budget;
        // The spliced program, from its entry.
        let mut budget = BUDGET;
        let mut env = Play::new(seed, site, root_pointer(root));
        env.planted = plant.clone();
        env.layout = Some(&layout);
        env.world = Some(&world);
        let mut w = wave(self.host_p, self.native, seed, group).map_err(err)?;
        self.host
            .run(&mut w, &mut env, 0, |_| false, &mut budget)
            .map_err(|f| fail(seed, f))?;
        let answers = env.answers;
        let host = std::mem::take(&mut env.stores);
        Ok(Played {
            world: world.clone(),
            guest_exit,
            entry_exec,
            guest,
            host,
            answers,
            instructions: guest_instructions + BUDGET - budget,
        })
    }

    /// Compares one world's two plays into `report`.
    pub fn compare(&self, seed: u64, played: &Played, report: &mut Report) -> Result<(), Failure> {
        let site = self.site;
        let wrong = |why: Why, detail: String| Failure {
            seed,
            offset: site.header,
            why,
            detail,
        };
        let mut left_out = 0u32;
        if let Some(exit) = &played.guest_exit {
            // Kept registers: EXEC as it was where the walk was entered (the
            // skip path enters no walk).
            let entered = played.answers.iter().any(Option::is_some);
            if site.kept.contains(&Reg::S(EXEC)) && entered && exit.exec != played.entry_exec {
                return Err(wrong(
                    "a ray-tracing walk that does not keep EXEC",
                    format!(
                        "{:#x} on entry, {:#x} on exit",
                        played.entry_exec, exit.exec
                    ),
                ));
            }
            for l in lanes(exit.exec) {
                let Some(answer) = played.answers[l] else {
                    return Err(wrong(
                        "a lane leaves the walk that the query does not answer",
                        format!("lane {l}"),
                    ));
                };
                let value = |kind: Kind| exit.regs.iter().find(|r| r.1 == kind).map(|r| r.2[l]);
                if let (Some(cap), Some(count)) = (site.cap, value(Kind::Count))
                    && count > cap
                {
                    report.capped += 1;
                    left_out |= 1 << l;
                    continue;
                }
                if answer.edge {
                    report.edges += 1;
                    left_out |= 1 << l;
                    continue;
                }
                if answer.fragile {
                    report.fragile += 1;
                    left_out |= 1 << l;
                    continue;
                }
                let hit = value(Kind::Hit).map(|x| x != 0);
                if hit.is_some_and(|h| h != answer.hit) {
                    return Err(wrong(
                        "a ray-tracing walk whose hit is not the host's",
                        format!("lane {l}: walk {hit:?}, host {answer:?}"),
                    ));
                }
                if let Some(t) = value(Kind::T) {
                    let t = f32::from_bits(t);
                    let close = (t - answer.t).abs() <= 1e-4 * answer.t.abs().max(1.0);
                    if !(close || t.to_bits() == answer.t.to_bits()) {
                        return Err(wrong(
                            "a ray-tracing walk whose t is not the host's",
                            format!("lane {l}: walk {t}, host {answer:?}"),
                        ));
                    }
                }
                report.lanes += 1;
                report.hits += usize::from(answer.hit);
            }
        }
        // The same stores, lane by lane.
        let by_address = |s: &[(usize, u64, u32)]| -> HashMap<u64, (usize, u32)> {
            s.iter().map(|&(l, at, x)| (at, (l, x))).collect()
        };
        let (g, h) = (by_address(&played.guest), by_address(&played.host));
        if g.len() != h.len() || g.keys().any(|k| !h.contains_key(k)) {
            return Err(wrong(
                "a spliced program that stores elsewhere than its walk's",
                format!("{} stores and {}", g.len(), h.len()),
            ));
        }
        for (at, &(l, x)) in &g {
            let (_, y) = h[at];
            if x == y || left_out >> l & 1 != 0 {
                continue;
            }
            let (a, b) = (f32::from_bits(x), f32::from_bits(y));
            if a.is_finite() && b.is_finite() && (a - b).abs() <= 1e-4 * b.abs().max(1.0) {
                continue;
            }
            return Err(wrong(
                "a spliced program that stores other values than its walk",
                format!("lane {l} at {at:#x}: walk {x:#x}, query {y:#x}"),
            ));
        }
        Ok(())
    }

    /// Plays the proof's worlds.
    pub fn prove(&self) -> Result<Report, Failure> {
        let mut report = Report::default();
        let mut misses = 0;
        for seed in 0..WORLDS {
            let root = match seed % 10 {
                0 => Root::Empty,
                1 | 6 => Root::Instance,
                _ => Root::Box,
            };
            let mask = u32::from(self.site.mask);
            let group = (mix(seed) % 64) as u32;
            let played = self.play(seed, group, root, &HashMap::new(), &mut |rays, rng| {
                world::around(rays, root, mask, rng)
            })?;
            let before = report.lanes - report.hits;
            self.compare(seed, &played, &mut report)?;
            misses += report.lanes - report.hits - before;
            report.worlds += 1;
            report.instructions += played.instructions;
        }
        // Worlds that exercised the walk: hits, misses, few lanes left out.
        if report.hits < 64 || misses < 64 || report.edges + report.fragile > report.lanes / 4 {
            return Err(Failure {
                seed: 0,
                offset: self.site.header,
                why: "a proof by play whose worlds do not exercise the walk",
                detail: format!("{report:?}"),
            });
        }
        Ok(report)
    }
}

/// Proves the walk at `site` in `p` by play (see the module notes).
pub(crate) fn prove(p: &Program, native: &NativeProgram, site: &Site) -> Result<Report, Failure> {
    let failure = |why| Failure {
        seed: 0,
        offset: site.header,
        why,
        detail: String::new(),
    };
    let spliced = splice(p.clone(), site).map_err(failure)?;
    Prover::new(site, native, p, &spliced)
        .map_err(failure)?
        .prove()
}
