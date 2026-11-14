//! The exec policy: "may this process get new code?", and the kernel-facing entry points that ask it.

use crate::dispatch::active;

/// What the kernel is about to do that puts new code into a process. There are exactly three ways, and the kernel asks about each
/// in one place (see [`ExecPolicy`]).
#[derive(Clone, Copy, Debug)]
pub enum ExecKind<'a> {
    /// `execve` (and init, and a `#!` interpreter) is about to replace the process with this image. Asked before anything is torn
    /// down, so a refusal leaves the caller exactly as it was. `interp` is the `PT_INTERP` path of a dynamically linked image.
    ///
    /// `path` is the symlink-resolved path the kernel read (`/bin/ls` arrives as `/bin/busybox`), **not normalised**: a policy that
    /// matches on it must refuse a relative one and `..`/`//` itself. `image` is the kernel's own copy of the whole file, never mapped
    /// to user space, so it cannot change while a policy reads it. `sidecar` is the contents of `<path><suffix>` (at most 4 KiB) when
    /// the table asks for one ([`Hooks::exec_sidecar`](crate::Hooks::exec_sidecar)) and the file exists; `None` otherwise.
    Image { path: &'a str, interp: Option<&'a str>, image: &'a [u8], sidecar: Option<&'a [u8]> },
    /// `mmap` with `PROT_EXEC`. `inode` is the regular file being mapped, `None` for anonymous memory.
    Map { inode: Option<u32> },
    /// `mprotect` adding `PROT_EXEC`. The kernel does not track which file a present page came from, so there is no identity.
    Protect,
}

/// An [`ExecKind`] and who is asking.
#[derive(Clone, Copy, Debug)]
pub struct ExecEvent<'a> {
    pub pid: u32,
    pub kind: ExecKind<'a>,
}

/// What an [`ExecPolicy`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecVerdict {
    Allow,
    /// Allow, **and map the image from the bytes this policy was shown**. For an `Image` event the loader then places every page
    /// from `image` (no lazy file record, no second lookup of the path), so the code that runs is exactly the code that was checked
    /// even if the file is rewritten, truncated or renamed afterwards. Costs the shared file-page cache for that image. For `Map` and
    /// `Protect` it is the same as `Allow`.
    AllowFromImage,
    /// Refuse, with this positive errno (`13` is `EACCES`).
    Deny(i32),
}

/// The one hook for "may this process get new code?".
///
/// It is called for every way the kernel introduces code (an image, a mapping, a protection change), so a policy is written once
/// as a `match` and cannot be bypassed by a path nobody hooked. Read-only, like every hook: it can allow or refuse, never change
/// what runs.
pub type ExecPolicy = fn(&ExecEvent) -> ExecVerdict;

/// Ask the registered [`ExecPolicy`] whether the process may get new code.
///
/// `Some(errno)` (positive) means refuse; no middleware, or no policy in it, means `None`. Called from the loader (before anything
/// is torn down) and from `mmap` and `mprotect`.
#[must_use]
pub fn exec_event(kind: &ExecKind<'_>) -> Option<u64> {
    match active()?.exec_event(kind) {
        ExecVerdict::Allow | ExecVerdict::AllowFromImage => None,
        ExecVerdict::Deny(no) => Some(no.unsigned_abs().into()),
    }
}

/// The suffix of the file the loader should also read next to an image (see [`Hooks::exec_sidecar`](crate::Hooks::exec_sidecar)), if the table wants one.
#[must_use]
pub fn exec_sidecar() -> Option<&'static str> {
    active()?.exec_sidecar()
}

/// [`exec_event`] with the whole answer: the loader needs to tell `Allow` from `AllowFromImage`. No middleware: `Allow`.
#[must_use]
pub fn exec_verdict(kind: &ExecKind<'_>) -> ExecVerdict {
    active().map_or(ExecVerdict::Allow, |mw| mw.exec_event(kind))
}
