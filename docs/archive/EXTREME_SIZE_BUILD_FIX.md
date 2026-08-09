# extreme-size: build fix + curl matrix

**Date:** 2026-08-09/10
**Branch:** `fix-extreme-size`
**Baseline:** `586c0dd` ("fix alloc prints")
**Status:** build fixed; `curl` over HTTP verified on `extreme-size`.

Supersedes the "Known breakage: `extreme-size` at `d3f28d6`" section of
`docs/reference/build-profiles.md`.

---

## 1. What was broken

`scripts/build_extreme_size.sh` failed with **17** errors: 15 × `E0433 could not
find file_page_cache in the crate root` plus 2 × `E0433 could not find container
in super`. Two independent causes, same species — a `mod` declaration sitting
under a `#[cfg]` that belonged to something else.

### 1a. The stolen `cfg` (15 errors)

`src/main.rs` read:

```rust
mod exceptions;
#[cfg(feature = "sc-framebuffer")]
mod file_page_cache;          // <-- wrong module under this gate
mod fw_cfg;
```

History shows how it got there:

- `27fdf90` ("down to almost 6mb") added `#[cfg(feature = "sc-framebuffer")]`
  above `mod fw_cfg;` — deliberate, since `fw_cfg` exists only to configure
  `ramfb`, which is gated the same way.
- `6f01fe7` ("more fixes") later inserted `mod file_page_cache;` *between* the
  attribute and `mod fw_cfg;`, in alphabetical position.

One inserted line did two things:

1. **Gated out the page cache.** `extreme-size` builds
   `--no-default-features --features no-tests,smoltcp,extreme`, so
   `sc-framebuffer` is absent, the declaration vanished, and the ~15
   unconditional call sites in `fs.rs`, `pmm.rs`, `vfs/mod.rs`, `main.rs` and
   `exceptions.rs` lost their module.
2. **Silently un-gated `fw_cfg`.** `release` and `size` kept compiling, so
   nothing complained — they have been carrying `fw_cfg` unconditionally since
   June, quietly undoing part of `27fdf90`'s size work.

Only `extreme-size` (and any future non-`sc-framebuffer` profile) surfaced it.

**Fix** — restore the gate to its intended module and leave the page cache
unconditional:

```rust
mod file_page_cache;
// fw_cfg exists only to configure ramfb, so it follows the framebuffer gate.
#[cfg(feature = "sc-framebuffer")]
mod fw_cfg;
```

`fw_cfg`'s only consumers are `src/ramfb.rs` (itself `sc-framebuffer`-gated) and
the framebuffer init in `main.rs`, so re-gating it is safe. This is the fix
`build-profiles.md` recommended ("moving the `mod` declaration out from behind
`sc-framebuffer`").

### 1b. `caller_box_and_pid` behind the container gate (2 errors)

`src/syscall/proc.rs` called `super::container::caller_box_and_pid()` from
`sys_spawn_ext` and `sys_set_box_stack`, but `mod container` is
`#[cfg(feature = "sc-containers")]`. Both syscalls are dispatched
unconditionally (`syscall/mod.rs`, `SPAWN_EXT` / `SET_BOX_STACK`), and both need
the caller's identity for their box-access check — so on `extreme-size` the call
sites survived while the module did not.

The helper has no container-specific dependency:

```rust
akuma_exec::process::current_process_shared().map_or((0, 0), |p| (p.box_id, p.pid))
```

**Fix** — moved it verbatim to `src/syscall/mod.rs` as `pub(crate)`, ungated.
`container.rs` does `use super::*`, so its two call sites resolve unchanged;
`proc.rs` now calls `super::caller_box_and_pid()`.

> Keeping the check working when `sc-containers` is off is the point. Stubbing
> it to `(0, 0)` would make every caller look like host/box 0 and hand any
> process a free pass through `can_access_box`.

### Files changed

| File | Change |
| --- | --- |
| `src/main.rs` | move `sc-framebuffer` gate off `file_page_cache` back onto `fw_cfg` |
| `src/syscall/mod.rs` | `caller_box_and_pid()` added, ungated |
| `src/syscall/container.rs` | helper removed (moved out) |
| `src/syscall/proc.rs` | 2 call sites → `super::caller_box_and_pid()` |

Verified: `scripts/build_extreme_size.sh` compiles warning-free, and
`cargo check --release` is unaffected by the `fw_cfg` re-gate.

---

## 2. Size cost of the shared file-page cache

The fix compiles `file_page_cache` into `extreme-size` for the first time. Cost
measured by flipping `config::SHARED_FILE_PAGES_ENABLED`, which every hot entry
point already early-returns on, so `false` lets LLVM DCE the subsystem (this is
the documented clean kill switch — no stub module needed).

| | text | data | bss | ELF | flat `.bin` |
| --- | ---: | ---: | ---: | ---: | ---: |
| page cache **ON** (shipped) | 700,191 | 33,000 | 150,768 | 785 KB | 737,488 |
| page cache **OFF** | 690,303 | 33,000 | 150,688 | 777 KB | 729,088 |
| **delta** | **+9,888 (+1.4 %)** | 0 | +80 | +8 KB | +8,400 |

~9.7 KB of text against the 4 MB RAM floor. It is not dead weight on extreme —
a 64 MB boot that runs one `curl` already reports:

```
[FPCACHE] entries=390 hits=12 misses=390 evict=0 inval=0
```

With it off, the `[fpcache] … enabled` init line and the `[FPCACHE]` heartbeat
row disappear entirely — a clean disable.

---

## 3. curl matrix — which curl, which mode, where

**Target:** `http://10.0.2.2:8477/hello.txt` (host `python3 -m http.server`
reached through QEMU SLIRP; body `EXTREME-SIZE-CURL-OK`). HTTP only — extreme
drops `kernel-tls`, so there is no `https://` in the built-in client.

**Harness:** `curl_matrix.py` (scratchpad). Every case gets its own
`SNAPSHOT=1` boot, so a fault cannot contaminate the next case and disk writes
are discarded. A case counts **OK** only if the body came back *and* no
`[Exception] Sync from EL1` appeared in that case's slice of the log.

Two distinct curls exist:

- **`/bin/curl`** — the real userspace binary (`bootstrap/bin/curl`). This is
  what you get by default: `config::SSH_BUILT_INS_FIRST` is `false`, so
  `find_executable()` searches `/usr/bin` then `/bin` *before* the registry.
  On this disk only `/bin/curl` exists.
- **built-in `curl`** — the in-kernel `CURL_CMD` (`src/shell/commands/net.rs`),
  registered under the `smoltcp` feature. Normally unreachable, because
  `/bin/curl` shadows it. Selected here by building with
  `SSH_BUILT_INS_FIRST = true`, and cross-checked by `rm`-ing `/bin/curl` on a
  snapshot boot so the external lookup misses and the chain falls back.

### Results

| Kernel | curl | mode | Result |
| --- | --- | --- | --- |
| extreme, cache ON | `/bin/curl` | plain | **OK** — 16/18 boots (2 EL1 faults) |
| extreme, cache ON | `/bin/curl` | `-v` | **OK** |
| extreme, cache ON | built-in | plain | **OK** |
| extreme, cache ON | built-in | `-v` | **OK** |
| extreme, cache OFF | `/bin/curl` | plain | **OK** — 11/15 boots (4 EL1 faults) |
| extreme, cache OFF | `/bin/curl` | `-v` | **OK** |
| extreme, cache OFF | built-in | plain/`-v` | OK (via `rm` fallback) |
| release (256 MB) | `/bin/curl` | plain | **OK** |
| release (256 MB) | `/bin/curl` | `-v` | **OK** |
| release (256 MB) | built-in | plain | **OK** |
| release (256 MB) | built-in | `-v` | **OK** |

Everything works. The two curls are distinguishable by their verbose output:
real curl prints `*   Trying 10.0.2.2:8477...`, the built-in prints
`* Connecting to 10.0.2.2:8477 (http)` / `< HTTP/1.0 200`.

RAM made no difference on extreme: `/bin/curl` plain was 4/4 at 64 MB and 4/4 at
256 MB in back-to-back batches.

### The page cache does **not** change curl reliability

An early batch read 11/12 (ON) vs 2/4 (OFF) and looked like a real effect. It
was not — the failures arrive in **bursts** correlated with host load, so
sequential batches are not comparable. Re-run as 6 **interleaved ON/OFF pairs**:

| | result |
| --- | --- |
| ON | 5/6 |
| OFF | 5/6 |

Pooled: ON 16/18, OFF 11/15 — Fisher exact **p ≈ 0.36**, not significant.

> **Methodology note for anyone repeating this.** Do not A/B two kernels in
> sequential batches on this workload. Interleave them, or you will "discover"
> whichever variant happened to run while the host was quiet.

---

## 4. Open: `/bin/curl` faults intermittently on extreme

~11 % of boots (2/18 with the cache on, pooled 6/33 across both variants), a
`curl` that has just been spawned takes an EL1 fault and the SSH session hangs.
Not specific to the page cache, not specific to `-v`, and not RAM-size driven.
Three signatures observed:

| ESR | meaning | note |
| --- | --- | --- |
| `EC=0x25 ISS=0x47` | EL1 data abort, **write**, translation fault L3 | dominant |
| `EC=0x25 ISS=0x4f` | EL1 data abort, **write**, permission fault L3 | write to RO kernel page (`FAR=0x4011d220`, in kernel text) |
| `EC=0x0 ISS=0x0` | unknown reason | wild branch (`ELR=0xcf90`) |

Example of the dominant one:

```
[SSH-EXEC] Command: Ok("curl -v http://10.0.2.2:8477/hello.txt")
[AS-NEW] pid=5 l0=0x403af000 asid=0x5 via=spawn
[Exception] Sync from EL1: EC=0x25, ISS=0x47
  ELR=0x4015e874, FAR=0x12ae8, SPSR=0x800003c5
  WARNING: Kernel accessing user-space address!
  EC=0x25 in kernel code — killing current process (EFAULT)
  Killing PID 5 (/bin/curl)
```

`FAR=0x12ae8` is a low user VA with **no region record** — `exceptions.rs`
`try_resolve_el1_user_copy_lazy_fault()` self-gates on
`ensure_user_page_mapped()` finding a registered lazy anon region, so a page
belonging to the ELF's `.data`/`.bss` is refused and falls to the kill path.
That matches the known "ELF `.data`/`.bss` has NO region record" class from the
`cowstale`/`bssfork` work.

Killing the process also leaves the SSH exec channel hung rather than returning
an error, which is why the harness sees a timeout rather than a failed command.

**Not symbolized.** `extreme-size` sets `strip = "symbols"`. Rebuilding with
`--config 'profile.extreme-size.strip="none"'` does **not** preserve layout — the
flat binaries are the same length but differ from byte `0xb4` onward, so
addresses from the symbol build do not map onto a stripped-build PC. Any future
symbolization needs a different approach (e.g. a linker map, or accepting the
symbol build's own PCs from its own boot).

---

## 5. Open: `mv` a binary, then exec its old name

Found while trying to unshadow the built-in curl. **This is not a curl bug** and
it cost a wrong conclusion before it was isolated, so it is worth recording.

Repro (snapshot boot, so nothing is lost):

```
mv /bin/curl /bin/curl.hidden
curl http://10.0.2.2:8477/hello.txt
```

After the rename, `find_executable("curl")` still resolves `/bin/curl`:

- **release** — the *old* binary still executes successfully
  (`[PROC-EXIT] name=/bin/curl code=0`). Silently stale.
- **extreme** — executing through the stale entry wild-branches:
  `EC=0x22 (PC alignment), ELR=0xd`, 2/2 deterministic.

`rm` does not have the problem: after `rm /bin/curl`, the lookup misses, the
chain falls back to the built-in, and it works (extreme 2/2, release 1/1). So
the defect is in **rename**, not unlink — an ext2/VFS directory entry (or lookup
cache) is left resolving to a path that no longer exists.

The `ELR=0xd` wild branch is what first looked like "the built-in curl is broken
on extreme". It is not — the built-in curl works on both profiles once selected
cleanly via `SSH_BUILT_INS_FIRST`.

---

## 6. Reproducing

```bash
scripts/build_extreme_size.sh
MEMORY=64M scripts/cargo_runner.sh target/aarch64-unknown-none/extreme-size/akuma

# host side, in a directory containing hello.txt
python3 -m http.server 8477 --bind 0.0.0.0
```

Then, from the guest (10.0.2.2 is the SLIRP host gateway):

```
curl http://10.0.2.2:8477/hello.txt
```

Note the in-kernel shell is **not** a POSIX shell — no `PATH=`, no `;`/`&&`
tricks to pick a binary. Selection is: built-ins first only if
`SSH_BUILT_INS_FIRST`, otherwise `/usr/bin` then `/bin`, then the registry.

---

## Background

- `docs/reference/build-profiles.md` — profile/feature matrix; its "Known
  breakage" section described §1a before this fix.
- `docs/reference/subsystems/memory.md` — shared file-page cache design.
- `docs/reference/subsystems/config-flags.md` — `SHARED_FILE_PAGES_ENABLED`,
  `SSH_BUILT_INS_FIRST`.
- `docs/archive/BKL_RUSTC_SCALING_BASELINE.md` — why the page cache exists.
