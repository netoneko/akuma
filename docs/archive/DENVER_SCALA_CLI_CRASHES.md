# Denver: why `scala-cli` does not run normally in a box (investigation, 2026-11-20)

Branch `ecosystem-demo-stage1`. Kernel: Denver (`containers,codex`), QEMU microvm + KVM, guest images in
`/var/tmp/akuma-eco/denver-ready`. Status: **fixed for `scala-cli run`** (2026-11-20: the reference demo passes in full). Defect A and B's cause is the non-leader `execve` leaving the caller on the old root (last section before Reproduce), Defect C is the `sysretq` register clobber on the fault-signal sigreturn. The launcher in `overlays/ecosystem-demo/scala/usr/local/bin/scala-cli` and `-Xint` hide the symptoms; they
are workarounds, not fixes, and this document is the record of what is actually wrong.

## Symptoms

1. `scala-cli run` of the task board: the JVM dies (`hs_err`), or the whole guest disappears.
2. `scala-cli compile` sometimes dies in HotSpot; `JAVA_TOOL_OPTIONS=-Xint` hid it.
3. The earlier trace showed `[execve] non-leader thread` followed by a sibling faulting on an unmapped stack.

## Defect A: `execve` from a non-leader thread (fixed; proven by `scala-cli run`)

The Tokio kernels (`akuma-tokio-kernel-core/src/proc.rs`, `exec_strs`) replaced the group's address space under live
sibling threads; Denver had no `de_thread`. The main amd64 kernel fixed the same class by detaching
(`EXEC_FROM_THREAD_DETACH.md`). Here, after the new image has loaded (so a failing exec still kills nothing),
`kill_siblings(cur, group_leader(cur))` stops every other thread, the leader included, and the leader's slot, not the
caller's, owns the new root (`disown_space`, `adopt_exec_root`, `OWNED_ROOT`). A non-leader caller continues as the
group's only thread. `overlays/ecosystem-demo/execthread-probe.py` passes 12/12 on Denver with it. **That proves little**:
the same probe passed intermittently before. Not yet shown: `scala-cli run` execing the compiled application.

## Defect B: the guest triple-faults when a thread cloned before an `execve` starts (fixed at the first-run window)

### What was observed

Run the same program with `JAVA_TOOL_OPTIONS=-XX:TieredStopAtLevel=1` (or `-Xint`) and the guest goes silent about 25 s in,
QEMU exits (`-no-reboot`), ssh answers "connection refused". Serial console last lines:

```
[exec] '/sbin/ldconfig': read failed (Read(NotFound))
[TRAMP-MISMATCH] tid=4 THREAD_PID_MAP=172 but table scan found 85
```

No panic text, no `[PANIC]`. It reproduces with and without the Defect A change (a baseline kernel was built from the
parent commit), so it is not a regression from it.

### How it was localised

* `-d cpu_reset` under KVM logs nothing useful. Under TCG (`ECO_ACCEL=tcg ECO_CPU=max ECO_QEMU_EXTRA='-d int,cpu_reset -D ...'`,
  new overrides in `overlays/ecosystem-demo/ecolib.py`) QEMU reports `Triple fault` at the same point, so the crash is not
  a KVM artefact. 200 MB of `-d int` log; the tail:

  ```
  v=0e e=0010 cpl=0 IP=0008:ffffffff802f7f4c SP=0010:ffff800000b75380 CR2=ffffffff802f7f4c   (#PF, instruction fetch)
  v=08 ...                                                                                    (#DF, same RIP)
  Triple fault           RAX=00000000210c3000
  ```

* `ffffffff802f7f4c` is two instructions into `akuma_mmu::UserAddressSpace::activate` (symbol `+0x2c`): it had just written
  `CR3 = 0x210c3000`, and the next fetch from kernel text faulted. So **a root that does not map the kernel's upper half was
  loaded**. The kernel text is not mapped in it, the #PF handler cannot run, the double fault cannot run: triple fault with no
  message.
* The amd64 `UserAddressSpace::activate` wrote CR3 unchecked, while `akuma_pagetable::amd64::activate` already refuses such a
  root (it asserts the live and new root share the kernel slots, the check behind the
  "root does not share the live kernel slot 511" panic recorded in `DENVER_BOX_WIRING.md`). It now goes through that
  function. Result, on the same run:

  ```
  [PANIC] crates/akuma-pagetable/src/amd64.rs:279
         activate: root does not share the live kernel slot 511 - refusing to load it
  ```

  A silent triple fault is now a named panic. That is the fix for the *silence*; the bad root itself is not fixed.

### Cause (found 2026-11-20) and fix

Temporary `[activate]`/`[root]` prints (pid, tgid, root at every `Process::run`, every exec, every zombie root) gave the
sequence, with pid 85 the JVM and slot 3 one of its Java threads:

```
[activate] run pid=167 tgid=85 root=0x210d7000          a thread of 85 starting, on 85's root
[root] exec pid=85 slot=4 caller=3 old=0x210d7000 new=0x2a5c1000     the JVM thread execs the compiled application
[activate] run pid=170 tgid=85 root=0x210d7000          a thread cloned BEFORE the exec starts, on the OLD root
[PANIC] ... root does not share the live kernel slot 511
```

A thread's `Process` holds a *view* of its leader's root taken at `clone`. `execve` replaced the group's root and the
old one was freed, but a thread that was cloned just before and had not yet run is not a published slot, so
`kill_siblings` (which walks in-use slots) could not stop it. Its first `Process::run` then loaded the freed root. Not
specific to scala-cli: any multithreaded program that execs while another thread is mid-`clone` can hit it; a JVM's
`Runtime.exec`/launcher does exactly that.

Fix (`akuma-exec/src/process/mod.rs`, `Process::run`): a thread (`pid != tgid`) whose view of the root differs from its
group leader's current root belongs to the replaced image and ends (`[RUN-REFUSED] thread ... holds a replaced root`)
instead of running. Verified: the same run no longer loses the guest; the refusal fires for pid 170. It is a guard at the
first run, not a removal of the window: a clone that publishes after this check and before the activate can still load a
replaced root, now a named panic (the checked `activate`) rather than a triple fault. Closing that fully needs `clone` and
`execve` to exclude each other, or `kill_siblings` to see a half-created thread. Open.

## Defect C: the HotSpot SEGV (fixed: see "Cause found and fixed ... `sysretq` ate `rcx`/`r11`")

`hs_err`: `SEGV_MAPERR` reading `0x600019001230`, in interpreted `AbstractValidatingLambdaMetafactory.checkDescriptor`.
Checked first because it looked like an unmapped heap page: it is not. `-Xlog:cds` in the box shows the compressed-oop base
`0x600019000000`, heap `0x600019200000-0x600039200000` (512 MB), CDS regions and class space mapped as logged. The fault address is
0x200000 below the heap, in the reserved, never-mapped gap, so the page fault is correct. The *value* is the bug: a
compressed oop `0x246` (`base + (0x246 << 3) = 0x600019001230`), i.e. the JVM read a wrong word. A second run with C2 only
(`-XX:-TieredCompilation`) failed with `ArrayIndexOutOfBoundsException: Index 167233916 out of bounds for length 61`, again
garbage in a register or a field. Findings:

| variant | result |
|---|---|
| default, smp=2 | crash, same `checkDescriptor` pc every run, reproduced in ~30 s with a 3-line lambda program |
| default, smp=1 | crash | 
| `-XX:-UseCompressedOops` | crash (a different `libjvm.so` frame) |
| `-XX:+UseSerialGC -XX:-TieredCompilation` | `ArrayIndexOutOfBoundsException` with a garbage index |
| `-XX:TieredStopAtLevel=1`, `-Xint` | no `hs_err`, but the guest then dies in Defect B |

So it is **not** an SMP race (smp=1 crashes) and **not** oop encoding. The main amd64 kernel runs the same workload.
Candidates not yet tested: wrong page contents after copy-on-write or `mprotect` (HotSpot flips code-cache and
polling-page protections), user register or FP state lost across a signal or thread switch, a stale TLB entry on a core.
Defect B and Defect C may share a cause (a wrong or stale root or mapping); that is a hypothesis, not a finding.

### Where it stands after the Defect B fix (2026-11-20, later)

With the first-run guard in, the default variant no longer takes the guest down; it dies in the JVM, deterministically,
at the same pc (`0x600011289c6a`), exit 139 within ~15 s, `[Fault] slot 24 killed (SIGSEGV)` on the console. New
facts:

* The faulting instruction is `cmp rax,[rcx]` in the template interpreter (an `invokevirtual`), with `rcx` loaded from the
  interpreter's expression stack: that stack slot holds `0x600019001230`, the decoding of narrow oop `0x246`. So a word
  the JVM wrote as an object reference now reads `0x246`.
* **`0x246` is the x86 `RFLAGS` a `syscall` leaves in `r11`** (the fault dump shows `r11=0x246`). A kernel-side write of a
  saved flags word into user memory, or a user register restored from the wrong frame slot, is the natural source. Not
  proven; it is the lead, and the value is too specific to be chance.
* The checks already ruled out: mapping size (above), oop encoding (`-XX:-UseCompressedOops` still crashes), SMP (smp=1
  still crashes), C1-only and `-Xint` (no crash there, so the corruption needs compiled code or the tiered machinery).
* Signal frames: `push_handler` skips the red zone and the synchronous-fault path (`faultsig`, feature `fault-signal`) restores
  from the `ucontext`, which is what HotSpot edits. Both were read, not yet instrumented.

Next steps, in order: watch for the first write of `0x246` into JVM memory (a user-memory watch under TCG, or log every
kernel write to a user address with that value); compare the fault-signal sigreturn's restored registers with the
interrupted ones for a thread that took a HotSpot safepoint or null-check signal; run the same program on the main amd64
kernel with `-Xlog` identical to confirm it does not take the path.

### Cause found and fixed (2026-11-20, later still): `sysretq` ate `rcx`/`r11` on the fault-signal sigreturn

The lead was right. `faultsig::sync_sigreturn` restored the interrupted registers from the ucontext, but the syscall exit is
`sysretq`, which loads `rcx` with `rip` and `r11` with `rflags` (`0x246`). The module header recorded this as a limit
("the restore cannot put `%rcx`/`%r11` back"). HotSpot's implicit null checks and safepoint polls reach it as SIGSEGV in
compiled code that keeps live values in `rcx`/`r11`, so after the handler returned a register held `0x246`; that is why
`-Xint` and C1-only never crashed.

Fix: `akuma_ring3_amd64::set_exit_rcx_r11` arms a one-shot per-CPU flag (offsets 40/48/56 of `PerCpu`) and `syscall_entry`
leaves by `iretq` with those two registers loaded; `sync_sigreturn` arms it from `gregs[REG_RCX]`/`gregs[REG_R11]`. The
async-signal path still clobbers them (unchanged).

Verified: `refdemo.py --scala-direct` no longer produces any `[Fault]`/SIGSEGV; `scala-cli compile` of the task board runs
to completion in 33 s. **Still open:** `scala-cli run` then does not bring the board up within the harness's 600 s
(board.log stays at the dependency hint). Diagnosed on the live guest: scala-cli's `run` execs the board JVM from the
launcher (tgid 279, Defect A's path) and the board never listens. The console shows, in order, `[RUN-REFUSED] thread pid=347
of tgid=279 holds a replaced root` (the first-run guard ending a thread cloned just before the group's execve),
`[TRAMP-MISMATCH] THREAD_PID_MAP=.. but table scan found 279` repeating, then `[Fault] PageFault rip=0x30068341 code=0x15`
(instruction fetch from a mapped non-executable page, `rax=-14` after a futex, so right after a syscall return) and
`slot 4 killed (SIGSEGV)`. This is the `clone`/`execve` window the Defect B section lists as open: closing it needs the two to
exclude each other, plus `THREAD_PID_MAP` kept consistent across the exec. `refdemo.py` takes `BOARD_WAIT_ITERS` to reach its
own diagnostics sooner.

Gates run on this change (2026-11-20): `scripts/gate.py` builds (16/16), host tests, Denver/shanghai/taoxichuan gateprobe
boots all pass (getcpu and threads_128 are the listed xfails); `check_budgets` fails on growth in crates this change did not
touch; the main-kernel boots pass once `hello.elf` is built; the Clojure `--with-jvm` gate could not run (no Docker for
`build-image.sh`).

### Cause found and fixed: a non-leader `execve` left the calling thread on the old root (2026-11-20, last)

The `clone`/`execve` race was a wrong diagnosis. `proc::exec_strs` gives the new root to the group leader's `Process`, but when
a non-leader thread calls `execve` it continues as the group's only thread on a `Process` of its own, and that record's
address-space view stayed the old root. `clone_thread` reads the parent's view, so every thread the new image created (the
board JVM's) started on the freed root: `[RUN-REFUSED] ... holds a replaced root`, then the `code=0x15` fault. Fix: `exec_strs`
points the caller's own `Process` at the new root too (`disown_space(slot_pid(caller), root)`). `kill_siblings` also rescans
until a pass finds no live sibling (a `clone` finishing in the lock-released wait could add one), which is correct but was not
this bug.

Verified: `refdemo.py --scala-direct` passes in full, 18 ok / 0 failed, including `scala-cli run` serving the board and the
reference acceptance suite from the Python box. No `RUN-REFUSED`, no `[Fault]`. Two `TRAMP-MISMATCH` lines remain per run
(the thread-id map disagrees with the table scan and the trampoline falls back); harmless here, not investigated.

## Reproduce

```sh
cargo build -p denver --target x86_64-unknown-none --release --features containers,codex
export ECO_KERNEL=$PWD/target/x86_64-unknown-none/release/denver
python3 overlays/ecosystem-demo/execthread-probe.py --out /var/tmp/akuma-eco/denver-ready          # Defect A
python3 overlays/ecosystem-demo/refdemo.py --ref ../akuma-demo-reference-impl --scala-direct \
  --git-server-bin /var/tmp/akuma-eco/denver-reference-git-server --out /var/tmp/akuma-eco/denver-ready   # B and C
# triple fault evidence:
ECO_ACCEL=tcg ECO_CPU=max ECO_QEMU_EXTRA='-d int,cpu_reset -D /tmp/q.log' ...
```

`--scala-direct` runs the scala-cli jar itself instead of the launcher workaround.

## Still open

Nothing known for `scala-cli run` as of 2026-11-20 (the full demo passes). The launcher's Denver special case and `--scala-interpreted` are retired (the image was rebuilt; the default `refdemo.py` run passes 18/0 with `scala-cli run` execing the application itself). The `TRAMP-MISMATCH` warning remains.

## Background

`DENVER_BOX_WIRING.md` (box roots, the second-box panic), `EXEC_FROM_THREAD_DETACH.md` (the main kernel's fix of Defect A's
class), `TOKIO_KERNELS_LOOPBACK_STARVATION.md`.
