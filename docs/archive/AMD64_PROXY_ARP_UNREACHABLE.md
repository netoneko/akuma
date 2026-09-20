# Two Akuma boxes cannot reach each other across proxy-ARP; every other host can

**Status: OPEN, not diagnosed.** Found 2026-09-20 while joining two litters —
one on the bare-metal trashcan (Akuma/amd64, wired), one in a Firecracker guest
on the Ryzen laptop (Akuma/amd64, reached through proxy-ARP over WiFi). A
workaround is in place (§5) and the cause is not established.

## 1. The shape

The guest has a **real LAN address, 192.168.1.50**, published by proxy-ARP on
the Ryzen host — the arrangement `LITTER_RAFT_LOOP.md` § "Deployment topology"
describes, because 802.11 will not bridge frames for another MAC.

Reachability, all measured the same afternoon:

| from | to | result |
|---|---|---|
| Mac (192.168.1.203) | guest `.50:2222` | **OPEN in 0.06 s** |
| Mac | trashcan `.123:7700` | OPEN in 0.01 s |
| trashcan (Akuma) | Mac `.203:8099` | works, instant |
| trashcan (Akuma) | ryzen host `.126:11435` | works, instant |
| guest (Akuma) | Mac `.203:8099` | works |
| guest (Akuma) | ryzen host `.126:11435` | works |
| **trashcan (Akuma)** | **guest `.50`, any port** | **nothing, ~10 s then failure** |
| **guest (Akuma)** | **trashcan `.123`, any port** | **nothing, ~10 s then failure** |

Every pair works except **Akuma → Akuma**, and the Mac traverses the exact same
proxy-ARP path to the guest in 60 ms. Both Akuma boxes reach the Ryzen host's
*own* address instantly, so it is not "Akuma cannot cross that WiFi hop".

## 2. The host side is configured correctly

On the Ryzen host, checked while the failure was live:

```
proxy_arp: wlp2s0=1  tap0=1  all=1
ip_forward: 1
ip route get 192.168.1.50  ->  dev tap0
ip neigh: 192.168.1.123 dev wlp2s0 lladdr 60:02:92:61:4e:73 REACHABLE
          192.168.1.50  dev tap0   lladdr 02:fc:00:00:00:01 STALE
```

Nothing was changed here during the investigation — **no firewall rule was
added or removed by this session.** (The leftover `DNAT`/`MASQUERADE` that
`LITTER_RELAY_TOPOLOGY.md` § "Still to do" lists are from an earlier session and
were never inspected; `sudo` on that host needs a password.)

## 3. The best current guess

For the trashcan to answer the guest at all it must resolve `192.168.1.50`,
which is on-link for its `/24` and therefore an **ARP** question that only the
Ryzen host's proxy-ARP can answer. A normal host accepts that reply; whether
Akuma's smoltcp neighbour layer accepts a reply whose sender hardware address
belongs to a different machine, or whether its request is even answered, is
**not established**. That asymmetry — a single ARP exchange being the one thing
the working cases do not need — is the first thing to test.

Note this breaks **both** directions with one cause: the guest's SYN can reach
the trashcan fine, but the trashcan's SYN-ACK has nowhere to go if it cannot
resolve `.50`, so the guest sees silence and blames its own outbound path.

## 4. A diagnostic trap met on the way

`hget` reported `ConnectionRefused` after **exactly 10 s** for these failures.
Ten seconds is `akuma_net`'s `CONNECT_TIMEOUT_US`, and `socket.rs` states that an
abandoned connect should surface as **`ETIMEDOUT`**, not `ECONNREFUSED`. So the
error string here reads as "something actively refused me" when the observed
behaviour is "nobody answered the SYN", which sent this investigation looking
for a firewall for a while. Whether the mislabel is in `hget`'s error mapping or
in the socket layer is worth a look — `ECONNREFUSED` and a connect timeout
should never be spelled the same way.

## 5. Workaround in place

Both Akuma boxes reach the Ryzen host's own address, so hub traffic is relayed
through it rather than sent between them. `/tmp/ryzen_bridges.py` on the Ryzen
host (started with `setsid`, logs to `/tmp/bridges.log`):

```
11435 -> 127.0.0.1:11434      ollama, for the guest's LLM calls
 7701 -> 192.168.1.50:7700    the guest's hub, dialled by the trashcan
 7702 -> 192.168.1.123:7700   the trashcan's hub, dialled by the guest
```

Each litter's `litter_static_peers` then names `192.168.1.126:770x` instead of
the other box. With that in place the trashcan **discovered** its peer for the
first time (`[event] static peer ryzen discovered at 192.168.1.126:7701`), which
is what proves the workaround carries real traffic.

## 6. To do

* Establish whether it is ARP: watch for the request and the proxy reply on the
  Ryzen host (`tcpdump -i wlp2s0 arp`, needs root there), while the trashcan
  tries `.50`.
* If the reply is on the wire, the question moves into `akuma-net`'s neighbour
  cache: whether a reply is accepted, and whether a **negative** entry from the
  period when the peer was genuinely down is being held far too long — both
  boxes spent hours unable to reach each other before this was measured.
* Fix the `ECONNREFUSED`-for-timeout spelling in §4 regardless; it is cheap and
  it actively misleads.

## 7. Background

* `userspace/meow/docs/LITTER_RAFT_LOOP.md` § "Deployment topology" — why the
  guest is on proxy-ARP at all (WiFi cannot be bridged), including the
  `noprefixroute` detail.
* [`AMD64_SPAWNED_THREAD_NEVER_RUNS.md`](AMD64_SPAWNED_THREAD_NEVER_RUNS.md) —
  the *other* reason the two litters did not join, and the one that outlived
  this workaround.
