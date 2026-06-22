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

# 2. clone sic with the in-VM git (scratch)
git clone https://git.suckless.org/sic /tmp/sic      # or a pinned mirror

# 3. compile sic against the rump stack with tcc, fully static.
#    - rump_sys_* replace the libc socket calls (a tiny shim maps
#      socket/connect/send/recv → rump_sys_*), OR sic is built with
#      -Dmain=sic_main and driven by a small rump bootstrap (rump_init +
#      ifcreate virt0 + ifsetlinkstr /dev/net/tap0 + dhcp_ipv4_oneshot).
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

---

## Why this proves the *correct* stack (not smoltcp)

- The client's sockets are `rump_sys_socket/connect/...`, resolved from `librump*`
  — never Akuma's `src/syscall/net.rs` (smoltcp) path.
- Packets leave via virtif → `/dev/net/tap0` (NIC1), which is L2-isolated from
  NIC0. NIC0/smoltcp is not in the path.
- A negative control: with `RUMP_NIC=0` (no `/dev/net/tap0` backend), the same
  binary must fail to connect — confirming it is *not* secretly using smoltcp.

## Open items before this is runnable

- [ ] virtif packet backend (`rumpcomp_virt_*`) over `/dev/net/tap0` (Akuma) —
      container proof of the stock TUN/TAP backend first (`docker-net-test.sh`).
- [ ] `rumpuser` scheduler-wrap under real concurrency (RX kthread) — workaround
      #3 in HANDOFF; the virtif RX thread is the first real test of it.
- [ ] DHCP one-shot against the QEMU user-net / tap.
- [ ] `package-sdk.sh` + the `/archives` herd install path.
- [ ] sic ↔ rump link recipe (libc-socket shim vs rump bootstrap wrapper).
