# Build the kernel from inside Akuma on the trashcan (for the on-box agent)

Use this when you (a cat/agent — `meow` or `kot`, running as `dumpster-akuma-amd64`
or under whatever name the roster gives the bare-metal box) have been asked to
compile, build, or self-host the Akuma kernel **on the box you are already
running on**. You are inside Akuma's own userland on the HP box ("the
trashcan"), not on a laptop, not on the Ubuntu side. This is the whole loop —
five commands, no recon needed.

If you are instead driving this box *remotely* from a laptop, this is the
wrong doc: see [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) and
[`selfhost-kernel-build-amd64.md`](selfhost-kernel-build-amd64.md) instead.

## 0. Confirm where you are

```sh
uname -a
```

Must say `Akuma`, not `Linux`. If it says `Linux`, you are on the box's
**Ubuntu** personality (a different machine, same hardware) — stop, this doc
does not apply there.

## 1. Build

```sh
kbuild
```

That's it. `kbuild` (`/bin/kbuild`) already sources `/etc/akuma-dev.env`, `cd`s
to `$AKUMA_SRC` (`/src/github.com/netoneko/akuma`, a live git checkout — not
something you need to fetch or sync yourself), and runs `cargo build -p
akuma-amd64 --target x86_64-unknown-none --release --offline -j1`.

- **Never pass `-j1` yourself.** The default is already 1, and cargo rejects
  a duplicate `--jobs`: `error: the argument '--jobs <N>' cannot be used
  multiple times`. Only pass `-j` for a *different* value, and don't — SMP is
  on and `-j4`+ has previously corrupted this build and SIGSEGV'd the linker.
  One core, always.
- **Use a long timeout — at least 20 minutes.** A clean build (`kbuild -c`)
  is ~13–23 minutes. An incremental one is *not* reliably fast: measured live
  2026-09-25 against a cache last built 5 days earlier, `kbuild` recompiled
  most of the tree anyway (several intervening commits touched shared crates
  like `akuma-net`) and still took **15m 49s**. Don't assume "incremental"
  means "quick" — give it the same budget as a clean build unless you know
  the cache is fresh (built from the same commit you're about to build).
  Whatever `Bash`-equivalent tool you have, give it minutes, not the default
  30 s, or you will kill a build that was working and report a failure that
  never happened.
- **`kbuild -c`** cleans `$CARGO_TARGET_DIR` first (default `/root/ktarget`)
  and rebuilds all ~95 crates — the real trial, if you were asked to prove a
  clean build works rather than "did the incremental cache paper over
  something." Only reach for it when the task actually calls for that.

A non-zero exit status is the failure signal; read the tail of the output for
the `^error` lines rather than a general "something's wrong" scroll.

## 2. Confirm the artifact

```sh
ls -la /root/ktarget/x86_64-unknown-none/release/akuma-amd64
```

An ELF with today's mtime and a size in the low single-digit MB (3–4 MB is
typical). That confirms the build actually produced something — `cargo`
returning 0 and there being no binary is not a state you should trust blind.

**Building is where this task usually ends.** Do not install or reboot unless
you were specifically asked to — see below for why.

## 3. Installing and booting it (only if asked)

`/boot/akuma-amd64` is the **default** GRUB entry on this box. A bad install
means the machine comes up at the GRUB prompt and needs a human standing at
it — there is no remote way back. Only do this step if the task explicitly
asked for an install/deploy/reboot, not merely "does it build."

```sh
kinstall                    # verifies the multiboot2 header, keeps a .prev, does NOT reboot
/bin/busybox reboot -f      # -- only after kinstall exits 0
```

`reboot -f` returns you to **Akuma**, not Ubuntu (this box's GRUB default has
been `Akuma/amd64` since 2026-09-18 — there is no `grub-reboot` one-shot to
consume). Give the box ~30–60 s to come back, then confirm the new kernel
reports its own boot suite:

```sh
dmesg | grep -E 'passed|failed'
```

Promote the fallback **only after** that reboot proves the kernel boots and
passes — never at install time:

```sh
cp -f /boot/akuma-amd64 /boot/akuma-amd64.good && sync
```

`Akuma/amd64 (known good)` is the *only* way back to a working kernel without
someone physically at the machine, so promoting a kernel that turns out
broken destroys the one thing standing between a bad build and a stuck box.

## Traps

| symptom | cause |
|---|---|
| `Error loading shared library librustc_driver-….so` | you ran `cargo`/`rustc` directly instead of through `kbuild`, so `LD_LIBRARY_PATH` was never set — this kernel has no `/proc/self/exe`, so musl can't resolve rustc's own `$ORIGIN` relative path on its own |
| `relocation R_X86_64_32 cannot be used against symbol '_start'` | built from the wrong working directory — cargo reads `.cargo/config.toml` from the *cwd*, not `--manifest-path`. `kbuild` always `cd`s to `$AKUMA_SRC` first; don't invoke cargo yourself from somewhere else |
| `error: the argument '--jobs <N>' cannot be used multiple times` | you passed `-j1` (or any `-j N`) as an extra arg on top of `kbuild`'s own default — see step 1 |
| build SIGSEGVs the linker near the very end, deterministically | you built at `-j4`+ with SMP on. Rebuild at the default `-j1` |
| a symbol you just added is "not found" | the box's checkout can drift from what you think is there — `git -C $AKUMA_SRC log -1` and `git status` before assuming the code is stale, not the build |
| `/bin/kbuild` (or `ubuild`/`mbuild`/`kinstall`) missing or stale | reinstall from the checkout: `cd $AKUMA_SRC && cp -f scripts/box/{kbuild,ubuild,mbuild,kinstall} /bin/ && chmod +x /bin/{kbuild,ubuild,mbuild,kinstall}` — [`scripts/box/README.md`](../../scripts/box/README.md) |

## Verify

```sh
uname -a                                                       # Akuma ...
kbuild                                                         # exit 0
ls -la /root/ktarget/x86_64-unknown-none/release/akuma-amd64   # fresh ELF, ~3-4 MB
```

Measured live, 2026-09-25, on this exact box, branch `cats-everywhere`
(commit `4b6797be`): `kbuild` against a five-day-old cache exited 0 in
**15m 49s** (`Finished \`release\` profile [optimized] target(s)`) and
produced

```
-rw-r--r-- 1 0 0 3444592 Sep 25 11:33 /root/ktarget/x86_64-unknown-none/release/akuma-amd64
md5: f07b4c874f6a1472144455639b5b613b
```

— a fresh, correctly-sized ELF. Not installed or booted as part of this
check (see step 3: only do that if asked).

## Background

- [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) § "Working **on**
  the box" — the full story: what each env var is for, why proc-macro
  linking needs musl's `libc.so`/`libgcc_s.so` on lld's search path, why
  `execve` here can't run a `#!` script, and the laptop-driven remote loop
  this doc deliberately does not cover.
- [`../../scripts/box/README.md`](../../scripts/box/README.md) — what the
  five installed files (`akuma-dev.env`, `kbuild`, `ubuild`, `mbuild`,
  `kinstall`) actually are and where they come from.
- [`../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md`](../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md)
  — the bring-up that made self-hosting on the metal possible at all.
- The sibling `akuma-miot` repo's `HANDOFF.md` § "The agent state machine"
  (2026-09-24) — the incident this doc exists to prevent: asked to compile
  the kernel, an on-box cat (`meow`) burned five turns on recon, never called
  `Bash`, and the build never started.
