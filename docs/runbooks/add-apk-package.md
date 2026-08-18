# Add an apk package to the devbox

How to add an Alpine apk package to the devbox image. Applies to any apk-based
image; the devbox is the reference.

> **Load-bearing rule:** add packages in the **single** `apk add` transaction.
> Separate `apk add` calls once reset apk's "wanted" set and purged earlier
> steps' packages (a real war story — see `overlays/devbox/bootstrap.sh` step 6
> comment).

## Steps

1. Edit `overlays/devbox/bootstrap.sh`. Find the relevant `apk ... add`
   transaction:
   - Step 6 (`DEVBOX_GIT`): `apk add ... musl git`
   - Step 7 (`DEVBOX_RUST_TOOLCHAIN`): the list is **built into `$DEVBOX_APK_PKGS`**
     (`clang lld gcc binutils make musl-dev`, plus `rust cargo` only when
     `DEVBOX_STABLE_RUST=true`) and then passed to one `apk ... add
     $DEVBOX_APK_PKGS`. Add your package to that **variable**, not to the `apk add`
     line — and note the variable is handed to the container as
     `-e DEVBOX_APK_PKGS="$DEVBOX_APK_PKGS"`; the bare `-e DEVBOX_APK_PKGS` form
     silently passes nothing, because bash does not export a plain assignment.
2. Add your package to the **same** `apk add` line. Do NOT create a new
   transaction.
3. If it needs `/etc` config, add it under `overlays/devbox/rootfs/etc/` (the
   sole `/etc` source).
4. Rebuild: `overlays/devbox/bootstrap.sh`.

## Env knobs (skip a step)

| Var | Default | Skips |
|---|---|---|
| `DEVBOX_CA_CERTS` | true | Mozilla CA bundle |
| `DEVBOX_GIT` | true | apk git |
| `DEVBOX_RUST_TOOLCHAIN` | true | the whole step — **C toolchain included** |
| `DEVBOX_STABLE_RUST` | **false** | apk `rust`/`cargo` (opt-in; nightly under `/usr/local` is the default toolchain) |
| `DEVBOX_SOUNDTRACK` | false | bonus music |

## Why a single transaction

`apk add` computes a "wanted" package set, resolves deps, then commits. A
second `apk add` recomputes "wanted" from its own args **only** — packages
installed by the first call that aren't in the second's wanted set get purged.
Step 4 (rootfs overlay) and the applet-symlink step never clobber a real
(non-symlink) binary, so hand-placed binaries are safe.

## Adding a binary that isn't an apk package

Ship it in `bootstrap/bin` (or `bootstrap/usr`). `bootstrap.sh` step 2 copies
`bootstrap/bin` + `bootstrap/usr` in. Step 4's busybox applet loop skips any
existing real binary (`if [ -e ... ] && [ ! -L ... ]`), so it won't be
overwritten by a busybox symlink.

## Verify

```bash
overlays/devbox/bootstrap.sh
overlays/devbox/run.sh
# in-VM:
<your-package-binary> --version
```

## Background

- `overlays/devbox/bootstrap.sh` (the authoritative 8-step build).
- `archive/APK_MISSING_SYSCALLS.md` — dynamic-linking support (PT_INTERP).
- [`build-devbox.md`](build-devbox.md).
