# box

Container management utility for Akuma OS. Creates isolated execution
environments ("boxes") with root directory redirection, process scoping, and
OCI image backing.

## Quick start

```
box pull busybox                                 # pull an image from Docker Hub
box run --rm busybox /bin/busybox echo hi        # run a container
box run --rm -i busybox sh                       # interactive shell in a container
box run --rm --entrypoint curl curlimages-curl -sS https://example.com
box ps                                           # list active boxes
box close demo                                   # stop a box
```

## Commands

| Command | Description |
|---------|-------------|
| `box run [opts] <image> [cmd [args...]]` | Run a container from a pulled image |
| `box pull <image>` | Pull an OCI image (Docker Hub, ghcr.io, etc.) |
| `box images` | List locally stored images |
| `box open <name> [opts] [cmd] [args...]` | Create a plain box and run a command in it |
| `box ps` | List active boxes |
| `box use <name\|id> [opts] <cmd> [args...]` | Run a command inside an existing box |
| `box grab <name\|id> [pid]` | Reattach terminal to a process in a box |
| `box cp <src> <dest>` | Copy a directory tree |
| `box close <name\|id>` | Stop a box and kill its processes |
| `box show <name\|id>` | Display box details and member processes |
| `box test [--net]` | Run built-in test suite |

### `box run` options

- `--rm` — kill the box and delete the container directory on exit
- `-d` / `--detached` — start in background, print the pid
- `-i` / `--interactive` — keep stdin attached (output is always attached in the foreground)
- `--name <id>` — container id (default `<image>-<uptime>`)
- `--entrypoint <path>` — override the image's Entrypoint
- `-w <dir>` / `--workdir <dir>` — override `WorkingDir`

Arguments after the image replace the image's **Cmd** and are passed to its
Entrypoint, as `docker run` does.

### `box open` options

- `--root <dir>` / `-r <dir>` — root directory for the box (default `/`)
- `--image <name>` — legacy: use an image's flat `rootfs/` as the box root.
  **`box pull` no longer creates that directory** — use `box run`.
- `-I` / `--interactive`, `-d` / `--detached`

## Containers: images, layers, overlay

`box pull` implements the OCI Distribution Spec — image references
(`busybox`, `ubuntu:22.04`, `ghcr.io/owner/repo:tag`), multi-arch manifest lists
(selects `linux/arm64`), 307 redirects to CDN blobs.

Each layer is extracted **once**, into a content-addressed directory shared by
every image that references it:

```
/var/lib/box/layers/sha256-<hex>/          extracted layer, read-only, shared
/var/lib/box/images/<name>/oci-config.json OCI config (Entrypoint, Cmd, …)
/var/lib/box/images/<name>/layers          digest per line, base-first
/var/lib/box/containers/<id>/upper         one container's writable layer
```

`box run` registers a box rooted at the container directory, then asks the
kernel to mount an **overlay** as that box's `/`: the image's layers as
read-only lowers, the container's `upper` on top. Writes copy up into `upper`;
deletes that cannot touch a read-only layer are recorded as `.wh.` whiteouts.
The image is never modified, and two containers of the same image share one copy
of it on disk.

`/etc/resolv.conf` and `/etc/hosts` are injected into `upper` at start — no OCI
image ships them and every networked program expects them.

### Limits

- **No nested OCI images.** Composing a container root needs an overlay mount,
  and a boxed process may not mount at all. Nested *boxes* still work.
- The image's `Env` is not passed through yet, so `PATH` inside a container is
  the kernel default; a bare Entrypoint is resolved against the standard `PATH`
  directories by `box` itself.
- A script Entrypoint needs `--entrypoint <binary>`: `spawn_ext` does not honour
  shebangs (only `execve` does).
- No layer GC (`box rmi`), no digest pinning.

## Testing

```
box test           # offline: JSON parser, OCI ref parser, HTTP header parsing
box test --net     # + network: downloads busybox manifest and layer from Docker Hub
```

See [docs/TESTING.md](docs/TESTING.md). The overlay itself is host-tested in
`crates/akuma-isolation` (`overlay_fs.rs`), and tar header parsing in
`userspace/tar` (`format.rs`).

## Source layout

```
src/
  main.rs      Command dispatch, box lifecycle (open/close/ps/use/grab/cp)
  run.rs       `box run`: container creation, overlay mount, entrypoint composition
  oci.rs       OCI Distribution client (auth, manifests, blob download, layer store)
  json.rs      Minimal no_std JSON parser
  images.rs    Local image/layer/container store layout
  tests.rs     Built-in test suite
docs/
  OCI_IMAGE_PULL.md   Pull pipeline architecture
  BOX_RUN.md          Containers, the overlay root, docker compatibility
  TESTING.md          Test suite reference and bug notes
```

## Dependencies

- `libakuma` — syscall wrappers, process spawning, filesystem ops
- `libakuma-tls` — HTTPS client (embedded-tls, TLS 1.3, AES-128-GCM)
- `akuma-tar` (`userspace/tar`) — layer extraction, **linked in, not spawned**.
  It used to run `/bin/tar`, which was silently a busybox applet whose hardlink
  handling expanded a 1.9 MB layer into 467 MB of mode-less copies:
  [`../../docs/archive/BOX_DOCKER_COMPAT.md`](../../docs/archive/BOX_DOCKER_COMPAT.md)

## Kernel integration

- **box_id** — per-box identifier tracked in each process's PCB
- **root_dir** — VFS path scoping via `SubdirFs`
- **ProcFS virtualization** — `/proc/boxes`; a boxed process sees only its box
- **REGISTER_BOX** (316) / **KILL_BOX** (317) — box lifecycle
- **SPAWN_EXT** (315) — spawn into a box
- **MOUNT_IN_NS** (325) — compose a box's mount namespace *from box 0*, including
  the `overlay` fstype that gives a container its root. A box's namespace is
  built entirely from outside, before it runs; boxed processes cannot mount.

The `box` binary is a userspace orchestrator; the kernel enforces the isolation
boundaries. See
[`../../docs/reference/subsystems/containers.md`](../../docs/reference/subsystems/containers.md)
and [`../../docs/runbooks/run-docker-image.md`](../../docs/runbooks/run-docker-image.md).
