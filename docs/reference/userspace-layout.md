# Userspace Layout

**Grade: B** (verify behaviour) — accurate as of 2026-08-10; drifts whenever a
member is added/removed from `userspace/` without this doc being updated.

Current-state index of `userspace/` top-level members (musl libc ELF binaries
and libraries, built by [`userspace/build.sh`](../../userspace/build.sh) via
`userspace/Cargo.toml`). For build mechanics see
[`build-system.md`](build-system.md#userspace-build); for docs on individual
binaries see [`../userspace/`](../userspace/) (covers the ones with dedicated
pages — not every member has one).

| Member | Purpose |
|---|---|
| `libakuma` | Core Rust syscall wrapper library — foundation for all native userspace binaries |
| `libakuma-tls` | TLS 1.3 client connections for userspace programs |
| `apk-tools` | Alpine apk bootstrap tooling (no binary — build.rs deploys bootstrap assets directly) |
| `herd` | Process supervisor — background services, auto-restart, config in `/etc/herd/` |
| `box` | Container manager: `box run` (Docker images on an overlay root), `box pull`, `box open/close/ps/inspect` |
| `meow` | AI coding assistant — LLM chat client with filesystem/network tool calling |
| `nca` | Build wrapper for [native-cli-ai](https://github.com/netoneko/native-cli-ai) (submodule); no binary in `/bin` — build.rs deploys to `bootstrap/bin/nca` |
| `tcc` | Tiny C Compiler port — compile and run C programs on-target |
| `neatvi` | Vi-like text editor, compilable on-target with TCC |
| `sshd` | Userspace SSH server, plus a companion SSH client (`ssh`, a second `[[bin]]` in the same package) |
| `paws` | **EXPERIMENTAL — `extreme-size` demo only.** Minimal first-party shell (598 lines, pure `libakuma`): 8 RO-mapped pages vs busybox's 265, which is what makes it usable as the login shell on a 4 MB box whose file-page dedup cache holds 128. **Not busybox/ash compatible** — hand-rolled parser, fixed builtin list, no `exec`/`printf`/`test`, unreliable pipes and redirection. Fine for `sshd --shell /bin/paws` execing one binary (that is how `acceptance/05`, and formerly `acceptance/archive/08`, pass at 4.0 MB); do not point real shell scripts at it. Originally removed at `c0af6c7`, revived 2026-08-10. The page-count measurements behind that comparison (busybox = 265 RO pages, and why the file-page cache cannot hold one below ~8 MB of RAM) are in [`../archive/FPCACHE_UNDERSIZED_AT_LOW_RAM.md`](../archive/FPCACHE_UNDERSIZED_AT_LOW_RAM.md) — a `BUSYBOX_TOYBOX_SIZING.md` was referenced here and in that doc but never written |
| `httpd` | HTTP server |
| `tar` | Tar extraction. A **library** (`akuma_tar`) that `box` links for layer extraction, plus a thin `/bin/tar` CLI. Header parsing (`format.rs`) is host-testable |
| `scratch` | Minimal Git client (Git Smart HTTP) — was removed then reverted; see `docs/archive/TRIM_FAT_PART_2.md`'s "Removed: scratch" section, which predates the revert |
| `llama.cpp` | llama.cpp port — LLM inference on-device |
| `rumpkernel` | NetBSD rump kernel port — real network stack (DHCP/HTTP/HTTPS) |
| `wavplay` | Streams a WAV file to `/dev/dsp` (VirtIO-sound); fixed-footprint, file-backed |
| `forktest` | Go fork/clone stress-testing harness; `forktest/c_stress/` holds pure-C musl-static control binaries (mmap/fault/futex/thread-spawn probes) used to disambiguate kernel bugs from Go-runtime bugs |
| `hiss` | New/WIP crate, not yet a `userspace/Cargo.toml` workspace member — audio-related, reuses `wavplay`'s playback logic |
| `echo2`, `elftest`, `hello`, `stackstress`, `termtest`, `allocstress` | Small repro/stress/example programs (echo loop, ELF-load + subprocess-spawn check, long-running argv/streaming test, exception-stack-overflow stress, terminal-attribute test, allocator-stress test) |
| `nettest` | Guest-side **network** client probes for the delayed-first-byte hunt (`docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`): `nettest` (libcurl), `nettest-std` (`std::net` + `poll(2)` + sync rustls) and `nettest-reqwest` (tokio + hyper + reqwest — nca's stack). Not a workspace member; musl `std` binaries built by `nettest/rust/build-musl.sh` into `bootstrap/bin` |
| `ncaprobe` | Guest-side **async-subprocess / epoll / tty** probes for the child-process hang (`docs/archive/TOKIO_PIPE_EPOLL_HANG.md`): `tokio`, `eofedge`, `epoll`, `cross`, `fds`, `waitid`, `raw`. Like `nettest`, a musl `std` binary (built by `ncaprobe/build-musl.sh`) rather than a `libakuma` one — the point is to exercise the real `pipe2`+`posix_spawn`+`epoll`+`pidfd` path, and to run the **same binary** under Docker on Linux as an A/B control |
| `fpcprobe`, `shareprobe` | Premature-free race probes (`docs/archive/MAPPED_PAGE_PREMATURE_FREE_FIX.md`): file-page-cache invalidate/evict-vs-mmap races with pattern verification (`fpcprobe`, pass `norename` to exclude the fd path-identity bug), and fork/CoW share-without-inc races over 2-page anon regions (`shareprobe`). Verdict lines `ALL PASS` / `CORRUPTION events=N`; a `0xFEEDFACE…` qword in a report is quarantine poison naming its frame |
| `ext2probe` | ext2 file-op throughput benchmark (`docs/archive/EXT2_PERFORMANCE_AUDIT.md`): times create/seq-write/seq-read/list-dir before and after a large synthetic create-then-mass-delete tree (`rm -rf`-shaped), to test whether ordinary fs ops regress afterward. `ext2probe [stress_files_per_dir] [stress_dirs]` (default 200×16 = 3200 files). Verdict line `REGRESSION` / `NO REGRESSION` (>=20% slower on any op) |

Removed members (documented in `docs/archive/`, kept for historical
reference): `quickjs`, `needle-server`, `crush`, `stdcheck`, `top`, `stp_test`,
`sqld`, `doom`, `xbps` — see `docs/archive/TRIM_FAT_PART_2.md` and
`docs/archive/TRIM_FAT_PART_3.md`.
