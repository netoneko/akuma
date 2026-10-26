# libakuma on plain Linux syscalls (idea, parked 2026-10-26)

**Status: an idea that came out of the Taoxichuan experiment. Not scheduled.** The owner's call: it is the right thing to do on its own merits, but it is not on the path to the ladder, which wants real `git` (a preparation for later stages). Goose is left to find its own way to stage 8's git gate; revisit this if that stalls.

## Why it came up
`userspace/scratch` (7,011 lines, a minimal git client over Smart HTTP, `libakuma` + `libakuma-tls` + `embedded-tls`) is a candidate for stage 8's `git clone` gate that would not need the process model: no fork, no `Command`, no OpenSSL, no dynamic loader.

## What was measured (2026-10-26, supervisor, no goose spend)
- `cd userspace && cargo build --release -p scratch --target x86_64-unknown-none` builds with no changes: 285,408 bytes, static ELF, 3.4 s. `libakuma` already carries an x86_64 entry stub and a second syscall table (`syscall::x86_64_nr`).
- `scratch`'s whole `libakuma` surface: `open close read_fd write_fd mkdir read_dir getcwd arg argc exit print print_dec time uptime` and `net::{resolve, TcpStream}`, plus `libakuma_tls::transport::TcpTransport`. Nothing Akuma-only except the three below.
- On x86_64 almost all of those map to Linux numbers (`openat`257, `read`0, `write`1, `mkdirat`258, `getdents64`217, `socket`41, `connect`42, `sendto`44, `recvfrom`45, `mmap`9 ...), which Taoxichuan already serves.
- Three calls sit in the **Akuma-private range** (`AKUMA_PRIVATE_BASE` = 0x1000, +300/+305/+319) and would return `-ENOSYS` on Taoxichuan:
  - `RESOLVE_HOST` (300): `net::resolve` -> `resolve_host`. It does not parse a dotted IP itself, so `scratch clone` fails before it sends a packet, even for an IP URL.
  - `UPTIME` (319): transfer-progress timing, 5 call sites (`http.rs`, `stream.rs`); a failed call returns an error value, so `uptime() - start` can wrap.
  - `TIME` (305): commit timestamps only (`commit.rs:108`); not on the clone path.
- **Not done:** `scratch` has not been run in a guest. The cheapest first test is the unmodified binary with no arguments (expect `scratch: missing command`), which proves the entry path on Taoxichuan.

## The change
Under `cfg(target_arch = "x86_64")` in `userspace/libakuma/src/lib.rs`:
1. `uptime()` and `time()` via `clock_gettime` (`CLOCK_MONOTONIC` / `CLOCK_REALTIME`, nr 228, already in the x86_64 table and served by Taoxichuan since M1).
2. `resolve_host()`: parse a dotted quad locally; otherwise a DNS A query over UDP sockets (`socket`/`sendto`/`recvfrom`) to the nameserver in `/etc/resolv.conf`, about 60 lines `no_std`. M2's DNS work shows the guest serves those sockets.
Keep the AArch64 path byte-identical (the Akuma-private calls stay there). A bigger version of the same idea is the one `libakuma` already hints at: grow `crates/akuma-syscalls-abi` to cover the 65 calls it uses.

## Why it is parked and not deleted
- Real `git` is wanted for what follows (the owner said cargo does not need it for registry fetches, but a working `git` is a better preparation than a 7k-line client that only clones).
- It is not kernel work and it is the right thing to do anyway, so it stays small enough to do on its own later: roughly 80 lines of `libakuma` and two boots, or a short goose session.
- Gate it with the zero-argument run first, then `scratch clone` from the mirror.

## Background
`docs/reference/userspace-layout.md` (the `scratch` row), `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (stage 8), `userspace/libakuma/src/lib.rs` (`syscall` module comment on the two tables).
