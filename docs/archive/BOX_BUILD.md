# `box build` — Docker-compatible image builds on Akuma

**Date:** 2026-10-14
**Status:** Prerequisites 1–3 (§5) implemented 2026-10-14 — permissions ([`UNIX_PERMISSIONS.md`](UNIX_PERMISSIONS.md) §11), hard links ([`HARD_LINKS.md`](HARD_LINKS.md) §4), directory rename (§7 below). **Steps 4–12 implemented 2026-10-15** (§8 below); step 13 (`box push`) is not. Current-state reference: [`userspace/box/docs/BOX_BUILD.md`](../../userspace/box/docs/BOX_BUILD.md); procedure: [`../runbooks/build-docker-image.md`](../runbooks/build-docker-image.md).
**Depends on:** [`UNIX_PERMISSIONS.md`](UNIX_PERMISSIONS.md) (steps 3–6) and
[`HARD_LINKS.md`](HARD_LINKS.md). Built without them, every image would be
`root:root` with hard links duplicated, and `USER`/`COPY --chown` would be no-ops.

Goal: `box build -t name [-f Dockerfile] [--build-arg K=V] [--target S]
[--no-cache] <context>` reads the same Dockerfiles `docker build` does and
produces an image `box run` can run. The bar is that the output **loads into
real Docker** (`box save | docker load`) and runs there.

---

## 1. What exists

- **The overlay's upper directory is already an OCI layer diff.**
  `akuma-isolation/src/overlay_fs.rs` records deletions as `.wh.<name>` and
  opaque directories as `.wh..wh..opq` — the OCI image-layer spec's names. A
  file and its own whiteout may coexist in one upper, which the spec allows
  (whiteouts only hide *lower* layers).
- **The layer store is extracted directories keyed by digest**
  (`/var/lib/box/layers/sha256-…`), with images being only `oci-config.json` + a
  base-first `layers` list (`userspace/box/src/paths.rs`). A committed upper can
  be **renamed** into the store — the same format a pulled layer is extracted to,
  so no re-extraction.
- `miniz_oxide` (deflate both ways) is already a `userspace/tar` dependency;
  `sha2` (no_std) is already in the workspace (`akuma-ssh-crypto`).
- `box run` already assembles overlay roots, injects resolv.conf/hosts, spawns
  with image `Env`/`WorkingDir`/Entrypoint/Cmd, and cleans up (`run.rs`).

## 2. What is missing

| Gap | Where |
|---|---|
| No `commit`/`build`/`save` command | `userspace/box/src/main.rs:44-60` |
| `akuma-tar` extracts only — no archive writer | `userspace/tar/src/` |
| JSON is read-only (`walk`, `string_at`, …) | `userspace/box/src/json.rs` |
| resolv.conf/hosts are written **into the upper** — would land in every layer | `run.rs:79` |
| Renaming a lower-layer directory → `NotSupported` → **`EIO`**. Linux overlayfs returns `EXDEV`, which makes `mv` fall back to copy+delete; `EIO` just fails | `overlay_fs.rs:513`, `fs_error_to_errno` default arm (`akuma-syscalls-glue/src/fs.rs:155`) |
| Pull keeps no blobs (tarball deleted after extract) and verifies no digests | `oci.rs:205` |
| No `rmi`, `tag`, or layer GC | `main.rs` |

## 3. Constraint: the builder is a box-0 program

A box may not mount (`docs/reference/subsystems/containers.md`), so a container
cannot assemble another container's root. `box build` therefore runs on the
host — the role `buildkitd` plays for Docker — and each `RUN` is a short-lived
box it creates. Building from *inside* a box is out of scope by design, exactly
as Docker-in-Docker is today (`userspace/box/docs/BOX_RUN.md`).

---

## 4. Design

### 4.1 One primitive: commit an upper into a layer

```
commit(upper_dir, parent_config) -> (layer_digest, diff_id)
  walk upper in sorted order (deterministic), skipping runtime files (§4.2)
  for each entry emit a tar header:
      mode, uid, gid (from stat — real after UNIX_PERMISSIONS),
      mtime, symlink target, typeflag '1' for the 2nd+ name of an inode
      with nlink > 1 (HARD_LINKS), PAX records for long names / big ids
      whiteouts and opaque markers emitted as-is: they ARE the OCI encoding
  stream:  tar bytes ─┬─ sha256 → diff_id
                      └─ deflate ─┬─ sha256 → digest
                                  └─ /var/lib/box/blobs/sha256-<digest>
  rename upper_dir → /var/lib/box/layers/sha256-<digest>
```

One pass, bounded memory (stream in 64 KB chunks; never hold a layer). Keeping
the blob is what lets `save`/`push` produce exactly the digest the config names
without needing a bit-reproducible re-tar later.

Exposed directly as **`box commit <container> <image>`** — Docker has the same
command, and it is the smallest end-to-end slice: run, change, commit, run the
result.

### 4.2 Runtime files move out of the upper

Instead of writing `/etc/resolv.conf` and `/etc/hosts` into the upper, write them
into a per-container **runtime layer** directory and pass it as the *topmost
lowerdir*. The container sees them; the upper only gets them if a step actually
modifies them (copy-up). Fixes `box commit` for `box run` containers too, with no
exclusion list to maintain.

### 4.3 Config writer

A small JSON *writer* in `boxlib` (host-tested): start from the parent config,
set `config.{Env,Cmd,Entrypoint,WorkingDir,User,ExposedPorts,Labels,Volumes,
StopSignal}`, append to `rootfs.diff_ids`, append a `history` entry
(`empty_layer: true` for metadata-only steps), keep `architecture`/`os` from the
base. Emit a manifest alongside (`config` + `layers` with digests, sizes and
media types) so `save`/`push` have it.

Pulled base images must also keep their manifest and blobs for `save`/`push` to
re-export them (open question 1).

### 4.4 The builder loop

```
parse Dockerfile → stages
for each instruction in the target stage's dependency closure:
    key = sha256(parent chain, canonical instruction, used build args,
                 content hash of COPY/ADD sources incl. mode/owner)
    cache hit → reuse layer, continue
    FROM x [AS s]    local store first, else `box pull`
    RUN              box with lowers = chain (+ runtime layer), fresh upper,
                     SHELL (default /bin/sh -c) or exec form,
                     Env = config Env + ARGs, WorkingDir, User (spawn creds);
                     exit != 0 → fail, print the step and its output
                     → commit(upper)
    COPY/ADD         host writes into a fresh upper: .dockerignore applied,
                     --chown/--chmod, ADD auto-extracts local tars, ADD <url>
                     downloads (libakuma-tls) → commit(upper)
    COPY --from=s    read stage s's merged view (open question 2)
    WORKDIR          mkdir -p in a fresh upper if missing → commit; set config
    ENV ARG LABEL EXPOSE USER CMD ENTRYPOINT SHELL STOPSIGNAL VOLUME HEALTHCHECK
                     config only, history entry with empty_layer
write images/<tag>/{oci-config.json,layers,manifest.json}
```

Variable expansion (`$V`, `${V}`, `${V:-d}`, `${V:+d}`), line continuations,
`# syntax`/`# escape` directives, JSON vs shell forms — all pure, all in
`boxlib::dockerfile` with host tests.

Refused loudly (not silently ignored): `ONBUILD`, `RUN --mount`,
`RUN --network`, `RUN --security`, heredocs. Each is a later addition with a
clear error until then.

---

## 5. Order

**Prerequisites**
1. UNIX_PERMISSIONS steps 1–6 (creds, ownership, tar keeps owners, spawn creds).
2. HARD_LINKS (real `linkat`, `st_nlink`, tar restores links).
3. Overlay directory rename → `EXDEV` (new `FsError::CrossDevice`, mapped in
   `fs_error_to_errno`). One-line fix plus a host test; `mv` then works in RUN
   steps.

**Build**
4. Runtime layer for resolv.conf/hosts (§4.2).
5. `akuma-tar` writer (owners, hard links, symlinks, PAX, whiteouts) + host tests.
6. `boxlib` JSON writer + config/manifest builder + host tests.
7. **`box commit`** (§4.1). First end-to-end gate.
8. `boxlib::dockerfile` parser + expansion, host-tested against a corpus of real
   Dockerfiles (official `alpine`-based images).
9. **`box build`**, single stage: FROM/RUN/COPY/ENV/ARG/WORKDIR/USER/CMD/
   ENTRYPOINT/LABEL/EXPOSE + `.dockerignore`, no cache.
10. Build cache, `--no-cache`, `--build-arg`.
11. Multi-stage (`AS`, `COPY --from`, `--target`), `ADD`, remaining metadata
    instructions.
12. `box save` (OCI layout / `docker load` format), `box tag`, `box rmi` + GC.
    Pull starts keeping blobs and verifying digests.
13. Later, optional: `box push` (registry auth + blob upload; check the TLS
    client's single cipher suite against Docker Hub/GHCR first).

## 6. Verification

- **Host:** tar writer round-trips through `akuma-tar` extract (modes, owners,
  links, whiteouts, PAX); Dockerfile parser corpus; config writer output parsed
  back by `manifest.rs`/`spec.rs`.
- **Same Dockerfile, two builders.** Build on Akuma and with Docker on the Mac;
  dump each image's merged rootfs as `(path, type, mode, uid, gid, size,
  sha256, link target, ino-group)` and diff. Timestamps and digests will differ;
  the tree must not.
- **Interop:** `box save` on Akuma → `docker load` on the Mac → `docker run`.
  This is the definition of done for "same as Docker".
- **Self-hosting:** a Dockerfile that builds a Rust binary in a `rust:alpine`
  stage and copies it into an `alpine` stage, built on the ryzen metal.
- **Unverified today:** that `RUN` steps have outbound network (`apk add`). `box
  run` injects `resolv.conf` but this sketch did not check which stack a fresh box
  gets. Check first; it gates step 9.

## 7. Prerequisite 3: directory rename in containers (2026-10-14)

Sketched as "overlay directory rename → `EXDEV`". Doing it found three more
faults on the same path, two of them in ext2 itself:

| Fault | Effect | Fix |
|---|---|---|
| Overlay refused **every** directory rename with `NotSupported` → `EIO` — upper-only directories too | `mv dir newname` failed in every container, including for a directory the container had just made | A directory any lower layer has (or a target name one has) is `CrossDevice` → `EXDEV`, as Linux overlayfs without `redirect_dir`; `mv` falls back to copy-and-delete. Upper-only directories rename in place |
| Overlay `rmdir` of a directory whose upper copy held only **whiteouts** was `ENOTEMPTY` | `rm -rf` of any image directory failed at the directory, so `mv`'s fallback failed too (`can't remove '/etc/apk/keys'`) | Merged view empty ⇒ remaining upper entries are whiteouts; clear them, then remove |
| ext2 `rename` of a directory to **another parent** left `..` at the old parent and both parents' `st_nlink` stale | `cd ..` after `mv d other/` lands in the old parent; link counts disagree with `e2fsck` | Repoint `..`, move the link between the parents' counts |
| ext2 `rename` onto a **non-empty directory** "succeeded" by dropping the destination's entry | Everything under the destination orphaned — leaked inodes and blocks; dir↔file renames unchecked | `ENOTEMPTY`, `ENOTDIR`, `EISDIR`, `EINVAL` (into own subtree) as Linux; an empty destination directory is replaced via the `rmdir` teardown (`drop_empty_dir`, now shared) |

**Verification.** Host: ext2 +2, overlay +4. `renameprobe` (15 checks) is
identical on Linux (Docker arm64), Akuma ext2 and inside an Akuma container on
AArch64 QEMU, and on the amd64 trashcan; in a container `mv /etc/apk /etc/apk2`
and `rm -rf /usr/share/apk` (both lower-layer) now work. On the metal the same
kernel also passed `linkprobe` 23/23 and `permprobe` 50/50, and busybox `ln`
(x86_64 `link`, 86) makes a real link. Boot suites: amd64 QEMU 807/0, AArch64
Firecracker 299/1 (the pre-existing `thread_slot_reclaim_on_spawn`).

## 8. Implementation (2026-10-15)

Steps 4–12 of §5, built in order, each pure piece host-tested before the I/O
around it. What differs from the sketch above, and what building it found.

### Where things landed

| Step | Landed as |
|---|---|
| 4 runtime layer | `containers/<id>/runtime/` is the overlay's topmost lower (`run.rs`, `paths::container_runtime`); `containers/<id>/image` records the base for `box commit` |
| 5 tar writer | `akuma-tar::write` (ustar + PAX, hard links, 8 tests); the **extractor** gained PAX/GNU long-name handling (`format::parse_pax`, gzip path) so a layer `box commit` wrote unpacks with its real names |
| 6 config writer | `boxlib::imgconfig` (no JSON-writer module — a config is a struct that serialises itself); manifest, OCI `index.json` and Docker's `manifest.json` beside it |
| 7 `box commit` | `commit.rs`. `commit_upper` **consumes** the directory (rename into the layer store); `commit_copy` keeps it (unpacks the blob it just wrote) for a container that is still running |
| 8 parser | `boxlib::dockerfile` — 10 tests; a small corpus of official-image shapes (nginx, a Go multi-stage, a CRLF Node file) is in the tests; a bigger corpus was not built |
| 9–11 builder | `build.rs`: all instructions, cache, `--build-arg`, multi-stage, `--target`, `ADD`. `.dockerignore` is `boxlib::dockerignore` |
| 12 `save`/`tag`/`rmi`/GC | `imgcmd.rs`. `box pull` now keeps blobs and **verifies** layer and config digests, and writes `manifest.json` (open question 1: yes) |

Open question 2 (`COPY --from` needs a merged view) was answered the second
way: **resolve through the stack in userspace** (`boxlib::stack`, generic over a
`LayerFs`, 9 tests), because a mount from box 0 was unverified and a global
overlay mount would have meant new kernel surface for one builder. The
drift-risk named in the question is real; the interop gate is the check.

### What building it found

| Fault | Effect | Fix |
|---|---|---|
| `SubdirFs` — the **upper layer of every container** — did not forward `truncate`, `fallocate`, `set_times`, `append`, `create_socket_node`; the trait default is `NotSupported` → `EIO` | `ftruncate(2)` failed on every file in every `box run` container. `apk add curl` died with `ERROR: Failed to create usr/bin/curl: I/O error` — the first real `RUN` anyone wrote. Shrinking failed too, `touch -d` failed | forwarded in `subdir_fs.rs`; the overlay forwards `set_times`/`create_socket_node` (copy-up first). The `SubdirFs` confinement test had been checking `rec.last()` after calls the recorder did not record, so it passed for methods it never exercised — the recorder now records them |
| `images::path_exists` followed symlinks | an Alpine image's `CMD ["cat"]` (`/bin/cat -> /bin/busybox`, an absolute target) was "not found" from the host and `box run` failed to spawn `cat` | `lstat`, which is what the doc comment already said |
| The container's `resolv.conf` was a verbatim copy of the host's | On a systemd host (ryzen/Gentoo) it named `127.0.0.53`, a resolver that does not exist inside the container: `apk add` → `temporary error (try again later)`, `wget` → `bad address`. The QEMU devbox's file happened to list `8.8.8.8` | `spec::container_resolv_conf`: loopback nameservers dropped as Docker does, `8.8.8.8`/`1.1.1.1` when none remain (host-tested) |
| A `RUN` that pipes through `tail` hides a failed `apk` | the failed step is *cached as a success* — a rebuild reuses it | not a bug (Docker does the same); `--no-cache`, or `set -o pipefail` |
| Docker Hub's `busybox` is glibc | `wget http://example.com` inside it: `bad address` (static glibc resolver needs NSS); `nslookup` in the same box works. Initially read as "boxes have no DNS" | not a fault; documented. Alpine is musl and resolves |
| Under **OpenSSH** (Gentoo, port 4444) a container's output is not pumped — `box run alpine echo` prints nothing, exit status is right | `reattach` hands output to the caller's process channel, which only Akuma's own sshd (2222) reads | not changed; builds from a 4444 session still succeed but their `RUN` output is lost |

### Verified

| Where | What |
|---|---|
| AArch64 QEMU (HVF, devbox-smoltcp, `INSTANCE=3`) | single-stage and multi-stage builds; `COPY` with `.dockerignore`, `--chown`, `--from`; `USER`; cache (4/4 layer steps hit on rebuild); a failing `RUN` stops the build and tags nothing; `--target`; `box commit` of a running container incl. hard link, symlink, nested dirs; `tag`, `rmi` + GC; `box pull` digest verification; **`apk add jq curl` in a `RUN`** after the `SubdirFs` fix (network from a build box works) |
| `box save` | every blob's file name is its sha256, the manifest's sizes match, each layer's gunzipped tar hashes to its `diff_id`, hard links (`typeflag 1`) and modes are in the tar. `docker load` was not run that day; it was on 2026-10-15 afterwards — see below |
| ryzen (Gentoo box 0, entry 17) | x86_64 `box` builds clean. On the old kernel (021f1626): `FROM alpine` with `COPY --chown`, `USER`, cache — correct owners and modes. **On the fixed kernel** (this tree, `no-tests`, installed with `overlays/ryzen/gentoo/deploy.py 17`; the previous one is `akuma-amd64.pre-box-build` on the ESP): `RUN apk add jq curl` over wifi, `box run`, 4/4 cache hits, `box save` whose blobs, `diff_id`s and `architecture: amd64` check out. Long builds must be detached (`nohup … &`): the wifi ssh session dropped mid-build once and took the foreground build with it |

### `docker load` and the trashcan (2026-10-15, later)

`box save | docker load | docker run` works, on AArch64 (QEMU) and on `amd64`
(the trashcan, new kernel from this tree, `box` pushed with `hpbox.akuma_push`).
Two bugs found, both fixed in `userspace/box`: the containerd image name was not
`docker.io/library/…` (so the image loaded but could not be addressed by name),
and `fsx::now_secs` called a private syscall amd64 lacks (`created` in year
586524, which Docker refuses to parse). Neither shows on the arch that has the
syscall, which is why the AArch64 run alone would have missed the second.

**uid -4 (fixed 2026-10-16, kernel).** On the trashcan, a `box run` child's
`/proc/<pid>/status` said `Uid: 4294967292` in about five spawns in six, so every
file it created — and every `RUN` layer — was owned by uid 4294967292.

Root cause, found by logging credential writes, the prologue's `EINTR` return and
every `interrupt_thread` caller: `box` calls `reattach(pid)` right after
`spawn_ext`, and `sys_reattach` sends the child **`SIGWINCH`**. On amd64
`pend_signal_to_group` raises the per-thread interrupt bit for every signal, and
glue's syscall prologue turns that bit into `EINTR` for the thread's *next
syscall of any kind* — the child's first, `getuid`. busybox's `check_suid` ran
for a SUID-drop applet, took the `-EINTR` for the real uid, and called
`setgid(0)` then `setuid(4294967292)`. The kernel was behaving as written; the
overflow is `-4` stored in a `u32` uid on the way through a program that did not
check the return of `getuid()`.

Wrongly ruled out first: the binary's owner, explicit credentials from `box`
(`set_creds(0,0)` changed nothing), and a stale slot (`x86_claim_slot` does scrub
the bit — the interrupt arrives *after* the claim).

Fix: `process::signal::signal_can_act` — an ignored signal, or one with default
action "ignore" (`SIGCHLD`, `SIGURG`, `SIGWINCH`) and no handler, is still pended
and the thread woken, but no longer raises the interrupt bit. A handler or a
fatal default still interrupts; `SIGKILL`/`SIGSTOP` always do. Verified on the
trashcan: 12 of 12 runs `uid=0`, files `0:0`. Not fixed: a *real* signal with a
handler still makes a non-blocking `getuid` return `EINTR`; Linux restarts or
completes such calls, and the prologue could exempt the non-blocking leaf tier.

### Not done

`box push`; `.tar.bz2`/`.xz` in `ADD`; preserving source mtimes in `COPY`
(libakuma has no `utimensat` wrapper); a build with more than 31 image layers;
`--squash`; the Akuma-vs-Docker rootfs diff of §6 as a script
(the procedure is in the runbook, it was not automated).

## Open questions

*(1 and 2 answered in §8.)*

1. **Keep blobs for pulled images?** Doubles their disk cost but makes
   `save`/`push` of anything `FROM` a pulled image work without re-downloading.
   Recommendation: yes, and GC them with the layer.
2. **`COPY --from` reads a merged multi-layer view.** Either box 0 mounts the
   stage's overlay somewhere temporary (needs `MOUNT_IN_NS` for box 0 or a
   global overlay mount — unverified that the kernel allows it), or `boxlib`
   resolves paths through the layer stack itself (duplicates `overlay_fs`
   semantics; drift risk). Recommendation: the mount, if the kernel supports it.
3. **Cache location and eviction** — `/var/lib/box/cache/<key>` → digest, swept
   by the same GC as `rmi`.

## Background

- `userspace/box/docs/BOX_RUN.md`, `OCI_IMAGE_PULL.md` — current run/pull flows
- `docs/runbooks/run-docker-image.md`
- `docs/reference/subsystems/containers.md` — why a box cannot mount
- `crates/akuma-isolation/src/overlay_fs.rs` — whiteout encoding
