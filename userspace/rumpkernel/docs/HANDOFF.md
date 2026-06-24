# Rump-kernel port — session handoff

**Read this first to resume.** Single source of truth for picking the work back
up. Detail docs: [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) (the full
plan + §10 forward architecture), [PHASE01_BUILDRUMP.md](PHASE01_BUILDRUMP.md),
[PHASE2_RUMPUSER.md](PHASE2_RUMPUSER.md), [PHASE3_KERNEL_TAP.md](PHASE3_KERNEL_TAP.md),
[DEV_ZERO.md](DEV_ZERO.md).
Post-M1 direction docs: [RUMP_SYSPROXY.md](RUMP_SYSPROXY.md) (**the committed next
architecture** — per-box rump server), [RUMP_PLUS_HERD.md](../../../docs/RUMP_PLUS_HERD.md),
[ARCHITECTURE_QUESTIONS.md](ARCHITECTURE_QUESTIONS.md) (unmodified-binary paths),
[FRANKENLIBC_EVAL.md](FRANKENLIBC_EVAL.md) (parked). Demos:
`acceptance/11_netbsd_rumpkernel_irc.md` (ssh-in + IRC), `acceptance/12_netbsd_binary_compatibility.md`.

Goal: **M1 DONE** (2026-06-22 — NetBSD stack in an Akuma box, DHCP + HTTP to the host).
**🏆 M2 DONE (2026-06-23) — the SHARED-STACK box via kernel-as-sysproxy-client:**
unmodified static binaries in a `stack=rump` box have their AF_INET routed by the
KERNEL to a shared boxed `rump_server`, validated end-to-end with **`curl` (HTTPS-by-IP)
AND `sic` holding a live IRC session on `#rumpkernel` (OFTC) over the NetBSD stack**.
See "M2 ACHIEVED" below + RUMP_SYSPROXY.md ("Phase B" / IRC). **DNS now works over
the rump stack (2026-06-23)** — `curl http://example.com` resolves + fetches HTTP 200
through NetBSD (`bind` + `sendto`-with-dest + `recvmsg` marshaling added to
`src/rump_proxy.rs`). **Latency — ✅ MAJOR WIN (2026-06-24): the cooperative FIBER
backend makes curl-over-rump ~3.85× faster (16.3s vs 62.8s) on 1 OS thread instead
of 19, futex storm gone. See `FIBER_HANDOFF.md`.** Remaining: residual ~1s/syscall
(event-driven channel wakeup is the next lever) + robustness (task #9) + boot self-tests.

### Session 2026-06-25: `meow` over the rump stack + built-in-shell pty regression fixed

**1. `meow` (LLM client) runs in the box over the NetBSD stack.** Another agent
added meow's `linux-net` feature (standard Linux socket/DNS syscalls +
`libakuma/linux-abi` for `getpid`/clock, replacing Akuma-custom `RESOLVE_HOST 300`
/ `UPTIME 319`) so the kernel sysproxy routes its traffic through rump. Built with
`cargo +nightly build --release -Zbuild-std=core,alloc --target
aarch64-unknown-linux-musl --features linux-net`, staged at
`bootstrap/srv/rumpbox/bin/meow`, run non-interactively via SSH:
`box use rumpnet -i /bin/meow -N -m qwen3.5:0.8b -c 'say hi'` → **"hi there"**,
with `connect fam=2 port=11434 ip=10.0.2.2` (host ollama) + streamed `recvfrom` on
a `RumpSocket` over the NetBSD stack (~42 TPS). Also verified the SAME binary on
the **native smoltcp stack** (box 0, run by path so it's not sysproxy-routed): "Hello!"
at ~81 TPS — so the Linux-syscall build is a viable universal build (Akuma implements
the Linux socket ABI natively, like the static curl). meow is NOT built linux-net by
default: `build.sh` targets `aarch64-unknown-none` with Akuma-custom syscalls. See
`userspace/meow/README.md` (now documents the `linux-net` feature + a known limitation:
interactive meow over `ssh -p 2223` into the box flickers / drops — the slow
rump-proxied session + task #9 wedge).

**2. Built-in `:2222` shell interactive sub-shells FIXED (regression from the pty work).**
`busybox sh` / `toybox sh` launched from the in-kernel built-in shell on `:2222`
hung (no prompt/echo, parked in `ppoll`), while single commands and interactive
`meow` worked. Cause: the `set_terminal(pty)` spawn change (commits `fd3fac3` /
`0126118`, made for the custom userspace sshd's piped-command bridge) defaulted the
spawn wrappers to `pty=false`, but the built-in shell uses the same path and its
`:2222` session is always a real pty. Fix: `execute_external_interactive`
(`src/shell/mod.rs`) now spawns with `pty=true`
(`spawn_process_with_channel_ext(..., 0, true)`). busybox/toybox `sh` now run as
real ttys (termios `ioctl`s, prompt, `ppoll`); meow still works (kernel honors
`TCSETS` → meow goes raw itself); custom-sshd path untouched. Full write-up:
`docs/BOX_PTY_INTERACTIVE_SHELL.md` ("Follow-up (2026-06-25)"). UNCOMMITTED.

### Session 2026-06-24 (latest): DNS + HTTPS inside the box with static curl — ✅ WORKING

**Goal (acceptance/11):** make DNS resolution + HTTPS work inside the rumpnet box
with the unmodified static `curl` (`bootstrap/bin/curl`, mbedTLS). **DONE — proven
6/6 via `box use`:** `box use rumpnet -i /bin/curl -sS -i https://example.com` →
`HTTP/1.1 200 OK` from Cloudflare, every run (full TLS handshake + body over the
NetBSD rump stack). Kernel trace confirms the whole path is rump: DNS UDP
(`socket DGRAM|NONBLOCK` → `bind` → `sendto :53` → `recvmsg -> 61`,
`example.com` → `172.66.147.243`), then TCP `connect ... port=443 -> OK`, TLS, body.

**Two fixes — both pure data staging into the BOX rootfs (`bootstrap/srv/rumpbox/`),
no kernel/code change. UNCOMMITTED (user commits):**
1. **`bootstrap/srv/rumpbox/etc/resolv.conf`** (was MISSING). The box runs in a
   fresh isolated SubdirFs root (`box_root = /srv/rumpbox`), so curl reads the
   BOX's `/etc/resolv.conf`, not the main root's. With it absent, musl's resolver
   defaulted to nameserver `127.0.0.1` → DNS queries went to the rump loopback →
   "Could not resolve host". Copied the main root's resolv.conf (`8.8.8.8` /
   `1.1.1.1`, reachable via the rump default route → SLIRP → internet).
2. **`bootstrap/srv/rumpbox/etc/ssl/certs/ca-certificates.crt`** (+ `etc/ssl/cert.pem`)
   (was MISSING). curl looks for `/etc/ssl/certs/ca-certificates.crt` (baked into
   the binary); without it mbedTLS failed before the handshake
   (`curl: (77) Error reading ca cert file ... PK - Read/write of file failed`).
   Copied the main root's bundle. After staging both, run a FULL `populate_disk.sh`
   (not `--bin-only`) so the box rootfs lands on the disk.

**🏆 Fork SIGSEGV — ROOT-CAUSED & FIXED this session (one-line kernel fix).** This
was THE blocker for the SSH-into-box path (`curl` forked by the box shell crashed
~90% with `[WILD-IA] ELR=0x0 x30=0x0`, RC139). It was a `[[fork_cow_tlb_asid_flush]]`
**kernel CoW/TLB bug — NOT busybox-specific, NOT rump-specific.** Isolated on plain
smoltcp box-0 via the diag sshds (`sshd_host.conf` busybox `:2323`, `sshd_diag_toybox.conf`
toybox `:4444`): a `/bin/{busybox,toybox} true` fork-stress crashed **37/40 (busybox)
and 32/40 (toybox)** — so both shells, no rump in the path. Instrumentation proved the
child enters EL0 with a **correct** context (`pc`/`sp`/`x30` valid, `spsr=0`) and only
crashes *after running*, branching to a zeroed return address — **post-fork stack
corruption, not a context-setup bug** (overturns the prior "context capture/zeroing"
theory). Root cause: `fork_process` demotes the parent's pages to RO for CoW
(`demote_range_to_ro`) but then flushed with `flush_tlb_asid(0)` — `tlbi aside1` for
**ASID 0 only**, while every user process runs under its **own non-zero ASID**
(`ttbr0 = (asid<<48)|l0`). So the parent's stale **RW** TLB entries survived → the
parent wrote *through* to the still-shared CoW page without faulting → clobbered the
child's snapshot (saved return addresses on the shared stack page) → child `ret`'d to
~0. ~90% (not 100%) because the next context switch's `activate()` does a full flush,
closing the window. **Fix:** `flush_tlb_asid(0)` → `mmu::flush_tlb_all()` (`tlbi
vmalle1`, all ASIDs) in the fork CoW path (`crates/akuma-exec/src/process/mod.rs`,
~line 1631). After the fix: busybox **0/40** and toybox **0/40** SIGSEGV, **0**
`WILD-IA` faults. (UNCOMMITTED — user commits. NOTE: vfork/clone variants don't call
`demote_range_to_ro` at all — separate follow-up if they show CoW issues.)
- **DNS cold-start flakiness (still open, minor).** The FIRST DNS query right after
  boot sometimes returns no answer (SLIRP/host-network warm-up); a second attempt
  resolves. Warm with one `box use ... curl` before relying on it. (Steady-state DNS = 6/6.)

The acceptance/11 capstone bar — DNS + HTTPS in the box on the NetBSD stack with an
unmodified static curl — is met via `box use` (6/6) and, with the fork fix, the
SSH-into-box path now forks cleanly too.

### Session 2026-06-24 (later): interactive PTY for the box shell + fork SIGSEGV lead (bug since FIXED — see above)

**SHIPPED (verified, UNCOMMITTED): the SSH-into-box shell is now a real
interactive terminal.** Was: no prompt/echo/line-editing, Enter (`\r`) never
terminated a command (`ls^M^M^M^M`). Root cause: the kernel's full line
discipline (`crates/akuma-terminal`: ICRNL, canonical, echo) was gated behind
`is_terminal`, and the box shell was spawned `is_terminal=false`. Fix = wiring
only, via Akuma's own SPAWN ABI: `SPAWN_FLAG_PTY` (arg6) + `libakuma::spawn_pty`
→ sshd's interactive `run_shell_session` uses it → `sys_spawn` decodes it →
`spawn_process_with_channel_ext(pty)` → `channel.set_terminal(pty)`. Scoped to
sshd's `pty-req` (NOT herd/box config — tty-ness is a session property, and a
per-box flag would wrongly mark `rump_server` a tty). Boot self-test
`test_spawned_child_pty_is_a_tty` PASSED. Live over rump: prompt/echo/editing/
Enter work; **curl PASSED** (`curl -H Host:ifconfig.me -L http://34.160.111.145`
from the box shell); **sic ran** interactively (caveat: `^C`/SIGINT through the
pty+bridge unconfirmed). Full writeup: **`docs/BOX_PTY_INTERACTIVE_SHELL.md`**.

**✅ RESOLVED 2026-06-24 (later) — intermittent fork SIGSEGV was a CoW TLB
flush-by-ASID bug. See the "DNS + HTTPS inside the box" session above for the full
write-up + fix (`flush_tlb_asid(0)` → `flush_tlb_all()` in fork's CoW path).** The
symptom: `busybox <applet>`/`wget`/`curl` SIGSEGV with the forked child resuming
later at **ELR(pc)=0 and x30=0** (`[WILD-IA] ... ELR=0x0 ... x30=0x0`), all other
GPRs/SP valid. *Historical note — the dead-end leads below were ALL ultimately wrong;
kept only as a cautionary record:* this session's instrumentation proved the child
enters EL0 with a fully CORRECT context (`pc`/`sp`/`x30` valid, `spsr=0`) via
`enter_user_mode(&proc.context)` and only crashes AFTER running — i.e. it was never a
context-setup/zeroing problem at all (the prior `get_saved_user_context` /
fake-IRQ-frame / `SPSR=0x20000000` theories were red herrings; the `0x20000000` came
from one early over-rump capture, but the clean smoltcp repro shows `SPSR=0x0`). The
real bug was post-fork stack corruption: the parent's stale RW TLB entries (its
non-zero ASID was never flushed) let it write through a shared CoW page and clobber
the child's saved return addresses. Same class as `docs/GO_FORK_EXEC_FIXES.md` /
`SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md` — those may share this root cause.

**Also (minor):** box rootfs `/srv/rumpbox/bin` lacks busybox applet symlinks,
so bare `ls` fails — use `busybox ls` or stage symlinks (main root gets them in
`populate_disk.sh`; the box doesn't). busybox `wget` DNS over rump returns "bad
address" (musl resolver path differs from curl's — separate).

---

## TL;DR status

| Piece | Status |
|-------|--------|
| Kernel `/dev/zero` prereq | ✅ done, boot self-test passes |
| Phase 3 — kernel `rump` feature: `/dev/net/tap0` raw L2 dev on 2nd NIC (`RUMP_NIC=1`, release-only) | ✅ done, verified on boot |
| `crates/akuma-rump` — host-testable tap orchestration + 14 unit tests | ✅ done |
| Phases 0/1 — `librump*.a` for aarch64-musl (full TCP/IP stack) | ✅ built (Linux container) |
| Phase 2 — Rust `rumpuser`: `rump_init()` returns 0 | ✅ **green** (container) |
| **`rump_init()` runs ON AKUMA** — NetBSD rump kernel boots in the VM | ✅ **GREEN 2026-06-22** |
| `rumpuser_component_*` family (scheduler bridge for virtif backend) | ✅ done, in `rumpuser/src/lib.rs` |
| Phase 4 — `librumpnet_virtif.a` (kernel driver `if_virt.o`) built | ✅ via `docker-build-virtif.sh` |
| Phase 4 — **container networking GREEN**: `virt0` up, IP assigned, `rump_sys_socket` OK | ✅ **2026-06-22** (`docker-net-test.sh`) |
| Phase 4 — `rumpuser` scheduler-wrap under concurrency | ✅ fixed (cv/mutex/rwlock + **clock_sleep**) |
| **Unmodified `curl` does HTTP over the rump stack** (container, real round-trip) | ✅ **2026-06-22** (`docker-hijack-demo.sh`) |
| DHCP over the rump stack (container, vs dnsmasq) | ✅ **2026-06-22** (`docker-hijack-demo.sh` RUMP_DHCP=1) |
| Akuma backend: `rumpcomp_user` over `/dev/net/tap0` (vs container TUN/TAP) | ✅ `rumpuser/rumpcomp_tap.c` |
| Kernel: **blocking `read()`** on `/dev/net/tap0` (`Tap{nonblock}`, no busy-wait) | ✅ `read_frame_blocking`; self-test updated |
| 🏆 **M1 — DHCP + HTTP to the host, rump in an Akuma box** | ✅ **DONE 2026-06-22** (`rumphttp` in a `RUMP_NIC=1` box) |
| **Inbound TCP server over rump, reachable from the host** | ✅ **2026-06-22** (`rumpserver.c`; host `:2223`→rump `:22`, banner+echo) |
| **🏆 M2 — kernel-as-sysproxy-client: unmodified binary's AF_INET → shared boxed rump_server** | ✅ **DONE 2026-06-23** |
| **`curl` HTTPS-by-IP over rump** (`-H Host:ifconfig.me http://34.160.111.145` → `87.71.13.205`) | ✅ **2026-06-23** |
| **`sic` IRC: live `#rumpkernel` session on OFTC over rump** (acceptance/11 capstone) | ✅ **2026-06-23** (`163.61.26.35:6667`) |
| **DNS over rump** — `curl http://example.com` resolves + fetches via NetBSD (`bind`+`sendto`-dest+`recvmsg`) | ✅ **2026-06-23** (`example.com`→`104.20.23.154`→HTTP 200) |
| **DNS + HTTPS in the box with unmodified static curl** — `box use rumpnet -i /bin/curl -sS -i https://example.com` | ✅ **2026-06-24** (`HTTP/1.1 200` from Cloudflare, **6/6**; needed `resolv.conf` + CA bundle staged in `/srv/rumpbox/etc`) |
| Phase 5 — herd autostarts `rumpnet` box (`--net --fd 3`, kernel attaches sysproxy channel) | ✅ **2026-06-23** (herd OWNS the rump_server; `restart=false`) |
| Rump SDK tarball (`bootstrap/archives/rump-sdk-aarch64-musl.tar.gz` → VM `/archives`) | ✅ `package-sdk.sh` (48 MB, 154 archives) |
| Akuma integration (libakuma, build-std core) | ✅ proven sufficient — stock host link runs on Akuma as-is |
| ⚠️ Per-syscall latency (~1s round-trip; rump pthread kthreads on 1 core) | ⏳ open — see "M2" + RUMP_SYSPROXY.md |
| **🏆 FIBER backend: curl over rump, ~3.85× faster, 1 OS thread** (`16.3s` vs `62.8s`; `clone=0 futex=0`) | ✅ **2026-06-24** — see `FIBER_HANDOFF.md` |
| ⚠️ Robustness: uninterruptible proxy syscalls, `kill` invalid-pid, client-slot wedge | ⏳ open — project task #9 |
| acceptance/11 — actual sshd on the rump stack | ✅ login/auth/shell-prompt over rump; ✅ **command round-trip FIXED (2026-06-24)** — was a kernel waitpid bug, not rump (see "SSH interactive bridge" below + `userspace/sshd/docs/INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md`) |
| NetBSD binary compat (pkgsrc) via per-process syscall table | 📋 future — `acceptance/12_netbsd_binary_compatibility.md` |

Branch `netbsd-rump-kernel-attempt-0`. **The M2 kernel/herd/sic changes are UNCOMMITTED** (the
user commits): kernel (`src/rump_proxy.rs`, `src/syscall/{proc,mod,poll}.rs`,
`crates/akuma-rump/src/{sysproxy,syscall_translation}.rs`, `src/syscall/net.rs`,
`crates/akuma-exec/.../types.rs` `RumpSocket`, the gated scheduler tweak in
`threading/mod.rs`), herd (`rumpnet.conf` + `restart` flag), and the `sic` submodule
(`userspace/rumpkernel/sic` recv-drain patch, uncommitted in the submodule).

### SSH interactive command bridge — ✅ RESOLVED (2026-06-24)

**UPDATE (2026-06-24, later): root-caused and FIXED.** The "shell spawns but emits no
output" symptom was **not** busybox stdin-exec and **not** rump/box-specific. It was a
kernel bug: `sys_waitpid`/`wait4`/`waitid` removed the child's stdout `ProcessChannel`
the instant they reaped the zombie, so the bridge — which checks `waitpid` before
draining stdout — found the channel gone and lost all buffered output (busybox flushes
stdio at `_exit` → lost everything; toybox writes incrementally → lost only its last
line). Fix: `reap_child_channel` keeps the channel until drained. busybox AND toybox now
round-trip commands over the bridge. Full writeup + tests:
**`userspace/sshd/docs/INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md`**. The original
investigation notes below are kept for history.

<details><summary>Original 2026-06-24 notes (output path PROVEN; busybox stdin-exec suspected — superseded)</summary>

Work on acceptance/11's **command round-trip** (`ssh -tt -p 2223` → type a command →
see output, over the NetBSD rump stack). Login, auth, shell-spawn, and the **output
direction are now proven end-to-end over rump**; what remains is busybox not executing
the piped commands — and that reproduces **outside the box too**, so it is NOT
box/rump-specific.

**Proven this session (the output path fully works):** with the box sshd's
`--shell /bin/hello` (a plain print-and-exit program, no stdin), `ssh -p 2223`
streamed its full output to the host client over the rump stack:
```
hello: started (PID 86, outputs=10, delay_ms=1000)
hello (1/10) ... hello (9/10)
```
So spawn → bridge → rump_server → SLIRP → host client (with `\n`→`\r\n`) is solid.

**Fixes made (kernel + userspace sshd + libakuma — UNCOMMITTED; each fixes a real
bug, verified in isolation; clippy clean, host tests 105/0, boot self-test added):**
- **Bridge deadlock (userspace `userspace/sshd/src/protocol.rs` `bridge_process`)** —
  the loop did a *blocking* `read_fd(stdout_fd)` before reading SSH input, so once
  busybox parked in `ppoll` on stdin (emitting nothing) the bridge blocked on stdout
  forever and never forwarded keystrokes (bridge waits on stdout, shell waits on
  stdin). Fix: set BOTH `stdout_fd` and the SSH socket non-blocking
  (`libakuma::set_nonblocking`) and poll both; only `sleep_ms(10)` when idle.
- **busybox interactive hang on `ESC[6n`** — every fd ≤ 2 returned success for
  `TCGETS`, so `isatty(0)` was always true and busybox started its line editor
  (cursor query). New `ProcessChannel::is_terminal` flag (default `true`; set `false`
  for channel-spawned children in `spawn.rs`); `term.rs` returns `ENOTTY` for the
  terminal ioctls when the channel is non-terminal. busybox now runs non-interactive
  (no `ESC[6n`; verified `is_term=false`). Boot self-test `test_spawned_child_not_a_tty`.
- **stdin was being cooked + echoed** — `fs.rs` Stdin read keyed canonical line
  discipline (echo, line-buffering) on `is_stdin_closed()` instead of terminal-ness,
  so a spawned child's *open* pipe stdin was treated as a tty and its input echoed
  back, corrupting the command stream. Fix: `is_pipe = is_stdin_closed() || !is_terminal()`.
- **Premature teardown** — the bridge `return`ed on `CHANNEL_EOF`/`CHANNEL_CLOSE`,
  dropping command output. Fix: keep draining the shell's stdout until the SHELL
  exits; only the client's EOF stops *input*. Also drain buffered SSH packets every
  iteration (a `CHANNEL_DATA` buffered during the handshake was never processed when
  the next `read` returned EAGAIN — a real input-delivery bug).
- **`CLOSE_CHILD_STDIN` syscall (326)** + `libakuma::close_child_stdin` — deliver
  stdin-EOF to the child on the client's `CHANNEL_EOF` so a shell reading a piped
  script stops waiting for input (mirrors the in-kernel sshd's `close_process_stdin`;
  `src/syscall/proc.rs::sys_close_child_stdin`, spawner-only + box-isolation checks).
  **GOTCHA fixed:** first picked 325, which collided with `MOUNT_IN_NS` (325) — and
  its dispatch arm came first, so every `MOUNT_IN_NS` (which mounts the box's `/proc`,
  needed for the bridge's `/proc/<pid>/fd/0` stdin writes) was hijacked → broke the
  box `/proc` mount. Now 326.

**The remaining bug (NEXT SESSION — first task):** busybox spawns, reads the piped
command + EOF, then **exits without executing or producing any output** — even for a
pure builtin (`echo X; exit`). Confirmed via kernel trace: `stdin-write pid=NN len=…`
and `close_child_stdin pid=NN` both fire (input + EOF delivered), but the child emits
zero `write`s. **This reproduces OUTSIDE the box** (smoltcp, no rump): the
`bootstrap/etc/herd/enabled/sshd_host.conf` diagnostic service runs `userspace/sshd`
on box 0 at smoltcp `:23` (host `:2323`) with `--shell /bin/sh`, and a connection
there shows the SAME empty-output behavior. So the bug is in the **userspace sshd ↔
busybox stdin/execution bridge** (or how busybox reads the channel), not the rump
path. Debug busybox's own syscalls next (does its `read(0)` return the bytes? does it
parse/execute? where does it exit?) — the `:2323` path is the fast repro (no rump).
(`/bin/sh` on every root is a busybox copy — argv[0] basename `sh` → ash; box 0's
`/bin/sh` was created from `bootstrap/bin/busybox` this session.)

**Also open (compounds testing):** the box **wedges after the first connection** — a
busybox left blocked in a proxied/stdin read holds the box's single client slot, so
later connections to `:2223` get nothing (the robustness gap, project task #9). Reboot
between box-side attempts; the `:2323` host repro avoids the rump client-slot issue.

</details>

### 🏆 M1 ACHIEVED (2026-06-22): NetBSD stack in an Akuma box — DHCP + HTTP to the host

`rumphttp` (a static Akuma binary = rump TCP/IP + our rumpuser + the
`/dev/net/tap0` backend) ran in a `MEMORY=1024M RUMP_NIC=1` box and fetched a page
off the **Mac host** through the NetBSD stack:

```
NetBSD 7.99.34 (RUMP-ROAST)
virt0: Ethernet address b2:0a:38:0b:0e:00
dhcp: virt0: adding IP address 10.0.2.15/24        ← DHCP from QEMU net1 SLIRP
dhcp: virt0: adding default route via 10.0.2.2
RUMPHTTP: connect 10.0.2.2:8888 -> 0               ← TCP to the host
HTTP/1.0 200 OK ... <html><body>HELLO-FROM-MAC-HOST-VIA-RUMP</body></html>
[VIRTIF STATS] tx=78 pkts/5828 bytes  rx=9 pkts/1832 bytes (over /dev/net/tap0)
RUMPHTTP: PASS — fetched 240 bytes over the NetBSD rump stack (DHCP + TCP via /dev/net/tap0)
```

That is the M1 goal verbatim: NetBSD TCP/IP as a userspace rump kernel inside an
Akuma box, DHCP an address, HTTP the QEMU host through it.

**Reproduce:** `( cd userspace/rumpkernel && ./build-rumphttp.sh )` → copy
`/tmp/rumphttp_akuma` to `disk.img:/bin/rumphttp` (docker `--privileged` loop-mount)
→ run a host HTTP server (`python3 -m http.server 8888 --bind 127.0.0.1`) →
`MEMORY=1024M RUMP_NIC=1 cargo run --release` → over SSH (`:2222`):
`/bin/rumphttp 10.0.2.2 8888`. (The host is reachable at the SLIRP gateway 10.0.2.2;
QEMU's net1 SLIRP also serves the DHCP lease.)

**The `/dev/net/tap0` backend is blocking, not busy-wait:** the kernel tap `read()`
now blocks (`FileDescriptor::Tap { nonblock }`; `rump_tap::read_frame_blocking`
cooperatively yields like socket `recv`/`wait_until`, since Akuma's net is poll-based
with no RX IRQ). The RX thread in `rumpcomp_tap.c` does a plain blocking `read()`.
Boot self-test `test_rump_tap` updated to open `O_NONBLOCK` (still checks EAGAIN).

New files: `rumpuser/rumpcomp_tap.c`, `rumpuser/rumphttp.c`, `build-rumphttp.sh`;
kernel: `Tap{nonblock}` + `read_frame_blocking`.

### 🏁 Milestone (2026-06-22): rump kernel boots on Akuma

`test_init` (the Phase-2 program) was linked with the **host** `aarch64-linux-musl-gcc`
(same toolchain as `userspace/build.sh` — the container was only ever needed to
*build* librump, not to link the final ELF), copied to `disk.img:/bin/test_init`,
and run in the VM (`MEMORY=1024M RUMP_NIC=0`, networking off). Output:

```
NetBSD 7.99.34 (RUMP-ROAST)
cpu0 at thinair0: rump virtual cpu
RUMPUSER-AKUMA: rump_init() returned 0
RUMPUSER-AKUMA: PASS — NetBSD rump kernel booted on our rumpuser
```

No crash. Proves the whole Phase-2 stack (Rust rumpuser, libkern overrides,
`rust_eh_personality` stub, static ELF) survives transplant onto Akuma's own
musl/pthread/mmap syscalls. **Akuma integration was a non-event** — the binary
that passes in the container is already an Akuma binary (same triple). The
`herd` route (run it as an OCI-bundle service; herd also wires the process VFS /
mounts for us) is now a trivial follow-up.

### 🏁 Milestone (2026-06-22): UNMODIFIED `curl` does HTTP over the rump stack

`docker-hijack-demo.sh` runs an off-the-shelf `curl 8.14.1` (no recompile) so its
network syscalls hit the NetBSD rump stack instead of the host kernel, and proves
it with instrumentation:

```
[VIRTIF TX#1] ARP →  RX#1/2 ARP reply →  TX#3 SYN → RX#3 SYN-ACK → TX#4 ACK
[VIRTIF TX#5] 138 (HTTP GET) →  RX#5/6 (HTTP 200 + body) →  TX/RX teardown
[VIRTIF STATS] tx=7 pkts/498 bytes  rx=7 pkts/711 bytes
<html><body>HELLO-FROM-NETBSD-RUMP-STACK</body></html>   ← body returned to curl
```

How: `rumpuser/hijack.c` is a single `LD_PRELOAD` `.so` that statically embeds the
whole rump stack (PIC archives) + our rumpuser; a constructor `rump_init`s and
brings up `virt0`; libc `socket/connect/send/recv/readv/writev/poll/fcntl/
getsockopt` are interposed onto `rump_sys_*` (fd offset `0x40000000`; Linux→NetBSD
sockaddr + `SOCK_NONBLOCK`/`O_NONBLOCK` handling). `rumpuser/virtif_user_instr.c` is
the stock TUN/TAP backend + per-frame counters/log at the rump↔wire seam (the
proof). Container `tun0`=10.0.0.1/24 + a python http server stand in for the wire.

Hard-won lessons (all in `docs/ARCHITECTURE_QUESTIONS.md`):
- **`busybox wget` can't be hijacked via LD_PRELOAD on musl** — it uses `FILE*`
  (`fdopen`) and musl stdio flushes via *inline* `writev`/`readv` syscalls that
  bypass the PLT. Use curl/nc-class tools (direct `send`/`recv`) — or kernel-routing.
- curl uses Linux-only `SOCK_NONBLOCK|SOCK_CLOEXEC` type bits NetBSD rejects → strip
  them; keep the rump socket blocking so connect/send/recv stay synchronous.

Scope: proven **in the container**. An Akuma box swaps the TUN/TAP backend for our
`rumpcomp_user` over `/dev/net/tap0` and runs it as a herd service — same shim, same
proof. New files: `rumpuser/hijack.c`, `rumpuser/virtif_user_instr.c`,
`docker-hijack-demo.sh`; the virtif PIC archive (build via `docker-build-virtif.sh`
without `MKPIC=no`), and rumpuser built `-C relocation-model=pic`.

---

## Environment facts

- Host: macOS arm64 (Apple Silicon). Docker daemon must be running.
- An arm64 Alpine container is **musl-native on aarch64** = Akuma's target, so we
  build librump + run the rump_init test *natively* in-container (no cross).
- Cross toolchain on host: `aarch64-linux-musl-gcc` (Homebrew). Rust target
  `aarch64-unknown-linux-musl` is installed.
- Big build outputs are **git-ignored** and currently **exist on disk** (so you
  can run the rump_init test immediately). A clean clone must re-run checkout +
  `docker-build.sh` (each ~1 min + a 375 MB clone).

---

## Reproduce everything (copy-paste)

```sh
cd userspace/rumpkernel

# (once) fetch pinned NetBSD source → src-netbsd/  (~375 MB, git-ignored)
./build.sh checkout

# (once) build librump*.a for aarch64-musl → obj/dest.stage/usr/lib/  (Linux container)
./docker-build.sh

# build the Rust rumpuser staticlib (host; no link step) → rumpuser/target/.../librumpuser_akuma.a
( cd rumpuser && cargo build --release --target aarch64-unknown-linux-musl )
#   add --features rumpuser_debug to trace every hypercall to stderr

# THE Phase-2 test: link librump.a + rumpuser + run rump_init() in the container
./docker-rumpuser-test.sh
# expect: "RUMPUSER-AKUMA: rump_init() returned 0  / PASS"
```

Kernel side (Phase 3, separate from the above):
```sh
RUMP_NIC=1 MEMORY=1024M cargo run --release      # adds NIC1 → /dev/net/tap0; boot prints
                                                 #   [rump] /dev/net/tap0 bound to NIC1 + [Test] rump_tap PASSED
cargo test -p akuma-rump --target "$(rustc -vV | grep '^host:' | cut -d' ' -f2)"   # 14 host tests
```
(Use `MEMORY=1024M` so an unrelated pre-existing `test_mmap_file_oom` boot test —
which needs a `/models` file larger than RAM — skips instead of panicking.)

---

## What's built, with file pointers

**Kernel (`rump` cargo feature, in `default` so release-only):**
- `crates/akuma-rump/src/lib.rs` — `RawNic` trait + `TapNic<N>` (RX two-phase
  state machine, bounds guard, TX) + `select_second_net_addr`; 14 host tests.
- `crates/akuma-net/src/rump_tap.rs` — `impl RawNic for VirtioRawNic` (real
  virtio-net NIC1), global instance, MMIO probe.
- `src/syscall/fs.rs` — `/dev/net/tap0` open/read/write/fstat; `src/syscall/term.rs`
  — `TUNSETIFF` no-op; `crates/akuma-exec/.../types.rs` — `FileDescriptor::Tap`.
- `src/main.rs` — `rump_tap::init(&mmio_addrs)` after net init.
- `src/process_tests.rs` — `test_rump_tap` (in `run_network_tests`) + `test_dev_zero`.
- `scripts/cargo_runner.sh` — `RUMP_NIC=1` adds NIC1 on `virtio-mmio-bus.4`.
- `/dev/zero`: `FileDescriptor::DevZero` mirrored across `fs.rs`/`proc.rs`.

**Userspace rump (`userspace/rumpkernel/`):**
- `build.sh` (checkout|build|host|clean), `docker-build.sh` (librump in Alpine),
  `docker-build-virtif.sh` (builds the one `-k`-skipped faction, `librumpnet_virtif.a`),
  `docker-rumpuser-test.sh` (link + run rump_init in container).
- `rumpuser/` — Rust **no_std** staticlib: `src/lib.rs` (`rumpuser_*` symbols +
  the `rumpuser_component_*` scheduler-bridge family added 2026-06-22),
  `csupport.c` (variadic `dprintf` + the libkern overrides + `rust_eh_personality`
  stub), `test_init.c` (calls `rump_init`), `Cargo.toml` (`rumpuser_debug` feature).
- `src-netbsd/` (git-ignored) — pinned NetBSD source; `rumpuser.h` is at
  `src-netbsd/sys/rump/include/rump/rumpuser.h` (`RUMPUSER_VERSION 17`).

**To rebuild + re-run the Akuma boot of `test_init`:**
```sh
( cd rumpuser && cargo build --release --target aarch64-unknown-linux-musl )
aarch64-linux-musl-gcc -O2 -static -o /tmp/test_init_akuma \
  rumpuser/test_init.c rumpuser/csupport.c -I obj/dest.stage/usr/include \
  -Wl,--allow-multiple-definition -Wl,--whole-archive \
    -L obj/dest.stage/usr/lib -lrump \
    rumpuser/target/aarch64-unknown-linux-musl/release/librumpuser_akuma.a \
  -Wl,--no-whole-archive -lpthread
# copy into disk.img:/bin/test_init  (docker run --privileged, mount -o loop)
# from repo root:  MEMORY=1024M RUMP_NIC=0 cargo run --release
# then over SSH (port 2222): run `/bin/test_init`  (bare path; the shell rejects
#   `VAR=val cmd` prefixes — RUMP_VERBOSE defaults ON anyway)
```

---

## Decisions locked in (don't relitigate)

- **`rumpuser` is ours, in Rust, no_std** (libc/pthread glue), replacing NetBSD's
  C librumpuser (buildrump `-k`).
- **virtif**: reuse rump's **kernel driver `if_virt.c`** (the NIC inside the
  NetBSD stack), but write **our own `rumpcomp_user` backend** over Akuma
  syscalls — NOT the stock Linux TUN/TAP backend. (So `/dev/net/tap0`'s
  `TUNSETIFF` no-op is now optional, not load-bearing.)
- **2nd dedicated NIC** (plan §4 option A) for L2 isolation; NIC0 stays smoltcp.
- `rump` is release-only (in `default`; size/extreme `--no-default-features` omit it).
- Forward architecture (post-M1, plan §10): config-driven per-box rump instances,
  host = box 0; optional later "NetBSD stack as a box's primary AF_INET stack".

---

## Carried workarounds (revisit before shipping)

1. **libkern byte-loop overrides** (`rumpuser/csupport.c`): rump's *optimized
   aarch64* `rumpns_{memset,memcpy,memmove,strlen,strcmp,strncmp}` run away in our
   environment, so we override them with trivial byte loops, linked with
   `-Wl,--allow-multiple-definition`. **Proper fix:** build `librump` with the
   generic C libkern routines (not the aarch64 asm); root-cause why the optimized
   ones run away (DC-ZVA / `DCZID_EL0` assumptions or how buildrump assembled them).
2. **`rust_eh_personality` no-op stub** (`csupport.c`): prebuilt Rust `core`
   references it under `panic=abort`. **Proper fix on Akuma:** rebuild core with
   nightly `-Z build-std` `-Cpanic=immediate-abort` (like Akuma's other userspace).
3. **`rumpuser` scheduler-wrap under concurrency** — ✅ **FIXED (2026-06-22).**
   The blocking `rumpuser` primitives now release the single rump CPU around the
   host blocking call (NetBSD's `rumpkern_unsched`/`rumpkern_sched` discipline):
   `mutex_enter`/`rw_enter` wrap **on contention** (trylock first; spin mutexes use
   the no-wrap path); `cv_wait`/`cv_timedwait` use `cv_unschedule`/`cv_reschedule`
   (with the spin-kmutex interlock special-case); and — the actual culprit —
   **`clock_sleep`** now unschedules around its `nanosleep`. The hardclock thread
   was holding the one rump CPU through every 10 ms tick, starving the main lwp
   parked in the scheduler slowpath (`cv_wait_nowrap`) — a classic lost-CPU-handoff
   ("missed delivery"). Found via a thread-ID-stamped, single-`write` trace
   (`--features rumpuser_debug`; lines no longer tear). Also note: `mutex_init` now
   stores the `RUMPUSER_MTX_SPIN|KMUTEX` flags (needed for the wrap decisions) and
   `cv_timedwait` now treats the timeout as RELATIVE+CLOCK_REALTIME (was absolute).
4. **Port the C glue → no_std Rust** (cleanup, do after it all works). `hijack.c`,
   `rumpcomp_tap.c`, and `rumphttp.c` are C for fast iteration, but none *need* to
   be — mirror `rumpuser/src/lib.rs` (no_std Rust exporting the C ABI). Interposers →
   `#[no_mangle] extern "C"`; the LD_PRELOAD constructor → `#[used]
   #[link_section=".init_array"]`; `rump_sys_*`/`dlsym(RTLD_NEXT)` → `extern "C"`
   decls. Only wrinkles: C-variadic `fcntl`/`open` (interpose with a fixed 3-arg
   signature — the optional arg sits in `x2` on aarch64; avoids nightly `c_variadic`),
   and the `.init_array` constructor. `csupport.c`'s variadic `rumpuser_dprintf` is
   the one piece that stays C-ish (variadic *definition* needs nightly — same reason
   it was split from the Rust rumpuser). The C files are the reference to debug
   against; keep them until the Rust port is proven equivalent.
5. **`/dev/net/tap0` should reset on close** (revealed by `rumpserver` testing). Only
   one rump process works per boot: an unclean exit leaves NIC1's RX two-phase state
   machine mid-flight, so the next `open("/dev/net/tap0")` can't receive (DHCP times
   out). Fine for a single long-lived box payload (sshd), but `close()`/process
   teardown should reset the `TapNic` RX state so a box can be restarted in place.

---

## 🏆 M2 ACHIEVED (2026-06-23): kernel-as-sysproxy-client — curl + IRC over rump

Sysproxy Steps 1–3 (spike / rump_server payload / rumpclient sharing) and the
transport-shape proof were already done; **M2 finished the kernel-as-client and
validated it end-to-end with two real unmodified binaries.** Architecture (decided
this session): **herd OWNS the one `rump_server` process** (`rumpnet.conf`:
`command=/bin/rump_server`, `args=--net --fd 3`, `stack=rump`, `restart=false`); herd
calls `SET_BOX_STACK` (syscall 324) before spawning, and when `sys_spawn_ext` sees that
spawn it calls `rump_proxy::attach_server`, which installs the kernel pipe pair on the
server's fd 3 (before it runs) and handshakes IN A KTHREAD. Then `handle_syscall` →
`rump_proxy::intercept_box_syscall` forwards a `stack=rump` box process's socket-family
syscalls (+ read/write/readv/writev/close on a `RumpSocket` fd) over the channel via
`akuma_rump::sysproxy::Client`, marshaled by `syscall_translation` + `ProcMem` (user-VA
copyin/copyout + sockaddr Linux↔NetBSD). New fd type `FileDescriptor::RumpSocket`.
Driving is **synchronous on the calling thread** (approach 1 — copyin/copyout hit
`current` VA); the kthread is only setup/handshake.

**Validated:**
- `box use rumpnet -i /bin/curl -sS -H Host:ifconfig.me http://34.160.111.145` →
  `87.71.13.205` over the rump stack (TCP path: socket/connect/getsockname/getsockopt
  /setsockopt/sendto/recvfrom).
- `box use rumpnet -i /bin/sic -h 163.61.26.35 -p 6667 -n netoneko` → full IRC
  registration + **live `#rumpkernel` session on OFTC** (acceptance/11 capstone).
  Required `readv`/`writev` marshaling (sic uses stdio) + `poll`/`select`-on-RumpSocket
  (MSG_PEEK probe) + a `sic` recv-drain patch (vendored: `userspace/rumpkernel/sic`).

## NEXT TASK — drive down latency + robustness (M2 weaknesses). Plan: RUMP_SYSPROXY.md

The path WORKS; the weaknesses are performance + robustness, not correctness:

1. **Latency — ✅ MAJOR WIN via the FIBER backend (2026-06-24): ~3.85× faster.**
   The cost was the ~19 pthread kthreads contending on one core (single-vCPU futex
   thundering-herd). The **cooperative fiber backend** (one OS thread + userspace
   scheduler, cargo feature `threads_fiber`) collapses them to 1 thread and kills the
   futex storm. **Now WORKING end-to-end**: `box use rumpnet -i /bin/curl -sS
   http://example.com/` → HTTP 200 in **16.3s on fiber vs 62.8s pthread (3/3 stable)**;
   `rump_server` = **1 OS thread (vs 19)**, PSTATS **clone=0 futex=0 (vs 20/2606)**.
   The "fiber backend is BLOCKED" note is OBSOLETE — the sp server now runs under
   fiber (NOT the old `rumpfiber_sp.c` stub; our `sp_serve_fd.c` + cooperative
   `pthread_mutex`/`pthread_cond` redirect to `akfiber_sp_*` fixed the COPYIN
   `pthread_cond_wait` deadlock). **Full write-up + how-to: `docs/FIBER_HANDOFF.md`.**
   Earlier dead-ends (for the record): heartbeat/`hz` 100→20 made it *worse*
   (`RUMP_LATENCY_SLEEP_FIX.md`); scheduler wakeup-locality hint (gated off). Residual
   under fiber: ~1s/proxied-syscall from poll/yield + ~10ms rump-clock granularity →
   event-driven channel wakeup is the next lever (curl 16s, not sub-second).
2. **Robustness (project task #9):** box procs stuck in a proxied syscall are
   UNINTERRUPTIBLE (the proxy channel read never checks `is_current_interrupted`/pending
   signals), `kill <pid>` of a box proc returns "invalid pid" (box/pid-namespace), and
   the single serialized `BoxProxy.client` slot means one wedged proc blocks ALL others
   (they spin in `with_client`). Only reboot clears it. Fix: interrupt/timeout the proxy
   read (return EINTR), reclaim the client slot from a dead holder, fix box-pid kill.
3. **DNS path (UDP): ✅ DONE (2026-06-23).** musl's resolver does
   `socket(AF_INET,DGRAM|NONBLOCK)` → `bind(INADDR_ANY:0)` → `sendto(query, ns:53)` →
   `recvmsg(answer)`. All three new calls are in `src/rump_proxy.rs`: `proxy_bind`
   (translate sockaddr like connect); `proxy_transfer` now marshals `sendto`'s dest
   addr (args[4]≠0 ⇒ UDP) and `recvfrom`'s source capture + `MSG_DONTWAIT`-when-
   nonblock so the drain loop ends on EAGAIN; and `proxy_recvmsg` decomposes the
   Linux `msghdr` in-kernel and drives the proven rump `recvfrom` (first iovec +
   `msg_name` source capture, `fromlenaddr`=the msghdr's `msg_namelen` field) — no
   full msghdr ABI translation needed. Validated live: `box use rumpnet -i /bin/curl
   http://example.com` → resolve `104.20.23.154` → HTTP 200. NOTE: requires a working
   nameserver — QEMU SLIRP's `10.0.2.3` returns empty answers on some hosts, so
   `bootstrap/etc/resolv.conf` now defaults to `8.8.8.8`/`1.1.1.1` (reached via the
   rump default route → SLIRP → internet). `sendmsg` (the send-side mirror) is still
   `EOPNOTSUPP` — this resolver sends via `sendto`, so DNS doesn't need it; add it if
   a glibc/c-ares client shows up. Multi-iovec `recvmsg` scatter is unimplemented
   (DNS is single-iovec; logged if `iovlen>1` ever appears).
4. **Kernel boot self-tests** for the proxy path (project policy — task #6).
5. **Security/hardening** (RUMP_SYSPROXY.md): sp-wire bounds-checks, seal `rumpuser__hyp`
   (mprotect), per-box isolation self-tests, channel-fd private to rump_server.
6. **Cleanup:** the `[RUMP-SP]` connect/transfer/iov debug prints are still in (gate them);
   `/dev/net/tap0` reset-on-close (workaround #5) so a box restarts without reboot.

---

## Gotchas learned (will save you time)

- **Serial/QEMU + rump logs contain control bytes** → use `grep -a` or grep finds
  nothing ("binary file matches").
- **Link the rump test `-static`** — the lib dir also has `librump.so`, which
  `-lrump` prefers → runtime "librump.so.0 not found".
- **`docker run` in background "completes" early** (false signal) — poll the log
  for your own `EXIT=` marker or watch `docker ps`, don't trust the task notice.
- **2016 NetBSD vs modern toolchain**: host-tool build needs `-fcommon`, trailing
  `-Wno-error` (after `"$@"` so it beats NetBSD's `-Werror`), and a `__BEGIN_DECLS`
  cdefs shim on musl — all in `docker-build.sh`'s gcc wrapper.
- **Debug a rump crash**: `--features rumpuser_debug` traces hypercalls; `apk add
  gdb` in the container, break at the call site to read real args.
- **NetBSD banner**: `rumpuser_getparam` defaults `RUMP_VERBOSE` ON (kept out of
  respect for the NetBSD attribution); an env `RUMP_VERBOSE` overrides; the
  `rump_quiet` cargo feature silences the default.
- **`libakuma` is awkwardly structured** and should be broken up (deferred) — keep
  in mind when adding the rump-net userspace binary.
