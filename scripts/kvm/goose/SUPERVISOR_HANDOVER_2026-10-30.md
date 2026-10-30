# Handover prompt - Taoxichuan supervisor, 2026-10-30 (paste this as the first message of the new session)

You are continuing the Taoxichuan experiment on branch `rust-org-il-experiment` in /Users/netoneko/github.com/netoneko/akuma (HEAD `8fb1a673`, pushed to origin and litter). On this branch you may commit, push and work autonomously; log decisions (who decided: user, supervisor, glm) in `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` - the user wants it for the story. Read, in this order: the last four entries of that log (from "State at the end of the 2026-10-29/30 stretch"), `scripts/kvm/goose/SUPERVISOR_RUNBOOK.md`, `taoxichuan/docs/reference/rust-syscalls.md`. Repo rules: CLAUDE.md (no agents/forks without asking; ssh only through Python, e.g. `python3 -I scripts/kvm/goose/ak.py '<cmd>'`).

## First, check you can reach the ryzen (the previous session could not)
`python3 -I scripts/kvm/goose/ak.py 'uname -a'` - the previous session got "No route to host" for every process it launched while the user's own terminal worked: macOS Local Network privacy for the app that holds the session. If it fails, say so and carry on with TCG. The ryzen is currently booted into Pop OS (Linux, KVM available), not Akuma; use it for a fast KVM run of the same kernel/images and for strace-ing the same binaries on real Linux when it helps.

## Where things stand (all on local QEMU TCG)
- `rustc hello.rs -o hello && ./hello` works in the guest (gcc driver + collect2 + GNU ld inside the guest).
- `cargo build` of a no-dependency hello project works once per boot (19.7 s of TCG time), prints "hello from cargo on taoxichuan".
- git clone over git:// works. HTTPS clone (`https://github.com/netoneko/akuma-playground.git`) is untested: needs TLS/CA certs in the guest.
- A second cargo invocation in the same boot (`cargo run`, `cargo build --release`) fails with exhaustion: `[forkp] refused: Failed to allocate x86 task stacks / Failed to create address space / user-half clone failed`, then "memory allocation failed". Suspected cause, not yet measured: nothing in taoxichuan frees an exited task's user address space, kernel stacks or ring slot (rustc maps the 312 MB librustc_driver.so by copying). `exec_real_task` itself leaked the image until the last commit.

## Your next steps, in order
1. Measure `akuma_pmm::free_count()` before/after one `rustc hello.rs` run (print it at task exit and at exec, temporarily), find who should free the address space (probably when `wait4` reaps, via `akuma_exec`'s Process table), free it, and make `cargo build; cargo run; cargo build --release` pass in ONE boot. Also check the ring slots / `x86_claim_slot` recycling.
2. Then: `cargo build` with a vendored dependency (user answer 7: vendored crates are fine for now), `-j4`, then HTTPS git clone / crates.io (TLS in the guest).
3. Then the planned work: init as a Process (pm S1: herd as pid 1 built with libakuma `linux-abi`), FPU/SSE save slice, MAP_SHARED writeback then signals, remaining `[nosys]`: statx/newfstatat 326, sendfile 40, linkat 265, tkill 200, getrusage 98, prctl 157, mount 165/umount2 166. Executables over 16 MiB are read whole into the heap (`read_image`); a lazy-mapped loader is the real fix.
4. Two background gaps worth a look when convenient: the `apk add git` exec-crossing stall that glm measured on the metal (7 of 8 boots; not seen on TCG), and `cc1` (38 MB) now execs through `read_image` but is untested.

## Local rig (all absolute paths; the old session's scratchpad still exists on disk)
SP=/private/tmp/claude-502/-Users-netoneko-github-com-netoneko-akuma/92196a57-f57d-4d69-8b74-b0b562261a61/scratchpad
- Build the trace kernel: `cd taoxichuan && cargo build --release --target x86_64-unknown-none --features no-tests,trace-err --target-dir $SP/tx-target-s1e` (production: `--features no-tests --target-dir $SP/tx-target-s1`). Always pass `--target x86_64-unknown-none`.
- Boot rs.img (Rust toolchain image, has gcc/ld/cargo; copied to img/run.img on each boot so changes inside the guest are lost): `cd $SP; QEMU_EXTRA="-netdev user,id=n0,hostfwd=tcp:127.0.0.1:2224-:22 -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1" ./runqemu_s1e.sh img/rs.img "init=/bin/sshd initargs=--port,22" 3000 40`, wait ~25 s, then `python3 -I sshq.py '<cmd>' <timeout_s>` runs a command in the guest (needs `export HOME=/root LD_LIBRARY_PATH=/usr/local/rust/lib:/usr/lib:/lib PATH=/usr/local/rust/bin:/usr/bin:/bin`). Console is `$SP/tx-console.log` (`grep -a`). `trace-err` prints `[err] sN nr=X -E` for failing syscalls and `[nosys]` for unserved numbers.
- Regression battery: `sh battery.sh runqemu_s1.sh` (F1, PK1, fs, apk add git, git --version) must stay green. Images: img/pl.img (ssh+apk+git), img/rs.img, debugfs at /opt/homebrew/opt/e2fsprogs/sbin. Host git daemon on 9418 serves $SP/gsrv (repo r.git); Alpine mirror on 8130 if needed.
- Probes: the in-tree probes under `userspace/` and `userspace/forktest/c_stress/*.c` build for amd64 with `x86_64-linux-musl-gcc -static` (installed); do not stop at "that is aarch64" (the user's lesson, in the log).
- Quota: `python3 scripts/kvm/goose/zai_usage.py`.
