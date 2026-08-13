# Acceptance: meow uses scratch to clone a repo, then compiles and runs hello.c on the extreme kernel at 4.0 MB

Verify the full agentic pipeline — meow clones `akuma-playground` with `scratch`,
compiles `hello.c` from the cloned repo with our static `tcc`, and runs the output
— on the **extreme-size** kernel at **4.0 MB** RAM.

**4.0 MB is the current floor**, as of 2026-08-10. It used to be 4.5 MB —
tcc's working set tipped 4.0 MB into OOM — but removing the in-kernel SSH
server, shell, editor and TLS stack, and switching extreme to
`userspace-sshd` (herd off, `/bin/paws` as the login shell) freed enough that
the whole flow completes at 4.0 MB with ~1.5 MB still free. See
`docs/archive/BUILTIN_SSH_REMOVAL.md`.

Ollama must be running on the host before starting the VM:
```bash
ollama serve   # if not already running
```
The VM reaches the host at `10.0.2.2:11434` via QEMU user-mode networking.
`/etc/meow/config` already points there with `qwen3:4b` (or whichever model is
configured in `bootstrap/etc/meow/config`).

---

## Preparation (host)

### 1. Build the extreme kernel and scratch

```bash
scripts/build_extreme_size.sh
```

This produces `target/aarch64-unknown-none/extreme-size/akuma` (~800 KB stripped
binary, RSA off, debug instrumentation off, ext2 block-cache disabled).

Then build and stage `scratch`:

```bash
userspace/build.sh --scratch-only
```

### 2. Populate the disk

```bash
scripts/populate_disk.sh --with-apk --with-musl-dev
```

This pre-installs `musl-dev` and extracts `libtcc1.tar` into the disk image so
no `apk add` is needed at VM runtime. It also wipes `/tmp` and re-stages
`bootstrap/tmp/` so compiled artifacts from prior runs are gone.

### 3. Start the VM at 4.0 MB

```bash
ELF=target/aarch64-unknown-none/extreme-size/akuma
MEMORY=4096K INSTANCE=0 bash scripts/cargo_runner.sh "$ELF" 2>&1 | tee 05_extreme_4mb.log
```

`MEMORY=4096K` = 4.0 MB. Use `SNAPSHOT=1` so every boot starts from the
populate-disk state — `/tmp` is clean and `/akuma-playground` doesn't exist:

```bash
MEMORY=4096K SNAPSHOT=1 INSTANCE=0 bash scripts/cargo_runner.sh "$ELF" 2>&1 | tee 05_extreme_4mb.log
```

Poll for boot (never call wait on the QEMU process — it runs forever):

```bash
until grep -aqE "sshd started|Started sshd" 05_extreme_4mb.log 2>/dev/null; do sleep 2; done
```

Expected boot banner (approximate):

```
Akuma OS — extreme profile
Code+Stack: ~3 MB   Heap: ~768 KB seed   User pages: ~1 MB
PMM: ~1152 total / ~625 alloc / ~527 free pages
[Main] sshd started (tid=8)
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

### 4. Check scratch is present

Nothing to install: `scratch` ships in `bootstrap/bin/`, and `populate_disk.sh`
copies it to `/bin/scratch` and creates the `/bin/git -> scratch` symlink itself
(step 2 above). Just confirm both landed.

```python
rc, out, err = ssh("busybox ls -l /bin/git /bin/scratch", timeout=30)
assert rc == 0 and "scratch" in out, f"scratch/git missing from image: {out} {err}"
print("scratch ready")
```

> This step used to run `pkg install scratch --as=/bin/git` against a host
> package server on `:8000`. **`pkg` no longer exists** — the userspace package
> manager was deleted on 2026-02-27 (`d12440a`, "add apk-tools and remove
> userspace pkg") in favour of `apk`, and there is no `/bin/pkg` in
> `bootstrap/bin/` or on any disk image. The step was also redundant by then,
> since `populate_disk.sh` already stages both files.

### 5. Ask meow to clone the repo, compile hello.c, and run it

The prompt forces **sequential** shell tool-calls: clone first, then compile, then
run. Merging compile+run into a single `&&` shell command makes the peak footprints
overlap, which at 4.0 MB tips into OOM. The phrase
`"run commands one by one using shell tool"` reliably steers `qwen3` to emit
separate calls.

`akuma-playground` contains `hello.c` at the repo root. scratch clones it into
`/akuma-playground/` when invoked from `/`.

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

### 6. Verify

```python
assert "Hello" in out or "Hello" in err, \
    f"Expected 'Hello' in meow output, got:\n{out}\n{err}"

# Belt-and-suspenders: run the binary directly
_, run_out, _ = ssh("exec /tmp/hello_c", timeout=30)
assert "Hello" in run_out, f"Binary /tmp/hello_c failed: out={run_out}"

print("PASS")
```

---

## Expected output

```
Hello, World!
```

(or any `Hello` variant — exact text depends on what `hello.c` in the repo
contains; the assertion checks for `Hello` as a prefix.)

---

## Memory profile at 4.0 MB (extreme)

Scratch and tcc run sequentially — scratch exits before tcc starts — so their
working sets do not overlap.

| Stage | Free RAM low-water |
|---|---|
| Post-boot idle | ~2520 KB |
| During scratch clone (TLS + pack parse) | ~2200 KB |
| After clone, scratch exits | ~2520 KB |
| During meow→ollama | ~2000 KB |
| During tcc compile peak | ~1988 KB |
| Settled after exit | ~2520 KB |

`panic=0` throughout; SSH remained responsive.

---

## Failure modes at this floor

| Symptom | Diagnosis |
|---|---|
| VM never reaches SSH | boot OOM — kernel image or stack reserve grew; check `IMAGE_RESERVE` |
| `/bin/scratch` or `/bin/git` missing | the image was populated before `scratch` was built. Run `userspace/build.sh --scratch-only`, then re-run `scripts/populate_disk.sh` (it creates the `git -> scratch` symlink). There is no package manager to fall back on — `pkg` is gone (see step 4) |
| `tcc: error: file 'tcc' not found` | the login shell passed the command name twice. **FIXED** 2026-08-13 — [`docs/archive/PAWS_DUPLICATED_ARGV0.md`](../docs/archive/PAWS_DUPLICATED_ARGV0.md). If it reappears, `/bin/paws` on the image predates the fix; rebuild and re-stage it |
| the compiled binary segfaults | it was linked **without** `-static`. tcc's default (dynamic) output links cleanly and then crashes; keep `-static -B /usr/lib/tcc` exactly as written above |
| `scratch clone` OOM | rare at 4.0 MB — scratch's peak (TLS + pack buffer) is ~300 KB; check kernel serial log for `anon alloc failed` |
| `scratch clone` fails with network error | DNS or TLS failure; verify the host has internet access and `github.com` is reachable from the guest via `nslookup github.com` |
| meow exits with `Failed to create request buffer` or empty path errors | lazy-ELF segment-boundary clobber (see `docs/LOW_MEMORY_ENVIRONMENT.md`); rebuild extreme kernel |
| tcc prints `memory full`, exit 1 | tcc's own allocator OOM — user pages dropped below tcc's ~4 MB working set; RAM is too low |
| `anon alloc failed` / `0 free pages` in serial log | PMM exhausted during compile; consider `MEMORY=5120K` |
| meow output missing scratch or tcc tool-call | model didn't cooperate; retry (ollama is on the host, RAM-independent) |
| `/akuma-playground/` already exists from prior boot | always boot with `SNAPSHOT=1`; repopulate disk otherwise |

---

## Floor reference

| Workload | Floor | Source |
|---|---|---|
| boot + SSH | **4.0 MB** | `logs/oomfix/boot_3mb.log` (3 MB also boots) |
| `scratch clone` (TLS clone, 4 MB RAM) | **4.0 MB** | verified live 2026-06-06 |
| `tcc -static hello.c` (direct, no meow) | **4.0 MB** | `scripts/our_tcc_floor.py` (2026-06-06, post heap-backoff fix) |
| **meow agentically clones + compiles + runs** | **4.0 MB** | this test (re-verified 2026-08-10) |

> **2026-06-06 — heap-growth backoff fix.** Fixed by backing off the contiguous
> run length toward `needed` in `PmmOomHandler::handle_oom`. Re-validated live at
> 3 / 4 / 5 MB: `panic=0`, zero crash markers. See `docs/LOW_MEMORY_ENVIRONMENT.md`.
