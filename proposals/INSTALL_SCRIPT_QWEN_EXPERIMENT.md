# A fair experiment: can a local Qwen install Akuma with `install.sh`? (proposal, 2026-11-04)

**Status: design, not run.** What exists now (`install-i1b` iterating on `install.txt` between Qwen attempts, on a host that cannot
boot a guest) is **development**, and the doc `docs/archive/BUILDING_KERNEL_WITH_LOCAL_AI_PAIN_AND_SUFFERING.md` records it as
such. This file says what a result must look like before anyone writes "the Qwen passes".

## Question

With **only** `install.txt` and the URL of `install.sh`, does Qwen3-8B Q4_K_M, served locally at ~4.6 generated tok/s,
reach `AKUMA_READY=1` and run `busybox uname -a` in the guest, unattended, **consistently**?

## What is wrong with the current setup

| flaw | why it makes a pass meaningless |
|---|---|
| the agent that edits `install.txt` also runs and judges the attempts | tuning on the test set: a pass shows the text fits one transcript |
| the host (ryzen/Akuma) has no QEMU, no KVM, no `ssh-keygen` | the ceiling is `check` + the stages before `boot`; `AKUMA_READY` is unreachable |
| the Qwen's CPU (trashcan) is also where a guest would boot | a booting guest and a 4-thread model server fight for 4 cores; timeouts look like model failures |
| one run, three retries | a coin that came up heads once |
| success is what the Qwen says it did | a model that claims `AKUMA_READY` without it counts as a pass |
| no control | a failure could be the model, the text, or the script, and nothing separates them |

## Design

### 1. Two phases, never mixed
- **Development**: free to edit `install.txt` and `install.sh` after any failure. Attempts are logged, never counted.
- **Frozen test**: the commit, `sha256(install.txt)`, `sha256(install.sh)`, `lock.txt`, the model file hash, the
  `llama-server` binary and its flags, the goose version and the task prompt are written into `trial/MANIFEST` **before**
  trial 1 and **nothing changes** until the last trial ends. A bug found mid-test is a failed trial; fix it, bump the
  version, and start a **fresh N** (old trials are kept, not reused).

### 2. Fixed inputs
- Model: `qwen3-8b-q4_k_m.gguf` (`54fffa05…cccd`), `llama-server-hsw -t 3 -c 16384 --no-mmap --reasoning-budget 0`
  (8192 is likely too small for an agent that reads script logs; check KV memory: ~1.1 GB per 8k at f16).
- Sampling: Qwen's published non-thinking settings (temperature 0.7, top-p 0.8, top-k 20), **seed = trial number**, written down.
- Harness: one goose version, `GOOSE_MODE=auto`, throwaway config dir per trial. The task prompt is one file, hashed:
  the URL, the goal ("reach AKUMA_READY=1 and run `busybox uname -a`"), nothing about the repo or the host.
- No human, no supervisor message, no steer during a trial.

### 3. The host must be able to finish
Run trials on a **Linux x86_64 host with KVM and QEMU, separate from the Qwen server**: ryzen booted into Pop (to verify that
QEMU and `/dev/kvm` are there; installing them is a `sudo` action the user must approve), Qwen on trashcan over the LAN.
Variants, reported separately:
- **V0, deps present**: the happy path. Headline number.
- **V1, `ssh-keygen` absent**: the model must read `check`, use `--key`, and not stop to ask. Held out: **never used to tune** V0's text.
- **V2 (later), Apple Silicon Mac** after I4. Held-out host.
Each trial starts from an empty `--dir`, a fresh user home and no leftover VM, pidfile or port; the harness resets and verifies that.

### 4. A trial
Budget: **40 tool calls, 90 minutes**, one goose session, no retry. It ends when the harness sees `AKUMA_READY`, when the
model stops (including **asking the user a question**, which counts as a failure of kind "asked"), or when a budget runs out.

### 5. Success is judged by the harness, not the model
Pass = the harness itself finds a live VM pid in `state/`, runs `install.sh ssh 'busybox uname -a'` and sees `Akuma`,
**and** the transcript contains no `sudo`, no package-manager call, and no write outside `--dir`. A claim of success
without that is recorded as **false success** and is a fail.

### 6. Measure per trial
Furthest stage reached (`check, clone, build, fetch, key, image, boot, wait`), exit codes seen, tool calls, wall time,
prompt and generated tokens, whether it asked, whether it used a forbidden command, and a failure label:
`model` (loop, hallucinated flag, gave up), `text` (install.txt ambiguous), `script` (bug), `env` (network, disk),
`infra` (Qwen server down). **`infra` trials are discarded and rerun**, the rule is declared now so it cannot be bent later.

### 7. Controls (same protocol)
- **Ceiling**: GLM-5.3-flash. If it fails, the text or the script is broken, not the Qwen.
- **Oracle**: `sh install.sh up` by hand. If it fails, the model is not the problem.
- **Floor**: the URL and the goal but **no** `install.txt`. Shows what the text is worth.

### 8. Analysis, fixed in advance
- N = 10 per condition; report k/10 with a Wilson 95 % interval (10 trials is coarse: 8/10 is [49 %, 94 %]) and the stage funnel.
- **"Passes consistently" = at least 8/10 on V0 and at least 6/10 on held-out V1.**
- A text or script change is justified only by **two independent failures with the same cause**, never by one transcript.
- Report failures by label, not only the pass rate. A pass rate with the failures hidden is the thing this document exists to stop.

## Cost, honestly
At ~4.6 gen tok/s and ~10 prompt tok/s a 30-turn trial could take 30 minutes to 2 hours (llama.cpp's prompt cache helps; this is
unmeasured). N = 10 sequential trials per condition is **a day or more of box time per condition**; the Qwen is a single
shared resource, so conditions cannot run in parallel. Measure trial 1 before committing to N. A screening pass of N = 5 on
V0 is acceptable **during development**, never as the reported result.

## Steps
1. Verify ryzen-on-Pop has `qemu-system-x86_64` and `/dev/kvm`; ask the user before any `apt`.
2. `scripts/kvm/goose/qwen_install_trial.py`: reset the host, write `MANIFEST`, run goose with the frozen prompt, enforce the
   budget, judge success, write `trial/NNN/{transcript,result.json}`. No model-facing logic in it.
3. Run the oracle and the GLM ceiling first (hours, not days) to prove the text and script are not the blocker.
4. Freeze, then V0 x10, then V1 x10, then the floor. Archive the transcripts and a short analysis in `docs/archive/`.

## Background
`proposals/INSTALL_SCRIPT.md`; `docs/archive/BUILDING_KERNEL_WITH_LOCAL_AI_PAIN_AND_SUFFERING.md`;
`scripts/kvm/goose/wait_qwen.py`; the Qwen service on trashcan (`/etc/herd/available/qwen.conf`).
