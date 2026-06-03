# Acceptance: meow agent compiles hello.c with tcc

Verify that `meow` in non-interactive mode can use its `Shell` tool to compile and run
a C program with `tcc` when connected to a local Ollama instance.

This test is also used as a **minimum-RAM sweep** — run it at decreasing `MEMORY` values
to find the floor where meow + tcc fit. See `docs/LOW_MEMORY_ENVIRONMENT.md` for the
per-tier analysis.

Ollama must be running on the host before starting the VM:
```bash
ollama serve   # if not already running
```
The VM reaches the host at `10.0.2.2:11434` via QEMU user-mode networking.
`/etc/meow/config` (from `bootstrap/etc/meow/config`) already points there with `qwen3:4b`.

---

## Preparation (host)

### 1. Build and populate

```bash
cargo build --release
scripts/populate_disk.sh --bin-only
```

### 2. Start the VM

```bash
MEMORY=32 cargo run --release 2>&1 | tee 04_meow_tcc.log
```

Adjust `MEMORY` for the sweep. Based on `docs/LOW_MEMORY_ENVIRONMENT.md`:

| profile | expected floor | notes |
|---------|---------------|-------|
| `release` | **32 MB** | `USER_STACK_SIZE_OVERRIDE=0` (auto 128 KB stacks); was 64 MB with 8 MB override |
| `size` | **16 MB** | always auto-scaled |

Poll for boot:
```bash
until grep -q "\[SSH Server\] Listening" 04_meow_tcc.log 2>/dev/null; do sleep 2; done
```

SSH helper (strip ANSI, ignore known-hosts noise):
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

### 3. Write hello.c

```python
hello_c = '#include <stdio.h>\nint main() { printf("Hello, Akuma!\\n"); return 0; }\n'
rc, out, err = ssh(f"echo '{hello_c}' > /tmp/hello.c")
print(f"write rc={rc}")
```

Or verify it already exists from a previous run:
```python
rc, out, err = ssh("cat /tmp/hello.c")
print(out)
```

### 4. Run meow in non-interactive mode

```python
rc, out, err = ssh(
    'meow -c "Compile /tmp/hello.c with tcc to /tmp/hello_c and run it. '
    'Report the output of the compiled binary."',
    timeout=180
)
print(f"rc={rc}\nout:\n{out}\nerr:\n{err}")
```

meow will use its `Shell` tool autonomously — it will try `tcc /tmp/hello.c -o /tmp/hello_c`,
run `/tmp/hello_c`, and return the output. The LLM figures out any needed flags (`-B`, paths).

### 5. Verify

```python
assert "Hello, Akuma!" in out or "Hello, Akuma!" in err, \
    f"Expected 'Hello, Akuma!' in meow output, got:\n{out}"
print("PASS")
```

---

## Expected output

```
Hello, Akuma!
```

---

## Memory sweep procedure

To find the minimum viable RAM:

```bash
for MB in 32 24 16 12; do
    pkill -9 qemu-system-aarch64 2>/dev/null; sleep 1
    MEMORY=$MB cargo run --release 2>&1 | tee 04_meow_tcc_${MB}mb.log &
    until grep -q "\[SSH Server\] Listening" 04_meow_tcc_${MB}mb.log 2>/dev/null; do sleep 2; done
    # run steps 3-5 above, record pass/fail
done
```

When a tier fails, check whether it's an OOM during meow load, during the LLM tool call
(Shell → tcc), or during tcc itself — each points to a different fix:

| failure point | diagnosis |
|---------------|-----------|
| VM doesn't reach SSH | kernel boot OOM — lower `SYSTEM_THREAD_STACK_SIZE` or heap floor |
| meow fails to start | ELF load OOM — user pages too small |
| `Shell` tool call fails | tcc fork OOM — lower `USER_THREAD_STACK_SIZE` |
| tcc compiles but output wrong | not a memory issue |

## Stack tuning knobs (src/config.rs)

To push the floor lower, reduce in order of payoff:

| knob | current | suggested next | saves (per slot) |
|------|---------|---------------|-----------------|
| `USER_THREAD_STACK_SIZE` | release: 128 KB, size: 64 KB | 64 KB / 32 KB | halves user-thread pool |
| `SYSTEM_THREAD_STACK_SIZE` | 256 KB | 64 KB (measure actual depth first) | 1.3 MB fixed |
| `USER_STACK_SIZE_OVERRIDE` | 0 (auto 128 KB) | already optimal | — |
