# aio (Linux AIO) syscalls

`io_setup` (0) / `io_destroy` (1) / `io_submit` (2) / `io_cancel` (3) /
`io_getevents` (4). Source: `src/syscall/aio.rs`. Gated `sc-aio` (Tier 1 —
pure dead weight when off; see [`../syscalls.md`](../syscalls.md) "Feature
gates & ExecRuntime stubs"). Not to be confused with `io_uring_setup/enter/
register` (425/426/427), which are unconditionally `ENOSYS`'d in
`src/syscall/mod.rs` — this file only implements the older Linux AIO ABI.

> **Stability: B (watch).** `io_setup` does real work (a real ring buffer);
> everything else is a permanent, deliberate stub. The recurring lesson:
> **these stub returns must never be negative** — glibc/Go/Bun treat a
> negative `io_*` return as a pointer-arithmetic base (`x0 + offset`) rather
> than an errno, so `ENOSYS`/`EINVAL` here becomes a wild dereference crash,
> not a clean error.

## io_setup — the one real piece

`sys_io_setup` (`aio.rs:35`) exists because glibc's `io_getevents` wrapper
reads a `struct aio_ring` **directly out of shared memory** instead of always
trapping into the kernel — so `ctx_idp` must be a real, mapped, correctly
laid-out page, not an opaque handle:

1. `nr_events == 0` → `EINVAL`. Cap `nr_events` to `AIO_MAX_NR_EVENTS` (126,
   i.e. what fits `PAGE_SIZE - sizeof(aio_ring)` in one page) so a huge
   caller-supplied value can't be used to justify a large allocation.
2. If `*ctx_idp != 0` on entry, only `EEXIST` if that value is a still-live
   context id — Linux requires zero-on-entry, but some callers (Bun) pass
   uninitialized memory, so a dead/garbage value is tolerated rather than
   rejected.
3. Allocate one user page (`proc.memory.alloc_mmap` + `pmm::alloc_page_zeroed`
   + `map_user_page`, `RW_NO_EXEC`), write a `struct aio_ring` header into it
   (`id=0, head=0, tail=0, magic=0xa10a10a1, nr=capped_nr`).
4. Register the context in `AIO_CONTEXTS` keyed by **the ring's virtual
   address**, then write that same VA to `*ctx_idp`. The VA *is* the context
   handle — every other `io_*` syscall's `ctx` argument is that VA, looked up
   as a `AIO_CONTEXTS` key, not dereferenced.

## io_submit / io_cancel / io_getevents — permanent stubs

None of these ever submit or complete real I/O; the ring stays perpetually
`head == tail` (empty). Each just checks whether `ctx` is a known context
(purely for the debug log line) and returns `0`:

- `sys_io_submit` (`aio.rs:143`): always `0` submitted, even for an unknown
  `ctx`.
- `sys_io_cancel` (`aio.rs:157`): always `0` (nothing in flight to cancel).
- `sys_io_getevents` (`aio.rs:173`): always `0` events ready.

## io_destroy

`sys_io_destroy` (`aio.rs:187`): removes `ctx` from `AIO_CONTEXTS` if present
and returns `0` either way — including for an unknown `ctx`, where Linux
would return `EINVAL`. The mapped page itself is not unmapped; it stays
tracked in `proc.address_space` and is freed on process exit (leaving it
mapped read-only-in-practice after destroy is safe since callers never reuse
the address).

## Background

- `archive/BUN_MEMORY_STUDY.md` "Follow-up Crash: io_setup Ring Buffer Not
  Mapped" — the original bug this file's design fixes: the first
  implementation wrote a sequential integer (`ctx=1`) to `*ctx_idp` instead
  of a mapped VA; Bun's `io_getevents` wrapper dereferenced it immediately as
  a ring-buffer pointer and crashed with a null-deref at `FAR=0x0`.
- `archive/FIX_MEMORY_MAPPING.md` "Phase 9: AIO Stubs + MAP_SHARED Read-Only"
  and "10B: IC flush replays SVC with wrong register state → spurious
  io_setup" — hardening the stubs' return-code discipline and a spurious
  re-entry bug from instruction-cache-flush SVC replay.
- `archive/GOLANG_IPC.md` — the errno-as-pointer WILD-DA crash pattern that
  motivated the never-return-negative rule reiterated throughout this file's
  doc comments.
