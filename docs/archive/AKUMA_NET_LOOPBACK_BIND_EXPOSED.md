# A socket bound to `127.0.0.1` accepts connections from the network

**Status: FIXED in the kernel 2026-09-25, found 2026-09-24. Verified on two
of the three boxes (ryzen Firecracker guest, HP box metal).** The cause is in `crates/akuma-net`, which both
kernels share, so it applied to amd64 and AArch64, on metal and under
Firecracker. Both layers are fixed — fix-plan steps 3 and 4, see
[§ What was done](#what-was-done-2026-09-25). The herd stopgaps (steps 1 and
2) were not done, since the kernel fix makes them unnecessary for this
exposure. This file records the evidence, the two layers of the cause, the
fix plan, and what was done.

**On-box probe, 2026-09-25:** the ryzen Firecracker guest (`192.168.1.50`),
booted on `c9586004` with its disk unchanged. From the Mac, `nc -z
192.168.1.50 7117` is **closed** (it was open on `b4a55330`). In the guest,
`herd start kot` still reaches the daemon over loopback (`kot: already
running`). kot's own `0.0.0.0:9944` still answers, and the guest rejoined
the akuma-miot mesh. **Same day, the HP box on metal** (`192.168.1.120`,
RTL8169, installed with `install_kernel_amd64.sh` and `reboot -f`, `.good`
left as it was): `:7117` **closed** from the Mac, `herd start kot` over
loopback still works, and kot rejoined the mesh. So the RX-path change is
fine on the one NIC the fast lane doesn't run. **Not yet probed:** the Lima
AArch64 guest, still on an older kernel. **Seen on both boots and not
explained:** the self-test went from `797 passed, 0 failed` on the previous
kernel to `794 passed, 3 FAILED` (ryzen) and `748 passed, 3 FAILED` (metal).
The same three every time:
`spawn: the child's registered table holds fd 0/1/2 as Stdin/Stdout/Stderr`.
Boots and services are otherwise normal. Don't promote this kernel to
`.good` until that's understood.

## Symptom

`herd`'s control socket (`userspace/herd/src/main.rs`,
`HERD_CONTROL_ADDR = "127.0.0.1:7117"`, the channel `herd start`/`herd stop`
use since 2026-09-24) is meant to be reachable only from the box itself. It
isn't. It answered a plain TCP connect, with no command sent, from outside
each of the three machines tested:

| machine | kernel | probed from | `:7117` |
|---|---|---|---|
| the HP box (bare metal, RTL8169), `192.168.1.120` | amd64 `b4a55330` | a Mac on the same LAN | **open** |
| Firecracker guest on the ryzen host, `192.168.1.50` (proxy-ARP'd onto the LAN) | amd64 | the same Mac | **open** |
| Firecracker guest inside Lima `fc`, `10.0.2.15` | AArch64 `66416bc9` | the `fc` VM, over `tap0` | **open** |

The third one isn't reachable from the LAN only because Lima doesn't forward
that port. It's still open to its host.

The control protocol is one unauthenticated line (`stop kot`, `start kot`).
Anyone who can reach the port can stop any service on the box, and a stopped
service stays `Halted` until someone runs `herd start` or reboots. That
includes `sshd`, the only way in to most of these machines. It's the same
exposure as an unauthenticated root shell restricted to herd's verbs.

## Root cause: two layers

### 1. `bind()` discards the address

`crates/akuma-syscalls-glue/src/net.rs` `sys_bind` copies the whole
`sockaddr_in` in, prints the IP in its trace line, and passes it to
`akuma_net::socket::socket_bind`. That function keeps only the port:

```rust
// crates/akuma-net/src/socket.rs, socket_bind
let port = bind_port_for(addr.port, alloc_ephemeral_port);
sock.bind_port = Some(port);
```

`KernelSocket` has no field that could hold the address. `socket_listen` then
builds `KernelSocket::new_listener(port, backlog)`, and every backlog handle
does

```rust
let _ = socket.listen(port);   // smoltcp: IpListenEndpoint { addr: None, port }
```

A smoltcp listen endpoint with `addr: None` accepts a SYN to that port on any
address the interface owns. So `bind("127.0.0.1:7117")` behaves exactly like
`bind("0.0.0.0:7117")`, and nothing tells the caller. The `EADDRINUSE` check
added on 2026-09-20 (`AKUMA_NET_BIND_NO_ADDRINUSE.md`) compares ports only,
for the same reason. Two sockets bound to `127.0.0.1:N` and
`192.168.1.120:N`, which Linux allows side by side, collide here.

### 2. Loopback is an address on the NIC's interface, not its own interface

`crates/akuma-net/src/smoltcp_net/init.rs` (and the DHCP paths in
`poll.rs`) give the one smoltcp `Interface` both the NIC's address and
`127.0.0.1/8`. `LoopbackAwareDevice` (`crates/akuma-net-nic/src/loopback.rs`)
keeps loopback traffic off the wire in one direction only:

- **TX:** `LoopbackAwareTxToken::consume` sends any frame for which
  `is_loopback_frame` is true (IPv4 source *or* destination in 127/8, or the
  ARP equivalent) into the internal ring instead of the NIC.
- **RX:** `LoopbackAwareDevice::receive` pops the ring first, otherwise hands
  up whatever the NIC delivered, **unchecked**. Nothing drops a frame from
  the wire whose source or destination is 127/8.

Because the interface owns `127.0.0.1/8`, smoltcp accepts such a frame as
local. Linux drops these as martians: `127/8` on a non-loopback device fails
`ip_route_input` unless `route_localnet` is set.

Layer 1 is what the probes above hit: they went to the NIC's own address.
Layer 2 is why a fix in herd alone (checking the peer is 127.0.0.1) isn't
enough. A peer address of `127.0.0.1` on `accept()` doesn't prove the
connection came from this box.

**Unverified:** whether layer 2 can actually be exploited. A spoofed SYN from
`127.0.0.1` gets its SYN-ACK looped back internally, so the attacker never
sees it and would have to guess the initial sequence number to finish the
handshake. How hard that is depends on how random smoltcp's ISNs are here.
That hasn't been checked, and no spoofing was attempted.

## What else this touches

Anything on Akuma that binds loopback expecting privacy:

- `herd`'s control socket (`127.0.0.1:7117`), above.
- `meow`'s litter-raft hub, `127.0.0.1:7700` (`userspace/meow`,
  `AKUMA_NET_BIND_NO_ADDRINUSE.md`).
- Any Linux program ported here that binds loopback on purpose: a debug
  endpoint, a local database, an admin API. Such a program is correct on Linux
  and exposed here.

Services that bind `0.0.0.0` on purpose (sshd, akuma-miot's `kot` on 9944,
which is mTLS-pinned) don't change.

## Fix plan

In order. Each step stands alone and closes more than the one before it.

1. **herd: authenticate the control channel (stopgap, userspace only).** At
   startup the daemon writes a random token to a root-only file (for example
   `/run/herd.token`, 0600, regenerated on every start). The CLI reads it and
   sends it first on the request line, and the daemon refuses anything else
   with `err`. This works whatever the kernel does with addresses, since a
   network peer can't read the file.
2. **herd: check the peer on accept (second layer).** Refuse any connection
   whose `accept()` peer isn't `127.0.0.1`. `socket_accept` already returns
   `remote_endpoint()`. Cheap, but layer 2 makes it spoofable, so it doesn't
   replace step 1.
3. **Kernel: honour the bound address (layer 1).** Keep the address in
   `KernelSocket` alongside `bind_port`. Pass it to smoltcp as
   `IpListenEndpoint { addr: Some(ip), port }` in every `listen(port)` call
   (`new_listener`, and the backlog refills at `socket.rs` ~1156, 1166, 1239).
   Scope the `EADDRINUSE` check by address, with `0.0.0.0` overlapping
   everything, as Linux does. Bind UDP the same way (`udp_socket_bind` takes
   only a port too). Refuse an address the interface doesn't own with
   `EADDRNOTAVAIL`, instead of silently listening everywhere.
4. **Kernel: drop martians on RX (layer 2).** In
   `LoopbackAwareDevice::receive`, drop any frame from the NIC (`FrameSource::External`)
   whose IPv4 source or destination is in 127/8, using the
   `is_loopback_frame` test TX already uses. Count the drops in `nicstat`
   next to `record_loopback`. Without this, step 3's
   `addr: Some(127.0.0.1)` still admits a frame from the wire addressed to
   127.0.0.1.

Steps 3 and 4 together are the real fix; 1 and 2 cover the time until then.

## What was done (2026-09-25)

Steps 3 and 4, as planned, with these specifics:

- **Layer 1.** `KernelSocket` has a `bind_ip` (`[0; 4]` = `INADDR_ANY`).
  `socket_bind` records it, and `socket::listen_endpoint(ip, port)` turns it
  into the smoltcp endpoint (`addr: None` only for the wildcard). It is used
  on every listen (`new_listener`, both refills in `listener_refresh`, the
  replacement in `socket_accept`), on `udp_socket_bind` (which now takes the
  address), and as the source endpoint of a TCP `connect` from a bound socket.
  `EADDRINUSE` compares address as well as port through
  `socket::bind_addrs_overlap`. `socket::bind_addr_check` refuses an address
  the interface doesn't own with `EADDRNOTAVAIL`. `getsockname` reports the
  bound address; an unbound or wildcard socket still reports the NIC's, as it
  did before.
- **Layer 2.** `LoopbackAwareDevice::receive` drops a frame from the external
  device when `is_loopback_frame` (the TX test) matches it, and counts it in
  `martian_drop_count()` (re-exported from `akuma_net::smoltcp_net`). The
  counter is always on, not in `nicstat`, which only exists in `net-profile`
  builds. One `receive` call discards at most `MARTIAN_DROPS_PER_RECEIVE`
  (16) and then reports no frame, so a flood can't hold `NETWORK` for a whole
  ring of drops. The frames behind it are picked up on the next lap.
- **New divergence from Linux (pinned in a test):** binding `127.0.0.2` or any
  other 127/8 address except `127.0.0.1` returns `EADDRNOTAVAIL`. Linux allows
  it because `lo` has a `/8` local route. Here the interface holds exactly
  `127.0.0.1`, and smoltcp drops traffic to any other 127/8 address before a
  socket sees it, so such a bind could never have received anything. It used
  to "succeed" by listening on every address.

Tests: `akuma-net` `tests::bind_address_tests` has the pure rules plus three
tests that run a real smoltcp `Interface` holding both `127.0.0.1/8` and a NIC
address over smoltcp's loopback device. A wildcard listener is reached on both
addresses (the control), a `127.0.0.1` listener is not reached through the NIC
address, and the reverse holds too. `akuma-net-nic` `loopback::tests` drives
`receive` over a scripted wire: IPv4 and ARP martians are dropped and counted,
the frame behind them still arrives, ordinary traffic is untouched, and a
burst is bounded per call.

## How to verify a fix

- **Host test, pure** (next to `bind_then_lookup`-style tests): a listener
  bound to `127.0.0.1:N` must not match a SYN to `<nic ip>:N`. One bound to
  `0.0.0.0:N` must match both. Two binds to `127.0.0.1:N` and `<nic ip>:N`
  must both succeed. Two binds to `0.0.0.0:N` and `127.0.0.1:N` must give
  `EADDRINUSE`.
- **Host test for step 4:** feed `receive` an external frame with a 127/8
  source and destination. It must be dropped and counted, not handed up.
- **On a box** (still to do): from another machine, `nc -z -w 4 <box> 7117` must fail,
  while `herd start <svc>` on the box still works. Repeat on all three rows
  of the table above; the Lima guest is probed from `fc`, not the LAN.
- **Under QEMU, A/B** (2026-09-25; fixed `c9586004` against pre-fix
  `d763cf1a`, same disk, both architectures). The guest NIC sits on a QEMU hub
  with the SLIRP netdev and a `-netdev socket,udp=` port, so a host script can
  put raw Ethernet frames on the guest's wire. SLIRP `hostfwd` connections
  arrive addressed to `10.0.2.15`, which makes them "the network" for layer 1.
  Evidence for layer 2 is `/proc/net/tcp`'s `BACKLOG` column: a half-open
  handshake moves a handle from listening to pending. Injected SYNs use an
  unused source (`10.0.2.77`), because the SLIRP gateway would reset the
  SYN-ACK and empty the backlog before it can be read.

  | check | AArch64 fixed | AArch64 pre-fix | amd64 fixed | amd64 pre-fix |
  |---|---|---|---|---|
  | `127.0.0.1` listener reached via NIC address | no | **yes** | no (herd `:7117`, RST) | **yes** (herd replied `err nosuchservice: …`) |
  | control: `0.0.0.0` / NIC-address listener reached | yes | yes | yes (sshd) | yes |
  | `127.0.0.1:N` + `10.0.2.15:N` coexist | yes | **no** | not run | not run |
  | `0.0.0.0:N` over a specific bind | `EADDRINUSE` | `EADDRINUSE` | `EADDRINUSE` | `EADDRINUSE` |
  | bind `10.9.9.9` (unowned) | `EADDRNOTAVAIL` | **succeeds, listens** | `EADDRNOTAVAIL` | **succeeds, listens** |
  | injected SYN to dst `127.0.0.1` | dropped | **accepted** (`0/1/0`) | dropped | **accepted** (`7/1/0`) |
  | control: injected SYN to `10.0.2.15` | accepted | accepted | accepted | accepted |
  | injected SYN from src `127.0.0.1` | dropped | dropped | not run | not run |

  The last row doesn't show the fix working: smoltcp already refuses a
  segment from a loopback source. The live layer-2 hole was the destination.
  amd64 used herd's own non-blocking listener, and the checks marked "not
  run" were skipped, because there a blocking `accept` in a busybox `nc -l`
  wedges sshd on both kernels. Also seen on both kernels under QEMU only, so
  not caused by this fix: an in-guest TCP connect to `127.0.0.1` mostly times
  out (busybox `nc`: 0/8 connects on AArch64 pre-fix, 1/8 fixed), and the
  herd CLI hung or reported `cannot reach the herd daemon` on amd64. Real
  boxes don't show it: `herd start kot` over loopback works on the ryzen
  guest and the HP box, as recorded above.

## Found how

While getting akuma-miot's `mimi` cat back up on the Lima Firecracker guest.
Its pid-1 herd predated the control socket, so `herd start kot` reported
"cannot reach the herd daemon on 127.0.0.1:7117". The operator pointed out
that Akuma doesn't distinguish a loopback bind from an interface bind, and a
LAN probe of the other two Akuma boxes confirmed it. akuma-miot's side of
that session is in its `docs/AGENT_STATE_MACHINE.md` and `HANDOFF.md`.
