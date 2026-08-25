# No wall clock on Firecracker: the guest boots at epoch 0 and TLS never validates

**Date:** 2026-08-25
**Status:** OPEN — diagnosed precisely, not fixed. Worked around by vendoring.
**Where:** AWS `m6g.metal` Firecracker host, `overlays/devbox-firecracker-aws`
image, kernel `2c1eb9d0`, 4 vCPU / 8 GiB.

## Symptom

Every outbound HTTPS request in the guest fails on certificate *validity dates*,
not on trust or connectivity:

```
$ git clone --depth 1 https://github.com/netoneko/akuma.git
fatal: unable to access 'https://github.com/netoneko/akuma.git/':
  SSL certificate OpenSSL verify result: certificate is not yet valid (9)
```

"Not yet valid" is the tell. The CA bundle is present and correct
(`/etc/ssl/certs/ca-certificates.crt`, ~120 roots), DNS resolves, and the TCP
connection is established — the guest simply believes it is 1970, so every
certificate on the internet has a `notBefore` in its future.

```
$ date
Thu Jan  1 00:00:00 UTC 1970
$ date -s "2026-08-24 22:44:05"
date: can't set date: Function not implemented
```

Both halves matter: **the clock is wrong, and userspace cannot correct it.**

This blocks `git clone` over HTTPS and `cargo fetch` from crates.io alike. It is
*not* the old crates.io connect failure
(`docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md`) — that was fixed weeks earlier;
this is a different failure with a different message.

## Root cause

Three facts compose:

1. **Firecracker exposes no RTC on aarch64.** Its device model is deliberately
   minimal — virtio-net, virtio-block, virtio-vsock, a serial port. There is no
   PL031. A normal Linux guest also boots at epoch 0 here and fixes it with NTP
   in userspace; that is the standard arrangement, not a Firecracker defect.
2. **Akuma's only wall-clock source is that missing PL031.**
   `crates/akuma-timer/src/lib.rs` keeps `UTC_OFFSET_US`, an atomic seeded at
   boot from the QEMU virt PL031 at `0x0901_0000`. With no RTC, it stays
   `UTC_OFFSET_UNSET`, so `utc_time_us()` returns `None`.
3. **`clock_gettime(CLOCK_REALTIME)` silently degrades to 0.**
   `src/syscall/time.rs::sys_clock_gettime` does
   `crate::timer::utc_time_us().unwrap_or(0)` — an unset clock is reported as a
   *valid* timestamp of zero rather than as an error, so nothing upstream can
   tell "no clock" from "it is 1970".

## What is missing

Akuma implements `clock_gettime` (113) and `clock_getres` (114). It implements
**no way to set the clock at all**:

| syscall | aarch64 nr | status | needed for |
|---|---:|---|---|
| `clock_settime` | 112 | **missing** | `date -s`, any NTP client's final step |
| `adjtimex` | 171 | **missing** | `ntpd` slewing (gradual correction) |
| `clock_adjtime` | 266 | **missing** | same, per-clock form |

`settimeofday` does not exist on aarch64 — `clock_settime` is the one that
matters. busybox ships both `ntpd` and `rdate`, and both end in `clock_settime`,
so neither can work until it lands.

**The setter already exists on the kernel side.**
`akuma_timer::set_utc_time_us(unix_epoch_us, boot_uptime_us)` is public and does
exactly the right thing (stores epoch minus uptime as an offset). Wiring
`clock_settime` to it is a small change — a syscall table entry, a
`CLOCK_REALTIME`-only guard, and a `timespec` read from userspace. That single
syscall unblocks `date -s` and `rdate`, and is the prerequisite for everything
below.

## AWS time source, for when this is picked up

EC2 provides the **Amazon Time Sync Service** on the link-local address
`169.254.169.123` (NTP/UDP 123), free and not metered. From this microVM it is
two hops away and needs routing work that has not been done:

- The guest sits on `10.0.2.0/24` behind the host's tap + MASQUERADE
  (`files/20-net.sh`). `169.254.0.0/16` is link-local *on the host*, so guest
  traffic to `169.254.169.123` will not be forwarded as-is — it needs an
  explicit route plus a NAT rule, or a DNAT that rewrites a guest-visible
  address onto it.
- The simpler alternative is a public pool (`2.amazon.pool.ntp.org`,
  `pool.ntp.org`), which goes out the normal NAT path with no extra rules but
  depends on external reachability.
- Either way the guest needs `clock_settime` first; a working NTP client with no
  way to apply the result is worth nothing.

An even cheaper first step, if NTP is more than is wanted: seed the clock from
the **kernel command line**. `boot_args` is ours to set in
`/opt/akuma/guest/akuma-devbox.json` (currently just `console=ttyS0`), and the
host knows the time when it launches the VM. `akuma.epoch=<unix_seconds>` parsed
at boot into `set_utc_time_us` gives a clock accurate to the boot delay, with no
network, no NTP client and no routing. It does not survive long uptimes without
drift correction, which is what NTP is for — but it makes TLS work today.

## Workaround in place

The image ships **vendored crates** so no fetch is needed:
`/src/github.com/netoneko/akuma/vendor` plus a `[source.crates-io]` replacement
appended to that repo's `.cargo/config.toml`. With it, a full kernel build runs
offline in the guest — verified 2026-08-25, `cargo build --release` finished in
**4m 30s** on 4 vCPUs, 0 errors, producing a 4,200,152-byte
`target/aarch64-unknown-none/release/akuma`.

`cargo clean` is safe: it removes only `target/`, leaving `vendor/` and the
source replacement, so a clean rebuild still needs no network. `git checkout .`
is **not** safe — it reverts the appended `.cargo/config.toml` stanza and the
next build tries crates.io again.

Cloning *into* the guest over HTTPS remains blocked. The repo currently in the
image was cloned on the **host** and staged into the image before boot.

## Background

Found while getting `main` to clone-and-compile inside Firecracker on AWS. The
same session fixed the missing busybox applet links in the image builder and
found `docs/archive/HTTPD_STARVATION.md`.

Related: `docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md` (a *different* crates.io
failure, already fixed), `docs/archive/AKUMA_FIRECRACKER_TERRAFORM.md`,
`crates/akuma-timer/src/lib.rs`, `src/syscall/time.rs`.
