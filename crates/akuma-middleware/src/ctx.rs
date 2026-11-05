//! What a hook is handed: [`KernelCtx`] (under the kernel lock, read-only), and the hook types.

use crate::calls::{Machine, Verdict};
use akuma_syscalls_abi::Syscall;

/// A user-memory access that did not go through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserError {
    /// The address (or a page of the range) does not translate.
    Fault,
    /// A string had no NUL inside the buffer.
    TooLong,
    /// A string was not UTF-8.
    NotUtf8,
}

impl UserError {
    /// The syscall result for it (`-EFAULT`, `-ENAMETOOLONG`, `-EINVAL`).
    #[must_use]
    pub const fn errno(self) -> i32 {
        match self {
            Self::Fault => 14,
            Self::TooLong => 36,
            Self::NotUtf8 => 22,
        }
    }
}

/// The context of a `kernel` hook: runs with the kernel lock held.
pub struct KernelCtx {
    /// The native syscall number (after the machine's legacy folding, where it has one).
    pub nr: u64,
    /// The call it names, if it names one.
    pub call: Option<Syscall>,
    /// The six arguments, as the program passed them. Read-only: a hook gets `&KernelCtx`, so nothing a hook does can change
    /// what the call sees (a guard's check always describes what runs).
    pub args: [u64; 6],
    /// The calling thread.
    pub tid: u64,
    /// The calling process.
    pub pid: u32,
    pub(super) machine: Machine,
}

impl KernelCtx {
    /// A context over `machine`, for a host test of a hook that lives in another crate (a kernel builds its contexts itself, in
    /// `run_kernel`). It forges nothing a hook could not already do: a `KernelCtx` is read-only and reaches memory only through
    /// the `Machine` it is given. Not for a kernel.
    #[doc(hidden)]
    #[must_use]
    pub fn for_test(nr: u64, args: [u64; 6], machine: Machine) -> Self {
        Self { nr, call: (machine.decode)(nr), args, tid: 1, pid: (machine.pid)(), machine }
    }

    /// Fill `buf` from the caller's memory at `addr`.
    ///
    /// # Errors
    /// [`UserError::Fault`] if a page does not translate.
    pub fn user_read(&self, addr: u64, buf: &mut [u8]) -> Result<(), UserError> {
        if (self.machine.copy_in)(buf, addr) { Ok(()) } else { Err(UserError::Fault) }
    }

    /// Write `data` to the caller's memory at `addr`.
    ///
    /// # Errors
    /// [`UserError::Fault`] if a page does not translate.
    pub fn user_write(&self, addr: u64, data: &[u8]) -> Result<(), UserError> {
        if (self.machine.copy_out)(addr, data) { Ok(()) } else { Err(UserError::Fault) }
    }

    /// A little-endian `u64` from the caller's memory.
    ///
    /// # Errors
    /// [`UserError::Fault`] if a page does not translate.
    pub fn user_read_u64(&self, addr: u64) -> Result<u64, UserError> {
        let mut b = [0u8; 8];
        self.user_read(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    /// Write a little-endian `u64` to the caller's memory.
    ///
    /// # Errors
    /// [`UserError::Fault`] if a page does not translate.
    pub fn user_write_u64(&self, addr: u64, v: u64) -> Result<(), UserError> {
        self.user_write(addr, &v.to_le_bytes())
    }

    /// A NUL-terminated UTF-8 string from the caller's memory, into `buf` (bytewise, so a string that ends
    /// just short of an unmapped page does not fault).
    ///
    /// # Errors
    /// [`UserError::Fault`], [`UserError::TooLong`] (no NUL in `buf`), [`UserError::NotUtf8`].
    pub fn user_str<'b>(&self, addr: u64, buf: &'b mut [u8]) -> Result<&'b str, UserError> {
        let mut n = 0;
        while n < buf.len() {
            let mut b = [0u8; 1];
            self.user_read(addr + n as u64, &mut b)?;
            if b[0] == 0 {
                return core::str::from_utf8(&buf[..n]).map_err(|_| UserError::NotUtf8);
            }
            buf[n] = b[0];
            n += 1;
        }
        Err(UserError::TooLong)
    }
}

/// `pre`.
pub type Pre = fn(&KernelCtx) -> Verdict;
/// `handler`: `Some(answer)` to serve the call, `None` to leave the `-ENOSYS`.
pub type Handler = fn(&KernelCtx) -> Option<u64>;
