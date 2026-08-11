# Running Docker images on Akuma — `box run`, overlays, and three bugs underneath

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

(A third — every `reattach` closing the SSH session — surfaced later and has its
own section below.)

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

## The session-closing bug (fixed)

Any SSH session closed — exactly as if you had typed `exit` — as soon as a
`box run`, `box open` or `box use` child finished. Reported twice during this
work, once as "interactive busybox exits my shell" and once for a plain
`box run --entrypoint curl … https://example.com`.

Isolated by elimination: a plain command, `box images` and `box run -d` all left
the session alive; **everything that called `reattach` killed it**, `box open`
included, so it predated `box run`.

The chain:

1. `reattach_process_ext` points the target's `p.channel` at the **caller's**
   channel, so the container's output appears on the caller's terminal. That
   part is intended.
2. `spawn.rs` registered the per-tid channel — the process's identity for exit
   notification — by *re-reading `p.channel`* when the spawned thread first ran.
3. `box run` calls `reattach` immediately after `spawn_ext` returns, which
   normally beats the new thread to that line. So the container registered the
   **shell's** channel as its own.
4. `sys_exit` stamps `get_channel(tid)` (`src/syscall/proc.rs:412`) — i.e. the
   shell's channel.
5. sshd ends a session when `waitpid_status(shell_pid)` reports the shell's
   channel exited (`userspace/sshd/src/protocol.rs:299`). It duly did.

The fix is one line of scope: capture the channel the spawn created and register
*that*, instead of re-reading a field that a concurrent `reattach` may already
have retargeted. `p.channel` is borrowable I/O; the per-tid registration is
identity. The fork path has always kept those separate on purpose — see the
`exit_channel` comment in `process/mod.rs` — and spawn now does too.

Stdin delegation was never broken: a container reading stdin gets what you type
(verified with `read X; echo GOT-$X`), and interactive `sh` worked throughout;
it was only the teardown that took the session with it.

## `/proc` in containers

An OCI image ships an empty `/proc` and expects something mounted there — without
it `ps` fails and `ls /` complains about the entry. `box run` now mounts a procfs
into the box's namespace before starting the container. Mounting is host-only, so
it has to happen from `box` (box 0), which is exactly the shape the mount policy
above intends. `ps` inside a container sees only that container's processes.

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

# after the session-closing fix, from an SSH login shell (real pty):
box run --rm --entrypoint curl curlimages-curl -sS https://example.com
echo SHELL-SURVIVED-CURL                           → SHELL-SURVIVED-CURL
box run --rm -i busybox sh
  / # ps        → only the container's own processes
  / # exit      → back at the host prompt, session still open
```
