# Error-handling audit: `let _ = ` and the unsafe surface in `src/` and `crates/`

**Date:** 2026-08-30 (redo; supersedes the 2026-08-16 pass, whose numbers are
kept below for the delta)
**Scope:** the kernel bin crate (`src/`) and the extracted crates (`crates/`).
Userspace (`userspace/`) is out of scope.
**Questions:** (1) how many `Result`/return-value discards are hiding a real
error, and (2) is there any mechanical way to shrink the `unsafe` surface.

Nothing here has been applied. It is a work list.

---

## 1. Method

```bash
grep -rn "let _ = " src crates --include="*.rs"
```

Dedicated test files (`process_tests.rs`, `tests.rs`, `fs_tests.rs`,
`pthread_tests.rs`, `daif_tests.rs`, `rump_tests.rs`, `kernel_tests.rs`) are
split out — a discarded `Result` in test teardown is a different risk class from
the same discard in a syscall handler. Production hits were then grouped by
**what is being discarded**, because the callee decides the risk far more than
the call site does.

The `unsafe` half was scanned separately (§5), and deliberately started by asking
what the *toolchain* already proves, rather than by listing occurrences.

---

## 2. Headline numbers, and what moved

| | 2026-08-16 | 2026-08-30 | delta |
|---|---:|---:|---:|
| total `let _ = ` | 404 | **528** | +124 |
| in dedicated test files | 249 | **318** | +69 |
| in production files | 139 | **210** | +71 |

`src/process_tests.rs` alone went 212 → 272. That is the boot self-test suite
growing, and it is the *correct* place for this pattern: best-effort
`unregister_process` / `kill_process_with_signal` / fixture cleanup **after** a
test's assertions have run. The test outcome does not depend on cleanup
succeeding.

The production number is the one that matters, and it grew by 51%.

## 3. Production discards (210), by what is discarded

| discarded callee | n | class |
|---|---:|---|
| `writeln!` / `write!` / `write_fmt` / `write_str` | 59 | **D — cosmetic** |
| `write_user_val` / `copy_to_user` / `copy_to_user_with` | 25 | **B — silent swallow, user-visible** |
| `fs.read_file` / `fs.read_dir` / `fs::write_file` | 11 | C |
| `unregister_process` / `kill_process` | 12 | A |
| `aspace.update_page_flags` / `unmap_page` / `update_page_flags_no_flush` | 5 | **B** |
| `spawn_system_thread_fn` | 3 | **B** |
| socket ops (`listen`, `udp_socket_bind`, `routes_mut`) | 8 | C |
| everything else (MMIO reads, atomics, `with_client`, …) | ~87 | A/D |

**The `writeln!` majority is a non-issue and should not be counted as debt.**
Every one of those writes into an `alloc::string::String` or a `StackWriter`,
whose `fmt::Write` impl cannot fail. `let _ =` there is the correct way to
discard an `Err` that is statically unreachable; a lint that bans `let _ =`
wholesale would fire 59 times on code with no defect.

## 4. The set worth acting on

### 4.1 `wait4` / `waitid` losing an exit status — the sharpest instance

`src/syscall/proc.rs` (five sites: 998, 1018, 1052, 1075, 1229):

```rust
if status_ptr != 0 {
    let status = encode_wait_status(code);
    let _ = write_user_val(status_ptr, &status);
}
// Reap the zombie: remove from process table + child channels.
akuma_exec::process::clear_lazy_regions(p);
```

The write is discarded and the reap happens **unconditionally, on the next
line**. If `status_ptr` is unmapped, the zombie is destroyed, the status is gone
forever, and `wait4` returns the pid as though it had reported it. The caller
cannot retry — there is nothing left to wait on.

Linux returns `EFAULT` here. The difference matters precisely because reaping is
**irreversible**: this is not "an error was ignored", it is "state was destroyed
while the report of it failed". That is the one shape in this whole audit where
the discard converts a recoverable error into an unrecoverable one.

Everything else in the 25-site `write_user_val` class is milder — `accept`
failing to write `addr_ptr` still leaves the fd, `recvmsg` failing to write
`msg_name` still delivers the data — but this one is a genuine defect.

### 4.2 Discarded page-table results — **the signature was the defect**

**Corrected 2026-08-30, after fixing it.** This section originally read "5
discarded `Result`s ... worth an explicit handler even if it currently cannot
fail". Reading the implementation instead of the call sites shows the discards
were harmless and the API was not:

```rust
fn update_page_flags_inner(&self, va: usize, new_flags: u64, flush: bool) {
    let Some(pte) = self.l3_slot(va) else { return };
    if old_entry & flags::VALID == 0 { return; }
    ...
}
```

The inner returns `()`. Both public wrappers returned `Result<(), &'static str>`
and ended in a literal `Ok(())`; `unmap_page_no_flush` likewise returned `Ok(())`
on its unmapped path. **No implementation could ever produce `Err`.**

That is worse than a discarded error, because it is invisible. Three *test* call
sites had grown assertions on it — `update_page_flags(...).is_ok()` inside an
`assert!`, `.is_err()` as a failure branch, and an `update_ok` term feeding a
`pass` flag — and all three were vacuous. One test's `pass` was in effect a
weaker condition than it appeared to check.

**Fixed:** the four fns (`unmap_page`, `unmap_page_no_flush`,
`update_page_flags`, `update_page_flags_no_flush`) return `()`; the five
`let _ = ` disappear as a consequence rather than as a cleanup; and the three
vacuous assertions were replaced with `read_l3_page_entry` read-backs that prove
the PTE actually changed.

**The rule:** a `Result` no implementation can populate is not harmless
conservatism. It teaches every caller that checking is optional in this area,
which is the habit that makes a genuinely discarded error — §4.1 — look like
ordinary house style.

### 4.3 Discarded thread-spawn failures — **withdrawn**

**Corrected 2026-08-30.** This section claimed the three discarded
`spawn_system_thread_fn` results in `src/smp_shared.rs` were "the shared-kernel
SMP machinery" and that a core could come up without its migration worker. That
was inferred from the callee names (`migration_worker`, `smp_worker`,
`blocking_relax_waiter`) without reading their callers. The callers are
`spawn_migration_probe`, `spawn_worker_demo` and `spawn_blocking_relax_waiters`
— an M4 self-test probe and two demo spawners, two of which document themselves
as "Best-effort (slot-limited)". **The discards are correct.**

One real defect sat next to them: `spawn_worker_demo` printed

```rust
crate::safe_print!(64, "[SMP-shared] spawned {} demo workers\n", cores + 1);
```

— the number *attempted*, unconditionally, for a loop explicitly allowed to
fail. A partial spawn reported as a full one, in a line that
`cores_that_ran_workers()`'s boot self-test is read against. Fixed to count
successes.

**The method lesson:** classifying a discard by its callee's *name* is the
shortcut this document warns about elsewhere. The risk lives in the caller.

---

## 5. The `unsafe` surface — what the toolchain already proves

Asked for "easy pickings for removing unsafe". The honest answer is **there are
almost none, and the reason is a good one**: the mechanical wins have already
been taken by the toolchain, so anything left needs judgement, not a sweep.

| category | count | verdict |
|---|---:|---|
| `unsafe { }` blocks (production) | 507 | — |
| `unsafe fn` | 46 | mostly trait-fixed signatures |
| `unsafe impl` | 8 | 3 trait-fixed (`GlobalAlloc`, `RawRwLock`, virtio `Hal`), 5 `Send`/`Sync` on device singletons |
| `unsafe extern` | 13 | asm/linker symbols |
| real `static mut` declarations | **9** | 3 are `#[cfg(test)]`-adjacent; 6 are DMA buffers |

Three facts kill the obvious sweeps before they start:

1. **The workspace is edition 2024** (`Cargo.toml:79`). `unsafe_op_in_unsafe_fn`
   is deny-by-default, so every `unsafe fn` body *already* has explicit inner
   blocks — the "redundant `unsafe` inside `unsafe fn`" cleanup does not apply.
2. **`static_mut_refs` is a hard error in 2024.** Every surviving `static mut` is
   already accessed through raw pointers, not references. The soundness upgrade
   that lint exists to force has been done.
3. **Clippy runs `all` + `pedantic` + `nursery` at warn and the tree is
   warning-clean.** `unused_unsafe` is on by default, so the count of genuinely
   redundant `unsafe` blocks in this tree is **zero**. Verified, not assumed.

Of the 46 `unsafe fn`, the great majority cannot become safe because the
signature is not ours: `GlobalAlloc::{alloc,dealloc,realloc}`, virtio-drivers'
`Hal::{dma_dealloc,share,unshare,mmio_phys_to_virt}`, `RawRwLock::unlock_*`, the
four `RawWakerVTable` fns. The rest (`map_user_page`, `demote_range_to_ro`,
`copy_to_user_safe`, `enter_user_mode`, `with_process_exclusive`) carry real
caller-side preconditions and should stay `unsafe`.

### 5.1 The one finding worth acting on

`crates/akuma-net/src/virtio_rings.rs` and `smoltcp_net.rs` hand out
**`&'static mut [u8]` into a `static mut` buffer**:

```rust
pub unsafe fn rx_frame(slot: usize) -> &'static mut [u8]
pub unsafe fn tx_frame(slot: usize) -> &'static mut [u8]
pub unsafe fn tx_discard()          -> &'static mut [u8]
unsafe fn     rx_buffer()           -> &'static mut [u8]   // smoltcp_net.rs:463
```

Two calls with the same `slot` produce two `&mut` aliases to the same memory —
instant UB by the language's rules, regardless of whether the NIC currently races
them. This is not a lint failure; `unsafe fn` makes it the caller's problem, and
no caller can discharge it.

**The fix is already in the same files.** `rx_buf(slot) -> *mut u8` and
`tx_buf(slot) -> *mut u8` (virtio_rings.rs:137, 147) do the same job returning a
raw pointer, which has no aliasing guarantee to violate. Narrowing the four
`&'static mut` accessors to the raw-pointer form — or to a slot-token type that
can only be held once — removes an entire UB class from the RX/TX path for a
mechanical change at a handful of call sites.

That is the whole list. It is short because the tree is in better shape here than
the `let _` side, and padding it with `unsafe impl Send` entries that are all
correct would misrepresent where the risk is.

---

## 6. Recommendation

1. **Fix §4.1.** DONE 2026-08-30 — all five `wait4`/`waitid` sites report before
   reaping and return `EFAULT` if the report fails. This was the only discard in
   the audit that turned a recoverable error into an unrecoverable one.
2. **Do not blanket-ban `let _ = `.** 59 of 210 production hits are infallible
   `fmt::Write`, and a ban trains people to write `.ok()` instead, which hides
   the same thing with fewer characters.
3. **Lint the callee, not the pattern.** `#[must_use]` on `write_user_val` and
   `copy_to_user` puts the pressure on exactly the sites that carry risk and
   leaves the other 180 alone. Not on `update_page_flags` or
   `spawn_system_thread_fn`: the first no longer returns a `Result` (§4.2), and
   the second's callers are best-effort by design (§4.3).
4. **Take §5.1.** DONE 2026-08-30 — the four accessors return `*mut [u8]`, so a
   caller mints a reference whose lifetime ends with the borrow instead of with
   the program.

**Two of the four items in this list were wrong when written** (§4.2, §4.3), and
both failed the same way: a call site was classified without reading the callee
or the caller. That is worth more than either finding.

---

## Background

- The 2026-08-16 pass this supersedes: 404 hits, 139 production, classified A–D.
  Its A/B/C/D scheme is kept; its per-crate table is dropped in favour of the
  by-callee grouping in §3, which is what actually predicts risk.
- [`GRANT_RECORDS_VS_DENY_RECORDS.md`](GRANT_RECORDS_VS_DENY_RECORDS.md) — why a
  silently-failed permission update is worse than it looks.
- [`SYSCALL_TRACE_AUDIT.md`](SYSCALL_TRACE_AUDIT.md) — the same
  "enumerate before you conclude" method applied to console prints, including how
  a scan that assumes one idiom reports a clean bill of health.
