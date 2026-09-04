# overlays/devbox-amd64

A place for the **amd64 (x86_64) bring-up target** to grow a real userland,
parallel to `overlays/devbox/` (the aarch64 Alpine distro). Right now it is a
one-file wrapper — the image itself is assembled by `amd64/mkdisk.sh`, which
`amd64/run.sh` calls on every boot.

## What's on the image

`amd64/mkdisk.sh` writes an 8 MiB ext2 image with:

| path | what |
|---|---|
| `/bin/hello`, `/bin/fdprobe` | the loader/syscall probes, compiled by `amd64/build.rs` |
| `/bin/paws` | the shell, built from `userspace/` for `x86_64-unknown-none` |
| `/bin/httpd` | the HTTP server (`INIT=/bin/httpd`) |
| `/bin/sshd` | userspace SSH, cooperative single-process build (no `fork-sessions`) |
| `/bin/busybox` + `/bin/{sh,uname,ls,cat,echo,…}` hard-links | stock `1.35.0-x86_64-linux-musl` static busybox, fetched and cached in `target/` |
| `/etc/sshd/authorized_keys` | the test pubkey (`target/x86_64-unknown-none/release/amd64-ssh-test-key.pub`) |
| `/etc/sshd/sshd.conf` | `shell = ${SSHD_SHELL:-/bin/paws}` |
| `/public/index.html` | what `httpd` serves for `GET /` |

## Boot it

```bash
overlays/devbox-amd64/run.sh                       # sshd on host :2223, paws shell
INIT=/bin/httpd        overlays/devbox-amd64/run.sh # curl http://localhost:8080/
INIT=/bin/busybox INITARGS=uname,-a overlays/devbox-amd64/run.sh   # busybox applet
SSHD_SHELL=/bin/sh     overlays/devbox-amd64/run.sh # sshd starts busybox
STRACE=1               overlays/devbox-amd64/run.sh # trace every syscall the init program makes
```

ssh in with the key `amd64/mkdisk.sh` generated:

```bash
ssh -i target/x86_64-unknown-none/release/amd64-ssh-test-key \
    -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -p 2223 root@localhost
```

Firecracker (real hardware): `FC_HOST=… FC_NET=1 INIT=/bin/sshd amd64/run-firecracker.sh`,
after `FC_HOST=… amd64/net-setup.sh`.

## State

- `busybox <applet>` runs (Stage S — `arch_prctl`, SSE, `uname`, `writev`).
- `busybox sh` runs its startup but needs `fstatat`/`stat` and `fork`+`wait4`
  before it can execute anything but a no-`fork` builtin — that is the next
  stage, and its rootfs assembly (a real `/etc`, `/dev`, PATH layout) is what
  this overlay is for.

Background: `docs/archive/AKUMA_FIRECRACKER_AMD64.md`, `amd64/README.md`.
