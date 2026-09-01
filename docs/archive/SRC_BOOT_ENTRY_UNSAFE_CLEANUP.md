# The `src/` boot entry: 11 `unsafe` sites to 3, 1 real operation

**2026-09-01.** The run after
[`AKUMA_EXCEPTIONS_EXTRACTION.md`](AKUMA_EXCEPTIONS_EXTRACTION.md), which took
`src/` production `unsafe` from 91 sites to 11 and left the boot entry point as
the whole of what remained.

| | before | after |
|---|---:|---:|
| `src/` production `unsafe` sites | 11 | **3** |
| ...of which are real operations | 7 | **1** |
| `crates/` production `unsafe` sites | 411 | 413 |
| tree production `unsafe` sites | 422 | **415** |
| `src/` **test** `unsafe` sites | 77 | **70** |
| crates that forbid | 23 of 39 | 23 of 39 |

Seven production sites are **gone**, not relocated, and no crate was added.
That is the point of this run: six of the seven real operations turned out not
to be operations at all — they were obligations a caller had been asked to
promise, which either the callee could check for itself or the type system could
carry. The seventh, §5, turned out not to *work*.

## What `src/` held

```
src/main.rs      10 sites (7 real: semihosting asm, extern-static read,
                            2x rebuild_boot_device_table, ensure_boot_identity_covers,
                            akuma_fdt::locate, Waker::from_raw)
src/boot.rs       1 site  (unsafe extern block, linker symbol)
src/smp_shared.rs 2 sites (unsafe extern block + #[unsafe(no_mangle)])
```

The three "sites" that are `unsafe extern "C"` declaration blocks and
`#[unsafe(no_mangle)]` attributes are counted by the census and by the
`unsafe_code` lint, but they are not operations — nothing is dereferenced,
nothing is executed. They are why `src/boot.rs` and `src/smp_shared.rs` can never
carry `#![forbid(unsafe_code)]` even at zero real `unsafe`, and the same is now
true of `src/main.rs`.

## 1. `rebuild_boot_device_table`: an `unsafe` that only enforced agreement

`boot.rs` fills the boot table's device L3 in pre-MMU assembly from compile-time
literals — enough to reach the console, and no more, because Firecracker's GIC
redistributor base moves with the configured vCPU count and cannot be a literal.
Once the FDT is parsed the real addresses are known and the boot table has to be
corrected before any GIC or virtio MMIO access.

The correcting function took the L3's physical address:

```rust
pub unsafe fn rebuild_boot_device_table(boot_dev_l3_phys: usize)
```

> **# Safety**
> `boot_dev_l3_phys` must be the physical address of the L3 table `boot.rs`
> installed under L0[1] […]

and the caller obtained it from `src/boot.rs`:

```rust
pub fn boot_device_l3_phys() -> usize {
    unsafe extern "C" { static boot_page_tables: u8; }
    (&raw const boot_page_tables) as usize + 5 * 4096  // "the device L3 is page 5"
}
```

Two independent descriptions of one table — the assembly's page numbering and
the callee's expectation — with an `unsafe` keyword standing in for "I promise
these still match". Nobody could discharge that obligation by reading the call
site; you had to read the boot assembly's page layout.

The function now finds the table instead, by walking the live boot TTBR0 for
`L0[1] -> L1[0] -> L2[0]`, which is *verbatim* the precondition `write_device_l3`
already stated:

```rust
pub fn rebuild_boot_device_table()          // safe, no argument
fn boot_device_l3_phys() -> Option<usize>   // private, walks the boot table
```

There is no longer a wrong table to pass. `boot::boot_device_l3_phys()` and its
`unsafe extern` block were deleted, so this alone removed three sites (two in
`main.rs`, one in `boot.rs`) and one whole class of caller error.

The walk descends only through entries whose low bits are `VALID | TABLE`, and
returns `None` if any level is missing — which is also what makes the function
compile and no-op on the host, where `get_boot_ttbr0()` is a `0` stub.

## 2. `# Safety` clauses that were runtime-checkable predicates

Both boot-table editors carried a variant of:

> Must run on the boot page table, single-threaded, before any other address
> space exists.

That window is not vague — it closes at `mmu::init`, which is what builds the
first real address space and precedes secondary bring-up. `akuma-mmu` already
tracks it in `MMU_INITIALIZED` for other reasons. So both functions check
`is_initialized()` themselves and refuse (with a `debug_assert!`) rather than
naming the condition in a doc comment that each caller re-copies as a `// SAFETY:`
line. Both are safe functions now.

All three call sites are at `src/main.rs` lines ~638/711/758, against
`mmu::init` at ~942 — the check is comfortably true today and will *fail loudly*
if a future reorder makes it false, which the comment could not.

This is the third time in the campaign a `# Safety` clause turned out to be a
predicate the callee could evaluate; see
[`SYSCALL_UNSAFE_CLEANUP.md`](SYSCALL_UNSAFE_CLEANUP.md), where three moved
operations gained real runtime checks on the way out.

## 3. `boot_x0_at_entry`: an extern static that did not have to be one

The boot assembly stores `x0` — the DTB pointer as firmware left it — at
`_boot + 4`, before anything can modify it, and Rust printed it back for
verification through an extern static:

```rust
unsafe extern "C" { static boot_x0_at_entry: u64; }
let x0_at_entry = unsafe { boot_x0_at_entry };
```

The store is a plain aligned 64-bit `str`, so the storage can just as well be a
Rust `AtomicU64` that a relaxed load reads with no `unsafe` at all. The `.quad`
in the assembly is gone; `src/boot.rs` owns the symbol:

```rust
#[unsafe(no_mangle)]
#[unsafe(link_section = ".data.boot")]
pub static BOOT_X0_AT_ENTRY: AtomicU64 = AtomicU64::new(0);
```

**The `link_section` is load-bearing.** A zero-initialised static goes to `.bss`
by default, and the `.bss` clear runs at the top of `_boot_code` — *after* the
store at `_boot + 4`. A `.bss` home would silently zero the value back out, and
the symptom would be a wrong debug print rather than a crash. Verify with `nm`:

```
$ nm target/aarch64-unknown-none/release/akuma | grep BOOT_X0
00000000401002d0 D BOOT_X0_AT_ENTRY      # 'D' = initialised data, not 'B'
$ objdump -h … | grep data.boot
 2 .data.boot  00000018  00000000401002c0  DATA   # it is inside this span
```

It lands in the third slot of `.data.boot`, beside `boot_ttbr0_addr` and
`boot_ttbr1_addr`, which is exactly where the assembly's `.quad` used to be.

## 4. `Waker::from_raw` -> `Waker::noop()`

The async main loop polls, drains and halts; it never parks on a waker. It was
building a `RawWakerVTable` of four empty closures to feed `Waker::from_raw`,
which is a hand-rolled reproduction of a safe `const` stdlib item added in Rust
1.85.

## 5. The semihosting exit: extracted, measured, then deleted

The last `asm!` in `src/main.rs` was `hlt #0xf000` with `SYS_EXIT_EXTENDED` —
ARM semihosting, which ends the QEMU process **with an exit code**:

```rust
fn halt_with_code(code: u32) -> ! {
    let block: [u64; 2] = [0x20026, u64::from(code)];   // [reason, exit_code]
    unsafe { asm!("hlt #0xf000", in("x0") 0x20u64, in("x1") block.as_ptr(),
                  options(nomem, nostack)); }
    loop { akuma_cpu::park::wfi(); }   // "if semihosting is not available"
}
```

It was extracted into a 27-line `akuma-qemu` crate on the `akuma-uart` /
`akuma-fdt` model — a crate whose only product is a discharged obligation — and
then **deleted the same day**, because measuring it showed the operation should
not exist at all. The extraction was the right reflex applied one question too
early.

### The measurement

A temporary `halt()` probe at the top of `rust_start`, run on both accelerators:

| mechanism | `HVF=1` (default on Apple silicon) | `HVF=0` (TCG) | carries an exit code |
|---|---|---|---|
| semihosting `hlt #0xf000` | **wedges the vCPU** | exits 42 | yes |
| PSCI `SYSTEM_OFF` (`akuma_psci::call`) | exits 0 | exits 0 | no |
| semihosting, then PSCI | **wedges** | exits 42 | — |

**Under HVF the `hlt` does not fall through.** This is the assumption the
original code was built on and it is false: the comment said "if semihosting is
not available, fall back to wfi loop", but the instruction never retires, so the
vCPU sits on it forever. Nothing after it runs — not the `wfi` loop written as
its safety net, and not the PSCI call in the third row above. A panic on a stock
`cargo run` did not stop the VM, it hung it, and had done so for as long as HVF
has been the default.

Ordering cannot rescue it. Semihosting-first hangs (row 3). PSCI-first makes
semihosting dead code, because PSCI never fails anywhere QEMU runs.

### Nothing was reading the exit code

The only thing semihosting can do that PSCI cannot is hand `$?` to the shell,
and that turned out to be unconsumed. Every harness under `scripts/` that judges
a run detects a panic by **grepping the log for `[PANIC]`**
(`forktest_smp_matrix.py`, `validate_fork_smp.py`, `test_memory_split.py`,
`verify_trim.py`). The `result.returncode == 0` conditions in
`forktest_smp_matrix.py` and `quick_forktest.py` are on *guest* binaries run
over ssh, not on QEMU. `sched_audit_matrix.py` prints QEMU's rc and does not
branch on it. And in practice the code was always `1`: `halt_with_code` had
exactly one caller, `halt()`.

### What `src/main.rs` has now

```rust
fn halt() -> ! {
    // Discarded deliberately: on success this does not return, so a status can
    // only mean "no PSCI conduit" — which the `wfi` below already handles.
    let _ = akuma_psci::call(akuma_psci::SYSTEM_OFF, 0, 0, 0);
    loop { akuma_cpu::park::wfi(); }
}
```

`halt_with_code` is gone rather than kept with an ignored argument — PSCI cannot
carry a code, and a silently discarded parameter is a lie in a signature. This
is deliberately not `akuma_boot::system_off()`, which makes the identical call:
`akuma-boot` sits behind the optional `sc-reboot` feature that `extreme-size`
builds without, and the panic path exists in every profile. The two also differ
in fall-through — that one spins, because its caller has already announced a
reboot and has nowhere else to go.

### The lesson, which is not about semihosting

"Which crate should own this `unsafe`?" is the *second* question. The first is
"does this operation still do anything?", and it costs one probe and two
accelerator runs to answer. Skipping it here would have added a 39th crate whose
sole function is to hang the default build, and dressed it up as safety work —
the extraction, the crate header, the census row and the `# Safety` prose would
all have been immaculate and all pointed at a defect.

Two smaller things surfaced on the way and are worth keeping:

- The asm said `options(nomem)`, which promises it reads no memory, while the
  semihosting host reads exactly the `[reason, exit_code]` block on the stack.
  Nothing but LLVM's goodwill kept those stores alive. If semihosting is ever
  reintroduced, it needs `options(readonly)` and both registers declared as
  clobbered outputs, since a trapped call need not preserve them.
- `akuma-psci`'s `call`/`hvc_call`/`smc_call` are **safe** `pub fn`s, despite its
  module header reading as though the API were unsafe. It is not: the obligation
  that crate discharges is the `asm!` keyword. What stays out of `akuma-cpu` is
  the *instruction* — `smc` with `CPU_ON` starts a core at a caller-supplied
  address — not the safety of the wrapper. The distinction matters when deciding
  where the next instruction goes, and it was misread once in this very run.

### If exit codes are ever wanted

Semihosting **under `HVF=0` only**, selected by the harness rather than by the
kernel. The kernel cannot distinguish the two accelerators without reading
`MIDR_EL1` and guessing, and a guess that is wrong hangs the VM.

## 6. The same defect, again, in the boot suite

`src/`'s test files held **77** `unsafe` sites. Seven came out, and six of those
seven were the *identical* mistake §4 found in `run_async_main` — hand-rolled
pinning and hand-rolled no-op wakers:

| site | was | now |
|---|---|---|
| `async_tests.rs` ×2 | `Pin::new_unchecked(Box::new(f))` + a 4-closure vtable | `Box::pin(f)`, `Waker::noop()` |
| `tests.rs` ×3 | `Pin::new_unchecked(&mut f)` | `core::pin::pin!(f)` |
| `tests.rs` ×1 | a no-op vtable, in a test *named* `test_block_on_noop_waker` | `Waker::noop()` |
| `akuma-exec` `process/types.rs` ×1 | `noop_waker()` helper over `Waker::from_raw` | `Waker::noop().clone()` |

The `block_on_noop_waker` one is worth singling out: the test exists to prove a
`block_on` loop works with a no-op waker, and it was building its own instead of
using the stdlib's. `Waker::noop()` is what the SSH `block_on` path actually uses,
so the test got *more* faithful, not less.

One `asm!` also left, and took an asymmetry with it:

| site | was | now |
|---|---|---|
| `tests.rs` ×1 | `asm!("msr fpcr, {}", …)` | `akuma_cpu::sysreg::set_fpcr` |

`akuma-cpu` already had the **reader** (`sysreg::fpcr()`, via `read_only!`), so a
bare `msr fpcr` in the boot suite was the only `asm!` in `src/tests.rs` and the
read-admitted / write-open-coded split was an accident rather than a decision.
`set_fpcr` is admitted on `set_tpidr_el0`'s reasoning with room to spare: FPCR
selects how the FPU *rounds*. It re-points nothing the kernel dereferences, names
no address, and a garbage value can only make arithmetic wrong — which is not a
memory-safety property. Contrast what stays `unsafe` at the call site
(`ttbr0_el1`, `vbar_el1`, `elr_el1`, `tpidr_el1`, `tpidrro_el0`): every one of
those re-points state the kernel then dereferences.

### The other 70 are the tests' subject matter

This is the part worth not re-litigating. The boot suite pokes page tables,
physical frames, PTE permission bits, the user-copy fault paths, `dc zva`, MMIO,
and self-modifying code. A test proving `copy_from_user_safe` returns `EFAULT`
has to hand it an unmapped address; the icache tests write two instructions and
`transmute` to a function pointer to run them; `dc zva` is on `akuma-cpu`'s
deliberately-excluded list. **The `unsafe` is the experiment.**

One left deliberately that *looks* removable: `test_waker_mechanism`'s counting
`RawWakerVTable`. `alloc::task::Wake` would remove the `unsafe`, but that test's
**subject** is the raw vtable — converting it would test a different mechanism.
(Its `CLONE_COUNT` is dead instrumentation, incremented and never asserted; the
`Wake` trait has no clone hook, which is part of why the swap is not equivalent.)

### And why a `crates/akuma-tests` split does not follow

The obvious next thought — move the unsafe tests to one crate and the rest to
another — was surveyed and rejected. **Six of the nine test files already hold
zero `unsafe`**, and they are 6% of the code:

```
process_tests.rs  20,394 lines   1,574 crate:: refs   23 unsafe
tests.rs           9,509           848                41
sync_tests.rs      2,525           127                 7
async/daif/fs/network/pthread/rump_tests
                   1,975           184                 0
```

So splitting on unsafe-ness puts 94% of the tests in the "unsafe" crate and
achieves nothing. Making it meaningful means splitting *within* `tests.rs` and
`process_tests.rs` — per-function triage of ~500 functions at one `unsafe` site
per 230 lines — which cuts across the subsystem grouping the suite's `run_test!`
ordering and its "Memory Tests: ALL PASSED" banners depend on, and separates a
safe CoW-fork test from the unsafe PTE-poking test of the same mechanism.
**Unsafe-ness is orthogonal to how these tests are organised, and it cuts that
organisation badly.**

The coupling makes it moot anyway: 2,606 `crate::` references, **187 distinct
symbols across 15 clusters** (of which **33 are types**, which a hooks struct
cannot carry), against the 8 clusters `ExceptionHooks` had to absorb. And it
would buy nothing for the ban — `src/` can never `forbid` while `global_asm!` and
`#[unsafe(no_mangle)]` are in `main.rs`/`boot.rs`/`smp_shared.rs`, tests or no
tests. The prerequisite is `src/syscall/` leaving first
([`SRC_SYSCALL_EXTRACTION_SURVEY.md`](SRC_SYSCALL_EXTRACTION_SURVEY.md)), and if
the goal is only navigability, `src/tests/{memory,threading,futex,…}.rs` as
plain modules gets that today for free.

## What is left in `src/`

```
src/main.rs:648   unsafe extern "C" { … }        linker symbols, read via safe &raw const
src/main.rs:733   unsafe { akuma_fdt::locate(dtb_ptr) }
src/smp_shared.rs:178  unsafe extern "C" { … }
```

plus `#[unsafe(no_mangle)]` / `#[unsafe(link_section)]` attributes in
`src/boot.rs`, `src/main.rs` and `src/smp_shared.rs`.

**`akuma_fdt::locate` is the only real operation, and it is irreducible.** It
dereferences a pointer handed over by firmware, which is precisely why
`akuma-fdt` exists as a crate: three `unsafe` sites so that `main.rs`,
`platform.rs` and `smp_shared.rs` need none. Making it safe would require
proving the pointer is mapped, which no crate below the MMU can do.

So `#![forbid(unsafe_code)]` across `src/` is now blocked on the lint's treatment
of `global_asm!` and `#[unsafe(no_mangle)]` — the boot and secondary trampolines
need both — rather than on any soundness question. That is the same wall
`AKUMA_SMP_SHARED_SPLIT.md` describes for `src/smp_shared.rs` at zero `unsafe`
operations.

## Verification

- `cargo build --release`, `scripts/build_extreme_size.sh` (711 K, unchanged
  against the 4 MB floor), `cargo clippy --release` clean.
- Host tests: 80 suites, all green.
- Release boot, **`MEMORY=2048M`**: **265 pass markers (165 `Result: PASS` + 100
  `[PASS]`), zero failures**, both "ALL PASSED" banners, run to the end of the
  suite and on into normal operation.

  **Run the suite at 2048M or it does not finish, and the truncation is
  silent.** At the default `MEMORY=256M` under HVF, QEMU aborts itself about
  two-thirds of the way in — `Assertion failed: (isv), function
  hvf_handle_exception, hvf.c:2437`, exit 134 — the documented HVF ISV=0
  writeback-form bug ([`QEMU_HVF_ISV_BUG.md`](QEMU_HVF_ISV_BUG.md)), which
  `scripts/cargo_runner.sh` warns about in the log. That truncated run reports a
  perfectly stable **119 `Result: PASS`**, matching a stashed-HEAD baseline
  exactly, which is what makes it dangerous: it reads as a green suite and is a
  suite that stopped early. Every waker, `Pin` and FPCR test in `src/tests.rs`
  lives *after* the cut, so a 119-vs-119 comparison proves nothing about them.
  This was mis-measured that way once during this work; the number to quote is
  the 2048M one.
- The boot log confirms all four changed early-boot paths:
  `x0 at _boot entry: 0x48000000` (matches the DTB argument, so the `.data.boot`
  static survived the `.bss` clear), `[Platform] qemu-virt device map installed`
  (bootstrap rebuild found the L3 by walk), `[DTB] found at 0x48000000` and
  `[Platform] FDT device map: GICR=0x80a0000` (post-FDT rebuild did too).
- Halt path: a temporary `halt()` probe exits **0 under both `HVF=1` and
  `HVF=0`**. Before this run, the same probe hung under `HVF=1` — that is the
  §5 fix, and it is the one behaviour change in this run that a user can see.

## 7. The census tool grew tests, and one of them found a bug

`scripts/cloc_akuma.py` is what regenerates
[`crate-safety.md`](../reference/crate-safety.md), so a miscount lands in the
docs as authoritative. Its `--self-test` gained **13 lexer cases and 4 structural
invariants**.

It was challenged as "wildly inaccurate" and is not: an independently written
lexer agrees with it at exactly **423** sites across `crates/`, and its
`test`/`production` split is right — the 11 test sites are all in inline
`#[cfg(test)] mod` blocks (6 of them in `akuma-exec/threading`), because no
`*_tests.rs` file under `crates/` contains any `unsafe` at all. The number *looks*
wrong next to `grep -c unsafe` (593 lines) for two reasons worth knowing: ~170 of
those lines are comments — this codebase writes *about* `unsafe` constantly — and
10 are `#[unsafe(…)]` attributes, which the script excludes on purpose because
they mark a declaration rather than an operation.

**The new lexer cases found a real bug.** `r#unsafe` — the raw-identifier escape,
the one spelling of those six letters that is *not* the keyword — was counted as
a site: the scanner saw `r`, then a bare `#`, then `unsafe`. Fixed. Nothing in the
tree writes it today; it is fixed because the failure is silent and inflates the
number. The other cases pin the constructs that desynchronise a naive lexer and
leave it "inside a string" for the rest of a file: `'"'` in a char literal,
lifetimes (`&'a u8`) versus char literals, escaped quotes, byte strings, hashed
raw strings, nested block comments, and `cfg(all(test, …))` / `cfg(not(test))`
gating.

The four invariants are each pinned to a failure this script has actually had:

| invariant | the failure it pins |
|---|---|
| every crate under `crates/` appears in the report | `akuma-gic` and `akuma-psci` were once *missing entirely*; an absent crate reads as "nothing to report", not as an error, and new crates land here every few days |
| `forbids_unsafe` and `unsafe_sites` agree both ways | the two are derived independently (regex over the file head vs. the lexer) and the headline "N of M crates forbid" rests on both |
| whole-file tests charge nothing to production | test `unsafe` counted as shipped `unsafe` is the number `src/` is judged on |
| sites ≤ substring occurrences | the cheap direction-of-error check; would have caught the overlapping-roots doubling without knowing anything about roots |

That last one **failed on first write, and the check was wrong, not the script**:
`akuma-alloc` opens two `GlobalAlloc` methods as
`unsafe fn talc_alloc(..) -> *mut u8 { unsafe {` — two keywords, one line. The
bound is occurrences, not lines containing one. Recorded in the comment so nobody
re-derives it.

## Background

- [`AKUMA_EXCEPTIONS_EXTRACTION.md`](AKUMA_EXCEPTIONS_EXTRACTION.md) — the run
  before this one, 91 -> 11.
- [`AKUMA_SMP_SHARED_SPLIT.md`](AKUMA_SMP_SHARED_SPLIT.md) — `akuma-psci`, which
  §5 leaves as the kernel's only way to stop a machine.
- [`SYSCALL_UNSAFE_CLEANUP.md`](SYSCALL_UNSAFE_CLEANUP.md) — `# Safety` clauses
  as runtime checks, first outing.
- [`INLINE_ASM_CLEANUP.md`](INLINE_ASM_CLEANUP.md) — `akuma-cpu`, and what
  deliberately stays out of it.
- [`SRC_SYSCALL_EXTRACTION_SURVEY.md`](SRC_SYSCALL_EXTRACTION_SURVEY.md) — where
  `tprint!` went and why, and what still blocks `src/syscall/` from leaving.
- [`../reference/crate-safety.md`](../reference/crate-safety.md) — the running
  census this run regenerated.
- [`../reference/subsystems/console.md`](../reference/subsystems/console.md) —
  the printing rules; `tprint!` now lives in `akuma-primitives`.
