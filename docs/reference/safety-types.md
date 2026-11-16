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
code with no caller (the boot-time mounts). Effects that demand one: `akuma_boot::{system_reset, system_off}`, `akuma_vfs_glue::{mount_with, unmount, mount_in_namespace}`,
`akuma_primitives::clock::set_wall_clock`, glue's `install_block_device` (a raw `/dev/vdX` descriptor), the amd64 kernel's
`open_control` / `open_wifi` / `open_power` / `open_kvm` (the machine's control nodes) and its `clock::set_unix_us`
(`settimeofday`). The syscall layer no longer names the unchecked `set_utc_time_us`; that is a review rule, not a type. **It is
not unforgeable**: kernel code can mint one. Its value is that forgetting the check stops compiling, and forging one is a grep.
The one remaining caller of the bool form, `caller_is_host_root()`, is glue's `egress_allowed` (the cloud-metadata address): it
decides on a destination, it has no effect function to hand a token to, and it is a predicate by nature.

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
parameters per component; `--list <crate>` prints the sites. **Done:** the 189 real user pointers are `UserAddr`
(batches: `unixsock`/`net`, `fs`, the process/signal/small files, `poll`/`container`, then the amd64 handlers, `akuma-exec` and
`akuma-syscalls-time`); the budget is 0 and the hook keeps it there. Classified in "Untyped pointer classification" below.

### The self-test validation bypass (`BYPASS_VALIDATION`) — audit M7
Sealed before init since `ed65401d`; now also **compiled out** of any image built with `--features no-tests`
(`akuma-user-access` has its own `no-tests` feature and `build.rs`, reached through `akuma-exec/no-tests`). In such an image
the flag, `BypassValidationGuard` and every check that read them do not exist (`seal_bypass()` is a no-op; verified by building
the aarch64 `no-tests` release and finding no `Bypass` symbol). The two production reads were removed: `sys_spawn`'s redundant
pre-check (`copy_from_user` validates anyway; it uses `read_user_vec` now) and the amd64 `range_ok_or_bypassed` arm. In the
default image, which carries the boot suite, it is still present and sealed.

### `!Send` guards — §11
`IrqGuard`, `PreemptGuard`, `BypassValidationGuard`.

## Untyped pointer classification (2026-11-16)

`scripts/check_user_ptr_params.py` matched 210 parameters named `ptr` or `*_ptr` and typed `u64`/`usize`. Each was read by
function body (how the pointer is used) and by crate. They are not one problem.

| Class | Count | What it is | Decision |
|---|---|---|---|
| **Real user pointers** | **189** | 169 in `akuma-syscalls-glue`, 14 in the amd64 kernel's handlers (`usermode.rs`, `fd.rs`), 4 in `akuma-exec` (`clone_thread`'s two tid pointers and the fork/vfork `stack_ptr` value), 1 each in `akuma-syscalls-ipc` and `akuma-syscalls-time` | convert to `UserAddr` |
| **Boundary primitives** | 12 | the validators (`user_range_ok`, `validate_user_range`, `upper_half_range_ok`), the cstr walk and byte read in `akuma-user-access`, and amd64's `range_ok`/`read_bytes`/`write_bytes`/`read_val`/`write_val`/`read_cstr` | keep raw: this is the layer where a raw address becomes a checked one |
| **Not user pointers** | 9 | kernel heap pointers (`akuma-alloc`), a boxed-closure pointer (`akuma-switch`), the device-tree `dtb_ptr` (`akuma-kernel`, `akuma-kernel-glue`), a test helper (`akuma-threading`) | false positives of the name match |

The last two classes are now excluded from the ratchet (with the reasons in the script), so its budget, **189**, counts only
what is actionable.

**How the 189 are used inside their functions** (a heuristic on the body): about 130 only copy through the pointer
(`copy_from_user*`, `read_user_into`, `write_user_val`, `validate_user_ptr`); about 35 of those also test it against `0` first
(a NULL "optional out-parameter", e.g. `accept`'s address); about 55 only pass it to another handler or helper (the AF_INET
and AF_UNIX `dispatch_*`/`sys_*` pairs, `execve`/`spawn` and the amd64 twins, `pselect6`/`ppoll`), which therefore convert as
a chain; 3 are unused (`_addr_len_ptr`, `_sigmask_ptr`, `_data_ptr`, kept for ABI shape); and the rest are one-offs
(`trace_*` debug helpers, `sys_core_init`'s program pointer).

**Why convert, since none of this is a bug today.** 128 functions take these pointers, and 95 of them also take another bare
`u64`/`usize` (a length, a flag, an fd): a pointer and a length swapped at a call still compiles. 17 take four or more. A
`UserAddr` makes the pointer a different type from the integers beside it, gives `offset_of`/`add` instead of raw `+`, and
makes `slice()` the one validation. The previous defects in this family (validate-then-ignore, wrapping `iov + i * 16`) are
already fixed at their sites; what remains is preventing the next one.

**Conversion** (done 2026-11-17, in batches, with the compiler's JSON diagnostics wrapping each mismatched argument so the call
sites needed no hand edits): a handler takes `UserAddr` and shadows it with the raw value at the top of its body, so the body
is unchanged and the dispatch wraps `args[n]` once. Glue's own `validate_user_ptr` and `copy_from_user_str` stayed raw `u64`:
they are the boundary helpers beside the `akuma_user_access` copy family, and converting them wrapped ~120 call sites for
nothing. The plan that was followed:

1. `unixsock.rs` (12) and the matching `net.rs` `dispatch_*` (33): the largest pass-through chain, and the one with the most
   same-typed neighbours (`addr_ptr`, `len_ptr`, `addrlen_ptr`).
2. `fs.rs` (43): the most call sites; mostly `buf_ptr` + `count`.
3. `proc.rs` + `execve.rs` + amd64 `usermode.rs` (28 + 5): `execve`/`spawn`/`wait4`, converted with the amd64 twins.
4. `poll.rs`, `signal.rs`, `container.rs`, and the remaining small files (about 50).

Each step is one crate-local sweep with the pattern: change the parameter to `UserAddr`, wrap `args[n]` once at dispatch, let
the compiler find the rest, run both boot suites, lower the budget with `--update`.

**What this does not do yet.** The bodies still read the raw `u64` after the shadow; the type protects the signature (a pointer
cannot be swapped with a length or an fd) and the call sites, not the arithmetic inside. Moving a body onto `offset_of` /
`slice()` is incremental and is not required for the ratchet.

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

3. **§5** the 60 `slot: usize` functions in `akuma-threading`; a distinct `Pid` type; a real `with_live` that holds the slot
   state lock. Without it, a generation check followed by an action is a race with a one-load window.
4. **§8** the 189 untyped user-pointer parameters (classified below; the other 21 matches are validators or not user pointers). And deleting `BYPASS_VALIDATION` outright: it is compiled out of `no-tests`
   images now (below), but the boot self-tests still use it at ~125 sites that pass kernel buffers as user pointers; each needs
   a real user mapping instead (`proposals/FUTURE_WORK.md`).
5. **§9** classifying and migrating the other static spinlocks.
6. **§10** the page-math sites in `akuma-mmap`/`akuma-mmu`/`akuma-exec` (wave 4 of the address plan).
7. Mutation checks for the two isolation tests (break isolation on purpose, see them fail); they pass today, which is not
   the same as proving they would catch a break.

Out of scope on purpose, as in the report: newtypes for every id, `TidSlot` alone, lock ordering in types, may-allocate /
may-sleep tokens, typed PTE flags (already planned), a canonical-path type for the VFS.
