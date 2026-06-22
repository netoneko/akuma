# Native stack already gives userspace internet (box 0) — validated 2026-06-22

**Finding:** Akuma's *native* smoltcp stack (NIC0, QEMU SLIRP `-netdev user`) already
provides full userspace internet access from box 0 (the root namespace) — DNS,
outbound TCP to arbitrary ports/IPs (SLIRP NAT), HTTP, and HTTPS. This does **not**
require the NetBSD rump port; the rump `--net` box (sysproxy → kernel-as-client) is a
separate, larger architectural goal (see `userspace/rumpkernel/docs/RUMP_SYSPROXY.md`).

This was the validation bar: *ssh in, run sic, `wget https://ifconfig.me` returns a
real answer.* All met over the native stack.

## What was verified live (SSH `:2222` → in-kernel shell → userspace binaries)

| Check | Result |
|-------|--------|
| `nslookup ifconfig.me` / `irc.libera.chat` | real IPs via SLIRP DNS 10.0.2.3 |
| `/bin/busybox wget -O - http://ifconfig.me/ip` | real public IP (userspace HTTP, plain TCP) |
| `/bin/curl -sS https://ifconfig.me/ip` | real public IP — **HTTPS, mbedTLS handshake** ✅ |
| `/bin/curl --max-time 8 telnet://irc.libera.chat:6667` | real Libera Chat banner (plain TCP, non-80 port) → **sic will work** |

## The TLS-client gap (why curl, not busybox wget)

busybox `wget https://…` **fails** against ifconfig.me — not a networking problem.
busybox's built-in TLS (matrixssl-derived) is too limited to handshake with the
Google-fronted host (`wget: note: TLS certificate validation not implemented`, then
exit 1). Akuma's networking is fine; busybox's TLS isn't. So we added a real client.

## The curl binary

`bootstrap/bin/curl` — curl 8.11.1, **statically linked** aarch64-musl, **mbedTLS 3.6.6**
+ zlib (1.5 MB, stripped). Built natively in arm64 Alpine (= Akuma's musl/aarch64
target), so no cross-compile. It's in `bootstrap/bin/`, so `scripts/populate_disk.sh`
(`cp -rv /bootstrap/bin/* /mnt/disk/bin/`) puts it at `/bin/curl` on every disk rebuild.

Rebuild it: see `scripts/build_static_curl.sh` (Docker arm64 Alpine; mbedtls-static +
zlib-static from apk, curl configured `--with-mbedtls --disable-shared --enable-static`,
final link `make LDFLAGS=-all-static`). Gotcha: do **not** put `-static` in `CFLAGS` —
it breaks autotools' compiler probe ("C compiler cannot create executables"); static
link only at the `make` step.

## Scope / honesty

- This is **box 0** (root namespace), not a herd-managed `box`. `ps` shows BOX=0 for
  herd + httpd. No herd `--net` box was spawned; the rump `--net` auto-spawn (Phase 5)
  is still unimplemented.
- Nothing here went over the rump NetBSD stack. The only thing ever proven over rump is
  M1's purpose-built `rumphttp` (last session, host-only). `wget`/HTTPS/internet over
  rump is **not** done and needs the sysproxy/kernel-as-client path.
