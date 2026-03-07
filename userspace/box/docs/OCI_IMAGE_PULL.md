# OCI Image Pull

`box pull` downloads OCI container images from Docker Hub (and other registries) and stores them locally for use with `box open --image`.

## Usage

```
box pull busybox
box pull ubuntu:22.04
box pull ghcr.io/owner/repo:tag
box open mybox --image busybox -i /bin/sh
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
  ├─ For each layer:
  │    ├─ Download blob via registry API (follows 307 redirects to CDN)
  │    └─ Extract with /bin/tar -xzvf -C <rootfs>
  └─ Save config to /var/lib/box/images/<name>/oci-config.json
```

### Components

| Component | Location | Role |
|-----------|----------|------|
| Image ref parser | `oci.rs` | Deconstructs `registry/name:tag`, defaults Docker Hub + `library/` prefix |
| JSON parser | `json.rs` | Minimal hand-rolled JSON extraction (no serde, no_std compatible) |
| Image store | `images.rs` | Manages `/var/lib/box/images/` layout, config persistence |
| TLS + HTTP | `libakuma-tls` | HTTPS client with redirect following (`download_file_with_headers`) |
| Tar extraction | `/bin/tar` | Gzip decompression (strips gzip header, uses miniz_oxide) + tar unpacking |

### Image store layout

```
/var/lib/box/images/
  └── busybox/
      ├── oci-config.json    # OCI image config (entrypoint, cmd, env, etc.)
      └── rootfs/            # Extracted filesystem layers
          ├── bin/
          ├── etc/
          └── ...
```

The base directory is created automatically on first use.

### OCI protocol flow

1. **Auth**: `GET https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/busybox:pull` → Bearer token
2. **Manifest**: `GET https://registry-1.docker.io/v2/library/busybox/manifests/latest` with `Accept: application/vnd.docker.distribution.manifest.list.v2+json, ...` → manifest list (image index)
3. **Platform resolution**: Find `linux/arm64` entry in manifest list → digest
4. **Platform manifest**: `GET .../manifests/<digest>` → config digest + layer digests
5. **Config**: `GET .../blobs/<config-digest>` (follows 307 redirect) → OCI config JSON
6. **Layers**: `GET .../blobs/<layer-digest>` (follows 307 redirect to CDN) → gzipped tar, streamed to disk

### `box open --image`

When opening a box with `--image`, the entrypoint/cmd are extracted from the OCI config's `config.Entrypoint` and `config.Cmd` fields. `WorkingDir` is also respected.

## Limitations

- Single-platform only (arm64/aarch64)
- No layer caching or deduplication — each `box pull` re-downloads everything
- No image deletion command yet
- No digest pinning (always pulls by tag)
- Registry auth is Docker Hub only (anonymous pull with token exchange)
