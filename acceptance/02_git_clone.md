# Acceptance: Git Clone, TCC Compile, and Run

Verify that `git` and `tcc` can be installed from Alpine apk, a repo cloned over
HTTPS, and a C program from that repo compiled and executed correctly.

## VM shell notes

The VM runs a custom mini-shell (not `/bin/sh`). It lacks `printf`, `which`,
`find -name`, `head`, `tail`, and the `/dev/null` device. Pipes and complex
redirections are unreliable. Use Python for all SSH commands (see step 4).

SSH commands often return rc=255 with `Connection to localhost closed by remote
host.` even when the command succeeded — the VM shell drops the connection after
long-running commands. **Check stdout for success output, not the return code.**

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

Define the SSH helper for all VM steps. `UserKnownHostsFile=/dev/null` avoids
host-key conflicts across disk image rebuilds:

```python
import subprocess, re

def ssh(cmd):
    r = subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no",
         "-o", "UserKnownHostsFile=/dev/null",
         "-p", "2222", "root@localhost", cmd],
        capture_output=True, text=True, timeout=60
    )
    out = re.sub(r'\x1b\[[0-9;]*[KmHm]', '', r.stdout).strip()
    err = '\n'.join(
        l for l in re.sub(r'\x1b\[[0-9;]*[KmHm]', '', r.stderr).strip().splitlines()
        if '@@@@' not in l and 'Warning: Permanently' not in l
    ).strip()
    return r.returncode, out, err
```

## Steps (in VM)

### 5. Install git

```python
rc, out, err = ssh("apk add git")
print(f"rc={rc}\n{out}")
# rc=255 is normal (SSH drop after apk); success if "OK:" appears in out
```

### 6. Verify git installed and clone the playground repo

```python
rc, out, err = ssh("git --version")
print(f"git version: {out}")

rc, out, err = ssh("git clone https://github.com/netoneko/akuma-playground")
print(f"clone rc={rc}\n{out}\n{err}")
```

### 7. Install tcc and musl-dev

`musl-dev` provides the C runtime startup files (`crt1.o`, `crti.o`, `crtn.o`),
standard headers (`stdio.h`, etc.), and static libc — all required for tcc to link.

```python
rc, out, err = ssh("apk add tcc musl-dev")
print(f"rc={rc}\n{out}")
# rc=255 is normal; success if "OK:" appears in out
```

### 8. Compile hello.c with tcc

```python
rc, out, err = ssh("tcc -o /tmp/hello akuma-playground/hello.c")
print(f"compile rc={rc} | out={out!r} | err={err!r}")
# Success: rc=255 (SSH drop), out and err empty
```

### 9. Run the compiled binary

```python
rc, out, err = ssh("/tmp/hello")
print(f"run rc={rc} | out={out!r}")
# Success: out == "Hello, World!"
```

## Expected Result

Step 5 installs git and its dependencies (rc=255 is normal):

```
OK: ... KiB in ... packages
```

Step 6 confirms git is installed and clones successfully:

```
git version 2.x.x
Cloning into 'akuma-playground'...
remote: Enumerating objects: ...
Resolving deltas: 100% (...), done.
```

Step 7 installs tcc and musl-dev (rc=255 is normal):

```
(1/x) Installing musl-dev (...)
(1/x) Installing tcc (...)
OK: ... KiB in ... packages
```

Step 8 produces no output (compile succeeds silently).

Step 9 prints:

```
Hello, World!
```
