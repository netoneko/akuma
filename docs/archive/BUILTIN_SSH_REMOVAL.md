# Removing the built-in SSH server from every profile but `extreme`

Follow-up to `docs/archive/TRIM_FAT_SSHD.md`, which argued the in-kernel SSH
server should be retired but never measured what retiring it buys. This doc is
the measurement, and the change that landed on `trim-fat-sshd`.

## What was actually there before

`userspace-sshd` was a **runtime** switch, not a compile-time one:

```rust
pub const ENABLE_USERSPACE_SSHD: bool = cfg!(feature = "userspace-sshd");
```

It skipped the `spawn_system_thread_fn(|| ssh::server::run())` call and nothing
else. `crates/akuma-ssh`, all of `src/ssh/`, and the in-kernel shell hanging off
it stayed linked into the image. `devbox-smoltcp` — the *default* devbox, which
sets `userspace-sshd` — had been shipping a complete second SSH-2 server it
never started.

## The measurement

`extreme-size` profile, `aarch64-unknown-none`, built with and without the
built-in server compiled in. Same commit, same feature set otherwise.

| | built-in SSH | compiled out | delta |
|---|---|---|---|
| `.text` | 591,464 | 443,320 | **−148,144** (−25%) |
| `.rodata` | 108,303 | 37,055 | **−71,248** (−66%) |
| `.data` | 32,976 | 32,928 | −48 |
| `.bss` | 150,768 | 150,464 | −304 |
| file on disk | 803,912 | 586,776 | **−217,136 (−27%)** |
| loaded image (boot banner) | 871 KB | 658 KB | −213 KB |

Default `--release` (unstripped, no LTO, so the same source costs far more):
**4,506,736 → 3,251,312 bytes, −1,255,424 (−27.9%)**.

That is 6× the ≤34 KB `docs/reference/build-profiles.md` predicted from symbol
attribution. Symbol attribution undercounts because it charges SSH only for
`akuma::ssh` + `akuma-ssh*`; it misses everything that exists *solely to serve*
the SSH session — the whole in-kernel shell command set, `async_fs`, the editor.

### Free RAM at the 4 MB floor

`pmm_free` from the `[FSCACHE]` line at the 30 s heartbeat. 1024 pages = 4 MB,
1152 pages = 4.5 MB.

At `MEMORY=4M`, disk + herd (core2herd, httpd, userspace sshd):

| moment | built-in SSH | compiled out | delta |
|---|---|---|---|
| PMM free right after init | 647 pg (2588 KB) | 700 pg (2800 KB) | +53 pg = +212 KB |
| PMM free at steady state | 114 pg (456 KB) | 191 pg (764 KB) | +77 pg = **+308 KB** |

The two numbers differ by exactly 24 pages = 96 KB — one system-thread stack
(`System threads: 7 × 96 KB` in the boot banner). The SSH server's thread is
simply never spawned; `tid=2` disappears from `[THR-DUMP]`. So the steady-state
saving decomposes cleanly as **213 KB image + 96 KB thread stack**, and the
image shrink converts to free RAM 1:1 because the kernel image occupies PMM
pages (`Code+Stack` loses exactly the 53 pages `User pages` gains).

At `MEMORY=4608K` (the `acceptance/05_meow_tcc_extreme_4mb.md` profile),
`SNAPSHOT=1`, idle:

| config | free |
|---|---|
| built-in SSH + herd (core2herd, httpd, userspace sshd) | 242 pg = 968 KB |
| no built-in SSH, herd still running | 319 pg = 1276 KB |
| **no built-in SSH, no herd, kernel-spawned `/bin/sshd`** | 678 pg = **2712 KB** |

Only 308 KB of that 1744 KB spread is the SSH removal. The rest is herd plus
the two services it starts — but herd could not be dropped *before* this change,
because herd was the only thing able to launch `/bin/sshd`. That is what
`config::AUTO_START_SSHD` now does directly, and it was verified reachable:
`ssh -p 2222` returned `AKUMA_OK` with no supervisor and no built-in server.

For reference, `acceptance/05` records ~2520 KB post-boot idle for the current
extreme profile, so the herd-less userspace-sshd configuration clears more
headroom than the 4.5 MB agentic demo has ever had.

## The change

`cfg(kernel_builtin_ssh)`, emitted by `build.rs` when
`smoltcp && extreme && !userspace-sshd`. Extreme keeps the built-in server —
a 4 MB box can then be reachable with nothing on disk but a kernel. Every other
profile serves SSH from the userspace `/bin/sshd`.

Gating the server alone does not compile: it orphans **118 items** on extreme
and **122** on `devbox-smoltcp`, because the built-in SSH session is the *only*
consumer of the entire in-kernel shell. (On a default build with the boot suite
on, only ~8 go dead — the tests are what keep the shell alive there, nothing
else.) So these went with it:

| gated wholesale | why |
|---|---|
| `src/ssh/`, `src/ssh_tests.rs` | the server |
| `src/shell/` including all of `commands/` | only drivers were `ssh/protocol.rs` and `shell_tests.rs` |
| `src/async_fs.rs` | only caller was the in-kernel shell |
| `src/editor/` (neko) | only reachable from the SSH shell |
| `src/shell_tests.rs` | builds the same command registry |

Leaf helpers gated individually: `fs::{read_to_string,append_file,stats,FsStats}`,
`vfs::{read_to_string,append_file,stats,list_mounts}`,
`kernel_timer::{from_millis,TimeoutError,with_timeout}`, `akuma::AKUMA_79`, and
the `ps`/`kthreads` checks in `tests.rs` (they run commands *through* the
in-kernel shell pipeline, so they cannot exist without it — they now start
`ps_done`/`kthreads_done` at `!cfg!(kernel_builtin_ssh)` so the wait loop does
not spin for a result that will never arrive).

Two deliberate exceptions:

- **`src/pmm.rs` got `#[allow(dead_code)]`, not a cfg.** `leak_count` and the
  `FrameTrackingStats` fields are frame-leak diagnostics paired with
  `DEBUG_FRAME_TRACKING`. Tying "can I diagnose a frame leak" to "is SSH
  compiled in" would be a lie about why the code exists.
- **New `cfg(kernel_tests)`** in `build.rs` (`!no-tests && OPT_LEVEL != "z"`),
  mirroring the `not(any(feature = "no-tests", kernel_profile_size))` condition
  `main.rs` already repeats a dozen times. `fs::append_file` and
  `vfs::append_file` are used by `fs_tests.rs` as well as by the shell, so they
  are `#[cfg(any(kernel_builtin_ssh, kernel_tests))]`.

The `compile_error!` guard in `main.rs` ("no in-kernel SSH without `smoltcp`;
enable `userspace-sshd`") was dropped — every non-extreme profile is now
userspace-sshd by construction, so the invariant it encoded is vacuous.

`config::AUTO_START_HERD` became
`!(cfg!(kernel_profile_extreme) && cfg!(feature = "userspace-sshd"))`: herd
stays on everywhere it was, and turns off only in the extreme+userspace-sshd
combination, where `AUTO_START_SSHD` starts `/bin/sshd --port 22 --shell /bin/sh`
from `kernel_main` instead.

## Verification

- All six configurations compile clean: `default`, `extreme`,
  `extreme+userspace-sshd`, `size`, `devbox-smoltcp`, `smp-shared`.
- Clippy clean on every crate plus the `release` and `size` profiles (what the
  pre-commit hook runs); 525 host tests pass, 0 failures.
- Live: extreme+userspace-sshd at 4.5 MB, kernel-spawned sshd, `ssh -p 2222`
  round-tripped a command with no herd and no built-in server.
- Live: default `--release` at 512 MB boots and is reachable — **but on port
  2323, not 2222** (see below).

## Resolved: the default image is back on port 2222

`bootstrap/etc/herd/enabled/sshd.conf` had `args = --port 23`, chosen when the
built-in server owned port 22 and the userspace one had to avoid it. With the
built-in server gone from every non-extreme profile, port 22 was free but
nothing bound it, so `ssh -p 2222` — what `CLAUDE.md`, the runbooks and 11 of
the 12 acceptance playbooks tell you to use — stopped answering.

Changed to `--port 22` and verified live on the default box (`cargo run
--release`, 256 MB, `disk.img`): `ssh -p 2222` returns `PORT_2222_OK`, and 2323
now correctly refuses. Existing disks need `scripts/populate_disk.sh --overlay`
with a tree containing `etc/herd/enabled/sshd.conf` (surgical — no `/etc` wipe,
so the host key survives).

## Open: extreme double-binds port 22

The flip above breaks `extreme-size`, which is the one profile that still runs
the built-in server *and* lets herd start `/bin/sshd`. Both now target port 22,
and **the kernel does not reject the second bind**:

```
[SSH Server] Starting SSH server on port 22...
[SSH Server] Listening...
[herd] Started sshd (pid= 4) on BSP fallback
[syscall] bind(fd=3, port=22, ip=0.0.0.0)        ← succeeds, no EADDRINUSE
```

Observed result on a 4.5 MB extreme boot: `ssh -p 2222` **hangs** (90 s, no
banner). The serial log shows the built-in server taking the connection
(`[SSH] New SSH connection` → `Client version received` → `Connection ended`)
while the userspace sshd's child thrashes the page pool:

```
[IA-DP] pid=5 va=0x100b5a00 readahead pool exhausted, 16 free pages — retrying single page
```

So it is not a clean "second listener loses" — it is two owners of one port on a
box with no pages to spare.

Two things to fix here, and they are independent:

1. **`bind()` should return `EADDRINUSE` when an in-kernel listener holds the
   port.** Silently accepting a conflicting bind is wrong on any profile; it
   just has nowhere to show up until two servers want the same port. This is the
   real defect.
2. **Extreme should not run two sshds.** herd's `/bin/sshd` is pure redundancy
   there — the built-in server already serves 22 — and it costs RAM on the
   profile least able to pay. The measured-better configuration is
   `extreme + userspace-sshd` (2712 KB idle free vs 968 KB), which turns herd off
   and lets `AUTO_START_SSHD` own port 22 alone. Adopting that for extreme would
   make the built-in server dead code on every profile and close this out;
   keeping the built-in server means finding another way to stop herd starting
   its sshd on a disk shared by all profiles.

## Boot-readiness marker: harness divergence

`[SSH Server] Listening` was printed by the **in-kernel** server's accept loop.
Nothing prints it any more. Every harness that polled for it would wait forever,
and `CLAUDE.md` still documents it as *the* boot-wait recipe.

Worse than a straight rename: there are now **two** startup paths with two
different markers, depending on who owns sshd.

| image | who starts sshd | marker |
|---|---|---|
| `extreme-size` (`userspace-sshd`, herd off) | kernel, via `config::AUTO_START_SSHD` | `[Main] sshd started (tid=N)` |
| everything else | herd, from `/etc/herd/enabled/sshd.conf` | `[herd] Started sshd (pid= N)` |

So a profile-neutral wait has to accept either:

```bash
until grep -aqE "sshd started|Started sshd" boot.log 2>/dev/null; do sleep 2; done
```

`-a` is required — QEMU/HVF emits a control byte that makes plain `grep` treat
the log as binary and silently match nothing.

Updated to the pattern above: `acceptance/05_meow_tcc_extreme_4mb.md`,
`acceptance/08_meow_clone_compile_run.md`, `docs/runbooks/boot-and-connect.md`,
`docs/runbooks/debug-boot-hang.md`, `scripts/test_memory_split.py`.

**`CLAUDE.md` is not updated** — it is reference-only by project convention, so
its wait recipe still names the dead marker. Anyone following it verbatim gets a
hang, not an error. Fixing that is a call for whoever owns that file.

Two other consequences of the same removal, for anyone grepping logs:

- `[Main] Built-in SSH server not compiled; userspace /bin/sshd only` was an
  interim message from the gated build; the final tree prints
  `[Main] SSH is the userspace /bin/sshd`.
- The `[SSH]` stats/stall-watchdog block in the memory monitor is gone with the
  server it reported on, so `[SSH] ... stall_us=` never appears. The stall
  watchdog described in `docs/runbooks/debug-ssh-latency.md` no longer has a
  kernel-side counterpart.

## Found

- **The file-page dedup cache is undersized at the 4 MB floor. OPEN** — full
  write-up, evidence and fix options in
  [`FPCACHE_UNDERSIZED_AT_LOW_RAM.md`](FPCACHE_UNDERSIZED_AT_LOW_RAM.md). In short:
  `src/file_page_cache.rs:105` sizes the cache `(total_ram_bytes / 8) / 4096` —
  144 pages (576 KB) at 4.5 MB. busybox is 1,116,408 bytes = **273 pages**, so
  the dedup table cannot hold half of one busybox, and every process on that box
  *is* busybox (`/bin/sh`, plus each applet in a pipeline). Measured
  `[FPCACHE] entries=144 hits=77048 misses=13175 evict=13031` — 99% of misses
  evict, the signature of a cache below its working set. Eviction drops the
  cache's reference but the frame survives while mapped, so what is lost is the
  *dedup entry*: the next faulter re-reads from ext2 into a **private** frame,
  and concurrent busybox instances stop sharing text. The box then OOMs on
  `fork` (`/bin/sh: can't fork: Out of memory` after a handful of SSH sessions).
  The module's own docstring argues the cap "can be generous" because a mapped
  entry costs nothing beyond the map node — but `RAM/8` makes it least generous
  exactly where dedup matters most. The cap wants sizing against the binaries
  being mapped, not as a fraction of RAM, with pressure-driven eviction for
  zero-mapper entries. At 64 MB (cap 2048 pages) the same workload is fine.
- **`[TESTS] low-mem …` lied on `no-tests` builds. FIXED.** `src/main.rs` printed
  `skipping boot self-test suite` whenever RAM ≤ `LOW_MEM_TEST_SKIP_MB` with no
  `cfg` guard, so `extreme` logged that it skipped a suite it never compiled
  (`mod tests` / `mod process_tests` are gated
  `not(any(feature = "no-tests", kernel_profile_size))`, and extreme sets both).
  The decision and its message now live inside a block under that same cfg.
  Verified: 0 occurrences in a fresh 4.5 MB extreme boot that still reaches SSH.

## Background

- `docs/archive/TRIM_FAT_SSHD.md` — the userspace `sshd` size work and the
  original "candidate for removal" argument, including the unauthenticated
  pre-auth panic that wedges the whole VM in the in-kernel copy
  (`crates/akuma-ssh/src/packet.rs:83`), still unfixed.
- `userspace/sshd/docs/PROTOCOL_UNDER_LOAD.md` — the same bug class in the
  userspace server, fixed.
- `docs/reference/build-profiles.md` — per-profile SSH ownership.
- `acceptance/05_meow_tcc_extreme_4mb.md` — the 4.5 MB profile these numbers
  were measured against.
