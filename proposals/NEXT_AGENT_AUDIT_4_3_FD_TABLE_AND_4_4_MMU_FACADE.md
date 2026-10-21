# Prompt: post-amd64 audit §4.3 (fd table) and §4.4 (mmu safe facade)

You are running **on Akuma itself** (the ryzen laptop, amd64, bare metal or its Firecracker
twin), in `/src/github.com/netoneko/akuma`, through `crush`. Do the two tasks below, in
order, **one at a time, each finished and written up before the next starts**. They are
independent; if one turns out to be a dead end, say so with evidence and continue with the
other.

Source of both tasks: `docs/archive/POST_AMD64_CODE_AUDIT.md` §4.3 and §4.4 (written
2026-10-17). **Parts of its premises are already stale — verify before you build on them.**
The audit's §4.2 (`akuma-tty`) was done on 2026-10-19 and is the worked example of how this
repo wants such a task done: read `docs/reference/subsystems/tty.md`,
`docs/archive/AKUMA_TTY_EXTRACTION.md` and `crates/akuma-tty/src/differential.rs` first and
copy the method (characterization tests *before* moving code; a pure `forbid(unsafe_code)`
core; the kernel keeps the acting half; delete the old copy instead of keeping both).

## Read first (in this order)

1. `CLAUDE.md` (repo root) — the whole of it. Especially "Kernel conventions" (**analyze
   every new or changed path for allocations**; console output only via `safe_print!`),
   "Crate naming" (`amd64`, never `x86`), the unsafe-budget rule, and "Working with Claude
   Code in this repo".
2. `docs/README.md` (symptom matrix), `docs/reference/subsystems/memory.md`,
   `docs/reference/subsystems/vfs.md`, `docs/reference/subsystems/kernel-hooks.md`.
3. Task A: `amd64/src/fd.rs` (the module doc, lines ~1-130, then `cur_table`/`KERNEL_TABLE`),
   `crates/akuma-exec/src/process/fd.rs` (all 683 lines), `crates/akuma-slot-table/src/lib.rs`
   (module doc only), `docs/archive/AKUMA_AMD64_C2_FD.md` if present.
4. Task B: `crates/akuma-pagetable/src/lib.rs` (the contract), `crates/akuma-mmu/src/lib.rs`
   (it is 4,900 lines — read the module doc and the `unsafe` sites listed below, not the
   whole file), `crates/akuma-user-space/src/lib.rs`, `crates/akuma-mmap/src/lib.rs`,
   `docs/reference/crate-safety.md`, `docs/reference/unsafe-budget.json`,
   `docs/archive/AKUMA_USER_SPACE_LEDGER.md`, `docs/archive/AKUMA_EXTRACT_MMAP.md`.

## Ground rules

- **Never `git commit`, `git push`, `git reset`, `git rebase`** — the user drives all commits.
  Leave the work uncommitted. **No subagents.**
- Do not change behaviour except where a step below says so; every intended change is named,
  pinned by a test, and listed in the write-up.
- Never open-code instructions that a safe wrapper already provides (`akuma-cpu`, `akuma-port`,
  `akuma-dma`, `akuma_pagetable::Table`). **Do not add `#[allow(unsafe_code)]` to a crate that
  carries `forbid`.** A new crate's unsafe budget is 0 (`scripts/check_unsafe_budget.py`; to
  *add* unsafe you must run it with `--update` so the budget diff carries into review — you
  should not need to).
- Console output only through `safe_print!`/`tprint!`; no `format!`/`String` feeding a print.
- **This box's traps:** an ssh/crush session has **no `PATH`** — use full paths or
  `. /etc/akuma-dev.env`. Closing a pty does **not** kill its children on Akuma; `kill -9` any
  background process you start by pid when you are done. Never `pkill -f <pattern>` (it
  matches your own command line). `/proc/self/exe` is missing. Builds are slow and `-j4` has
  corrupted builds here: use `kbuild` (default `-j1`) or `cargo … -j1`.
- Do **not** reboot this machine, install a kernel, or touch `/boot`.

## What you can and cannot verify here

Can: `cargo test` host tests for the crates you touch (try
`. /etc/akuma-dev.env; cd /src/github.com/netoneko/akuma; cargo test --offline --target x86_64-unknown-linux-musl -p <crate>`;
if the host target is not usable offline, say exactly what failed and fall back to reading
and reasoning, and tell the user), `cargo check -p akuma-amd64 --target x86_64-unknown-none`,
`kbuild`, `clippy -D warnings` on the touched crates, `python3 scripts/check_unsafe_budget.py`.

Cannot: run QEMU/Lima (no KVM inside Akuma), so **the AArch64 build, the AArch64 boot suite
and the amd64 boot suite are the user's to run.** For each task end with a "Verification the
user must run" block giving exact commands, e.g.
`cargo check` (AArch64 kernel), `cargo build --release` + `limactl shell fc -- sh scripts/lima_aarch64_run.sh`
(expect the known baseline: 300 `[Test] … PASSED`, one known failure `test_epoll_socket_waker`),
and the ryzen rehearsal (`DISK=nvme KERNEL=<tests build built on ryzen> sh overlays/ryzen/qemu.sh 0 300 std`,
expect **804 passed, 0 failed**). Do not claim a boot suite passed that you did not run.

---

# Task A — §4.3 the fd table

## What the audit says, and what is actually true

The audit says the fd table is global ("single-kernel-fd-table" parity gap), and
`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.24.4 says the same. **That is stale.** Since
2026-09-06 (C2 step 4b) every process on amd64 has its own `SharedFdTable`
(`crates/akuma-exec/src/process/fd.rs`), `amd64/src/fd.rs::cur_table()` resolves to it, and
the old `FDS`/`FILES` arrays are deleted. Confirm that by reading, then re-derive what is
*really* left. My reading (verify each — if one is wrong, drop it):

1. **The table is not host-testable.** `SharedFdTable` is a `BTreeMap<u32, FileDescriptor>`
   plus two `BTreeSet<u32>` (`cloexec`, `nonblock`) behind three spinlocks, inside
   `akuma-exec`, and its reference bookkeeping goes through the `runtime()` hook table
   (`clone_fd_refs`, `release_fd_entry`, `socket_clone_ref`, pipe refs). Nothing about
   "lowest free fd", `dup` numbering, `close_range`, cloexec-on-exec or fork inheritance can be
   exercised without a kernel.
2. **Allocation.** Every insert allocates a BTreeMap node; `get_fd` deep-clones the whole
   `FileDescriptor` (a recent investigation, `docs/archive/` `ls /root`, found a *60 ms per
   call* clone of a directory entry on this path — `with_fd` exists because of it). The
   kernel convention is "prefer fixed arrays and borrows".
3. **Two pinned POSIX divergences**, documented in the `amd64/src/fd.rs` header:
   `dup` copies a `KernelFile` **by value** (two descriptors on one file get independent
   cursors — POSIX shares the open file description), and `nonblock` is keyed by **fd number**
   (so `dup` of a non-blocking socket loses the flag). Fixing the first "honestly is a change
   to `akuma-exec`'s `FileDescriptor` (an `Arc` inside the variant)".
4. amd64 enforces `MAX_FDS = 1024` in its lookup paths while glue's `alloc_fd` has no ceiling
   (documented in `amd64/src/fd.rs` around `sys_dup2`/`sys_pipe2`); a `KERNEL_TABLE` static
   serves the boot task; `EpollFd` entries are stripped on fork.
5. The audit proposes building on `akuma-slot-table`. **Check whether that is a fit.** That
   crate is a lock-free per-slot pointer store with generations and deferred reclamation for
   the *process table*; an open-file-description arena may want something else. Decide, and
   state the reason in the new crate's docs.

## Goal

An `akuma-fdtable` crate (name: `akuma-<thing>`, no suffix) that is **the** fd-table state
machine: pure, `#![forbid(unsafe_code)]`, no dependency on `akuma-exec`, no locks, no hooks.
Suggested shape — adapt it if the code argues otherwise:

- Two layers: a **description arena** (what an open file description is: refcount, and the
  things POSIX makes per-description — file offset, status flags incl. `O_NONBLOCK`) and a
  **per-process name table** (fd number → description id, plus per-fd `FD_CLOEXEC`).
- Generic over the description payload (`D`), so the crate never names `FileDescriptor`
  (the `akuma-locks-rw-cell` parametricity argument in `CLAUDE.md`).
- **Effects are returned, never performed** (the `akuma-pipes`/`akuma-tty` shape): a close or
  a fork returns "these descriptions lost their last reference: release them" / "these gained
  one: bump them"; the kernel performs the pipe/socket/rump releases after dropping its lock.
- Operations: lowest-free allocation from `min_fd`, `dup`/`dup2`/`dup3`/`F_DUPFD`, `close`,
  `close_range`, cloexec set/clear/sweep-on-exec, fork copy (with the epoll stripping rule as a
  stated policy, not an accident), the 1024 ceiling as a parameter.
- Think about **allocation** explicitly: what is the bounded representation (fixed array,
  bitmap + small vec, sorted small-vec)? Justify it against the 1024 ceiling and the number of
  processes; a 4 KiB array per process may be wrong, a BTreeMap node per fd may be wrong. Put
  the analysis in the crate docs.

## Method

1. **Characterization first.** Write host tests for the *current observable semantics* of the
   table (lowest-free allocation, `dup` numbering, cloexec sweep, fork copy, close-then-reuse,
   the refcount effects) against a small model of today's behaviour, and list which of them
   the amd64 boot suite already pins (grep `src/` and `amd64/src/` self-tests for `dup`,
   `cloexec`, `fd`). Include the two pinned divergences as tests that say "PINNED DIVERGENCE".
2. Build the crate, then make `SharedFdTable` a thin wrapper over it **keeping its public API**
   (`alloc_fd`, `alloc_fd_from`, `with_fd`, `get_fd`, `set_fd`, `swap_fd`, `remove_fd`,
   `update_fd`, `set_cloexec`/…, `clone_deep_for_fork`, `close_all`) so `akuma-syscalls-glue`
   and `amd64/src/fd.rs` do not change in this step.
3. Only then, **as a separate, clearly marked step**, decide whether to fix `dup` sharing the
   open file description (and `nonblock` per description). If you do, it is the one intentional
   behaviour change: flip the pinned tests, list it in the write-up, and say which programs it
   affects (shells doing `exec 3>&1`, `dup2` of a file then `lseek`). If the blast radius is
   unclear, **stop and leave it as a recommendation.**
4. Delete the old open-coded logic; do not keep two copies. Repoint tests at the wired twin.

## Done when

The crate + tests; `akuma-exec` using it; `cargo test` for `akuma-fdtable` and `akuma-exec`
green; `cargo check -p akuma-amd64 --target x86_64-unknown-none` and the AArch64 `cargo check`
(user) green; clippy `-D warnings` clean; `check_unsafe_budget.py` ok with `akuma-fdtable` at 0;
`docs/reference/subsystems/` gains `fdtable.md` (or an `fd` section in `vfs.md`) naming the
single owner of each fd property (number→description, cloexec, nonblock, offset); a
`CLAUDE.md` crate-list line; `docs/archive/AKUMA_FDTABLE_EXTRACTION.md` with what you measured;
**correct** the stale sentences in `AKUMA_FIRECRACKER_AMD64.md` §3.24.4 and the audit §4.3
(date the correction — "a stale FIXED is worse than an edited record"). Add rows to
`docs/README.md`'s matrix where a symptom maps to this.

---

# Task B — §4.4 the `mmu` safe-wrapper facade

## What the audit says, and what is actually true

The audit says `akuma-mmu` holds 549 `unsafe` lines, a third of the kernel's total, and asks
for a safe facade (checked map/protect/translate wrappers, raw `unsafe` shrinking to the leaf
writes). **Much of that has already happened:** page-table walks now go through
`akuma_pagetable::Table` (512 `AtomicU64`s; `Table::get/set` are safe; `Table::at` is the one
`unsafe`, with one stated contract). `docs/reference/unsafe-budget.json` now says
**`akuma-mmu` = 31 sites** (kernel total 434). Re-measure (`python3 scripts/check_unsafe_budget.py`,
`python3 scripts/cloc_akuma.py src crates`) and treat 31 as the number to beat.

Where the remaining sites are at `HEAD` (`crates/akuma-mmu/src/lib.rs` line numbers — re-grep,
they move):

| kind | lines (approx.) | what it is |
|---|---|---|
| table-frame claim | 686, 696 | `Table::at(addr)` inside `fn table(addr)` / `table_phys(pa)` |
| public `unsafe fn` API | 3935 `shatter_block_to_pages`, 4020 `map_user_page`, 4190 `map_user_page_no_flush`, 4770 `demote_range_to_ro(l0_ptr: *mut u64, …)` (+ the private `map_user_page_inner`) | free functions taking raw addresses/pointers; **every caller in other crates carries an `unsafe` block for them** |
| shootdown hooks | 910, 924 | `transmute::<usize, fn(…)>` to store function pointers in atomics |
| CR3 | 963, 978, 2962, 3854, 3871 | `x86_write_cr3` and friends (amd64) |
| inline asm / barriers | 1723, 1745, 1772, 1795 | check whether these are instructions `akuma-cpu` already wraps safely |
| user/physmap memory | 1824 `from_raw_parts_mut(phys_to_virt(..))`, 4480 `read_unaligned`, 4855 `write_unaligned` | |
| link-time symbols / DTB | 331 `akuma_fdt::locate`, 4336-4342 extern statics | |
| address-space methods | 2302, 2564, 2579, 3517, 3688 | inside `UserAddressSpace` |

## Goal

Make `akuma-mmu`'s public surface *safe by construction* wherever the obligation can be
expressed in a type, and make the remaining `unsafe` sit in a few named leaf functions that
each state one obligation. Concretely:

1. The four public `unsafe fn`s become **methods on a value that carries the obligation** —
   `UserAddressSpace` already exists and owns a root and a ledger; the free functions that act
   on "the current/boot address space" need a typed handle (`TableRoot`, `L0Ptr`, whatever
   fits) whose constructor is the one place the contract is claimed. Callers outside `akuma-mmu`
   should lose their `unsafe` blocks; **count the call sites before and after** — that is
   half the value.
2. The `usize → fn` transmutes go away (there is `akuma_primitives::Registered`/`OnceCopy` in
   `kernel-hooks.md`; use what the tree already uses).
3. CR3 writes stay `unsafe` (the `akuma-cpu` rule: writes that re-point state the kernel
   dereferences are never safe) but become **one** leaf, like `x86_write_cr3` should already
   be; and any instruction `akuma-cpu` already exposes safely must be used instead of open-coded.
4. Do not weaken a stated invariant to make a signature safe. If a function is only sound under
   a lock or an IRQ mask, the safe version must take the guard (or be a method on something that
   holds it). If you cannot express it, leave it `unsafe` and say why.

## Method

1. Re-measure; produce the table above for the *current* tree in your write-up.
2. For each public `unsafe fn`: find every caller (`grep -rn` over `src/ amd64/ crates/`), say
   what each caller already guarantees, then choose the type that makes the guarantee
   checkable. **Characterization tests before changing signatures** — `akuma-mmu` has host
   tests for the amd64 walkers; add cases for what you are about to move (map/unmap/demote/
   shatter on a fake `PhysMap`), not just compile checks.
3. Move in small steps, each leaving `cargo check` green on **both** targets: this crate is
   built for AArch64 (the primary kernel) and `x86_64-unknown-none`; you can only check the
   second here — keep every AArch64-only item untouched unless you can reason it through, and
   list exactly which AArch64 code you changed for the user to build.
4. After each step run `python3 scripts/check_unsafe_budget.py`. The number for `akuma-mmu`
   must go **down**; when it does, run it with `--update` so the budget file carries the
   decrease into review (and never let another crate's number rise).

## Done when

`akuma-mmu` unsafe sites strictly below 31 with the reason each remaining one cannot be safe
written next to it; the callers' `unsafe` blocks that disappeared counted; host tests green;
`cargo check -p akuma-amd64 --target x86_64-unknown-none` green; clippy `-D warnings` on
`akuma-mmu` and every crate you touched; `docs/reference/subsystems/memory.md` and
`docs/reference/crate-safety.md` updated with the new numbers (regenerate with
`python3 scripts/cloc_akuma.py src crates`, never by hand); a `docs/archive/AKUMA_MMU_SAFE_FACADE.md`
write-up (before/after table, what you could not make safe and why); **correct** audit §4.4's
"549 lines" premise with a dated note. Add a `CLAUDE.md` sentence if you introduce a type that
future code must use instead of the old free functions ("never call X again").

> **Correction, 2026-10-21 (after §4.4 was executed).** The "549 `unsafe` lines, a third of
> the kernel's total" premise was stale: it predates the `akuma_pagetable::Table` migration
> and the budget gate. The measured truth at execution time was **31 production sites**
> (`docs/reference/unsafe-budget.json`), finished at **20**; `akuma-exceptions` lost the nine
> caller blocks (38 → 29). Three of the nine `akuma-exceptions` call sites deliberately do
> not hold `as_lock`, so the audit's "methods on a value that carries the obligation" shape
> was rejected in favour of safe free functions with documented obligations — the same shape
> `update_current_user_page_flags` already had. See
> [`docs/archive/AKUMA_MMU_SAFE_FACADE.md`](../docs/archive/AKUMA_MMU_SAFE_FACADE.md).
> A second stale guess: the lines the audit listed at 1723/1745/1772/1795 and 1824/4484/4859
> were already named safe leaves (`copy_from_phys`, `write_phys`, `with_phys_bytes_mut`,
> `read/write_current_user_val`), not open-coded instructions.

---

## How to report back

When a task is done (or blocked), stop and write, in your final message: what changed (files and
counts), what you measured (tests passed/failed, unsafe sites before/after, allocation notes),
every behaviour change (there should be none for Task B and at most one, named, for Task A),
what you could not verify here, and the exact commands the user must run. If you were wrong
about a premise above, say so first.
