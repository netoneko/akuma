# Safety types: what the compiler carries for the kernel

Implements `docs/archive/TYPE_SYSTEM_SAFETY_REPORT.md` (2026-11-16, a read-only survey). That report ranked eleven
proposals; this page is the **status and the rules**: which type exists, where it lives, what bug class it removes, what
it deliberately does not, and what is still open. Each proposal landed as its own commit on branch `ouch`
(`git log --grep 'TYPE_SYSTEM_SAFETY_REPORT'`).

Where the report and the tree disagreed, the tree won: `Pod` (§3) and the fixes for defects A and B (§2) already
existed, and are noted as such below.

## Rules for new code

| You are about to... | Use | Why |
|---|---|---|
| return an errno from a user-copy helper | `akuma_primitives::errno::Errno` | has no conversion to `u64`; `?` into a `SysResult` arm does not compile. `Errno::ret()` is the one way to the ABI value |
| read a NUL-terminated user string | `copy_from_user_str`, which is `akuma_user_access::read_user_cstr` | validates each page it enters and uses the answer |
| size a kernel buffer from a user length | `akuma_user_access::read_user_vec(ptr, len, max)` | bounded before allocating, fallible allocation, validated copy. Never `vec![0u8; n]` with a user `n` |
| copy a struct across the boundary | a type admitted with `akuma_pod::pod!` | compile-time proof of no padding and no invalid bit patterns |
| offset a user pointer | `UserAddr::offset_of::<T>(i)` / `add` | checked; plain `+` on a `u64` wraps |
| take a user pointer in a new handler | `UserAddr`, not `*_ptr: u64` | `scripts/check_user_ptr_params.py` (pre-commit) fails a component that gains one |
| do something to the whole machine (reboot, mount, set the clock) | take a `HostRoot` | `akuma_exec::process::host_root()` is the check; the effect function demands the token |
| name a box | `BoxId`, and `CallerBox` for the caller's | `can_access_box(source, target)` is directional; the two ids are different types |
| spawn "into the caller's box" | `SpawnBox::Inherit` | the raw field's `0` means "inherit" there and "the host" everywhere else |
| key anything by file | `FileKey` (`MountId`, `Ino`) | an inode number means nothing without its mount |
| record a mapping's protection | `Protection` | unrecorded and `PROT_NONE` are different facts; a grant reader and a deny reader are different methods |
| convert a user length to pages | `Pages::from_len_ceil` | bounded by the 48-bit space once, so `bytes()` cannot wrap |
| hold a thread beyond the moment you learned its tid | `ThreadRef`; `OptTidCell::get_live()` when *acting* on it | a recycled slot is somebody else's thread |
| take a lock an IRQ handler or the scheduler also takes | `IrqSpinlock` | there is no unmasked `lock()` |
| write a guard that restores per-core / per-thread state | add `PhantomData<*mut ()>` | it must drop where it was made |

## The types

### `Errno` (`akuma-primitives::errno`) — report §1
`NonZeroU16`, `repr(transparent)`. The user-copy helpers in `akuma-user-access` return `Result<_, Errno>`. Before, they
carried a positive errno in a bare `u64` while `SysResult` carries a negated one, so `read_user_into(..)?` compiled and
returned `Err(14)`, which userspace reads as a successful call. `SysResult` is now `Result<u64, Errno>` too, along with
the ~45 glue helpers that carried a negated `u64` (`fs_error_to_errno`, `perm::check*`, `read_vector`, `mount_errno`, the
`ExecArch` trait both kernels implement, ...), so `?` is correct everywhere and `flat()` is the one place an `Errno` becomes
`x0`. `scripts/check_errno_sign.py` is deleted. `Errno::NAME` constants are generated from the errno table;
`Errno::from_negated` decodes a negated `u64`. A plain `-> u64` arm still returns the negated constants.

### `read_user_cstr`, `read_user_vec` (`akuma-user-access`) — §2
The report's defects A (a path argument naming kernel memory on AArch64) and B (an unbounded `vec!` from a user length)
were already fixed on this branch. What the work added: the walk is **one** function, so the byte read is private; and the
rump proxy's `copyinstr`, which walked bytes with no per-page check at all (the same hole as defect A), now uses it. Seven
user-sized allocations use `read_user_vec`; their bounds and errnos did not change.

### `Pod` (`akuma-pod`) — §3
Already implemented, better than the report's sketch: `pod!` checks field types, field `Pod`-ness and that the fields tile the
struct (no padding) at compile time, with no `unsafe` at the call site. No change.

### `HostRoot`, `BoxId`, `CallerBox`, `SpawnBox` — §4
`HostRoot` (`akuma-primitives::privilege`) is zero-sized and `Copy`. Its two mint sites, by name: `vouch_checked()`, called
only from `akuma_exec::process::host_root()` after it has tested the caller's box and uid; and `kernel_boot()`, for kernel
code with no caller (the boot-time mounts). Effects that demand one: `akuma_boot::{system_reset, system_off}`,
`akuma_vfs_glue::{mount_with, unmount, mount_in_namespace}`, `akuma_primitives::clock::set_wall_clock`. The syscall layer no
longer names the unchecked `set_utc_time_us`; that is a review rule, not a type. **It is not unforgeable**: kernel code can
mint one. Its value is that forgetting the check stops compiling, and forging one is a grep. Decision-only sites (raw block
open, the amd64 `/dev/kvm0` and `/dev/power` control devices, the cloud-metadata gate) still call `caller_is_host_root()`
because they have no effect function to hand a token to.

### `FileKey` (`akuma-primitives::file_key`) — §6
`FileKey { mount: MountId, ino: Ino }`, built from a record's two fields with `FileKey::from_raw(mount_id, inode)`.
**Deviation from the report:** plain `u32`, not `NonZeroU32`. Zero is "none" in many callers that treat it as "do nothing";
a fallible constructor would have changed each. `is_shareable()` is the one spelling of the zero check.

### `Protection` (`akuma-mmap`) — §7
`Unrecorded | Stated(Prot)`. `MmapRegion`'s field is private; `grant_prot()` (unrecorded grants nothing) and
`recorded_prot()` (unrecorded denies nothing) are the two readers. Fork no longer patches a second field after building the
child region.

### `Pages`, `PageRange` (`akuma-primitives::pages`) — §10
`Pages::from_len_ceil(len) -> Option<Pages>`, bounded by `2^48 / 4096`. Fixed four sites that wrapped in release:
`munmap_len` (unmapped nothing and returned 0), `sys_mprotect` (changed nothing and returned 0; now `ENOMEM`, as Linux),
and `mremap::plan` (unbounded `new_size` and `old_size`; `ENOMEM` and `EINVAL`). `PageRange` is an aligned start, a `Pages`
count and an end within `2^48`, checked at construction; `sys_mprotect` uses it and `sys_mmap` takes its count from `Pages`.
The architecture's own user limit (47 bits on amd64) is still the caller's check against `end()`. **Open:** the other ~90
page-math sites (`akuma-mmap`, `akuma-mmu`, `akuma-exec`) belong to wave 4 of `typed-addresses.md`, not a second sweep.

### `ThreadRef` (`akuma-threading`) and `OptTidCell::get_live` — §5
`WakeHandle` generalised and renamed (`WakeHandle` is an alias). `Process::thread_id` is stored as a packed `ThreadRef`, so
slot and generation are read atomically. `get_live()` answers `None` for a recycled slot. Used where the tid is *acted on*
or reported: `kill_process`, `kill_process_with_signal`, `kill_thread_group`, the SIGCHLD target choice, `group_cpu_time_us`
and the procfs `cpu_time`/`processor` fields (each could previously attribute another process's thread to the wrong one).
Left on `get()`: the table's own staleness test (`unregister_process`, which compares against `THREAD_PID_MAP`), diagnostics,
and the exit path's own current thread. **Limit:** a check followed by an action is still a race (the
generation bump is lock-free); the window is one load instead of the process's lifetime.

### `IrqSpinlock` (`akuma-irq-lock`) — §9
A new crate, because `akuma-primitives` takes no dependencies and this needs `spinning_top`. `lock()` masks IRQs, then
acquires; the guard drops the lock, then restores the mask, and is `!Send`. This does **not** cover lock ordering.
Migrated: `IRQ_HANDLERS`, `POOL` (the scheduler's thread pool), `DMESG` and `CONSOLE_LOCK` (every IRQ-context print reaches
them). Each was verified on both boot suites. **Open:** classify the remaining ~100 static spinlocks. The rule is "any lock an
IRQ handler or the scheduler takes"; `grep -rn 'static [A-Z_]*: Spinlock<'` lists them, and a handler's call graph decides.

### `UserAddr`, `UserSlice` (`akuma-user-access`) — §8
`UserAddr` has no `Add`; `offset_of::<T: Pod>(i)` and `add` are checked, `slice(len, prefault)` validates and returns a
`UserSlice` proof. Used at the two pointer-arithmetic sites the report named, and as the parameter type of eight
`akuma-syscalls-time` handlers (the template conversion: change the parameter, wrap the raw argument once at dispatch, let
the compiler find the rest). `scripts/check_user_ptr_params.py` (pre-commit) ratchets the remaining untyped `*_ptr`
parameters per component; `--list <crate>` prints the sites. **Open:** ~210 parameters, 169 of them in
`akuma-syscalls-glue`.

### The self-test validation bypass (`BYPASS_VALIDATION`) — audit M7
Sealed before init since `ed65401d`; now also **compiled out** of any image built with `--features no-tests`
(`akuma-user-access` has its own `no-tests` feature and `build.rs`, reached through `akuma-exec/no-tests`). In such an image
the flag, `BypassValidationGuard` and every check that read them do not exist (`seal_bypass()` is a no-op; verified by building
the aarch64 `no-tests` release and finding no `Bypass` symbol). The two production reads were removed: `sys_spawn`'s redundant
pre-check (`copy_from_user` validates anyway; it uses `read_user_vec` now) and the amd64 `range_ok_or_bypassed` arm. In the
default image, which carries the boot suite, it is still present and sealed.

### `!Send` guards — §11
`IrqGuard`, `PreemptGuard`, `BypassValidationGuard`.

## Tests

- **Isolation** — `process_tests::test_process_memory_isolated_except_cow` (aarch64) and
  `uas_tests::isolation_except_cow_test` (amd64): unrelated processes share no physical frame, a page only one mapped is
  unreachable from the other, and after a fork every frame visible in two spaces is read-only, CoW-marked/counted ≥ 2, while an
  unrelated third process is untouched. These read page tables and frames; they do not execute a user read from a second
  process.
- Host unit tests: `Errno`, `FileKey`, `Pages`, `IrqSpinlock`, `SpawnBox`, `munmap_len`, `mremap::plan`.

How they were run: aarch64 under QEMU/TCG (`cargo build --release`, then `scripts/cargo_runner.sh` with `INSTANCE=10` so the
host ports are unprivileged, and the active toolchain's `rust-objcopy` on `PATH`); amd64 under QEMU with KVM
(`sh amd64/run.sh -accel kvm -cpu host`, `DISK=<img>` from `amd64/mkdisk.sh`). Not built or run: anything outside those two.

## Open work (what the report listed that is not done)

2. **§4** decision-only privilege sites (raw block open, amd64 `/dev/kvm0`, `/dev/power`) call `caller_is_host_root()`;
   they have no effect function to take a token.
3. **§5** the 60 `slot: usize` functions in `akuma-threading`; a distinct `Pid` type; a real `with_live` that holds the slot
   state lock. Without it, a generation check followed by an action is a race with a one-load window.
4. **§8** the ~210 untyped `*_ptr` parameters. And deleting `BYPASS_VALIDATION` outright: it is compiled out of `no-tests`
   images now (below), but the boot self-tests still use it at ~125 sites that pass kernel buffers as user pointers; each needs
   a real user mapping instead (`proposals/FUTURE_WORK.md`).
5. **§9** classifying and migrating the other static spinlocks.
6. **§10** the page-math sites in `akuma-mmap`/`akuma-mmu`/`akuma-exec` (wave 4 of the address plan).
7. Mutation checks for the two isolation tests (break isolation on purpose, see them fail); they pass today, which is not
   the same as proving they would catch a break.

Out of scope on purpose, as in the report: newtypes for every id, `TidSlot` alone, lock ordering in types, may-allocate /
may-sleep tokens, typed PTE flags (already planned), a canonical-path type for the VFS.
