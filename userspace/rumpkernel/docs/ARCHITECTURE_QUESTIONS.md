# Rump port — open architecture questions (user-raised, to resolve)

Captured so they don't get lost. These are about *how a box's programs reach the
rump NetBSD TCP/IP stack* — the layer above "does the stack run" (which is green:
`rump_init()` boots on Akuma).

## Q1 — Can an unmodified `curl` use the rump stack, since "all syscalls go through rumpuser"?
**Answer: ✅ PROVEN (2026-06-22, in container) — but not the way the question
assumes, and not for *every* binary.** `rumpuser` is the rump kernel's *downcall*
interface to the host (malloc/mmap/pthread/clock) — it is NOT a shim that reroutes
a program's `socket()`/`connect()` into the rump kernel. A program reaching the
rump stack needs an *interposer*. We built a minimal one — `rumpuser/hijack.c` → a
single `LD_PRELOAD` `.so` that statically embeds the whole rump stack + our
rumpuser, brings up `virt0` in a constructor, and interposes libc
`socket/connect/send/recv/...` onto `rump_sys_*` (fd-offset routing; Linux→NetBSD
sockaddr + socket-type translation). With it, **unmodified `curl 8.14.1` did a full
HTTP GET over the NetBSD stack** (`docker-hijack-demo.sh`): ARP → SYN/SYN-ACK/ACK →
GET → 200+body → teardown, all counted at the virtif seam (tx=7/rx=7 frames), body
returned. smoltcp has no virtif, so the counters are the proof.

**Caveat — which binaries work:** the interposer only catches libc calls made
through the PLT. `curl` (and `nc`) do socket I/O via *directly-called* `send`/`recv`/
`read`/`write`, so they're interposable. **`busybox wget` does NOT work** — it wraps
the socket in a `FILE*` (`fdopen`), and **musl stdio flushes via inline `writev`/
`readv` syscalls that bypass the PLT**, so `LD_PRELOAD` can't catch them (they hit
the host kernel with the offset fd → EBADF). This is a fundamental musl property,
not a bug in the shim. For stdio-based binaries the answer is kernel-routing (Q3
below) or a syscall-level intercept.

## Q2 / Q3 — Don't we have a Linux syscall interface on rump (`libsys_linux`)? Isn't that what runs unmodified Linux binaries?
**Right about what it's FOR; it is NOT built yet.** `libsys_linux`
(`sys/rump/kern/lib/libsys_linux`) is the rump component that translates the
**Linux syscall ABI → NetBSD** inside the rump kernel. With it, the rump kernel can
*service* Linux-ABI syscalls. But buildrump only adds it for i386/amd64/evbearm/
evbppc — for our `evbarm64` the emul dir resolved empty, so **`librumpkern_sys_linux.a`
was not built** (verified: not in `obj/dest.stage/usr/lib/`).

Crucially, `libsys_linux` is the *translation table*, not the *delivery mechanism*.
It still needs something to take the box program's native syscall and hand it to
rump. That mechanism is one of Q4/the routing options below.

## Q4 — Aren't we supposed to preload the rumpuser library when running something in the box?
**Yes — this is the standard rump answer and it IS viable on Akuma.** (Correction:
Akuma *does* have dynamic linking — apk installs dynamic musl ELFs with an ELF
interpreter; my earlier "static-only" claim was wrong. The `-static` requirement
was only for *our own* tcc-built binaries that lacked an interp.) The library you
preload is **`librumphijack`**, not `rumpuser` (rumpuser is the kernel's host
downcall layer, not an interceptor). `LD_PRELOAD=librumphijack.so` intercepts libc
`socket/connect/read/write/...` and forwards them to `rump_sys_*`. Two sub-models:
- **in-process**: link `librump*` + `rumpuser` + `librumphijack` into one process;
  hijack redirects libc→`rump_sys` in the same address space. Purpose-built host,
  but runs the *app code* unmodified.
- **sysproxy**: a `rump_server` daemon owns the stack; client processes
  `LD_PRELOAD=librumphijack` + `RUMP_SERVER=unix://…` proxy their syscalls to it via
  `rumpclient` + the `sp_*` hypercalls. This runs a **truly unmodified** `busybox
  curl`. Cost: build `librumphijack`/`librumpclient`, and **implement the `sp_*`
  hypercalls in our rumpuser** (currently stubbed) + run a rump server.

So the preload path is real here. The open choice is preload+hijack (userspace,
needs sp_* + hijack libs) vs. kernel-routing (below, no preload).

## Two real designs for running unmodified binaries
**(A) Preload + hijack (userspace).** ✅ **demonstrated in-process** (no server, no
`sp_*`): `rumpuser/hijack.c` is a single `LD_PRELOAD` `.so` embedding the rump
stack; its constructor `rump_init`s and brings up `virt0`, and it redirects the
app's libc socket calls to `rump_sys_*`. Unmodified `curl` fetched a page over the
stack (`docker-hijack-demo.sh`). The classic sysproxy variant
(`LD_PRELOAD=librumphijack` + a `rump_server` over `sp_*`) would run a *separate*
process and handle stdio binaries too, at the cost of building the hijack/client
libs + un-stubbing the `sp_*` hypercalls + per-call proxy overhead. The in-process
variant we built only works for binaries whose socket I/O is interposable (curl/nc,
not musl-stdio binaries like busybox wget — see Q1).

**(B) Kernel-side routing (Akuma-native).** Akuma **is** the kernel hosting the
box, so it owns the box's syscall entry — it can forward a box's network (or, with
`libsys_linux`, *all* Linux) syscalls into that box's rump instance via `rump_sys_*`
with no preload at all. This is plan §10.2 generalized:
- minimal: route only AF_INET socket syscalls to rump (per-box `stack=smoltcp|rump`).
- maximal: build `libsys_linux` (needs aarch64 portability work) + route the box's
  Linux syscalls to rump for full unmodified-binary support.
Pro: no preload, no per-call proxy if in-process. Con: touches the hot syscall path.

Until either exists, the proof path is **link the client against rump**
(`rump_sys_*`) — hence the `sic` capstone (`acceptance/11_netbsd_rumpkernel_irc.md`).

**(C) Per-process ABI personality — the chosen long-term direction (future).**
Rather than translate a foreign (Linux/musl) binary's calls, run an **actual
NetBSD binary** and give Akuma a **swappable, per-process syscall table** chosen by
the **ELF loader** from the binary's ABI note — exactly how production kernels do
binary compat (NetBSD `struct emul`, FreeBSD `sysentvec`, Linux `personality()`).
A NetBSD aarch64 ELF traps `SVC` as normal; Akuma dispatches through *that
process's* NetBSD syscall table, whose handlers carry NetBSD semantics and route to
rump (network → the box's rump TCP/IP, VFS → rump VFS or Akuma, etc.). **Zero
translation** (the binary and the table are both NetBSD), no LD_PRELOAD, no rumprun
linking. Why it wins: it unlocks **pkgsrc** — the entire prebuilt NetBSD/aarch64
package set runs on Akuma unmodified. This is deferred (post-M1); see
IMPLEMENTATION_PLAN §10.5 for the concrete code-level plan. The shim (A)/sic path
gets us to M1; (C) is the real end state for "run NetBSD software on Akuma."

**(D) frankenlibc — a rump-backed libc (reference; future).**
`https://github.com/justincormack/frankenlibc` (Justin Cormack). musl + a rump
kernel, where libc's **syscall stubs** are wired to rump (with a policy for which
calls go to rump vs. the host); ships a loader to run programs in that environment.
It's the mature form of our `hijack.c`, but one layer lower — at the libc
syscall-stub layer, not LD_PRELOAD. That placement sidesteps the two walls we hit:
- the **musl-stdio** problem (Q1): stdio's inline `writev`/`readv` are themselves
  rump syscalls, so even `busybox wget` works (no PLT to bypass).
- the **mixed-fd `select()`** problem (the interactive-sshd blocker): one coherent
  libc decides per-call rump-vs-host, so a `select()` over {rump socket, host pty}
  is handled internally instead of split across an LD_PRELOAD seam.
So it's a 4th option between (A) crude LD_PRELOAD and (C) the kernel syscall table:
a **userspace rump-backed libc** — full ABI, runtime, no kernel change. Also a
codebase to lift the hard parts from (fd split, syscall router, rump bootstrap).
Caveats to check before relying on it: aarch64 support, and whether it can ride our
existing `librump*` build vs. building its own.

**Our own programs are a 5th, cleaner case.** Akuma's first-party binaries (e.g.
`userspace/sshd`) don't use libc sockets directly — they go through **`libakuma`'s
net abstraction** (`net.rs`: `socket/bind/listen/accept` → Akuma syscalls). For
those, the right integration is a **libakuma rump backend**: a selectable net
backend whose calls are `rump_sys_*` (built against the rump SDK), so any
libakuma-net program can choose the NetBSD stack without hijack or relinking app
logic. This is the long-term answer for our stack; `librumphijack`+`LD_PRELOAD`
stays the route for **unmodified third-party** binaries (dropbear, busybox).
(Relates to [[libakuma_needs_restructure]] — the backend seam wants the cleanup.)

---

## Debugging note (separate from the above) — `ifcreate` hang  ✅ RESOLVED 2026-06-22
**Fixed.** Root cause: `rumpuser_clock_sleep` didn't release the rump CPU around its
`nanosleep`, so the hardclock thread held the single CPU through every tick and
starved the main lwp (parked in the scheduler slowpath `cv_wait_nowrap`) — the
user's "missed delivery" / lost CPU handoff. Fix = the `rumpkern_unsched/sched`
wrap on `clock_sleep` (+ on contended mutex/rwlock and the cv waits). After that,
`docker-net-test.sh` reaches PASS: `virt0` up, `10.0.0.2/24`, `rump_sys_socket` OK.
Historical detail below.


After the rumpuser scheduler-wrap fix (cv/mutex/rwlock now release the rump CPU
before blocking), `docker-net-test.sh` advances from a hard hang to: `rump_init()`
= 0, then the rump **hardclock thread ticks fine** (clock_sleep loop), but
`rump_pub_netconfig_ifcreate("virt0")` still doesn't return.

Refined finding (loose/torn-tolerant grep — exact counts lie because the trace
tears under concurrent unsynchronized `dprint`): `component_unschedule`,
`component_schedule`, and the RX thread's `component_kthread` each fire **once**,
and there are **no backend errors** (`can't open`/`TUNSETIFF`/`poll error` all 0).
So **`VIFHYPER_CREATE` completed** — tap opened, RX thread up, CPU reacquired. The
hang is **after** that, in the post-create attach path: the main thread hits a
single wrapped `cv_wait` (the only one in the whole run) and parks; thereafter only
the clock thread runs and **no further `cv_signal` arrives**. So the user's **"main
thread missed delivery?"** looks right — the main lwp is parked on a cv whose
wakeup never comes (lost wakeup, or the signaller thread never runs).

NEXT DEBUG STEP: replace the torn trace with a **locked, thread-ID-stamped** trace
(serialize `dprint` behind a pthread mutex; prefix each line with `pthread_self()`
+ a sequence #) so we can see which lwp parks on which cv and which thread should
signal it. Then inspect the rump `if_attach`/config path for the cv the main thread
waits on (likely a workqueue/config-thread completion).
