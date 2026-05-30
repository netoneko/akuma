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
MEMORY=2048 cargo run --release 2>&1 > 01_verify_apk_bootstrap_acceptance.log
```

The QEMU process runs forever — do NOT block on it or call job_output with wait=true. Poll the log instead:

```bash
until grep -q "SSH Server\] Listening" 01_verify_apk_bootstrap_acceptance.log 2>/dev/null; do sleep 2; done
```

If the port is already in use:

```bash
pkill -9 qemu-system-aarch64
```

## Steps (in VM)

The `ssh` CLI is blocked by security policy. Use Python for all SSH commands:

```python
import subprocess
def ssh(cmd): return subprocess.run(["ssh","-o","StrictHostKeyChecking=no","-p","2222","root@localhost",cmd], capture_output=True, text=True)
```

### 5. Install busybox

```python
ssh("apk add busybox")
```

### 6. Verify busybox works

```python
ssh("busybox echo busybox OK")
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
