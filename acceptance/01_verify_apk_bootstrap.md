# Acceptance: Alpine apk Bootstrap

Verify that the Alpine apk bootstrap flow works on a running Akuma instance
starting from a fresh empty disk image (no pre-built userland).

## Preparation (host)

### 1. Set up SSH authorized keys

```bash
mkdir -p bootstrap/etc/sshd/
cp ~/.ssh/id_ed25519.pub bootstrap/etc/sshd/authorized_keys
```

### 2. Build apk-tools and stage bootstrap assets

```bash
cd userspace && ./build.sh --apk-only && cd ..
```

This downloads the static apk binary, Alpine signing keys, repo config, and CA
certificates into `bootstrap/`.

### 3. Create and populate the disk image

```bash
./scripts/create_disk.sh
./scripts/populate_disk.sh
```

### 4. Start the VM

```bash
cargo run --release
# or: ./scripts/run.sh
```

Wait until SSH is accepting connections on `localhost:2222`.

## Steps (in VM)

### 5. Install busybox

```bash
ssh -p 2222 root@localhost "apk add busybox"
```

### 6. Verify busybox works

```bash
ssh -p 2222 root@localhost "busybox sh -c 'echo busybox OK'"
```

## Expected Result

```
(1/2) Installing musl (1.2.5-r23)
(2/2) Installing busybox (1.37.0-r30)
  Executing busybox-1.37.0-r30.post-install
Executing busybox-1.37.0-r30.trigger
OK: 1612 KiB in 2 packages
busybox OK
```
