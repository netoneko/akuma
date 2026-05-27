# apk-tools — Alpine Package Manager for Akuma

Pre-built Alpine Linux `apk` (static binary) packaged for Akuma OS.

## What This Is

This package downloads the official `apk-tools-static` binary from Alpine
Linux and packages it for the Akuma disk image. No cross-compilation —
Alpine publishes a statically-linked aarch64 binary that runs directly on
Akuma.

## Output

- `bootstrap/bin/apk` — the apk binary (static-PIE ELF64 aarch64, ~5 MB)
- `bootstrap/archives/apk-tools.tar` — archive containing the binary + signing keys
- `bootstrap/etc/apk/` — repository config, arch, signing keys
- `bootstrap/lib/apk/db/` — empty package database directory
- `bootstrap/var/cache/apk/` — empty package cache directory

## Build

```bash
cargo build --release -p apk-tools
```

Downloads are cached in `vendor/` and only fetched once.

## Usage on Akuma

```
apk update                    # fetch repository index
apk add <package>             # install a package
apk search <pattern>          # search available packages
apk info                      # list installed packages
apk del <package>             # remove a package
```

## ELF Loader Note

The `apk` binary is static-PIE (`ET_DYN`), not a traditional static
executable (`ET_EXEC`). Akuma's ELF loader was extended to support this:
segments are loaded at a base address of `0x1000_0000`, and the binary
self-relocates at startup via musl's `_dlstart_c`.

## Sources

| Component | URL |
|-----------|-----|
| apk-tools-static | `https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64/apk-tools-static-3.0.5-r0.apk` |
| alpine-keys | `https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64/alpine-keys-2.6-r0.apk` |
