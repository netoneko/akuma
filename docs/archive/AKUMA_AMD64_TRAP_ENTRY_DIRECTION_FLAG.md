# Akuma/amd64: the hand-written trap entries never cleared `DF` — every kernel `memcpy`/`memset` after a fault mid-`memmove` ran backwards

**Status: ROOT-CAUSED and FIXED, 2026-09-23.** Fix in `amd64/src/idt.rs`
(`fixable_exception_entry!`, `timer_entry`, `debug_entry`,
`invalid_opcode_entry`) and `amd64/src/shootdown.rs` (`shootdown_entry`): one
`cld` per stub, plus a tripwire (`trap_entry_df_seen`) and a boot-suite check
that fails without it (verified A/B, below). Deployed to the bare-metal box the
same night. This closes the mechanism behind two photographed console deaths;
whether it is the *only* corruption source on the metal is not yet known.

## The second capture (2026-09-23 02:54, `IMG_6220.HEIC`)

The box died on the TV with a ring-0 `#PF`, three cores spinning on the BKL
the dead core still held:

```
[bkls>] core=4 ticket=413436 serving=413433 owner=3 spins=1048576
[EXCEPTION] #PF page fault err=0x0000000000000000
  rip=0xffffffff803b1849  rsp=0xffff800020bab080
  cs=0x8 rflags=0x0000000000010402
  cr2=0x00000000629c3080 cr3=0x000000006099e000 freed=0
  core=2 task_slot=11 slot_root=0x000000006099e000
  [rsp/physmap] ... ffffffff8031f439 ffffffff8031f400 ffffffff8042d806 ...
                ... ffffffff803b097f ... ffffffff803b4db5 ... ffffffff803b28e5 ...
```

Decoded against the 2026-09-23 00:25 build (`llvm-nm` + nearest-preceding
symbol, then `llvm-objdump` at the addresses):

| word | resolves to | meaning |
|---|---|---|
| `rip` `…803b1849` | `btree::remove_leaf_kv<usize,u16>` +0xb9, `mov 0x78(%rcx,%rsi,8),%rdi` | reading `parent->edges[idx-1]` of a leaf node |
| `…803b4db5` | `BTreeMap<usize,u16>::remove` +0xb5 | caller |
| `…803b28e5` | `akuma_pmm::cow_ref_dec` +0xe5 | the return address right after `call BTreeMap::remove` |
| `…8042d806` | `x86_yield_now` +0x176 | stale, from an earlier switch on this stack |
| `…8031f4xx` | `net::uptime_us` | stale |
| `cr2` `0x629c3080` | `rcx + 0x78 + 8·1` | so the leaf's **`parent` pointer was `0x629c3000`** |

`0x629c3000` is not a kernel VA at all (the heap is in the physmap at
`0xffff8000_…`). It is the shape of a **physical frame address** — the *keys*
of that very map, `COW_REFCOUNTS: BTreeMap<frame_pa, u16>`, and `0x629c0000`,
`0x62746000`, `0x6246a000` sit on the same stack as locals. A node's `parent`
field had been overwritten with a neighbouring frame number. Heap corruption,
inside the CoW refcount table, reached from the page-fault handler's CoW break.

**The tell is in the saved flags.** `rflags=0x10402` = `RF | DF | 1`. `IF=0`
is right (`cow_ref_dec` does `cli`), `RF` is set by delivery — but **`DF=1`
inside Rust code** that never executes `std` means the whole excursion had been
running with the direction flag set since it entered the kernel.

## Why that is fatal here

1. **Delivery does not clear `DF`.** An exception or interrupt gate clears
   `TF`, `RF`, `NT`, `VM` and (interrupt gate) `IF`. `DF` survives. The SysV ABI
   only promises `DF=0` at a *call boundary*, which a trap is not. Linux's entry
   code has an explicit `cld` for exactly this reason; so does rustc — **every
   `extern "x86-interrupt"` handler in this binary begins with `cld`** (25 sites,
   confirmed by disassembly). The five `global_asm!` entries did not, and
   `syscall_entry` was safe only because `IA32_FMASK` includes `DF`.

2. **Userspace sets `DF` all the time.** musl's x86_64 `memmove`, verbatim from
   `bootstrap/bin/nettest-std.x86_64`:
   ```
   lea -0x1(%rdi,%rdx),%rdi
   lea -0x1(%rsi,%rdx),%rsi
   std
   rep movsb
   cld
   ```
   A page fault on **any byte** of an overlapping-forward copy — into a CoW
   page after `fork`, into a lazily-mapped page, into a fresh `brk`/`mmap` page —
   enters `page_fault_entry` with `DF=1`. `Vec::insert`/`remove`, `String`
   edits, every allocator that slides a buffer: rustc, cargo, tokio and kot do
   this constantly. The kernel's own `memmove` (compiler_builtins) has the same
   `std; rep movsq; cld` window, so a timer or shootdown IPI landing inside a
   kernel `memmove` reproduced it from ring 0 too.

3. **Every kernel `memcpy`/`memset`/`memmove` is a `rep` string op**
   (compiler_builtins' x86_64 `mem` impls: `rep movsb`/`movsq`/`stosb`/`stosq`).
   With `DF=1`, `rep stosb` at `dst` writes `dst, dst-1, …, dst-(n-1)`. So in
   `page_fault_dispatch`:
   - demand paging's `write_bytes(phys_ptr(frame), 0, 4096)` **zeroes the 4 KiB
     below the fresh frame**, i.e. the *previous physical page*, whatever it is —
     a heap page holding BTree nodes, a page table, a kernel stack — and leaves
     the fresh page unzeroed;
   - the CoW break's `copy_nonoverlapping(pa, fresh, 4096)` writes the *source's
     preceding page's bytes* backwards over the page below `fresh`.
   Then `cow_ref_dec` runs — still with `DF=1` — and walks the refcount tree.
   That is this dump, exactly: the corruption of one random physical page per
   affected fault, followed sooner or later by a `#PF`/`#GP` in whoever owned
   that page. (`akuma-user-access`'s own `rep movsb` loop is unaffected: it
   `cld`s itself — `__arch_copy_user_region_start+0x3`.)

## Why this is the same bug as the 2026-09-22 `#GP` in `sys_clock_gettime`

[`AKUMA_AMD64_CLOCK_GETTIME_SWITCH_FRAME_GP.md`](AKUMA_AMD64_CLOCK_GETTIME_SWITCH_FRAME_GP.md)
reconstructed a `[SWITCH BADFRAME]` on the console pump daemon's slot: a saved
switch frame near the **top** of a `vec![0u8; 32K].leak()` kernel stack, with
`rflags=0x1` (impossible from `pushfq`) and a return address mid-instruction,
while the stack's own canary at its **base** was intact. Its Theory C drew the
geometry — "a neighbour scribbles the victim's *top* end while the victim's
base canary stays clean" — and could not name a writer that grows downward
across an allocation boundary. A backward `rep stos`/`movs` from the *base* of
the next-higher allocation is that writer: it corrupts the words just below
its start, which are the previous allocation's top — where a shallow thread's
saved frame lives. Theories A (two cores, one stack) and B (stale `Context`)
are not needed for that capture, though the "deeper fragility" the 2026-09-12
doc names is unchanged and still deserves its ring-3 reconcile.

Both photos: one core holds the BKL, dies in `idt::fatal()`, and the other
three print `[bkls>]` forever. That is the symptom of *any* fatal fault under
the lock, not a lock bug.

## Fix

`cld` as the first instruction after the register pushes in every hand-written
entry — six stubs, since `fixable_exception_entry!` instantiates twice:

| stub | file |
|---|---|
| `page_fault_entry`, `general_protection_entry` | `amd64/src/idt.rs` (`fixable_exception_entry!`) |
| `timer_entry`, `debug_entry`, `invalid_opcode_entry` | `amd64/src/idt.rs` |
| `shootdown_entry` | `amd64/src/shootdown.rs` |

Kernel `cld` count went 25 → 32 (six stubs + one in the new test). Nothing
else changed in the stubs; `iretq` restores the interrupted code's own `DF`.

**Tripwire.** Each of the six dispatchers now calls `note_trap_entry_flags()`
first: `pushfq; pop` and count if bit 10 is set (`TRAP_ENTRY_DF_SEEN`,
read by `trap_entry_df_seen()`). Two instructions and a never-taken branch per
entry; it exists so that the *next* hand-written stub that forgets its `cld`
is caught by the suite rather than by a television.

**Boot-suite check** (`idt::user_copy_smoke_test`, case 7): with interrupts
masked, one asm block does `std; mov al,[lazy_va]; cld` against an armed lazy
page, so the `#PF` is delivered with `DF=1` and cleared again before any Rust
runs. Asserts: the fault was demand-paged; the fresh page reads zero end to end;
`trap_entry_df_seen()` did not move.

## Evidence

QEMU `-M microvm -cpu max`, `SMP=4`, patched kernel:

```
  trap entry: a fault taken with DF set was demand-paged   [OK]
  trap entry: the page it zeroed is zero end to end (the zeroing ran forwards)   [OK]
  trap entry: no dispatcher ran with the direction flag set   [OK]
Akuma/amd64 self-test: 792 passed, 0 failed
```

Same tree with the `fixable_exception_entry!` `cld` removed (nothing else),
`SMP=1`:

```
  trap entry: no dispatcher ran with the direction flag set   [FAIL] got 0x1 want 0x0
[EXCEPTION] #PF page fault err=0x0000000000000000
```

— the check fails, and the *unpatched* kernel then dies of a ring-0 `#PF`
later in the same boot, which is the bug demonstrating itself: that one
backward zero of a random frame under the test's fresh page was enough. Note
the "zero end to end" check passed even there (the fresh frame happened to be
clean already); the tripwire is the discriminating assertion.

**Deployed** to the box (192.168.1.123) 2026-09-23 ~03:30 via
`busybox wget` from the Mac + `scripts/install_kernel_amd64.sh` (md5
`e8d91be3…` verified on both sides); it came back on `e6770987-release-smp-shared`
with `.prev` = the 2026-09-22 kernel. Not yet promoted to `.good`.

## What this does and does not close

- **Closes:** the mechanism for random single-page kernel corruption on
  amd64 — heap nodes, switch frames, page tables — triggered by ordinary
  userspace `memmove` under memory pressure. It is amd64-only (AArch64 has no
  direction flag) and was invisible in every host test (the copies are correct
  when `DF=0`).
- **Does not close:** the 2026-09-12 "two cores, one stack" fragility and its
  missing ring-3 reconcile (`AKUMA_AMD64_BKL_NETWORKING.md` item 3); the kot
  `writev` spin on the metal (`AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md`). The
  box also went unreachable ~30 s after `herd disable kot` on the **old**
  kernel, before this fix was installed; that event is unexplained and should
  be re-tried on the new kernel before being filed anywhere.

## Found while checking process/socket cleanup on the new kernel (2026-09-23) — orphans were unreapable; FIXED

Probed over ssh on the box after the deploy, with `busybox nc -l` as the
victim. Sockets themselves clean up: a listener's row leaves `/proc/net/tcp`
when its process dies, a client blocked on it reads EOF, and re-binding the
same port works every time. What did not clean up was the **process**: every
killed or orphaned process stayed in `ps` — 11 of them in state `Z`, `PPid: 1`,
after one evening — each holding a task slot, and each stale row one more
`[TRAMP-MISMATCH] tid=N THREAD_PID_MAP=… but table scan found <zombie pid>`
line on the console.

**A false lead first, recorded because it cost two hours:** `nc` with a live
client read `R` in `/proc/<pid>/stat` for seconds after `SIGTERM`, which looked
like "the socket read ignores signals". It does not. The kernel log on the box
had `[signal] pid=251 killed by signal 15 (default action)` for every one of
them, and the strace-enabled QEMU guest showed the parked `accept` returning
`EINTR` and the default action running. Under the BKL load kot generates the
transition to `Z` simply lags a few seconds. (Two probe traps on the way:
`[ -d /proc/<pid> ]` is true for **any** pid on this `/proc`, and
`pkill -f`/`ps | grep` match the ssh session's own `sh -c` line and kill it.)

**The real defect — two views of parenthood.** On exit, amd64 reparents the
dying process's children to pid 1 by rewriting `Process.parent_pid` in the
table (`usermode.rs`). But `wait4(-1)`, `has_children`, `is_child_of_group`,
`find_exited_child` and `raise_sigchld_for_parent` all read
`akuma_exec::process::children`'s **child-channel registry**, which kept the
dead parent. So init's `wait4(-1, WNOHANG)` asked `has_children(1)`, got
"none", answered `ECHILD`, and no orphan on this target was ever reapable —
not by herd, not by anyone. herd never tried anyway: it reaps only its own
service pids by `waitpid_status(pid)`.

**Fix, both halves:**

- Kernel (shared): `akuma_exec::process::reparent_children_to(dying, new_parent)`
  moves the registry entries *and* the table rows. Called from amd64's exit
  path (`spawn_record_exit`), from glue's `sys_exit_group` for every member of
  the dying thread group (the AArch64 route — see below), and from the two
  kill paths in `akuma-exec`'s `signal.rs`. Host test
  `reparent_moves_registry_entries_so_the_new_parent_can_wait` (an
  already-exited child moves too and is what init's next `wait4(-1)` finds).
- **AArch64 had the same hole, later the same day.** It had no reparenting
  site at all. Two things about where the call had to go: (1) `return_to_kernel`
  is the wrong place — on the `exit_group` route `current_process_shared()` is
  already gone when it runs, so a hook there never fired (three boot cycles to
  learn that); the call sits in glue's `sys_exit_group`, right after the
  child-channel notify. (2) `return_to_kernel`'s *fall-off/fault* routes keep
  their existing policy of **killing** forked children (`[ORPHAN-KILL]`,
  `kill_child_processes*`) — untouched, so a segfaulting parent's children still
  die as before; only the normal `exit_group` route now reparents instead of
  abandoning. Boot test `test_orphan_reparented_to_init`
  (`src/process_tests.rs`): `sh -c 'sleep 3 & sleep 1; exit 0'`, then the
  sleeper must be a registry child of 1 and reapable once it exits. It reads
  the registry through the new `children_of(parent)`, not the table — the table
  `parent_pid` is not what decides reapability. Boot suite: 312 PASSED, 0
  FAILED (`MEMORY=2048 INSTANCE=5 cargo run --release`; below 2 GB HVF
  asserts `(isv)` on an unrelated test, `docs/archive/QEMU_HVF_ISV_BUG.md`).
- herd: `check_process_exits` now ends with a `wait_any()` (`wait4(-1,
  WNOHANG)`) sweep. A *service* pid that comes back from the sweep is routed
  through the normal exit handling, because a pid reaped once can never be
  waited for again. Prints `[herd] reaped N orphaned process(es)`.

**Evidence.** QEMU `-M microvm`, `SMP=4`, `init=/bin/herd`, both halves: an
orphaned `sleep 1` and a `SIGTERM`ed `nc -l` both left `ps` within seconds,
`zombies=0`, `[herd] reaped 2 orphaned process(es)` / `reaped 1 …` lines,
suite still `792 passed, 0 failed`. Deployed to the box the same night, final
binaries: kernel `192f6b4c…` at `/boot/akuma-amd64` (the 2026-09-22 kernel is
`.prev`), herd `eb037539…` at `/bin/herd` (`/bin/herd.prev` kept); both take
effect at the next reboot, left to the operator because kot was live.

`libakuma::print_dec` used to emit a stray NUL before the digits (`reaped \0 1`
in the first serial capture): its fill loop decremented one slot past the first
digit and printed from there. Fixed the same night (fill from `buf.len()`,
`i -= 1` before the store); the `reaped 2 orphaned process(es)` line above is
the after.

## Rules this adds

- **Any `global_asm!` entry that can run Rust must `cld` before the `call`.**
  `note_trap_entry_flags()` goes first in its dispatcher.
- A ring-0 dump whose `rflags` has bit 10 set is a smoking gun on its own;
  check it before decoding anything else.
- On x86 do not read "the values written are frame addresses" as "a page-table
  writer did this" — a backward `memcpy` of a keys array of a frame map writes
  frame addresses too.

## Background

- [`AKUMA_AMD64_CLOCK_GETTIME_SWITCH_FRAME_GP.md`](AKUMA_AMD64_CLOCK_GETTIME_SWITCH_FRAME_GP.md) — the first capture and the three theories this replaces.
- [`AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md`](AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md) — canaries and `x86_check_incoming_frame`, the detectors that fired.
- [`AKUMA_USER_ACCESS_X86_FIXUP.md`](AKUMA_USER_ACCESS_X86_FIXUP.md) — why vectors 13/14 have hand-written stubs at all.
- [`AKUMA_AMD64_COW.md`](AKUMA_AMD64_COW.md) — the CoW break whose copy ran backwards.
