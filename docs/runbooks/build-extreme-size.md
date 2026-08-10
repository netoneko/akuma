# Build the `extreme-size` image (and keep it small)

**Stability: B — verify behaviour.** The 4 MB RAM floor target. Full profile
background in [`docs/reference/build-profiles.md`](../reference/build-profiles.md);
this runbook is the do-it procedure plus the memory traps that bite here and
nowhere else.

## Build

```bash
scripts/build_extreme_size.sh
SSHD_FORK_SESSIONS=0 userspace/build.sh --sshd-only   # see "sshd" below
scripts/populate_disk.sh --bin-only
```

The kernel build is:

```bash
cargo +nightly build --profile extreme-size --no-default-features \
    --features no-tests,smoltcp,extreme,userspace-sshd -Z build-std=core,alloc
```

**Verify:** `ls -lh target/aarch64-unknown-none/extreme-size/akuma` — expect
~575 KB. `scripts/cargo_runner.sh` enforces the ceiling and fails the run with
`kernel binary is N KB, exceeds ... limit` if it regresses.

## sshd: build it without `fork-sessions` here

`/bin/sshd` defaults to **one forked process per SSH session**
([`PROCESS_PER_SESSION.md`](../../userspace/sshd/docs/PROCESS_PER_SESSION.md)).
That is the right trade on a devbox and the wrong one at a 4 MB floor: each live
session is a whole process — its own address space, its own stack — against a
global `MAX_PROCESSES = 64`, and sshd's default `max_sessions = 24` allows two
dozen of them.

There is exactly one binary for every image, so this is not handled by the
kernel profile. Build it explicitly:

```bash
SSHD_FORK_SESSIONS=0 userspace/build.sh --sshd-only
```

**Verify** you got the cooperative build — the two differ by their log strings:

```bash
strings bootstrap/bin/sshd | grep -c 'session pid'   # 0 = cooperative, 1 = forking
```

Do this whenever a low-RAM image shows memory pressure, OOM kills, or `fork
failed` in the sshd log, not only for `extreme-size`. The cost of reverting is
the cooperative executor's known limits (`LIMITATIONS.md` §1-§2): one OS thread,
no fault isolation between sessions, and one blocking syscall stalls every
session.

The kernel half (`many-sessions`: 32-deep listener backlog, larger socket table,
~1 MB of heap per listening socket) is **already off** on this profile — it is
not in the `--no-default-features` list, and `kernel_profile_extreme` overrides
the constants back to 8/32 regardless. Nothing to do there; the consequence is
that this image RSTs past 8 simultaneous connection *arrivals*, which is the
behaviour it has always had.

## Other memory traps on this profile

- **`small-sockets` / `kernel_profile_extreme`** cap the smoltcp socket table at
  32 total, shared by every process. A leak or a pile of `TIME_WAIT` sockets
  shows up as `bind`/`accept` failures long before anything reports OOM.
- **No `kernel-tls`.** No in-kernel `curl https://`; use a userspace tool.
- **The built-in SSH server is compiled in on this profile only**, and it binds
  port 22 alongside the userspace `/bin/sshd` — see `build-profiles.md`
  § "Known break on `extreme-size`".

## Verify the whole thing boots

```bash
scripts/create_disk.sh && scripts/populate_disk.sh
MEMORY=4 cargo run --profile extreme-size    # the floor it exists to prove
```

Expect `[Main] sshd started` (this profile has the kernel spawn sshd directly;
every other profile lets herd do it and prints `[herd] Started sshd`). Then:

```bash
ssh -o StrictHostKeyChecking=no root@localhost -p 2222
```

## Background

- [`docs/reference/build-profiles.md`](../reference/build-profiles.md) — the
  profile/feature matrix and where `many-sessions` sits in it.
- [`userspace/sshd/docs/PROCESS_PER_SESSION.md`](../../userspace/sshd/docs/PROCESS_PER_SESSION.md)
  — what `fork-sessions` does, and its measured behaviour.
- `acceptance/05` — the extreme-size acceptance playbook.
