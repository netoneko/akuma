# Fix: EL1 Crash During epoll_pwait / Stack Overflow / DNS Hang in Bun

## Date

2026-03-13

---

## Problems (in order of discovery)

### Crash 1 — EL1 data abort (kernel panic)

```
EC=0x25  — Synchronous Data Abort from current EL (kernel, EL1)
FAR=0x50004000
DFSC=0x07 — Translation fault, level 3 (leaf PTE not present)
STR x11, [x9]  (x9 = 0x50004000)
```

The kernel panicked instead of recovering. The faulting write was to a user VA
inside Bun's JSC mmap (`[0x50000000, 0x90000000)`).

### Crash 2 — User process SIGSEGV (stack overflow)

```
[WILD-DA] pid=44 FAR=0x203ffdff60 ELR=0x2d1d870 last_sc=113
[signal] sig 11 frame page 0x203ffdf000 not mappable
Process 44 (/bin/opencode) SIGSEGV after 0.63s
```

Bun uses more than 512 KB of stack during startup (JSC initialization is ~600 KB).
The eagerly-allocated stack had no lazy backing region below it, so growth past the
initial allocation caused an unhandled translation fault. Signal delivery then also
failed because the signal frame page was equally unmapped.

### Regression 1 — EFAULT on `//package.json` (introduced and reverted)

`bun install @google/gemini-cli` returned:

```
error: failed to read package.json "//package.json": EFAULT
```

An attempted fix for Crash 1 added a kernel VA exclusion `[0x4000_0000, 0x6000_0000)`
to `validate_user_ptr`. Bun's JSC heap mmap starts at `0x50000000`, so path strings
allocated in the JSC heap were inside the excluded range and incorrectly returned
EFAULT. The exclusion was reverted.

### Regression 2 — SSH drops / kernel loops on EC=0x25

After adding the EL1 recovery (return after killing the process), the kernel handler
did `eret` back to `ELR_EL1` — which still pointed at the faulting instruction. The
fault fired again immediately, re-entering the handler, recursing until the kernel
stack overflowed and corrupted state including the SSH server.

Fix: the handler redirects `ELR_EL1` to an `el1_fault_recovery_pad()` function before
returning, so `eret` lands in the recovery pad instead of the faulting instruction.

### Regression 3 — DNS hangs after fork

`bun install` would start, then hang indefinitely on DNS resolution.

**Root cause:** When `bun` forks child processes (e.g. for lifecycle scripts),
the child inherits the parent's fd table, including `EpollFd(epoll_id)` entries.
Both parent and child point to the **same `epoll_id`** in the global `EPOLL_TABLE`.
Our new `sys_close` arm calls `epoll_destroy(epoll_id)` when an epoll fd is closed.
If the child explicitly calls `close(epoll_fd)` — or if any code path triggers
`sys_close` on that fd — `epoll_destroy` removes the shared global instance. The
parent's next `epoll_pwait` call then gets EBADF and the DNS event loop breaks
silently.

Additionally, `EPOLL_CLOEXEC` was being ignored (`_flags` parameter was unused in
`sys_epoll_create1`), so epoll fds were never marked as close-on-exec even when
requested.

### Regression 4 — EL1 fault loop (FAR=0x1)

With the ELR+4 fix, the first fault was skipped, but the next instruction in the
sequence used the same corrupt register (e.g. x9 still held the bad address) and
faulted again. Each +4 advance put us deeper into the same broken instruction
sequence. The cascade was observed as:

```
[Exception] Sync from EL1: EC=0x25, ISS=0x47
  ELR=0x403d8b5c, FAR=0x1
  WARNING: Kernel accessing user-space address!
  (repeating)
```

`FAR=0x1` was not the original fault address — it was a cascaded fault after the
ELR+4 hack drifted into other instructions that happened to use the same poisoned
register. The process exit code was -137 (killed externally due to the hung state).

---

## Root Causes (summary)

| # | Bug | Root cause |
|---|-----|-----------|
| 1 | EL1 panic | `rust_sync_el1_handler` always looped on `wfe`, no recovery |
| 2 | Stack overflow SIGSEGV | Stack eagerly allocated to fixed size, no lazy growth region |
| 3 | EFAULT on JSC heap addrs | Kernel VA exclusion in `validate_user_ptr` overlapped JSC mmap |
| 4 | SSH crash | EL1 recovery returned to faulting instruction → recursion → stack overflow |
| 5 | DNS hang | `epoll_destroy` in `sys_close` shared-with-child instances; `EPOLL_CLOEXEC` ignored |
| 6 | Infinite fault loop | ELR+4 moved to next instruction using same bad register → re-faulted |

---

## Why EC=0x25 happens at all

The syscall code writes directly to user VA pointers at EL1:

```rust
core::ptr::write_unaligned(events_ptr as *mut EpollEvent, out_event)
```

On AArch64 during a syscall, low VAs are translated through TTBR0 (user page
table). If the page is not yet mapped, this write takes an EC=0x25 data abort.
`validate_user_ptr` tries to pre-map the page via `ensure_user_pages_mapped`, but
there are TLB coherency or TTBR0-swap scenarios where the page isn't visible to the
subsequent write even after mapping.

The principled fix is a `copy_to_user()` and `copy_from_user()` primitive set backed by an exception fixup mechanism. 

This was implemented on 2026-03-13:
1.  **Thread Infrastructure:** Added `user_copy_fault_handler` to `ThreadSlot`.
2.  **Exception Handling:** `rust_sync_el1_handler` now checks this handler and redirects `ELR_EL1` to a recovery path if a fault occurs during safe user access.
3.  **Safe Primitives:** Implemented `copy_from_user_safe` and `copy_to_user_safe` in `crates/akuma-exec/src/mmu/user_access.rs` using AArch64 assembly.
4.  **Syscall Hardening:** Refactored `sys_read`, `sys_write`, `sys_epoll_pwait`, `sys_ppoll`, `sys_pselect6`, and other metadata-related syscalls to use these safe primitives instead of raw pointer dereferences.

This provides robust protection against TLB coherency issues and race conditions (TOCTTOU) when accessing user memory from the kernel.

---

## Fixes

### Fix 1 — EL1 data-abort recovery with landing pad (`src/exceptions.rs`)

Added `el1_fault_recovery_pad()` — a function that just calls `yield_now()` in a
loop. When EC=0x25 fires with ELR in kernel code range `[0x4020_0000, 0x6000_0000)`:

1. Marks the current process as Zombie (exit code −14 / EFAULT)
2. Calls `kill_thread_group` to terminate all threads of the process
3. Sets `ELR_EL1` to `el1_fault_recovery_pad` (not ELR+4)
4. Returns — `eret` lands in the pad which yields; scheduler cleans up the slot

Redirecting to the pad (rather than +4) prevents the fault cascade where the next
instruction uses the same poisoned register and faults again.

### Fix 2 — Epoll cleanup on explicit close (`src/syscall/poll.rs`, `src/syscall/fs.rs`)

Added `pub(super) fn epoll_destroy(epoll_id: u32)` that removes the instance from
`EPOLL_TABLE`. `sys_close` now matches `EpollFd` and calls `epoll_destroy`. The
`close_cloexec_fds` exec path also calls `epoll_destroy` for cloexec epoll fds.

### Fix 3 — Lazy demand-paged stack region (`crates/akuma-exec/src/process/mod.rs`)

All four process-creation paths (`from_elf`, `from_elf_path`, `replace_image`,
`replace_image_from_path`) now register a 32 MB lazy anonymous region covering
`[stack_top − 32 MB, stack_top)`.

```
const LAZY_STACK_MAX: usize = 32 * 1024 * 1024;
push_lazy_region(pid, stack_top.saturating_sub(LAZY_STACK_MAX), LAZY_STACK_MAX, RW_NO_EXEC);
```

Physical pages are only allocated on fault. This also fixes signal delivery to a
process whose stack has grown past the initial eager pages.

### Fix 4 — Don't inherit EpollFd across fork (`crates/akuma-exec/src/process/mod.rs`)

Both fork paths now strip `EpollFd` entries from the child's fd table. Children
exec immediately and don't need the parent's epoll state; inheriting it causes
`epoll_destroy` to nuke the parent's shared global instance when the child closes
that fd.

### Fix 5 — Honor `EPOLL_CLOEXEC` (`src/syscall/poll.rs`)

`sys_epoll_create1` previously ignored the `flags` parameter. It now calls
`proc.set_cloexec(fd)` when `EPOLL_CLOEXEC` is set.

---

## Why the validate_user_ptr Kernel-VA Check Was Removed

A proposed fix excluded `[0x4000_0000, 0x6000_0000)` from `validate_user_ptr`.
This was wrong because:

- Kernel code loads at physical `0x40200000`. With 1 GB RAM the kernel heap can
  extend to `~0x50200000`.
- Bun/JSC maps a 1 GB "gigacage" starting at `0x50000000`.
- The range `[0x50000000, 0x50200000)` is in both the kernel heap and the JSC mmap.
  No fixed VA exclusion can cover the kernel heap without also rejecting valid user
  addresses.
- The EL1 landing-pad recovery is the correct defence: if a write to a user VA faults
  at EL1, the kernel kills only the offending process and continues.

### Regression 5 — Socket table exhaustion / second `bun install` hangs on DNS

After the EL1 recovery fix, `bun install` of small packages (express) worked, but
`bun install @google/gemini-cli` (200+ packages) crashed with:

```
[WILD-DA] pid=44 FAR=0xfffffffffffffffa ELR=0x2d1d870 last_sc=113
[signal] sig 11 frame page 0x203ffdf000 not mappable
```

A second `bun install express` would then hang indefinitely on DNS resolution, but
only if `node_modules` was still present.

**Root cause:** `el1_fault_recovery_pad` looped calling `yield_now()` forever. The
process was marked Zombie but:
1. Its socket fds were never cleaned up (`cleanup_process_fds` was not called for
   the faulting process itself — only for sibling threads via `kill_thread_group`)
2. The thread slot occupied the scheduler run-queue permanently

Each crash leaked all of the process's open sockets (TCP connections to npm registry,
UDP sockets for DNS). With MAX_SOCKETS=128, after a few crashes the socket table
was full. The next `bun install` could not allocate a UDP socket for DNS, causing the
resolver to hang waiting for a response from a socket that was never created.

The "remove node_modules fixes it" observation: removing `node_modules` changes which
packages Bun tries to install, resulting in fewer syscalls between the crash point and
DNS resolution — by chance the socket table happens to not be exhausted yet.

**Fix:** `el1_fault_recovery_pad` now calls `return_to_kernel(-14)` instead of
yielding forever. Since ERET from an EL1 exception restores SPSR_EL1 (which contains
EL1 mode bits), the function runs at EL1 and can safely call kernel code.
`return_to_kernel` calls `cleanup_process_fds`, removes the channel, deactivates the
address space, and unregisters the process — exactly the same cleanup path as a
normal process exit.

---

## Files Changed (updated)

| File | Change |
|------|--------|
| `src/exceptions.rs` | EL1 abort recovery with landing pad; `el1_fault_recovery_pad()` now calls `return_to_kernel(-14)` |
| `src/syscall/poll.rs` | `epoll_destroy()`; honor `EPOLL_CLOEXEC` in `epoll_create1` |
| `src/syscall/fs.rs` | `sys_close` — `EpollFd` arm calls `epoll_destroy` |
| `src/syscall/proc.rs` | `execve` cloexec path — `EpollFd` arm calls `epoll_destroy` |
| `crates/akuma-exec/src/process/mod.rs` | 32 MB lazy stack region; strip `EpollFd` on fork |

---

## Known Limitation

`epoll_destroy` is not reference-counted. If two processes share an epoll id (e.g.
via `dup` across a fork without our EpollFd-stripping fix), closing either fd
destroys the shared instance. The current mitigation is to strip EpollFd from forked
children. A proper fix would use a per-instance refcount incremented on fork/dup and
decremented on close.
