# Userspace Layout

**Grade: B** (verify behaviour) — accurate as of 2026-08-08; drifts whenever a
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
| `box` | Container manager (`box open/close/stop/ps/inspect`) |
| `meow` | AI coding assistant — LLM chat client with filesystem/network tool calling |
| `nca` | Build wrapper for [native-cli-ai](https://github.com/netoneko/native-cli-ai) (submodule); no binary in `/bin` — build.rs deploys to `bootstrap/bin/nca` |
| `tcc` | Tiny C Compiler port — compile and run C programs on-target |
| `neatvi` | Vi-like text editor, compilable on-target with TCC |
| `sshd` | Userspace SSH server |
| `httpd` | HTTP server |
| `tar` | `tar` utility implementation |
| `scratch` | Minimal Git client (Git Smart HTTP) — was removed then reverted; see `docs/archive/TRIMMING_FAT_PART_2.md`'s "Removed: scratch" section, which predates the revert |
| `llama.cpp` | llama.cpp port — LLM inference on-device |
| `rumpkernel` | NetBSD rump kernel port — real network stack (DHCP/HTTP/HTTPS) |
| `wavplay` | Streams a WAV file to `/dev/dsp` (VirtIO-sound); fixed-footprint, file-backed |
| `forktest` | Go fork/clone stress-testing harness; `forktest/c_stress/` holds pure-C musl-static control binaries (mmap/fault/futex/thread-spawn probes) used to disambiguate kernel bugs from Go-runtime bugs |
| `hiss` | New/WIP crate, not yet a `userspace/Cargo.toml` workspace member — audio-related, reuses `wavplay`'s playback logic |
| `echo2`, `elftest`, `hello`, `stackstress`, `termtest`, `allocstress` | Small repro/stress/example programs (echo loop, ELF-load + subprocess-spawn check, long-running argv/streaming test, exception-stack-overflow stress, terminal-attribute test, allocator-stress test) |

Removed members (documented in `docs/archive/`, kept for historical
reference): `quickjs`, `needle-server`, `crush`, `stdcheck`, `top`, `stp_test`,
`sqld`, `doom`, `xbps` — see `docs/archive/TRIMMING_FAT_PART_2.md` and
`docs/archive/TRIMMING_FAT_PART_3.md`.
