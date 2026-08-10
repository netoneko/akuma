# Acceptance: nca installed via `cargo install`, run against mlx/ollama

Verify that `nca` — a normal, unmodified upstream Rust CLI (tokio + reqwest +
ratatui, cross-compiled with a plain `cargo install`, no Akuma-specific source
patches) runs correctly on Akuma and can drive the same agentic
compile-and-run task the `meow` acceptance tests use.

Unlike `meow`/`tcc`/`scratch`, which are linked against `libakuma` (Akuma's own
syscall-numbering runtime), `nca` is built for the plain
`aarch64-unknown-linux-musl` target — this is the proof that a binary built
the ordinary way any Rust user would build it also runs on Akuma, not just
first-party binaries authored against this repo's own libc replacement.

Repo (upstream, real): `https://github.com/madebyaris/native-cli-ai.git` — the
same URL used in `docs/archive/DEVBOX_ISSUES.md`.

---

## Preparation (host)

### 1. Install nca straight from git via `cargo install`

```bash
rustup target add aarch64-unknown-linux-musl

export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc
export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
export AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar

cargo install --git https://github.com/madebyaris/native-cli-ai.git \
    --target aarch64-unknown-linux-musl \
    --bin nca \
    --no-default-features \
    --root /tmp/nca_install \
    nca-cli
```

`nca-cli` is a bare positional argument here, not `-p`/`--package` — `cargo
install` selects the workspace member to install that way (`-p` is a
`cargo build`/`cargo run` flag; `cargo install` doesn't have it, since without
`--path`/`--git` the positional already means "crate name").

`--no-default-features` drops the `clipboard` feature (`arboard`/`image` —
no display server over SSH anyway). Nothing else is touched: no vendored
source, no submodule, no Akuma-tuned `RUSTFLAGS` — just `cargo install`
against the real upstream repo, cross-compiled. musl targets are statically
linked by default, so the output binary needs no extra linker flags.

### 2. Stage the binary and its config onto the disk

```bash
mkdir -p bootstrap/bin
cp /tmp/nca_install/bin/nca bootstrap/bin/nca
```

`bootstrap/root/.nca/config.toml` is already checked in — it lands at
`/root/.nca/config.toml` on the disk (nca's global config path, read on
every invocation since sshd logs in as root):

```toml
[provider]
default = "custom"

[provider.custom]
compatibility = "openai"
base_url = "http://10.0.2.2:8080"
api_key = "mlx"
model = "mlx-community/Qwen-AgentWorld-35B-A3B-oQ4"
temperature = 0.7

[provider.openai]
base_url = "http://10.0.2.2:11434"
api_key = "ollama"
model = "qwen3:4b"
temperature = 0.7
```

nca only ships four named providers (`minimax`/`anthropic`/`openai`/
`openrouter`) plus exactly one `[provider.custom]` slot — no equivalent of
meow's `[provider:mlx]` / `[provider:ollama]` sections, so mlx (the intended
default) has to be the `custom` slot, and ollama piggybacks on the `openai`
slot since its `/v1/chat/completions` endpoint is genuinely OpenAI-compatible.
`default = "custom"` makes mlx the default without any env var; switching to
ollama at runtime is just `NCA_DEFAULT_PROVIDER=openai nca ...` (no base
URL/model/key overrides needed — those are already in the `[provider.openai]`
table above).

### 3. Build the kernel + userspace and populate the disk

```bash
cargo build --release
cd userspace && ./build.sh && cd ..
./scripts/create_disk.sh
./scripts/populate_disk.sh
```

`populate_disk.sh` (full run, no flags) copies the whole `bootstrap/` tree
onto the disk verbatim, so `bootstrap/bin/nca` and
`bootstrap/root/.nca/config.toml` need no special-casing.
`bootstrap/tmp/hello.c` lands at `/tmp/hello.c` the same way it does for the
`meow` tests.

### 4. Start the LLM backend(s) on the host

```bash
ollama serve   # if not already running

# and/or, for the mlx path:
mlx_lm.server --model mlx-community/Qwen-AgentWorld-35B-A3B-oQ4 --port 8080
```

Both are reachable from the guest at `10.0.2.2:11434` / `10.0.2.2:8080` — the
same two endpoints `bootstrap/etc/meow/config` already points `meow` at
(`[provider:ollama]` / `[provider:mlx]`).

### 5. Boot

```bash
MEMORY=512 cargo run --release 2>&1 | tee 13_nca_cargo_install.log
```

Poll for boot (never call `wait` on the QEMU process):

```bash
until grep -q "\[SSH Server\] Listening" 13_nca_cargo_install.log 2>/dev/null; do sleep 2; done
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

### 6. Verify the binary landed

```python
_, out, _ = ssh("ls -l /bin/nca")
assert "nca" in out, f"nca missing from /bin: {out}"
print(out)
```

### 7. Confirm the staged config is in effect (mlx, the default) and run nca's own health check

```python
_, out, _ = ssh("cat /root/.nca/config.toml")
assert 'default = "custom"' in out, f"config missing/wrong: {out}"

rc, out, err = ssh("nca doctor", timeout=30)
print(f"rc={rc}\n{out}\n{err}")
```

### 8. One-shot task against mlx (the default — no env vars needed)

```python
TASK = (
    'nca -p "Compile /tmp/hello.c with tcc to /tmp/hello_c and run it. '
    'Report the output of the compiled binary." '
    '--no-tui --permission-mode bypass-permissions --max-turns 5'
)

rc, out, err = ssh(TASK, timeout=180)
print(f"rc={rc}\nout:\n{out}\nerr:\n{err}")
```

### 9. Verify

```python
assert "Hello" in out or "Hello" in err, \
    f"Expected 'Hello' in nca output, got:\n{out}\n{err}"
print("PASS (mlx, default)")
```

### 10. Repeat against ollama (optional — proves the second backend too)

Same task, just switching the provider for this one invocation via the env
var — no config edits, no base-url/model/key overrides:

```python
rc, out, err = ssh("NCA_DEFAULT_PROVIDER=openai " + TASK, timeout=180)
assert "Hello" in out or "Hello" in err, \
    f"Expected 'Hello' in nca output, got:\n{out}\n{err}"
print("PASS (ollama)")
```

`mlx-server` has a known finish-reason-formatting quirk that silently drops
tool calls for `meow` specifically (`userspace/meow/docs/MLX_SERVER_TOOL_CALLS.md`)
— `nca` uses a real JSON parser (serde), so it isn't expected to hit the same
bug, but if step 8 (mlx) comes back empty with a fast, low-token response,
that's the first thing to check.

---

## Expected output

```
Hello, Akuma!
```

(or whatever `hello.c` in the tree prints — same output shape as the `meow`
tests, from both step 9 and step 10.)

---

## Failure modes

| Symptom | Diagnosis |
|---|---|
| `cargo install` fails to link | musl cross toolchain (`aarch64-linux-musl-gcc`) not on `PATH`, or env vars in step 1 not exported in the same shell as the `cargo install` call |
| `nca` missing from `/bin` on the guest | `bootstrap/bin/nca` wasn't populated before `populate_disk.sh`, or the disk wasn't recreated |
| `nca doctor` reports no provider configured | `PROVIDER_ENV` not exported in the same SSH command — each `ssh()` call is a fresh shell, so the `export` and the `nca` invocation must be one command string |
| `nca -p ...` returns instantly with empty output | model emitted a tool call nca didn't parse — see the mlx note above; also check `OPENAI_MODEL` matches a model actually loaded on the host server |
| VM never reaches SSH | boot OOM — raise `MEMORY`; nca (tokio + reqwest + ratatui) is heavier than the `libakuma`-native binaries, so 512 MB is a starting point, not a floor |
| `Connection refused` to `10.0.2.2:11434`/`:8080` | `ollama serve` / `mlx_lm.server` not running on the host, or not bound to all interfaces |

---

## Background

- `docs/archive/DEVBOX_ISSUES.md` — source of the upstream repo URL; the same
  clone target that first surfaced an unrelated devbox `git clone` deadlock.
- `acceptance/archive/09_nca_docker_clone_compile_run.md` — the previous nca
  proof, off-kernel (Docker + `disk.img` chroot, no Akuma kernel at all). This
  is the first time nca runs directly on Akuma.
- `userspace/meow/Cargo.toml`'s `linux-net` feature (`libakuma/linux-abi`) —
  why `meow`'s *default* build is not, by itself, proof that Akuma runs stock
  Rust/Linux binaries: `meow` normally links `libakuma` directly, and only
  opts into the standard Linux syscall ABI for the rump-stack demo
  (`acceptance/11_netbsd_rumpkernel_irc.md`). `nca` never links `libakuma` at
  all — it doesn't know Akuma exists.
- `bootstrap/etc/meow/config` — the mlx/ollama host endpoints this doc reuses.
- `docs/archive/TRIM_FAT_PROFILES_AND_ACCEPTANCE.md` — why `acceptance/` is a
  short, curated set (05, 10, 11) rather than every milestone playbook; this
  doc is `13`, one past the archived `12_multikernel_demo.md`.
