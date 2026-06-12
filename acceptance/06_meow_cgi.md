# Acceptance: meow handles an agentic task via CGI under httpd

Verify that meow executes the same clone → compile → run task as test 05,
but triggered by an HTTP POST request instead of a direct SSH command.
httpd runs under herd supervision (boxed mode requires release kernel with
`sc-containers`; extreme kernel is used with `boxed=false`).

The POST body is the prompt. meow detects CGI context from the
`REQUEST_METHOD` environment variable injected by httpd, prints CGI headers,
then runs non-interactively.

Ollama must be running on the host before starting the VM:
```bash
ollama serve   # if not already running
```

---

## Preparation (host)

### 1. Build the extreme kernel, httpd, and meow

```bash
scripts/build_extreme_size.sh
userspace/build.sh --httpd-only
userspace/build.sh --meow-only
```

### 2. Populate disk

```bash
scripts/populate_disk.sh
```

This puts the new `httpd`, `meow`, and the herd config
(`/etc/herd/enabled/httpd.conf`) on the disk so httpd auto-starts on boot.

### 3. Start the pkg server (needed for pkg install inside VM)

```bash
cd bootstrap && python3 -m http.server 8000 &
cd ..
```

---

## Boot (extreme kernel)

```bash
ELF=target/aarch64-unknown-none/extreme-size/akuma
MEMORY=16M INSTANCE=0 bash scripts/cargo_runner.sh "$ELF" 2>&1 | tee 4mb_cgi.log
```

Poll for SSH:
```bash
until grep -q "\[SSH Server\] Listening" 4mb_cgi.log 2>/dev/null; do sleep 2; done
```

---

## SSH helper

```python
import subprocess, re

def ssh(cmd, timeout=60):
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

### 4. Install meow, scratch, and tcc runtime

```python
rc, out, err = ssh("pkg install meow", timeout=60)
assert rc == 0, f"pkg install meow failed: {out} {err}"
print("meow installed")

rc, out, err = ssh("pkg install scratch --as=/bin/git", timeout=60)
assert rc == 0, f"pkg install scratch failed: {out} {err}"
print("scratch installed")

rc, out, err = ssh("apk add musl-dev && busybox tar xf /archives/libtcc1.tar -C /", timeout=60)
assert rc == 0, f"tcc runtime install failed: {out} {err}"
print("tcc runtime ready")
```

### 5. Expose meow as a CGI binary

```python
rc, out, err = ssh("ln -sf /bin/meow /public/cgi-bin/meow", timeout=10)
assert rc == 0, f"symlink failed: {out} {err}"
print("meow linked into cgi-bin")
```

### 6. Confirm httpd is running under herd

```python
rc, out, err = ssh("herd status", timeout=10)
print(f"herd status: {out}")
assert "httpd" in out, f"httpd not listed in herd status: {out}"
```

### 7. Clean up any stale state from prior runs

`/tmp` is wiped by `populate_disk.sh` on every repopulation, so only the cloned
repo needs clearing here.

```python
ssh("rm -rf /akuma-playground", timeout=15)
print("stale state cleared")
```

---

## Test CGI from host

Port 8080 is already forwarded by `cargo_runner.sh` (INSTANCE=0).

```python
import urllib.request, time

PROMPT = (
    "Clone https://github.com/netoneko/akuma-playground.git using scratch "
    "(run: scratch clone https://github.com/netoneko/akuma-playground.git from /), "
    "then compile /akuma-playground/hello.c with tcc: "
    "tcc -static -B /usr/lib/tcc -o /tmp/hello_c /akuma-playground/hello.c, "
    "then run /tmp/hello_c. Run commands one by one using shell tool."
)

print(f"POSTing prompt to http://localhost:8080/cgi-bin/meow (may take 1-3 min)...")
t0 = time.time()
req = urllib.request.Request(
    "http://localhost:8080/cgi-bin/meow",
    data=PROMPT.encode(),
    method="POST",
    headers={"Content-Type": "text/plain", "Content-Length": str(len(PROMPT))},
)
with urllib.request.urlopen(req, timeout=300) as resp:
    body = resp.read().decode(errors="replace")
    status = resp.status
elapsed = time.time() - t0

print(f"HTTP {status} in {elapsed:.1f}s ({len(body)} bytes)")
print(body[:3000])
```

### 8. Verify

```python
assert status == 200, f"Expected HTTP 200, got {status}"
assert "Hello" in body, f"Expected 'Hello' in CGI response, got:\n{body[:500]}"

# Belt-and-suspenders: run the compiled binary directly
rc2, run_out, _ = ssh("/tmp/hello_c", timeout=30)
assert rc2 == 0 and "Hello" in run_out, \
    f"Binary /tmp/hello_c failed: rc={rc2} out={run_out}"

print("PASS")
```

---

## Expected output

The HTTP response body contains meow's streaming output including:
```
Hello, World!
```
(or any `Hello` variant from hello.c in akuma-playground.)

---

## Failure modes

| Symptom | Diagnosis |
|---|---|
| VM never reaches SSH | boot OOM at this memory — try `MEMORY=32M` |
| `herd status` missing httpd | herd not auto-started; check serial log for `[herd]`; verify `/bin/herd` exists |
| httpd not in herd status | `/etc/herd/enabled/httpd.conf` not on disk; re-run `scripts/populate_disk.sh` |
| HTTP 504 Gateway Timeout | meow idle > 60 s; check `ollama serve` is running and reachable at 10.0.2.2:11434 |
| HTTP 200 but body is empty | CGI header boundary not found; check `herd log httpd` via SSH |
| "Hello" missing | Model didn't cooperate; retry after clearing stale clone below |
| Stale `/akuma-playground` breaks clone | `ssh "rm -rf /akuma-playground"` then retry POST |
| Boxing has no effect | Extreme kernel lacks `sc-containers`; `boxed=false` is correct for this test |

---

## Boxing on release kernel

To test with namespace isolation on the release kernel:

1. Edit `bootstrap/etc/herd/enabled/httpd.conf`: change `boxed = false` → `boxed = true`
2. Rebuild: `cargo build --release`
3. Repopulate disk: `scripts/populate_disk.sh`
4. Boot with release kernel: `MEMORY=64M bash scripts/cargo_runner.sh target/aarch64-unknown-none/release/akuma`

The `sc-containers` feature is in `[features] default` so it's present in the
release build without any extra flags.
