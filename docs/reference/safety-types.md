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
returned `Err(14)`, which userspace reads as a successful call. `scripts/check_errno_sign.py` was the guard and had
scanned nothing since `src/syscall/` moved; it now scans the syscall crates. **Open:** `SysResult`'s `Err` is still a
negated `u64`; making it `Errno` is a wider sweep and would let the script be deleted.

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

### `Pages` (`akuma-primitives::pages`) — §10
`Pages::from_len_ceil(len) -> Option<Pages>`, bounded by `2^48 / 4096`. Fixed three sites that wrapped in release:
`munmap_len` (unmapped nothing and returned 0), `sys_mprotect` (changed nothing and returned 0; now `ENOMEM`, as Linux),
and `mremap::plan` (unbounded `new_size`; now `ENOMEM`).

### `ThreadRef` (`akuma-threading`) and `OptTidCell::get_live` — §5
`WakeHandle` generalised and renamed (`WakeHandle` is an alias). `Process::thread_id` is stored as a packed `ThreadRef`, so
slot and generation are read atomically. `get_live()` answers `None` for a recycled slot; `kill_process`,
`kill_process_with_signal` and `kill_thread_group` use it. **Limit:** a check followed by an action is still a race (the
generation bump is lock-free); the window is one load instead of the process's lifetime.

### `IrqSpinlock` (`akuma-irq-lock`) — §9
A new crate, because `akuma-primitives` takes no dependencies and this needs `spinning_top`. `lock()` masks IRQs, then
acquires; the guard drops the lock, then restores the mask, and is `!Send`. This does **not** cover lock ordering.

### `UserAddr`, `UserSlice` (`akuma-user-access`) — §8
`UserAddr` has no `Add`; `offset_of::<T: Pod>(i)` and `add` are checked, `slice(len, prefault)` validates and returns a
`UserSlice` proof. Used at the two pointer-arithmetic sites the report named.

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

See the status line at the top of each section above; the running list is kept at the end of this file.
