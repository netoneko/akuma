# `box run` — docker images on overlays

**Repo:** `/Users/netoneko/github.com/netoneko/akuma`, branch `fix-cargo-networking`.
**Read first:** `docs/reference/subsystems/containers.md`, `userspace/box/docs/OCI_IMAGE_PULL.md`.

Goal: `box run <image> [cmd]` runs a real Docker image without mutating the
image, sharing layers between containers. Sanity target: a `curl` from Docker
Hub making an HTTPS request from inside a box.

## Status (2026-08-11)

**Phases 0, 1, 2 and 3 are implemented and verified on devbox-smoltcp.** The
sanity ladder passes end to end:

```
box pull busybox            → 4.1 MB layer store (was 467 MB, see "Two bugs found")
box run --rm busybox /bin/busybox echo hi                     → hi
box run --name t9 busybox /bin/busybox sh -c '…write, delete…'
    → copy-up in the container's upper, image layer byte-identical afterwards,
      `rm /etc/group` recorded as `.wh.group`, merged listing correct
box pull curlimages/curl
box run --rm --entrypoint curl curlimages-curl --version
    → curl 8.21.0 (aarch64-unknown-linux-musl) OpenSSL/3.5.7 …
box run --rm --entrypoint curl curlimages-curl -sS https://example.com
    → the page. DNS, TCP over smoltcp from a non-zero box, TLS with the image's
      own CA bundle.
box pull alpine/curl        → 3 layers, merged root, same HTTPS result
```

Two things landed on top of the plan, both from review:

- **`userspace/tar` is a library** (`akuma_tar`) that `box` links, instead of a
  binary it spawns. Header parsing (`format.rs`) is pure and host-tested, and
  the API refuses entries whose paths escape the extraction directory and caps
  the decompressed size.
- **Mount policy is locked down.** A boxed process can no longer mount or
  unmount at all, and a box's root can be set once and never redirected. The
  consequence is deliberate: no nested OCI images. Nested *boxes* still work.
  See `docs/reference/subsystems/containers.md` -> "Mount policy".

Phase 2b (`/proc/mounts`) and Phase 4 (env passthrough) are **not** done. Two
gaps found while running the ladder are written up under "Follow-ups" — neither
is overlay work, both are worth fixing.

Full write-up: [`BOX_DOCKER_COMPAT.md`](BOX_DOCKER_COMPAT.md).

## Where things stood before this work (verified 2026-08-11)

- `box pull` works end-to-end — auth token → manifest list → `linux/arm64`
  manifest → config + layer blobs (`userspace/box/src/oci.rs`).
- It extracts **every layer, stacked, into one directory**:
  `/var/lib/box/images/<name>/rootfs` (`oci.rs:281-329`). Whiteout entries
  (`.wh.*`) land as ordinary files; `userspace/tar` has no notion of them.
- `box open -i <img>` points the box root *at that same directory*
  (`main.rs:246`). A container therefore writes into the image, and two
  containers share one mutable rootfs. This is the thing overlays fix.
- A box's jail is a `SubdirFs` mounted at `/` in the box's `Namespace`, created
  by `REGISTER_BOX` (`src/vfs/mod.rs:63-75`). `spawn_ext` activates that
  namespace for the child (`crates/akuma-exec/src/process/spawn.rs:108-120`).
- `MOUNT_IN_NS` (syscall 325) understands exactly two fstypes, `proc` and
  `tmpfs` (`src/syscall/container.rs:174-180`). Highest custom syscall in use is
  `CORE_INIT = 327`.
- `run` is currently a bare alias for `open` (`main.rs:96`). No docker-shaped
  command exists.
- Nothing anywhere implements overlays.

Everything below is the plan as written; the Status block above records what
actually landed against it.

## Phase 0 — content-addressed layer store (userspace only)

`userspace/box/src/{oci,images}.rs`. Extract each layer into its own directory
instead of stacking them:

```
/var/lib/box/layers/sha256-<hex>/          # extracted once, shared across images
/var/lib/box/images/<name>/oci-config.json
/var/lib/box/images/<name>/layers          # ordered digest list, one per line
```

- Extract to `<digest>.tmp`, then `rename` (the kernel has `renameat`), so a
  half-extracted layer can never be mistaken for a complete one. Skip a layer
  whose final dir already exists.
- **Leave `.wh.` files in place.** Whiteouts belong to the layer; the overlay
  interprets them at runtime, which is what the OCI spec intends.
- Drop the per-layer size-probe loop (`oci.rs:295-319`) while in this file — it
  reads every layer a second time just to print a number.

## Phase 1 — `OverlayFs` (`crates/akuma-isolation/src/overlay_fs.rs`)

```rust
pub struct OverlayFs { upper: Arc<dyn Filesystem>, lowers: Vec<Arc<dyn Filesystem>> }
```

Each member is a `SubdirFs` over the root ext2. Semantics:

- **lookup** — upper first, then lowers topmost-first. `.wh.<name>` in a higher
  layer masks `<name>` in every lower one; `.wh..wh..opq` in a directory masks
  the whole corresponding lower directory.
- **read_dir** — merge, first-wins dedupe, drop `.wh.*` entries themselves and
  everything they mask, honour opaque markers.
- **writes** — copy-up: materialize the parent directory chain in upper, copy
  the file from the winning lower, then delegate. The `Filesystem` trait's
  default `write_at` is already read-modify-write, so copy-up rides on it.
- **remove** — delete from upper if present; if the name still exists in a
  lower, drop a whiteout in upper.

**The critical constraint:** `read_at_by_inode` passes straight through to the
underlying filesystem, and that is only sound because every layer and the upper
directory live on the same ext2, so inode numbers are already unique. The file
page cache is keyed on inode (`src/file_page_cache.rs`, faulted from
`src/exceptions.rs:3684`), so a `MemoryFilesystem` upper would be actively
dangerous: memfs synthesizes inodes as an FNV hash of the path
(`crates/akuma-vfs/src/memfs.rs:56`), which collides with real ext2 inode
numbers and would hand the fault path the wrong page. **ext2-backed upper only.**
A tmpfs upper for `--rm` needs an inode-namespacing scheme; deferred.

This phase is fully host-unit-testable with a fake `Filesystem`, exactly like the
`SubdirFs` tests (`crates/akuma-isolation/src/subdir_fs.rs:260`). Build the
whiteout / merge / copy-up matrix as host tests before booting anything.

## Phase 2 — plumbing the mount

Extend `MOUNT_IN_NS` with fstype `"overlay"` plus a Linux-shaped data string
(`lowerdir=a:b:c,upperdir=d`); the dispatch has a free argument slot. Keep the
box-0-only rule — `box` runs in box 0.

Ordering wrinkle: `create_box_namespace` has already mounted a `SubdirFs` at
`/`, and `MountNamespace::mount` rejects a duplicate path
(`crates/akuma-isolation/src/mount.rs:30`). Userspace must not be able to
unmount `/` (correctly refused, `src/syscall/container.rs:135-141`), so add a
kernel-side root-replace used only by this handler. (Shipped as
`MountNamespace::replace_pristine_root`, a one-shot — see the archive doc.)

## Phase 2b — make mounts visible in procfs

There is **no `/proc/mounts` today** — `ProcFilesystem::read_file`
(`src/vfs/proc.rs:496`) knows `boxes`, `cores`, `net/tcp`, `net/udp`,
`sysvipc/msg` and the per-pid files, and nothing else. `MountNamespace::list_mounts`
(`crates/akuma-isolation/src/mount.rs:109`) and `crate::vfs::list_namespace_mounts`
(`src/vfs/mod.rs:106`) already produce exactly the data needed, and the latter is
sitting behind `#[allow(dead_code)]` with no caller.

Once a box's `/` is an overlay of N layers, "what is actually mounted here" stops
being guessable, so this becomes a debugging necessity rather than a nicety —
and ordinary Linux software reads `/proc/mounts` and `/proc/self/mountinfo`.

- Add `mounts` and `self/mountinfo` to procfs, rendered in the real Linux
  formats (`<source> <target> <fstype> <opts> 0 0`), sourced from the **calling
  process's** namespace so a boxed process sees its own mount table and box 0
  sees the global one.
- `MountInfo` currently carries only `path` and `fs_type`
  (`crates/akuma-vfs/src/types.rs:104`). Extend it with a source/options string
  so an overlay can report its `lowerdir=…,upperdir=…`, which is the whole
  point of the exercise.
- Also list the entries in `read_dir` and give them sizes in `metadata`, or
  tools that stat before reading will skip them.

No sysfs exists at all; not needed for this work.

## Phase 3 — `box run`

```
box run [--rm] [-d] [-it] [--name X] [-e K=V] [-w dir] <image> [cmd [args...]]
```

Docker argument order. Leave `box open` alone, but stop aliasing `run` to it.

1. Resolve the image (error if not pulled, as today).
2. Allocate a container id; create `/var/lib/box/containers/<id>/upper`.
3. `REGISTER_BOX` rooted at the container directory.
4. Overlay-mount: lowers = the image's layer dirs, upper = `upper/`.
5. Inject `/etc/resolv.conf` and `/etc/hosts` into upper.
6. Optional `/proc` mount.
7. Entrypoint / Cmd / WorkingDir from the OCI config — factor the existing block
   out of `cmd_open` (`main.rs:248-283`).
8. `spawn_ext`, wait, and on `--rm` tear the container directory down.

Rewire `box open -i` onto the same overlay and delete the flat `rootfs/`.

## Phase 4 — env passthrough

`SpawnOptions` has no env field (`userspace/box/src/main.rs:33` and the kernel
side at `src/syscall/proc.rs:1410`), so `spawn_process_with_channel_ext` is
called with `None` and the child inherits `DEFAULT_ENV`. The image's
`config.Env` — `PATH` above all — is silently dropped today. Add
`env_ptr`/`env_len` to both copies of the struct, and audit every other caller of
syscall 315 before changing the layout.

## Phase 5 — sanity ladder

Each rung is a gate, in order:

1. `box run busybox /bin/busybox echo hi` — single layer, static. Proves the
   overlay mount, exec-from-overlay, and entrypoint plumbing.
2. `box run busybox /bin/busybox sh -c 'echo x >/tmp/x; cat /tmp/x'` — proves
   copy-up, and that the layer directory is still byte-identical afterwards.
3. A two-layer image whose top layer deletes a file — proves whiteouts.
   Hand-build the tars locally if no convenient public image fits.
4. **curl.** `curlimages/curl` is alpine + dynamic musl, which additionally
   exercises `ld-musl` (works since the RELR fix); a genuinely static build
   avoids that variable. Try static first, fall back. Don't pre-guess arm64
   availability — `box pull` reports `no linux/arm64 manifest found` when it is
   missing. Then `curl -sS https://example.com` inside the box.

Checked prerequisites: `/dev/urandom` works inside a box (path-intercepted at
`src/syscall/fs.rs:1353`, before namespace resolution), CA certificates come
from the image, DNS needs the injected `resolv.conf`, and sockets from a
non-zero box route to smoltcp by default.

## Two bugs found while bringing this up

Both were mistaken for overlay bugs at first. Neither is.

**1. `/bin/tar` was busybox, and akuma's `link()` copies whole files.**
`bootstrap/bin/tar` did not exist — `userspace/build.sh` had never deployed it —
so `/bin/tar` was a busybox applet symlink, and `box pull` had always extracted
layers with busybox tar. Busybox tar creates hardlinks with `link()`, and
`sys_linkat` (`src/syscall/fs.rs:2378`) is implemented as `read_file` +
`write_file`: a full copy that also loses the mode. The busybox image's 410
hardlinks to one 1.1 MB binary therefore became 410 real copies — **467.7 MB
extracted from a 1.9 MB layer** — every one of them `0644`.

That is why the container could not run its own commands: busybox's `PATH`
search calls `access(X_OK)`, and a `0644` binary fails it with EACCES, which
surfaced as `sh: cat: Permission denied` while `/bin/ls` (explicit path, no
access check) worked. Fixed by shipping our own tar and teaching it to apply the
archived mode bits (`apply_mode`, both extraction paths). The layer store went
4.1 MB and the applet symlinks resolve to a `0755` binary.

The underlying `linkat`-copies-files defect is untouched and worth its own fix:
ext2 can hold real hard links, and `remove_file` would need to decrement
`hard_links` and only free blocks at zero.

**2. `spawn_ext` does not honour shebangs.** `execve` does
(`src/syscall/proc.rs:743`), but syscall 315 goes straight to the ELF loader, so
`curlimages/curl`'s `/entrypoint.sh` could not be started. Worked around with
docker's own `--entrypoint` flag, which `box run` now supports; the real fix is
to give `spawn_ext` the same shebang handling `execve` already has.

## Follow-ups

- Phase 2b: `/proc/mounts` (above) — more valuable now that a box root is a
  union of N layers with nothing to inspect.
- Phase 4: env passthrough, so `PATH` and friends come from the image config.
  `box run` currently compensates with `resolve_in_container`, which walks the
  standard `PATH` directories itself.
- `spawn_ext` shebang support (see above); then `--entrypoint` becomes optional.
- Real hard links in `sys_linkat` (see above).
- Layer GC: `box rmi` / prune. Nothing reclaims a layer no image references.

## Known sharp edges

- `userspace/tar` applies mode bits as of this work; nothing checks the exec bit
  at *spawn*, but a shell's `PATH` search does. See "Two bugs found".
- `tar` converts hardlinks into relative symlinks (`userspace/tar/src/main.rs:248`)
  — harmless for overlay reads, wrong for anything inspecting link counts.
- `MAX_NS_MOUNTS = 16` (`mount.rs:7`) is plenty, but cap `OverlayFs::lowers` at
  ~32 anyway.
- No layer GC. `box rmi` / prune is a follow-up.
- `box rm` is currently an alias for `close`; container cleanup can share it.
