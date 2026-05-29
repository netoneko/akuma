# Acceptance: Git Clone, TCC Compile, and Run

Verify that `git` and `tcc` can be installed from Alpine apk, a repo cloned over
HTTPS, and a C program from that repo compiled and executed correctly.

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
MEMORY=2048 cargo run --release 2>&1 > 02_git_clone_acceptance.log
```

The QEMU process runs forever — do NOT block on it or call job_output with wait=true. Poll the log instead:

```bash
until grep -q "SSH Server\] Listening" 02_git_clone_acceptance.log 2>/dev/null; do sleep 2; done
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

### 5. Install git

```python
ssh("apk add git")
```

### 6. Clone the playground repo

```python
ssh("git clone https://github.com/netoneko/akuma-playground")
```

### 7. Install tcc

```python
ssh("apk add tcc")
```

### 8. Compile hello.c with tcc

```python
ssh("tcc -o /tmp/hello akuma-playground/hello.c")
```

### 9. Run the compiled binary

```python
ssh("/tmp/hello")
```

## Expected Result

Step 5 installs git and its dependencies:

```
(1/x) Installing ...
OK: ... KiB in ... packages
```

Step 6 clones successfully:

```
Cloning into 'akuma-playground'...
remote: Enumerating objects: ...
...
Resolving deltas: 100% (...), done.
```

Step 7 installs tcc and its dependencies:

```
(1/x) Installing tcc (...)
OK: ... KiB in ... packages
```

Step 8 produces no output (compile succeeds silently).

Step 9 prints:

```
Hello, World!
```
