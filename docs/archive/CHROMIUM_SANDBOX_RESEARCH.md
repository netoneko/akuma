# Headless Chromium sandbox research

Research branch: `research/chromium-sandbox`. Host: Akuma amd64, kernel
`0.0.9 b02b61ab-release-smp-shared`; installed Alpine Chromium
`152.0.7977.82`. The existing kami launcher passes `--no-sandbox`.

## Findings

Chromium's Linux sandbox combines syscall filtering with a namespace or setuid
sandbox. Akuma implements seccomp BPF and its actions, including SIGSYS;
the existing tests cover Codex's filter, not Chromium's. See [SECCOMP.md](SECCOMP.md)
and [Chromium's sandbox documentation](https://chromium.googlesource.com/chromium/src/+/main/docs/linux/sandboxing.md).

Akuma's private mount namespace supports bubblewrap, but user namespace UID and
GID mappings are accepted without taking effect, PID namespace flags do not
create separate PID numbers, and network namespace behavior differs from Linux.
See [BWRAP_ON_BOXES.md](BWRAP_ON_BOXES.md) and
[`private_ns.rs`](../../crates/akuma-syscalls-glue/src/private_ns.rs). These are
potential compatibility and isolation gaps, not yet proven causes of a Chromium
failure. The installed `/usr/lib/chromium/chrome-sandbox` is setuid root; its
path has not been tested.

## Tests and next steps

A bounded root invocation using `--headless --disable-gpu
--disable-background-networking --user-data-dir=/tmp/chromium-sandbox-research-root
--dump-dom 'data:text/html,<title>SandboxProbe</title>'` exited 1 and printed
`Running as root without --no-sandbox is not supported.` Run Chromium as a
non-root user for the next test.

An attempted non-root invocation using `setpriv --reuid 65534 --regid 65534
--clear-groups` was interrupted by the code-mode host connection failing; it is
unknown whether the command started. Check for a surviving process before
retrying. Capture stderr, exit status, and failing syscalls, then inspect actual
renderer sandbox state and verify isolation rather than relying only on startup.
The runner repeatedly disconnected during this investigation, so no non-root
Chromium sandbox result is available yet.
