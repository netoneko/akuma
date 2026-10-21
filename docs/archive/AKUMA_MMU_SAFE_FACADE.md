# The `akuma-mmu` safe facade (2026-10-21)

Branch `kvm-part-0`, executing §4.4 of
[`proposals/NEXT_AGENT_AUDIT_4_3_FD_TABLE_AND_4_4_MMU_FACADE.md`](../../proposals/NEXT_AGENT_AUDIT_4_3_FD_TABLE_AND_4_4_MMU_FACADE.md).
Companion piece to [`AKUMA_FDTABLE_EXTRACTION.md`](AKUMA_FDTABLE_EXTRACTION.md) (§4.3, same
session).

## What the audit asked, and what was actually true

The audit (written against an older tree) said `akuma-mmu` held **549 `unsafe` lines, a third
of the kernel's total**, and asked for a safe facade. Both halves of that premise were stale:

- The 549-line figure predates the `akuma_pagetable::Table` migration (512 atomics behind a
  safe `get`/`set`), the raw-pointer helpers `akuma-exec`'s CoW path already consumes
  (`copy_from_phys`, `write_phys`, `with_phys_bytes_mut`, `read_current_user_val`,
  `write_current_user_val`), and the budget gate itself.
  `docs/reference/unsafe-budget.json` had the real number: **31 production sites**, not 549
  lines. Dated correction appended to the audit below.
- A facade already half-existed: `map_user_page_tracked`/`map_user_page_tracked_no_flush`
  (the `&mut UserAddressSpace` forms) and the TTBR0-relative helpers were safe. What remained
  `unsafe` was the **free-function** half of the API and the shootdown-hook transmutes.

Re-measured at start of work: budget 31, tree 31 sites (`python3 scripts/check_unsafe_budget.py`).

## What changed

### 1. The four public `unsafe fn`s are safe (or gone)

| was (`pre-edit` line) | now (post-edit line) | what happened |
|---|---|---|
| `pub unsafe fn map_user_page(va, pa, flags)` (4030) | `pub fn map_user_page` (lib.rs:4051), `#[must_use]` | `unsafe` dropped; `# Safety` section rewritten as **caller obligations** (see below) |
| `pub unsafe fn map_user_page_no_flush` (4200) | `pub fn map_user_page_no_flush` (lib.rs:4220), `#[must_use]` | same, plus "caller must range-flush before userspace reaches the mapping" |
| `unsafe fn map_user_page_inner` (4041) | `fn map_user_page_inner` (lib.rs:4062) | body only touches safe calls (`table_phys`, atomic `Slot` ops, `akuma_cpu` barriers); the `#[must_use]` on the wrappers covers clause 4 at compile time |
| `pub unsafe fn shatter_block_to_pages(l3_frame_addr: usize, …)` (3945) | `pub fn shatter_block_to_pages(frame: PhysFrame, …)` (lib.rs:3953) | the raw `usize` address became the `PhysFrame` the type system handed out; `shatter_into` unchanged |
| `pub unsafe fn demote_range_to_ro(l0_ptr: *mut u64, …)` (4780) | **deleted** | sole caller (`src/process_tests.rs` BENCH-2) switches to the pre-existing `UserAddressSpace::demote_range_to_ro` — identical body, both call `demote_range` |

**Why safe free functions and not methods on a lock guard** (the audit's rule-4 option): eight
of the nine `akuma-exceptions` call sites hold `as_lock` and could use a method, but three
**deliberately do not hold it at call time** — the `None =>` no-owner arm (there is no owner to
lock), the sigreturn trampoline (maps *before* locking, and re-locking risks self-deadlock on
the non-reentrant spinlock), and the QEMU STP-misrouting workaround (comment says "no
`as_lock`"). A `lock()`-taking method would force those three to take the lock — a behaviour
change and a deadlock — so the obligation stays stated in the doc, not expressed in a type.
The crate already had this exact shape safe: `update_current_user_page_flags` (lib.rs:4118),
"current space via TTBR0, caller serializes". The new docs name it as the precedent and name
the three no-lock paths, with "do not add a fourth without writing down why".

**Why no checked user-VA newtype:** akuma-mmu has no canonical user-VA bound constant (the
boundary is a per-VM/platform fact), so a checked type would invent a policy the rest of the
crate does not enforce. `va` stays a documented obligation, like every other TTBR0-relative
walker here.

### 2. The `usize → fn` transmutes are gone

`SHOOTDOWN_SEND`/`SHOOTDOWN_WAIT` (pre-edit lib.rs:910/924) were
`AtomicUsize` + `transmute::<usize, fn(…)>` on read. They are now
`akuma_primitives::OnceCopy<ShootdownSend>` / `<ShootdownWait>` (lib.rs:887-889);
`set_shootdown_hooks` (lib.rs:895) fills them once per boot from `amd64/src/boot.rs:220`, and
readers use `let Some(send) = SHOOTDOWN_SEND.get() else …`. `OnceCopy::set` is first-wins,
which matches the once-per-boot contract that was already in force. Two transmutes, zero new
mechanism — `akuma_primitives` was already a dependency.

### 3. The TTBR0 install is one leaf

`activate`/`deactivate` each carried an identical `dsb ish; msr ttbr0_el1; isb` block inside
the publish bracket (pre-edit lib.rs:2564/2579). Both now call one aarch64-cfg'd leaf,
`publish_and_install_ttbr0(ttbr0: u64)` (lib.rs:2581), which brackets the single
`unsafe` asm in `publish_l0_begin` (lib.rs:1190). Net −1 site; the IrqGuard and
kernel-mapping obligations are stated on the leaf.

### 4. The audit's "check whether akuma-cpu already wraps these" — they were already named safe leaves

The audit guessed pre-edit lines 1723/1745/1772/1795 (inline asm/barriers), 1824, 4484, 4859
were open-coded instructions. **They were not.** Every one was already a named safe helper:
`copy_from_phys` (1702), `copy_to_phys`, `copy_phys_page`, `write_phys` (1777) — each with a
`pmm::contains` range check — `with_phys_bytes_mut` (1800), and
`read_current_user_val`/`write_current_user_val` (4503/4862) with AP-gated runtime checks.
Left untouched.

## The callers' `unsafe`, counted (the audit asked for before/after)

| caller | before | after |
|---|---|---|
| `crates/akuma-exceptions/src/lib.rs` — production blocks around `map_user_page{,_no_flush}` (pre-edit lines 1600, 1718, 1743, 2011, 2022, 2069, 4444, 4872, 4929) | **9** | **0** |
| `src/tests.rs` — test-call blocks (pre-edit 3453, 3492, 4168, 4214, 4273, 4664, 4671, 4809, 7110, 7122, 7217, 7219, and the shatter call 7866) | **13** | **0** |
| `src/process_tests.rs` — BENCH-2 demote (pre-edit 7431) | **1** | **0** |

`src/` production `unsafe` was already 0 and stays 0 (the test sites are outside the budget's
production count; `cloc_akuma.py` reports them separately as `test_unsafe_sites`).

Budget (`docs/reference/unsafe-budget.json`, ratcheted with one `--update` run):

- `crates/akuma-mmu`: **31 → 20** (−11: four `unsafe fn` headers, the two wrapper bodies'
  `unsafe { map_user_page_inner(…) }`, the same block in `map_user_page_tracked_inner`, two
  transmutes, one TTBR0 block from the merge, the free `shatter` header, the free `demote`
  header; nothing added).
- `crates/akuma-exceptions`: **38 → 29** (the nine call-site blocks).
- `amd64/src`: 79 → 81 in the same `--update`. **Disclosed, not mine:** that +2 came in with
  the kvm-ladder commits already on this branch (`bbaa81d1` et seq.) and predates this work;
  the single `--update` run required by the audit absorbs it into the budget file. The
  increase itself is unreviewed here and remains the kvm branch's to justify.

## What stays `unsafe`, and why each cannot be safe

All in `crates/akuma-mmu/src/lib.rs`, post-edit lines:

- **684/696 `table`/`table_phys`** — the one shared walk `unsafe`: `Table::at(addr)` names a
  table from a raw address. One stated contract (the address is a frame this crate allocated
  or the installed root); everything above it is safe because it goes through here.
- **2581 `publish_and_install_ttbr0`** — contains the `msr ttbr0_el1` asm (inside
  `publish_l0_begin`). Re-pointing the register every per-slot `static` dereferences is never
  a safe operation (`akuma-cpu` rule).
- **2967 `x86_write_cr3`** and its three call sites (flush reload ~952, x86 `activate` ~3864,
  x86 `deactivate` ~3881) — CR3 stays `unsafe` per the audit's rule 3; it already *is* the one
  leaf the audit asked for.
- **331 `akuma_fdt::locate`** — link-time DTB symbol, no runtime check possible.
- **4366 `unsafe extern "C"` block** — kernel text/rodata/data bounds from linker symbols.
- **4503/4862 internals** — `read_unaligned`/`write_unaligned` on a user VA; the AP-gated
  runtime check lives in the named method, the raw op stays `unsafe` inside it.
- **3521/3692-area internals** — `write_page_bytes` / `zero_mapped_page` raw copies behind
  named methods with their own checks.

## Verification

- `cargo test -p akuma-mmu --offline -j1 --target x86_64-unknown-linux-musl`: **28 passed,
  0 failed** — including the ten `table_walk_characterization_tests` written *before* the
  signature changes (resolve/demote/shatter on `Box::leak`'d aligned tables; a 2MB block lives
  at L2, so the block test's expected literal is `0x5012_3456`-shaped, not a leaf PA).
- `cargo test -p akuma-exec …`: 104 passed, 2 failed — both pre-existing
  (`ryzen_2026_10_16_zombie_tid_tests`, page-alignment), untouched by this work.
- `cargo clippy -p akuma-mmu --all-targets -- -D warnings`: clean. Two pre-existing host-build
  lint debts fixed to get there, both behaviour-neutral: `x86_walk_leaves`' `edited` flag is
  read only under the amd64 `cfg`, so the lint allowance moved to the function and grew
  `unused_assignments` (the comment there explains the cfg shape); and the ASID test
  `allocate_all_then_none` collected into a `Vec` it never read — the push became the
  `assert!(… .is_some())` it implicitly was.
- `cargo check -p akuma-amd64 --target x86_64-unknown-none`: green.

**Zero compile feedback was available for the AArch64 half** (no rustup, no aarch64 target on
this box). The AArch64-reachable files changed are exactly: `crates/akuma-mmu/src/lib.rs`,
`crates/akuma-exceptions/src/lib.rs`, `src/tests.rs`, `src/process_tests.rs`,
`crates/akuma-mmu/src/asid.rs` (host-only test), `docs/reference/unsafe-budget.json`.
Expected on a Lima boot: 300 passed / 1 known fail (`test_epoll_socket_waker`); on the ryzen
rehearsal (`DISK=nvme KERNEL=<tests build> sh overlays/ryzen/qemu.sh 0 300 std`): 804 passed,
0 failed.

## Background

- [`proposals/NEXT_AGENT_AUDIT_4_3_FD_TABLE_AND_4_4_MMU_FACADE.md`](../../proposals/NEXT_AGENT_AUDIT_4_3_FD_TABLE_AND_4_4_MMU_FACADE.md)
  §4.4 — the tasking (and, at its bottom, the dated correction of its own 549-line premise).
- [`AKUMA_EXEC_AUDIT.md`](AKUMA_EXEC_AUDIT.md) §6 — where the raw-pointer helpers came from.
- [`AKUMA_FDTABLE_EXTRACTION.md`](AKUMA_FDTABLE_EXTRACTION.md) — §4.3, same session.
- `docs/reference/subsystems/memory.md` — the current-state rule this establishes.
- `docs/reference/crate-safety.md` — the regenerated numbers (mmu 20 sites / 2605 lines,
  exceptions 29 / 3104).
