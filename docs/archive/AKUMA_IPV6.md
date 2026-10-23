# IPv6 support: scoping the work (2026-10-23)

A survey of what it would take to add IPv6 to the native networking stack.
Not a bug — a missing feature, already pinned as one in
`docs/reference/subsystems/syscalls/net.md` § "No IPv6".

## Current state

- `sys_socket` accepts `domain == 2` (`AF_INET`) only; anything else,
  `AF_INET6` included, is `EAFNOSUPPORT` (97). Both syscall layers enforce
  this: `crates/akuma-syscalls-glue/src/net.rs` and `amd64/src/sock.rs:66`.
- That errno is load-bearing today:
  `crates/akuma-kernel-glue/src/rump_proxy.rs:523` relies on curl's
  `AF_INET6` probe failing so the box falls back to IPv4. The rump devbox
  path already translates `AF_INET6` for its own purposes
  (`crates/akuma-rump/src/syscall_translation.rs:378`).
- smoltcp is built `proto-ipv4` only (identical dep line in the root
  `Cargo.toml:830`, `crates/akuma-net/Cargo.toml:108` and
  `crates/akuma-net-nic/Cargo.toml:43`, smoltcp 0.12.0).
- There is not one `Ipv6` identifier in `crates/akuma-net/src/`.
- Address config today: `static_ipv4()` + DHCPv4
  (`crates/akuma-net/src/smoltcp_net/poll.rs:66`, `init.rs`). DNS resolves
  to `Ipv4Address` only (`crates/akuma-net/src/dns.rs`).

## What makes it cheap

- The stack is smoltcp; its IPv6 support is a feature flag (`proto-ipv6`),
  after which `IpAddress`/`IpEndpoint` become v4/v6 enums. The socket core
  (`crates/akuma-net/src/socket.rs`, 2114 lines) is largely
  family-agnostic already.
- The drivers are L2 Ethernet and untouched by IPv6: `akuma-net-nic`
  (virtio-net), rtl8169, rtw89. The `smoltcp::phy::Device` impls and the
  DMA contract do not change.
- The interface module is small (`smoltcp_net/iface.rs` is 58 lines).

## Work items, sized

| Piece | Size | Where |
|---|---|---|
| Enable `proto-ipv6` in the three Cargo.tomls; re-measure the extreme-size (4 MB floor) image | XS | root, `akuma-net`, `akuma-net-nic` |
| `sockaddr_in6` wire type + parse/marshal beside `SockAddrIn` (`socket.rs:111`) | S | `akuma-net`, `akuma-syscalls-linux` |
| Plumb `AF_INET6` through both syscall layers: every `SockAddrIn` site (~15 in glue, plus `amd64/src/sock.rs` / `amd64/src/net.rs`) gets a family dispatch | M | `akuma-syscalls-glue/src/net.rs` (2225 lines), `amd64/src/sock.rs`, `amd64/src/net.rs` (1384 lines) |
| Interface config: v6 address + default route, `::1`; NDP arrives with the smoltcp flag | S–M | `smoltcp_net/init.rs`, `smoltcp_net/iface.rs` |
| Address autoconfiguration | the open question | no DHCPv6 in smoltcp 0.12; static config is easy, SLAAC depends on how completely smoltcp handles router advertisements |
| DNS AAAA in the resolver | S | `crates/akuma-net/src/dns.rs` (249 lines) |
| Host tests (sockaddr parsing, family dispatch, errno map) + VM validation | M | existing suite patterns (`scripts/epoll_suite.py`, mem/futex suites) |

## Estimate

**1–2 weeks** for usable static-config IPv6: TCP/UDP over v6, `::1`,
AAAA lookups.

## Risks to settle before committing

1. **smoltcp 0.12 IPv6 maturity** — TCP-over-v6 yes; how completely
   NDISC/router discovery works in this version decides whether LAN
   autoconfiguration is free or hand-rolled.
2. **extreme-size code budget** — `proto-ipv6` grows `.text` measurably;
   the 4 MB floor profile may need the flag gated.
3. **Config plumbing** — everything flows from `static_ipv4()` + DHCPv4
   today; v6 needs a config source (kernel cmdline? herd keys?) and a
   definition of "connected" without DHCPv6.

## First spike, if taken up

Flip the flag, add `sockaddr_in6`, run `cargo check` on both kernels, and
measure: (a) what actually breaks, (b) the extreme-size `.text` delta,
(c) whether smoltcp 0.12 answers router solicitations.
