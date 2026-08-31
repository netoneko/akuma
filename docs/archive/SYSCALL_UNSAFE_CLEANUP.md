# `src/syscall/` to zero `unsafe`, and the ban that keeps it there

**Date:** 2026-08-31
**Scope:** `src/syscall/` (24 files, 11,505 lines), plus the crate-side homes the
unsafe operations moved into.
**Result:** **17 `unsafe` blocks → 0**, and `src/syscall/mod.rs` now carries
`#![forbid(unsafe_code)]`. `scripts/cloc_akuma.py` reads `src/syscall` at
**0 sites, 100.0%** — see [`crate-safety.md`](../reference/crate-safety.md).

Why this subtree and not the whole bin crate: `src/exceptions.rs` alone has 87
sites and page-table and trap-frame work is the job there. `src/syscall/` is the
subtree that runs with **userspace-controlled arguments on every call**, so it is
the one where a stray `unsafe` is worth a compile error.

---

## 1. What the ban does and does not mean

It means no `unsafe` is written *here*. It does **not** mean the syscall layer is
proven sound. The operations that were genuinely unsafe still are; they now live
behind named functions in the crate that owns the thing being poked, where the
obligation is stated once and discharged once instead of at each call site.

| was, in `src/syscall/` | is, in a crate | newly *checked*? |
|---|---|---|
| `copy_from_user_safe` byte loop | `akuma_exec::…::user_access::read_user_byte` | no — trampoline as before |
| `map_user_page` + hand-rolled frame tracking | `UserAddressSpace::map_user_page_tracked` | **yes** — installed-TTBR0 + user-VA |
| `phys_to_virt` + `slice::from_raw_parts` | `akuma_mmu::copy_from_phys` / `copy_to_phys` | **yes** — PMM-managed range |
| `msr tpidr_el0` | `akuma_cpu::sysreg::set_tpidr_el0` | n/a — safe to execute |
| `with_process_exclusive(pid, …)` | `akuma_exec::process::with_own_process_exclusive` | **partly** — see §5 |
| `enter_user_mode` | `akuma_exec::process::enter_user_mode_checked` | **yes** — SPSR targets EL0 |

Three of the moves turned an assumption into a runtime check. One did not, and
§5 is honest about which.

---

## 2. The eight that were just wrong code

These needed no new API — they were raw-pointer spellings of things safe Rust
says directly. Landed in commit `d5383e4b`.

| site | was | now |
|---|---|---|
| `term.rs` ×2 | `copy_nonoverlapping(…, 20)` between `[u8;20]` and a `[u32;9]` tail | `copy_from_slice` via `as_user_bytes{,_mut}` |
| `fs.rs` getdents64 | raw cursor: `write_unaligned` ×3, `copy_nonoverlapping`, 2 `ptr::write` | `akuma_syscalls_linux::dirent::encode` (§3) |
| `fs.rs` eventfd | `ptr::read(cast::<u64>())` off a `&[u8]` | `u64::from_ne_bytes` |
| `mod.rs` sched_getaffinity | `ptr::write(cast::<u64>())` into a `Vec<u8>` | `copy_from_slice(&mask.to_ne_bytes())` |
| `proc.rs` clear_child_tid | `ptr::write(tid_addr as *mut u32, 0)` — **to a user VA** | `write_user_val_with(…, Prefault::No)` |
| `aio.rs` ring header | 8× `(*ring_kva).field = …` through the frame's kernel alias | build the `repr(C)` struct, `write_user_val` |

**Two were latent UB.** `getdents64` and `sched_getaffinity` both used the
*aligned* `ptr::write` through a pointer derived from a `Vec<u8>` (1-aligned).
AArch64 tolerates unaligned normal-memory access, which is exactly why neither
ever showed up.

**One was a real bug.** `clear_child_tid`'s raw store to a user address had no
fixup, so a page that went away between the `is_current_user_page_mapped` check
and the store took an unrecoverable EL1 fault. It goes through the recovery
trampoline now.

---

## 3. `linux_dirent64` moved to the ABI crate

`getdents64` was building records from five bare literals — `0`, `8`, `16`, `18`,
`19` — and `(19 + len + 1 + 7) & !7`. That is now
`akuma_syscalls_linux::dirent`, with `reclen()`, `encode()`, and host tests.

The trap it exists to close, recorded because it is easy to "fix" wrongly: the
natural `#[repr(C)]` header `{u64, i64, u16, u8}` measures **24** bytes, because
C pads a struct to its own alignment — but `d_name` begins at **19**, right after
`d_type`. Reaching for `size_of::<Header>()` as the name offset silently eats the
first five characters of every filename. So this is offsets-plus-an-encoder, not
a struct, and `name_offset_is_not_the_padded_header_size` asserts exactly that.

`encode()` also zeroes the pad itself rather than trusting the caller's buffer to
have arrived zeroed — "the pad happens to be zero already" was a property of one
call site, not of the format.

---

## 4. `map_user_page_tracked` — the contract as a signature

All four `map_user_page` call sites repeated the same ritual: unsafe map, then
`track_user_frame`, then a loop tracking the returned table frames. That
repetition *is* the bug surface — the `[WPF] … cow_ref=0 lazy_self=NONE` incident
that killed cargo mid-build was a discarded `installed` flag.

`UserAddressSpace::map_user_page_tracked{,_no_flush}` turns `map_user_page`'s
four-clause contract into a signature:

1. **as_lock held** — `&mut UserAddressSpace` is only reachable through
   `Process::with_address_space`. Proven by the receiver type.
2. **`va` is a user VA** — checked (TTBR0 range, page-aligned).
3. **`self` is the address space the walk will edit** — checked.
   `map_user_page` reads `TTBR0_EL1`, so it edits *whatever is installed*, not
   necessarily the one you hold a `&mut` to. That is the stale-TTBR0 class this
   tree hit three separate times (`clone_thread`, `fork_process`,
   `vfork_process` — `overlays/devbox/README.md`). Now compared on the L0 base
   and refused, not assumed.
4. **the return is consumed** — both frame lists tracked before returning;
   `installed` is `#[must_use]`.

`&mut self` is load-bearing for clause 1 and nothing else, so clippy's
`needless_pass_by_ref_mut` fires; it is `#[allow]`ed with that reason, because
downgrading it to `&self` silently reopens the clause.

### Not to be confused with the existing `map_and_track`

`akuma-mmu` already had a `map_and_track`. It is the **ELF loader's** primitive
and is a different thing: it walks `self.l0_frame` (so it can build an address
space that is not installed yet) but allocates page tables *inside* the call,
overwrites a live L3 entry without noticing, and issues no TLB flush. None of
those three are acceptable under `as_lock`. Both now carry a cross-reference so
the next reader does not pick the wrong one.

### It found a real SMP bug

`io_setup` was calling `map_user_page` **without holding `as_lock`**, then
tracking through `proc.address_space` directly. On `smp-shared` a concurrent
unmap could free a table frame that walk was descending through. The wrapper's
`&mut self` made the site impossible to convert without fixing; the frame
allocation stays *outside* the hold, because the PMM's reclaim path re-enters
`as_lock`.

---

## 5. `with_own_process_exclusive` — the one that is not proven

`with_process_exclusive`'s `# Safety` has three clauses. The new safe wrapper
discharges two:

- *"`pid` must be the calling thread's own process"* — resolved inside from
  `read_current_pid()`, so there is no pid argument to get wrong.
- *"the call must be on a BKL-held path"* — checked against
  `akuma_bkl::bkl::held_by_current()`; a caller without it gets `None`.

The third — *no other reference to this `Process` may be live on this thread* —
is **a discipline, not a proof**. Nothing stops a caller nesting it. It rests on
the call sites staying enumerated (currently one: `sys_execve`'s destructive
window), which is the same basis on which `Process::with_address_space` is
already a safe function in this crate.

**Adding a second caller is a change to that argument, not ordinary use.**
Phase 7f still owns converting the window itself
([`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §5).

A per-thread re-entrancy flag was considered and rejected: execve's window
**never returns** on success (`enter_user_mode` erets), so a "currently inside"
flag would stay set for that TID forever and refuse the thread's *next* execve.
Closing that properly means splitting the window so it ends before the eret,
which is precisely Phase 7f.

---

## 6. `tpidr_el0` moved *into* `akuma-cpu`

[`INLINE_ASM_CLEANUP.md`](INLINE_ASM_CLEANUP.md) §2 put three registers on one
exclusion row: `msr tpidr_el1` / `tpidrro_el0` / `tpidr_el0` — *"re-points every
per-thread static, **or** userspace's whole TLS"*. Those are two different
arguments, and only the first is about soundness.

- **`TPIDRRO_EL0`** holds the thread id. `akuma_primitives::preempt::current_tid`
  indexes every per-slot static in the kernel with it and halts the core if it is
  out of range. **`TPIDR_EL1`** is the kernel's own per-thread base. Writes to
  either re-point kernel state the kernel then dereferences. Both stay excluded.
- **`TPIDR_EL0`** is opaque userspace state: read in exactly one place in the tree
  (`src/exceptions.rs`, to save it into the trap frame), never dereferenced,
  never indexed with. A garbage value faults EL0's own TLS accesses, in
  userspace, contained to the process that asked for it.

The crate's admission test is "safe to execute", and this one is. Added as
`sysreg::set_tpidr_el0`, next to the reader that was already there. The exclusion
row and `CLAUDE.md`'s copy were corrected rather than left to read as if all
three were one rule.

---

## 7. `copy_from_phys` / `copy_to_phys`, and what they do not promise

The two `phys_to_virt` + `slice::from_raw_parts` sites in `mem.rs` became
bounds-checked bulk copies in `akuma-mmu`, backed by a new
`akuma_pmm::contains(pa, len)` (the PMM had no range query at all). An arbitrary
`usize` now either lands in PMM-managed RAM or is refused — a check the old idiom
never had.

**They return a snapshot, not a stable view.** For `MAP_SHARED` write-back the
frame is still mapped writable in the process, so userspace may be storing into
it as the copy runs and individual bytes may be pre- or post-store. That race is
inherent to writing a shared mapping back to disk and is what Linux's write-back
does too. The improvement is narrower than "now it's safe": the code no longer
hands the compiler a `&[u8]` it is entitled to assume is unchanging.

Cost: one staging page per *call* — allocated outside the loop, not per page. The
loop runs once per 4 KB of a mapping that can be hundreds of megabytes, and the
write-back path runs at process exit, which is exactly where a per-page
allocation would be least welcome.

---

## 8. Verify

```bash
# The ban is real: this must fail to compile.
#   (add `unsafe { core::arch::asm!("nop"); }` to any file under src/syscall/)
cargo check --release -p akuma        # error: usage of an `unsafe` block

grep -rn unsafe src/syscall/          # only prose matches remain
python3 scripts/cloc_akuma.py src crates | grep '^src/syscall'
#   src/syscall   11505   0   0   100.0%   bin

HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo test --target $HOST             # 1083 passed, 0 failed
cargo clippy --release -- -D warnings
cargo clippy --profile extreme-size --no-default-features \
    --features no-tests,smoltcp,extreme,userspace-sshd -- -D warnings

# On a guest (devbox-smoltcp):
python3 scripts/mem_suite.py --port 2222        # 10/10 PASS, 3 DIVERGE
userspace/abiprobe/c/build.sh --push-akuma 2222
ssh -p 2222 root@localhost /tmp/abi_write_probe # 22 checks, 0 failed, 1 skipped
```

Measured 2026-08-31 on devbox-smoltcp, SMP=4: all of the above, plus a 60×
`execve` hammer. **No new guard ever fired** across a full boot and both suites —
no `without the BKL`, no `map_and_track refused`, no `does not target EL0` — so
the BKL really is held at execve, no address space was ever mismatched, and no
context ever failed the SPSR check.

Two traps met while verifying, neither a kernel fault:

- `mem_suite.py` scored `smapsdirty` **SILENT (rc=0)** on one run. The pushed
  binary was **0 bytes** — a truncated base64 transfer, not a regression. The
  suite's refusal to score a silent probe as a pass is what surfaced it; re-push
  and it is `ok, 3 DIVERGE` as before. Check the pushed file's size before
  believing a silent probe.
- The `[WPF]` + `SIGSEGV` pair in the boot log is `eager_mprotect_probe` doing its
  job (`PHASE1/PHASE2 PASS: write correctly SIGSEGV'd`), not damage.

The **termios arm stays unverified on hardware**: Akuma's sshd never allocates a
pty even with `ssh -tt`, and the devbox console runs no shell, so `TCGETS`
returns `ENOTTY` and `abi_write_probe` skips rather than scoring it. It is the
most mechanical change in the set (`copy_from_slice` between two 20-byte views,
lengths now compiler-checked), but it has not been watched to run.

---

## Background

- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) — the 2026-08-12 tree-wide work list this
  finishes a slice of; §P0 (the user-copy wrapper) and §P1 (`repr(C)` structs
  instead of hand-offset byte writes) are the two findings it lands.
- [`INLINE_ASM_CLEANUP.md`](INLINE_ASM_CLEANUP.md) — the `akuma-cpu` migration
  and the exclusion list §6 corrects.
- [`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) §5 — why the execve window is
  still `&mut`-exclusive, and what Phase 7f owes it.
- [`GRANT_RECORDS_VS_DENY_RECORDS.md`](GRANT_RECORDS_VS_DENY_RECORDS.md) — the
  `[WPF]` incident behind clause 4 of `map_user_page`'s contract.
- [`crate-safety.md`](../reference/crate-safety.md) — the tree-wide `forbid`
  tally this adds `src/syscall` to.
