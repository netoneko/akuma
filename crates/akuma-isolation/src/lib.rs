#![no_std]

extern crate alloc;

pub mod mount;
pub mod net;
pub mod subdir_fs;

use alloc::sync::Arc;
use mount::MountNamespace;
use net::NetworkNamespace;
use spinning_top::Spinlock;

pub struct Namespace {
    pub id: u64,
    pub mount: Spinlock<MountNamespace>,
    pub net: NetworkNamespace,
}

impl Namespace {
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            mount: Spinlock::new(MountNamespace::new()),
            net: NetworkNamespace::Shared,
        }
    }
}

static GLOBAL_NAMESPACE: Spinlock<Option<Arc<Namespace>>> = Spinlock::new(None);

/// Returns a shared reference to the global (host) namespace.
/// Creates it on first call.
pub fn global_namespace() -> Arc<Namespace> {
    let mut slot = GLOBAL_NAMESPACE.lock();
    if let Some(ns) = slot.as_ref() {
        return ns.clone();
    }
    let ns = Arc::new(Namespace::new(0));
    *slot = Some(ns.clone());
    ns
}
