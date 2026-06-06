# Acceptance: meow writes and compiles hello.c on the extreme kernel at 4.5 MB

Verify the full agentic pipeline — meow writes `/tmp/hello.c`, compiles it with
our static `tcc`, and runs the output — on the **extreme-size** kernel at **4.5 MB**
RAM, the lowest verified floor for the meow+tcc agentic path (see
`docs/TCC_LOW_MEMORY.md` and `docs/LOW_MEMORY_ENVIRONMENT.md`).

At **4.0 MB** the kernel boots and meow can reach ollama, but tcc's ~4.5 MB
working set makes the compile OOM at that size. **4.5 MB is the current floor.**

Ollama must be running on the host before starting the VM:
```bash
ollama serve   # if not already running
```
The VM reaches the host at `10.0.2.2:11434` via QEMU user-mode networking.
`/etc/meow/config` already points there with `qwen3:4b` (or whichever model is
configured in `bootstrap/etc/meow/config`).

---

## Preparation (host)

### 1. Build the extreme kernel only

```bash
scripts/build_extreme_size.sh
```

This produces `target/aarch64-unknown-none/extreme-size/akuma` (~800 KB stripped
binary, RSA off, debug instrumentation off, ext2 block-cache disabled).

### 2. Populate the disk

```bash
scripts/populate_disk.sh --bin-only
```

The disk must already contain `apk add musl-dev`-installed headers and
`/archives/libtcc1.tar` (our static tcc's runtime archive). Both are part of the
standard apk bootstrap — see `acceptance/01_verify_apk_bootstrap.md`.

### 3. Start the VM at 4.5 MB

```bash
ELF=target/aarch64-unknown-none/extreme-size/akuma
MEMORY=4608K INSTANCE=0 bash scripts/cargo_runner.sh "$ELF" 2>&1 | tee 05_extreme_4mb.log
```

`MEMORY=4608K` = 4.5 MB. `SNAPSHOT=1` may be passed to get a pristine `/tmp` on
each boot (prevents a stale binary from masking a failed compile):

```bash
MEMORY=4608K SNAPSHOT=1 INSTANCE=0 bash scripts/cargo_runner.sh "$ELF" 2>&1 | tee 05_extreme_4mb.log
```

Poll for boot (never call wait on the QEMU process — it runs forever):

```bash
until grep -q "\[SSH Server\] Listening" 05_extreme_4mb.log 2>/dev/null; do sleep 2; done
```

Expected boot banner (approximate):

```
Akuma OS — extreme profile
Code+Stack: ~3 MB   Heap: ~768 KB seed   User pages: ~1 MB
PMM: ~1152 total / ~625 alloc / ~527 free pages
[SSH Server] Listening on 0.0.0.0:22
```

SSH helper (strip ANSI, ignore known-hosts noise):

```python
import subprocess, re

def ssh(cmd, timeout=180):
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

### 4. Install the tcc runtime

```python
rc, out, err = ssh("apk add musl-dev && busybox tar xf /archives/libtcc1.tar -C /", timeout=60)
assert rc == 0, f"tcc runtime install failed: {out} {err}"
print("tcc runtime ready")
```

### 5. Ask meow to write hello.c, compile, and run it

The prompt forces **sequential** shell tool-calls (one for write, one for compile,
one for run). Merging compile+run into a single `&&` shell command makes the peak
footprints overlap, which at 4.5 MB tips into OOM. The phrase
`"run commands one by one using shell tool"` reliably steers `qwen3` to emit
separate calls.

```python
rc, out, err = ssh(
    'meow -c "write a C hello-world program to /tmp/hello.c (use printf, include stdio.h, '
    'main returns 0), then compile it: tcc -static -B /usr/lib/tcc -o /tmp/hello_c /tmp/hello.c, '
    'then run /tmp/hello_c. Run commands one by one using shell tool."',
    timeout=300
)
print(f"rc={rc}\nout:\n{out}\nerr:\n{err}")
```

### 6. Verify

```python
assert "Hello" in out or "Hello" in err, \
    f"Expected 'Hello' in meow output, got:\n{out}\n{err}"

# Belt-and-suspenders: run the binary directly and confirm it survived in /tmp
rc2, run_out, _ = ssh("/tmp/hello_c", timeout=30)
assert rc2 == 0 and "Hello" in run_out, \
    f"Binary /tmp/hello_c failed: rc={rc2} out={run_out}"

print("PASS")
```

---

## Expected output

```
Hello, World!
```

(or `Hello, Akuma!` — exact text depends on what the model writes into `hello.c`;
the assertion checks for `Hello` as a prefix.)

---

## Memory profile at 4.5 MB (extreme)

From `logs/4.5mb_meow5.log` (2026-06-05):

| Stage | Free RAM low-water |
|---|---|
| Post-boot idle | ~2520 KB |
| During meow→ollama | ~2000 KB |
| During tcc compile peak | ~1988 KB |
| Settled after exit | ~2520 KB |

`panic=0` throughout; SSH remained responsive. The ext2 block-cache is disabled
on `extreme` (`#[cfg(not(kernel_profile_extreme))]`), so no heap fragmentation
from block reads.

---

## Failure modes at this floor

| Symptom | Diagnosis |
|---|---|
| VM never reaches SSH | boot OOM — kernel image or stack reserve grew; check `IMAGE_RESERVE` |
| meow exits with `Failed to create request buffer` or empty path errors | lazy-ELF segment-boundary clobber (see `docs/LOW_MEMORY_ENVIRONMENT.md`); rebuild extreme kernel |
| tcc prints `memory full`, exit 1 | tcc's own allocator OOM — user pages dropped below tcc's ~4 MB working set; RAM is too low |
| `anon alloc failed` / `0 free pages` in serial log | PMM exhausted during compile; consider `MEMORY=5120K` |
| meow output missing tcc tool-call | model didn't cooperate; retry (ollama is on the host, RAM-independent) |
| `/tmp/hello_c` exists but runs `Hello, Akuma!` from a *prior* boot | stale binary; reboot with `SNAPSHOT=1` |

---

## Floor reference

| Workload | Floor | Source |
|---|---|---|
| boot + SSH | **4.0 MB** (3.0 MB observed) | `logs/4mb_meow0.log`, `text_offset = 1 MB` fix; 3 MB boots+serves SSH in `logs/oomfix/boot_3mb.log` (2026-06-06, unverified for workloads) |
| `meow -c "say hi"` (LLM only) | **4.0 MB** | same log |
| `tcc -static hello.c` (direct, no meow) | **4.5 MB** (4.0 MB observed) | `scripts/our_tcc_floor.py`; direct compile+run succeeded at 4.0 MB in `logs/oomfix/boot_4mb.log` (2026-06-06) after the heap-growth backoff fix — single run, confirm repeatability |
| **meow agentically writes + compiles + runs** | **4.5 MB** | `logs/4.5mb_meow5.log` |

> **2026-06-06 — heap-growth backoff fix.** A second `EC=0x3c` `brk #1` abort
> (`4mb_meow_tcc0.log`) was *not* PMM exhaustion — it was kernel-heap growth
> failing on a **fragmented** pool (108 free pages, no contiguous run). Fixed by
> backing off the contiguous run length toward `needed` in
> `PmmOomHandler::handle_oom` (see `docs/LOW_MEMORY_ENVIRONMENT.md` → *Heap-growth
> backoff*). Re-validated live at 3 / 4 / 5 MB: `panic=0`, zero crash markers; the
> direct tcc path dropped to 4.0 MB. The genuine multi-page-contiguous OOM that
> remains is the job of the planned OOM killer (`docs/OOM_KILLER_PLAN.md`).
