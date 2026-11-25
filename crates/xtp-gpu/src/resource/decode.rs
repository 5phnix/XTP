//! Tracks texel versions for R9G9B9E5 decoding. Consecutive reads in a
//! single command list share one decode. Across lists, submission order
//! determines whether the conversion runs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::HostTexture;
use crate::host::HostList;
use crate::object::This;
use crate::texture::Predicate;

pub(super) fn revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[derive(Default)]
pub(crate) struct Uses {
    // Maps each destination to its source since the last write to either.
    reads: HashMap<usize, usize>,
    // One version update suffices between reads.
    writes: HashSet<usize>,
    // A command can read an alias before writing its UAV.
    uavs: Vec<Arc<HostTexture>>,
}

impl Uses {
    pub(crate) fn clear(&mut self) {
        self.reads.clear();
        self.writes.clear();
        self.begin_bind();
    }

    pub(crate) fn begin_bind(&mut self) {
        self.uavs.clear();
    }

    pub(super) fn current(&self, from: &Arc<HostTexture>, to: &Arc<HostTexture>) -> bool {
        self.reads.get(&(Arc::as_ptr(to) as usize)) == Some(&(Arc::as_ptr(from) as usize))
    }

    fn write(&mut self, list: &HostList, copy: &Arc<HostTexture>) {
        let key = Arc::as_ptr(copy) as usize;
        if self.writes.insert(key) {
            self.reads.retain(|&to, from| to != key && *from != key);
            let copy = copy.clone();
            list.defer_host((0, 0), move || {
                copy.content.store(revision(), Ordering::Relaxed)
            });
        }
    }
}

fn uses(list: &HostList) -> std::sync::MutexGuard<'_, Uses> {
    list.decode_uses.lock().unwrap_or_else(|e| e.into_inner())
}

fn tracked(resource: This) -> Option<&'static Arc<HostTexture>> {
    if resource.is_null() {
        return None;
    }
    super::resource(resource)
        .host
        .as_ref()
        .filter(|copy| copy.decode_tracked)
}

pub(crate) fn wrote(list: &HostList, resource: This) {
    if let Some(copy) = tracked(resource) {
        uses(list).write(list, copy);
    }
}

pub(crate) fn bound_uav(list: &HostList, resource: This) {
    if let Some(copy) = tracked(resource) {
        uses(list).uavs.push(copy.clone());
    }
}

pub(crate) fn bound_writes(list: &HostList) {
    let mut uses = uses(list);
    while let Some(copy) = uses.uavs.pop() {
        uses.write(list, &copy);
    }
}

pub(super) fn recorded(
    list: &HostList,
    from: &Arc<HostTexture>,
    to: &Arc<HostTexture>,
    predicate: Option<Predicate>,
    #[cfg(feature = "diagnostics")] count: Option<Arc<AtomicU64>>,
) {
    let mut uses = uses(list);
    let (source, dest) = (Arc::as_ptr(from) as usize, Arc::as_ptr(to) as usize);
    uses.reads.insert(dest, source);
    uses.writes.remove(&source);
    uses.writes.remove(&dest);
    let (from, to) = (from.clone(), to.clone());
    list.defer_host((0, 0), move || {
        let version = from.content.load(Ordering::Relaxed);
        let previous = to.content.swap(version, Ordering::Relaxed);
        let runs = predicate
            .as_ref()
            .is_none_or(|p| p.decide(previous != version));
        #[cfg(test)]
        if runs {
            to.decode_count.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(feature = "diagnostics")]
        if runs && let Some(count) = &count {
            count.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(not(any(test, feature = "diagnostics")))]
        let _ = runs;
    });
}
