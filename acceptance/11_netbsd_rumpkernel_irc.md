# Acceptance: NetBSD rump-kernel TCP/IP — clone, compile & IRC `#rumpkernel`

**Status: ✅ PASSES (2026-06-23).** The capstone IRC proof is met: **`sic` holds a
live `#rumpkernel` session on OFTC, with the entire IRC session carried by the
NetBSD rump TCP/IP stack** running on our Rust `rumpuser` inside Akuma — not
smoltcp. Source of record: `userspace/rumpkernel/docs/HANDOFF.md` ("🏆 M2
ACHIEVED (2026-06-23)") and commit `28df3f1` *"IRC works end to end on netbsd
networking stack"* (build-up: `e523669` "connected to libera via sic", `075029f`
"patch sic").

How it actually ran (differs from the original `tcc`-in-VM plan below — see
"What shipped" note): the unmodified static `sic` binary runs in a `stack=rump`
box, and the **kernel forwards its AF_INET socket syscalls to a shared boxed
`rump_server`** (kernel-as-sysproxy-client). The proof is unchanged: the client's
sockets resolve to the NetBSD stack via the rump_server, packets leave over
virtif → `/dev/net/tap0` (NIC1), and NIC0/smoltcp is never in the path.

```
box use rumpnet -i /bin/sic -h 163.61.26.35 -p 6667 -n akuma_test
# → full IRC registration + live #rumpkernel session on OFTC, over the rump stack
```

This proof originally targeted **sic** built in-VM with `tcc` against `librump*` +
our `rumpuser` (the recipe below). It is preserved as the design narrative; the
shipped path used the sysproxy routing above instead, which proves the same thing
(real bytes over the NetBSD stack) without the in-VM compile step.

---

## Prerequisites (the pieces this demo assembles)

1. **Rump SDK archive.** `userspace/rumpkernel/package-sdk.sh` produces
   `rump-sdk-aarch64-musl.tar.gz` (the `obj/dest.stage/usr` tree: `librump*.a` +
   `librumpnet_virtif.a` + headers `rump/*.h`) plus our `librumpuser_akuma.a` and
   the virtif backend object. Staged on the host under `bootstrap/archives/`
   (alongside `apk-tools.tar`, `libtcc1.tar`); `populate_disk.sh` copies the
   `bootstrap/` tree into the disk, landing it at the VM's `/archives` (temporary
   install path — see "Install mechanism" below).

2. **herd profile** with a build toolchain available to the service:
   - `tcc` (Akuma's own static tcc — `userspace/tcc`),
   - `scratch` symlinked as `git` (the in-VM git used by acceptance 02/08),
   - the rump SDK installed under `/usr` (libs in `/usr/lib`, headers in `/usr/include/rump`).

3. **Kernel** built `--release` with the `rump` feature (default) and booted with
   `RUMP_NIC=1` so `/dev/net/tap0` is backed by NIC1 (the rump NIC, isolated from
   NIC0/smoltcp). Give it real RAM: `MEMORY=1024M`+ (full NetBSD kernel + mbufs).

## Install mechanism (temporary)

For now, herd installs archives from a local `/archives` directory on the disk
(populated at disk-build time from the host's `archives/`). A herd bundle may
declare an archive to unpack into its rootfs before `process_args` runs:

```
/etc/herd/available/rump-irc/
  config.json        # process_args: ["/bin/sh","/opt/run-irc.sh"], mounts, archives
  archives/          # or referenced from /archives/rump-sdk-aarch64-musl.tar.gz
```

`herd` already wires the process VFS / mounts, so `/dev/net/tap0` and the unpacked
`/usr` land in the service's namespace declaratively (no hand-plumbing into the
box). Longer term this becomes a real package step; `/archives` is the bring-up
shortcut.

---

## The demo (what `run-irc.sh` does, in-VM)

```sh
# 1. install the rump SDK (temporary: untar the staged archive)
tar -xzf /archives/rump-sdk-aarch64-musl.tar.gz -C /

# 2. fetch sic sources (pinned) — builds cleanly with tcc
#    https://dl.suckless.org/tools/sic-1.3.tar.gz   (~suckless simple IRC client)
git clone https://git.suckless.org/sic /tmp/sic     # or: curl the pinned 1.3 tarball

# 3. compile sic against the rump stack with tcc, fully static.
#    - rump_sys_* replace the libc socket calls (a tiny shim maps
#      socket/connect/send/recv → rump_sys_*), OR sic is built with
#      -Dmain=sic_main and driven by a small rump bootstrap (rump_init +
#      ifcreate virt0 + ipv4 via dhcp_ipv4_oneshot). NOTE: with the stock-style
#      virtif (no RUMP_VIF_LINKSTR) the iface binds its tap at clone time, so
#      there is NO ifsetlinkstr call — see docker-net-test.sh / test_net.c.
#    - link: -lrumpnet_config -lrumpnet_virtif -lrumpnet_netinet -lrumpnet_net
#            -lrumpnet -lrump  + librumpuser_akuma.a + virtif_user backend,
#            --whole-archive for the component constructors, -static.
tcc -static -I/usr/include -L/usr/lib ... -o /tmp/sic_rump /tmp/sic/*.c

# 4. bring the stack up and connect to a public IRC network.
#    (freenode's successor is Libera.Chat; the rump project channel is
#     #rumpkernel — historically on freenode, mirror to whatever network is live.)
/tmp/sic_rump -h irc.libera.chat -p 6667 -n akuma-rump <<'IRC'
:JOIN #rumpkernel
:NAMES #rumpkernel
:TOPIC #rumpkernel
IRC
```

## Expected output (the proof)

- DHCP assigns an address to `virt0` (logged by the rump stack).
- The TCP connect to the IRC server succeeds **through the NetBSD stack**.
- sic prints the channel **topic** and **names** list for `#rumpkernel`.

Seeing live `#rumpkernel` channel state = real bytes over the real internet,
carried by the NetBSD TCP/IP stack running on our Rust `rumpuser`, inside Akuma,
supervised by herd. That is the end-to-end win the whole port is for.

## Same demo, other direction — SSH straight into the box over the rump stack

**Status: ✅ inbound proven; login reaches a busybox prompt (2026-06-24).** The IRC
client proves the **outbound** (connect) path. The same box also runs an SSH server
**listening on the rump stack**, proving the **inbound** (listen/accept) path: from
the host, `ssh -p 2223 root@localhost` connects, completes the SSH key exchange,
authenticates, and lands at a **busybox `/bin/sh` prompt running in the box's fresh
`/srv/rumpbox` root** — the entire TCP handshake + session carried by the NetBSD rump
stack (the kernel sysproxy routes sshd's `listen`/`accept`/`recvfrom`/`sendto`). We do
this with **our own `userspace/sshd`**, spawned by **herd inside the rumpnet box** —
not dropbear, not LD_PRELOAD. (Known rough edge: the interactive *command* round-trip —
forwarding client keystrokes into the shell's stdin via the sshd bridge — is still
being finished; the login, auth, shell spawn, and prompt all work over rump.)

**Topology (two stacks, side by side):**

| Reach | Host port | Guest NIC | Stack | Server |
|-------|-----------|-----------|-------|--------|
| Akuma itself | `2222` | NIC0 (net0) | smoltcp (`src/syscall/net.rs`) | Akuma's in-kernel sshd |
| **the box**  | **`2223`** (`RUMP_SSH_PORT`) | NIC1 (net1) → `/dev/net/tap0` | **NetBSD rump** | `userspace/sshd` in the rumpnet box |

So `ssh -p 2222 root@localhost` lands on Akuma (smoltcp); `ssh -p 2223 root@localhost`
lands on the **box's** sshd whose sockets live on the **NetBSD rump stack**. Genuinely
different interface, different stack, and a different guest IP — `virt0` gets its
address from net1's own SLIRP DHCP (`10.0.2.15`, gateway `10.0.2.2`), independent of
NIC0/smoltcp. `scripts/cargo_runner.sh` adds `hostfwd=tcp::2223-:22` on net1 when
`RUMP_NIC=1` (override with `RUMP_SSH_PORT`).

**How it works (no dropbear, no LD_PRELOAD).** `userspace/sshd` is built on
`libakuma` net, whose `socket/bind/listen/accept` are Akuma syscalls. Because sshd
runs **inside the `stack=rump` rumpnet box**, the kernel **sysproxy intercepts those
syscalls and forwards them to the box's `rump_server`** (`src/rump_proxy.rs`
`intercept_box_syscall`) — the exact same kernel-as-client path that carries curl/sic
outbound, now extended to the inbound server calls. No libakuma rump backend, no
hijack `.so`. sshd's **login shell is busybox `/bin/sh`** (argv[0] dispatch → ash), so
the session gets full POSIX grammar (`&&`, pipes, `$?`, `VAR=val cmd`) — *not* Akuma's
in-kernel restricted shell (which you hit on `:2222`). sshd's filesystem is the box's
**fresh `/srv/rumpbox`** root (it reads `/srv/rumpbox/etc/sshd/sshd.conf`; an SSH
`ls /` shows the fresh tree, not the host root).

### Config (this is the reproducible acceptance setup)

Two herd services in `bootstrap/etc/herd/enabled/`, both boxed into the **same** box
(`box_id` from the name "rumpnet"):

`rumpnet.conf` — owns the rump_server, `box_root` is the fresh dir:
```
command  = /bin/rump_server
args     = --net --fd 3
boxed    = true
box_root = /srv/rumpbox
stack    = rump
restart  = false
```

`sshd.conf` — joins the rumpnet box, busybox shell, /proc mounted, delayed start:
```
command    = /bin/sshd
args       = --port 22 --shell /bin/sh
join_box   = rumpnet      # spawn INTO the rumpnet box → AF_INET sysproxy-routed to its rump_server
mount      = proc         # fresh-root box has no /proc; sshd's stdin bridge needs /proc/<pid>/fd/0
start_delay = 4000        # start after rumpnet's rump_server handshake; restart backstops the race
restart    = true
```

The fresh box rootfs lives at `bootstrap/srv/rumpbox/` (copied to the disk's
`/srv/rumpbox` by `populate_disk.sh`): `bin/{rump_server,sshd,busybox,sh}` (`sh` is a
copy of busybox) and `etc/sshd/sshd.conf` (`shell=/bin/sh`, `port=22`,
`disable_key_verification=true`). `/dev/net/tap0` needs no entry — it is matched
pre-namespace by the kernel (see docs/BOX_SUBDIR_FS_LIMITATIONS.md).

### Run it fresh

```sh
cargo build --release
userspace/build.sh                 # builds userspace/sshd, herd, busybox staging, etc.
scripts/create_disk.sh             # (re)create the ext2 disk
scripts/populate_disk.sh           # copies bootstrap/ (incl. srv/rumpbox + the 2 herd confs) onto it
RUMP_NIC=1 MEMORY=1024M cargo run --release
```

Boot prints the proxy self-test `[Test] rump_listen_accept PASSED`; herd starts
`rumpnet` (DHCP `10.0.2.15`) then `sshd` in the same box (logs `[SSHD] Listening on
0.0.0.0:22`). Then from the host (the `ssh` CLI is policy-blocked here — drive it via
Python), an INTERACTIVE session (sshd handles the SSH "shell" request, not "exec"):

```python
import subprocess
subprocess.run(["ssh","-tt","-o","StrictHostKeyChecking=no","-o","UserKnownHostsFile=/dev/null",
                "-p","2223","root@localhost"], input="ls /\nexit\n", text=True, timeout=120)
# → authenticates and lands at a busybox prompt:
#     /bin/sh: can't access tty; job control turned off
#     ~ #
#   running IN the box (fresh /srv/rumpbox root). The TCP handshake + session are
#   carried by the NetBSD rump stack — kernel log shows [RUMP-SP] route ... listen /
#   accept / recvfrom / sendto on the sshd pid, routed through the box's rump_server.
```

(The login, key exchange, auth, busybox-shell spawn, and prompt are all carried over
the rump stack. Forwarding typed commands into the shell's stdin via the sshd bridge is
the one piece still being finished — see "What this required" / known issues.)

**Negative control:** boot with `RUMP_NIC=0` (no `/dev/net/tap0`, no net1) → `:2223`
refuses — confirming the session isn't secretly smoltcp.

### What this required (2026-06-24, kernel + herd — UNCOMMITTED, user commits)

- **Kernel sysproxy inbound** (`src/rump_proxy.rs`): `proxy_listen` + `proxy_accept`
  (previously "Not marshaled yet"). `accept` is forwarded **non-blocking** (the
  listener is set `O_NONBLOCK` server-side) and waits in the kernel, yielding the core
  to the rump_server — a blocking accept would stall to the 15s transport timeout
  (EIO). The accepted rump fd is registered as a box `RumpSocket`; the peer sockaddr
  is translated NetBSD→Linux. Connected `recv` on a **blocking** box socket now also
  blocks in the kernel (`MSG_DONTWAIT` + yield) instead of server-side — libakuma's
  `TcpStream::read` is a blocking recv, so this avoids both the 15s hang and a
  busy-spin. (`Op::Listen`/`Op::Accept` sysnos already existed in
  `crates/akuma-rump/src/syscall_translation.rs`.)
- **herd** (`userspace/herd/src/main.rs`): `join_box` (spawn into an existing box, no
  re-register / no re-mark), `mount = proc|tmpfs` (mount into the box namespace), and
  `start_delay` (defer initial start).
- **Kernel ns idempotency** (`src/vfs/mod.rs`): `create_box_namespace` returns the
  existing namespace on re-register, so herd's pid-update register doesn't drop the
  box's `/proc` mount.
- **Boot self-test**: `[Test] rump_listen_accept` (in `rump_proxy::run_demo`, gated on
  `RUMP_NIC=1`) drives socket→bind→listen→nonblock→accept and asserts a fast EAGAIN.

---

## Why this proves the *correct* stack (not smoltcp)

- The client's sockets are `rump_sys_socket/connect/...`, resolved from `librump*`
  — never Akuma's `src/syscall/net.rs` (smoltcp) path.
- Packets leave via virtif → `/dev/net/tap0` (NIC1), which is L2-isolated from
  NIC0. NIC0/smoltcp is not in the path.
- A negative control: with `RUMP_NIC=0` (no `/dev/net/tap0` backend), the same
  binary must fail to connect — confirming it is *not* secretly using smoltcp.

## Status of the pieces

**The IRC capstone PASSES** (2026-06-23) via the kernel-as-sysproxy-client path
(see header + `docs/HANDOFF.md` "M2 ACHIEVED"). The list below tracks the
original in-VM-`tcc` recipe; several items were satisfied by the sysproxy route
instead of literally as written.

Done:
- [x] **IRC capstone — live `#rumpkernel` session over the NetBSD rump stack** —
      `sic` in a `stack=rump` box, AF_INET forwarded to the boxed `rump_server`
      (commit `28df3f1`). Also: unmodified `curl` does HTTPS-by-IP over rump.
- [x] `rumpuser` scheduler-wrap under real concurrency — fixed (clock_sleep +
      contended mutex/rwlock + cv waits release the rump CPU). Container net test
      (`docker-net-test.sh`) is GREEN: `virt0` up, IP assigned, `rump_sys_socket` OK.
- [x] `package-sdk.sh` → `bootstrap/archives/rump-sdk-aarch64-musl.tar.gz`.
- [x] `cargo_runner.sh` forwards host `:2223` → rump:22 on net1 (`RUMP_SSH_PORT`).

Done:
- [x] **In-process LD_PRELOAD hijack PROVEN** — unmodified `curl` did HTTP over the
      rump stack (`docker-hijack-demo.sh`): `rumpuser/hijack.c` (the `.so`) +
      `rumpuser/virtif_user_instr.c` (per-frame proof counters). The IRC/SSH demo
      reuses this exact shim.
- [x] **Inbound TCP server over rump, host-reachable** (`rumpuser/rumpserver.c`):
      in a `RUMP_NIC=1` box it DHCPs, `bind`/`listen`/`accept`s on the rump stack,
      and the **Mac host** reached it via `localhost:2223` → SLIRP → rump `:22`
      (banner + echo returned). This is the transport an sshd sits on — the SSH
      `listen/accept` path is proven; what's left is the SSH *protocol*.
      Caveat: one rump process per boot (unclean kill leaves NIC1 RX mid-flight →
      next process can't DHCP). Fix later: reset `/dev/net/tap0` on close.

Done (resolved by M1/M2 — see `docs/HANDOFF.md`):
- [x] virtif packet backend over `/dev/net/tap0` on **Akuma** — our `rumpcomp_tap.c`
      over the kernel tap device (M1, 2026-06-22).
- [x] DHCP one-shot (`rump_pub_netconfig_dhcp_ipv4_oneshot`) against net1's SLIRP
      (M1: `virt0` → `10.0.2.15`).
- [x] herd autostarts the rump networking box (`rumpnet.conf`: `command=/bin/rump_server`,
      `--net --fd 3`, `stack=rump`, `restart=false`) — supersedes the `/archives` SDK
      unpack bundle; herd OWNS the `rump_server` (M2, 2026-06-23).
- [x] **sic** over rump — via the kernel-as-sysproxy-client route (no in-VM `tcc`
      link needed); needed `readv`/`writev` marshaling + `poll`/`select` on
      `RumpSocket` + a `sic` recv-drain patch (vendored `userspace/rumpkernel/sic`).

Done (the inbound SSH variant):
- [x] **sshd on the rump stack** (2026-06-24) — `userspace/sshd` runs in the rumpnet
      box (busybox `/bin/sh`, fresh `/srv/rumpbox` root), spawned by herd
      (`join_box`); its `listen`/`accept`/`recv` are sysproxy-routed to the box's
      `rump_server` via the new `proxy_listen`/`proxy_accept` + connected-blocking-recv
      kernel-wait in `src/rump_proxy.rs`. No dropbear, no LD_PRELOAD — see the
      "SSH straight into the box" section above for the full config + recipe.

Remaining (polish — not blocking the IRC capstone):
- [ ] DNS over rump (UDP `sendto`/`recvmsg`) — use raw IPs until then.
- [ ] per-syscall latency (~1s round-trip) + robustness — see `docs/HANDOFF.md` / `RUMP_SYSPROXY.md`.
