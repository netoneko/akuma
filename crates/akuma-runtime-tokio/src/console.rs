//! Stdio of a process with no I/O channel goes to the kernel's console.
//!
//! Glue would buffer a `write` to `Stdout`/`Stderr`/`DevTty` in a channel nothing
//! drains on a tokio kernel; init and every process forked from it write their
//! stdio this way. Shared by both machine halves (moved from the amd64 half,
//! B21 A5.6); each decodes its own `write`/`writev` numbers.

use akuma_exec::process::FileDescriptor as F;
use akuma_exec::process::user_access;

/// Cap on one `write(2)`: the console is a serial line, not a disk.
pub const WRITE_LIMIT: u64 = 1 << 20;
const EFAULT: u64 = 14;

/// `write` (or, `vectored`, `writev`) of the caller's stdio to `putb`.
///
/// `a` is the call's arguments; it applies when `fd` is the caller's stdio and
/// it has no channel, and answers the count as the syscall's return. `None` for every
/// other descriptor or process, so the fd layer serves it.
#[must_use]
pub fn stdio_write(vectored: bool, a: [u64; 6], putb: fn(u8)) -> Option<u64> {
    // `Process.channel`, not `current_channel()`: the latter falls back to the
    // per-thread exit channel every forked child is registered with for its
    // parent's wait, and glue would buffer the child's stdout there, unread.
    let p = akuma_exec::process::current_process_shared()?;
    if p.channel.is_some() {
        return None;
    }
    if !p.with_fd(a[0] as u32, |d| matches!(d, F::Stdout | F::Stderr | F::DevTty))? {
        return None;
    }
    let emit = |base: u64, len: u64| -> u64 {
        let mut chunk = [0u8; 256];
        let mut done = 0u64;
        while done < len {
            let n = ((len - done) as usize).min(chunk.len());
            if user_access::copy_from_user(&mut chunk[..n], base + done).is_err() {
                return if done == 0 { 0u64.wrapping_sub(EFAULT) } else { done };
            }
            for &b in &chunk[..n] {
                putb(b);
            }
            done += n as u64;
        }
        done
    };
    if !vectored {
        return Some(emit(a[1], a[2].min(WRITE_LIMIT)));
    }
    let (iov, cnt) = (a[1], a[2]);
    let mut total = 0u64;
    for i in 0..cnt.min(1024) {
        let mut entry = [0u64; 2];
        if user_access::read_user_into(&mut entry, iov + i * 16).is_err() {
            return Some(if total == 0 { 0u64.wrapping_sub(EFAULT) } else { total });
        }
        let n = emit(entry[0], entry[1].min(WRITE_LIMIT));
        if n.cast_signed() < 0 {
            return Some(if total == 0 { n } else { total });
        }
        total += n;
    }
    Some(total)
}
