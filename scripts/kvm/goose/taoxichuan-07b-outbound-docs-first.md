# Taoxichuan stage 7, session B: guest outbound TCP - other guests already do this, read how first

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Session A
(`/root/dev-cycle/taoxichuan-s7/RESULT-A.md`, commit `934e9603` on `taoxichuan-s4`) left H1 failing: a guest `wget` held
the gateway's SYN, the relay dialed and got ESTABLISHED, then `to_host=0` forever. **Keep your context small.**

## The key fact: this already works for other guests - do not rebuild the host kernel

`vm_outbound` is a finished, metal-verified feature. A stock Alpine/Linux guest and the full Akuma guest, on this same box and
the same host kernel, `wget` the host's LAN address and the Alpine CDN by name, and download 4 MiB with a matching md5. So the
host side (gateway + relay) is correct and **the host kernel is NOT to be changed or rebuilt**. Whatever is wrong is on the
Taoxichuan guest's side of the handshake. Session A's belief that "10.0.2.2 needs a host-kernel fix" was a mistake of the
brief: `10.0.2.2` is only the gateway (ARP/DHCP/ICMP/UDP echo); no guest ever used it as an alias for the host. Connect to
real addresses: **the gate address is the box's LAN IP `192.168.1.159`** (your control run already worked with it).

## Read these first (in this order, about 15 minutes, `sed -n` the sections named; do not read whole files)

1. `docs/reference/subsystems/kvm.md` - the section "Outbound networking (`vm_outbound`, 2026-10-23)" (~line 277) and the
   `virtio_net`/`gateway` row (~line 41): what the gateway holds, the `OUT_SLOTS`, and the counter line
   `[KVM] relay: outbound opened= ... gw_syns= ...`.
2. `docs/archive/KVM_OUTBOUND_NETWORKING.md` - "Design" (the 4-step SYN-held / dial / `out_accept` / shuttle sequence) and
   "Verified": this is the exact behaviour a guest must meet. The gateway has **no TCP retransmit timer** and ignores the
   guest's own SYN retries while the host dials.
3. `docs/archive/KVM_RELAY_SYN_STALL.md` - "The 'lost' SYNs ... were slow, not lost": a one-vCPU guest that does not poll its
   NIC while it is busy answers late. Your guest is single-task and single-vCPU. **Suspect first: the Taoxichuan guest only
   polls its NIC inside certain loops, so the SYN-ACK sits in the rx ring while `connect`/`wget` waits somewhere that never
   polls.** Compare with how the full Akuma guest polls (`docs/archive/AKUMA_KVM_AKUMA_GUEST.md`, the networking section, and
   `docs/reference/subsystems/networking.md`).
4. `crates/akuma-svm-amd64/tests/gateway_tcp.rs` - the test
   `an_outbound_connection_is_held_until_the_host_answers_then_streams_both_ways` is the smoltcp-guest reference sequence.
   It passes on the host, so diff **your guest's** behaviour against it, not the gateway.

## Gates

- **H1:** over the exec-only ssh path (`ssh -p 2224 root@localhost '<cmd>'`), the guest's `busybox wget -qO- http://192.168.1.159:8123/`
  prints `hello-from-host`, served by a listener you start on the box (`python3 -m http.server 8123` in a directory holding an
  `index.html` with that line, or the `nc` loop). Counters in `/var/log/herd/vm-dev.log` show `outbound opened=1` and
  `to_host`/`to_guest` > 0. **Remove the `hostrelay=` rewrite from `connect_arm`/`taoxichuan/src/main.rs`** (it exists only for
  the `10.0.2.2` alias, which is dropped); the guest connects to the address it is given.
- **H2:** char-device open: `dd if=/etc/hostname of=/dev/null` works (`/dev/null` open returns EACCES today).
- **H3:** write-after-create: `sh -c 'echo x > /tmp/f; cat /tmp/f'` prints `x` and the file's size is 2.
- Zero `Fault|PANIC|sig!` in a passing boot; re-run stage 6's G1-G3 once at the end.

If H1 still hangs on the LAN IP, your first probe is the one session A suggested: print the guest's smoltcp socket state per
wait lap in `finish_connect_wait`, and whether the NIC rx ring is drained while it waits.
Do not start DNS, `apk update`, TLS or the clock.

## Ops traps (all cost time last session)

- `scripts/install_kernel_amd64.sh` defaults to `/boot/akuma-amd64`, **the host kernel GRUB boots**. Always pass the explicit
  destination `/boot/akuma-guest-dev`. Never write anything to `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev` starts (kvm0 is exclusive). Edit **both** `/etc/herd/available/vm-dev.conf` and
  `/etc/herd/enabled/vm-dev.conf` (herd enable is a no-op when enabled) and confirm the `guest disk:` line in
  `/var/log/herd/vm-dev.log`. Fresh copy of `/boot/akuma-dev-root-apk.img.orig` before every boot. `vm_outbound = true` goes in
  the conf; `/root/dev-cycle/taoxichuan-s7/vm-dev.conf.stage7-hostrelay` is a template (drop `hostrelay=`).
- Stop `vm-dev` and the `:8123` listener at the end; restore both confs and `vm-serve-mc`.

## Commit rule (you have no clock, so this is by event, not by minutes)

**Commit and `git push litter taoxichuan-s4` after every boot cycle that taught you something and after every fix that
builds.** WIP commits are fine. Never push `origin`. Gate arch-specific code with `cfg` and keep amd64-only crates out of
aarch64 default-members.

## When to stop

Stop when H1 passes in a clean boot (then H2, H3 if cheap), after three boots on H1 with no new evidence, or at the quota
error. Count your own boots; do not try to measure minutes. Write `/root/dev-cycle/taoxichuan-s7/RESULT-B.md` (gates, the exact
failing step with console evidence, what is next) and end the turn.
