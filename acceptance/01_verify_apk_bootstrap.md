# Acceptance: Alpine apk Bootstrap

Verify that the Alpine apk bootstrap flow works on a running Akuma instance.

**Prerequisite:** QEMU/Akuma must be running with SSH on `localhost:2222`.

```bash
cargo run --release
# or: ./scripts/run.sh
```

## Steps

Run each command over SSH. Replace `$SSH` with:

```bash
SSH="ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 -o BatchMode=yes -p 2222 root@localhost"
```

### 1. Download apk-tools-static from Alpine CDN

```bash
$SSH "curl -L 'https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64/apk-tools-static-3.0.6-r0.apk' -o /tmp/apk.apk"
```

### 2. Extract the static apk binary

```bash
$SSH "mkdir -p /tmp/apkstatic && tar xzf /tmp/apk.apk -C /tmp/apkstatic"
```

### 3. Set up Alpine repositories

```bash
$SSH "mkdir -p /tmp/apkroot/etc/apk && echo 'https://dl-cdn.alpinelinux.org/alpine/latest-stable/main' > /tmp/apkroot/etc/apk/repositories"
```

### 4. Install busybox

```bash
$SSH "/tmp/apkstatic/sbin/apk.static --root /tmp/apkroot add busybox"
```

### 6. Busybox sanity checks

```bash
$SSH "/tmp/apkroot/usr/bin/busybox ls /"
$SSH "/tmp/apkroot/usr/bin/busybox echo 'busybox OK'"
$SSH "/tmp/apkroot/usr/bin/busybox uname -a"
```

### 7. Install a second package to confirm the package DB is healthy

```bash
$SSH "/tmp/apkstatic/sbin/apk.static --root /tmp/apkroot add file"
$SSH "/tmp/apkroot/usr/bin/file /tmp/apkroot/bin/busybox"
```

## Expected Result

All steps complete without error. The final `file` command should output:

```
ELF 64-bit LSB executable, ARM aarch64
```
