//! Device-removed extended data (DRED 1.2) beyond what `host.rs` reports:
//! diagnostics builds with `XTP_D3D12_DRED=1` ([`crate::host::dred_on`]).
//!
//! `host.rs` turns on auto-breadcrumbs and page-fault data and reports the
//! faulting address, the names of the allocations there and, for each list
//! that did not finish, the commands between the barriers around where it
//! stopped, with the texts of our markers (`HostList::note_op`) paired with
//! the breadcrumbs by order. This module adds:
//!
//! - the state DRED found the device in (hung, fault, page fault);
//! - for each list that stopped, its last finished command and its first
//!   unfinished one, each with the marker just before it: what the title's
//!   list ran there (draw, dispatch or copy, the pipeline and its program
//!   hashes, user data, root views and tables: `draw.rs` `op_text`);
//! - for the allocations DRED lists at a page fault, their host address
//!   range and where the fault lies in it;
//! - with `XTP_D3D12_DRED=contexts`, breadcrumb contexts
//!   (`ID3D12DeviceRemovedExtendedDataSettings1`): the runtime keeps each
//!   marker's text with its breadcrumb. Off otherwise: on the L4 they cost
//!   about 3 s a world frame and came back empty
//!   (docs/analysis/device-removed.md), and markers are then paired with
//!   our notes by order, as `host.rs` does.

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use crate::host::HostList;

/// The longest marker text the report quotes, in characters.
const QUOTE: usize = 1500;

/// Allocations listed per kind (existing, freed) at a page fault.
const ALLOCATIONS: usize = 8;

/// Whether `XTP_D3D12_DRED`'s value asks for breadcrumb contexts.
fn contexts_asked(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v.eq_ignore_ascii_case("contexts"))
}

/// Turns on the auto-breadcrumbs and page-fault data through the DRED 1.2
/// settings, and breadcrumb contexts when asked (`XTP_D3D12_DRED=contexts`),
/// before the host device is made (`host::create`), when device-removed
/// data is on.
pub(crate) fn enable() {
    if !crate::host::dred_on() {
        return;
    }
    let contexts = contexts_asked(std::env::var_os("XTP_D3D12_DRED").as_deref());
    let mut settings: Option<ID3D12DeviceRemovedExtendedDataSettings1> = None;
    // SAFETY: a plain interface query and settings calls, before the
    // device is made (DRED reads them at device creation).
    unsafe {
        match D3D12GetDebugInterface(&mut settings) {
            Ok(()) => {
                if let Some(s) = settings {
                    s.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                    s.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                    if contexts {
                        s.SetBreadcrumbContextEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                    }
                    tracing::info!(
                        contexts,
                        "device-removed data on: breadcrumbs, page faults, marker texts kept \
                         by DRED only with XTP_D3D12_DRED=contexts"
                    );
                }
            }
            Err(e) => tracing::warn!(error = %e, "no DRED 1.2 settings"),
        }
    }
}

/// What DRED 1.2 adds to the device-removed report (`Host::removal_data`),
/// for the lists of the submissions in flight (`runs`, by index) with the
/// texts of the markers each one set, in order (`notes`, by the same
/// index; empty without diagnostics): empty when the device has no
/// device-removed data.
pub(crate) fn report(device: &ID3D12Device, runs: &[&HostList], notes: &[Vec<String>]) -> String {
    let Ok(dred) = device.cast::<ID3D12DeviceRemovedExtendedData2>() else {
        return String::new();
    };
    let mut text = String::new();
    // SAFETY: the outputs are the runtime's, valid while the device lives;
    // nodes are walked as linked, histories and contexts have their counts.
    unsafe {
        text.push_str(&format!(
            "; DRED: device {}",
            state_name(dred.GetDeviceState())
        ));
        let mut fault = D3D12_DRED_PAGE_FAULT_OUTPUT2::default();
        if dred.GetPageFaultAllocationOutput2(&mut fault).is_ok() && fault.PageFaultVA != 0 {
            let va = fault.PageFaultVA;
            for (what, head) in [
                (
                    "allocations at the fault",
                    fault.pHeadExistingAllocationNode,
                ),
                (
                    "freed allocations at the fault",
                    fault.pHeadRecentFreedAllocationNode,
                ),
            ] {
                let found = allocations(head, va);
                if !found.is_empty() {
                    text.push_str(&format!("; {what} {va:#x}: {}", found.join(", ")));
                }
            }
        }
        let crumbs = match dred.GetAutoBreadcrumbsOutput1() {
            Ok(crumbs) => crumbs,
            Err(e) => {
                text.push_str(&format!("; no breadcrumbs with contexts ({e})"));
                return text;
            }
        };
        let mut lists = Vec::new();
        let mut node = crumbs.pHeadAutoBreadcrumbNode;
        while let Some(p) = node.as_ref() {
            lists.push(Crumbs::read(p, runs, notes));
            node = p.pNext;
        }
        text.push_str(&lists_text(&lists));
    }
    text
}

/// One list's breadcrumbs, read out of DRED's node.
struct Crumbs {
    /// Its index among the lists in flight, if it is one of ours.
    run: Option<usize>,
    /// Its debug name, if it has one.
    name: Option<String>,
    history: Vec<D3D12_AUTO_BREADCRUMB_OP>,
    /// How many of its commands finished, as far as DRED saw.
    done: u32,
    /// The marker texts, by the breadcrumb index they belong to.
    contexts: Vec<(u32, String)>,
    /// Where those texts come from.
    texts: Texts,
}

/// Where a list's marker texts come from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Texts {
    /// DRED's breadcrumb contexts.
    Dred,
    /// Our notes, the k-th one for the k-th marker command (as `host.rs`
    /// pairs them).
    Order,
    /// Neither: no contexts and no notes for the list.
    None,
}

impl Crumbs {
    /// # Safety
    /// `p` is a node of the runtime's breadcrumb output, still valid.
    unsafe fn read(
        p: &D3D12_AUTO_BREADCRUMB_NODE1,
        runs: &[&HostList],
        notes: &[Vec<String>],
    ) -> Self {
        // SAFETY: the node's pointers are the runtime's, with their counts.
        unsafe {
            let history = if p.pCommandHistory.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(p.pCommandHistory, p.BreadcrumbCount as usize).to_vec()
            };
            let done = p.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
            let contexts = if p.pBreadcrumbContexts.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(
                    p.pBreadcrumbContexts,
                    p.BreadcrumbContextsCount as usize,
                )
                .iter()
                .map(|c| {
                    let text = if c.pContextString.is_null() {
                        String::new()
                    } else {
                        c.pContextString.to_string().unwrap_or_default()
                    };
                    (c.BreadcrumbIndex, text)
                })
                .collect()
            };
            let name = (!p.pCommandListDebugNameW.is_null())
                .then(|| p.pCommandListDebugNameW.to_string().unwrap_or_default());
            let ours = (*p.pCommandList).as_ref().map(Interface::as_raw);
            // By the list, or by the name `host.rs` gives ours (its
            // `list_name`): NVIDIA's DRED gave a list's name and no pointer
            // to it (xcl#1592).
            let run = runs.iter().position(|r| {
                Some(r.list.as_raw()) == ours
                    || name.as_deref()
                        == Some(format!("xtp host list {:#x}", r.list.as_raw() as usize).as_str())
            });
            let ours = run.and_then(|i| notes.get(i)).filter(|n| !n.is_empty());
            let (contexts, texts) = match (contexts.is_empty(), ours) {
                (false, _) => (contexts, Texts::Dred),
                (true, Some(notes)) => (by_order(&history, notes), Texts::Order),
                (true, None) => (contexts, Texts::None),
            };
            Self {
                run,
                name,
                history,
                done,
                contexts,
                texts,
            }
        }
    }

    /// Where its marker texts come from, for the report.
    fn texts_text(&self) -> &'static str {
        match self.texts {
            Texts::Dred => "markers kept by DRED",
            Texts::Order => "markers paired with our notes by order: DRED kept no texts",
            Texts::None => "no marker texts: DRED kept none and the list has no notes",
        }
    }

    fn who(&self) -> String {
        match (self.run, &self.name) {
            (Some(i), _) => format!("list {i}"),
            (None, Some(name)) => format!("list {name:?}"),
            (None, None) => "a list not ours".to_owned(),
        }
    }
}

/// The lists' progress and, for each one that stopped part way, its last
/// finished command and its first unfinished one with their markers. With
/// none part way (DRED's count may lag the GPU), the last command of the
/// last finished list and the first of the first list not started.
fn lists_text(lists: &[Crumbs]) -> String {
    let count = |l: &Crumbs| l.history.len() as u32;
    let finished = lists.iter().filter(|l| l.done >= count(l)).count();
    let waiting = lists.iter().filter(|l| l.done == 0 && count(l) > 0).count();
    let mut text = format!(
        "; DRED lists: {} ({finished} finished, {waiting} not started)",
        lists.len()
    );
    let stopped: Vec<&Crumbs> = lists
        .iter()
        .filter(|l| l.done > 0 && l.done < count(l))
        .collect();
    for l in &stopped {
        text.push_str(&format!(
            "; {} stopped after {} of {} commands ({}): last done {}; first not done {}",
            l.who(),
            l.done,
            count(l),
            l.texts_text(),
            command_text(&l.history, &l.contexts, l.done as usize - 1),
            command_text(&l.history, &l.contexts, l.done as usize),
        ));
    }
    if stopped.is_empty() {
        if let Some(l) = lists
            .iter()
            .rev()
            .find(|l| l.done >= count(l) && count(l) > 0)
        {
            text.push_str(&format!(
                "; none stopped part way; the last finished, {}, ended with {}",
                l.who(),
                command_text(&l.history, &l.contexts, l.history.len() - 1),
            ));
        }
        if let Some(l) = lists.iter().find(|l| l.done == 0 && count(l) > 0) {
            text.push_str(&format!(
                "; the first not started, {}, begins with {}",
                l.who(),
                command_text(&l.history, &l.contexts, first_work(&l.history)),
            ));
        }
    }
    text
}

/// Command `at` of a list: its operation and the marker text nearest
/// before it (ours precede each draw, dispatch and copy, `note_op`).
fn command_text(
    history: &[D3D12_AUTO_BREADCRUMB_OP],
    contexts: &[(u32, String)],
    at: usize,
) -> String {
    let Some(&op) = history.get(at) else {
        return format!("command {at} (past the list's {})", history.len());
    };
    let marker = contexts
        .iter()
        .filter(|(index, _)| *index as usize <= at)
        .max_by_key(|(index, _)| *index);
    match marker {
        Some((index, text)) => {
            let quoted: String = text.chars().take(QUOTE).collect();
            let cut = if quoted.len() < text.len() { "…" } else { "" };
            format!(
                "command {at} ({}) after marker {index}: «{quoted}{cut}»",
                op_name(op)
            )
        }
        None => format!("command {at} ({}), no marker before it", op_name(op)),
    }
}

/// Our notes as marker texts: the k-th marker command of `history` gets
/// the k-th note (`HostList::note_op` sets one marker per note).
fn by_order(history: &[D3D12_AUTO_BREADCRUMB_OP], notes: &[String]) -> Vec<(u32, String)> {
    history
        .iter()
        .enumerate()
        .filter(|(_, op)| **op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
        .zip(notes)
        .map(|((at, _), note)| (at as u32, note.clone()))
        .collect()
}

/// The first command of a list that does work (not a marker, event or
/// barrier), or 0.
fn first_work(history: &[D3D12_AUTO_BREADCRUMB_OP]) -> usize {
    history
        .iter()
        .position(|op| {
            ![
                D3D12_AUTO_BREADCRUMB_OP_SETMARKER,
                D3D12_AUTO_BREADCRUMB_OP_BEGINEVENT,
                D3D12_AUTO_BREADCRUMB_OP_ENDEVENT,
                D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER,
                D3D12_AUTO_BREADCRUMB_OP_BARRIER,
            ]
            .contains(op)
        })
        .unwrap_or(0)
}

/// The allocations of a DRED list, with the host address range of those
/// that are buffers and where `va` lies against it.
///
/// # Safety
/// `head` is a node list of the runtime's page-fault output, still valid.
unsafe fn allocations(head: *const D3D12_DRED_ALLOCATION_NODE1, va: u64) -> Vec<String> {
    let mut found = Vec::new();
    let mut node = head;
    // SAFETY: the nodes are the runtime's, walked as linked.
    while let Some(n) = unsafe { node.as_ref() }
        && found.len() < ALLOCATIONS
    {
        let name = if n.ObjectNameW.is_null() {
            "unnamed".to_owned()
        } else {
            // SAFETY: a string of the runtime's output.
            unsafe { n.ObjectNameW.to_string() }.unwrap_or_default()
        };
        let range = (*n.pObject)
            .as_ref()
            .and_then(|o| o.cast::<ID3D12Resource>().ok())
            .and_then(|r| {
                // SAFETY: plain queries on a live resource.
                let (start, desc) = unsafe { (r.GetGPUVirtualAddress(), r.GetDesc()) };
                (start != 0).then_some((start, desc.Width))
            });
        let place = range.map_or_else(String::new, |(start, len)| {
            format!(" {start:#x}+{len:#x}{}", place(va, start, len))
        });
        found.push(format!(
            "{name:?} ({}){place}",
            allocation_name(n.AllocationType)
        ));
        node = n.pNext;
    }
    found
}

/// Where `va` lies against the range `start`, `len` bytes long.
fn place(va: u64, start: u64, len: u64) -> String {
    if va < start {
        format!(" (fault {:#x} below it)", start - va)
    } else if va - start < len {
        format!(" (fault at +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its end)", va - start - len)
    }
}

fn state_name(state: D3D12_DRED_DEVICE_STATE) -> String {
    match state {
        D3D12_DRED_DEVICE_STATE_HUNG => "hung (a command did not finish in time)".to_owned(),
        D3D12_DRED_DEVICE_STATE_FAULT => "faulted".to_owned(),
        D3D12_DRED_DEVICE_STATE_PAGEFAULT => "page fault".to_owned(),
        D3D12_DRED_DEVICE_STATE_UNKNOWN => "state unknown".to_owned(),
        other => format!("state {}", other.0),
    }
}

fn allocation_name(kind: D3D12_DRED_ALLOCATION_TYPE) -> String {
    match kind {
        D3D12_DRED_ALLOCATION_TYPE_RESOURCE => "resource".to_owned(),
        D3D12_DRED_ALLOCATION_TYPE_HEAP => "heap".to_owned(),
        D3D12_DRED_ALLOCATION_TYPE_DESCRIPTOR_HEAP => "descriptor heap".to_owned(),
        D3D12_DRED_ALLOCATION_TYPE_COMMAND_LIST => "command list".to_owned(),
        D3D12_DRED_ALLOCATION_TYPE_COMMAND_ALLOCATOR => "command allocator".to_owned(),
        D3D12_DRED_ALLOCATION_TYPE_PIPELINE_STATE => "pipeline state".to_owned(),
        D3D12_DRED_ALLOCATION_TYPE_QUERY_HEAP => "query heap".to_owned(),
        D3D12_DRED_ALLOCATION_TYPE_STATE_OBJECT => "state object".to_owned(),
        other => format!("type {}", other.0),
    }
}

/// The name of a breadcrumb operation (`D3D12_AUTO_BREADCRUMB_OP`).
fn op_name(op: D3D12_AUTO_BREADCRUMB_OP) -> String {
    const NAMES: [&str; 49] = [
        "set marker",
        "begin event",
        "end event",
        "draw",
        "indexed draw",
        "execute indirect",
        "dispatch",
        "copy buffer region",
        "copy texture region",
        "copy resource",
        "copy tiles",
        "resolve subresource",
        "clear render target",
        "clear unordered access",
        "clear depth stencil",
        "resource barrier",
        "execute bundle",
        "present",
        "resolve query data",
        "begin submission",
        "end submission",
        "decode frame",
        "process frames",
        "atomic copy buffer uint",
        "atomic copy buffer uint64",
        "resolve subresource region",
        "write buffer immediate",
        "decode frame 1",
        "set protected resource session",
        "decode frame 2",
        "process frames 1",
        "build acceleration structure",
        "emit acceleration structure post-build info",
        "copy acceleration structure",
        "dispatch rays",
        "initialize meta command",
        "execute meta command",
        "estimate motion",
        "resolve motion vector heap",
        "set pipeline state 1",
        "initialize extension command",
        "execute extension command",
        "dispatch mesh",
        "encode frame",
        "resolve encoder output metadata",
        "barrier",
        "begin command list",
        "dispatch graph",
        "set program",
    ];
    usize::try_from(op.0)
        .ok()
        .and_then(|i| NAMES.get(i))
        .map_or_else(|| format!("op {}", op.0), |name| (*name).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: D3D12_AUTO_BREADCRUMB_OP = D3D12_AUTO_BREADCRUMB_OP_SETMARKER;
    const DISPATCH: D3D12_AUTO_BREADCRUMB_OP = D3D12_AUTO_BREADCRUMB_OP_DISPATCH;
    const BARRIER: D3D12_AUTO_BREADCRUMB_OP = D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;

    fn crumbs(done: u32, history: &[D3D12_AUTO_BREADCRUMB_OP]) -> Crumbs {
        // A marker before each dispatch, its text naming the dispatch.
        let contexts = history
            .iter()
            .enumerate()
            .filter(|(_, op)| **op == MARKER)
            .map(|(at, _)| (at as u32, format!("dispatch at {}", at + 1)))
            .collect();
        Crumbs {
            run: Some(3),
            name: None,
            history: history.to_vec(),
            done,
            contexts,
            texts: Texts::Dred,
        }
    }

    #[test]
    fn contexts_are_asked_by_the_switch_value() {
        use std::ffi::OsStr;
        assert!(contexts_asked(Some(OsStr::new("contexts"))));
        assert!(contexts_asked(Some(OsStr::new("Contexts"))));
        assert!(!contexts_asked(Some(OsStr::new("1"))));
        assert!(!contexts_asked(None));
    }

    #[test]
    fn without_dred_texts_the_notes_are_paired_with_the_markers_by_order() {
        // As the L4's report: notes before a run of unmarked host builds.
        const BUILD: D3D12_AUTO_BREADCRUMB_OP =
            D3D12_AUTO_BREADCRUMB_OP_BUILDRAYTRACINGACCELERATIONSTRUCTURE;
        let history = [
            MARKER, DISPATCH, MARKER, DISPATCH, BARRIER, BUILD, BARRIER, BUILD, MARKER, DISPATCH,
        ];
        let notes: Vec<String> = ["copy", "dispatch 4x1x1", "dispatch 5x1x1", "unused"]
            .map(String::from)
            .to_vec();
        let contexts = by_order(&history, &notes);
        assert_eq!(
            contexts,
            vec![
                (0, "copy".to_owned()),
                (2, "dispatch 4x1x1".to_owned()),
                (8, "dispatch 5x1x1".to_owned()),
            ]
        );
        let list = Crumbs {
            run: Some(0),
            name: None,
            history: history.to_vec(),
            done: 7,
            contexts,
            texts: Texts::Order,
        };
        assert_eq!(
            lists_text(&[list]),
            "; DRED lists: 1 (0 finished, 0 not started); list 0 stopped after 7 of 10 \
             commands (markers paired with our notes by order: DRED kept no texts): last done \
             command 6 (resource barrier) after marker 2: «dispatch 4x1x1»; first not done \
             command 7 (build acceleration structure) after marker 2: «dispatch 4x1x1»"
        );
    }

    #[test]
    fn the_operation_names_follow_the_d3d12_values() {
        assert_eq!(op_name(D3D12_AUTO_BREADCRUMB_OP_SETMARKER), "set marker");
        assert_eq!(op_name(D3D12_AUTO_BREADCRUMB_OP_DISPATCH), "dispatch");
        assert_eq!(
            op_name(D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER),
            "resource barrier"
        );
        assert_eq!(
            op_name(D3D12_AUTO_BREADCRUMB_OP_COPYRAYTRACINGACCELERATIONSTRUCTURE),
            "copy acceleration structure"
        );
        assert_eq!(
            op_name(D3D12_AUTO_BREADCRUMB_OP_DISPATCHMESH),
            "dispatch mesh"
        );
        assert_eq!(op_name(D3D12_AUTO_BREADCRUMB_OP_SETPROGRAM), "set program");
        assert_eq!(op_name(D3D12_AUTO_BREADCRUMB_OP(77)), "op 77");
    }

    #[test]
    fn a_stopped_list_names_its_last_done_and_first_not_done_dispatches() {
        // marker, dispatch, marker, dispatch, barrier, marker, dispatch:
        // the GPU finished the first four commands.
        let list = crumbs(
            4,
            &[
                MARKER, DISPATCH, MARKER, DISPATCH, BARRIER, MARKER, DISPATCH,
            ],
        );
        let text = lists_text(&[list]);
        assert_eq!(
            text,
            "; DRED lists: 1 (0 finished, 0 not started); list 3 stopped after 4 of 7 \
             commands (markers kept by DRED): last done command 3 (dispatch) after marker 2: \
             «dispatch at 3»; first not done command 4 (resource barrier) after marker 2: \
             «dispatch at 3»"
        );
    }

    #[test]
    fn with_no_list_part_way_the_edges_of_finished_and_waiting_lists_are_named() {
        let finished = crumbs(2, &[MARKER, DISPATCH]);
        let waiting = crumbs(0, &[BARRIER, MARKER, DISPATCH]);
        let text = lists_text(&[finished, waiting]);
        assert_eq!(
            text,
            "; DRED lists: 2 (1 finished, 1 not started); none stopped part way; the last \
             finished, list 3, ended with command 1 (dispatch) after marker 0: «dispatch at \
             1»; the first not started, list 3, begins with command 2 (dispatch) after marker \
             1: «dispatch at 2»"
        );
    }

    #[test]
    fn a_command_without_a_marker_before_it_says_so() {
        assert_eq!(
            command_text(&[BARRIER, DISPATCH], &[], 1),
            "command 1 (dispatch), no marker before it"
        );
        assert_eq!(
            command_text(&[BARRIER], &[], 4),
            "command 4 (past the list's 1)"
        );
    }

    #[test]
    fn long_markers_are_cut() {
        let long = "x".repeat(QUOTE + 10);
        let text = command_text(&[MARKER, DISPATCH], &[(0, long)], 1);
        assert!(text.ends_with(&format!("«{}…»", "x".repeat(QUOTE))));
    }

    #[test]
    fn the_fault_is_placed_against_a_range() {
        assert_eq!(place(0x1000, 0x2000, 0x100), " (fault 0x1000 below it)");
        assert_eq!(place(0x2010, 0x2000, 0x100), " (fault at +0x10)");
        assert_eq!(place(0x2100, 0x2000, 0x100), " (fault 0x0 past its end)");
        assert_eq!(place(0x2180, 0x2000, 0x100), " (fault 0x80 past its end)");
    }
}
