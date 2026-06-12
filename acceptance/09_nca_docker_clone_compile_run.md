# Acceptance: nca clones a repo, compiles hello.c, and runs it (Docker, 24 MB)

Verify the full agentic pipeline using **nca** inside a throwaway Docker busybox
container, under a hard memory cap with no swap — mirroring Akuma's constraints.

1. **nca** receives a one-shot prompt inside the container
2. nca calls an ollama model (on the host) with shell-tool access
3. The model clones `akuma-playground` with `git clone` (→ scratch via symlink),
   compiles `hello.c` with `tcc -static`, and runs the binary
4. Output confirms `Hello, Akuma!`

Goal: find the Docker memory floor for nca + tcc, starting at **24 MB**.

| Model | Size | Status |
|-------|------|--------|
| `qwen3:4b` | 2.5 GB | target |
| `qwen3.5:0.8b` | 1.0 GB | stretch goal |

---

## Setup notes

- `--memory <N>m --memory-swap <N>m` — hard cap, zero swap (Docker minimum is 6 MB)
- `--platform linux/arm64` — same ISA as Akuma VM; runs native on Apple Silicon
- `./bootstrap` mounted at `/bootstrap`; `bootstrap/usr` mounted at `/usr` — no loop
  mount, no chroot, no DNS hijack
- `git clone` works because `populate_disk.sh` adds `/bin/git → scratch` symlink and
  we copy that symlink to `bootstrap/bin/git`
- tcc does not set `+x` on its output binary; the prompt instructs the model to
  `chmod +x` before running

---

## Theoretical tcc floor (no nca overhead)

Measured 2026-06-12 on Apple Silicon, Docker 29.1, linux/arm64 busybox, no swap:

| Memory cap | Result |
|------------|--------|
| 4 MB | Docker refuses (minimum is 6 MB) |
| 5 MB | Docker refuses (minimum is 6 MB) |
| **6 MB** | **PASS** — `Hello, Akuma!` |

**tcc floor: 6 MB** (Docker hard minimum; actual working set is below this).
nca adds its own heap, tokio runtime, and HTTP client on top of tcc's footprint.

---

## Preparation (host)

### 1. Start ollama with the target model

```bash
ollama serve   # if not already running
ollama pull qwen3:4b
```

The container reaches the host at `host.docker.internal:11434` (macOS Docker
Desktop resolves this automatically).

### 2. Populate the disk and sync bootstrap

```bash
scripts/populate_disk.sh --with-apk --with-musl-dev
```

Then extract the musl/tcc runtime from `disk.img` into `bootstrap/` so the
Docker run does not need a loop mount:

```python
import subprocess, os
subprocess.run([
    "docker", "run", "--rm", "--privileged",
    "-v", f"{os.getcwd()}/disk.img:/disk.img",
    "-v", f"{os.getcwd()}/bootstrap:/out",
    "alpine", "sh", "-c",
    "mkdir -p /mnt && mount -o loop /disk.img /mnt && "
    "cp -a /mnt/usr/. /out/usr/ && umount /mnt",
], check=True)
print("bootstrap/usr/ populated")
```

This creates `bootstrap/usr/lib/crt1.o`, `bootstrap/usr/lib/tcc/libtcc1.a`, etc.

`populate_disk.sh` also creates `/bin/git → scratch` in the disk; copy it to
`bootstrap/bin/` so it is available without mounting the disk:

```bash
ln -sf scratch bootstrap/bin/git
```

---

## Steps

### 3. Run nca in a memory-capped busybox container

```python
import subprocess, re, os

MEM = "24m"   # start here; lower to find the floor

result = subprocess.run(
    [
        "docker", "run", "--rm",
        "--platform", "linux/arm64",
        f"--memory={MEM}",
        f"--memory-swap={MEM}",   # swap = 0
        "-v", f"{os.getcwd()}/bootstrap:/bootstrap:ro",
        "-v", f"{os.getcwd()}/bootstrap/usr:/usr:ro",
        "busybox",
        "sh", "-c",
        (
            "export NCA_DEFAULT_PROVIDER=openai && "
            "export OPENAI_BASE_URL=http://host.docker.internal:11434 && "
            "export OPENAI_API_KEY=ollama && "
            "export NCA_MODEL=qwen3:4b && "
            "export HOME=/tmp && "
            "export PATH=/bootstrap/bin:/bin:/usr/bin && "
            "/bootstrap/bin/nca --no-tui --permission-mode bypass-permissions "
            '--prompt "git clone https://github.com/netoneko/akuma-playground.git '
            "/akuma-playground, then compile /akuma-playground/hello.c with tcc: "
            "tcc -static -B /usr/lib/tcc -o /tmp/hello_c /akuma-playground/hello.c, "
            "then chmod +x /tmp/hello_c and run /tmp/hello_c. "
            'Run commands one by one using shell tool."'
        ),
    ],
    capture_output=True,
    text=True,
    timeout=300,
)

out = re.sub(r'\x1b\[[0-9;]*[KmHm]', '', result.stdout).strip()
err = re.sub(r'\x1b\[[0-9;]*[KmHm]', '', result.stderr).strip()
print(f"rc={result.returncode}  mem_cap={MEM}")
print(f"--- stdout ---")
print(out)
if err:
    print(f"--- stderr ---")
    print(err)
```

### 4. Verify

```python
assert "Hello" in out or "Hello" in err, \
    f"Expected 'Hello' in nca output, got:\nstdout={out}\nstderr={err}"

print("PASS")
```

---

## Expected output

Step 3 (nca orchestration output, approximate):
```
Connected to qwen3:4b
...
Hello, Akuma!
```

Step 4:
```
PASS
```

---

## Memory floor — finding the minimum for nca

Lower `MEM` in step 3 until Docker OOM-kills the container (`rc=137`).

| Memory cap | Swap | Result |
|------------|------|--------|
| `24m` | none | TBD |
| `16m` | none | TBD |
| `12m` | none | TBD |
| `8m` | none | TBD |
| `6m` | none | TBD (tcc alone passes; nca adds overhead) |

Note: Docker refuses caps below 6 MB. The nca floor will be somewhere above the
tcc-alone floor of 6 MB.

---

## Differences from acceptance/08 (meow + QEMU)

| Aspect | 08 (meow + QEMU) | 09 (nca + Docker) |
|--------|------------------|-------------------|
| Runtime | Akuma kernel on QEMU virt | busybox container |
| Agent binary | `meow` | `nca` |
| Ollama host | `10.0.2.2:11434` | `host.docker.internal:11434` |
| Provider config | `/etc/meow/config` | `NCA_DEFAULT_PROVIDER=openai` + env |
| Memory cap | `MEMORY=4096K` (QEMU flag) | `--memory` + `--memory-swap` (Docker) |
| Swap | none | none (`--memory-swap == --memory`) |
| Clone command | `scratch clone <url>` | `git clone <url>` (git → scratch symlink) |
| tcc +x | not needed (Akuma sets it) | `chmod +x` required (musl tcc omits it) |

---

## Failure modes

| Symptom | Diagnosis |
|---------|-----------|
| `rc=137` | OOM kill — raise `MEM` or this is the floor |
| `tcc: crt1.o not found` | `bootstrap/usr/` not populated; re-run the extraction step |
| `tcc: libtcc1.a not found` | ignored — tcc also checks `lib/tcc/` via `-B`; look for a real error after |
| `git: not found` | `bootstrap/bin/git` symlink missing; run `ln -sf scratch bootstrap/bin/git` |
| `Permission denied` running hello_c | model skipped `chmod +x`; add it explicitly to the prompt |
| `connection refused` to ollama | ollama not running on host |
| nca exits immediately, no output | missing `--no-tui`, or provider not reachable |
| model didn't clone/compile sequentially | retry with `qwen3:4b`; the 0.8b model may conflate steps |
