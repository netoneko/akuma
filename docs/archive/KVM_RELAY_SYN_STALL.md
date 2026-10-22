# The KVM relay wedged: a reused 4-tuple, a SYN sent once, and a slot that never timed out

2026-10-22. ryzen metal, entry 28, `GUEST=serve` and `GUEST=gentoo`.

## Symptom

`ssh -p 2223` into the guest worked, then every later connection hung at the banner and finally
the port refused. The host's relay lines froze:

    [KVM] relay: slot 1 connecting ... in_flight=1 peer_wnd=0 ... host_recv_idle=231645

`scripts/kvm/relay_stress.py` (three concurrent sessions on the relay's two slots) reproduced it
on the metal in its first phase, every time on the old kernel. Under QEMU TCG the same script
passes 3 rounds of everything, including abusive clients — the wedge needs the metal's timing.

## Cause

Two different things were behind "the SYN is lost"; only one was a hang.

**The permanent wedge: a reused 4-tuple.** A forward slot used the same source port
(`40000 + slot`) for every connection, the gateway's TCP sends a SYN once (no retransmit timer),
and the relay never timed a `Connecting` slot out. A guest still holding the previous connection
on that tuple (a reset lost, a client killed mid-banner) does not answer a new SYN with a SYN-ACK,
so the slot stayed `connecting` for minutes; with both slots held nothing called `accept`, the
listen backlog (4) filled and the host began refusing. Not reproducible under QEMU TCG; reproduced
on the metal in the first phase of `relay_stress.py` (three overlapping sessions on two slots).
No permanent wedge since the source port rotates per connection (below).

**The "lost" SYNs the first fix retried were slow, not lost.** With the counters this fix added
(`syn_out`, `syn_to_guest`, `synack_in`, `stale_in`), a stress run read `syn_out=19
syn_to_guest=19 synack_in=17 stale_in=2` with `syn_retries=2`: every SYN reached the guest's
receive buffer, and the two missing SYN-ACKs arrived *after* the 1.5 s retry had moved the slot to a
new port (`stale_in`, answered with a reset). A one-vCPU guest whose kernel is not preempted runs
OpenSSH's key exchange to completion before its netpoll answers the next SYN. The retry timeout
is 4 s now (`SYN_RETRY_US`); at 4 s a run of 105 connections (stress plus bulk) had `syn_retries=0
stale_in=0`.

## Fix

- `Gateway::fwd_open` takes the slot's **next source-port generation** (`FWD_GENS = 8` per slot);
  only the current generation's segments are accepted, an older one is reset. A connection never
  reuses the previous 4-tuple.
- `Gateway::fwd_reopen(slot)` restarts a connection from a new port; the relay calls it after
  `SYN_RETRY_US` (4 s) of silence, up to `SYN_TRIES` (4), then resets the client and frees the
  slot instead of holding it.
- Tests in `crates/akuma-svm-amd64/tests/gateway_tcp.rs` against smoltcp: a dropped SYN is retried
  and the connection carries data; ten connections on one slot never reuse a port.
- Instrumentation that stays: the relay line prints `syn_tries / syn_retries / gave_up /
  connect_us_max`; a `[KVM] guest:` line prints cpu0 state, exits, the next timer deadline, the
  gateway's counters (`syn_out`, `syn_to_guest`, `synack_in`, `rst_in`, `stale_in`, `rx_to_guest`)
  and `rx_starved` (frames that waited for the guest to post a receive buffer).
- `scripts/kvm/relay_stress.py` (overlap, connect-and-drop, silent holds, garbage) and
  `scripts/kvm/relay_bulk.py` (timed bulk through the relay from the ryzen itself).

## Verified (metal, ryzen entry 28)

- 12 stress rounds on the serve guest and 4 on the Gentoo guest pass; one overlapped client in a
  round sometimes sees `Connection reset by peer` in key exchange (the guest closing a
  connection); a second ssh succeeds.
- **The relay path is fast.** From the host itself, 8 MiB through the relay: download 5.8-8.0 MB/s,
  upload 4.3-4.9 MB/s (ssh setup included), byte-exact, 105 connections answered, no retries.
- **The old open item is closed.** A 1 MiB guest-to-Mac download over the ryzen's wifi, which hung
  for 10 minutes before, takes 46 s (upload 28 s, md5-exact). The wifi is the limit: the host's own
  1 MiB download from `speed.cloudflare.com` takes 27 s (~39 KB/s).

## Two virtual processors did not help (tried the same evening)

`vcpus=2` in `/boot/kvm-guest` (serve guest, same kernel and 4 s retry): the guest sees 2 processors
(`grep -c ^processor /proc/cpuinfo`) and the stress passes, but the relay got *slower to answer*:
`syn_out=99 synack_in=92 stale_in=5 syn_retries=7`, against 0 retries over 105 connections with one
vCPU; one 8 MiB upload stalled for 33.7 s (243 KiB/s) before the next ran at 7.3 MB/s. Both
vCPUs run on the one host core the service task is pinned to, taking turns, so the guest gains no
compute and pays for the time-slicing. The option stays (`vcpus=N`) for experiments; the default
is one. More vCPUs only help once they run on separate host cores, which the single-task
hypervisor does not do.

## Still open

- `rx_starved` is ~20% of frames: the guest's NIC driver posts one receive buffer
  (`akuma-net-nic` without `net-noalloc`). It does not limit throughput (above), so the 8-slot
  ring was not switched on; `amd64`'s `net-noalloc` feature exists for the A/B and the feature's
  own build break (a missing `C` import in `device.rs`) is fixed.
- Idle cost: with a guest running, host `cpu0` is 100% by the tick accounting. The A/B (same
  kernel, entry 26, no guest) shows the same *total* (28% of 8 cores on an idle box), so the
  hypervisor is not the whole story, but the baseline is not idle either, and no thermal sensor is
  readable from Akuma to settle the fan question.

## Background

`docs/archive/AKUMA_KVM_AKUMA_GUEST.md` § ssh into the guest, `scripts/kvm/relay_stress.py`,
`crates/akuma-svm-amd64/src/gateway.rs`, `amd64/src/kvm/relay.rs`.
