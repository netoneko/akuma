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
| `sshd` | Userspace SSH server |
| `paws` | **EXPERIMENTAL — `extreme-size` demo only.** Minimal first-party shell (598 lines, pure `libakuma`): 8 RO-mapped pages vs busybox's 265, which is what makes it usable as the login shell on a 4 MB box whose file-page dedup cache holds 128. **Not busybox/ash compatible** — hand-rolled parser, fixed builtin list, no `exec`/`printf`/`test`, unreliable pipes and redirection. Fine for `sshd --shell /bin/paws` execing one binary (that is how `acceptance/05`, and formerly `acceptance/archive/08`, pass at 4.0 MB); do not point real shell scripts at it. Originally removed at `c0af6c7`, revived 2026-08-10. See [`../archive/BUSYBOX_TOYBOX_SIZING.md`](../archive/BUSYBOX_TOYBOX_SIZING.md) |
| `httpd` | HTTP server |
| `tar` | Tar extraction. A **library** (`akuma_tar`) that `box` links for layer extraction, plus a thin `/bin/tar` CLI. Header parsing (`format.rs`) is host-testable |
| `scratch` | Minimal Git client (Git Smart HTTP) — was removed then reverted; see `docs/archive/TRIM_FAT_PART_2.md`'s "Removed: scratch" section, which predates the revert |
| `llama.cpp` | llama.cpp port — LLM inference on-device |
| `rumpkernel` | NetBSD rump kernel port — real network stack (DHCP/HTTP/HTTPS) |
| `wavplay` | Streams a WAV file to `/dev/dsp` (VirtIO-sound); fixed-footprint, file-backed |
| `forktest` | Go fork/clone stress-testing harness; `forktest/c_stress/` holds pure-C musl-static control binaries (mmap/fault/futex/thread-spawn probes) used to disambiguate kernel bugs from Go-runtime bugs |
| `hiss` | New/WIP crate, not yet a `userspace/Cargo.toml` workspace member — audio-related, reuses `wavplay`'s playback logic |
| `echo2`, `elftest`, `hello`, `stackstress`, `termtest`, `allocstress` | Small repro/stress/example programs (echo loop, ELF-load + subprocess-spawn check, long-running argv/streaming test, exception-stack-overflow stress, terminal-attribute test, allocator-stress test) |
| `fpcprobe`, `shareprobe` | Premature-free race probes (`docs/archive/MAPPED_PAGE_PREMATURE_FREE_FIX.md`): file-page-cache invalidate/evict-vs-mmap races with pattern verification (`fpcprobe`, pass `norename` to exclude the fd path-identity bug), and fork/CoW share-without-inc races over 2-page anon regions (`shareprobe`). Verdict lines `ALL PASS` / `CORRUPTION events=N`; a `0xFEEDFACE…` qword in a report is quarantine poison naming its frame |

Removed members (documented in `docs/archive/`, kept for historical
reference): `quickjs`, `needle-server`, `crush`, `stdcheck`, `top`, `stp_test`,
`sqld`, `doom`, `xbps` — see `docs/archive/TRIM_FAT_PART_2.md` and
`docs/archive/TRIM_FAT_PART_3.md`.
