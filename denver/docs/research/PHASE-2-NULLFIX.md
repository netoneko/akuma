# PHASE-2-NULLFIX — research (R1, written before any code)

**Gate:** PHASE-2-NULLFIX — G9's fix pass. PHASE-2-NULLSTORE named the death to first
order (D38) and left four opening jobs: bisect the clobber path, fix behind an
off-by-default feature relayed 1:1 (R6), regpreserve all-REGPASS, and the java arm
must MOVE or the fix isn't one (D37). This artifact carries the bisect's verdict
and the fix's shape.

## Finding 0 — the bisect is closed at the source: the legacy fold rewrites the caller's frame and never restores it

D38's hypothesis (a trap-frame preservation bug on the syscall-internal
fault-retry path) was **wrong in mechanism** — and the regpreserve arm's own
output contained the refutation, read only now: shape (c)'s `stat` returned
**−2 ENOENT** (`denver/gates/PHASE-2-NULLSTORE.log` ride-along), so
`sys_newfstatat` (`crates/akuma-syscalls-glue/src/fs.rs:2920`) returned *before*
`write_user_val` — **the copy-out never ran and no ring-0 fault ever fired**,
yet all six registers still came back clobbered. The fault-retry path is
exonerated; the clobber happens on every folded syscall, fault or no fault.

The rewriter is `dispatch_inner`
(`crates/akuma-tokio-kernel-amd64/src/lib.rs:1610`):

```rust
if let Some((call, a)) = akuma_syscalls_abi::legacy::fold(fr.rax, [fr.arg1()…fr.arg6()]) {
    fr.rax = call.to_x86_64();
    (fr.rdi, fr.rsi, fr.rdx, fr.r10, fr.r8, fr.r9) = (a[0], a[1], a[2], a[3], a[4], a[5]);
}
```

The `syscall` entry stub (`akuma-ring3-amd64::syscall_entry`) pushes the user
GPR file onto the task's kernel stack, `DISPATCH` gets `&mut SyscallFrame`
pointing at it, and the stub's exit pops **the same memory back into ring 3**.
The fold writes the `*at` call's marshaled arguments into the caller's return
state. `fold(4)` = `(Newfstatat, [AT_FDCWD, a1, a2, 0, 0, 0])`
(`crates/akuma-syscalls-abi/src/lib.rs:740`) — every value in the REGRED lines,
explained:

| observed after `stat` | fold's write |
|---|---|
| rdi=0xffffffffffffff9c | AT_FDCWD |
| rsi=0x600000000000 | the path pointer (page p) |
| rdx=0x600000001000 | the buf (q) |
| r10/r8/r9=0 | flags/unused args, zeroed |

The G9 death is this bug through musl's fstatat: the guest musl issues the raw
`stat` (nr 4, measured in the NULLSTORE ring), the fold succeeds (the path
exists), returns 0 — and musl's saved buf (r8) comes back **zeroed**, so its
`mov %r8,%rdi; rep stos` copy-out stores 144 bytes at address 0. A *successful*
folded syscall destroys the caller's argument registers. 19 legacy numbers
inherit the bug (`NUMBERS`: 2, 4, 6, 21, 22, 85, 111, 213, 232, 82–94 —
open/creat/stat/lstat/access/pipe/creat/…/epoll_wait/rename/mkdir/unlink/…), and
`child::deliver_at_return` packs the (already folded, already clobbered) frame
into any signal frame built on the same call. Why the other regpreserve shapes
stayed green: the lazy fault is not a syscall return, and `getppid` (110) is not
in `NUMBERS` — exactly the (b)/(c) axis D38 named.

## Precedents (R9) — searched docs/archive, docs/reference, the sibling kernels

1. **The metal is clean and states the right shape.**
   `crates/akuma-kernel-amd64/src/usermode/dispatch.rs:75` —
   `syscall_dispatch(nr, a1…a6) -> u64` folds **by value**
   (`fold(nr, [a1, …])` → local `call`/`args`); the caller's saved registers are
   never rewritten, so the metal has run busybox/python over folded `stat`/
   `mkdir`/`unlink` for months with no reg bug. The tokio deviation is the
   frame-passing dispatch: "rewritten in the frame so the one `*at` arm below
   serves both spellings" (the tokio comment) fixed the dispatch's *structure*
   and changed the caller's *observable state* — the metal's contract says the
   latter must not move.
2. `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH1.md` (cited by the fold's own doc) —
   the fold's history: a contract table, adopted for argument spellings; nothing
   in it licenses register-state change. The symlink/utimensat story is the
   pattern: a fold convenience that broke observable semantics and had to be
   named and fixed.
3. Linux syscall ABI (the contract the guest is written against): a syscall
   returns rax=RESULT, rcx/r11 clobbered by the `syscall` instruction itself,
   **every other register preserved** — rdi/rsi/rdx/r10/r8/r9 included. musl's
   fstatat keeping its saved buf in r8 across the syscall is exactly this.
4. `akuma-user-access`'s fixup invariants (lib.rs:244–268) — the same discipline
   stated for the copy path: the fault return must not spend the interrupted
   code's live registers. The fold's rewrite violates the sibling rule for the
   syscall return path.
5. The aarch64 twin is unaffected: no `legacy::fold` in
   `akuma-tokio-kernel-aarch64/src` (the x86-only spellings never arrive there);
   `procfs-diag`'s relay is the amd64-only feature-line shape to copy.

## Decisions

- **D39 — the fix: snapshot the six argument registers before the fold, restore
  them after dispatch, behind `fold-regpreserve` (off by default, R6).** In
  `dispatch_inner` only; one `Option<[u64; 6]>`, restored into the frame before
  `dispatch_inner` returns so `deliver_at_return`'s signal-frame packing (and
  the stub's pops) see the caller's state, while `dispatch_after_fold` still
  reads the folded arguments it needs. `rt_sigreturn`/exec/signal-entry rewrites
  of the frame are untouched — they define the return state on purpose; only
  the fold's rewrite is a clobber of it. Workspace lints are `deny` on
  unused_variables, so the default build (feature off) cfg_attr-allows the
  now-unread binding; the default build must stay behavior-identical, checked
  by a byte-compare of the default kernel before/after (R6 evidence).
  Relay 1:1: `akuma-tokio-kernel-amd64` → `akuma-tokio-kernel` (amd64-only
  line, the procfs-diag shape) → `akuma-jvm-kernel` → `denver`. Metal untouched.
- **D40 — the probe: `denver/probe-nullfix.sh`** (probe-nullstore.sh's shape:
  rig stamps, free-the-slot first, stop at the end, console window last). New
  rig `denver-nullfix` + `vm-nullfix.spec` (the nullstore spec's disk and port,
  new kernel path). Arms dispatched, in order: tokioprobe; RWX; MAPS; AUXV;
  MMARMS; SYSINFO (recorded, G7's feed); **regpreserve** (the fix's java-free
  proof: every REGRED is a fail, ≥20 REGPASS expected across the three shapes);
  **the java arm** (`jvmprobe name /opt/jdk/bin/java 3005fc41 -version`,
  output redirected to `/tmp/java-out.log` guest-side — sshd may never answer,
  the designed path).
- **D41 — the verdict.** RESULT PASS requires, in order: (1) rig identity —
  uname says denver, not the nullstore hash `800e8c80`, not procfs `f0dcacb6`,
  not vdso `3531cf69`; (2) tokioprobe PASS; (3) RWX PASS (rides); (4) MAPS +
  AUXV PASS (rides); (5) MMARMS PASS (rides); (6) SYSINFO recorded (PASS or
  INFO); (7) **REGPASS all** — zero REGRED/CRASH lines and every expected
  REGPASS present; (8) **the java arm MOVED**: the console window carries NO
  `[Fault] ring3 PageFault rip=0x000000003005fc41` and no slot-kill at the old
  death — progress past fstatat is a different rip (attribution recorded as the
  next gate's data), version output recovered from `/tmp/java-out.log`, or no
  kill at all. A kill at the old rip is an honest FAIL: the fix didn't take.

## Composition plan and new-LOC estimate (R5 budget)

1. this artifact (~120 LOC).
2. `crates/akuma-tokio-kernel-amd64/src/lib.rs`: the cfg-gated snapshot/restore
   in `dispatch_inner` (~20 LOC with the comment).
3. Feature lines in four Cargo.tomls (~12 LOC).
4. `denver/probe-nullfix.sh` (~200 LOC, mostly the nullstore shape).
5. Rig staging, `denver/gates/PHASE-2-NULLFIX.log`, RESULT.md, LOOP_LOG entry.

## Experiment proposal (the one-per-phase rule)

None new — PHASE-2-NULLSTORE's recorded proposal (knob
`GOOSE_TOOL_CALL_CUTOFF` at default, judged by this session's `final_ctx` in
COST_LEDGER.md) stands for the phase; the operator owns applying it.

## Outcome (session 12, 2026-11-14 ~09:0x–09:5x UTC — run to PASS)

**Verdict: PASS** (`denver/gates/PHASE-2-NULLFIX.log`, guest `0.1.0-72be5c55-denver`,
wall 33 s) — conditions 1–8 green: rig identity (not nullstore/procfs/vdso),
tokioprobe, `RWX PASS rw_to_rx=42`, `MAPS PASS lines=12` / `AUXV PASS` 15 pairs,
MMARMS (the recorded W+X→never-writable divergence rode again, still not judged),
`SYSINFO INFO errno=38` (G7's feed unchanged), **regpreserve 20/20 REGPASS**
(shape (c): r8/r9/r10 kept — the JVM-killing zeros are gone — rdi/rsi kept at
their entry values, rdx canary kept, `stat returned -2 (rdx canary kept)`), and
**the java arm MOVED**: the old rip 3005fc41 never appeared; the previously
lethal `stat` (nr 4) shows `-> 0` in the dying task's own ring, and java died
much later at a NEW rip with a full attribution window.

**The fix, measured.** `fold-regpreserve` (D39) restored the six argument
registers on the folded path; the regpreserve arm proves it java-free and the
java arm proves it at the death site. R6 evidence: the default build's `.text`
is byte-identical to HEAD's (objcopy-compared); the whole-file cmp differs only
in `.rodata` panic-Location line numbers (the source grew 25 lines) — behavior
identical with the feature off. Clippy: zero warnings for
`akuma-tokio-kernel-amd64` in both the default and the gate feature set.

**D42 — the probe's shape (c) expectations were unsatisfiable-by-ABI, not a
kernel residue.** The first PASS attempt failed on two REGRED lines (rdi/rsi):
the arm's own asm loads rdi/rsi with the syscall arguments (path/buf), so a
*preserved* register returns its ENTRY value — path/buf, not the canaries those
two `movabs` put there a moment earlier (unsatisfiable on the host too). The
first run was still the fix's proof where it mattered: r8/r9/r10/rdx came back
kept. jvmprobe.c now checks rdi/rsi against the entry values (D42); the
clobber-hunt power is unchanged — the buggy kernel's shifted-by-one pattern
(AT_FDCWD/path/buf, zeros) fails all six wants.

**The java arm's new death (recorded, next gate's data).** rip=0x600000067cd7a
(in libjvm's `r-xs` text), cr2=0x6000112e9000, code=0x7 — a user WRITE to a
PRESENT, read-only page: `pte=0x20dc6025` (P=1, W=0, U=1), refs=2 (shared),
instruction `mov %r12,(%rsi)` — an oop store + `movb $1,0x8(%rsi)` card mark.
`[mm] kind=2` says the faulting range was **MAP_FIXED-released** earlier in this
task's life. The next pass reads this block first (it is appended to the gate
log in full). Working name for the ladder: PHASE-2-HEAPRO (the present-RO heap
write), pending the REQUIREMENTS re-rank.

**Machine facts recorded, not fought:** the java arm's ssh connection died at
~15 s on ssh's own keepalives ("No data received for 15s over 3 keepalive
probes"), not the designed 180 s timeout — the guest-side arm kept running and
the console window carried the verdict anyway; the probe's `head -30` evidence
display cut the kill-path attribution out of the gate log (the full console
lives in the rig's `vmrun.log`), so the window was recovered and appended
post-verdict — display bounds are not evidence bounds, check the source file.

*(The experiment proposal from PHASE-2-NULLSTORE stands for the phase — knob
`GOOSE_TOOL_CALL_CUTOFF`, judged by this session's `final_ctx` in
COST_LEDGER.md.)*
