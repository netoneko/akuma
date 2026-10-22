# The KVM relay wedged: a SYN the guest never answered held a slot forever

2026-10-22. ryzen metal, entry 28, `GUEST=serve` and `GUEST=gentoo`.

## Symptom

`ssh -p 2223` into the guest worked, then every later connection hung at the banner and finally
the port refused. The host's relay lines froze:

    [KVM] relay: slot 1 connecting ... in_flight=1 peer_wnd=0 ... host_recv_idle=231645

`scripts/kvm/relay_stress.py` (three concurrent sessions on the relay's two slots) reproduced it
on the metal in its first phase, every time on the old kernel. Under QEMU TCG the same script
passes 3 rounds of everything, including abusive clients — the wedge needs the metal's timing.

## Cause (what is established, and what is not)

Established, from the code:

- the gateway's TCP (`akuma-svm-amd64::tcp`) has **no retransmission timer**: a SYN is sent once;
- a forward slot used the **same source port** (`40000 + slot`) for every connection, so a guest
  still holding the previous connection half-open would ignore the next SYN;
- the relay never timed a `Connecting` slot out, so one lost SYN held it for good; with both slots
  held nothing called `accept`, the listen backlog (4) filled and the host began refusing.

Measured with the instrumented relay (this fix): over ~14 stress rounds the relay sent **27 SYN
retries and gave up 0 times**, i.e. SYNs are lost about twice per round of three simultaneous
connects, and a second SYN recovers each. Gateway counters at the time: `gw_lost=0`,
`gw_queued=0`, `gw_resets=0`, so the SYN is not lost in the gateway's queue.

**Not established: where the first SYN goes.** The guest polls its NIC (no interrupt is raised),
so a frame is seen at its next netpoll lap; a SYN landing while the guest is mid-lap or halted is
the suspect. The new `connect_us_max` (slowest SYN-to-established) separates "answered late" from
"lost" and is the next thing to read.

## Fix

- `Gateway::fwd_open` takes the slot's **next source-port generation** (`FWD_GENS = 8` per slot);
  only the current generation's segments are accepted, an older one is reset. A connection never
  reuses the previous 4-tuple.
- `Gateway::fwd_reopen(slot)` restarts a connection from a new port; the relay calls it after
  `SYN_RETRY_US` (1.5 s) of silence, up to `SYN_TRIES` (4), then resets the client and frees the
  slot instead of holding it.
- Tests in `crates/akuma-svm-amd64/tests/gateway_tcp.rs` against smoltcp: a dropped SYN is retried
  and the connection carries data; ten connections on one slot never reuse a port.
- Instrumentation that stays: the relay line prints `syn_tries / syn_retries / gave_up /
  connect_us_max`; a `[KVM] guest:` line prints cpu0 state, exits, the next timer deadline and the
  gateway's counters while a connection is waiting.

## Verified

Metal, same kernel: 12 stress rounds (overlap x3, connect-and-drop, silent holds beyond the slots,
garbage then close) pass, then 4 more against the Gentoo guest. One overlapped client in a round
sometimes sees `Connection reset by peer` in key exchange (the guest closing a connection); a
retry of the ssh succeeds.

## Still open

- Why the first SYN is dropped (above).
- Idle cost: with a guest running, host `cpu0` is 100% by the tick accounting. The A/B (same
  kernel, entry 26, no guest) shows the same *total* (28% of 8 cores on an idle box), so the
  hypervisor is not the whole story, but the baseline is not idle either.

## Background

`docs/archive/AKUMA_KVM_AKUMA_GUEST.md` § ssh into the guest, `scripts/kvm/relay_stress.py`,
`crates/akuma-svm-amd64/src/gateway.rs`, `amd64/src/kvm/relay.rs`.
