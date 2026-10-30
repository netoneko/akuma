# Taoxichuan: plan to reduce `unsafe` in `taoxichuan/src/` (PLANNING ONLY)

By goose-glm, 2026-10-30. Planning document only — no code changes were made. Composed from the
brief (`BRIEF-unsafe.md`), the full `grep -n "unsafe" taoxichuan/src/*.rs` list (137 lines), the
repo policy docs, and the existing leaf-crate facades in `crates/`. Every class below was
verified by reading at least one site in situ; single sites were read where the plan leans on
their exact shape.

**Repository policy, restated from `docs/reference/crate-safety.md`:** isolate `unsafe` in small
leaf crates whose only job is one stated contract; shrink drivers/bins to safe code; never
open-code an instruction a safe wrapper exists for (`docs/archive/INLINE_ASM_CLEANUP.md`: "can
executing this violate memory safety, for any argument the type allows?" — if no, it is a safe
fn). 70 of 97 crates already `forbid(unsafe_code)`; `akuma-slot-table`, `akuma-dma`,
`akuma-primitives::mmio`, `akuma-locks-rw-cell` are the worked examples this plan copies.

---

## 0. Ground truth (verified up front)

- `grep -c unsafe`: child.rs 50, main.rs 62, exec_twin.rs 15, net.rs 7, sock.rs 2, pipes.rs 1
  = 137 *lines*. Of these, 2 are comment hits (main.rs:5102, child.rs:1451) and 4 are
  `#[unsafe(no_mangle)]` attributes (main.rs:5136, 5296; child.rs:1518, 1624) — attributes are
  not memory-safety sites. **≈131 real sites.**
- **The budget gate does not see taoxichuan today.** `scripts/check_unsafe_budget.py` measures
  `PATHS = ["src", "crates", "amd64"]`; `taoxichuan/src` is in neither `unsafe_sites` nor
  `forbids` of `docs/reference/unsafe-budget.json`, so every change there is uncounted. Slice 1
  must fix this (one line + `--update` baseline) — until then, numbers below are grep-count
  proxies, stated as such.
- Gate vocabulary per `taoxichuan/docs/runbooks/gates.md`: battery stages F1/PK1/fs/apk/git with
  `run_gates.sh`, zero `[Fault] ring3`-absent regressions; plus `scripts/kvm/goose/cow_leak_gate.py
  --cycles 20` (exact-zero free-page delta, `cowrefs=0`) since `clone_user_tree`/`cow_resolve`
  are touched. Host cargo only: `cargo test -p <crate> --target $(rustc -vV | sed -n "s/host: //p")`.
- Precedent ledger shape for the new crates: each states its **one contract** in the crate header
  (`akuma-slot-table`, `akuma-user-access`), carries `#![forbid(unsafe_code)]` if it dereferences
  nothing, and otherwise owns exactly the `unsafe` its contract needs — never a call-site block.

## 1. Census — every `unsafe` site classified

7 classes, 131 sites (counts = grep lines minus comments/attributes; classes verified in situ):

| Class | What it does | Sites | Files | Real safety obligation | Kind |
|---|---|---|---|---|---|
| **A** | `static mut` kernel tables & per-slot scalars accessed as `&'static mut` / volatile reads under "single core, IF clear" | **71** | main.rs 28, child.rs 36, exec_twin.rs 6, pipes.rs 1 | (1) index in range; (2) *no two live `&mut` derived from the same static overlap*; (3) no access while ring 3 runs (IF is the lock). (2) is **unchecked** today — `task()` + `task_slots()` can alias | runtime invariant + convention |
| **B** | physmap derefs: page-table walks (`leaf_entry`, `cow_resolve`, `cow_privatize`, `clone_user_tree`, `free_user_root`, `free_subtree` via `phys::phys_ptr::<[u64;512]>`) and frame byte copies | **21** (all main.rs) | main.rs (146, 337–431, 1185, 3106, 4679–5023, 5111) | frame is PMM-resident and < `PHYSMAP_LIMIT` (asserted by `phys_to_virt`); the table was reached from a present, non-large entry of a user root; PTE writes keep the frame the entry names | runtime invariant (half checkable) |
| **C** | user-memory copy engines: `copy_from_user`/`copy_to_user` inner `copy_nonoverlapping` through the physmap | **2** | main.rs 1456, 1484 | translation exists (`paging::translate_in`), CoW broken first (`cow_prepare_write`), length stays inside the frame | runtime invariant (already checked at the call site) |
| **D** | inline asm: **D1 safe-to-execute** — `pause`, `rdtsc`, `out/in dx`, `rdrand`, `sti;nop;cli`/`sti;hlt;cli` ×3, `cli;hlt`, `out` in `raw_uart` (11); **D2 not-safe-to-execute** — `mov cr3` ×2 open-coded + 2 inside naked asm, `cr0.WP` RMW, `fxsave64`/`fxrstor64`, two `#[unsafe(naked)]` fns (`child_enter`, `exit_cross`), `unsafe impl Sync for Once` (9) | **20** | net.rs 6, exec_twin.rs 7, child.rs 4, main.rs 3 | D1: none (by INLINE_ASM_CLEANUP's test these are safe fns). D2: cr3 swaps the address space under live pointers; fxsave needs a 16-aligned 512-B area; naked asm is an ABI convention (offsets into `CrossSave`/`EntryFrame`) | D1: none; D2: ABI/protocol convention |
| **E** | crossing leaves: calls to `akuma_ring3_amd64::enter_user`, `child_enter`/`exit_cross` call sites, `exec_return` (unsafe fn + body), `akuma_cpu::msr::write(ApicBase)` | **13** | child.rs 8, main.rs 4, exec_twin.rs 1 | enterable ring-3 state; CR3 already the target's; kernel IF clear; MSR write leaves the address field as found | convention (ABI of the crossing pair) |
| **F** | MMIO: `MmioWindow::new(DEVMAP_BASE+base, 4096)` for the LAPIC | **1** | main.rs 639 | the page is the LAPIC register file, mapped device-uncached for the kernel's lifetime, page-aligned | runtime invariant |
| **G** | byte reinterpretation: `struct_bytes` (`repr(C)` POD as bytes), `sock.rs` `read_unaligned` + `from_raw_parts` on `SockAddrIn`, no-op-vtable `Waker::from_raw` | **4** | main.rs 1, sock.rs 2, net.rs 1 | type is `repr(C)` plain-old-data, no pointers/niches; vtable fns ignore the data pointer | type-level/convention |

Notes against the brief's example list: the "raw pointer in the trap/context-switch path" sites
(`TRAP_STACKS`, `TSS`, `GDT`, `TRAP_STACK` — main.rs 211/238) are class A-shaped but raw-pointer
only (never references) and live in the boot path; they go with slice 2's cell but their
obligation is one-core-boot rather than IF-clear. `akuma-user-access`'s fault-returning copy loop
does **not** apply: its x86 story needs the exception-table fixup in `amd64/src/idt.rs`, while
taoxichuan deliberately uses translate-then-copy because "the kernel's physmap writes never
fault" (`docs/archive/TAOXICHUAN_COW_FORK.md`) — class C is a *decision table*, not a missing
crate (see §5 Q4, doc-wins note).

## 2. Per class — existing facade vs new leaf type

### A → new crate `akuma-per-slot` (modeled on `akuma-dma`'s borrow word)
Existing facades evaluated: **`akuma_slot_table::SlotTable<T, N>`** (crates/akuma-slot-table) is
the wrong shape — it heap-`Box`es each slot, keeps atomic state/generation/cooldown for
cross-core BKL-protected scans, and its contract is deferred reclamation. taoxichuan wants BSS
inline arrays, no heap, no generations. **`akuma_locks_rw_cell`** is parametric but lock-shaped.
`DmaArray`'s borrow word is exactly right: `state: AtomicU32`, `0` free / `u32::MAX` writer /
reader count.

```rust
// crates/akuma-per-slot (new). Deps: akuma-primitives (only for volatile opts). NOT forbid(unsafe_code):
// its whole job is the UnsafeCell + borrow word. Host tests FIRST (state machine, no_std, pure logic).
//! One contract: a borrow taken from a cell/element cannot overlap another; a violating
//! access PANICS (aliasing becomes a bug report, not UB). Indexing is bounds-checked.
pub struct PerCell<T>   { v: UnsafeCell<T>, state: AtomicU32 }          // scalars: CURRENT, TASK_SLOT, SHELL_THREAD_TID, NEXT_SLOT, HEAD, FXHEAD, MM_HEAD, N_PROGRAMS
pub struct PerSlot<T, const N: usize> { cells: UnsafeCell<[T; N]>, state: AtomicU32 } // the tables
impl PerCell<T> { const fn new(v); fn set(&self, v); fn get(&self) -> T;         // plain, for non-crossing scalars
                  fn set_volatile / get_volatile;                                // CURRENT & friends (read across opaque asm)
                  fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R; }         // borrow-word guard
impl PerSlot<T,N> { fn with<R>(&self, i: usize, f: impl FnOnce(&mut T) -> R) -> Result<R, SlotErr>;
                     fn get_cloned(&self, i) where T: Copy; }
```
Consumers keep the domain logic; `static CURRENT: PerCell<usize> = PerCell::new(NO_SLOT);` etc.
`child.rs`'s header comment "the discipline that owns every static in this module" becomes the
crate's stated contract, machine-checked. `static mut` disappears from all six files.

**Before → after (representative, `task()`, main.rs:1392):**
```rust
// before: fn task() -> &'static mut Task { unsafe { let slot = read_volatile(&raw const TASK_SLOT);
//     &mut (*addr_of_mut!(TASKS))[slot] } }                       // &mut escapes; aliasing unchecked
// after:  TASKS.with(task_slot_index(), |t| …) -> R              // or task_snapshot() -> Task where Task: Copy-ish
```

### B → `TableFrame`/`Frame` view types, added to `akuma-boot-amd64::phys`
`phys.rs` already owns physmap arithmetic (`phys_to_virt` asserts range; `phys_ptr<T>` returns a
raw `*mut T` — the *unchecked* spelling). It already has an owned-`Frame` idea (line ~110).
Add, next to it, one unsafe constructor per view and safe methods after:

```rust
pub struct TableFrame { pa: u64 }               // pub struct Frame { pa: u64 } for data pages
impl TableFrame {
    /// # Safety: `pa` is a PMM-resident page-table frame whose table belongs to a walk
    /// reached from a present entry of a root (the ctor discharges "reached from present";
    /// phys_to_virt discharges "in the physmap"). ONE ctor per walk instead of a block per access.
    pub unsafe fn at(pa: u64) -> Self;
    pub fn entry(&self, i: usize) -> u64;                 // bounds-checked load
    pub fn entry_is_present_large(&self, i: usize) -> Option<(bool, bool)>;
    pub fn pte_at(&self, i: usize) -> Pte;                // Pte = newtype{ ptr: *mut u64 } with safe read()/write(v)
}
impl Frame { pub fn copy_from(&self, src: &Frame); pub fn read_bytes(&self, off, dst: &mut [u8]); pub fn write_bytes(...); }
```
PTE encodings stay local (`PTE_W`/`PTE_COW` bit 9 per COW_FORK doc); `akuma-pte-amd64` already
forbids and carries the bit algebra — reuse, don't duplicate. `akuma_mmu` has the analogous
`copy_from_phys`/`copy_to_phys` (SYSCALL_UNSAFE_CLEANUP table, "newly checked") but AArch64/`akuma-boot`
shaped; the amd64 physmap lives in `akuma-boot-amd64`, so the views belong there, shared with
`amd64/src/paging.rs` (§4).

**Before → after (representative, `cow_resolve`, main.rs:360):**
```rust
// before: let pte = unsafe { slot.read() };                      // slot: *mut u64 from leaf_entry
// after:  let t = unsafe { TableFrame::at(root) };               // one ctor at walk entry
//         let Some(pte) = t.pte_at_leaf(va) else { return CowAction::Fault };
//         pte.write((pte.get() | PTE_W) & !PTE_COW); akuma_cpu::tlb::invlpg(…);   // all safe
```
`clone_user_tree` loses both its `unsafe fn` mark and its body block; `free_user_root`'s local
`unsafe fn table()` dies; class B's 21 sites → ~5 ctor lines.

### C → expressed over `Frame` (no new crate)
`copy_from_user`/`copy_to_user` keep their region gate + `cow_prepare_write` +
`paging::translate_in` (all already safe) and replace the inner `copy_nonoverlapping` with
`Frame::read_bytes/write_bytes` — the 2 sites leave taoxichuan; the unsafe moves into
`akuma-boot-amd64::phys` (budgeted there: +2). The page-at-a-time loop decision table is pure
logic → extract to host tests inside the same slice (the hole-allocator precedent: pure logic
becomes a crate with host tests).

### D1 → adopt existing safe wrappers; consolidate the window (needs §5 Q1)
- `outb`/`inb` (net.rs 46/56, `raw_uart` main.rs ~5332) → **`akuma_port`** exists for exactly
  this (4 asm sites behind one `allow(unsafe_code)`; it already replaced `amd64/src/port.rs`).
  Raw violation of "never open-code an instruction a safe wrapper exists for" today.
- `rdtsc` (net.rs 98) → `akuma_cpu::tsc` exists.
- `pause` (net.rs 67) → `core::hint::spin_loop()` (its non-x86 arm already is that; on x86_64 it
  emits `pause`).
- `sti;nop;cli` ×2 (exec_twin tick_window/allow_tick) and `sti;hlt;cli` ×1 (net.rs wait_lap):
  three open-coded copies of one idiom → ONE safe fn in `akuma_cpu` (e.g.
  `akuma_cpu::irq::one_instruction_window()` / `halt_until_irq()`), argued safe by
  INLINE_ASM_CLEANUP's test (no memory access; the bounded-handler obligation is a documented
  contract, as `akuma-slot-table` documents its reclamation discipline). **This needs user
  approval** (Q1) because the tree has so far kept IF-gating in `unsafe`.
- `halt`'s `cli;hlt` → same fn.
- `rdrand` (net.rs 263) → check `akuma-primitives::rng` (it owns the RNG hook; if no rdrand
  helper exists, add it there — entropy plumbing is that crate's subject).

### D2 → irreducible, but single-homed
- `child_enter`/`exit_cross` naked asm + `unsafe(naked)` attrs (child.rs 1518/1519, 1624/1625):
  the crossing pair IS the contract; keep. CrossSave field offsets get a `const`-assert
  (offset_of! vs the asm literals) so a field rename panics at compile time instead of corrupting
  a stack.
- `mov cr3` open-coded ×2 (main.rs spawn ~5120, exec_twin 146 & 601 — 3 sites) → one local
  `fn switch_root(root_pa: u64)` in exec_twin (or a documented `unsafe fn` on `akuma_cpu::ctlreg`
  — but main.rs records ctlreg *deliberately* refuses the write; doc wins, keep it local, one
  SAFETY comment instead of three).
- `cr0.WP` (main.rs ~5160): one boot-time control-register write; keep.
- `fxsave64`/`fxrstor64` (exec_twin 187/197): extraction plan §2.5 already proposes
  **`akuma-fpu-amd64`** for the area algebra (size/align/init-image/reset rule, host-tested);
  the two asm sites stay in the kernel per that plan. `FX: [FxArea; N]` static then becomes a
  `PerSlot<FxArea, N>` (class A fix) holding the crate's type → exec_twin keeps exactly the 2 asm
  sites.
- `unsafe impl Sync for spin_idle::Once` (exec_twin 238) + `IDLE_STACK.call` (289) → replace
  with **`akuma_primitives::OnceCopy`** (exists; used by `rng.rs`) if its get-once semantics fit
  `call(f) -> *mut T`; else keep 1 site. Either way the hand-rolled `Once` dies.

### E → keep (the crossing core), shrink the neighborhood
`enter_user`, the 5 `child_enter`/`exit_cross` call sites, `exec_return` (its body is the
return-side asm pair), `msr::write(ApicBase)` stay. Everything AROUND them (the tables, the
walkers, the copies) becomes safe, which is the point of the exercise: the remaining unsafe reads
as "here is the machine boundary" and nothing else. `#[unsafe(no_mangle)]` attrs stay (symbols
`boot.s` names).

### F → keep (1 site, already the facade)
`MmioWindow::new` is the crate's one unsafe ctor; taoxichuan already uses it correctly.

### G → plain safe code
- sock.rs 457/472: give `SockAddrIn` `from_bytes(&[u8;16]) -> Self` / `to_bytes(&self) -> [u8;16]`
  via `u16/u32::from_ne_bytes` — pure, host-testable, belongs in `akuma-syscalls-net` per
  extraction plan §2.4 (sockaddr marshalling is already on that list).
- main.rs `struct_bytes`: one site; move the POD-as-bytes helper into `akuma-primitives` (or
  `akuma-syscalls-abi`) so the `repr(C)` obligation is stated once.
- net.rs 331 noop `Waker::from_raw`: move `noop_waker()` into `akuma-net` (forbids, 0 sites) —
  it is net-runtime vocabulary.

**End state ≈ 131 → ~26 sites:** D2 (naked 4 incl. attrs counted as lines, cr3 helper 1, cr0 1,
fx 2, Once fallback 0-1) + E (11) + F (1) + B ctors (5) ≈ 25-26. Classes A (71), C (2), G (4),
D1 (11) go to **zero** in taoxichuan/src.

## 3. Ordered slices (each ≈100 tool calls, gate-verified)

**S0/S1 — `akuma-per-slot` + the scalars + the gate learns taoxichuan (~100 calls).**
Host tests first (state-machine model: overlap rejected, reentrant with() panics, volatile round-trips).
Edit `scripts/check_unsafe_budget.py` `PATHS` += `taoxichuan/src`; `--update` to seed the baseline (131);
`forbids` gets nothing new this slice. Adopt `PerCell` for the 11 scalar statics (CURRENT, TASK_SLOT,
SHELL_THREAD_TID, NEXT_SLOT, SLOT_FS, HEAD, FXHEAD, MM_HEAD, N_PROGRAMS, CURRENT-in-main, EVENTFDS-count).
Gate: `cargo test -p akuma-per-slot` (host), kernel builds, battery, leak gate.
**Sites 131 → ~120** (A: 71 → ~60).

**S2 — `PerSlot` for the tables (~100 calls).** SLOTS, SLOT_STATE (cross_mut/save_ptr die —
`save_ptr` stays a 1-line raw-pointer mint for the naked asm's `%rdx`), TASKS, CHILD_BIND, PARENT_OF,
EVENTFDS, INFLIGHT, PROGRAMS/EXIT_CODES, PIPES, FX (as `PerSlot<FxArea,N>`), TRAP_STACKS, OWNED_ROOT/
PENDING_ROOT, MM_EVENTS. Panic-on-overlap replaces silent aliasing; run the battery + one-boot cargo
trio + leak gate; any panic is a latent bug surfaced (file it, don't weaken the cell).
Gate: full battery, `cow_leak_gate.py --cycles 20` delta=0, clippy ±trace-err.
**Sites 120 → ~60** (A: ~60 → ~2).

**S3 — physmap views (~100 calls).** `TableFrame`/`Frame`/`Pte` in `akuma-boot-amd64::phys`
(host tests: Vec-backed fake physmap, bounds/reach asserts, copy round-trip). Adopt in: leaf_entry,
cow_resolve, cow_privatize, cow_prepare_write, clone_user_tree, free_user_root, free_subtree,
copy engines, exec load copy, writeback. `cargo test -p akuma-boot-amd64`; kernel: leak gate is
THE gate (walkers changed) + battery + `cargo build -j1` cargo-trio in TCG if available.
**Sites ~60 → ~40** (B: 21 → 5; C: 2 → 0).

**S4 — asm adoption + consolidation (~100 calls, needs Q1).** akuma_port ×3, tsc ×1,
spin_loop ×1, window fn ×3→1 (+halt), rdrand move, switch_root consolidation ×3→1, OnceCopy swap,
CrossSave offset const-asserts. Gate: TSC calibration line in boot log, net relay smoke (ssh round
trip), battery. **Sites ~40 → ~26** (D1: 11 → 0; D2: −4).

**S5 — byte views + ratchet (~60 calls).** sock.rs bytes ↔ `akuma-syscalls-net` (differential
test old-vs-new wire bytes, per extraction plan §2.4), struct_bytes move, noop waker move;
`check_unsafe_budget.py --update` ratchets 131 → ~26 into the ledger; crate-safety.md run note.
**Final: ~26 sites, ~22% of today; classes A/C/D1/G zero.**

## 4. Shared with `amd64/src/` (85 sites today)

- **`TableFrame`/`Frame`** land once in `akuma-boot-amd64` and serve both kernels — `amd64/src/paging.rs`
  (7 sites) has the identical physmap shape; its walks become safe over the same ctor. Biggest
  shared win; do S3 with amd64's paging.rs open beside you and file the adoption ticket same-day.
- **`PerSlot`**: `amd64/src/sched.rs` (17 sites, biggest file) has the same per-slot statics; the
  atomic borrow word is correct cross-core too (it *is* the DmaArray design). `akuma_slot_table`
  remains the right tool where heap `Box`es + BKL scans are the shape (akuma-exec); inline arrays
  are the sched/idt shape. idt.rs (15) partially — its trap-path raw pointers may prefer staying.
- **`akuma-fpu-amd64`** (extraction plan §2.5): taoxichuan's FX is the second consumer; the plan
  already exists — this plan's S2/S4 adopts its types.
- **Already done in amd64, reuse, don't rewrite:** `akuma_port` (amd64/src/port.rs replaced,
  22 call sites cleaned — net.rs is taoxichuan's laggard copy of that same pattern);
  `akuma-user-access` (its x86 fixup lives in amd64/src/idt.rs — do NOT bolt it onto taoxichuan's
  no-fault copy design); extraction-plan slices 1-2 done and taoxichuan already adopted them
  (`akuma_mmap::HoleSet` = the munmap/mremap hole table; `akuma-syscalls-signal` = the real
  per-task handler table — both in the 2026-10-30 log entries).
- **Not proposed here, per the standing plan:** `akuma-user-tree` (§2.3 of
  TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md) already scopes the *decision* half of
  clone_user_tree/free_user_tree with host-testable plans; this plan's S3 is its kernel-side
  twin (the raw walkers), and they should land as one review.

## 5. Risks (each with the cheapest check), docs read, questions

| Risk | Cheapest check |
|---|---|
| Aliasing UB hidden by the single-core convention (e.g. `task()` live while a helper calls `task_slots()`) | S2's borrow flag **is** the check: full battery + cargo trio; a panic is a found bug. Keep `trace-err` on for the ring dump. |
| Interrupt re-entering a held borrow (tick inside a table's `with`) | Syscall path runs IF-clear; ticks only in the window fns and at park. Debug-assert (trace-err builds) in `one_instruction_window` that no PerSlot/PerCell writer word is held. |
| CoW fault handler running inside a syscall that holds a table | Cannot: physmap writes never fault and `cow_prepare_write` pre-breaks; `fault_trap`'s write arm is ring-3-only (`from_ring3()`). Assert once in S3 with a comment + the leak gate. |
| Borrow-flag cost in hot paths (`current_slot`/`task_slot_index` per syscall) | Scalars use plain/volatile get (no CAS); only `with` pays a CAS, and tables are touched per-syscall-event, not per-instruction. Measure the syscall floor before/after S2 (efficiency plan's read(2) stage profiler). |
| Volatile semantics lost in the swap (CURRENT read across opaque asm crossings) | `PerCell::get_volatile` in the crate contract; a host test pins that the volatile ops compile to volatile ops (miri/asm check is overkill — code review + the battery). |
| Gate blind spot: taoxichuan unmeasured until S1's script edit | S1 does it first, so every later slice reports a hard delta. Until then numbers here are grep proxies (stated). |
| Naked-asm offsets drift when `CrossSave` changes | `const _: () = assert!(offset_of!(CrossSave, rsp) == 0x00);` next to the asm (S4). |
| Doc-wins deviations in this plan | Recorded: (1) `akuma-user-access` NOT adopted for tx copies (translate-then-copy is a design, not a gap — COW_FORK doc); (2) cr3 write stays out of `akuma_cpu::ctlreg` (main.rs documents the deliberate refusal); (3) fx asm stays in the kernel (extraction plan §2.5 "+0"); (4) futex/pipes not re-planned (extraction plan §2.6: already extracted). |

**Docs read:** BRIEF-unsafe.md; `docs/archive/TAOXICHUAN_COW_FORK.md` (full); 
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (top + last 3 entries);
`docs/reference/crate-safety.md` (header + audit history); `docs/reference/unsafe-budget.json` (full);
`scripts/check_unsafe_budget.py` (full); `taoxichuan/docs/runbooks/gates.md`;
`docs/archive/SYSCALL_UNSAFE_CLEANUP.md` (§1-2); `docs/archive/INLINE_ASM_CLEANUP.md` (§1-2);
`docs/archive/TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md` (§2-3);
crate headers: akuma-slot-table, akuma-dma (DmaArray), akuma-primitives (mmio::MmioWindow, OnceCopy),
akuma-locks-rw-cell, akuma-user-access, akuma-boot-amd64 (phys.rs, paging::translate_in),
akuma-cpu (ctlreg/tlb/msr surface), akuma-threading (x86_* surface), akuma-ring3-amd64 (enter_user);
in-situ reads: main.rs (137-150, 205-245, 328-440, 628-652, 1105-1125, 1183-1190, 1370-1412,
1450-1505, 3104-3112, 4675-4700, 5012-5030, 5065-5135, 5150-5240, 5290-5345), child.rs (205-245,
425-500, 1510-1548, 1620-1660), exec_twin.rs (50-95, 140-240, 280-330, 590-645), net.rs (40-105,
258-270, 325-335), sock.rs (450-480), pipes.rs (70-85).

**Questions for the user**
1. **Q1 (blocks S4):** may `sti;nop;cli` / `sti;hlt;cli` become ONE safe fn in `akuma_cpu`
   (`one_instruction_window()` / `halt_until_irq()`), with the bounded-handler obligation as a
   documented contract? The tree has kept IF-gating in `unsafe` so far; the INLINE_ASM_CLEANUP
   test says "no memory access ⇒ safe fn", but this changes a standing idiom.
2. **Q2 (S1):** confirm editing `scripts/check_unsafe_budget.py` (`PATHS` += `taoxichuan/src`) +
   baseline `--update` is wanted — it changes the gate's scope, which is the point.
3. **Q3:** `akuma-fpu-amd64` — confirm asm stays in the kernels (standing plan) vs moving the two
   fx sites into the crate (taoxichuan −2, amd64 −2 later).
4. **Q4:** sock.rs + sockaddr marshalling: extend `akuma-syscalls-net` now (extraction plan §2.4,
   verify-then-extend) or ship the local `from_bytes/to_bytes` fix first and extract later?
5. **Q5:** for `spin_idle::Once` → `akuma_primitives::OnceCopy`: acceptable if `OnceCopy` gains a
   `get_or_init`-style accessor (one new safe method in a forbid-carrying crate), or keep the
   local Once with its 1 `unsafe impl Sync`?
