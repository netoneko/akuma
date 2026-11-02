# B9's remainder: `madvise`, `pidfd_open`, `rt_sigsuspend` (2026-11-02)

**Stability: B**: three kernels changed; gates below.

B9 of [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md) named four tokio gaps. Signal delivery and `kill` landed
with the B9 row ([`TAOXICHUAN_SIGNAL_DELIVERY.md`](TAOXICHUAN_SIGNAL_DELIVERY.md)); `madvise` (28) and `pidfd_open` (434)
stayed `[nosys]` on Taoxichuan, and amd64 still answered `rt_sigsuspend` with `ENOSYS`. This closes all three. Running
new probes against Linux 6.17 (Pop on the ryzen) first found two more defects, one in glue, one in every kernel.

## What changed

**`madvise` on Taoxichuan** (`madvise_arm`, `taoxichuan/src/main.rs`). The advice decode and the per-page rule are
`akuma-syscalls-mem`'s, as on the other kernels: `MADV_FREE` is `EINVAL` (musl's mallocng probes it and reads `EINVAL` as
"not supported"; the `[nosys] nr=28` lines in the gate were that probe), `MADV_WILLNEED` and the hints are no-ops (every
mapping here is populated at `mmap`). `MADV_DONTNEED` walks only the used part of the mmap slab, so its cost is bounded
by what the task allocated, not by the length ring 3 asked for. An unshared frame is zeroed in place; a frame a fork
peer also maps is replaced by a fresh zeroed one (zeroing it would wipe the peer's page; the null-`Rc` class in
`CARGO_HEAP_NULL_RC.md`); a writable `MAP_SHARED` file page is written back and kept, which is what Linux's drop and
refault reads. Two divergences are pinned in the doc comment: a private file page is zeroed (Linux refaults the file's
bytes; nothing records which slab pages came from a file), and pages outside the slab are left alone.

On the way, the three places that turned a present leaf into a `phys::Frame` (`cow_resolve`'s copy, `shared_writeback`
and the new zero) share one `Pte::frame()`: Taoxichuan's `unsafe` sites went 34 -> 33.

**`pidfd_open`/`pidfd_send_signal` on Taoxichuan.** `akuma-runtime-tokio`'s `process` feature turns on glue's
`sc-pidfd` and routes 434 and 424 by number (`shared_number`): Linux numbered every call since about 5.1 the same on
both ABIs, so `akuma-syscalls-abi` cannot name them, and a test pins that the numbers agree. The crate exports the
`ExecRuntime::pidfd_close` hook (`PIDFD_CLOSE`) the kernel registers. A child's exit already woke pidfd pollers through
`publish_child_exit` (A1), so tokio's pidfd reaper works with no kernel change.

**`rt_sigsuspend` is glue's on all three kernels.** Glue already had it, and `pause`, on the restore-sigmask slot
(`akuma_threading::{set,take}_restore_sigmask`, Linux's `saved_sigmask`), which AArch64's frame builder reads. amd64 now
routes `rt_sigsuspend` there instead of `ENOSYS`, and Taoxichuan's own `sigsuspend_arm` (a second copy of the same loop)
is deleted; Taoxichuan also gets `pause` (34) the way amd64 has it. Each x86 kernel takes the slot once at its syscall
return (`signal::deliver_pending`, `child::deliver_at_return`): a handler entered there records the saved mask in its
frame for `rt_sigreturn`, and with no handler entered the saved mask is put back at once.

## Found by the probes

1. **Glue's `pidfd_open` accepted `O_CLOEXEC` and made the fd close-on-exec only on request.** Linux accepts one flag,
   `PIDFD_NONBLOCK` (= `O_NONBLOCK`), answers `EINVAL` for anything else, `O_CLOEXEC` included, and makes every pidfd
   close-on-exec. The first version of `pidfdprobe` asked for `O_CLOEXEC`, and Linux refused it. Fixed in
   `akuma-syscalls-glue::pidfd` for all three kernels; the in-tree callers (Go's probe in `ncaprobe`, tokio) pass 0.
2. **`rt_sigpending` (x86_64 127, asm-generic 136) was served by no kernel**: no abi row, no glue arm. `sigsusprobe`'s
   last rung reads it. New glue arm (`signal::sys_rt_sigpending`: the thread's pending signals that its mask blocks,
   Linux's `do_sigpending`), a row in `akuma-syscalls-abi`, both x86 kernels route it.

## Verified

Probes (`userspace/forktest/c_stress`, musl static, built in `alpine:3.20` on Pop): `pidfdprobe` (new: readiness through
poll and epoll, reap status, flag rules, close-on-exec, `pidfd_send_signal`) and `sigsusprobe` (new: `EINTR`, the
handler's mask, the mask after the call, a blocked pending signal does not wake it, `sigpending`) pass on Linux 6.17
first. `madvshared` and `decommitprobe` pass there as they did before.

- Taoxichuan (Mac TCG, `sig.img`), at `smp=1` and `smp=4`: `pidfdprobe`, `sigsusprobe`, `madvshared`, `decommitprobe`
  all PASS; `sigprobe` rungs 1-8 as at B9 (rung 9 is the pinned "faults do not run handlers" kill, its two fault lines
  the only ones); `killtree 8` PASS; ash `sleep 1 & sleep 2 & wait` returns. No `[nosys]`. Battery 11/11 + goose,
  `tokioprobe` (4 workers) at `smp=1` and twice at `smp=4`, `cow_leak_gate` delta 0, toolchain gate (rustc, cargo with
  a build script); 0 faults. The only `[nosys]` left across the gate is 40 (`sendfile`).
- amd64: self-tests 826/826 in 3 of 3 runs. New: the `pidfd_open(O_CLOEXEC)`, `rt_sigsuspend` and `rt_sigpending`
  dispatch checks, and `sigsuspend_test` (ash `trap` + `wait`: the trap runs, `wait` answers 138, and the mask is back,
  so a second `kill` reaches the trap). Before this change `rt_sigsuspend` was `ENOSYS` here.
- AArch64: boot suite 311 PASSED, 0 FAILED (`MEMORY=2048`): B9's 309 plus `pidfd_open_rejects_o_cloexec` and
  `rt_sigpending_served`.

## Still open

- **AArch64 leaves the restore slot armed when the signal that woke `sigsuspend` is discarded.** `akuma-exceptions`
  takes it only while building a frame, so a default-ignore signal that ends the sleep leaves the temporary mask
  installed. The x86 kernels take it at every syscall return; AArch64 should do the same in its return path.
- Taoxichuan's `mmap` maps every page `USER_RW` whatever `PROT_READ` asked for (the B9 row's "first-touch write to a
  `PROT_READ` page does not fault"), and `PROT_NONE` is not enforced.
- `sendfile` (40) and `setitimer` (38) stay `[nosys]` on Taoxichuan; B10's census decides them.

## Background

- [`TAOXICHUAN_SIGNAL_DELIVERY.md`](TAOXICHUAN_SIGNAL_DELIVERY.md): B9's delivery, and `deliver_one`'s restore argument.
- [`AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md`](AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md): the gap list.
- [`AKUMA_EXTRACT_MMAP.md`](AKUMA_EXTRACT_MMAP.md) §10.1: why `madvise` validates its range first.
