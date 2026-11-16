# Where the type system could carry more of the kernel's safety (2026-11-16)

**Status: research report, nothing changed.** A read-only survey made alongside
[`SECURITY_AUDIT_2026_11_16.md`](SECURITY_AUDIT_2026_11_16.md). That audit's "Formal verification" section covers Kani,
Verus, Miri and Loom; this report covers only what Rust's types can enforce at compile time. Every claim says whether it
was **verified** (read in code at the cited line) or **inferred** (reasoned, not read or run). Nothing was built or booted.

## Summary

1. The user-memory boundary is still raw `u64` + `usize`. Two live defects sit in it: an AArch64 kernel-memory read
   through path strings, and an unbounded user length passed to `vec!` (kernel panic). Types would have prevented both.
2. The cheapest win is an `Errno` newtype: the sign trap it removes is guarded by a script that has scanned nothing
   since `src/syscall/` moved into `crates/akuma-syscalls-glue/`.
3. `T: Copy` is the only bound on values copied across the boundary. Padding is kept out of user copies by convention.
4. Privilege and box checks return `bool` and the effects take no proof. That is how nine box checks went unconsulted
   in August, and it is the shape of the audit's H2 and M9.
5. The largest bug class by incident count is a stale thread id acting on a recycled slot. `WakeHandle` already fixes
   it for wake-ups and nowhere else.

**Ranked by value for cost:**

1. `Errno` newtype for user-access errors (and fix the inert `check_errno_sign.py` today).
2. `UserCStr` reader + `UserLen` (stage A of the user boundary): string reads and user-sized allocations.
3. `AbiPlain` bound instead of `T: Copy` on user copies.
4. Permit types for privileged and box operations (`HostRoot`, `BoxAccess`, `BoxId`).
5. Generation-checked thread references (`ThreadRef` / `LiveThread`, generalizing `WakeHandle`).
6. `FileKey { mount, ino }` for the 25 `(mount_id, inode: u32)` signatures.
7. `Protection` enum in place of `MmapRegion::{prot, prot_recorded}`.
8. `UserAddr` / `UserSlice` across all of glue (stage B of the user boundary).
9. `IrqSpinlock<T>`: a lock that masks IRQs as part of acquiring.
10. `Pages` / `PageRange`: bounded page counts whose byte length cannot overflow.
11. `!Send` on per-thread and per-core guards (five lines; low value, near-zero cost).

## Table

| # | Proposal | Bug class removed | Call sites (grep) | Cost | Verdict |
|---|---|---|---|---|---|
| 1 | `Errno` newtype | positive errno returned as a syscall *success* via `?` | 62 `Result<_, u64>`, 47 `SysResult`; `.is_err()` callers unchanged | small | do now |
| 2 | `UserCStr`, `UserLen` | unchecked user string reads; user length into allocation/indexing | 32 `copy_from_user_str`; ~30 `vec![0u8; n]` in glue | small-medium | do now |
| 3 | `AbiPlain` | kernel stack bytes leaked through implicit padding; invalid bit patterns from user bytes | ~156 call sites unchanged; ~30-40 type registrations (estimate) | small-medium | do |
| 4 | `HostRoot`, `BoxAccess`, `BoxId` | missing privilege/box check; swapped source/target box | ~20 privileged sites (estimate), 3 box predicates | small | do with H2 |
| 5 | `ThreadRef`/`LiveThread` | stale tid acts on a recycled slot; pid/tid confusion | 60 `pub fn` in `akuma-threading` take `slot: usize`; 2 stored `thread_id` fields | large | do incrementally |
| 6 | `FileKey` | mount/inode swap; key missing the mount (cross-box data) | 25 signatures | small | do |
| 7 | `Protection` enum | grant record read as a deny record; second field dropped on copy | ~6 constructors, ~10 region `.prot` readers | small | do |
| 8 | `UserAddr`/`UserSlice` | validate-then-ignore; pointer arithmetic on raw `u64`; test bypass in production | 170 handlers, 115 `_ptr` params, ~445 copy/validate calls | large | after 1-3 |
| 9 | `IrqSpinlock<T>` | lock taken from IRQ context while held with IRQs on (self-deadlock) | 272 `with_irqs_disabled(`, 102 static spinlocks | medium | worth it |
| 10 | `Pages`/`PageRange` | page-count/byte-length overflow on user lengths | ~100 page-math sites | medium | fold into addr wave 4 |
| 11 | `!Send` guards | guard dropped on another thread/core restores the wrong slot | 3 structs | trivial | do anyway |

## 1. `Errno`: one sign for every user-access error

**Evidence (verified).** `crates/akuma-syscalls-glue/src/lib.rs:380-418` documents "the sign trap". `SysResult` carries
a *negated* errno while `akuma-user-access` returns a *positive* one (`EFAULT` = 14, `lib.rs:393`), so
`read_user_into(&mut ts, ptr)?` compiles inside a `SysResult` arm and returns `Err(14)`, which userspace reads as success.
The defence is `scripts/check_errno_sign.py`, run by `scripts/git-hooks/pre-commit`. Its `ROOTS = ("src/syscall",)`
(line 56). **That directory no longer exists** (`ls src/` lists only `aarch64.rs`, `amd64.rs`, `main.rs` and text files), so
`rglob` finds nothing and the check passes vacuously. A manual `rg -U` with the script's own regex over
`akuma-syscalls-glue`, `-glue-amd64`, `-time`, `akuma-kernel-glue` and `akuma-kernel-amd64` finds no live violation
today; the only hit is the doc comment. The guard has lapsed, and nothing has broken yet.

**Sketch.**

```rust
// akuma-primitives::errno
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct Errno(core::num::NonZeroU16);
impl Errno {
    pub const EFAULT: Errno = /* 14 */;
    /// The `x0` the syscall ABI wants: `-errno` as u64.
    pub const fn ret(self) -> u64 { (-(self.0.get() as i64)) as u64 }
}
pub type SysResult = Result<u64, Errno>;   // flat(): Ok(v) => v, Err(e) => e.ret()
// akuma-user-access
pub fn read_user_into<T: AbiPlain>(dst: &mut T, src: UserAddr) -> Result<(), Errno>;
```

`?` is then correct by construction, and the script can be deleted.

**Cost.** The copy helpers return `Result<(), u64>`. Almost every caller uses `.is_err()`, which compiles unchanged, so
the migration is the helpers' signatures, `SysResult`, `flat`, and the few sites that read the error value. No runtime
cost (inferred: a `repr(transparent)` `u16` in a `Result`). **Do now**; until then, change `ROOTS` to
`crates/akuma-syscalls-glue/src` and the other syscall crates. That is a one-line fix to a gate that is meant to be on.

## 2. Stage A of the user boundary: `UserCStr` and `UserLen`

Two defects found by this survey show what the raw types allow. Neither needs a type to fix; both need one so they stay
fixed.

**Defect A: on AArch64 a path argument can name kernel memory (verified in code; the leak itself needs a probe).**
`copy_from_user_str` (`crates/akuma-syscalls-glue/src/lib.rs:504`) checks `ptr < USER_VA_LIMIT` and, once per page,
calls `validate_user_ptr(addr, 1)`, but **discards the result** (`let _ =`, line 525). It then reads through
`read_user_byte`, which validates nothing (`crates/akuma-user-access/src/lib.rs:734`). On AArch64 the kernel's RAM is
identity-mapped low, below `USER_VA_LIMIT`, as EL1-only blocks in every user address space (`akuma-mmap/src/place.rs:39`
`Window::hole`; `UNSAFE_AUDIT.md` §4.0a). The copy loop runs at EL1 with ordinary loads, so an EL1-only page does not
fault. Only the page-table AP test in `validate_user_range` rejects such a page, and that is the result being dropped.
So `symlinkat(<kernel address>, "/tmp/x")` (`fs.rs:3771`) should store kernel bytes up to the first NUL as the link
target, and `readlink` returns them (needs a probe; UTF-8 validation limits what one call returns, but it still reveals
data at each address). PAN would not help: PAN blocks EL1 access to EL0 pages, and these pages are EL1-only. amd64 is
not affected, because its kernel is in the upper half above `USER_VA_LIMIT`. `read_user_byte`'s doc calls its bound "a
liveness obligation, not a soundness one". That is true on amd64 and false on AArch64.

**Defect B: a user length reaches `vec!` with no bound (verified in code; panic behaviour inferred from `alloc`).**
`sys_spawn_ext` copies a user `SpawnOptions` and then runs `alloc::vec![0u8; o.cwd_len]` (`proc.rs:1765`) and
`alloc::vec![0u8; o.stdin_len]` (`proc.rs:1786`), with no `validate_user_ptr` and no maximum. Any process can call it,
and `box_id == 0` needs no check. A length above `isize::MAX` makes `alloc` panic with "capacity overflow". That path does
not reach `alloc_error_handler`, and the kernel panic handler halts (`src/aarch64.rs:60`). A large length that is still
valid takes the OOM path instead, which kills the process from inside the syscall. Other `vec![0u8; n]` sites in glue
validate the range first, which bounds `n` below 2^48 but not below the heap size (e.g. `container.rs:7`,
`net.rs:1769`, `lib.rs:1059`). One site in the rump-only build sums user `iov_len`s with wrapping `.sum()` and then
slices a buffer of the wrapped size (`net.rs:1438`): a slice panic, not in the default build.

**Sketch.**

```rust
/// A length as userspace passed it. No `Deref`, no arithmetic: the only exits are checks.
#[derive(Clone, Copy)] #[repr(transparent)]
pub struct UserLen(usize);
impl UserLen {
    pub fn at_most(self, max: usize) -> Result<usize, Errno>;           // E2BIG/EINVAL past max
    pub fn read_vec(self, from: UserAddr, max: usize) -> Result<Vec<u8>, Errno>; // bounded, try_reserve, copy
}
/// A NUL-terminated user string reader: validates each page it enters and *uses the answer*.
pub struct UserCStr { at: UserAddr, max: usize }
impl UserCStr { pub fn read(self) -> Result<String, Errno>; }
```

`read_user_byte` becomes private to `akuma-user-access`. The string walk is the only caller it was ever meant for
(`glue/src/lib.rs:491-501`).

**Bug class removed.** A check that is computed and then ignored, and a user-controlled size reaching an allocator or a
slice index. Precedents: `UNSAFE_AUDIT.md` §4 P0 (126 opt-in checks against 167 copies), `AKUMA_EXTRACT_MMAP.md` §10.1
(user `len` straight from a register), `USER_COPY_FOLD.md`.

**Cost.** `copy_from_user_str` has 32 callers and keeps its signature. The allocation sites are the ~30 `vec![0u8; n]`
in glue (`rg 'vec!\[0u8; *[a-z_.]+\]'`), some of them already bounded. Small to medium. No runtime cost beyond the
bound check (inferred).

## 3. `AbiPlain` instead of `T: Copy` on user copies

**Evidence (verified).** `write_user_val<T: Copy>`, `read_user_into<T: Copy>`, `as_user_bytes<T: Copy>`
(`akuma-user-access/src/lib.rs:750, 805, 776`) and `akuma_mmu::read_current_user_val<T: Copy>` (`akuma-mmu/src/lib.rs:4699`)
turn any `Copy` value into bytes and back. `Copy` admits two things that are wrong here:

- **Implicit padding.** `write_user_val` copies `size_of::<T>()` bytes, so a padding hole sends uninitialised kernel stack
  to userspace (an infoleak, and UB to form the `&[u8]`). The tree knows this and handles it by convention: every ABI
  struct spells padding out as a named field. `Sysinfo::_f` (`akuma-syscalls-linux/src/proc.rs:63-72`) says that as
  implicit padding the four bytes "would go out as whatever was on the kernel stack". The same applies to `MsgHdr::_pad3`
  (`net.rs:40-53`), `Flock::{_pad0,_pad1}` (`glue/src/recordlock.rs:40`) and `StackT::_pad`. No struct without its
  padding written out was found among the types sampled. Nothing in the type system stops the next one.
- **Invalid bit patterns.** `read_user_into` writes user bytes over a `T`. For a `bool`, an enum, a `char`, a `NonZero*` or
  a reference, which are all `Copy`, that is undefined behaviour. None of the sampled call sites does it (verified for
  `KernelSigaction`, `Timespec`, `Timex`, `MsgHdr`). Again only convention prevents it.

**Sketch** (shape only, not compiled). The trait is *safe*, so the `forbid(unsafe_code)` crates (`akuma-syscalls-linux`,
glue) can implement it through a macro. Its guarantee is structural:

```rust
/// Plain ABI data: every bit pattern valid, every byte a field.
pub trait AbiPlain: Copy + 'static { const FIELD_BYTES: usize; }
// integers and [T; N] where T: AbiPlain get impls in akuma-user-access.
macro_rules! abi_plain { ($t:path { $($f:ident),* $(,)? }) => {
    impl AbiPlain for $t { const FIELD_BYTES: usize = 0 $(+ field_bytes(|v: &$t| &v.$f))*; }
    const _: fn($t) = |v| { let $t { $($f: _),* } = v; };   // exhaustive: a new field fails to compile
}}
pub fn write_user_val<T: AbiPlain>(dst: UserAddr, v: &T) -> Result<(), Errno> {
    const { assert!(core::mem::size_of::<T>() == T::FIELD_BYTES) };  // implicit padding = compile error
    // ...
}
```

`field_bytes` requires each field to be `AbiPlain`, so a `bool` field does not compile. The exhaustive pattern catches a
field that was not listed, and the size equation catches padding. `zerocopy`'s `IntoBytes`/`FromBytes` derives do the
same job, but that is a third-party dependency and the tree's rule is to avoid unvetted ones.

**Cost.** None of the ~156 call sites changes shape. Each ABI type gets one macro line (an estimated 30-40 types) placed
next to its definition. Zero runtime cost. Small to medium.

## 4. Permits for privileged and box operations

**Evidence (verified).** Every privilege decision in glue returns `bool`, and the operation it guards takes no evidence:

- `caller_may_reboot()` (`glue/src/reboot.rs:13`) and `caller_may_mount()` (`container.rs:161`) are
  `current_process_shared().is_none_or(|p| p.box_id == 0)`. They are box-only and **fail open** when there is no
  current process. `akuma_boot::system_reset()` takes no argument.
- `sys_clock_settime` and `sys_clock_adjtime` (`akuma-syscalls-time/src/lib.rs:365, 405`) check nothing, then call
  `set_utc_time_us` / `step_utc_by` (audit M9).
- The amd64 `CharDevOps::open(path, flags)` (`akuma-kernel-amd64/src/chardev.rs:29`) does not even receive the caller,
  so it *cannot* check who is opening `/dev/kvm0` or `/dev/power` (audit H2).
- `can_access_box(registry, source_box_id: u64, target_box_id: u64, pid)` (`akuma-isolation/src/box_registry/access.rs:19`)
  takes two adjacent `u64` box ids. The rule is directional (an ancestor may access a descendant, not the reverse;
  `hierarchy::is_ancestor(reg, box_id, ancestor_id)`), so swapping them inverts the policy and still compiles.
- Box id `0` means "host" in `access.rs:25`, "inherit the caller's box" in `sys_spawn_ext` (`proc.rs:1754-1756`), and
  `parent_box_id: None` also means host (`hierarchy.rs`, `get_children`).

The incident that best matches this shape is `BOX_ISOLATION_SECURITY_FIXES.md` §1: `can_access_box`, `can_kill_box` and
`validate_nested_root` were tested, documented, and **called from nowhere**. Nine boundaries went unenforced while every
test passed. H2 and M9 in the security audit are the same failure, only smaller.

**Sketch.**

```rust
// akuma-isolation (forbid(unsafe_code); already owns Creds)
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub struct BoxId(u64);       // 0 = host, and only that
pub enum SpawnBox { Inherit, Into(BoxId) }                                 // spawn_ext's 0 becomes Inherit
#[derive(Clone, Copy)] pub struct HostRoot { _private: () }               // no literal outside this crate
pub fn host_root(box_id: BoxId, creds: &Creds) -> Option<HostRoot>;      // box 0 AND uid 0
pub struct BoxAccess { target: BoxId }                                     // !Copy, consumed by the effect
pub fn can_access_box(reg: &Registry, caller: CallerBox, target: BoxId) -> Option<BoxAccess>;
// effects
pub fn system_reset(_: HostRoot) -> !;
pub fn set_wall_clock(_: HostRoot, unix_us: u64, uptime_us: u64);
pub fn kill_box(p: BoxAccess) -> Result<(), KillError>;
```

**Bug class removed.** A missing check becomes a compile error, provided the effect is reachable only through the
token-taking function. `CallerBox` and `BoxId` are distinct types, so a swapped call does not compile. "No process
means host" has to be stated in one constructor instead of four `is_none_or` closures.

**What it does not do (inferred).** The token is not unforgeable: kernel code can call `host_root(BoxId::HOST,
&Creds::ROOT)`. Its value is that forgetting the check stops compiling and forging it is easy to grep for. The boot paths
that set the clock legitimately (`akuma-kernel-core/src/ntp_boot.rs:76`, `timer.rs:286`, the amd64 `clock.rs`) need either
a named `HostRoot::kernel_boot()` or to keep calling the unchecked primitive in their own crate. Then the rule
"`akuma-syscalls-*` never names `clock::set_utc_time_us`" is a review rule, not a type.

**Cost.** About 20 sites (estimate): reboot, mount/umount/mount_in_ns, raw block open (`fs.rs:2333`), clock_settime,
adjtime, settimeofday, sethostname, chroot, the `/proc/dac` write, five amd64 char devices, and the four box operations.
The `require_host_root()` the audit proposes is the constructor here. Small. Do it as part of fixing H2, not afterwards.

## 5. Generation-checked thread references

**Evidence (verified).** A tid is a bare `usize` index into a recycled array (`MAX_THREADS` = 256/512,
`akuma-primitives/src/preempt.rs:66`): 126 tables are sized by `MAX_THREADS`/`MAX_PROCESSES` (71 of them in
`akuma-threading/src/lib.rs`), and 60 `pub fn`s there take `slot: usize`. `Process::thread_id: Option<usize>`
(`akuma-exec/src/process/mod.rs:822`) stores a tid for later use. The tree already built the fix once:
`WakeHandle` (`akuma-threading/src/lib.rs:557`) packs `(generation, tid)`, with the per-slot `SLOT_GEN` bumped once per
rebirth. Its doc says "wait queues must store THIS, not a bare `usize` tid", but nothing else stores it.

**Incident history** (from `BUG_FIX_LIST.md`; by count the largest class in this report):
`KTG_STALE_TID_EXIT_STAMP_J4_HANG.md` (a dead sibling's slot, recycled to `ld`, was stamped with a forged clean exit; the
`-j4` self-host hang), `STALE_THREAD_SLOT_KILL.md` (`kill_thread_group` killed an unrelated `collect2`),
`AKUMA_STALE_TID_ZOMBIE_SLOT.md` (a `kill` of a zombie's pid resolved to the tid's new owner),
`AKUMA_AMD64_SIGNAL_DELIVERY.md` (`deliver_signal` named a recycled slot; `gettid` answered the *pid*),
`SELFHOST_DEVBOX_SMOLTCP.md` (`clone_thread` published the pid where the tid belonged; a dead tid's futex entry absorbed
a wake), plus `CURRENT_TRAP_FRAME[tid]`, `DRAINING[tid]` and `GROUP_EXIT` leaking to the next occupant. Several of these
are pid/tid confusion, which `NEWTYPE_AUDIT.md` ("no audit evidence of confusion bugs") and
`UNSAFE_OWNERSHIP_TYPES_DESIGN.md` §3 ("no confusion bugs on record") both said did not exist. Those two statements
should be corrected; this report does not edit them.

**Sketch.**

```rust
/// A slot index below MAX_THREADS by construction; indexes the per-slot tables without a check.
#[derive(Clone, Copy, PartialEq, Eq)] pub struct TidSlot(u16);
/// One incarnation of a thread (today's WakeHandle, renamed and widened).
#[derive(Clone, Copy, PartialEq, Eq)] pub struct ThreadRef { slot: TidSlot, gen: u64 }
/// Proof that the incarnation is live, valid only inside `with_live`.
pub struct LiveThread<'a> { slot: TidSlot, _held: &'a SlotStateGuard }
impl ThreadRef {
    pub fn current() -> Self;
    /// Re-checks `gen` under the slot's state lock and runs `f` only if it still matches.
    pub fn with_live<R>(self, f: impl FnOnce(LiveThread<'_>) -> R) -> Option<R>;
}
pub fn deliver_signal(t: LiveThread<'_>, sig: Signal);
pub fn stamp_exit(t: LiveThread<'_>, code: i32);
```

`Pid` stays `u32` (pids come from a monotonic allocator and do not recycle the same way), but becomes a newtype distinct
from `TidSlot`, so publishing one where the other belongs no longer compiles.

**What the type alone does not fix (inferred).** A check-then-act on the generation is still a race. The guard has to sit
*inside* the action, under whatever lock orders slot recycling, which is why `with_live` hands out a borrowed proof
rather than a `bool`. The existing `THREAD_PID_MAP` ownership checks are the runtime form of this. The type makes them
impossible to skip.

**Cost.** Large: the 60 `slot: usize` functions, every stored tid, and both kernels' schedulers. Migrate incrementally,
starting with the stored tids that outlive a syscall (`Process::thread_id`, `kill_thread_group`, signal delivery, channel
exit stamps), because every incident above starts from one of those. Runtime: one generation load per use, which
`WakeHandle` already pays. `TidSlot` on its own (just the bounded index) is **not worth doing**: `current_tid()` already
halts on an out-of-range value (`preempt.rs:103`), and the 83 explicit `>= MAX_THREADS` guards only make out-of-range
calls do nothing silently.

## 6. `FileKey { mount, ino }`

**Evidence (verified).** 25 production signatures take `(mount_id: u32, inode: u32)` adjacently (`rg '(mount_id|inode):
u32, (inode|mount_id): u32'`), across `akuma-fpcache`, `akuma-fpcache-rw`, `akuma-vfs-glue/src/{lib,fs}.rs`,
`akuma-exec/src/shm.rs` and `akuma-exec-core`. `lookup_and_ref` (`akuma-fpcache/src/lib.rs:322`) then treats `0` as a
sentinel for each one separately. The incident is `FPCACHE_MOUNT_IDENTITY.md`: the page cache was keyed by inode alone,
so "a box and the host ... could read each other's file data".

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileKey { pub mount: MountId, pub ino: Ino }   // MountId(NonZeroU32), Ino(NonZeroU32)
```

**Cost.** Small: 25 signatures, all mechanical. The `NonZero` niches keep `Option<FileKey>` at 8 bytes (inferred).
**Do.**

## 7. `Protection` instead of `(prot, prot_recorded)`

**Evidence (verified).** `MmapRegion` has `pub prot: Prot` and `pub prot_recorded: bool`
(`akuma-mmap/src/region.rs:46, 80`). The doc says `NONE` "is otherwise two different facts in one `u64`". Fork still
builds the child region and then patches the second field afterwards (`akuma-exec/src/process/mod.rs:3151-3153`). The
grant-side reader `eager_region_flags_for_page_fault` (`children.rs:1484`) reads `.prot` directly. That is correct for
granting, but the type does not make the reader choose. `GRANT_RECORDS_VS_DENY_RECORDS.md` instance 4 is exactly
"carries the value, drops 'was it recorded'", and the class killed `rustc` four times.

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Protection { Unrecorded, Stated(Prot) }
impl Protection {
    pub fn for_grant(self) -> Prot;          // Unrecorded => NONE: grants nothing
    pub fn for_deny(self) -> Option<Prot>;   // Unrecorded => None: denies nothing
}
```

**Cost.** Small, and mostly inside `akuma-mmap` (which forbids `unsafe` and has no dependencies): about six constructors
and about ten region readers elsewhere. Also make `MmapRegion`'s fields private while at it; only `akuma-mmap` writes
them today (one write, verified). **Do.**

## 8. Stage B of the user boundary: `UserAddr` / `UserSlice` everywhere

**Evidence (verified).** Syscall arguments enter as `args: &[u64; 6]` and are cast at dispatch (185 `args[n]` uses in
`glue/src/lib.rs`). 170 `sys_*` handlers take 115 `*_ptr: u64|usize` parameters. Validation and copy are separate calls
at ~445 sites (`validate_user_ptr` 101, copies ~340). User pointers are offset with plain arithmetic
(`iov + i * 16`, `glue-amd64/src/lib.rs:240`; `ptr + i * 8`, `execve.rs:84`); a wrap there lands on a different *user*
address, which the next copy validates again, so that is a correctness issue rather than a safety one (inferred). The
test bypass is a production runtime flag: `BYPASS_VALIDATION` is consulted by `validate_user_range` in every image
(`akuma-user-access/src/lib.rs:575`), plus 6 non-test reads in glue and amd64 (audit M7).

```rust
#[derive(Clone, Copy)] #[repr(transparent)]
pub struct UserAddr(u64);                                // a raw syscall argument
pub struct UserSlice<'m> { addr: u64, len: usize, _m: PhantomData<&'m ()> }   // validated range
impl UserAddr {
    pub fn slice(self, len: UserLen, pf: Prefault) -> Result<UserSlice<'_>, Errno>;
    pub fn offset<T: AbiPlain>(self, i: usize) -> Result<UserAddr, Errno>;    // checked
    #[cfg(kernel_tests)] pub fn kernel_for_test<T>(v: &T) -> UserAddr;       // the bypass, compiled out of no-tests
}
pub fn copy_from_user(dst: &mut [u8], src: UserSlice<'_>) -> Result<(), Errno>;
```

A validated slice can go stale (another thread unmaps it) between validation and copy. That is safe: the copy still runs
under fault recovery, and a user mapping can never be replaced by the kernel identity map, because `MAP_FIXED` refuses
the hole (`akuma-syscalls-mem/src/mmap.rs:123`) (inferred from the code read).

**Cost.** Large and mechanical, touching every handler in glue, `glue-amd64` and `akuma-kernel-amd64`. It pays off
mostly by turning M7 into a compile-time property and by making stage A's rules hold for every new handler. Do it after
1-3, crate by crate, with a ratchet on untyped `_ptr` parameters like `check_addr_params.py`. The addr-params budget
already lists `akuma-syscalls-glue` at 40 user `addr` parameters that `NEWTYPE_AUDIT.md` says "should move to a different
type (`UserAddr`)".

## 9. `IrqSpinlock<T>`: masking IRQs is part of acquiring the lock

**Evidence (verified).** Masking is a separate call at each site: 272 `with_irqs_disabled(` calls, typically
`with_irqs_disabled(|| TABLE.lock()...)` (`glue/src/recordlock.rs:91`). Two locks already bundle the two:
`AddressSpaceGuard` holds the spinlock guard and an `IrqGuard` (`akuma-exec/src/process/address_space.rs:268`; the field
order makes drop unlock first, then restore DAIF), and `akuma-bkl`'s `PreemptBoundedGuard` does the same for
preemption. **Incidents:** `IRQ_HANDLER_TABLE_DEADLOCK.md` (`IRQ_HANDLERS` taken with IRQs on while `dispatch_irq` takes
it from the vector), `STABILITY_URGENT_ISSUES.md` (`RUNTIME` self-deadlock from an IRQ handler),
`THREADING_RACE_CONDITIONS.md` (`POOL` + allocator deadlock, "fixed via `IrqGuard` around `POOL.lock()`").

```rust
pub struct IrqSpinlock<T> { inner: Spinlock<T> }
pub struct IrqSpinlockGuard<'a, T> { g: SpinlockGuard<'a, T>, _irq: IrqGuard }  // drops g, then restores DAIF
impl<T> IrqSpinlock<T> { pub fn lock(&self) -> IrqSpinlockGuard<'_, T>; }        // there is no unmasked lock()
```

**Cost.** Medium. Classify the 102 static spinlocks: any lock an IRQ handler or the scheduler takes becomes
`IrqSpinlock`, and its `with_irqs_disabled` wrappers go away. The generated code is the same as today's paired calls
(inferred). This does **not** cover lock *ordering* (AB-BA, e.g. `CONNECT_TIMEOUT_LOCK_INVERSION.md`); see "not worth
doing".

## 10. `Pages` and `PageRange`

**Evidence (verified).** Page counts and byte lengths are both `usize`, and the conversions wrap in release:

- `munmap_len` computes `(len + 4095) & !4095` (`akuma-syscalls-mem/src/mmap.rs:150`). For
  `len > usize::MAX - 4095` that is `0`, so `sys_munmap` (`glue/src/mem.rs:1352`) unmaps nothing and returns 0 where
  Linux returns `EINVAL`.
- `sys_mprotect` computes `len.div_ceil(4096) * 4096` (`mem.rs:1283`). That wraps to 0 for `len > 2^64 - 4096`, and the
  call returns 0 having changed nothing.
- `mremap::plan` bounds `old_addr` but not `new_size` (`mremap.rs:36`). `new_pages * 4096` (`mem.rs:807`) wraps to 0
  when `new_size > 2^64 - 4096`. The call is rescued only because the placer returns `None` for a zero length
  (`akuma-mmap/src/place.rs:66`), so the result is `ENOMEM`.

All three are benign today. The two that were not (`AKUMA_EXTRACT_MMAP.md` §10.1: an unbounded `madvise` loop and a
`MAP_FIXED` guard bypass) were the same arithmetic.

```rust
/// A page count no larger than the 48-bit user space, so `bytes()` cannot overflow.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pages(u64);
impl Pages {
    pub const MAX: u64 = (1 << 48) / 4096;
    pub fn from_len_ceil(len: usize) -> Option<Pages>;     // None past MAX: the boundary check, once
    pub const fn bytes(self) -> usize;                     // infallible
}
pub struct PageRange { start: VirtAddr, pages: Pages }      // start aligned, end() <= 2^48: checked at construction
```

**Cost.** Medium: about 100 page-math sites (30 in `glue/src/mem.rs`, 25 in `akuma-mmap`, 13 in `akuma-syscalls-mem`,
24 in `akuma-mmu`, 10 in `akuma-exec`). Fold it into wave 4 of the existing address plan (`typed-addresses.md`:
`akuma-mmap` after `akuma-pmm`) rather than running it as a separate sweep. Before then, a one-line `new_size` bound in
`mremap::plan`, like the one `len_too_large` gives `mmap`, removes the one case that is saved only by luck.

## 11. `!Send` on per-thread and per-core guards

**Evidence (verified).** `LifecycleGuard` is `!Send` because "the disable/enable pair must run on the same thread"
(`akuma-exec/src/process/lifecycle.rs:71`). Three guards with the same property are `Send`, because their fields are plain
`bool`/`u64`: `IrqGuard` (restores this core's DAIF; `akuma-primitives/src/irq.rs:170`), `PreemptGuard` (per-thread
counter; `preempt.rs:399`) and `BypassValidationGuard` (restores the *current* tid's slot on drop;
`akuma-user-access/src/lib.rs:482`). `typed-addresses.md` already states the rule ("per-core capabilities are
`!Send`/`!Sync`"). Add `PhantomData<*mut ()>` to each. No incident is on record. Trivial cost; do it the next time those
files are touched.

## Not worth doing

- **Newtypes for every id.** The scan found 63 production signatures with two same-typed id parameters next to each other
  (`(pid, fd)`, `(uid, gid)`, `(pid, ppid)`, ...). Only `(mount_id, inode)` (§6) and tid/pid (§5) have incidents behind
  them. `uid`/`gid` live in `Creds`, which already prevents most mix-ups. Revisit an id type when a bug appears, as
  `NEWTYPE_AUDIT.md` recommends.
- **`TidSlot` alone** (§5): it only turns silent out-of-range ignores into impossible ones. The generation check is what
  matters.
- **Lock ordering in types** (lock levels in the style of the `lock_ordering` crate). `BUG_FIX_LIST.md` has 41
  deadlock/inversion entries, but encoding a global order over ~100 locks in ~30 crates is a redesign. The `[stuck]`
  reporter (`spin_report.rs`) and the `akuma-bkl`/`akuma-locks-rw` model checkers are better value.
- **"May allocate" / "may sleep" context tokens** (e.g. no frame allocation inside `with_address_space`,
  `akuma-exec/src/process/mod.rs:1171-1176`). The global allocator and the PMM are free functions, so a token cannot stop
  a call. Threading a context through every allocation site is a far larger change than the bugs justify.
- **A lock token for the nine free `map_user_page` calls** (`akuma-exceptions/src/lib.rs`). `AKUMA_MMU_SAFE_FACADE.md`
  explains why three of them must not take `as_lock`, and the `&mut UserAddressSpace` forms already made the unlocked
  spelling inexpressible elsewhere (the `io_setup` fix under `SYSCALL_UNSAFE_CLEANUP.md`). A `NoLockFaultPath` marker
  would add little; consider it only when that file is next reworked.
- **Typed PTE flags and `map_user_page(va, pa, flags: u64)`.** Real, but already planned: `TRIM_FAT_PTE_NEWTYPE.md`
  Stage B and wave 5 of the address plan (`akuma-mmu`, 131 untyped parameters). Do not start a second sweep over the
  same file.
- **A canonical-path type for the whole VFS.** `Filesystem` takes `&str` everywhere. The narrow version is worth it later:
  `MountSet::{mount, unmount}` (`akuma-vfs/src/mount.rs:110, 163`) compare raw strings and depend on callers to
  canonicalize (`BOX_ISOLATION_SECURITY_FIXES.md` §2i). A `CanonPath` argument there alone would cover that.
- **Discarded results** (`let _ = write_user_val(..)`: `clock_adjtime` reports success after a failed write,
  `syscalls-time/src/lib.rs:445`; the `wait4` incident under `ERROR_HANDLING_AUDIT.md`). `Result` is already `must_use`;
  this needs a lint (`clippy::let_underscore_must_use`) or review, not a type.
- **Turning on `overflow-checks`.** Not a type change. With `panic = "abort"` it turns a silent wrap into a kernel halt
  (`AKUMA_EXTRACT_MMAP.md` §10.1.1); §10 is the type-level answer.

## Cost notes common to all of the above (inferred)

Newtypes, the enums and the permit types are `repr(transparent)` or zero-sized, with no runtime or code-size cost. The
`const { assert! }` checks run at compile time. `UserSlice`/`ThreadRef` add one comparison that the code mostly already
does. Self-hosted build time should not move measurably: none of this adds proc macros or heavy generics.
`AbiPlain`'s macro is `macro_rules!`, and `UserAddr::offset::<T>` monomorphizes per ABI type (a few dozen). This is
unmeasured: check `cargo build --timings` before and after the first two items.

## Background

- [`SECURITY_AUDIT_2026_11_16.md`](SECURITY_AUDIT_2026_11_16.md): H2, M7, M9 and the formal-verification plan this
  complements.
- [`NEWTYPE_AUDIT.md`](NEWTYPE_AUDIT.md), [`../reference/typed-addresses.md`](../reference/typed-addresses.md),
  [`UNSAFE_OWNERSHIP_TYPES_DESIGN.md`](UNSAFE_OWNERSHIP_TYPES_DESIGN.md): the address and ownership types already
  planned.
- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) §4, [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md): the user-copy fold and the AP-bit
  check that defect A bypasses.
- [`AKUMA_EXTRACT_MMAP.md`](AKUMA_EXTRACT_MMAP.md) §10.1: the user-length overflows.
- [`GRANT_RECORDS_VS_DENY_RECORDS.md`](GRANT_RECORDS_VS_DENY_RECORDS.md): the protection-record class.
- [`BOX_ISOLATION_SECURITY_FIXES.md`](BOX_ISOLATION_SECURITY_FIXES.md): checks that existed and were never called.
- [`FPCACHE_MOUNT_IDENTITY.md`](FPCACHE_MOUNT_IDENTITY.md): the inode-without-mount key.
- [`KTG_STALE_TID_EXIT_STAMP_J4_HANG.md`](KTG_STALE_TID_EXIT_STAMP_J4_HANG.md),
  [`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md),
  [`AKUMA_STALE_TID_ZOMBIE_SLOT.md`](AKUMA_STALE_TID_ZOMBIE_SLOT.md),
  [`AKUMA_AMD64_SIGNAL_DELIVERY.md`](AKUMA_AMD64_SIGNAL_DELIVERY.md): stale and confused thread ids.
- [`IRQ_HANDLER_TABLE_DEADLOCK.md`](IRQ_HANDLER_TABLE_DEADLOCK.md),
  [`THREADING_RACE_CONDITIONS.md`](THREADING_RACE_CONDITIONS.md): IRQ-context self-deadlocks.
- [`AKUMA_MMU_SAFE_FACADE.md`](AKUMA_MMU_SAFE_FACADE.md), [`SYSCALL_UNSAFE_CLEANUP.md`](SYSCALL_UNSAFE_CLEANUP.md): the
  lock-in-type precedent and why three no-lock map paths stay.
- [`BUG_FIX_LIST.md`](BUG_FIX_LIST.md): incident counts cited above.
