// SPDX-License-Identifier: BSD-2-Clause
//! Per-thread seccomp state, shared immutably with children at fork or clone.

use akuma_primitives::errno::Errno;
use akuma_seccomp::{Data, FILTER_OVERHEAD, MAX_INSNS_PER_PATH, Program, Verdict};
use alloc::sync::Arc;
use spinning_top::Spinlock;

#[derive(Clone, Default)]
struct State {
    no_new_privs: bool,
    head: Option<Arc<Filter>>,
}

struct Filter {
    program: Program,
    older: Option<Arc<Self>>,
    path_cost: usize,
}

impl Drop for Filter {
    fn drop(&mut self) {
        // A chain can hold thousands of one-instruction filters. Unlink the
        // unique tail iteratively so teardown uses constant stack space.
        let mut older = self.older.take();
        while let Some(link) = older {
            let Some(mut filter) = Arc::into_inner(link) else {
                break;
            };
            older = filter.older.take();
        }
    }
}

#[derive(Default)]
pub struct Seccomp(Spinlock<State>);

impl Seccomp {
    /// Snapshot the calling thread's settings. A later install affects only
    /// that thread. The existing immutable chain needs no new allocation.
    #[must_use]
    pub fn inherit(&self) -> Self {
        Self(Spinlock::new(self.0.lock().clone()))
    }

    pub fn set_no_new_privs(&self) {
        self.0.lock().no_new_privs = true;
    }

    #[must_use]
    pub fn no_new_privs(&self) -> bool {
        self.0.lock().no_new_privs
    }

    #[must_use]
    pub fn has_filter(&self) -> bool {
        self.0.lock().head.is_some()
    }

    fn path_cost(head: Option<&Filter>, len: usize) -> usize {
        head.map_or(len, |filter| len + filter.path_cost + FILTER_OVERHEAD)
    }

    #[must_use]
    pub fn has_room(&self, len: usize) -> bool {
        Self::path_cost(self.0.lock().head.as_deref(), len) <= MAX_INSNS_PER_PATH
    }

    pub fn install(&self, program: Program) -> Result<(), Errno> {
        // Only the owning task installs. Allocate outside the lock so the
        // allocator may reclaim without a seccomp lock held.
        let older = self.0.lock().head.clone();
        let path_cost = Self::path_cost(older.as_deref(), program.instruction_count());
        if path_cost > MAX_INSNS_PER_PATH {
            return Err(Errno::ENOMEM);
        }
        let filter = Arc::try_new(Filter {
            program,
            older,
            path_cost,
        })
        .map_err(|_| Errno::ENOMEM)?;
        self.0.lock().head = Some(filter);
        Ok(())
    }

    #[must_use]
    pub fn evaluate(&self, data: &Data) -> Verdict {
        let state = self.0.lock();
        let mut verdict = Verdict::ALLOW;
        let mut next = state.head.as_deref();
        while let Some(filter) = next {
            verdict.consider(filter.program.evaluate(data));
            next = filter.older.as_deref();
        }
        verdict
    }
}
