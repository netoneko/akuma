# Acceptance: NetBSD rump-kernel TCP/IP — clone, compile & IRC `#rumpkernel`

**Status: TARGET (not yet runnable).** This is the capstone proof for the
rump-kernel port (`userspace/rumpkernel/`): build a real network client *inside
Akuma* against the NetBSD rump TCP/IP stack, connect it to a public IRC network
over the real internet, and read back live channel state. If the channel topic /
names list comes back, the bytes provably traversed **the NetBSD stack** (not
Akuma's smoltcp) — because the client is linked against `librump*` + our
`rumpuser`, and its socket calls are `rump_sys_*`, carried by the virtif NIC over
`/dev/net/tap0`.

The client is **sic** (suckless simple IRC client, ~250 lines of C) — small
enough to compile in-VM with `tcc`, and pure BSD-sockets so it links cleanly
against the rump syscall surface.

See `userspace/rumpkernel/docs/HANDOFF.md` for port status. Milestones already
green: `rump_init()` boots on Akuma; `librumpnet_virtif.a` built; `rumpuser_component_*`
in place. Remaining before this demo runs: virtif packet backend over
`/dev/net/tap0`, DHCP, the rump-SDK install path, and the `tcc`-against-rump link.

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

The IRC client proves the **outbound** (connect) path. The same box also runs an
SSH server **listening on the rump stack**, proving the **inbound** (listen/accept)
path and a full interactive bidirectional session — a stronger end-to-end check.

**Topology (two stacks, side by side):**

| Reach | Host port | Guest NIC | Stack | Server |
|-------|-----------|-----------|-------|--------|
| Akuma itself | `2222` | NIC0 (net0) | smoltcp (`src/syscall/net.rs`) | Akuma's in-kernel sshd |
| **the box**  | **`2223`** (`RUMP_SSH_PORT`) | NIC1 (net1) → `/dev/net/tap0` | **NetBSD rump** | dropbear in the box |

So `ssh -p 2222 root@localhost` lands on Akuma (smoltcp); `ssh -p 2223 root@localhost`
lands on the **box's** sshd whose socket lives on the **NetBSD stack**. Genuinely
different interface, different stack, and a different guest IP — `virt0` gets its
address from net1's own SLIRP DHCP (`10.0.2.15`, gateway `10.0.2.2`), independent of
whatever NIC0/smoltcp holds. `scripts/cargo_runner.sh` adds the
`hostfwd=tcp::2223-:22` on net1 when `RUMP_NIC=1` (override with `RUMP_SSH_PORT`).

**Why dropbear, not our `userspace/sshd`:** our sshd is built on `libakuma`'s net
abstraction (`net-async`, whose `socket/bind/listen/accept` are Akuma syscalls), so
pointing *it* at rump would mean adding a **libakuma rump backend** — a separate
integration. dropbear is an unmodified C binary that uses libc sockets directly, so
it drops onto the **standard `librumphijack` `LD_PRELOAD`** path with no code
changes — the simpler route for this demo. (The libakuma-backend route is the
better long-term answer for *our* programs; tracked in docs/ARCHITECTURE_QUESTIONS.md.)

**In-box (added to the herd service's run script):**
```sh
# after virt0 is up with an address (dhcp_ipv4_oneshot):
#   run an unmodified dropbear whose libc socket/bind/listen/accept are redirected
#   to the rump stack. Two ways (see docs/ARCHITECTURE_QUESTIONS.md):
#     (a) in-process: dropbear linked/preloaded with librumphijack against the
#         in-process rump kernel — the box is one process hosting rump + dropbear;
#     (b) sysproxy: rump_server owns the stack, dropbear runs with
#         LD_PRELOAD=librumphijack + RUMP_SERVER=unix://… (needs sp_* hypercalls).
LD_PRELOAD=/usr/lib/librumphijack.so dropbear -F -E -p 22 -r /etc/box_hostkey
```

**Login shell = busybox `sh`.** dropbear runs a real login shell (`/bin/busybox
sh`, already shipped static in `bootstrap/bin`), so an SSH session into the box
gets full POSIX shell grammar — `&&`, pipes, `$?`, `VAR=val cmd` — *not* Akuma's
in-kernel SSH shell (which rejects those; you hit it on `:2222`). So the entire
in-box flow above (clone sic → `tcc` compile → run) is driven from a normal shell
over the NetBSD stack. (Configure via dropbear's shell target / the box's
`/etc/passwd`.)

**Expected:** from the host, `ssh -p 2223 root@localhost` opens a busybox shell
**in the box**, with the TCP handshake + the entire session carried by the NetBSD
stack on our rumpuser. `RUMP_NIC=0` (no `/dev/net/tap0`) → `:2223` refuses — the
negative control that it isn't secretly smoltcp.

---

## Why this proves the *correct* stack (not smoltcp)

- The client's sockets are `rump_sys_socket/connect/...`, resolved from `librump*`
  — never Akuma's `src/syscall/net.rs` (smoltcp) path.
- Packets leave via virtif → `/dev/net/tap0` (NIC1), which is L2-isolated from
  NIC0. NIC0/smoltcp is not in the path.
- A negative control: with `RUMP_NIC=0` (no `/dev/net/tap0` backend), the same
  binary must fail to connect — confirming it is *not* secretly using smoltcp.

## Open items before this is runnable

Done:
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

Remaining:
- [ ] virtif packet backend over `/dev/net/tap0` on **Akuma** — container proof uses
      the stock Linux TUN/TAP backend; Akuma needs our `rumpcomp_user` over the kernel
      tap device.
- [ ] DHCP one-shot (`rump_pub_netconfig_dhcp_ipv4_oneshot`) against net1's SLIRP.
- [ ] the `/archives` herd install + bundle (`config.json`: SDK unpack, mounts,
      `process.args` for the run script).
- [ ] **sic** ↔ rump link recipe (libc-socket shim vs rump bootstrap wrapper).
- [ ] **dropbear** check: it must do its socket I/O via *directly-called* libc
      `socket/accept/read/write` (interposable by `hijack.c`), NOT via `FILE*`
      stdio — musl stdio flushes through inline `writev`/`readv` syscalls that
      bypass the PLT and **cannot** be LD_PRELOAD-hijacked (this is why `busybox
      wget` fails but `curl` works). If dropbear uses stdio on the socket, fall back
      to the sysproxy model (`librumphijack`/`librumpclient` + un-stub `sp_*`) or
      kernel-routing.
