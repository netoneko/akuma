# AArch64: Claude Code (Bun) segfaults at 0x2040000000 on interactive start

**Status: OPEN — reproduced 3/3, not investigated.** Found 2026-10-19 while trying to A/B the
`/login` input bug (`AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`) on the AArch64 kernel.

## Symptom

`claude` (2.1.303, `@anthropic-ai/claude-code-linux-arm64-musl`, Bun 1.4.4) started on a pty
from an interactive `sh -i` runs a spinner for ~10 s ("Checking connectivity"), then:

```
panic: Segmentation fault at address 0x2040000000
oh no: Bun has crashed. ...
```

about 22 s after launch (Bun's own `Elapsed: ~22000ms`), before any login screen. Same
address every run. `claude -p hi` (no TUI) does **not** crash and reaches the auth check, so
networking, DNS and TLS work.

Kernel side: `[signal] Process N (/usr/local/bin/claude) terminated by signal 11 (default
action)` and nothing else — no fault address, FAR or region dump. The
`[tty] raw->canonical by pid=N (HeapHelper|ollector Thread) fd=0 via TCSETS` line just before
it is Bun's crash handler restoring the terminal, not the cause.

`0x2040000000` is ~129 GiB: looks like a reserved heap arena (mimalloc/JSC) that was
`PROT_NONE`/lazy and then touched. Unchecked guess: the lazy-region / `mprotect` family
(`project_lazy_region_mprotect`, `project_mprotect_tlb_asid_bug`). Not verified.

## Reproduce

Lima (`limactl start fc`), kernel = AArch64 release build, `scripts/lima_aarch64_run.sh`:

```sh
# stage: copy disk.img into Lima, then with debugfs -w:
#   /usr/local/bin/claude (arm64-musl binary from npm), /etc/passwd, /etc/group,
#   /etc/hosts ("127.0.0.1 localhost"), /root/.claude/settings.json
DISK=/tmp/cc/disk.img MEMORY=2048M SMP=2 INSTANCE=4 sh scripts/lima_aarch64_run.sh
XTERM=xterm-ghostty scripts/utils/ssh_pty_steps.py 2622 'exec /bin/sh -i' 60 \
  '~2' 'cd /tmp; HOME=/root /usr/local/bin/claude\r' '~25'
```

Kill orphaned claudes between runs (closing ssh does not end them).

## Side findings

- Without `/etc/hosts` the connectivity check timed out ("Connection to api.anthropic.com timed
  out after 10 seconds"); with it, `-p` reaches the API layer. Cause not examined (AAAA/`EAFNOSUPPORT`
  is a candidate: there is no IPv6, `networking.md` § address families).
- The AArch64 disk image has no Claude, `/etc/passwd` or `/etc/hosts` by default.

## Next

Get the fault address/VMA state from the kernel on SIGSEGV (EL0 data abort: FAR, ESR, the region
covering `0x2040000000`), then compare with `mmap`/`mprotect` calls on that range in `[PSTATS]`/
a syscall trace. Until it runs, AArch64 cannot answer the `/login` input question.
