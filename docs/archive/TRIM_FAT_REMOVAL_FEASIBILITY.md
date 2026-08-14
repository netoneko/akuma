# Removal feasibility: in-kernel SSH/shell/editor, and `libakuma`

Was `proposals/CLEANUP.md` — a two-line "remove the following parts" wishlist
that turned into the feasibility audit below. Both entries came back the same way:
**neither removal was the small thing it looked like**, and the value here is the
measurements and the coupling that say why.

Outcomes since:

- The kernel-side removal **was done** 2026-08-10 — see
  [`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md) for the change itself; this
  doc is the scoping that preceded it and the size numbers it was justified on.
- The `libakuma` entry was **rejected** and stays rejected; the correction below
  is the reason, and the tractable version of it (per-binary dependency
  slimming) is [`TRIM_FAT_SSHD.md`](TRIM_FAT_SSHD.md).
- The process-per-session note in the last bullet was **overturned** — `fork()`
  inherits the fd table, so no new kernel primitive was needed. See
  [`MISSING_SOCKET_MACHINERY.md`](MISSING_SOCKET_MACHINERY.md).

---

## Kernel

* Remove in-kernel shell, editor and ssh via features from devbox-smoltcp profile

  **Findings (2026-08-10, from the sshd-trimming session on `rewrite-sshd-libc`):**

  - **Not a feature flip.** `userspace-sshd` (used by `devbox`/`devbox-smoltcp` today)
    only dead-codes the *startup* branch that spawns the built-in server thread
    (`config::ENABLE_USERSPACE_SSHD`, `src/config.rs:775`). The SSH code itself
    stays linked because `ssh::init_host_key()` runs unconditionally under
    `smoltcp` (`src/main.rs:1404`) and `ssh::server::stats()` feeds the main-loop
    status line (`src/main.rs:1827`). Actually deleting `crates/akuma-ssh` needs
    `#[cfg]`-gating work, not a feature toggle. See
    `docs/reference/build-profiles.md` §"What `userspace-sshd` actually does".
  - **Shell and editor are coupled to it, not just co-located.** `src/shell/mod.rs`
    is written directly against `SshChannelStream` (the SSH channel stream type,
    `src/ssh/protocol.rs:113`) for its `InteractiveRead` impl, and
    `crate::editor::TermSizeProvider` is implemented *for* `SshChannelStream` too
    (`src/ssh/protocol.rs:345`). So "remove ssh" cascades into "shell and editor
    need a different transport first" — confirms this bullet's own grouping is
    right, and why it's one project, not three independent removals.
  - **Real numbers exist** (`docs/reference/build-profiles.md`, measured on
    `size`): in-kernel SSH's own code is 34,853 bytes; the curve25519/ed25519/
    sha2/aes crypto it shares with `kernel-tls` is 63,580 bytes. On `size` that
    crypto is shared with outbound HTTPS, so removing SSH there only nets ≤34 KB.
    **On `extreme-size`, `kernel-tls` is already dropped, so that crypto is
    SSH-only and dead the moment SSH goes too — the real saving there is closer
    to 34 + 62 ≈ 96 KB out of 684 KB text (~14%).** That's the profile where this
    is actually worth doing.
  - **Not free on the other side of the ledger.** The userspace `/bin/sshd`
    replacement has to exist on the ext2 image plus its own runtime heap/thread
    stacks/page tables, and needs `herd` supervision (see
    `userspace/sshd/docs/PROTOCOL_UNDER_LOAD.md` for what that supervision does
    and doesn't cover). `/bin/sshd`'s on-disk size *did* drop this session
    (152,120 → 115,256 bytes, the `fast`/`zeroize` feature fix — see
    `docs/archive/TRIM_FAT_SSHD.md`), so that side of the tradeoff is cheaper
    than `build-profiles.md` currently states, but runtime RSS of `/bin/sshd` is
    still unmeasured — same gap that doc already flagged.
  - **If a process-per-session model for userspace `sshd` is ever pursued**
    (fault-isolating one session's crash from others — see
    `userspace/sshd/docs/PROTOCOL_UNDER_LOAD.md` and
    `userspace/sshd/docs/OPTIONAL_PARALLELISM.md`), handing an already-`accept()`ed
    socket off to a fresh sibling process is **not buildable on existing
    primitives** today: `/proc/<pid>/fd/<n>` only ever exposes fd 0/1 (stdin
    injection into a spawned child, gated by `spawner_pid`); `sys_spawn` has no
    fd-inheritance/file-actions argument; `sendmsg`/`recvmsg` read `msg_control`
    but never interpret it (no `SCM_RIGHTS`). Any of those three would need new
    kernel work — worth knowing before scoping that as "just" a userspace change.

* remove dead code

## Userspace

* ~~analyze if removal of libakuma is feasible, does not look like it's used for much~~

  **Correction (2026-08-10):** this doesn't hold up. `libakuma` is the runtime
  (`#[global_allocator]`, `#[panic_handler]`, entry glue, syscall wrappers) for
  essentially every userspace Rust binary built for `aarch64-unknown-none`:
  `echo2`, `hello`, `herd`, `httpd`, `meow`, `box`, `scratch`, `tcc`, `wavplay`,
  `stackstress`, `termtest`, `allocstress`, `elftest`, `libakuma-tls`, plus
  `crates/akuma-exec`. Removing it would mean replacing the allocator/panic/
  entry-point/syscall layer for the entire userspace tree, not a small cleanup.
  crates.io's `libc` doesn't substitute for it either — it has no bindings for
  `target_os = "none"` (see `docs/archive/TRIM_FAT_SSHD.md`, which hit exactly
  this wall trying it for `sshd` alone). Sizing down individual binaries'
  *dependencies* (e.g. `sshd`'s crypto feature flags, same doc) is a much
  smaller and more tractable version of "libakuma is too big."

