# Runbooks

Runbooks are action-first: do these steps, expect to see this output. No
narrative, no investigation story (that lives in `../archive/`).

## Triage matrix

Start from the symptom or task on the left.

| You are... | Read |
|---|---|
| Building the devbox image | [`build-devbox.md`](build-devbox.md) |
| Booting a VM and connecting via SSH | [`boot-and-connect.md`](boot-and-connect.md) *(Phase 3)* |
| Debugging the devbox (SSH down, cargo crash, 100% CPU) | [`debug-devbox.md`](debug-devbox.md) |
| Recovering a wedged / hung / 100%-CPU VM | [`recover-wedged-vm.md`](recover-wedged-vm.md) |
| Debugging networking (either stack) | [`debug-network.md`](debug-network.md) *(Phase 3)* |
| Debugging OOM / panics / allocation failures | [`debug-memory-oom.md`](debug-memory-oom.md) *(Phase 3)* |
| Debugging a boot hang | [`debug-boot-hang.md`](debug-boot-hang.md) *(Phase 3)* |
| Debugging SSH latency / echo lag | [`debug-ssh-latency.md`](debug-ssh-latency.md) *(Phase 3)* |
| Self-hosting (compiling the kernel inside Akuma) | [`selfhost-kernel-build.md`](selfhost-kernel-build.md) *(Phase 3)* |
| Adding an apk package to the devbox | [`add-apk-package.md`](add-apk-package.md) *(Phase 3)* |
| Adding a `sc-*` kernel feature | [`add-syscall-feature.md`](add-syscall-feature.md) *(Phase 3)* |

## Conventions

- Each runbook ends with a **Verify** section: the exact output that confirms
  success.
- Commands are copy-pasteable. Env knobs are called out explicitly.
- "Background" footers link to `../archive/` originals for the investigation
  story behind a procedure.

## Authoring a new runbook

1. Name it after the *task* or *symptom*, not the subsystem
   (`debug-devbox.md`, not `rump.md`).
2. Lead with the one-paragraph "when to use this".
3. Steps are numbered, present-tense, imperative.
4. End with **Verify** - the log lines / command output / SSH result that means
   it worked.
5. Add a row to the triage matrix above.
