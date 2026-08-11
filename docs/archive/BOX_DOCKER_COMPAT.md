# Running Docker images on Akuma — `box run`, overlays, and two bugs underneath

**Date:** 2026-08-11. **Branch:** `fix-cargo-networking`. **Verified on:** devbox-smoltcp (SMP=4).

Goal: `box run <image>` starts a real Docker Hub image without mutating the
image, sharing layers between containers. Sanity target: `curl` from Docker Hub
making an HTTPS request from inside a box. Both achieved; the interesting part
is what was in the way.

Plan and open follow-ups: [`../../proposals/BOX_RUN_OVERLAYFS.md`](../../proposals/BOX_RUN_OVERLAYFS.md).
Current-state reference: [`../reference/subsystems/containers.md`](../reference/subsystems/containers.md)
-> "OCI images and the overlay root". Procedure:
[`../runbooks/run-docker-image.md`](../runbooks/run-docker-image.md).

## What existed before

`box pull` already spoke the registry protocol — auth token, manifest list,
`linux/arm64` resolution, config and layer blobs. It then extracted **every
layer, stacked, into one directory** (`/var/lib/box/images/<name>/rootfs`) and
`box open -i` pointed a box's root straight at it. A container wrote into the
image; two containers shared one mutable rootfs. Nothing implemented overlays.

## What was built

**Content-addressed layer store.** Each layer extracts to
`/var/lib/box/layers/sha256-<hex>/`, shared by every image referencing it, via a
`<digest>.tmp` staging directory renamed into place so an interrupted pull
cannot leave a directory that looks complete. The image directory keeps only
`oci-config.json` and a `layers` file (digests, base-first). Whiteout entries are
left on disk as files — they are part of the layer per the OCI spec, and the
overlay interprets them at lookup time, so an extracted layer needs no
rewriting.

**`OverlayFs`** (`crates/akuma-isolation/src/overlay_fs.rs`), one writable upper
over N read-only lowers, all `SubdirFs` over the same ext2. Lookup walks one
path component at a time so a whiteout or opaque marker on an ancestor hides the
subtree beneath it; `.wh.<name>` and `.wh..wh..opq` are honoured; writes copy up;
deletes that cannot touch a lower write a whiteout; re-creating a deleted name
clears it. 42 host tests.

The constraint worth remembering: `read_at_by_inode` forwards to the underlying
filesystem, which is sound **only** because every layer sits on the same ext2 and
inode numbers are globally unique. The file page cache is keyed on inode alone
(`src/file_page_cache.rs`). A `MemoryFilesystem` upper — the obvious way to make
`--rm` cheap — synthesizes inodes by hashing the path, would collide with real
ext2 inodes, and would hand the page-fault path an unrelated file's contents.

**`overlay` fstype on `MOUNT_IN_NS`**, taking a Linux-shaped
`lowerdir=a:b,upperdir=c` string. Mounting one at `/` replaces the box's
`SubdirFs` jail through `replace_pristine_root`, which is a one-shot: it fails
unless `/` still holds the untouched jail, and the syscall additionally refuses
a box that already has processes.

**`box run`**, docker-shaped: `--rm`, `-d`, `-i`, `--name`, `-w`,
`--entrypoint`. Entrypoint/Cmd composed the way `docker run` composes them
(command-line arguments replace **Cmd** and are passed to the Entrypoint),
`/etc/resolv.conf` and `/etc/hosts` injected into the upper layer.

## Two bugs, both mistaken for overlay bugs

### 1. `/bin/tar` was busybox, and `link()` copies whole files

`bootstrap/bin/tar` did not exist — `userspace/build.sh` had never deployed it —
so `/bin/tar` was a busybox applet symlink and `box pull` had always extracted
with busybox tar. Busybox creates hardlinks with `link()`, and `sys_linkat`
(`src/syscall/fs.rs:2378`) is `read_file` + `write_file`: a full copy that also
loses the mode.

The busybox image has 410 hardlinks to one 1.1 MB binary. Extracted:

```
467.7M  /var/lib/box/layers/sha256-025fe1949698…/     # from a 1.9 MB layer
-rwxr-xr-x  1185328  bin/[          # the real file
-rw-r--r--  1185328  bin/cat        # a copy. not executable.
-rw-r--r--  1185328  bin/ls         # another copy
```

The symptom did not look like this at all. It looked like:

```
sh: line 0: cat: Permission denied
sh: line 0: ls: Permission denied
```

while `/bin/ls /etc` with an explicit path worked fine. A shell's `PATH` search
calls `access(X_OK)` and refuses a `0644` binary; an explicit path just execs,
and **nothing in akuma's spawn or ELF loader checks the exec bit**, so the two
spellings disagreed. The chain from "Permission denied" to "our tar was never
shipped" ran through four wrong theories first — see "What misled" below.

Fixed by shipping our own tar and having it apply the archived mode bits.
Layer store: 467.7 MB → **4.1 MB**, `bin/[` at `0755`, the other 410 names
relative symlinks to it.

The `linkat`-copies-files defect underneath is untouched. Fixing it means real
hard links in ext2 (increment `hard_links`, add a directory entry) **and**
teaching `remove_file` to decrement rather than free blocks at the first unlink.

### 2. `spawn_ext` does not honour shebangs

`execve` does (`src/syscall/proc.rs:743`); syscall 315 goes straight to the ELF
loader. `curlimages/curl`'s Entrypoint is `/entrypoint.sh`, so it could not
start. Worked around with `--entrypoint`, a flag worth having anyway. The real
fix is giving `spawn_ext` the same shebang handling.

## Mount policy: composed from outside, once

Prompted by review during this work, two rules were tightened:

- **A boxed process may not mount or unmount at all.** `sys_mount` and
  `sys_umount2` are box-0-only (`umount2` now always fails; the host never used
  it either). Previously a box could mount into its own namespace and unmount
  anything except `/`. A mount table is the box's whole view of the filesystem —
  anything a box can mount, it can mount *over*, including its own `/proc` — so
  the namespace is composed entirely from outside, by box 0, before the box runs.
- **A box's root can be set once and never redirected.** `replace_pristine_root`
  refuses unless `/` is still the birth jail, and re-rooting a box that already
  has processes is `EPERM`. Swapping a live root would move the filesystem under
  processes holding paths and cwds resolved against the old one.

Consequence, and the intended one: **no nested OCI images.** Assembling a
container root requires an overlay mount, and no box can mount. Nested **boxes**
still exist — they are process and network-stack grouping, and
`register_box` of a subtree of one's own root still works (boot-test case 5).
Docker-in-docker can be revisited later; nothing needs it now.

Guards regress in `test_box_isolation_syscall_guards` (cases 8b–8d).

## `tar` is a library now

`box` linked `/bin/tar` by path, and a path can be replaced — which is exactly
what happened. `userspace/tar` is now `[lib] akuma_tar` + a thin CLI:

- `format.rs` — pure header parsing (modes, typeflags, USTAR prefix, checksums,
  path safety, gzip framing), **host-testable**, 13 tests. Both bugs above lived
  in header interpretation and none of it had a test.
- `extract.rs` — the I/O, behind the default `akuma` feature.
- `box` depends on the crate; `extract_layer` is a function call with a real
  `Result` instead of a spawn and an exit code.

Added while the API was no longer constrained by argv: entries whose paths
escape the extraction directory (absolute, or containing `..`) are refused and
counted — `box pull` treats a non-zero count as a failed layer — and the gzip
path has a 512 MB decompressed ceiling, since in-process that memory is box's
rather than a child's to lose.

## What misled

- **"Permission denied" pointed at the overlay.** It was mode bits, one layer
  down, from an extractor nobody had noticed was the wrong program.
- **`ls -l` inside the container looked like proof of symlinks** — 410 entries
  with identical sizes. They were identical because they were identical *copies*.
  `readlink` on the host settled it in one command; the size column never would
  have.
- **The layer tar was assumed to match what was on disk.** Downloading the blob
  on the host and running `tar -tvzf | awk` over the type column (410 `h`, 16
  `-`) is what proved the archive used hardlinks and the extractor had expanded
  them.
- **A one-off rung-2 failure** (empty output, exit 255) never reproduced across
  five subsequent runs, including twice back-to-back. Not explained; recorded
  here rather than claimed fixed.

## Open: `-i` from a login shell closes the session

Starting a container interactively from an SSH login shell kills that shell when
the container exits. Isolated 2026-08-11 by elimination: a plain command,
`box images`, and `box run -d` all leave the session alive; **everything that
calls `reattach` kills it**, `box open` included, so this predates `box run`.

Mechanism: `reattach_process_ext` gives the target the caller's
`Arc<ProcessChannel>` (`crates/akuma-exec/src/process/exec.rs:267`). The
container and the login shell then hold one channel, and on exit
`publish_child_exit` stamps `set_exited()` on it
(`crates/akuma-exec/src/process/children.rs:149`). sshd cannot distinguish "the
borrowed process exited" from "the login shell exited", so it closes the
session. Stdin delegation itself works — a container reading stdin gets what you
type (verified with `read X; echo GOT-$X`).

The fix is to make the borrow explicit: mark the target's channel as borrowed
when reattaching, skip the exit stamp on a borrowed channel, and clear the
caller's `delegate_pid` when the borrowee exits. That means touching the group
exit path, which has history — the `KTG-STALE-CH` comment a few lines up in
`mod.rs` is a war story about stamping exits onto the wrong channel — so it
wants its own change with its own testing rather than a drive-by.

## Verified

```
box pull busybox                                   → 4.1 MB layer store
box run --rm busybox /bin/busybox echo hello       → hello
box run --rm busybox …'echo NEW > /etc/newfile; rm /etc/group; ls /etc'
    → newfile present, group absent; image layer byte-identical;
      upper holds exactly newfile, the copied-up passwd, and .wh.group
box run --rm busybox …'mount -t proc proc /proc'   → permission denied
box run --rm busybox …'umount /tmp'                → Operation not permitted
box pull curlimages/curl
box run --rm --entrypoint curl curlimages-curl --version
    → curl 8.21.0 (aarch64-unknown-linux-musl) OpenSSL/3.5.7 …
box run --rm --entrypoint curl curlimages-curl -sS https://example.com
    → the page: DNS, TCP over smoltcp from a non-zero box, TLS with the
      image's own CA bundle
box pull alpine/curl                               → 3 layers, merged, same result
```
