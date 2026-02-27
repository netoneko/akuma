# apk-tools — Build Notes

## Approach

Unlike XBPS (which is cross-compiled from source with 6 dependency libraries),
apk-tools uses the pre-built `apk-tools-static` binary published by Alpine
Linux. This avoids the entire cross-compilation dependency chain.

| | XBPS | apk-tools |
|---|---|---|
| Method | Cross-compile from source | Download pre-built binary |
| Dependencies | zlib, lz4, zstd, LibreSSL, libarchive | None |
| Build fixes needed | 11 | 0 |
| Archive size | 56 MB (16 binaries) | 5 MB (1 binary) |

## What build.rs Does

1. Downloads `apk-tools-static-3.0.5-r0.apk` from Alpine CDN (cached in `vendor/`)
2. Downloads `alpine-keys-2.6-r0.apk` (signing keys for package verification)
3. Extracts `sbin/apk.static`, renames to `bin/apk`
4. Extracts signing keys from `etc/apk/keys/` and `usr/share/apk/keys/`
5. Creates `dist/apk-tools.tar` containing `bin/`, `etc/`, `usr/`
6. Copies the archive to `bootstrap/archives/apk-tools.tar`
7. Copies `apk` binary directly to `bootstrap/bin/apk`
8. Creates APK config in `bootstrap/`:
   - `etc/apk/repositories` — Alpine main + community repos
   - `etc/apk/arch` — `aarch64`
   - `etc/apk/keys/` — Alpine signing keys
   - `var/cache/apk/` — package download cache (empty)
   - `lib/apk/db/` — package database (empty)

## Alpine .apk Package Format

An `.apk` package is a gzipped tar archive containing:

```
.SIGN.RSA.<keyname>     # detached signature
.PKGINFO                # package metadata
<filesystem tree>       # actual files (usr/, etc/, sbin/, ...)
```

This means standard `tar xzf` extracts them.

## Static-PIE Binary

The `apk.static` binary is compiled as static-PIE (`-static-pie`), meaning:

- ELF type: `ET_DYN` (not `ET_EXEC`)
- Statically linked (no shared libraries, no interpreter)
- Position-independent — loaded at a kernel-chosen base address
- Self-relocating — musl's startup code (`_dlstart_c`) applies RELR
  relocations before calling `main`

This required extending Akuma's ELF loader (`src/elf_loader.rs`) to accept
`ET_DYN` binaries. Segments are mapped at base `0x1000_0000` + `p_vaddr`.
The kernel skips relocation processing — the binary handles it internally.

## Bootstrap Directory Layout

After build, the following APK-related paths exist in `bootstrap/`:

```
bootstrap/
├── bin/apk                          # the apk binary
├── etc/apk/
│   ├── arch                         # "aarch64"
│   ├── keys/                        # Alpine signing keys
│   │   ├── alpine-devel@...58199dcc.rsa.pub
│   │   └── alpine-devel@...616ae350.rsa.pub
│   └── repositories                 # repo URLs
├── lib/apk/db/                      # package database (empty)
├── var/cache/apk/                   # download cache (empty)
└── archives/apk-tools.tar           # installable archive
```

## Repository Configuration

```
http://dl-cdn.alpinelinux.org/alpine/latest-stable/main
http://dl-cdn.alpinelinux.org/alpine/latest-stable/community
```

Architecture: `aarch64` (Alpine's native aarch64 musl repo).

## Potential Syscall Requirements

APK will need many of the same syscalls that XBPS required. Known needs:

| Syscall | Status | Notes |
|---------|--------|-------|
| uname | Implemented | Architecture detection |
| socket/connect/sendto/recvfrom | Implemented | Network + DNS |
| sendmsg/recvmsg | Implemented | DNS resolution |
| openat/read/write/close | Implemented | File I/O |
| mkdirat/unlinkat | Implemented | Directory/file operations |
| fstat/newfstatat | Implemented | File metadata |
| mmap/munmap/brk | Implemented | Memory management |
| symlinkat | **Missing** | Packages contain symlinks |
| readlinkat | **Missing** | Package verification |
| fchmodat | **Missing** | File permissions |
| fchownat | **Missing** | File ownership |

The missing syscalls will need to be implemented before `apk add` can
install packages that contain symlinks (which is most packages).
