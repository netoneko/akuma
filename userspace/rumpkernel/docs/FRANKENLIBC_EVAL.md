# frankenlibc evaluation (parked — revisit when sshd-over-rump is the priority)

Cloned `https://github.com/netoneko/frankenlibc.git` (our fork of justincormack's)
to `userspace/rumpkernel/frankenlibc/` (git-ignored for now; → submodule if adopted).
**Verdict: promising, but a sizable adoption project — deliberately parked.**

## What it is
A build tool that fuses **NetBSD libc + a rump kernel + a tiny platform libc** into a
**single `libc.a`** you link apps against, so the app's syscalls go to rump instead
of the host. Ships `rexec` ("rump exec") to run a binary with files / block / **tap
devices** passed in, plus a compiler wrapper. It's the mature, full-ABI form of our
hand-rolled `hijack.c`.

## Why it's attractive for us (solves walls we hit)
- **Full libc syscall layer**, not LD_PRELOAD → musl-stdio's inline `writev`/`readv`
  go to rump too, so `busybox wget`-class binaries work (the Q1 wall).
- **One coherent libc decides rump-vs-host per call** → a `select()` mixing a rump
  socket and a host pty is handled internally — the interactive-**sshd** blocker.
- **Fiber-based** rump (`RUMP_CURLWP=hypercall RUMP_LOCKS_UP=yes
  RUMP_KERNEL_IS_LIBC=1`): cooperative single-thread, *uniprocessor* locks. Sidesteps
  the pthread scheduler-wrap we fought in `rumpuser-akuma` (clock_sleep/cv/mutex).

## aarch64 status — better than "WIP" suggests
The hard arch primitives are **present and look correct**:
- `platform/linux/aarch64/syscall.s` — standard `svc 0` stub (x8=nr, x0–x6 args) —
  **exactly Akuma's ABI** (Akuma runs Linux/musl aarch64 binaries via `svc`).
- `franken/ucontext/aarch64/swapcontext.S` — full callee-saved fiber switch
  (x19–x30, sp, d8–d15). `makecontext.c`/`ucontext.h` present.
- File set mirrors the working `arm` target. So aarch64 is a real (if untested) port;
  the remaining work is build/integration/test, not writing arch primitives.

## Akuma fit
- The **`linux` platform target** is the closest match — Akuma presents a Linux-ish
  `svc` syscall surface, so `platform=linux arch=aarch64` likely runs the *platform*
  syscalls (mmap/clock/fiber needs) on Akuma with little change.
- `rexec`'s **seccomp/Capsicum sandbox is Linux/FreeBSD-specific** → drop/ignore on
  Akuma. The tap-as-NIC plumbing maps to our `/dev/net/tap0`.
- It builds its **own rump** (bundled `buildrump`, own `src/`, `RUMP_KERNEL_IS_LIBC=1`)
  — it does **not** reuse our `librump*.a`. Adoption is a parallel, self-contained
  build (likely a container cross-build like ours), not a drop-in against our libs.

## Cost of adoption (why it's parked)
1. Get aarch64 building (`platform=linux`, cross from a container) and fix WIP gaps.
2. Akuma-adapt: confirm `platform=linux` runs on Akuma, or add a thin `platform/akuma`;
   wire `rexec`'s tap to `/dev/net/tap0`; drop the seccomp sandbox.
3. Build dropbear (or our payload) with franken's compiler wrapper → all its syscalls
   hit rump → interactive sshd over the NetBSD stack, no LD_PRELOAD, no mixed-fd wall.

## Fiber vs pthread — and why fibers are a *plus*, not a gap to fill
librumpuser has two upstream flavors: **`rumpuser_pth`** (real pthreads, SMP-capable)
and **`rumpfiber`** (cooperative, single host thread, `RUMP_LOCKS_UP=yes` =
uniprocessor locks). Our `rumpuser-akuma` is the **pthread** one; franken uses the
**fiber** one. Almost every bruise this session came from the pthread flavor — the
`clock_sleep` starvation, the `cv`/`mutex`/`rwlock` unschedule dance, "single rump CPU
held across a host blocking call." **All of that is a non-problem under fibers:** one
thread, cooperative scheduling, "blocking" just yields the fiber — nothing to preempt,
no CPU to starve. rump in `LOCKS_UP` mode is uniprocessor anyway, so pthreads bought us
complexity, not capability.

So "patch in pthreads later" splits into two very different things — don't conflate:
1. **pthread *API* over fibers** (threads-as-fibers): cheap, *keeps* all the fiber
   benefits. The only kind worth adding, and only if an app actually calls
   `pthread_create`. Our targets (sshd, sic, curl, busybox) are single-threaded or
   `fork`-based, so likely not even needed.
2. **Real preemptive pthreads + SMP rump** (`RUMP_LOCKS_UP=no`): re-imports exactly
   the pain we escaped; only pays off for true multicore parallelism *inside* the box.
   A deliberate, justified step — never a default "later."

Default: **stay fiber.** Adopting franken's fiber librumpuser means setting
`rumpuser-akuma` (pthread) aside *for that path* — not wasted (it proved the stack
boots+networks on Akuma; the kernel `/dev/net/tap0` + blocking-read are reusable under
either model).

## Recommendation
Park it. It's the **cleanest long-term answer** for "run unmodified binaries
(sshd, busybox) on the rump stack" and the natural pair to the cluster vision — but
it's a multi-session adoption. When sshd-over-rump becomes the priority, the lighter
alternative to weigh against it is the **§10.2 kernel socket-routing** (smaller,
Akuma-native, but only reroutes sockets — franken reroutes the whole ABI). Transport
underneath either is already proven (`rumpserver`).
