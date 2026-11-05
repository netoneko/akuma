# Unsafe audit, next step: the thread context table, the IRQ-frame layout, and the exceptions page copies

**Date:** 2026-11-06. **Status:** design, not started. **Follows:** [`UNSAFE_KERNEL_CPU_STEP.md`](UNSAFE_KERNEL_CPU_STEP.md) (Part 2 is
the state this starts from, commit `ef8d68e4`). **Plan it belongs to:** [`UNSAFE_KERNEL_AUDIT.md`](UNSAFE_KERNEL_AUDIT.md).
**Audience:** the implementer (glm). Read this whole document, then `docs/archive/BKL_PHASE7D_THREAD_CONTEXTS.md` (the ownership audit this design
encodes) and `CLAUDE.md` (isolate `unsafe` in small crates with one stated contract; `scripts/check_unsafe_budget.py` is the gate).

## 0. Goal and honest target

| component | production `unsafe` sites now | target |
|---|---|---|
| `akuma-threading` | 59 | 30-35 (context table + IRQ-frame helpers + racy reads) |
| `akuma-exceptions` | 27 | 12-15 (page copies, IRQ-frame reads, EL1 crash dump) |

Do **not** chase the last sites: the `global_asm!` blocks, `extern "C"` declarations, `#[unsafe(no_mangle)]` entries, `msr tpidr_el1`/`vbar_el1`
writes and the switch itself are the irreducible core and stay (each with its `SAFETY` comment). The number that matters is that every remaining
site states a contract a reviewer can check, not that the count is small.

The three work items are independent; do them in this order, one commit each (or more), because 1 is the design-heavy one and 2-3 are mechanical:

1. **IRQ-frame layout, single-sourced** (small, unlocks the rest). §1.
2. **Page copies in `akuma-exceptions` through `akuma_phys`** and the EL1 crash dump into `akuma-exceptions-forensics`. §2.
3. **The context table**: ownership tokens instead of `get_context`/`get_context_mut` raw pointers. §3. Then, optionally, switch asm placement. §4.

## 1. The IRQ trap frame: one definition, one reader

**Today.** The aarch64 IRQ frame a thread is switched out from has `ELR_EL1` at `+240` and `SPSR_EL1` at `+248`. This is **not** the EL0
`UserTrapFrame` layout (there `+248` is `sp_el0`), so `akuma_trap_aarch64::FrameHandle` does not fit it (its contract is "the frame the vector asm
just pushed for this handler"). The offsets are written down in three places that must agree: `setup_fake_irq_frame` (threading, writes the
frame), `akuma-exceptions` (`IRQ_FRAME_SPSR_OFFSET = 248`, reads at `irq_eret_poison_check` and three more sites) and `akuma-threading`
(`IRQ_FRAME_ELR`/`IRQ_FRAME_SPSR` + `irq_frame_word`, added in `ef8d68e4`). About 10 raw `read_volatile((sp + 24x) as *const u64)` in total.

**Design.** Add to `akuma-trap-aarch64` (it already depends only on `akuma-exec-core`; keep it that way):

```rust
pub mod irq_frame {
    pub const ELR: usize = 240;
    pub const SPSR: usize = 248;
    pub const SIZE: usize = 832;           // the frame the SGI/IRQ vector pushes
    /// A saved or live IRQ trap frame at `sp`.
    #[derive(Clone, Copy)] pub struct IrqFrame { sp: usize }
    impl IrqFrame {
        /// # Safety  `sp` points at an IRQ frame of `SIZE` bytes on a kernel stack that outlives this value:
        /// the live frame the vector just built, or the `Context.sp` of a thread that is not running.
        pub unsafe fn at(sp: u64) -> Self;
        pub fn elr(self) -> u64;           // read_volatile, one unsafe inside
        pub fn spsr(self) -> u64;
        pub fn from_el0(self) -> bool;     // spsr & 0xF == 0
    }
}
```

Plus **`setup_fake_irq_frame` writes through the same constants** (it currently hard-codes the layout; add `fn write_elr/write_spsr` or a `build(sp, elr, spsr, ..)`
in the same module so the writer and the readers cannot drift). Host test: build a frame in a `[u64; 104]`, read it back through `IrqFrame`, and
assert `size_of::<UserTrapFrame>()`-independence (the point is that 240/248 are *not* `UserTrapFrame` offsets; pin that in a comment-test).

**Call sites to convert** (find with `grep -n "+ 240\|+ 248\|IRQ_FRAME" crates/akuma-threading/src/lib.rs crates/akuma-exceptions/src/lib.rs`):
threading `irq_frame_word` users (`dump_thread_resume_points`, `get_saved_kernel_resume`, the SGI handler's `interrupted_el0` and POISON tripwire);
exceptions `irq_eret_poison_check` (2 reads) and the three `IRQ_FRAME_SPSR_OFFSET` reads. Each creates one `IrqFrame::at` with the existing SAFETY text.
Expected: ~10 raw sites -> ~6 `IrqFrame::at` sites + 1 inside the crate. The gain is mostly the single-sourced layout; say so in the commit.

## 2. `akuma-exceptions`: page copies and the crash dump

### 2a. Frame copies through `akuma_phys`
`crates/akuma-phys` has `Frame` (`from_pa` unsafe once, then `entry/read_bytes/write_bytes/copy_page_from` safe and bounds-checked) and an owned-page
type (see the `alloc`/`bytes_mut`/`publish` section of `lib.rs`). In `akuma-exceptions/src/lib.rs`:

| site (find with `grep -n "phys_to_virt" crates/akuma-exceptions/src/lib.rs`) | now | becomes |
|---|---|---|
| file-cache mismatch check (`DP_FILE_CACHE_MISMATCH`), `from_raw_parts(kva, 0x1000)` | raw slice over a cached frame | `Frame::read_bytes(0, &mut buf)` into a stack buffer (diagnostic-only path; the 4 KiB copy is fine, but check the stack budget of the fault handler first - if tight, compare in 8 x 512-byte chunks) |
| the two demand-fill sites, `from_raw_parts_mut(page_ptr.add(dst_off), len)` | file read straight into a fresh frame | the owned-page type's `bytes_mut()` (fresh, unpublished frame = exclusive owner), then `publish()` where the code maps it; if the frame is already registered in the ledger at that point, add a `Frame::write_bytes`-based path instead and say why |
| CoW break `copy_nonoverlapping(src, dst, 0x1000)` | raw | `Frame::copy_page_from` |
| trampoline install `copy_nonoverlapping(TRAMPOLINE, ptr, len)` | raw | `Frame::write_bytes` |

Each conversion keeps its behaviour byte-for-byte; the `from_pa` contract ("RAM no Rust object lives in") holds for PMM frames and user pages. A new
`Frame::from_pa` is itself an `unsafe` site, so convert only where the sites collapse (several uses of one frame -> one `from_pa`).

### 2b. EL1 crash dump -> `akuma-exceptions-forensics`
The EL1 fault dump reads whatever the crashed context pointed at: `saved_regs.read()` x6, `*(elr as *const u32)` (the faulting instruction),
`*(l0_base as *const u64)` / `*(l1_addr as *const u64)` (walks the page table at a physical address). These are debug output on a path that is already
dying and are deliberately unchecked-raw. Move that function (and only it) into `crates/akuma-exceptions-forensics` (the crate created in `ef8d68e4`),
behind the existing `forensics` feature with the same empty-stub pattern. That moves ~5 sites out of the handler crate; they become the forensics
crate's problem and its stated contract is "reads memory the crash named, may fault". Keep the call site's control flow (it must still print and
then halt); only the dumping moves.

### 2c. Leave alone (state why in the commit)
`msr tpidr_el1` (stack_top), `vbar_el1` install, `exception_vector_table` address, the `extern "C"` blocks (`el1_sync_gpr_clobber_mask` probe, vector
symbols), all `#[unsafe(no_mangle)]` entries, `FrameHandle::new` at the EL0 entry, and the two `sigframe::sync_frame_neon` derefs. The last
pair is the only maybe: if `sigframe` can return a `&mut`/`&` NEON view tied to `FrameHandle`'s lifetime, do it; otherwise leave.

## 3. The context table (the real design work)

### 3.1 What is there
`static THREAD_CONTEXTS: [SyncContext; MAX_THREADS]` with `unsafe impl Sync`; `get_context(idx) -> *const Context`, `get_context_mut(idx) -> *mut Context`.
The `Context` type is `akuma_exec_core::thread::Context` (aarch64: `magic, x19..x30, sp, daif, elr, spsr, ttbr0, user_entry, user_sp, user_tls,
is_user_process`, `#[repr(C)]`, **its field offsets are read by `global_asm!`**; x86_64: `magic, rsp`). The soundness argument is the `SyncContext` doc
comment (`BKL_PHASE7D_THREAD_CONTEXTS.md`): every access is one of

1. **Switch**: both slots touched only while `POOL` is held across decision + save + load.
2. **Spawn/reclaim setup**: the writer won the `FREE|TERMINATED -> INITIALIZING` CAS on `THREAD_STATES[idx]`, so it owns the slot until
   `mark_thread_ready` (a `SeqCst` store = release) publishes it.
3. **Self-read** of the live thread: cannot race itself.
4. (Accepted, undocumented as a case) **display-only reads** of arbitrary slots (`dump_thread_resume_points`, `list_kernel_threads`, the
   `ttbr0` scan, the test-only stamp slots), racing with a running owner.

There are 34 call sites (`grep -n "get_context(\|get_context_mut(\|get_context_ptrs" crates/akuma-threading/src/lib.rs`). Classify each before you
change any; the table below is a first pass from reading, **verify it**:

| case | sites (function names; line numbers drift) | proposed type |
|---|---|---|
| 1 switch | `ThreadPool::get_context_ptrs`; the SGI handler's save/load block; x86 `switch_to` path and its POISON read of `rsp` | `SwitchPair` (3.3) |
| 2 setup | `init_thread_slot_context`-style writers: spawn closure paths (x2 aarch64, x2 x86 `x86_build_closure_context`), the `Context::zero()` reset in `reclaim_terminated_slots`, idle adoption (`adopt_current_as_idle`-like, writes `magic/ttbr0/is_user_process`), `update_thread_context` (writes `elr/spsr/user_*`), `spawn_system_fn` simple path, the two `ctx.is_user_process = ..` writers | `Initializing` token (3.2) |
| 3 self | `get_saved_user_context` (`get_context(thread_id)` after `thread_id == current_thread_id()`), fork's read | `with_current_ctx` (3.4) |
| 4 racy | `dump_thread_resume_points`, `get_saved_kernel_resume`, the `ttbr0` scan (`(*get_context(i)).ttbr0`), `sps[i]` in the thread list | `peek` (3.5) |
| tests | the `stamp_slot` helpers and the `is_user_process`/`user_*` pokes in tests | keep `unsafe` in `#[cfg(test)]`; production budget excludes tests |

### 3.2 Ownership token for case 2
Today `claim_free_slot`/the reclaim CAS return a bare `usize` and the owner keeps using `idx`. Introduce a **linear token**:

```rust
struct Initializing { idx: usize }            // !Copy, !Clone; only the claim paths construct it
impl Initializing {
    fn idx(&self) -> usize;
    fn ctx(&mut self) -> &mut Context;        // safe: unsafe inside, justified by "I won the CAS and have not published"
    fn publish(self);                         // = mark_thread_ready(idx); consumes the token
    fn abandon(self);                         // the existing error paths that put the slot back to FREE
}
```

`ctx()` returning `&mut Context` borrowed from `&mut self` is what makes two live `&mut` to one slot a compile error. The `unsafe` inside is the CAS
argument, written once. Constructing the token is the only place that argument is claimed, so the two **constructors** are the audit surface:
`claim_free_slot` (FREE -> INITIALIZING) and the `TERMINATED -> INITIALIZING` branch of `reclaim_terminated_slots`. Both are `fn(...) -> Option<Initializing>`
and their `compare_exchange` stays exactly as is (`SeqCst`).

Hazards - the reason this is a design and not a mechanical move:
* Callers currently hold `idx: usize` across `POOL.lock()` sections, stack allocation, process-table callbacks, `x86_build_closure_context` and error
  exits. The token must be threaded through those; if a function needs `idx` for `THREAD_STATES[idx]`/`POOL.slots[idx]`, use `tok.idx()`.
  Do **not** add `impl Drop` that frees the slot: several paths publish via a different route or hand the slot to another core, and a silent Drop
  would double-free or race. Make `Initializing` `#[must_use]` and require an explicit `publish`/`abandon`; add a debug assertion in tests that no token is
  dropped unconsumed (a `Drop` that only `debug_assert!`s in test builds is fine).
* `update_thread_context` (execve/fork) runs on a **READY or RUNNING** slot, not an INITIALIZING one, under `with_irqs_disabled`. It is *not* case 2. Look at why
  it is sound today (it writes the slot of the *current* thread, or of a not-yet-published child) before deciding its type; it may need its own narrow
  function (`set_user_entry(idx_child_token, ..)` for the child; `with_current_ctx_mut` for self). If you cannot state the argument in two sentences, leave it
  `unsafe` and write the argument in its SAFETY comment. That is an acceptable outcome for 1-3 sites.
* x86: `x86_build_closure_context` returns a `Context` by value that the caller stores with `*get_context_mut(slot) = ..`; with the token this becomes
  `*tok.ctx() = ..` and the unsafe disappears at the caller.
* Publication ordering is the contract the host test
  `ready_transition_publishes_context_writes_without_a_lock` checks. After the change that test must go through the token API
  (`claim -> ctx() write -> publish`) and still pass **20 consecutive runs**; do not weaken it, do not replace `SeqCst` with anything else.

### 3.3 Case 1: the switch pair
`get_context_ptrs(&mut self, old, new) -> (*mut Context, *const Context)` is already `&mut ThreadPool` (the `POOL` guard), but is `unsafe fn` and the callers
then deref the raw pointers. Make it safe and return a newtype:

```rust
struct SwitchPair<'p> { old: *mut Context, new: *const Context, _pool: PhantomData<&'p mut ThreadPool> }
impl SwitchPair<'_> {
    fn save_sp_ttbr0(&mut self, sp: u64, ttbr0: u64);      // the SGI handler's "(*old_ctx).sp = current_sp" block
    fn load(&self) -> LoadedContext;                        // new.sp, new.ttbr0 by value
    fn into_raw_for_switch_asm(self) -> (*mut Context, *const Context);   // the one escape, consumed by x86_switch_context
}
```

The lifetime ties the pair to the borrowed `POOL` guard, so it cannot outlive the lock hold - this is the "POOL held across the *entire* switch" invariant
as a type. Keep every `with_irqs_disabled`/guard exactly where it is; this only wraps pointer derefs. The aarch64 SGI path hands `current_sp`/`new_sp` to asm
in registers (not through `Context`), so its asm interface does not change.

### 3.4 Case 3: self reads
```rust
fn with_current_ctx<R>(f: impl FnOnce(&Context) -> R) -> R   // asserts idx == current_thread_id(); irqs disabled around f
fn with_current_ctx_mut<R>(f: impl FnOnce(&mut Context) -> R) -> R
```
`get_saved_user_context` takes a `thread_id` and branches on `thread_id == current_thread_id()` (and otherwise returns the stale-triple diagnostic via
case 4). Keep that shape; the equality branch becomes `with_current_ctx`.

### 3.5 Case 4: racy reads, made defined
These are the accepted display-only races. A plain read of a `u64` another core may be writing is a data race (UB in Rust's model) even though the doc
calls it benign. Give them one door:

```rust
fn peek(idx: usize, field: ContextField) -> u64   // AtomicU64::from_ptr(addr_of!(ctx.field)).load(Relaxed)
```
`ContextField` is a small enum mapped to `offset_of!(Context, ..)` (aarch64: `Sp, Ttbr0, Elr, X30`; x86: `Rsp`). `AtomicU64::from_ptr` is the one
`unsafe` (alignment guaranteed by `repr(C)` + `u64`; the cell is a `static` so it is never dangling). Relaxed loads can be stale, which the doc already
accepts; they can no longer tear or be UB. This also lets the `ttbr0` scan and the `sps[]` loop lose their `unsafe`.

**Do not** change `Context` fields to `AtomicU64`: the switch asm indexes the struct by offset and derives `Copy`/`Debug`; same layout, but `Copy` would go, and
`Context::zero()` is `const` in `akuma-exec-core`. `peek` gets the benefit without touching the type.

### 3.6 Where it lives
Put all of 3.2-3.5 in a **module** of `akuma-threading` first (`src/ctx_table.rs`, `THREAD_CONTEXTS` private to it, `get_context*` deleted). The ownership
argument depends on `THREAD_STATES` (hundreds of uses) and `POOL`, so extracting a crate now would drag the state array with it. A later `akuma-context`
crate (generic `SlotTable<C, N>` owning states + contexts) is plausible but is **out of scope**: record it under "Not done" instead of starting it.
The module boundary is what makes the per-site `unsafe` count drop and what lets `scripts/check_unsafe_budget.py` show it (the budget counts the module's
sites in `akuma-threading`; the claim "~29 -> ~8" is a *cluster*, not a new crate).

Expected after 3: `get_context`/`get_context_mut` callers 29 -> the table module's ~6-8 (`Initializing::ctx`, `SwitchPair` x2, `with_current_ctx` x2, `peek`,
`stamp` in tests excluded). If it does not reach ~8, say which sites resisted and why in the doc.

## 4. Optional: switch asm placement (do only if 1-3 are green and merged)
`akuma-sched-amd64` already owns the amd64 machine half and says "asm lives here and nowhere else in the scheduler", but `akuma-threading` still carries the
x86 `x86_switch_context`, `x86_build_closure_context`, `thread_start`/`thread_start_closure` trampolines and the hooked-switch `global_asm!` (grep
`target_arch = "x86_64"` in `lib.rs`). Moving them there is a code move with `~9` sites changing crate, no gain in the total. The aarch64 counterpart
(`thread_start`, `thread_start_closure`, `setup_fake_irq_frame`, the `global_asm!` at the top) would be a new `akuma-sched-aarch64`. **Recommendation: skip
this in this pass.** It buys layering, not safety, and it is the part that cannot be tested on ryzen (amd64). If done, aarch64 boots on HVF here.

## 5. Rules for the implementer
* Branch `unsafe-context-table` from `post-release-improvements`; commit per item (1, 2a, 2b, 3.2, 3.3, 3.4/3.5), real messages, no `--amend`, no force-push, no push to `main`.
* Never change: `Context` field order/types, the `global_asm!` offsets, `SeqCst` on `THREAD_STATES` transitions, the `POOL` lock scope around a switch, `MAX_THREADS`.
* Every remaining `unsafe` keeps a `SAFETY:` comment stating the *argument*, not restating the operation. Every new `unsafe fn` has a `# Safety` section.
* No allocation on the paths touched (see CLAUDE.md "Kernel conventions"): tokens are `Copy`-sized, `peek` allocates nothing, the fault-path diagnostics must not grow the stack past what the handler had.
* `FrameHandle` is for the *current handler's* EL0 frame only; do not stretch it to other threads' frames (that was considered and rejected in `UNSAFE_KERNEL_CPU_STEP.md`).
* Run `python3 scripts/check_unsafe_budget.py --update` last, so the budget diff shows the real movement; put the before/after per component in the commit message.

## 6. Verification (all of it, in this order)
```
H=$(rustc -vV | sed -n 's/^host: //p')
cargo test -p akuma-threading --target $H            # 31 tests now; the 20x loop below for the publication test
for i in $(seq 20); do cargo test -p akuma-threading --target $H ready_transition -- --nocapture 2>&1 | grep -E "test result"; done
cargo test -p akuma-trap-aarch64 -p akuma-phys --target $H
cargo clippy -p akuma-threading -p akuma-trap-aarch64 -p akuma-exceptions -p akuma-exceptions-forensics -p akuma-phys -- -D warnings
cargo check                                          # aarch64 kernel
cargo check -p akuma-amd64 --target x86_64-unknown-none
cargo check -p akuma-tokio-kernel-amd64 --target x86_64-unknown-none
cargo check -p akuma-exceptions --no-default-features   # forensics off still builds
SMP=4 sh shanghai/mw_gate.sh all                     # boots aarch64 (HVF on a Mac) and amd64 (TCG); expect "MW GATE PASS", ^PASS 186, ^FAIL 0
SMP=4 sh amd64/run.sh                                # QEMU TCG boot suite; expect 0 [FAIL], 4 cpus online
```
On the ryzen box (Pop OS, no HVF) the aarch64 half of `mw_gate.sh` is TCG and slow; the supervisor runs the gate on the Mac. Do the host tests, clippy and
the amd64 boot there, and say in the hand-back which gates you could not run. **Do not** claim the aarch64 SGI/IRQ-frame changes (items 1 and 3.3) verified
without a booted aarch64 kernel: they are exactly the code a unit test cannot exercise.

Hand-back: a short results doc appended as "Part 3" to `UNSAFE_KERNEL_CPU_STEP.md` (what moved, per-component before/after, what resisted and why, which gates ran).

## Background
Prior art in this tree: `with_current_uctx` on amd64 (a closure-scoped borrow of a per-slot cell; the caveat is the same - the switch asm reads the record outside any
Rust borrow), `akuma_phys::Frame`, `akuma_trap_aarch64::FrameHandle`, `akuma-dma`'s borrow word. Ownership audit: `BKL_PHASE7D_THREAD_CONTEXTS.md`.
