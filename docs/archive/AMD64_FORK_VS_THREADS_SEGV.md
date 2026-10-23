# amd64: a Go process that spawns children dies in `memmove`/`copystack` — "fork vs. threads"

**Status: FIXED 2026-10-20 in the signal return path (`amd64/src/signal.rs`, `.Lsig_return`),
verified locally — the 60-run sweep and the metal run are the open items (§ E).** Found 2026-10-19/20 on the ryzen metal (kernel `7d4d22d6-release-smp-shared`,
GRUB entry 20, `smp=8`) when `crush` died at startup; reproduced locally under QEMU at SMP=2
the same day (harness below).

The title is the symptom as it was first understood. The cause is **signal delivery**: a signal
delivered at a LAPIC tick (or a fault) and returned from through `sysretq` hands the interrupted
code back with `RCX = its own RIP`, `R11 = RFLAGS`, and whatever the handler left in the SSE/AVX
registers. Every child exit is a `SIGCHLD`, Go installs a handler for it, and Go's `memmove`
keeps a pointer in `CX` and the copy's tail in `X5`–`X12` across its loop. The forks matter only
because they produce the signals; the sibling threads matter only because a thread that is busy
in ring 3 takes its signal at a tick instead of at a syscall return.

## Symptom

`crush` (v0.98.1, a static Go binary) started in `/src/github.com/netoneko/akuma` — a git
repository — dies within seconds with a Go runtime crash:

```
SIGSEGV: segmentation violation
PC=0x498ed5 m=2 sigcode=2 addr=0x498e35
runtime.memmove()  runtime/memmove_amd64.s:456
runtime.copystack(...)  runtime/stack.go:985
runtime.newstack()  runtime/stack.go:1216
goroutine 1 [copystack]: ... internal/ui/styles.LoadTheme  themes.go:297
```

(variants: `fatal error: fault`, `unexpected signal during runtime execution`). Started in a
scratch directory (not a git repo) the same binary never crashed. In a repo crush builds its
system prompt at startup by running `git status` & friends through `os/exec`: the same dump shows
four goroutines parked in `wait4` while goroutine 1 grows its stack to ~1.5 MB.

Go's `os/exec` starts children with `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)`, which this kernel
serves as a copy-on-write `fork` (`amd64/src/usermode.rs`, the `Syscall::Clone` arm →
`sys_spawn_clone` → `akuma_exec::process::fork_process`) and does not suspend the parent. **Each
child's exit raises `SIGCHLD` on the parent** — that is the part that turned out to matter.

## Minimal repro — no crush, no git

`scripts/utils/go_fork_stack_repro/main.go` (2.8 MB static binary, cross-compiled on the Mac:
`GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o gofork .`; installed on the ryzen box as
`/bin/gofork`). N goroutines spawn `/bin/true` in a loop; the main goroutine repeatedly makes
fresh goroutines that must grow a ~1.5 MB stack.

```sh
gofork -spawners 0 -rounds 200      # control: always prints OK
gofork -spawners 1 -rounds 200      # BUG: SIGSEGV / "fatal error: fault" every time
gofork -spawners 4 -rounds 200      # BUG
gofork -spawners 8 -rounds 200      # BUG
```

Always `timeout 60` a run. Ryzen metal, 4 runs each, 200 rounds:

| `-spawners` | result |
|---|---|
| 0 | **4 / 4 pass** |
| 1 | **4 / 4 crash** |
| 4 | **4 / 4 crash** |
| 8 | **4 / 4 crash** |

Crashes look like `unexpected fault address 0x248800000 / fatal error: fault / SIGSEGV code=0x1
(MAPERR)` and, in one run, `sigcode=2 (ACCERR) addr=PC-0xa0`. **One concurrent spawner is
enough.** A child that lives 100 ms (`busysleep`) instead of exiting at once made the run pass:
the rate scales with the number of child *exits*, which is the number of `SIGCHLD`s.

## Root cause

### The three defects, in `amd64/src/signal.rs` and `.Lsig_return` (`amd64/src/usermode.rs`)

A pending signal is acted on at one of three boundaries: a `syscall` return (`deliver_pending`,
register file in `UserCtx`), a ring-3 `#PF`/`#GP` (`deliver_fault_signal`, register file in
`TrapRegs`), or a LAPIC tick that interrupted ring 3 (`deliver_pending_on_tick`, same). Entering
the handler is correct on all three. **Returning is not**, for a signal delivered on the second or
third:

1. **`RCX` and `R11` are not carried.** `Regs` (the shared register-file shape) holds twelve
   registers plus `rip`/`rsp`/`rflags`/`rax`; `rcx` and `r11` were left out on purpose because the
   `syscall` instruction destroys them, and `signal::smoke_test` pins the omission
   ("rcx is not carried (sigcontext reports 0)"). That reasoning holds for a syscall-return
   delivery. A tick or a fault interrupts an **arbitrary instruction**, where both are live.
2. **`rt_sigreturn` returns through `sysretq`.** `sys_rt_sigreturn` stores the frame's file into
   `UserCtx` and `.Lsig_return` `sysret`s with it. `sysretq` loads `RIP` from `RCX` and `RFLAGS`
   from `R11`, so the interrupted code resumes with **`RCX = the address it was interrupted at`**
   and `R11 = its flags` — whatever it had in them is gone. Linux returns from `rt_sigreturn`
   through `iret` for exactly this reason.
3. **No FPU/SSE/AVX state is saved across the handler.** The module header states it as a pinned
   divergence (`uc_mcontext.fpstate` is `NULL`; "nothing in this tree's userspace reads it"). What
   matters is not who reads the field but that the *registers* come back: the handler's own code
   clobbers them, and nothing restores them. Go's compiled code uses `XMM` for every struct zero
   and copy, so its `SIGCHLD` handler clobbers `X0`–`X14` of whatever it interrupted. The kernel
   is soft-float and saves this state at context switches (`sched.rs`, `xsave64`/`xrstor64`,
   `XCR0 = 0b111`), so the plumbing exists; it is simply not applied at signal delivery.

Defect 1 also means a handler entered from a tick or fault sees `rcx = 0`, `r11 = 0` in
`uc_mcontext` — cosmetic next to the return.

### Why this shows up as "fork vs. threads"

- Every child exit is `publish_child_exit` → `pend_signal_to_group(parent, SIGCHLD)`, and
  `pend_signal_to_group` pends the signal on **every thread of the group** (a divergence from
  Linux, which delivers a process-directed signal once). The Go runtime installs a handler for
  `SIGCHLD` (`sigtable`: `_SigNotify+_SigUnblock+_SigIgn`, installed because the inherited
  disposition is `SIG_DFL`), with `SA_SIGINFO|SA_ONSTACK|SA_RESTORER`. So one `/bin/true` exit
  = one handler entry on *each* Go thread, including the one running `copystack`.
- A thread blocked in `wait4`/`futex` takes its copy at the syscall return — harmless, the ABI
  already clobbers `rcx`/`r11` and the FP registers are dead across a Go syscall. A thread that is
  **busy in ring 3** (goroutine 1 growing a 1.5 MB stack: demand faults and `memmove`) takes its
  copy at the next tick, mid-instruction-stream — the dangerous case. `-spawners 0`: no exits,
  no signals, no crash. `busysleep` children: ~100× fewer exits, pass.
- "Multithreaded parent" is therefore a proxy: single-threaded `fork` users (cargo, gcc, a shell)
  spend the handler window inside `wait4`, so their delivery is at a syscall return.

### The metal signature, decoded against Go 1.25's `memmove_amd64.s`

```
430:	LEAQ	(DI)(BX*1), CX          // CX = destination end, held across the loop
432: gobble_mem_fwd_loop:
439:	VMOVDQU	(SI), Y0 ... Y3         // Y0–Y3 and X5–X12 (the tail) live across the loop
...
450:	JA		gobble_mem_fwd_loop
453:	SFENCE
454:	VMOVDQU	Y4, (R8)
455:	VZEROUPPER
456:	MOVOU	X5, -0x80(CX)           // <- memmove_amd64.s:456, the crash line
457:	MOVOU	X6, -0x70(CX)
...
```

The dump says `PC=0x498ed5`, `addr=0x498e35`, `sigcode=2` (ACCERR). A tick inside the loop
delivers the `SIGCHLD`; `rt_sigreturn` resumes with `RCX = the interrupted RIP` (about `0x20`
bytes before the store at line 456); the store at line 456 then writes to `RIP − 0x80 =
PC − 0xa0`, which is memmove's own text — present and read-only, hence `SEGV_ACCERR`. Had the
store not faulted, `X5`–`X12` were clobbered by the handler anyway and the tail would have been
garbage. This decodes an *un-instrumented* crash dump from the metal to the instruction.

In the non-AVX paths (lines 110–122: `REP; MOVSB` / `REP; MOVSQ`) `CX` is the **count**. A `REP`
string instruction is interruptible between iterations, so a tick lands inside a long one
routinely; the return sets `RCX := RIP ≈ 0x4xxxxx` ≈ 4–5 million further iterations, and the copy
runs megabytes past its destination. That is the "lost update / clobber via a stale translation"
the 2026-10-20 addendum inferred — the stale translation was a register, not a TLB — and it is the
Mac repro's `SIGSEGV code=0x1 addr=0x248800000`: the overrun reaches the first page of the arena
chunk Go had not `MAP_FIXED`-mapped yet, exactly as the region trace showed (Go never issued the
mapping; a runaway copy walked into it).

### What this clears up

- **H1 (fork walk vs. concurrent fault), H2 (frame accounting), H3 (large-stack path) are all
  wrong.** The CoW/TLB paths read as sound: the amd64 share pass holds the owner's address-space
  lock for the whole walk and flushes `AllCores` once at the end; the `Copy` arm flushes
  `AllCores` before freeing the old frame; `install_filled_page` and `cow_write_fault` take the
  same owner lock; the shootdown targets every core whose published `CR3` is the edited root. The
  instrumentation that was added to find a memory bug (refusal counters, region trace,
  `pmm-forensics`) was silent because there was none.
- The `#GP in allocNeedsZero`, `found bad pointer in Go heap`, `new g is not Gdead` and
  prot-none `MAPERR` presentations are all downstream of one runaway or short `memmove`.
- `SMP=1` passing was **one** local run and is not explained by this analysis: a one-core boot
  delivers to the busy thread at its next tick too. Re-run it several times when verifying; it is
  not needed by the fix.
- `AMD64_CLAUDE_LOGIN_INPUT_STUCK.md` (Bun's startup spin of `vfork`s with a multithreaded
  parent, main thread stalls) is very likely the same defect: ~18 child exits = ~18 `SIGCHLD`s to a
  JIT-heavy process. Re-test after the fix before investigating it on its own.
- The same return path serves `deliver_fault_signal`, so a program that **resumes** after a
  caught fault — Go's `sigpanic`, V8's wasm traps — has been resuming with a corrupted `rcx`/`r11`
  and FP file since 2026-09-11.

## Fix (landed 2026-10-20)

Smallest change that makes the invariant true: **a signal handler's entry and return preserve
the complete ring-3 machine state of the interrupted instruction.** Parts A–D below landed as
designed, with three differences: on a plain syscall-return delivery `Regs::load` reports
`rcx`/`r11` as what the `syscall` instruction left in them (the return address and the flags),
which is what Linux's `sigcontext` shows there; the probe is a raw `#![no_std]` program under
`userspace/amd64/sigregs/` (built by `amd64/build.rs` and embedded like `killprobe`, not a musl
binary), with a `fork`ed child as the sender; and its fault check is every eighth window of the
same loop rather than a second mode.

### A. Carry `rcx` and `r11` (`amd64/src/signal.rs`, `amd64/src/usermode.rs`)

- `Regs` gains `rcx: u64, r11: u64`. `from_trap` reads them from `TrapRegs::{rcx, r11}`;
  `to_sigcontext` writes them (the frame stops reporting 0); `from_sigcontext` reads them back;
  `store` writes them to two new `UserCtx` fields; `load` zeroes them unless the file being loaded
  is a restored one (`sig_return != 0`), in which case it reads the `UserCtx` fields.
- `UserCtx` gains `sig_rcx` (offset 200) and `sig_r11` (offset 208); add both to the hand-indexed
  offset assertions and to `UserCtx::new()`.
- `signal::smoke_test`: replace "rcx is not carried" with a round-trip check of `rcx`/`r11`
  through `from_trap` → `to_sigcontext` → `from_sigcontext`, and add the two `UserCtx` offsets.

### B. Return through `iretq` (`.Lsig_return`)

`sysretq` cannot restore `rcx`/`r11` by construction. `.Lsig_return` keeps its one-path-two-authors
shape and becomes:

```
.Lsig_return:
    mov qword ptr [rcx + 184], 0        /* consume uctx.sig_return */
    push {user_ss}                      /* iretq frame: ss, rsp, rflags, cs, rip */
    push qword ptr [rcx + 8]            /* uctx.user_rsp */
    push qword ptr [rcx + 176]          /* uctx.user_rflags (already sanitized) */
    push {user_cs}
    push qword ptr [rcx + 32]           /* uctx.user_rip */
    mov rax, [rcx + 192]                /* sig_rax */
    mov rdi, [rcx + 48] ... mov r15, [rcx + 136]   /* the twelve, as today */
    mov r11, [rcx + 208]                /* sig_r11 */
    mov rcx, [rcx + 200]                /* sig_rcx — last, it is the base */
    swapgs
    iretq
```

`user_cs`/`user_ss` are `const` operands of the `global_asm!` from `akuma_gdt::{USER_CS,
USER_SS}` (RPL 3 included), so the selectors cannot drift from the GDT. `sanitize_rflags` stays
load-bearing — `iretq` installs the frame's `RFLAGS` as `sysret` installed `r11`, and `IF` must
stay set / `IOPL`,`NT` clear. `user_rip_ok` stays — `iretq` `#GP`s in ring 0 on a non-canonical
`RIP` as `sysret` does. Entry from a tick/fault (`install_handler_frame`) is already an `iretq`
and does not change; `enter_user_mode`, `enter_user_mode_forked` and `.Lexec_return` keep their
`sysretq` (fresh contexts with no `rcx`/`r11` to preserve).

### C. Save and restore the FPU/SSE/AVX file across the handler

- `sched.rs` exposes three helpers over the existing `FxArea` (832 bytes, 64-aligned,
  standard-format `xsave`; the first 512 are the `fxsave` image used when `smp::XSAVE_ON` is
  false): `fpu_save_live(&mut FxArea)`, `fpu_restore_live(&FxArea)`, `fpu_reset_live()`
  (`xrstor` of `FxArea::initial()`, which is what Linux's `fpu__clear_user_states` gives a handler).
- `build_frame`: save the live state into a stack `FxArea`, place it on the user stack **above**
  the `rt_sigframe` at a 64-byte-aligned address (Linux's placement), `write_val` it, set
  `uc_mcontext.fpstate` to that address, then reset the live state. The alternate-stack size test
  must include the area. On any write failure decline as today.
- `sys_rt_sigreturn`: read `fpstate` from the frame; require a user address, 64-aligned,
  readable; copy to a kernel `FxArea`; **sanitize before `xrstor`/`fxrstor`**, which `#GP` in ring 0
  on a malformed image: `MXCSR &= mxcsr_mask` (bytes 28..32 of the image; `0xFFBF` if zero),
  `XSTATE_BV &= XCR0_ENABLED`, `XCOMP_BV = 0`, header reserved bytes zero. A frame that fails
  validation kills the process with `SIGSEGV`, as a bad `rip` does. Then `fpu_restore_live`.
- Pinned divergence to record in the module header, replacing the current one: no
  `_fpstate.sw_reserved` magic (`FP_XSTATE_MAGIC1/2`); nothing in the tree reads it and the
  standard-format image from offset 512 is what Linux's `_xstate` carries.
- Allocation audit: none — a stack `FxArea` in the two functions, both already on the task's
  kernel stack; the user copies go through `write_val`/`read_val`. Cost: two 832-byte copies plus
  an `xsave`/`xrstor` per **delivered** signal; zero on the no-signal path.

### D. A ring-3 regression probe that fails before and passes after

`userspace/forktest/c_stress/sigregs.c` (static musl, shipped into the amd64 image like
`killtree`, run from the boot suite beside `usermode::kill_test`, as `/probes/sigregs`):

- the main thread spins in inline asm holding sentinels in `rcx`, `r11` and `xmm0`–`xmm15`
  (`ymm` upper halves through `vinsertf128` when `cpuid`/`xgetbv` say AVX), comparing all of them
  on every iteration;
- a helper thread loops `tgkill(main_tid, SIGUSR1)` with a short `nanosleep`, so the signal is
  pending while the main thread is in ring 3 and is delivered at a tick — the exact path; this
  works at `SMP=1` too (the sender runs while the spinner is preempted, the next tick delivers);
- the handler deliberately clobbers `rcx`, `r11` and the vector registers;
- exit 0 after N deliveries with no mismatch; otherwise print the register, the expected and the
  observed value, and exit 1. Before the fix the first mismatch is `rcx == RIP` within a few
  deliveries.

Add a second mode (`sigregs fault`) that takes a caught `SIGSEGV` from a deliberate null store
inside the same checked loop and resumes past it from the handler, to cover `deliver_fault_signal`.

### E. Verification — measured so far (2026-10-20, QEMU/TCG on the Mac, kernel with the fix)

| check | result |
|---|---|
| amd64 boot suite, fixed kernel, SMP=2 | **829 passed, 0 failed**, `sigregs` green |
| the same probe on the **pre-fix** kernel (`0456f044` + probe only, a scratch worktree) | **826 passed, 1 FAILED**: `sigregs: mismatch mask 0x3fffff` — all 22 checked registers clobbered, exit 1 |
| `gofork -spawners 8 -rounds 200`, SMP=2 | **`OK`** after 416 s of guest time — the first pass of that configuration; it crashed within seconds on every pre-fix run |
| `cargo check` for `x86_64-unknown-none` and the default AArch64 target | clean; clippy clean for the lines this change touches (the crate's other pedantic findings are pre-existing) |

In progress / not yet done:

1. The sweep: 20 runs each at spawners 8, 4 and 1, SMP=2, four VMs at a time
   (`gofork_campaign.sh` in the session scratchpad: a copy of the disk image per VM, the VM
   stopped as soon as `OK` or a crash marker prints). Results go here when it finishes.
2. `SMP=1` a few runs for the record; the cost of a signal round trip before/after (`raise`
   × 100 000) — expect about a microsecond per delivery for the two 832-byte copies.
3. Metal (the ryzen box), per the ground rules below: install to the **verify slot only**, the user
   arms GRUB entry 26, then `/bin/gofork` 20/20 at spawners 1, 4, 8; `crush` headless in the repo;
   a `kbuild`; the `AMD64_CLAUDE_LOGIN_INPUT_STUCK.md` scenario re-tested. The amd64 boot suite
   there should read 829 passed (804 + the sigregs checks + the instrumented build's own additions).

### F. Docs updated with the fix

- `amd64/src/signal.rs` module header: the "no FP state" divergence is retired; the return path
  paragraph says `iretq` and why.
- `docs/archive/AKUMA_AMD64_COW.md` § Known limits rewritten (CoW fork runs at `SMP>1` behind the
  2026-09-09 shootdown; threads exist and the share pass / break serialize on the leader's lock;
  **this bug was not there**); `docs/reference/subsystems/memory.md` and the `Syscall::Clone`
  comment in `usermode.rs` say the same.
- `docs/README.md` symptom row carries the fixed status; `userspace/amd64/README.md` lists the probe.

## Ground rules when finishing this on the metal (folded from the retired proposal)

- **Never `git commit`, `git push`, `git reset`, `git rebase`** — the user drives all commits. No
  subagents. Do not reboot the box; do not touch `/boot/akuma-amd64`. Build with `kbuild` (about
  10 minutes, detached: `nohup sh -c 'kbuild -j 1 > /root/kb.log 2>&1' &`) and install **only** to
  the verify slot:
  `sh scripts/install_kernel_amd64.sh /root/ktarget/x86_64-unknown-none/release/akuma-amd64 /boot/akuma-amd64.verify`,
  then tell the user to arm GRUB entry 26 from Pop (boots the verify kernel with entry 20's
  command line) and run `gofork` there.
- The box's traps: an ssh/crush session has **no `PATH`** (full paths, or `. /etc/akuma-dev.env`);
  closing a pty does not kill its children — `kill -9` strays by pid, never `pkill -f`; `-j1` for
  builds (*correction 2026-10-23: `-j4` has been stable for weeks; the corruption this note feared was fixed, see `AMD64_J4_BUILD_CRASH_HUNT.md`, so use `kbuild -j 4`*). There is no Go toolchain on the box: `gofork` is
  rebuilt on the Mac.
- Kernel conventions apply: no allocation on the signal path, console output through
  `safe_print!`, `amd64` never `x86`.

**Done when:** `gofork` 20/20 at spawners 1, 4 and 8 on the verify kernel (user-confirmed on the
metal); `sigregs` fails without the fix and passes with it; host checks clean; the docs in § F
updated. **Report:** the before/after `gofork` numbers, the cost measurement, and what could not
be verified locally with the exact commands the user must run.

## Local harness (how every local claim here was made)

The bug reproduces under QEMU on the Mac — TCG, no KVM — at SMP=2, ~4 min per run:

```sh
# repro binary — the Mac has a Go toolchain
GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o target/gofork/gofork \
    scripts/utils/go_fork_stack_repro/main.go
# disk: build the standard image and drop the binary in
sh amd64/mkdisk.sh target/gofork/root.img 128
export PATH="/opt/homebrew/opt/e2fsprogs/sbin:$PATH"   # debugfs is keg-only
debugfs -w -R "write target/gofork/gofork bin/gofork" target/gofork/root.img
# boot it as init, SMP=2; output (and the crash) lands in the log
DISK=$PWD/target/gofork/root.img SMP=2 INIT=/bin/gofork \
    INITARGS=-spawners,8,-rounds,200 SSH_PORT=2224 HTTP_PORT=8081 \
    overlays/devbox-amd64/run.sh > run.log 2>&1
```

sshd into the guest does **not** work under this rig (TCP connects, the handshake never
completes); booting gofork as `INIT` avoids that. `target/gofork/` holds the image with `gofork`
and `busysleep` already installed plus the logs of the 2026-10-20 runs (`pid1.log` is the fully
traced one).

| config | result (kernel `4b7bdc59` + instrumentation) |
|---|---|
| SMP=1 | passes (one run) |
| SMP=2, `-spawners 8` | crashes — the metal signature: `SIGSEGV code=0x1 addr=0x248800000 pc=0x4c7854` (`main.grow`) |
| SMP=8 | crashes; presentations vary (`bad pointer in Go heap`, `#GP` in `allocNeedsZero`, `new g is not Gdead`) |
| SMP=2, children living 100 ms (`/bin/busysleep`) | passes |

## History of the hunt (kept for the record; superseded by "Root cause")

### What was established before the cause was found

1. The parent is multithreaded and forks while a sibling thread takes page faults.
2. No kernel log line accompanies the crash (Go installs its own `SIGSEGV` handler).
3. Single-threaded fork is fine (cargo/rustc/gcc, the self-host build); multithreaded processes
   that don't fork are fine (`-spawners 0`).

### The memory hypotheses, as ranked on 2026-10-20 (all wrong — see "What this clears up")

- H1 — CoW share pass vs. a concurrent demand fault in the same address space.
- H2 — frame accounting: a page installed during the pass counted wrongly, freed by the child's
  teardown while the parent still maps it.
- H3 — the large-stack allocation path.

The evening session's instrumentation showed the kernel's region list, PMM refcounts, UAF
quarantine, premature-free check and resurrection detector all clean and mutually consistent
through every crash; the one fully traced run showed the fatal write at `0x248800000` landing in a
`PROT_NONE` reservation Go had never `MAP_FIXED`-mapped, with no foreign-pid region edit anywhere.
That was read as "downstream corruption of Go's own bookkeeping" and re-ranked H2 down. It was
downstream of a runaway `REP MOVS` with `RCX = RIP`.

### Stale CoW doc text found on the way

- `amd64/src/thread.rs` (module doc): since 2026-09-09 the target has a shootdown, so CoW `fork` is
  no longer SMP=1 only. `docs/archive/AKUMA_AMD64_COW.md` § Known limits, the `Syscall::Clone`
  comment and `docs/reference/subsystems/memory.md` still say "SMP=1 only" and "no threads"; CoW
  fork has run at `SMP>1` for weeks (self-host builds at `-j4`) and threads exist. Dated corrections
  were added 2026-10-20; the rewrite is item F above. The bug was **not** in that code.

### Instrumentation landed 2026-10-20 (behaviour unchanged; keep — a healthy kernel prints nothing)

- `amd64/src/mm.rs`: `FAULT_REFUSALS` + `note_fault_refusal(va, why)` naming the refusing arm
  (`no-region`, `prot-none`, `oom`, `install`, `populate`); `REGION_TRACE` toggle (default off) with
  `MAP_FIXED`/detach/insert lines carrying the editing pid; `usermode::current_mm_pid()`.
- `amd64/src/idt.rs`: `cow_write_fault`'s `CowAction::Fault` arm prints through
  `note_fault_refusal(addr, "cow-fault (ro, unmarked)")`.

## Related

`AMD64_CLAUDE_LOGIN_INPUT_STUCK.md` (probably the same defect — re-test after the fix),
`AKUMA_AMD64_FAULT_SIGNALS.md` (the fault-delivery path that shares the return),
`AKUMA_AMD64_COW.md`, `AMD64_TLB_STUCK_MMAP_REGIONS.md`, `PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md`
(why a group signal is pended on every thread).
