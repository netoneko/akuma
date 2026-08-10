# Acceptance: nca compiles hello.c and runs it (Docker, disk.img chroot)

Verify nca's agentic pipeline (shell-tool orchestration + tcc compilation) inside
a throwaway Docker container, under a hard memory cap with no swap — mirroring
Akuma's constraints.

Disk: the same `disk.img` used by QEMU tests, loop-mounted and chrooted. No
additional mounts needed. The repo is pre-cloned into the disk during preparation
(scratch uses `libakuma-tls` and cannot network on standard Linux kernels).

1. **nca** receives a one-shot prompt inside the chroot
2. nca calls an ollama model on the host with shell-tool access
3. The model compiles `hello.c` with `tcc -static` and runs the binary
4. Output confirms `Hello, Akuma!`

Goal: find the Docker memory floor for nca + tcc, starting at **24 MB**.

| Model | Size | Status |
|-------|------|--------|
| `qwen3:4b` | 2.5 GB | target (~90 s/tool-call in thinking mode) |
| `qwen3.5:0.8b` | 1.0 GB | stretch goal |

---

## Setup notes

- `--memory <N>m --memory-swap <N>m` — hard cap, zero swap (Docker minimum is 6 MB)
- `--platform linux/arm64` — same ISA as Akuma VM
- `--privileged` — needed to loop-mount `disk.img`
- DNS fix: copy the container's `/etc/resolv.conf` and `/etc/hosts` into the chroot
  so `host.docker.internal` resolves correctly
- `populate_disk.sh` now creates busybox symlinks (`sh`, `chmod`, `ls`, …) via
  `busybox.static` so nca's shell tool can spawn a shell inside the chroot
- scratch uses `libakuma-tls` and cannot make TLS connections on standard Linux;
  pre-clone the repo into the disk during preparation instead

---

## Theoretical tcc floor (no nca overhead)

Measured 2026-06-12, Docker 29.1, `disk.img` chroot, linux/arm64, no swap:

| Memory cap | Result |
|------------|--------|
| < 6 MB | Docker refuses (hard minimum) |
| **6 MB** | **PASS** — `Hello, Akuma!` |
| 8–24 MB | PASS |

**tcc floor: 6 MB.** nca adds tokio runtime + HTTP client; its floor will be higher.

---

## Preparation (host)

### 1. Start ollama

```bash
ollama serve
ollama pull qwen3:4b
```

### 2. Populate the disk

```bash
scripts/populate_disk.sh --with-apk --with-musl-dev
```

This installs `busybox-static`, `musl-dev`, extracts `libtcc1`, copies all
bootstrap binaries, creates the `git → scratch` symlink, and creates essential
busybox symlinks (`sh`, `chmod`, …).

### 3. Pre-clone the test repo into disk.img

scratch cannot clone over TLS on standard Linux. Use Alpine's git instead:

```python
import subprocess, os
subprocess.run([
    "docker", "run", "--rm", "--privileged",
    "-v", f"{os.getcwd()}/disk.img:/disk.img",
    "alpine", "sh", "-c",
    "apk add --no-cache git >/dev/null 2>&1 && "
    "mkdir -p /mnt && mount -o loop /disk.img /mnt && "
    "git clone --depth=1 "
    "  https://github.com/netoneko/akuma-playground.git /mnt/akuma-playground && "
    "umount /mnt",
], check=True)
print("repo pre-cloned into disk.img")
```

This is a one-time step; the clone persists in `disk.img` across test runs.

---

## Steps

### 4. Run nca in a memory-capped busybox container

```python
import subprocess, re, os

MEM = "24m"   # start here; lower to find the floor

result = subprocess.run(
    [
        "docker", "run", "--rm", "--privileged",
        "--platform", "linux/arm64",
        f"--memory={MEM}",
        f"--memory-swap={MEM}",   # swap = 0
        "-v", f"{os.getcwd()}/disk.img:/disk.img",
        "busybox",
        "sh", "-c",
        (
            "set -e && "
            "mkdir -p /mnt && "
            "mount -o loop /disk.img /mnt && "
            # Fix DNS so host.docker.internal resolves inside the chroot
            "cp /etc/resolv.conf /mnt/etc/resolv.conf && "
            "cp /etc/hosts /mnt/etc/hosts && "
            "export NCA_DEFAULT_PROVIDER=openai && "
            "export OPENAI_BASE_URL=http://host.docker.internal:11434 && "
            "export OPENAI_API_KEY=ollama && "
            "export NCA_MODEL=qwen3:4b && "
            "export HOME=/tmp && "
            "export PATH=/bin:/usr/bin && "
            "chroot /mnt /bin/nca --no-tui --permission-mode bypass-permissions --max-turns 5 "
            '--prompt "Run each shell command one at a time. '
            "tcc prints diagnostic open() lines - they are NOT errors, ignore them. "
            "Commands: "
            "1) tcc -static -B /usr/lib/tcc -o /tmp/hello_c /tmp/hello.c  "
            "2) /bin/busybox chmod +x /tmp/hello_c  "
            '3) /tmp/hello_c"'
        ),
    ],
    capture_output=True,
    text=True,
    timeout=600,   # qwen3:4b thinking mode: ~90 s/tool-call × 3 steps + margin
)

out = re.sub(r'\x1b\[[0-9;]*[KmHm]', '', result.stdout).strip()
err = re.sub(r'\x1b\[[0-9;]*[KmHm]', '', result.stderr).strip()
print(f"rc={result.returncode}  mem_cap={MEM}")
print(out)
if err:
    print("--- stderr ---")
    print(err)
```

### 5. Verify

```python
assert "Hello" in out or "Hello" in err, \
    f"Expected 'Hello' in nca output, got:\nstdout={out}\nstderr={err}"
print("PASS")
```

---

## Expected output

```
Connected to qwen3:4b
...
  ⚡ EXECUTE_BASH
  ✓ Tool completed
  ⚡ EXECUTE_BASH
  ✓ Tool completed
  ⚡ EXECUTE_BASH
  ✓ Tool completed
...
Hello, Akuma!
```

---

## Memory floor

Measured 2026-06-12, Docker 29.1, `disk.img` chroot, linux/arm64, `qwen3:4b`, no swap:

| Memory cap | Swap | Result | Notes |
|------------|------|--------|-------|
| 6 MB | none | FAIL (model) | nca runs fine; model misread tcc diagnostic as failure |
| **8 MB** | none | **PASS** | "Hello, Akuma!" — first clean end-to-end pass |
| 12 MB | none | timeout | model wandered; non-deterministic |
| 24 MB | none | PASS | confirmed working |

- tcc-alone floor: **6 MB** (Docker hard minimum; tcc binary + musl static link)
- nca floor: **8 MB** (nca binary RSS + tokio runtime + HTTP client fit in 8 MB)
- The 6 MB failure is model reasoning, not OOM (`rc=0`, not `rc=137`)
- qwen3:4b thinking mode: ~90 s/tool-call; `--max-turns 5` bounds runaway exploration

---

## Differences from acceptance/08 (meow + QEMU)

| Aspect | 08 (meow + QEMU) | 09 (nca + Docker) |
|--------|------------------|-------------------|
| Runtime | Akuma kernel on QEMU virt | busybox + chroot into disk.img |
| Agent binary | `meow` | `nca` |
| Ollama host | `10.0.2.2:11434` | `host.docker.internal:11434` |
| Provider config | `/etc/meow/config` | `NCA_DEFAULT_PROVIDER=openai` + env |
| Memory cap | `MEMORY=4096K` QEMU flag | `--memory` + `--memory-swap` Docker flags |
| Swap | none | none |
| Clone | scratch clone (Akuma TLS) | pre-cloned via Alpine git (scratch can't TLS on Linux) |
| chmod | not needed | required (tcc on musl omits execute bit) |

---

## Failure modes

| Symptom | Diagnosis |
|---------|-----------|
| `rc=137` | OOM kill — this is the nca floor |
| `Spawn failed: No such file or directory` | no `/bin/sh` in chroot; re-run `populate_disk.sh` |
| `host.docker.internal` connection refused | DNS fix missing or ollama not running |
| `nca: not found` | disk not populated; run `populate_disk.sh --with-apk --with-musl-dev` |
| `tcc: libtcc1.a not found` (first attempt) | harmless; tcc retries via `-B /usr/lib/tcc` — only fail if no SUCCESS line follows |
| test times out | qwen3:4b thinking mode is slow; use `timeout=900` (15 min) |
| disk changes lost between runs | `disk.img` is a persistent file — changes accumulate; `SNAPSHOT=1` not available here |
