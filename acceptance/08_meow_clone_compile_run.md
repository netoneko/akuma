# Acceptance: meow clones a repo, compiles hello.c, and runs it (extreme kernel, 4 MB)

Verify the full agentic pipeline on the extreme-size kernel at **4.0 MB** RAM:

1. **meow** inside the VM receives a one-shot prompt
2. meow calls an ollama model (on the host) with shell-tool access
3. The model clones `akuma-playground` using `scratch`, compiles `hello.c` with `tcc -static`, and runs the binary
4. Output confirms `Hello, Akuma!`

The goal is to show this works on increasingly small models:

| Model | Size | Status |
|-------|------|--------|
| `qwen3:4b` | 2.5 GB | target |
| `gemma4-yolo-4b:latest` | 9.6 GB | target |
| `qwen3.5:0.8b` | 1.0 GB | stretch goal |

---

## VM shell notes

The VM runs a custom mini-shell (not `/bin/sh`). It lacks `printf`, `which`,
`find -name`, `head`, `tail`, and the `/dev/null` device. Pipes and complex
redirections are unreliable. Use Python for all SSH commands.

SSH commands often return rc=255 with `Connection to localhost closed by remote
host.` even when the command succeeded. **Check stdout for success output, not the
return code.**

---

## Preparation (host)

### 1. Start ollama with the target model

```bash
ollama serve   # if not already running
ollama pull qwen3:4b   # or gemma4-yolo-4b:latest
```

The VM reaches the host at `10.0.2.2:11434` via QEMU user-mode networking.
`/etc/meow/config` must point there with the target model (see step below).

### 2. Configure meow model

Edit `bootstrap/etc/meow/config` to set the ollama model:

```
# For qwen3:4b:
MODEL=qwen3:4b
OLLAMA_HOST=http://10.0.2.2:11434

# For gemma4-yolo-4b:latest:
MODEL=gemma4-yolo-4b:latest
OLLAMA_HOST=http://10.0.2.2:11434
```

### 3. Build the extreme kernel and scratch

```bash
scripts/build_extreme_size.sh
userspace/build.sh --scratch-only
```

### 4. Populate the disk

```bash
scripts/populate_disk.sh --with-apk --with-musl-dev
```

Pre-installs into the disk image:
- `busybox-static` and `musl-dev` via Alpine apk (no network needed in VM)
- `libtcc1.tar` → `/usr/lib/tcc/` (tcc runtime)
- `bootstrap/tmp/hello.c` → `/tmp/hello.c` (fallback test file)
- `scratch` binary → `/bin/scratch` (or similar path from build)

### 5. Start the VM at 4 MB

```bash
ELF=target/aarch64-unknown-none/extreme-size/akuma
MEMORY=4096K SNAPSHOT=1 INSTANCE=0 bash scripts/cargo_runner.sh "$ELF" 2>&1 | tee 08_meow_clone.log
```

Poll for boot (never call wait on the QEMU process — it runs forever):

```bash
until grep -q "\[SSH Server\] Listening" 08_meow_clone.log 2>/dev/null; do sleep 2; done
```

Define the SSH helper for all VM steps:

```python
import subprocess, re

def ssh(cmd, timeout=120):
    r = subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no",
         "-o", "UserKnownHostsFile=/dev/null",
         "-p", "2222", "root@localhost", cmd],
        capture_output=True, text=True, timeout=timeout
    )
    out = re.sub(r'\x1b\[[0-9;]*[KmHm]', '', r.stdout).strip()
    err = '\n'.join(
        l for l in re.sub(r'\x1b\[[0-9;]*[KmHm]', '', r.stderr).strip().splitlines()
        if '@@@@' not in l and 'Warning: Permanently' not in l
    ).strip()
    return r.returncode, out, err
```

---

## Steps (in VM)

### 6. Verify scratch is present

`scratch` is pre-installed to `/bin/scratch` by `populate_disk.sh` (via `bootstrap/bin/*`). No network install needed.

```python
_, out, _ = ssh("ls /bin/scratch")
assert "scratch" in out, f"scratch missing from /bin: {out}"
print("scratch ready")
```

### 7. Run meow with the one-shot prompt

The prompt forces **sequential** tool calls: clone first, then compile, then run.
Merging compile+run into `&&` makes peak footprints overlap and tips into OOM at
4 MB. The phrase `"run commands one by one using shell tool"` steers small models
to emit separate calls.

```python
rc, out, err = ssh(
    'meow -c "Clone https://github.com/netoneko/akuma-playground.git using scratch '
    '(run: scratch clone https://github.com/netoneko/akuma-playground.git from /), '
    'then compile /akuma-playground/hello.c with tcc: '
    'tcc -static -B /usr/lib/tcc -o /tmp/hello_c /akuma-playground/hello.c, '
    'then run /tmp/hello_c. Run commands one by one using shell tool."',
    timeout=300
)
print(f"rc={rc}\nout:\n{out}\nerr:\n{err}")
```

### 8. Verify

```python
# Primary: check meow output contains Hello
assert "Hello" in out or "Hello" in err, \
    f"Expected 'Hello' in meow output, got:\n{out}\n{err}"

# Belt-and-suspenders: run the binary directly
_, run_out, _ = ssh("exec /tmp/hello_c", timeout=30)
assert "Hello" in run_out, f"Binary /tmp/hello_c failed: out={run_out}"

print("PASS")
```

---

## Expected output

Step 6:
```
scratch ready
```

Step 7 (meow orchestration output, approximate):
```
Cloning into '/akuma-playground'...
Hello, Akuma!
```

Step 8:
```
Hello, Akuma!
PASS
```

---

## Memory profile at 4.0 MB

Operations run sequentially — scratch exits before tcc starts — so their
working sets do not overlap.

| Stage | Free RAM low-water |
|---|---|
| Post-boot idle | ~2520 KB |
| During scratch clone (TLS + pack parse) | ~2200 KB |
| After clone, scratch exits | ~2520 KB |
| During meow→ollama request | ~2000 KB |
| During tcc compile peak | ~1988 KB |
| Settled after exit | ~2520 KB |

---

## Failure modes

| Symptom | Diagnosis |
|---|---|
| VM never reaches SSH | boot OOM — kernel image grew; check `IMAGE_RESERVE` |
| `scratch` missing from `/bin` | `populate_disk.sh` wasn't run; re-run `scripts/populate_disk.sh --with-apk --with-musl-dev` and recreate disk |
| `scratch clone` network error | verify host internet access and `github.com` reachable from guest |
| meow exits with `Failed to create request buffer` | lazy-ELF segment-boundary clobber; rebuild extreme kernel |
| `tcc: error: file 'libtcc1.a' not found` | `-B /usr/lib/tcc` missing from tcc command; check meow output |
| `memory full` from tcc | RAM < 4 MB; use `MEMORY=4608K` |
| `/akuma-playground/` already exists | always boot with `SNAPSHOT=1`; repopulate disk otherwise |
| model didn't clone/compile/run sequentially | retry; add `"run commands one by one using shell tool"` to prompt; try larger model |
| meow output empty | check ollama is running on host and `OLLAMA_HOST` in `/etc/meow/config` points to `10.0.2.2:11434` |

---

## Floor reference

| Workload | Floor | Source |
|---|---|---|
| boot + SSH | **4.0 MB** | `logs/oomfix/boot_3mb.log` |
| `scratch clone` | **4.0 MB** | verified 2026-06-06 |
| `tcc -static hello.c` (direct) | **4.0 MB** | `scripts/our_tcc_floor.py` (2026-06-06) |
| meow → qwen3:4b → clone + compile + run | **TBD** | acceptance/08 target |
| meow → gemma4-yolo-4b → clone + compile + run | **TBD** | acceptance/08 target |
| meow → qwen3.5:0.8b → clone + compile + run | **TBD** | acceptance/08 stretch goal |
