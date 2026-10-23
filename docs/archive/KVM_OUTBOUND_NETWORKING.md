# KVM: outbound networking for a guest (2026-10-23)

Asked for: "install software there and interact with the network". The gateway had been a deliberate
non-route (`docs/reference/subsystems/kvm.md`); this adds the opt-in `vm_outbound`. Reference behaviour is in
kvm.md § Outbound networking; this is how it was built and checked.

## Design

A slirp-style proxy, with the host as the NAT. The gateway (`crates/akuma-svm-amd64/src/gateway.rs`, pure,
`forbid(unsafe)`, no allocation) answers the guest as the far end of any outside address and the host
(`amd64/src/kvm/relay.rs`) makes the real connection:

1. guest SYN to `a.b.c.d:p` -> slot `Asked` (the SYN is held; the gateway's TCP has no retransmit timer and
   the guest's own SYN retries are ignored while the host dials);
2. the relay takes it (`Connecting`), dials with a non-blocking `socket_connect`, and polls the host socket's
   state (`tcp_recv_snapshot`: `ESTABLISHED` -> `out_accept`; anything but `SYN_SENT`, or 15 s -> `out_refuse`);
3. `out_accept` builds the SYN-ACK from the held SYN (`Tcp::accept` with a synthesized segment), source
   address and port = the real peer; `out_refuse` queues `RST|ACK` for the next `pump`;
4. data: the same `shuttle` as the forward slots (the old `pump_open`, factored out; forward behaviour is
   unchanged), both directions, FIN/`SHUT_WR` passed on, errors become a RST to the guest.

DNS is a datagram proxy (queue, verbatim forward, match by id), not a resolver: AAAA, TXT and the rest work as
the host's upstream answers them.

## Verified

* Host tests (`tests/gateway_tcp.rs`, smoltcp as the guest): off by default and nothing held; SYN held until
  the host answers, then 200 000 + 150 000 bytes byte-exact both ways and an orderly close; refusal resets the
  guest and frees the slot; the ninth connection is reset; subnet/loopback/multicast never held; DNS queued,
  forwarded verbatim, retry not double-queued, answer from `10.0.2.3:53`. `routable()` unit test. Spec round-trip.
* TCG rehearsal (`serve_rehearse.py --herd --outbound`): `wget` of a file on the Mac by LAN address; a 4 MiB
  download with a matching md5 in 0.8 s; `nslookup neverssl.com 10.0.2.3`; `wget` of the Alpine CDN's
  `latest-releases.yaml` over the real internet.
* Metal (ryzen entry 30, kernel `158ed98d`, `vm-serve-mc` with `vm_outbound = true`, 2 vCPUs on cores 4,6): from the
  host, `ssh -p 2223 root@127.0.0.1` into the guest, `echo nameserver 10.0.2.3 > /etc/resolv.conf` (the staged image
  had none; it persists in the image), then `wget` of `http://dl-cdn.alpinelinux.org/.../latest-releases.yaml`
  (`branch: v3.24`, `arch: x86_64`) and of `http://pool.ntp.org/` **by name**: both worked. The image has no `nslookup`.

## Open

* **`http://neverssl.com/` (34.223.124.45:80) is refused** from the guest: the relay counts a failed dial
  (`out_failed`, `gw_refused`) while the Mac reaches the same address with curl, and other destinations
  (the Alpine CDN, the Mac's LAN address) work. A log line for it
  (`relay: dial to a.b.c.d:p ended in state X after N ms`) was built but not read before this was parked;
  start there. It was seen under QEMU's user-mode NIC (slirp), so it may be slirp's own behaviour toward that address.
* No ICMP, no non-DNS UDP, no IPv6 (by design for now).
* The guest's `/etc/resolv.conf` is `10.0.2.3` only in the rehearsal image (`akuma_guest_rootfs.sh`); a real
  guest's DHCP already hands that address out.
* `herd` links `akuma-vm-spec` and validates keys itself: a herd built before `vm_outbound` existed refuses the
  conf (`vm_outbound: unknown key`). Rebuild it (`cargo build -p herd --target x86_64-unknown-none --release`
  in `userspace/`) wherever it is installed.
