# box

Container management utility for Akuma OS. Creates isolated execution environments ("boxes") with root directory redirection, process scoping, and optional OCI image backing.

## Quick start

```
box open mybox --root /srv/myroot -i /bin/sh    # interactive shell in isolated root
box pull busybox                                 # pull OCI image from Docker Hub
box open demo --image busybox -i /bin/sh         # run shell inside busybox rootfs
box ps                                           # list active boxes
box close demo                                   # stop a box
```

## Commands

| Command | Description |
|---------|-------------|
| `box open <name> [opts] [cmd] [args...]` | Create a box and run a command inside it |
| `box pull <image>` | Pull an OCI image (Docker Hub, ghcr.io, etc.) |
| `box images` | List locally stored images |
| `box ps` | List active boxes |
| `box use <name\|id> [opts] <cmd> [args...]` | Run a command inside an existing box |
| `box grab <name\|id> [pid]` | Reattach terminal to a process in a box |
| `box cp <src> <dest>` | Copy a directory tree |
| `box close <name\|id>` | Stop a box and kill its processes |
| `box show <name\|id>` | Display box details and member processes |
| `box test [--net]` | Run built-in test suite |

### `box open` options

- `--root <dir>` / `-r <dir>` — root directory for the box (default `/`)
- `--image <name>` — use a pulled OCI image as the root filesystem
- `-i` / `--interactive` — reattach terminal to the spawned process
- `-d` / `--detached` — start in background

When `--image` is used without an explicit command, the entrypoint and cmd from the OCI config are used automatically.

## OCI image support

`box pull` implements the OCI Distribution Spec to download container images:

- Parses image references (`busybox`, `ubuntu:22.04`, `ghcr.io/owner/repo:tag`)
- Handles multi-arch manifest lists (selects `linux/arm64`)
- Follows 307 redirects to CDN for blob downloads
- Extracts gzipped tar layers into a local rootfs

Images are stored under `/var/lib/box/images/<name>/` with an `oci-config.json` and a `rootfs/` directory. See [docs/OCI_IMAGE_PULL.md](docs/OCI_IMAGE_PULL.md) for protocol details.

## Testing

```
box test           # offline: JSON parser, OCI ref parser, HTTP header parsing
box test --net     # + network: downloads busybox manifest and layer from Docker Hub
```

20 tests covering the JSON parser, image reference parser, HTTP header parsing, and end-to-end download validation. See [docs/TESTING.md](docs/TESTING.md).

## Source layout

```
src/
  main.rs      Command dispatch, box lifecycle (open/close/ps/use/grab/cp)
  oci.rs       OCI Distribution client (auth, manifests, blob download)
  json.rs      Minimal no_std JSON parser
  images.rs    Local image store management
  tests.rs     Built-in test suite
docs/
  OCI_IMAGE_PULL.md   Pull pipeline architecture
  TESTING.md          Test suite reference and bug notes
```

## Dependencies

- `libakuma` — syscall wrappers, process spawning, filesystem ops
- `libakuma-tls` — HTTPS client (embedded-tls, TLS 1.3, AES-128-GCM)
- `/bin/tar` — layer extraction (gzip decompression + tar unpacking)

## Kernel integration

Boxes use kernel-side isolation primitives:

- **box_id** — per-box identifier tracked in each process's PCB
- **root_dir** — VFS path scoping (processes see only their box's filesystem subtree)
- **ProcFS virtualization** — `/proc/boxes` lists active boxes; processes in a box see only their own box's processes
- **SYSCALL_REGISTER_BOX** (316) — register a new box with the kernel
- **SYSCALL_KILL_BOX** (317) — terminate all processes in a box

The `box` binary is a userspace orchestrator; the kernel enforces the isolation boundaries.
