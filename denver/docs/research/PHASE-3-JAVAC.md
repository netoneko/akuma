# PHASE-3-JAVAC — a synchronous fault reaches the JVM's own SIGSEGV handler (G13)

Session 18 (continued), 2026-11-14. The operator's top priority: inside the guest,
`java -m jdk.compiler/com.sun.tools.javac.Main -version` (the entry point — `jdk.compiler`
is carried, only `bin/javac` is missing) dies with
`[Fault] ring3 PageFault rip=0x6000118848e3 cr2=0x8 code=0x4` — faulting bytes
`44 8b 52 08` = `mov 0x8(%rdx),%r10d` with rdx=0, **in the JIT code cache** (`rwxs`
mapping); slot 5 killed (SIGSEGV), kernel d4acb339. This gate names the cause from the
console first (R8), then fixes it behind an off-by-default feature (R6). **Done means:
inside the guest javac compiles a real `.java` file (the staged `Workload.java`) and the
class it produces runs.**

## The cause, named (D72)

The kernel's ring-3 fault path (`akuma-tokio-kernel-amd64::fault_trap_locked`) tries
lazy-fill, stack-grow, and COW, and if none resolves **kills the task outright**. The
kernel's signal *decision* layer (`akuma-tokio-kernel-core::signal`) and the machine
half's async delivery (`child.rs` `push_handler` at syscall return, driven by
`kill/tkill`-raised pending signals) exist — `rt_sigaction` (13) is served (the
JAVACLASSLIB census shows `nr=13 → 0`) — but **no fault-generated signal is ever
delivered**: a synchronous fault never consults `signal_actions`. HotSpot is
trap-and-handle: implicit null checks (the `mov` through rdx=0 above) and safepoint
polling are faults **by design**, expecting the kernel to deliver SIGSEGV to its own
`SA_SIGINFO` handler, which reads `siginfo`/`ucontext`, patches the saved `%rip` to its
exception stub, and returns (raising NullPointerException / parking at the safepoint).
Without delivery, every such trap is a death.

## R9 precedents (what was searched and found)

- **`crates/akuma-kernel-amd64/src/signal.rs` (the metal kernel)**: a complete fault→
  signal path — `deliver_fault_signal` / `build_frame` / `install_handler_frame`, real
  `siginfo` (`si_code` MAPERR/ACCERR/SI_KERNEL, `si_addr` = cr2), `ucontext` with the
  register file, `SA_ONSTACK` via `get_sigaltstack`, the blocked-mask rule (a blocked
  fault cannot be deferred — the instruction re-executes; kill), the red zone, the
  entry-flags rule. Validated on metal by Chromium/crashpad, Go's `sigpanic` (the
  `AMD64_FORK_VS_THREADS_SEGV.md` startup crash, `docs/archive/`), V8 wasm traps, and
  Rust's stack-overflow handler. **This port is 1:1 that design.**
- `docs/archive/` + `docs/reference/` grep for sigaction/sigreturn/signal-frame: the
  metal signal work above is the only fault-delivery precedent; the tokio stack's
  async delivery (`core::signal` + `child.rs`) is the frame-push precedent this module
  shares `user_ok`/`copy_to_user`/`FxArea` plumbing with.
- **One metal bug found and NOT inherited** (recorded for a later metal pass, out of
  this gate's scope): metal's `SigContext` is `#[repr(C)]` but carries
  `rbx, rdx, rax, rcx, rsp` in the greg slots where Linux/glibc puts
  `rsp, rbx, rdx, rax, rcx` (`REG_RSP`=11 → byte 88; metal: byte 120). `rip`@128,
  `eflags`@136, `fpstate`@184 agree — which is why rip-only handlers (HotSpot's null
  check, Go) worked on metal — but any `REG_RSP`/`REG_ERR`/`REG_CR2` read on metal got
  the wrong field. This port uses the Linux/glibc field order the musl JVM indexes by
  (divergence 1, module header).

## Decision D73: port metal's path behind `fault-signal` (off by default, relayed 1:1)

`crates/akuma-tokio-kernel-amd64/src/faultsig.rs` (new, ~490 lines):

- **`deliver(t, cr2, code)`** from `fault_trap_locked`'s ring-3 arm, before the kill:
  `Kind::PageFault` only, slotted real task, `SIGSEGV` action is `UserFn`, not blocked,
  `SA_RESTORER` present — else `false`, and the kill proceeds **unchanged** (the
  unfeatured behavior is byte-identical; the `si_code` rule: `code & 1 == 0` →
  MAPERR(1), else ACCERR(2); `si_addr` = cr2).
- The frame is metal's `rt_sigframe` (pretcode=`sa_restorer`, ucontext, siginfo;
  `rsi`/`rdx` carry the pointers when `SA_SIGINFO`) with the FPU image 64-aligned above
  (`fpstate` points at it), written through the same `user_ok`/`copy_to_user` the async
  arm uses; red zone 128 below the interrupted `%rsp`; `SA_ONSTACK` honored
  best-effort (`get_sigaltstack` — the guest's sigaltstack(131) is ENOSYS today, so no
  registered stack ⇒ the interrupted stack, metal's fallback); install = Linux's
  `setup_rt_frame` shape (`rip`, `rsp`, `rflags=0x202`, `rdi`=sig, `rsi`/`rdx`
  pointers — the rest of the trap-saved file untouched).
- **`sync_sigreturn`** hooks the head of `sigreturn_arm`: a per-slot LIFO of live frame
  bases matches the returning `%rsp − 8` (where the restorer's `ret` lands); a match
  restores **from the ucontext** — the whole point: HotSpot edits
  `uc_mcontext.gregs[REG_RIP]`, so the ucontext, not a private block, is the restore
  source — with the same `USER_TOP`/rflags/FP-image sanitization the async arm applies,
  the restored mask, and the interrupted `rax` as the syscall's answer. No match ⇒ the
  async path proceeds untouched (zero behavior change for kill/tick signals).
- **Divergence 2** (module header): the async frame format already exists, so a fault
  frame needs identification at `rt_sigreturn` — the LIFO above, not stack magic.
- **Recorded limit**: `%rcx`/`%r11` cannot cross the restore (the syscall exit is
  `sysret`; both are consumed). The async path already clobbers them across every
  handler return today, and guest programs tolerate that; if javac names one, the
  datum says so.
- **R6 evidence**: the default build's `.text` is **byte-identical** to HEAD's
  (objcopy-compared, md5 `eecd14dafcceb70b385873d45a66083c`); clippy clean on
  `akuma-tokio-kernel-amd64` under the gate feature set. Feature relayed 1:1
  `denver` → `akuma-jvm-kernel` → `akuma-tokio-kernel` → `akuma-tokio-kernel-amd64`
  (the `vsyscall`/`getcpu` pattern).

D74 — a `#[repr(C)]` lesson (recorded-not-judged): the first draft omitted `repr(C)`
on the layout structs and the const offset assertions caught it at compile time
(`offset_of!(SigContext, rsp)` came out 152, not 88 — repr(Rust) reordering). The
assertions earned their keep before the first boot.

## The probe (D75) — `denver/probe-javac.sh`, arms in order

Kernel rig: `/root/denver/w/denver-faultsig` (the denver-getcpu feature set + 
`fault-signal`), disk `/root/denver/w/denver-faultsig.img` (fresh copy of
`denver-jdk.img` + debugfs-staged `/work/Workload.java` and `/work/sigprobe`, the
static-musl signal probe built `gcc -static -O2` like `jvmprobe`). Identity arms as in
every gate (uname says denver, no older rig hash), tokioprobe, getcpu arm; the reader
is the pipe arm only (L2 rule); alarms bound every dispatch.

1. **SIGPROBE** (answers the operator's question 2 without the JVM): installs a
   `SA_SIGINFO` SIGSEGV handler, writes through NULL, and the handler — after recording
   `si_signo/si_code/si_addr` and the interrupted `rip`/`rsp`/`cr2`/`err` from the
   ucontext — patches the saved `%rip` to the instruction after the faulting `mov` and
   returns; `rt_sigreturn` resumes there. PASS = delivered + the edits took effect
   (`rax == 7` at the post-fault site proves the patched context is what ran). A kill
   instead is a decisive FAIL (delivery does not exist).
2. **JAVAC -Xint** (question 3, JIT-specificity): `java -Xint -m
   jdk.compiler/com.sun.tools.javac.Main -version` — recorded observation; on the
   unfeatured kernel the JIT death already implies the interpreter path was never the
   suspect, and this arm bounds the runtime cost too (alarm 90 s).
3. **THE DATUM**: `java -m jdk.compiler/com.sun.tools.javac.Main -version` — exit 0 and
   no `task killed` in the console window; the old rip `0x6000118848e3` must NOT appear.
4. **DONE** (the gate's bar): `java -m jdk.compiler/com.sun.tools.javac.Main -d /work
   /work/Workload.java` (javac compiles the staged real source **inside the guest**),
   then `java -cp /work Workload` — the produced class runs: all four surface markers +
   `WORKLOAD-ALL-PASS` through the pipe arm (the JAVACLASSLIB markers, now from
   guest-compiled classes).

PASS = arms 1, 3, 4 (2 is a recorded observation). Every failure names its datum into
the log; REDs land with the class-load/diagnostic trace like the workload gate's.
