# Build a Docker image with `box build`

Build an image from a Dockerfile on Akuma, run it, and prove it is the same
image Docker would have built. How it works and where it differs:
[`userspace/box/docs/BOX_BUILD.md`](../../userspace/box/docs/BOX_BUILD.md).

Needs a `box` built after 2026-10-15 and a kernel with the 2026-10-15
`SubdirFs`/overlay fix (below) — before it, any `RUN` that grew a file with
`ftruncate`/`fallocate` (every `apk add` of a large file) died with `I/O error`.

## 1. A first build

```
mkdir -p /tmp/app && cd /tmp/app
cat > Dockerfile <<'EOF'
FROM alpine:3.20
ARG GREETING=hello
ENV WHO=akuma
WORKDIR /app
COPY hello.txt /app/
RUN apk add --no-cache jq && echo "$GREETING $WHO" > /app/greeting
CMD ["cat", "/app/greeting"]
EOF
echo "from the context" > hello.txt
box build -t app:1 /tmp/app
box run --rm app:1                       # -> hello akuma
```

`FROM` pulls the base if it is not in the store. Each `RUN` prints its own
output. A second `box build` prints ` ---> Using cache` for every layer step.

Options: `-f <Dockerfile>`, `--build-arg K=V` (repeatable), `--target <stage>`,
`--no-cache`, several `-t`. `.dockerignore` is read from the context.

## 2. Look at what was made

```
cat /var/lib/box/images/app-1/layers         # digests, base-first
ls  /var/lib/box/blobs                       # the compressed layers, by digest
ls  /var/lib/box/layers                      # the same layers, extracted
cat /var/lib/box/images/app-1/oci-config.json
```

`box commit` does the same for a container: `box run --name c -d …`, change
something inside it, `box commit c newimage`.

## 3. The two gates

**Same Dockerfile, two builders.** Build on Akuma and with Docker, then compare
the merged filesystem, not the digests (timestamps and image ids differ):

```
# On the Mac:   docker build -t app:1 . && docker create --name x app:1 && docker export x | tar tvf - | sort > docker.txt
# On Akuma:     box save -o /tmp/app.tar app:1     # then list each layer in order, applying whiteouts
```

Per path: type, mode, uid, gid, size, content hash, link target, hard-link
group. They must match.

**Interop.** `box save` on Akuma → `docker load` → `docker run`:

```
ssh -p 2222 root@localhost 'box save app:1' | docker load
docker run --rm app:1
```

Verified 2026-10-15 against Docker 29.1.2 (containerd image store), both ways:
an AArch64 build in QEMU and an `amd64` build on the trashcan (`FROM alpine`,
`COPY`, `RUN apk add jq`) load, and `docker run` prints the `CMD` output, with
`jq` and the copied file present. It found two bugs, both fixed:

- `index.json`'s `io.containerd.image.name` was the bare `app:1`. Docker keeps
  it verbatim, so `docker load` said "Loaded image" and then no lookup by
  name found it (`docker run app:1` tried to pull it; by ID it ran). It is now
  `docker.io/library/app:1` (`imgconfig::qualified_reference`).
- `created` was `586524-01-19…` on amd64 and Docker refused to deserialize the
  config. `libakuma::time()` is a private syscall the amd64 kernel does not
  implement; its `-ENOSYS` read as microseconds. `fsx::now_secs` uses
  `clock_gettime` now.

The tar comes back from the trashcan over ssh as `base64` — the channel turns
LF into CRLF on the way out (`amd64-bare-metal-loop.md`) — checked by `md5sum`.
Run an `amd64` image on an arm64 Mac with `docker run --platform linux/amd64`.

`RUN`-created files were owned by uid 4294967292 on amd64 until 2026-10-16
(`SIGWINCH` from `reattach` interrupted the child's first `getuid`;
`docs/archive/BOX_BUILD.md` § 8, "uid -4"). If a layer lists that uid, the
kernel predates the fix.

## 4. Push it to a registry

```
box tag app:1 ghcr.io/me/app:1
echo "$TOKEN" | box login -u me --password-stdin ghcr.io      # once; or BOX_REGISTRY_USER/_PASSWORD
box push ghcr.io/me/app:1
```

Expect `Layer already exists` for every blob a registry has and `1: digest:
sha256:… size: …` at the end — the same digest `docker pull` then prints.

Try it without an account: on the Mac, `docker run -d -p 5000:5000 registry:2`,
then on Akuma `box tag app:1 <mac-ip>:5000/app:1 && box push --insecure
<mac-ip>:5000/app:1`, and `docker pull localhost:5000/app:1` on the Mac. For a
login, `docker run --rm --entrypoint htpasswd httpd:2 -Bbn alice pw >
htpasswd` and start `registry:2` with `REGISTRY_AUTH=htpasswd`,
`REGISTRY_AUTH_HTPASSWD_REALM=r`, `REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd`.

**TLS is not verified** (`libakuma-tls` has no trust store): do not push over a
network you do not trust. `box push` says so each time. Reference:
[`box push`](../../userspace/box/docs/BOX_BUILD.md#box-push).

## 5. Clean up

```
box rmi app:1          # drops the name, then collects layers/blobs/cache nothing references
box prune              # the same collection without removing an image
```

Both discard the build cache's intermediate layers. Not while a build runs.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `box build: line N: …is not supported` | `ONBUILD`, `RUN --mount/--network/--security`, heredocs, `FROM --<flag>` are refused on purpose |
| `the image already has 32 layers…` | the overlay root takes 32 lower directories, one is the runtime layer; squash the base or use fewer steps |
| `ERROR: Failed to create usr/bin/…: I/O error` from `apk` inside a `RUN` | kernel older than the 2026-10-15 `SubdirFs`/overlay fix: `ftruncate` answered `EIO` for every file under a container's upper |
| `apk`: `temporary error (try again later)`, or `bad address`, from a `RUN` on a host whose `/etc/resolv.conf` is `127.0.0.53` | kernel/`box` older than 2026-10-15: the container's `resolv.conf` was a copy of the host's, and a loopback stub does not exist inside the container. `box` derives it now (loopback nameservers dropped, `8.8.8.8`/`1.1.1.1` if none are left) |
| `wget: bad address 'example.com'` in a `busybox` (glibc) base | the Docker Hub `busybox` image is glibc-linked and its static resolver needs NSS; not a network fault — `nslookup` in the same box works. Use `alpine` (musl) |
| `RUN` prints nothing, but the build succeeds | the output goes to the caller's process channel, which only Akuma's own sshd (port 2222) reads; under another sshd (Gentoo's OpenSSH on 4444) exit statuses are right and the text is lost. Run builds from a 2222 session |
| a long build dies when the ssh session drops (wifi) | detach it: `nohup box build … > /tmp/build.log 2>&1 &` and read the log |
| `box save: layer … has no stored blob` | the image was pulled before 2026-10-15; `box rmi` and `box pull` it again |
| a second `box build` re-runs a `RUN` you did not change | its key covers the **whole environment**, WORKDIR and USER — an earlier `ARG`/`ENV` change reaches it |

## Verify

```
box build -t t1 /tmp/app && box build -t t1 /tmp/app   # second run: every layer step says "Using cache"
box run --rm t1                                          # prints the CMD's output
box save -o /tmp/t1.tar t1 && tar tvf /tmp/t1.tar        # oci-layout, index.json, manifest.json, blobs/sha256/…
cd userspace && cargo test -p box --lib --no-default-features --target $(rustc -vV | sed -n 's/host: //p')
(cd tar && cargo test --lib --no-default-features --target $(rustc -vV | sed -n 's/host: //p'))
```

## Background

- [`archive/BOX_BUILD.md`](../archive/BOX_BUILD.md) — the design and its prerequisites (permissions, hard links, directory rename)
- [`archive/UNIX_PERMISSIONS.md`](../archive/UNIX_PERMISSIONS.md), [`archive/HARD_LINKS.md`](../archive/HARD_LINKS.md)
