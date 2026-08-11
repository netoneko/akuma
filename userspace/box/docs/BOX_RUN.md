# `box run` — containers from OCI images

How a `box run` container is assembled, and where it differs from `docker run`.

Procedure and troubleshooting: [`../../../docs/runbooks/run-docker-image.md`](../../../docs/runbooks/run-docker-image.md).
Kernel side: [`../../../docs/reference/subsystems/containers.md`](../../../docs/reference/subsystems/containers.md)
-> "OCI images and the overlay root".

## Startup sequence

```
box run [--rm] [-d] [-i] [--name X] [-w dir] [--entrypoint P] <image> [cmd …]
  │
  ├─ resolve image → /var/lib/box/images/<store>/{oci-config.json,layers}
  ├─ read `layers` (base-first) → reverse → overlay lowerdirs (topmost-first)
  ├─ verify every layer directory exists
  ├─ mkdir /var/lib/box/containers/<id>/upper
  ├─ inject upper/etc/resolv.conf (copied from the host) + upper/etc/hosts
  ├─ REGISTER_BOX(box_id, root=/var/lib/box/containers/<id>)
  │     → kernel creates the namespace with a SubdirFs jail at /
  ├─ MOUNT_IN_NS(box_id, "/", "overlay", "lowerdir=…,upperdir=…")
  │     → replaces that jail with the union. One-shot: only works while the
  │       root is still the pristine jail and the box has no processes.
  ├─ compose argv: Entrypoint + (command line ? command line : Cmd)
  ├─ resolve argv[0] against the container's PATH dirs if it has no slash
  ├─ SPAWN_EXT into the box, reattach, wait
  └─ --rm → KILL_BOX + delete the container directory
```

`box_id` is a hash of the container id, so a `--name`d container is stable
across runs and an unnamed one (`<image>-<uptime>`) is unique.

## Docker compatibility

| Behaviour | Status |
|---|---|
| `docker run image ARGS` replaces **Cmd**, keeps Entrypoint | ✅ |
| `--entrypoint` replaces the Entrypoint and drops Cmd | ✅ |
| `WorkingDir`, `-w` | ✅ |
| Image layers read-only and shared; per-container writable layer | ✅ |
| OCI whiteouts (`.wh.`, `.wh..wh..opq`) | ✅ |
| `/etc/resolv.conf`, `/etc/hosts` injected | ✅ |
| Image `Env` (notably `PATH`) | ❌ — `SpawnOptions` has no env field. `box` resolves a bare Entrypoint against the standard `PATH` directories itself as a stand-in |
| Script (shebang) Entrypoint | ❌ — `spawn_ext` does not honour `#!`; use `--entrypoint` |
| `-p` port mapping, `-v` volumes, `-e` env | ❌ |
| `USER`, cgroups, capabilities, seccomp | ❌ — isolation here is namespace + network-stack |
| Docker-in-docker | ❌ **by design** — see below |

## No nested OCI images

A container root is an overlay mount, and **a boxed process may not mount at
all** (`sys_mount` / `sys_umount2` are box-0-only, `MOUNT_IN_NS` likewise). So a
container cannot build a container.

This is deliberate. A mount table is a box's entire view of the filesystem;
anything a box can mount it can mount *over*, including its own `/proc`. Keeping
composition outside means a box's isolation is described by what its creator set
up rather than by whatever it did last. Nested **boxes** are unaffected — they
are process and network-stack grouping, and a box may still register a child box
rooted inside its own subtree.

Docker-in-docker can be revisited if something needs it; nothing does today.

## Layer sharing

Layers are content-addressed, so pulling two images that share a base extracts
that base once, and `box run` points both containers' overlays at the same
directory. Nothing reclaims a layer when the last image referencing it goes
away — there is no `box rmi` yet.

## Inspecting a container

Run without `--rm` and look at the writable layer:

```
# find /var/lib/box/containers/<id>/upper -type f
…/upper/etc/resolv.conf     injected at start
…/upper/etc/hosts           injected at start
…/upper/etc/passwd          copied up when the container appended to it
…/upper/etc/.wh.group       the container deleted /etc/group
```

That listing is the complete diff between the image and the container.
