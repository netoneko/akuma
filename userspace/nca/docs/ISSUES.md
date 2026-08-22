# nca issues found running on Akuma

## FIXED 2026-08-22: `execute_bash`'s timeout doesn't kill the child — it orphans it, and the orphan can block later commands

**First seen:** 2026-08-22, running a self-hosted `cargo build` inside the VM
at `/src/github.com/netoneko/akuma-cli`. (`../../../docs/archive/NCA_MISSING_SYSCALLS.md`
§1's old "spawn EFAULT" theory for *why* a build might fail is now marked
stale — that session's real build worked fine, many `cargo build`/`cargo
check` steps in a row, no EFAULT ever seen. This issue is about what happens
after any build genuinely fails or is slow, not about why one particular
build might fail.)

### Root cause

**Correction:** the fix described below was first applied to
`crates/core/src/tools/bash.rs`, which turns out to be **dead code** for
`nca-cli` — the binary actually registers `RuntimeBashTool`
(`crates/runtime/src/bash_tool.rs`, `crates/runtime/src/pty.rs`) via
`Supervisor::create` (`crates/runtime/src/supervisor.rs:159`), confirmed by
the error string (`"Command timed out after {0}s"`, from `PtyError::Timeout`
in `pty.rs`) matching exactly what the live session showed. Same bug, same
fix, applied to both — `core::tools::bash::BashTool` fixed too for
consistency in case it's ever wired up, but `pty.rs` is the one that
mattered.

`crates/runtime/src/pty.rs`'s `PtyManager::exec()` (called from
`RuntimeBashTool::execute()` in `bash_tool.rs`, which parses `timeout_secs`
from the model's own tool call):

```rust
let mut cmd = tokio::process::Command::new("sh");
cmd.arg("-lc").arg(command).current_dir(&self.workspace_root)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());
    // no .kill_on_drop(true)

let output = match timeout(Duration::from_secs(timeout_secs), cmd.output()).await {
    ...
    Err(_) => { /* reports "Command timed out after Ns" */ }
};
```

`tokio::time::timeout` on expiry just **drops** the `cmd.output()` future —
and per tokio's own documented default, dropping a `Child` **without**
`.kill_on_drop(true)` sends it no signal at all. The process keeps running,
completely orphaned from nca's own tracking (nca closes its own read ends of
the pipes, but the child and everything it spawned carries on).

### Why this produced an apparently-unrelated, much worse hang

Sequence observed live:

1. Turn 6: `cargo build --release 2>&1` — legitimately slow (or blocked on
   the self-host toolchain having its own problems), hits nca's 30s timeout,
   reports `"Command timed out after 30s"`. **The real `cargo build
   --release` process is not killed and keeps running in the background.**
2. Turn 7 (three seconds later): a plain `cargo build`, no `2>&1`, in the
   **same** workspace. Cargo takes an exclusive lock on its own `target/`
   directory for the duration of a build. The still-running orphan from
   step 1 already holds that lock. This second invocation blocks waiting for
   it — a real, if silent, resource contention that has nothing to do with
   anything this second command itself did.
3. Observed for 900+ seconds: no `cargo`/`rustc` process anywhere in `ps`
   (the orphan from step 1 presumably finished or died on its own by then,
   quite possibly on the pre-existing, separately-tracked self-host
   toolchain issues), yet nca still held the pidfd and both pipes open the
   entire time, and the whole session's message queue was blocked.

Step 3 is the part not fully explained by the orphan alone — an orphaned
process contending for a lock should still let nca's own **independent**
30s timeout fire again for the second call, the same way it did for the
first. Live-tested with `ncaprobe timeoutleak` (below): the core leak is
confirmed (`pid=77 still alive=true` via `kill(pid, 0)`, ~2s after nca's own
reported timeout), but the probe's *contention* half (does a second command
block on the first's held lock?) turned out to be unmeasurable on Akuma for
an unrelated reason — **`sys_flock` isn't implemented anywhere in this
kernel** (`grep -rn "fn sys_flock" src/syscall/` — zero hits), so `flock`
never actually blocks call B regardless of what call A is doing, in both the
FD form (`flock 9`) and the file form (`flock FILE -c PROG`). Whether cargo's
own real build-lock mechanism uses `flock()` specifically (in which case it
would have the same problem — never actually contending, so the *lock*
theory for why turn 7 sat for 900s+ might be wrong too) or something else
(a lockfile + `fcntl` `F_SETLKW`, which Akuma may or may not implement — not
checked) is unresolved. `.kill_on_drop(true)` removes the actual proven bug
(the orphan) regardless of which lock mechanism, if any, explains step 3.

### The fix

One line, `crates/core/src/tools/bash.rs`:

```rust
.stdout(std::process::Stdio::piped())
.stderr(std::process::Stdio::piped())
.kill_on_drop(true);
```

Applied directly to the submodule working tree (uncommitted — this repo's
convention per `../README.md` is patches as commits on the personal fork
branch; not committed here per this session's own no-auto-commit rule).
Verified: `nca-core` and the full `nca-cli` binary both cross-build clean
(`aarch64-unknown-linux-musl`, the exact flags in `../README.md`'s ## Build
section) with the change, and `bootstrap/bin/nca` was rebuilt from it
(10 913 912 bytes stripped).

### New regression probe: `ncaprobe timeoutleak [--fixed]`

Replicates `bash.rs`'s exact pattern (`tokio::time::timeout` wrapping
`Command::output()`, piped stdout+stderr): call A holds an `flock` for 8s
against a 2s timeout (mirroring cargo's own build lock held across a
timed-out command); after A's own timeout fires, checks via `kill(pid, 0)`
whether A is actually still alive; then call B wants the *same* lock,
isolating "does the orphan cause contention" from "does B's own,
independent timeout still fire despite that contention." Without `--fixed`
this should show A `LEAKED`; with `--fixed` it should show A `correctly
killed`. **Not yet run against a live kernel** — written and cross-build
verified this session, but the VM was killed before it could be booted
against it. Run it on the next boot:

```bash
# host
userspace/ncaprobe/build-musl.sh --serve
# guest
curl -s -o /tmp/ncaprobe http://10.0.2.2:8899/ncaprobe && chmod +x /tmp/ncaprobe
/tmp/ncaprobe timeoutleak            # reproduces the leak (pre-fix bash.rs shape)
/tmp/ncaprobe timeoutleak --fixed    # shows the fix working
```

### Also flagged by the user, same session — separate, still open

- **A single stuck tool call took the whole UI down**, not just that one
  tool call — no new messages could be queued. Likely just a **consequence**
  of the bug above (an orphan holding a lock, or a leaked reader task
  competing for nca's small worker pool) rather than an independent bug —
  worth re-checking once the fix has been run live, but not otherwise
  investigated further.
- **The user should be able to write a new message to the model while a tool
  call is pending**, rather than being blocked until it resolves or times
  out. Independent UX/architecture ask, not a bug report — unaddressed here.

### Ruled out this session (kept for anyone chasing a *different* recurrence)

- `ncaprobe tokio` (three trivial commands, none writing to stderr) — basic
  dual-pipe-plus-pidfd mechanism (`docs/archive/TOKIO_PIPE_EPOLL_HANG.md`'s
  2026-08-17 fix) is intact for a process that exits voluntarily and
  quickly. Not a strong control in hindsight — it never wrote real content
  to a separate stderr pipe, so it couldn't have caught a content-dependent
  variant either way.
- Four `nettest-reqwest` network-stack probes (plaintext/TLS ×
  mid-stream-drop/idle-pool-reuse) — not the CLOSE_WAIT/socket-EOF class of
  bug; a `CLOSE_WAIT` socket observed during the same investigation was an
  unrelated red herring.
- The kill/exit-notify path (`sys_kill` → `kill_process_with_signal` →
  `publish_child_exit`) — initially suspected, but reading it end to end
  shows `sig=9` (what `Child::kill()` sends, and irrelevant here anyway
  since the bug is that nothing gets sent at all) takes an unconditional
  hard-kill path with unconditional, idempotent channel notification. Not
  the mechanism.
- The generic `PipeRead` arm of `sys_read` (`src/syscall/fs.rs`) — correctly
  honours `O_NONBLOCK` and re-arms the epoll edge on every drain/EAGAIN,
  symmetric across stdout/stderr with no index-dependent special-casing.

### Follow-up: is the timeout configurable, and can the user type while a tool runs?

Both asked and answered the same session:

- **Timeout, before this fix:** only per-tool-call, via the model's own
  `timeout_secs` argument on the `execute_bash` call — nothing external. The
  30s figure was a hardcoded `unwrap_or(30)` fallback in `pty.rs`, not
  configurable any other way.
- **Timeout, after this fix:** default raised to **120s**, and now a real
  external config: `NcaConfig::tools.bash_timeout_secs`
  (`crates/common/src/config.rs`, `ToolsConfig`) — settable in
  `config.toml` (global or workspace-local, same layering as every other
  config section) or via the `NCA_BASH_TIMEOUT_SECS` env var, following the
  exact pattern `web.timeout_secs`/`NCA_WEB_TIMEOUT_SECS` already used. The
  model's own per-call `timeout_secs` still overrides this when present;
  this only changes the fallback. Threaded through
  `Supervisor::create` → `RuntimeBashTool::new` → `PtyManager::exec`.
- **Typing while a tool call is pending:** already fully implemented before
  this session touched anything — `app.rs`'s `Submit` handler echoes the
  message into the transcript immediately and queues it behind the current
  turn (`cmd_tx`, capacity 64), and `state.rs`'s `MessageReceived` handler
  dedups against the real event once it's actually processed
  (`pending_own_submits`). It looked broken only because `run_turn` was
  stuck *forever* — queuing behind an infinite wait is indistinguishable
  from queuing being broken. Now that the timeout actually bounds the wait,
  this should work as designed; not independently re-verified live.

### Aside: `sys_flock` is not implemented in Akuma at all

Found while building the `timeoutleak` probe's contention check (`ncaprobe
timeoutleak`): `grep -rn "fn sys_flock" src/syscall/` returns nothing. Not
investigated further, and not necessarily related to this issue (unclear
whether cargo's own build lock uses `flock()` or something else), but worth
someone's attention if any tool that relies on real file locking behaves
oddly on Akuma.
