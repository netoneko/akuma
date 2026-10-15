# `box build` — Docker-compatible image builds

How `box build`, `box commit`, `box save`, `box tag`, `box rmi`, `box prune`,
`box push`, `box login` and `box logout` work, and where they differ from Docker. Procedure:
[`../../../docs/runbooks/build-docker-image.md`](../../../docs/runbooks/build-docker-image.md).
Design history and the reasoning behind each choice:
[`../../../docs/archive/BOX_BUILD.md`](../../../docs/archive/BOX_BUILD.md).

```
box build -t name[:tag] [-t …] [-f Dockerfile] [--build-arg K[=V]]… [--target stage] [--no-cache] <context>
box commit <container> <image>
box save [-o file.tar] <image>          # OCI layout + Docker's manifest.json; `docker load` reads it
box tag <source> <target>
box rmi <image>…                        # drops the names, then collects unreferenced layers
box prune                               # the same collection, on its own
box push [--insecure] <registry/name:tag>
box login -u <user> (-p <password> | --password-stdin) <registry>
box logout <registry>
```

## What a build does

The builder runs in **box 0**, the role `buildkitd` plays for Docker: a box may
not mount, so a container cannot assemble another container's root.

```
parse Dockerfile -> stages          (boxlib::dockerfile)
for each stage in the target's dependency closure, in file order:
    FROM          local store first, else `box pull`; `scratch` = empty;
                  an earlier stage's alias = that stage's final state
    RUN           a short-lived box: overlay of [runtime layer, image layers],
                  fresh upper, SHELL or exec form, image Env + ARGs, WORKDIR,
                  USER (spawn credentials). Non-zero exit fails the build.
                  The upper is then COMMITTED as the next layer.
    COPY / ADD    the host writes into a fresh upper (no box involved)
    WORKDIR       creates the directory in a layer only if the merged view lacks it
    everything else  edits the image config only (history entry `empty_layer`)
write images/<tag>/{oci-config.json, layers, manifest.json}   (config last)
```

### One primitive: commit an upper into a layer (`commit.rs`)

The overlay's upper directory **is** an OCI layer diff — deletions are
`.wh.<name>` files, opaque directories carry `.wh..wh..opq` — so it is emitted
as the ordinary files it is. One pass, bounded memory:

```
walk upper in sorted order -> tar headers (mode, uid, gid, mtime, symlink target,
        typeflag '1' for the 2nd+ name of an inode with nlink > 1, PAX when a
        field does not fit)
tar bytes --+-- sha256 ----------------------> diff_id
            +-- deflate -- crc32, sha256 ----> digest   -> /var/lib/box/blobs/sha256-<hex>
rename upper -> /var/lib/box/layers/sha256-<hex>          (or delete it if that layer exists)
```

The gzip header carries no timestamp, so the same tar always compresses to the
same bytes: an unchanged step re-commits to the same digest. The compressed blob
is **kept** — `box save` must export exactly the digest the manifest names, and a
bit-reproducible re-tar later is not something to rely on.

### Runtime files are a layer of their own

`/etc/resolv.conf` and `/etc/hosts` are written to
`containers/<id>/runtime/` and passed as the overlay's **topmost lower**, never
into the upper. The container sees them; a committed layer contains them only if
a process in the container rewrote them (which copies them up). This applies to
`box run` too, so `box commit` of a `run` container is clean.

### Cache

`/var/lib/box/cache/<key>` holds `digest diff_id size` for a layer-producing step.

| Step | Key covers |
|---|---|
| `RUN` | parent layer digest, argv, the full environment, WORKDIR, USER |
| `COPY`/`ADD` (local) | parent layer digest, destination, mode, `--chown`/`--chmod`, and the **content hash of every source file** |
| `WORKDIR` | parent layer digest, path |

`ADD <url>` is never cached (nothing says the URL's content is unchanged).
`--no-cache` skips lookups but still writes. A hit prints ` ---> Using cache`.
A step's *metadata* effect (ENV, CMD, …) is applied every time — only the layer
is cached.

### `COPY --from` reads a merged view without mounting

Box 0 has no overlay mount of its own, so `boxlib::stack` resolves paths through
a stage's layer list with the OCI rules the kernel's `overlay_fs` applies: a
`.wh.<name>` in layer L hides `<name>` in every layer *below* L (not in L), and
`.wh..wh..opq` hides a directory's contents below. It is generic over a
`LayerFs` trait and host-tested against an in-memory tree. This is a second
implementation of overlay semantics; the tests pin the cases that matter, and
`box save | docker load` is the check that would catch drift.

New directories a `COPY` creates inside a directory the lower layers already
have **inherit that directory's mode and owner** — an upper directory's
metadata wins in an overlay, so a naive `mkdir /tmp` in a `COPY` layer would turn
`1777` into `0755`.

## Docker compatibility

| Feature | Status |
|---|---|
| `FROM [--platform] image [AS n]`, `scratch`, stage aliases, global `ARG` in `FROM` | ✅ |
| `RUN` shell and exec form, `SHELL`, `ENV`/`ARG` expansion (`$V ${V} ${V:-d} ${V:+d} ${V-d} ${V+d}`), quoting | ✅ |
| `COPY`/`ADD` with globs, `.dockerignore`, `--chown` (names resolve against the image's own passwd/group), `--chmod`, `--from=<stage\|image>` | ✅ |
| `ADD <local .tar/.tar.gz/.tgz>` auto-extract, `ADD <http(s) url>` | ✅ (`.tar.bz2`/`.xz` not unpacked; no checksum flag) |
| `WORKDIR`, `USER`, `CMD`, `ENTRYPOINT` (clears an inherited `CMD`), `LABEL`, `EXPOSE`, `VOLUME`, `STOPSIGNAL`, `HEALTHCHECK`, `MAINTAINER` | ✅ |
| Multi-stage, `--target`, `--build-arg` (unused ones warn), `--no-cache` | ✅ |
| `# escape=` directive, line continuations, comments inside continuations | ✅ |
| `ONBUILD`, `RUN --mount`/`--network`/`--security`, heredocs, `COPY --link`'s semantics | ❌ **refused with an error naming the line** (`--link` is accepted and ignored) |
| BuildKit frontends (`# syntax=` other than the default), secrets, SSH forwarding, cache mounts | ❌ |
| `box push` | ✅ — see [`box push`](#box-push) |

Other divergences:

- **At most 31 image layers.** The kernel's overlay takes 32 lower directories
  and one is the runtime layer. A `FROM` image with 20 layers leaves 11 steps.
  `box build` says so rather than failing in the mount. Docker's limit is ~125.
- `COPY` does not preserve source **mtimes** (libakuma has no `utimensat`
  wrapper yet); layers carry the build time.
- The config `Env` ordering, `history[].created_by` text and image id differ
  from BuildKit's. Compare the **merged rootfs** (path, type, mode, uid, gid,
  size, hash, link target), not digests — see the runbook.
- Config fields a base image carries that `box` does not model
  (`container_config`, `Memory`, …) are dropped on rewrite.
- Pulled images keep their blobs from 2026-10-15 and verify them (layer and
  config sha256) as they arrive. An image pulled **before** that has no blob:
  `box save` of it says "re-pull". `box build` from it works.
- `rmi`/`prune` collect every layer no image lists — including the **build
  cache's intermediates**, which are unreferenced by design. Do not run them
  while a build is in flight.

## `box save`

An OCI image layout in a plain tar: `oci-layout`, `index.json` (with the
`io.containerd.image.name` annotation Docker reads), `blobs/sha256/<hex>` for the
config, the manifest and every layer, **plus** Docker's legacy `manifest.json`.
Digests in it are verified by construction: the config digest is the sha256 of
the bytes stored, and layers are the blobs the manifest names. Writes to stdout
without `-o`, so `box save app | ssh mac docker load` works over a pipe.

## `box push`

`box push [--insecure] registry/name:tag` uploads an image the store already holds
under that name (`box tag app:1 ghcr.io/me/app:1` first — a bare `app:1` means
`docker.io/library/app`, as in Docker). It sends exactly what `box save` writes:
the same config bytes and the same manifest, so the digest it prints is the one
`docker pull` reports.

```
GET /v2/                      200 = open; 401 = read WWW-Authenticate
  Basic  -> Authorization: Basic <stored credentials>
  Bearer -> GET <realm>?service=…&scope=repository:<name>:pull,push   (Basic creds, if any)
            -> Authorization: Bearer <token>
for the config and each layer:
  HEAD /v2/<name>/blobs/<digest>        200 = skip ("Layer already exists")
  POST /v2/<name>/blobs/uploads/        202 + Location
  PUT  <Location>[&]digest=<digest>     201   (monolithic: one request, streamed from the blob file)
PUT /v2/<name>/manifests/<tag>          201   Content-Type: application/vnd.oci.image.manifest.v1+json
```

- **Credentials**, first found: `BOX_REGISTRY_USER` + `BOX_REGISTRY_PASSWORD`;
  `/var/lib/box/auth.json` (written by `box login`, mode `0600`); Docker's
  `/root/.docker/config.json` `auths` (read only; credential helpers are not
  supported). `auth.json` has Docker's `{"auths":{"<registry>":{"auth":"<base64 user:password>"}}}`
  shape. A password may contain `:`; a user name may not.
- **A 401 mid-upload re-authenticates once and retries** (a bearer token lasts
  minutes; a layer can take longer). `Authorization` is only sent to the
  registry's own host, never to a different host an upload `Location` names.
- **`--insecure`** speaks plain HTTP to that registry (also on `box pull`). Docker
  Hub ignores it. Docker applies this to `localhost` on its own; `box` never
  guesses.
- **Certificates are not verified.** `libakuma-tls` has no trust store, so an
  `https://` push is safe from passive listeners and from nothing else:
  credentials and layers can be read or replaced by anyone on the path. `box
  push` prints a note every time. Fix that before pushing anything that matters.
- **Not done:** chunked/resumable upload (`PATCH`), cross-repository blob
  `mount`, pushing an index (multi-arch), credential helpers, `box push` of an
  image pulled before 2026-10-15 (no stored blobs), authenticated **pull**
  (private base images), `docker.io` token flow verified live (the Bearer flow
  is tested against a mock; Hub and GHCR were not pushed to).

## Source map

| File | Role |
|---|---|
| `boxlib::dockerfile` | parse, expansion, `ENV`/`ARG` grammars, stage closure |
| `boxlib::dockerignore` | `.dockerignore` matching (last match wins, `**`, `!`) |
| `boxlib::copyplan` | destination/source arithmetic, `--chmod`, ADD kinds |
| `boxlib::buildopts` | `box build` argv, durations, `HEALTHCHECK`, ports |
| `boxlib::imgconfig` | config parse/write, manifest, index, RFC 3339 |
| `boxlib::layer` | streaming tar → gzip with both digests |
| `boxlib::stack` | merged view over layers (whiteouts, opaque dirs) |
| `boxlib::registry` | push/login decisions: base64, credential file, `WWW-Authenticate`, token and upload URLs |
| `libakuma_tls::http_request` | any HTTP method, response headers, file-streamed body |
| `akuma-tar::write` | tar header encoder (ustar + PAX) |
| `build.rs`, `commit.rs`, `imgcmd.rs`, `push.rs`, `fsx.rs` | the I/O around them |

All `boxlib` modules and `akuma-tar` run under
`cargo test -p box --lib --no-default-features --target $HOST`.
