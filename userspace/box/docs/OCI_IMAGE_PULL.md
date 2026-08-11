# OCI Image Pull

`box pull` downloads OCI container images from Docker Hub (and other registries) and stores them locally for use with `box run`.

## Usage

```
box pull busybox
box pull ubuntu:22.04
box pull ghcr.io/owner/repo:tag
box run --rm busybox /bin/busybox echo hi
box images
```

## Architecture

The pull pipeline is entirely in userspace. No kernel changes are needed.

```
box pull <image>
  │
  ├─ Parse image reference (registry, name, tag)
  ├─ Fetch Bearer token from auth.docker.io
  ├─ Fetch manifest (handles manifest lists → arm64 resolution)
  ├─ Fetch OCI config JSON
  ├─ For each layer, skipping any digest already in the layer store:
  │    ├─ Download blob via registry API (follows 307 redirects to CDN)
  │    ├─ Extract into /var/lib/box/layers/<digest>.tmp (akuma_tar, linked in)
  │    └─ rename → /var/lib/box/layers/<digest>/
  ├─ Save config to /var/lib/box/images/<name>/oci-config.json
  └─ Save digest list to /var/lib/box/images/<name>/layers (base-first)
```

### Components

| Component | Location | Role |
|-----------|----------|------|
| Image ref parser | `oci.rs` | Deconstructs `registry/name:tag`, defaults Docker Hub + `library/` prefix |
| JSON parser | `json.rs` | Minimal hand-rolled JSON extraction (no serde, no_std compatible) |
| Image store | `images.rs` | Manages `/var/lib/box/images/` layout, config persistence |
| TLS + HTTP | `libakuma-tls` | HTTPS client with redirect following (`download_file_with_headers`) |
| Tar extraction | `akuma_tar` (`userspace/tar`) | Gzip decompression + tar unpacking, **linked in, not spawned**: `/bin/tar` was silently a busybox applet whose hardlinks go through `link()`, which akuma implements as a full file copy — one 1.9 MB layer became 467 MB with its mode bits lost ([`../../../docs/archive/BOX_DOCKER_COMPAT.md`](../../../docs/archive/BOX_DOCKER_COMPAT.md)) |

### Image store layout

Layers are content-addressed and shared; an image directory holds only metadata.

```
/var/lib/box/layers/
  └── sha256-025fe19496…/    # one extracted layer, read-only, shared by every
      ├── bin/               # image that references this digest
      ├── etc/
      └── ...
/var/lib/box/images/
  └── busybox/
      ├── oci-config.json    # OCI image config (entrypoint, cmd, env, etc.)
      └── layers             # digest per line, base-first
/var/lib/box/containers/
  └── <id>/upper/            # one container's writable layer
```

Base directories are created automatically on first use. Extraction stages in
`<digest>.tmp` and renames into place, so an interrupted pull cannot leave a
half-populated directory that the next pull would accept as complete. Whiteout
entries (`.wh.*`) are left on disk as files — they belong to the layer, and the
overlay interprets them at lookup time.

### OCI protocol flow

1. **Auth**: `GET https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/busybox:pull` → Bearer token
2. **Manifest**: `GET https://registry-1.docker.io/v2/library/busybox/manifests/latest` with `Accept: application/vnd.docker.distribution.manifest.list.v2+json, ...` → manifest list (image index)
3. **Platform resolution**: Find `linux/arm64` entry in manifest list → digest
4. **Platform manifest**: `GET .../manifests/<digest>` → config digest + layer digests
5. **Config**: `GET .../blobs/<config-digest>` (follows 307 redirect) → OCI config JSON
6. **Layers**: `GET .../blobs/<layer-digest>` (follows 307 redirect to CDN) → gzipped tar, streamed to disk

### Running a pulled image

`box run` composes the image's layers into an overlay root — see
[`BOX_RUN.md`](BOX_RUN.md). Entrypoint/Cmd/WorkingDir come from the OCI config.

`box open --image` predates this and still points a box root directly at an
image's flat `rootfs/` directory, which `box pull` **no longer creates**.

## Limitations

- Single-platform only (arm64/aarch64)
- No image deletion or layer GC (`box rmi` / prune) — nothing reclaims a layer
  when the last image referencing it is gone
- No digest pinning (always pulls by tag)
- Registry auth is Docker Hub only (anonymous pull with token exchange)
- No layer digest verification: a blob is trusted to be what its digest says.
  Path traversal *is* checked — entries that would escape the layer directory
  are refused and the pull fails
