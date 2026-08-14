# Run a Docker image with `box run`

Pull an OCI image from a registry and run it in a box whose root is the image's
layers plus a private writable directory. The image is never modified and its
layers are shared between containers.

Needs a build with `sc-containers` and working networking. Everything below was
run on **devbox-smoltcp**.

## 1. Boot and connect

```bash
scripts/build_devbox_smoltcp.sh
overlays/devbox/run-smoltcp.sh          # separate terminal
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost
```

## 2. Pull an image

```
box pull busybox
box pull curlimages/curl
box images
```

Layers land in `/var/lib/box/layers/sha256-<hex>/`, one directory per layer,
shared by every image that references them. The image directory holds only
metadata:

```
/var/lib/box/images/<name>/oci-config.json    # OCI config (Entrypoint, Cmd, …)
/var/lib/box/images/<name>/layers             # digest per line, base-first
```

A layer already in the store is **not** re-downloaded. Extraction goes to
`<digest>.tmp` and is renamed into place, so an interrupted pull cannot leave a
half-populated directory that the next pull would accept.

## 3. Run it

```
box run --rm busybox /bin/busybox echo hello
box run --rm --entrypoint curl curlimages-curl -sS https://example.com
box run --name web -d nginx
```

Arguments after the image replace the image's **Cmd** and are passed to its
Entrypoint, exactly as `docker run` does. `--entrypoint` replaces the
Entrypoint outright.

| Flag | Meaning |
|---|---|
| `--rm` | kill the box and delete the container directory on exit |
| `-d` | detach: print the pid and return |
| `--name X` | container id (otherwise `<image>-<uptime>`) |
| `--entrypoint P` | override the image's Entrypoint |
| `-w DIR` | working directory (overrides `WorkingDir`) |

The container's writable layer is `/var/lib/box/containers/<id>/upper`. Inspect
it after a non-`--rm` run to see exactly what the container changed.

## Interactive shell in a container

`-i` keeps stdin attached. Over SSH you need a PTY on the ssh side too, so use
`ssh -tt`:

```bash
ssh -tt -o StrictHostKeyChecking=no -p 2222 root@localhost \
    'box run --rm -i busybox sh'
```

`sh` has no slash, so it is resolved against the container's `PATH` directories
— `box run --rm -i busybox /bin/busybox sh` is the explicit spelling and does
the same thing. From the box's own console, drop the `ssh -tt` wrapper:

```
# box run --rm -i busybox sh
box: running '/bin/sh' in busybox-343003896 ( 1 layers, ID= e54c3befa445fd9)
/ # cat /etc/hosts
127.0.0.1 localhost busybox-343003896
/ # exit
```

The hostname in `/etc/hosts` is the container id, which is how you can tell the
injected file apart from anything the image shipped.

A container gets its own `/proc`, so `ps` works and shows only that container's
processes:

```
~ # box run --rm -i busybox sh
/ # ps
PID   USER     TIME  COMMAND
    6 root      0:00 /bin/[
    8 root      0:00 {[} ps
/ # exit
~ #                      <- back in the host shell; the session stays open
```

## Verify

A run of a pulled image prints the banner, then the program's own output:

```
# box run --rm --entrypoint curl curlimages-curl --version
box: running '/usr/bin/curl' in curlimages-curl-7976404 ( 1 layers, ID=95ac0a551733cd3f)
curl 8.21.0 (aarch64-unknown-linux-musl) libcurl/8.21.0 OpenSSL/3.5.7 …
```

The layer store is the size of the image, **not** a multiple of it:

```
# du -sh /var/lib/box/layers/*/
4.1M	/var/lib/box/layers/sha256-025fe1949698…/
```

Copy-on-write is real — write and delete inside a container, then check both
sides:

```
# box run --name t9 busybox /bin/busybox sh -c 'echo NEW > /etc/newfile; rm /etc/group; ls /etc'
hosts  localtime  network  newfile  nsswitch.conf  passwd  resolv.conf  shadow
                                          ^ group is gone, newfile is there

# ls /var/lib/box/layers/sha256-025fe*/etc          # image: untouched
group  localtime  network  nsswitch.conf  passwd  shadow

# find /var/lib/box/containers/t9/upper -type f     # container: only its own changes
…/upper/etc/resolv.conf   …/upper/etc/hosts        # injected at start
…/upper/etc/newfile                                 # created
…/upper/etc/.wh.group                               # whiteout for the delete
```

`group` is absent from the container's view but present in the layer, and the
deletion is recorded as a `.wh.group` marker — that is the overlay working.

## Troubleshooting

**`sh: cat: Permission denied` for a command the image obviously ships, while
`/bin/cat` with an explicit path works.** The binary lost its `+x` bit during
extraction, and a shell's `PATH` search calls `access(X_OK)`. Check
`/bin/tar` — if it is a symlink to busybox, layers were extracted by busybox
tar, which creates hardlinks via `link()`, which akuma implements as a full
file copy that drops the mode:

```
# ls -la /bin/tar
lrwxrwxrwx  1 0 0  7  /bin/tar -> busybox        # WRONG
-rwxr-xr-x  1 0 0  41392  /bin/tar               # right — our tar
```

Fix: `(cd userspace && ./build.sh --tar-only)`, remove `/bin/tar` in the VM
(otherwise `cp` follows the symlink and overwrites busybox itself), shut the VM
down, `DISK=devbox.img scripts/populate_disk.sh --bin-only`, then wipe
`/var/lib/box/layers` and re-pull. The tell that you were hit by this is a
layer store hundreds of megabytes larger than the image.

**`box run: failed to spawn /entrypoint.sh`.** The image's Entrypoint is a shell
script, and `spawn_ext` (syscall 315) does not honour shebangs — only `execve`
does. Use `--entrypoint <the real binary>`.

**`box run: image '<x>' has no layer list`.** The image was pulled before the
layer store existed. Re-pull it.

**`box run: overlay mount failed: errno 2`.** A layer directory named in the
image's `layers` file is missing from `/var/lib/box/layers/`. Re-pull.

## Background

- [`../reference/subsystems/containers.md`](../reference/subsystems/containers.md)
  -> "OCI images and the overlay root" — the layer store, `OverlayFs`, and how a
  container root is assembled.
- [`../reference/subsystems/syscalls/container.md`](../reference/subsystems/syscalls/container.md)
  -> "mount_in_ns" — the `overlay` fstype and its option string.
- [`../../userspace/box/docs/OCI_IMAGE_PULL.md`](../../userspace/box/docs/OCI_IMAGE_PULL.md),
  [`../../userspace/box/docs/BOX_RUN.md`](../../userspace/box/docs/BOX_RUN.md).
- [`../archive/BOX_RUN_OVERLAYFS.md`](../archive/BOX_RUN_OVERLAYFS.md)
  — the plan this was built from, its open follow-ups, and the two bugs found
  bringing it up.
