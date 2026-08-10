# Profile inconsistencies found while trimming the in-kernel SSH server

Written 2026-08-10 on branch `trim-fat-sshd`. Companion to
[`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md) — that doc covers the change
itself; this one records what touching every profile at once exposed about the
profile system, plus a note on where `acceptance/` stands.

Most of what follows is **not fixed** (C and F are; the rest are not). Each item
was confirmed by building or booting, not inferred, and is written down because
the next person to add a profile will hit the same edges.

## A. A profile is three things, and only a shell script binds them

A build target is *profile + feature set*, and the only place the correct pairing
exists is `scripts/build_*.sh`. Nothing enforces it:

```bash
cargo build --profile extreme-size          # compiles, and is NOT the extreme kernel
scripts/build_extreme_size.sh               # the actual extreme kernel
```

The first inherits `size` codegen but keeps default features, so it has no
`extreme` feature — and `build.rs` keys the extreme behaviour off
`CARGO_FEATURE_EXTREME`, not off the profile. You get a silently different
kernel with the same output path. Same shape for `--profile release-smp`
without `--features smp`, and `--profile devbox` without `--features devbox`.

`Cargo.toml` documents the pairing in prose per profile; `build.rs` asserts only
that `smp` and `smp-shared` aren't both on. A cheap improvement would be for
`build.rs` to fail when a profile's discriminating feature is absent, the same
way it already panics on the SMP pair.

## B. `kernel_profile_size` also means "extreme"

```rust
let size_profile = std::env::var("OPT_LEVEL").as_deref() == Ok("z");
let extreme_profile = size_profile && env::var("CARGO_FEATURE_EXTREME").is_ok();
if size_profile    { println!("cargo:rustc-cfg=kernel_profile_size"); }
if extreme_profile { println!("cargo:rustc-cfg=kernel_profile_extreme"); }
```

`extreme` is a **subset** of `size`, not a sibling — but the names read as two
alternatives. Every `#[cfg(not(kernel_profile_size))]` in the tree also excludes
extreme, which is usually what was meant but never what the name says. `main.rs`
repeats `not(any(feature = "no-tests", kernel_profile_size))` a dozen times to
mean "the boot suite exists"; this branch added `cfg(kernel_tests)` for exactly
that predicate, and the rest of those sites could move onto it.

## C. `userspace-sshd` was a runtime flag wearing a build flag's name — FIXED

It only set `config::ENABLE_USERSPACE_SSHD`, skipping a startup branch while the
entire SSH-2 server stayed linked in. Two profiles (`devbox`, `devbox-smoltcp`)
selected it believing they had dropped the server. `devbox-smoltcp` — the
documented *default* devbox — shipped a complete second SSH server it never
started. Now a real `cfg(kernel_builtin_ssh)` gate; see
[`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md).

The general lesson: a feature whose only effect is a `const` cannot shrink an
image, and naming it after the thing you wish it did hides that for a year.

## D. Three different idioms for "what is in this image"

| script | idiom |
|---|---|
| `build_size.sh`, `build_extreme_size.sh` | `--no-default-features` + explicit re-add |
| `build_devbox.sh` | `--no-default-features` + explicit re-add |
| `build_devbox_smoltcp.sh` | layers `--features devbox-smoltcp,no-tests` **on top of defaults** |
| `cargo build --release` | defaults |

So answering "does this image have X" takes a different method per target, and
the meta-features (`devbox`, `devbox-smoltcp`) hide their expansion inside
`Cargo.toml`. This is what let `devbox-smoltcp` quietly keep `smoltcp` +
`kernel-tls` + the built-in SSH server while its name and docs said otherwise.

## E. The profile table mixed units, and two entries were stale

`docs/reference/build-profiles.md` listed a "Binary size" column that was linked
`.text` for `size`/`extreme-size` and on-disk ELF for everything else, dated a
month apart, with the mismatch explained in a footnote below the table. Measured
this session:

| row | table said | measured |
|---|---|---|
| `extreme-size` | 684 KB text | **578 KB text** (591,464 B) |
| `release` | 3.8 MB | **4.3 MB** before this branch, 3.1 MB after |

Both corrected. The underlying problem is that the numbers are hand-copied and
there is no `scripts/` helper that regenerates the row set, so drift is
guaranteed. Worth a small script that prints the table.

## F. A documented profile did not build — FIXED

`scripts/build_devbox.sh` (rump-only devbox) failed:

```
error: unused import: `crate::runtime::PreemptGuard`
  --> crates/akuma-net/src/socket.rs:20:5
```

`unused_imports = "deny"` is workspace-wide. The import's only consumer is
`with_table`, which is `#[cfg(feature = "smoltcp")]`; the import was not, so it
went unused on any build without the native stack — i.e. exactly and only the
rump devbox. Fixed by giving the import the same gate as its use site.

**Pre-existing and unrelated to the SSH work** — confirmed by stashing this
branch's changes and reproducing. It is kept in this list because one of the
seven documented targets had been unbuildable without anyone noticing, which is
the same class of problem as G below: a target nothing routinely builds rots
quietly. `scripts/build_devbox.sh` now produces a 1.8 MB image again.

## G. The default devbox has no acceptance coverage

`devbox-smoltcp` is *the* documented "develop inside Akuma" image. No playbook
in `acceptance/` builds it. Mapping every playbook to the target it builds:

| playbook | builds |
|---|---|
| 01, 02, 03, 04, 10, 11 | `cargo build/run --release` |
| 05, 07, 08 | `build_extreme_size.sh` |
| 06 | both extreme and release |
| 12 | `build_smp.sh` |
| 09 | names no kernel build at all |

`devbox`, `devbox-smoltcp`, `release-smp-shared` and `size`: zero playbooks. The
two most-exercised targets are `release` and `extreme-size`, and everything else
is covered by hand or not at all.

## H. Runtime policy was profile-blind

- `config::AUTO_START_HERD` was `true` for every profile including the 4 MB
  floor, where herd plus the services it starts costs ~1908 KB of a 4608 KB box
  (measured). Now `!(extreme && userspace-sshd)`.
- `bootstrap/etc/herd/enabled/sshd.conf` still pins `--port 23`, chosen for the
  multikernel demo when the built-in server owned port 22. It is a single
  disk-side file applied to every image regardless of profile, which is why
  removing the built-in server moved *every* non-extreme image's SSH from host
  2222 to 2323. Unresolved — see `BUILTIN_SSH_REMOVAL.md`.

Both are cases where a policy that is really per-profile lives somewhere with no
notion of profile: a global `const`, or a file on the disk image.

## I. Boot markers are profile-dependent; their consumers are not — ACTION NEEDED

`[SSH Server] Listening` is printed by the **built-in** server only, so after
this branch it appears on `extreme-size` and nowhere else. Three consumers still
poll for it unconditionally:

| consumer | effect on a non-extreme image |
|---|---|
| `scripts/test_memory_split.py:49` | waits forever |
| `docs/runbooks/boot-and-connect.md` | documented wait never fires |
| `docs/runbooks/debug-boot-hang.md:45` | readiness marker never appears |

`CLAUDE.md` also documents this grep as *the* boot-wait recipe. A profile-neutral
readiness marker is needed — `[Main] Network ready!` is printed on every profile,
and herd's `Started sshd` covers the userspace path. This is direct fallout of
the SSH removal and should be fixed before the next long test run.

## Feature matrix

Resolved by `cargo tree --depth 0 -f "{f}"` per target on 2026-08-10 — cargo's
own answer, not read off the scripts. `sc-*` is all eight syscall families
(`aio`, `sysv-ipc`, `framebuffer`, `containers`, `timerfd`, `eventfd`, `pidfd`,
`epoll`); `no-bkl-*` is all six (`network`, `vfs`, `process`, `mm`, `drivers`,
`irq`).

| feature | release | size | extreme | smp | smp-shared | devbox | devbox-smoltcp |
|---|:-:|:-:|:-:|:-:|:-:|:-:|:-:|
| `smoltcp` | ● | ● | ● | ● | ● | – | ● |
| `kernel-tls` | ● | ● | – | ● | ● | – | ● |
| `tls-rsa` | ● | – | – | ● | ● | – | ● |
| `neko` | ● | – | – | ● | ● | ● | ● |
| `sound` | ● | – | – | ● | ● | ● | ● |
| `rump` | ● | – | – | ● | ● | ● | ● |
| `rump-default` | – | – | – | – | – | ● | – |
| `fs-cache` | ● | – | – | ● | ● | – | ● |
| `sc-*` (8) | ● | ● | – | ● | ● | ● | ● |
| `no-tests` | – | ● | ● | – | – | ● | ● |
| `rump-tests` | – | – | – | – | – | ● | – |
| `extreme` | – | – | ● | – | – | – | – |
| `smp` | – | – | – | ● | – | – | – |
| `smp-shared` | – | – | – | – | ● | – | ● |
| `no-bkl-*` (6) | – | – | – | – | ● | – | ● |
| `userspace-sshd` | – | – | – | – | – | ● | ● |

Derived cfgs:

| cfg | release | size | extreme | smp | smp-shared | devbox | devbox-smoltcp |
|---|:-:|:-:|:-:|:-:|:-:|:-:|:-:|
| `kernel_profile_size` | – | ● | ● | – | – | – | – |
| `kernel_profile_extreme` | – | – | ● | – | – | – | – |
| `kernel_builtin_ssh` | – | – | ● | – | – | – | – |
| `kernel_tests` | ● | – | – | ● | ● | – | – |

What the matrix shows:

- **`size` has no unique feature.** Its column is `release` minus
  `{neko, sound, rump, fs-cache, tls-rsa}` plus `no-tests`. Every one of those is
  a subtraction; there is nothing you can only get from `size`.
- **`extreme` is the only column with a unique feature** (`extreme`) and the only
  one that drops the `sc-*` families.
- **`release`, `smp` and `smp-shared` are the same image** plus their SMP model.
  `smp-shared` additionally carries the six `no-bkl-*` flags.
- **`devbox` is the odd one out**: the only target without `smoltcp`, and the
  only one with `rump-default`/`rump-tests`.
- **`devbox-smoltcp` is `smp-shared` + `userspace-sshd` + `no-tests`.** Nothing
  else distinguishes it.

## Dropping `size`: what it would cost

Sketched here because it came up while reading the matrix; not a decision.

**In favour.** `size` has no unique capability (above), no acceptance playbook
builds it, and it is the reason `kernel_profile_size` is emitted for two
profiles — finding B. Collapse it and that cfg means exactly "extreme", so
`kernel_profile_size` and `kernel_profile_extreme` become the same predicate and
one of them can go.

**What depends on it today.**

| dependency | note |
|---|---|
| `[profile.extreme-size] inherits = "size"` | would need to inherit `release` and re-declare `opt-level = "z"`, `lto`, `codegen-units = 1`, `strip`, `panic = "immediate-abort"` |
| `.git/hooks/pre-commit` runs `cargo clippy --profile size` | drop the line or point it at `extreme-size` |
| `scripts/build_size.sh`, `scripts/symbol_sizes.py` example, `cargo_runner.sh:100` (`/size/` path match) | script/doc updates |
| **101 `kernel_profile_size` sites** across `src/` and `crates/` (20 in `main.rs`, 12 in `syscall/mod.rs`, 10 in `config.rs`) vs 56 for `kernel_profile_extreme` | the real work — a mechanical but wide sweep, and each site needs a decision: did it mean "small" or "extreme"? |

That last row is the catch. The two cfgs are **not** interchangeable today —
`kernel_profile_size` is true for extreme as well, so any site meaning "size but
not extreme" is currently expressed as `all(kernel_profile_size, not(kernel_profile_extreme))`
and any site meaning "either" is the bare `kernel_profile_size`. Collapsing them
is safe only after auditing all 101, and the ones that are really "the boot suite
is absent" should move to `cfg(kernel_tests)` instead (this branch added it).

Cheapest sequencing if this is wanted: (1) migrate the "no boot suite" sites off
`kernel_profile_size` onto `kernel_tests`, (2) see how many of the remaining 101
genuinely mean "extreme", (3) only then decide whether `size` is worth keeping.
Steps 1–2 are useful on their own even if `size` stays.

**On devboxes inheriting `release`:** they already do at the *profile* level —
`[profile.devbox] inherits = "release"`, and `devbox-smoltcp` builds under
`release-smp-shared`, which also inherits `release`. The divergence is in
*features*, not codegen: `devbox-smoltcp` layers on the default set, while
`devbox` passes `--no-default-features` to drop `smoltcp` (finding D). If
`devbox` also kept the defaults, the two would collapse to one axis —
`devbox = devbox-smoltcp + rump-default` — at the cost of carrying a smoltcp
stack the rump image never routes through. On a 1.8 MB dev image that cost is
probably worth the simplification, but it has not been measured.

## Two latent defects the `smp-shared` default exposed — both FIXED

`smp-shared` moved into the default feature set on 2026-08-10 (real SMP is now
what `cargo build --release` gives you; the multikernel `smp` is the
experimental alternative and needs `--no-default-features`). Nothing about that
change is a bug, but it compiled two configurations that had never been built
before, and each had a latent defect waiting in it.

**1. `crates/akuma-exec/src/bkl.rs` — cfg mismatch between a static and its users.**
`KERNEL_LOCK` was gated `#[cfg(kernel_smp_shared)]` while every one of its users
is `#[cfg(all(kernel_smp_shared, target_os = "none"))]`. Invisible while
`smp-shared` was opt-in, because no host build ever set it. The moment it became
default, `cargo test` (host target) compiled the static with nothing referencing
it and `dead_code = "deny"` failed the build. Fixed by giving the static — and
its `use crate::sync::KernelLock;` — the same `target_os = "none"` gate as its
users.

**2. `threading::disable_preemption()` panicked without a registered runtime.**
It calls `runtime().uptime_us` to stamp `PREEMPTION_DISABLED_SINCE` on the
0→1 transition, and `runtime()` is the *panicking* accessor
(`runtime.rs:265`: "ExecRuntime not registered — call akuma_exec::init() first").
`PreemptGuard::new()` documents itself as needing no registration —

> Direct call: akuma-exec owns threading. No runtime registration needed … so
> this works during early boot and in host tests alike.

— which that one line made false. It surfaced as `akuma-ext2 tests::append_to_file`
panicking in the workspace host-test run: `akuma-ext2`'s `no-bkl-vfs` paths take a
`PreemptGuard`, and once `smp-shared` was unified into the graph the guard became
real instead of an empty struct. Fixed by probing `crate::runtime::is_registered()`
and degrading the timestamp to `0` — the documented contract now holds.

Worth noting how it presented, because it wasted a cycle: `cargo test` **aborts**
on the first failing crate, so the workspace total dropped from 399 to 250 and a
naive "sum the passed counts" read it as a smaller-but-clean run. The
`test result: FAILED. 52 passed; 1 failed` line has a different field layout from
`test result: ok. 198 passed; …`; any harness parsing these must handle both or it
will report a failure as a pass.

Both defects were pre-existing and dormant. Neither was caused by the SSH
removal; both were caused by *building a combination nobody had built*, which is
the same class as finding F above (`scripts/build_devbox.sh` unbuildable) — a
configuration nothing routinely compiles rots quietly.

## Acceptance suite: trimmed

The review flagged above happened later the same day. Kept: **05, 08, 10, 11**.
Moved to `acceptance/archive/`: 01, 02, 03, 04, 06, 07, 09, 12. `acceptance/`
itself is now four files; nothing in the numbering or content of the kept four
changed, so `acceptance/05_meow_tcc_extreme_4mb.md`,
`08_meow_clone_compile_run.md`, `10_selfhost_compile_akuma.md`, and
`11_netbsd_rumpkernel_irc.md` are still the same paths they always were.

**Why these four.** Each proves a structurally different capability rather than
a variation on the same one: 05/08 are the extreme-size 4.0 MB floor (scratch
clone + static tcc compile + run, the tightest resource-constrained path in the
repo), 10 is the self-hosting probe (rustc building Akuma's own kernel inside
Akuma), 11 is the NetBSD rump TCP/IP stack proof (the only playbook exercising
a second network stack end-to-end, including an IRC capstone that actually
passed). None of the twelve ever covered `devbox`, `devbox-smoltcp`, `size`, or
`smp`/`smp-shared` (finding G above), so this trim doesn't create that gap —
it was already there. It also doesn't fully realize the "touch each profile
once" heuristic from finding G: 10 and 11 both build `release` (per the
`cargo build/run --release` row in the target-mapping table above), so the kept
set is extreme-size ×2, release ×2, and still zero for the other four targets.
The selection optimizes for distinct *capability* coverage, not distinct
*profile* coverage — worth knowing if `acceptance/` gets revisited again.

**Overlap among the eight archived playbooks.** None were deleted — they still
run, and still document real ground — but most were fully or mostly subsumed
by one of the kept four or by each other:

| archived | overlaps with | relationship |
|---|---|---|
| 01 (apk bootstrap: install busybox) | 02 | 02's prep is 01's prep verbatim (same SSH-key/apk-tools/disk steps) plus git+tcc on top; 01 is a strict subset |
| 02 (apk-installed git + tcc, clone + compile + run) | 05, 08 | same clone-then-compile-then-run shape as 05/08, but via `apk add git tcc` instead of `scratch` + static tcc — an earlier, heavier path to the same proof |
| 03 (two-VM meow+tcc, llama.cpp backend) | 04 | identical task to 04 (meow compiles hello.c with tcc) with a second QEMU VM standing in for the Ollama host — same proof, different LLM transport |
| 04 (meow+tcc memory sweep, release kernel) | 05, 08 | direct ancestor: same meow-drives-tcc-via-Shell-tool pattern, on `release` instead of `extreme-size`; 05/08 repeat its memory-sweep methodology at the lower floor |
| 06 (meow+tcc via CGI/httpd) | 05 | says so explicitly in its own header — "the same clone → compile → run task as test 05," only the trigger (HTTP POST vs SSH) differs |
| 07 (tcc -static prereqs, extreme, no agent) | 05, 08 | the layer underneath 05/08 — same extreme-size 4.0 MB floor, minus the meow/scratch orchestration; 05's and 08's own floor-reference tables cite 07's numbers directly |
| 09 (nca + Docker chroot, no Akuma kernel) | 08 | carries an explicit "Differences from acceptance/08" comparison table in its own text; same clone+compile+run proof, ported off the Akuma kernel entirely to isolate the memory floor from kernel overhead |
| 12 (multikernel demo: core pinning + forwarding) | — | no overlap with the tcc/meow cluster or with each other; distinct subsystem (SMP/multikernel core lifecycle). Archived on suite-size grounds, not redundancy — per finding G it was the *only* playbook covering the experimental `smp` feature, so multikernel now has zero acceptance coverage (`docs/reference/subsystems/smp.md` notes this) |

The one non-obvious exception to "subsumed by a kept test": **04, 06, 07 also
overlap with each other**, not just with 05/08 — 06 is 05's task via CGI, and
07 is what both 04 and 06's underlying tcc step reduces to once you strip the
agent. If `acceptance/archive/` itself ever needs a further cut, that
04/06/07 trio is the next redundant cluster, with 07 the most load-bearing of
the three (05/08 cite its measured floor numbers) and 04 the least (fully
superseded by 08's memory-sweep procedure on the tighter profile).

Two facts that motivated the cut, carried over from the original review:

- **11 of 12 playbooks referenced port 2222**, which after this branch answers
  on `extreme-size` alone (finding I). Only 09 did not.
- **Coverage was lopsided, not thin** — four playbooks exercised extreme,
  seven exercised release, four other documented targets had none. That
  imbalance is why the kept set leans extreme-heavy (05/08) rather than
  release-heavy: extreme was already the better-proven floor and the harder
  constraint to keep passing.

## Background

- [`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md) — the change these were
  found during, with the size and free-RAM measurements.
- [`TRIM_FAT_SSHD.md`](TRIM_FAT_SSHD.md) — the userspace sshd size work that
  started the thread.
- [`FPCACHE_UNDERSIZED_AT_LOW_RAM.md`](FPCACHE_UNDERSIZED_AT_LOW_RAM.md) — the
  other open defect from the same session.
- `docs/reference/build-profiles.md` — current-state profile reference (updated
  on this branch).
- `docs/reference/subsystems/config-flags.md` — every feature, cfg and env knob.
