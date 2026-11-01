//! The process family's pure decisions.
//!
//! Each x86 kernel (`amd64`, Taoxichuan) wrote its own `clone` flag test, its
//! own "does this `exit` end the thread or the process", and its own `execve`
//! argument accounting, and they had drifted: Taoxichuan made a thread of any
//! `CLONE_THREAD`, amd64 only of `CLONE_VM | CLONE_THREAD`; the argument caps
//! differed in what a string costs. This crate is the one answer each of them
//! asks for; what the answer *does* (`fork_process`, `clone_thread`, ending a
//! thread, copying strings out of ring 3) stays in the kernel.
//!
//! It knows no process, thread or address space (it does not depend on
//! `akuma-exec`), so every rule here has a host test.

#![no_std]
#![forbid(unsafe_code)]

/// `clone(2)`: which of the three things a caller asked for.
pub mod clone {
    use akuma_syscalls_linux::proc::clone_flags::{CLONE_SIGHAND, CLONE_THREAD, CLONE_VM};

    /// What a `clone` asks the kernel to make.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Kind {
        /// `CLONE_VM | CLONE_THREAD` (`pthread_create`): another thread of the caller's group.
        Thread,
        /// `CLONE_VM` without `CLONE_THREAD`: musl's `posix_spawn` child
        /// (`CLONE_VM | CLONE_VFORK | SIGCHLD`), which runs on `child_stack` and
        /// calls `execve` or `_exit` and nothing else. Served as a fork whose child
        /// enters on that stack: the address space is copied, not shared.
        SpawnFork { child_stack: u64 },
        /// No `CLONE_VM`: a fork. The child resumes on the parent's stack.
        Fork,
    }

    /// Linux's `EINVAL` combinations: `CLONE_THREAD` requires `CLONE_SIGHAND`,
    /// which requires `CLONE_VM`. musl's `pthread_create` always sets all three.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Invalid;

    /// Decode `clone(flags, child_stack, ...)`.
    ///
    /// The x86-only `fork` (57) and
    /// `vfork` (58) carry no flags word; a kernel maps both to [`Kind::Fork`]
    /// itself (vfork's "the parent waits" is not honoured by either kernel, and
    /// musl's callers only `waitpid`).
    pub fn decode(flags: u64, child_stack: u64) -> Result<Kind, Invalid> {
        let vm = flags & CLONE_VM != 0;
        if flags & CLONE_THREAD != 0 && flags & CLONE_SIGHAND == 0 {
            return Err(Invalid);
        }
        if flags & CLONE_SIGHAND != 0 && !vm {
            return Err(Invalid);
        }
        Ok(match (vm, flags & CLONE_THREAD != 0) {
            (true, true) => Kind::Thread,
            (true, false) => Kind::SpawnFork { child_stack },
            (false, _) => Kind::Fork,
        })
    }
}

/// `exit(2)` / `exit_group(2)`, and the wait status a death reports.
pub mod exit {
    /// What an exit ends.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Scope {
        /// Only the calling thread; the process runs on.
        Thread,
        /// The whole thread group.
        Group,
    }

    /// `exit_group` ends the group. `exit` ends only the caller when it is a
    /// thread (a `CLONE_THREAD` child), and the group when it is the group's
    /// leader.
    ///
    /// The leader case is a **pinned divergence** from Linux, in both kernels:
    /// there a leader's `exit` leaves the leader a zombie while its other
    /// threads run on. Here the leader's exit ends them. musl's `exit()` is
    /// `exit_group`, and a program that ends its main thread with a raw `exit`
    /// while others run is the case this changes.
    #[must_use]
    pub fn scope(exit_group: bool, caller_is_thread: bool) -> Scope {
        if !exit_group && caller_is_thread { Scope::Thread } else { Scope::Group }
    }

    /// The `wait4` status word of a death: `code` is an exit code (`0..=255`)
    /// or `-signal` for a death by that signal, the form `akuma-exec` stores
    /// in `Process::exit_code` and the child channel.
    #[must_use]
    pub fn wait_status(code: i32) -> u32 {
        if code < 0 {
            code.unsigned_abs() & 0x7F
        } else {
            (code.cast_unsigned() & 0xFF) << 8
        }
    }
}

/// `execve(2)`'s argument budget.
pub mod exec {
    /// The bytes one argument or environment string costs on the initial
    /// stack: its bytes, its NUL, and its pointer in `argv`/`envp`.
    #[must_use]
    pub const fn string_cost(len: usize) -> usize {
        len + 1 + 8
    }

    /// Linux's answer when the block does not fit: `E2BIG`. Never a truncated
    /// list: a command line that is well formed and wrong ran linkers without
    /// their inputs on both kernels before this was the rule.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct TooBig;

    /// Running totals for one `execve`'s strings, checked as each is read so a
    /// kernel never copies megabytes out of ring 3 for a block it would refuse.
    #[derive(Clone, Copy, Debug)]
    pub struct Budget {
        max_strings: usize,
        max_str_len: usize,
        max_bytes: usize,
        strings: usize,
        bytes: usize,
    }

    impl Budget {
        /// `max_strings` across argv and envp, `max_str_len` per string (without
        /// its NUL), `max_bytes` of [`string_cost`] in all: the kernel's initial
        /// stack layout decides all three.
        #[must_use]
        pub const fn new(max_strings: usize, max_str_len: usize, max_bytes: usize) -> Self {
            Self { max_strings, max_str_len, max_bytes, strings: 0, bytes: 0 }
        }

        /// Charge one string of `len` bytes, or refuse the whole call.
        pub fn charge(&mut self, len: usize) -> Result<(), TooBig> {
            if len > self.max_str_len || self.strings == self.max_strings {
                return Err(TooBig);
            }
            let bytes = self.bytes.saturating_add(string_cost(len));
            if bytes > self.max_bytes {
                return Err(TooBig);
            }
            self.strings += 1;
            self.bytes = bytes;
            Ok(())
        }

        /// Strings charged so far.
        #[must_use]
        pub const fn strings(&self) -> usize {
            self.strings
        }
    }
}

#[cfg(test)]
mod tests {
    use super::clone::{self, Kind};
    use super::exec::{Budget, TooBig, string_cost};
    use super::exit::{self, Scope};
    use akuma_syscalls_linux::proc::clone_flags::*;

    /// `SIGCHLD` as the low byte of a fork-style `clone`'s flags.
    const SIGCHLD: u64 = 0x11;

    #[test]
    fn clone_kinds() {
        // musl pthread_create.
        let pthread = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM
            | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
        assert_eq!(clone::decode(pthread, 0x7000), Ok(Kind::Thread));
        // musl posix_spawn.
        assert_eq!(
            clone::decode(CLONE_VM | CLONE_VFORK | SIGCHLD, 0x9000),
            Ok(Kind::SpawnFork { child_stack: 0x9000 })
        );
        // fork() on x86_64 musl is clone(SIGCHLD).
        assert_eq!(clone::decode(SIGCHLD, 0), Ok(Kind::Fork));
        // A stack without CLONE_VM is still a fork on the parent's stack.
        assert_eq!(clone::decode(SIGCHLD, 0x1234), Ok(Kind::Fork));
    }

    #[test]
    fn clone_refuses_what_linux_refuses() {
        assert_eq!(clone::decode(CLONE_THREAD | CLONE_VM, 0), Err(clone::Invalid), "THREAD without SIGHAND");
        assert_eq!(clone::decode(CLONE_SIGHAND, 0), Err(clone::Invalid), "SIGHAND without VM");
        assert_eq!(clone::decode(CLONE_THREAD | CLONE_SIGHAND, 0), Err(clone::Invalid), "THREAD without VM");
    }

    #[test]
    fn exit_scopes() {
        assert_eq!(exit::scope(true, true), Scope::Group);
        assert_eq!(exit::scope(true, false), Scope::Group);
        assert_eq!(exit::scope(false, true), Scope::Thread);
        assert_eq!(exit::scope(false, false), Scope::Group, "the pinned leader divergence");
    }

    #[test]
    fn wait_status_matches_the_wait_macros() {
        assert_eq!(exit::wait_status(0), 0);
        assert_eq!(exit::wait_status(7), 7 << 8); // WIFEXITED, WEXITSTATUS 7
        assert_eq!(exit::wait_status(256 + 3), 3 << 8, "only the low byte survives");
        assert_eq!(exit::wait_status(-11), 11); // WIFSIGNALED, WTERMSIG SIGSEGV
        assert_eq!(exit::wait_status(-9), 9);
    }

    #[test]
    fn budget_refuses_rather_than_truncates() {
        let mut b = Budget::new(3, 10, 1000);
        assert_eq!(b.charge(10), Ok(()));
        assert_eq!(b.charge(11), Err(TooBig), "one string over the per-string cap");
        assert_eq!(b.charge(0), Ok(()));
        assert_eq!(b.charge(0), Ok(()));
        assert_eq!(b.charge(0), Err(TooBig), "a fourth string");
        assert_eq!(b.strings(), 3);
        let mut small = Budget::new(10, 100, string_cost(5) + string_cost(5));
        assert_eq!(small.charge(5), Ok(()));
        assert_eq!(small.charge(5), Ok(()));
        assert_eq!(small.charge(0), Err(TooBig), "the byte total");
    }
}
