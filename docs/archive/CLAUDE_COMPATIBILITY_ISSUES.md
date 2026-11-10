# Claude Code compatibility issues

Running Claude Code on Akuma itself. Each entry is an open problem unless it says otherwise.
Started 2026-11-10 on branch `akuma-convergence-research`.

## 1. Bash tool fails: `EIO: i/o error, mkdir '<scratchpad>/tasks'`

### Symptom

Every Bash tool call fails before the command starts:

```
EIO: i/o error, mkdir '/tmp/claude-0/-src-github-com-netoneko-akuma/<session-id>/tasks'
```

Seen with `git pull`, `ls`, and a command that itself began with `mkdir -p` of the same path. The
`mkdir -p` inside the command made no difference, because the failing `mkdir` is not in the command.
The tool creates its per-session `tasks` directory first, and that call returns `EIO`. The Read and
Write tools kept working, so file access is intact and only the Bash tool is dead.

### What is known

- Plain `mkdir` and `mkdir -p` typed in the user's terminal on the same box work.
- The failing call is Node's `fs.mkdir`, probably `recursive: true`, issued by the Claude Code
  process. It is not the `mkdir` binary.
- The tool reports `Linux 0.0.9` as its OS version. That string is Akuma's own `uname` and matches no
  real Linux release (real versions were 0.01, 0.02, 0.03, 0.10, 0.11, 0.12). It is not the cause, but
  it is worth knowing that software sees a version that predates anything it supports.
- Retrying did not help, so it is not a transient fault.

### Suspects (unverified)

1. **Recursive `mkdir` sequence.** Node does `mkdirat`, and on failure a `stat`/`newfstatat` to
   distinguish `EEXIST` from a real error. If Akuma returns `EIO` where Linux returns `EEXIST` or
   `ENOTDIR` for some intermediate path, the `mkdir` binary would never show it.
2. **Mode argument.** The tool may pass a restrictive mode such as `0700`, or follow the `mkdir` with
   `chmod`/`fchmodat`, and one of those returns `EIO`.
3. **A half-made leftover.** The `tasks` entry, or a parent, may already exist as something other
   than a directory from an earlier session. `ls -ld` on the path was never run, so this is open.
4. **How `/tmp` is mounted.** ext2 versus tmpfs; a full or inode-exhausted filesystem would give
   `ENOSPC`, but a path returning `EIO` is worth ruling out.

### How to narrow it down (from a working shell)

```sh
ls -ld /tmp/claude-0/-src-github-com-netoneko-akuma/<session-id>/tasks
df /tmp; mount | grep tmp
mkdir -p /tmp/claude-0/a/b/c
strace -f -e trace=mkdir,mkdirat,stat,newfstatat,fchmodat \
    mkdir -p /tmp/claude-0/-src-github-com-netoneko-akuma/<session-id>/tasks
```

If there is no `strace`, a Node one-liner reproduces the tool's call:
`node -e 'require("fs").mkdirSync(process.argv[1], {recursive:true, mode:0o700})' <path>`

### Verify (when fixed)

Start a Claude Code session on the box; any Bash tool call, for example `git status`, runs and
returns output instead of the `EIO` above.

### Update 2026-11-10 (Bun probe on the trashcan, c523598a)

Bun's `fs.mkdirSync(.., {recursive:true, mode:0o700})` (twice over the same path), plain `mkdirSync`,
`EEXIST` and `fs.promises.mkdir` recursive all behave; the `EIO` did **not** reproduce, so suspect 1
and 2 look unlikely on a clean path. The probe did find a real, separate bug while testing a file in
the path: `mkdir f/x` with `f` a regular file succeeded and corrupted `f` — fixed, see
[`MKDIR_UNDER_REGULAR_FILE.md`](MKDIR_UNDER_REGULAR_FILE.md). Suspect 3 (a non-directory on the path)
is still the one to check first: run `ls -ld` on each component of the failing path.

## Not yet written up

Nothing else recorded. Add further Claude Code problems as numbered sections here, and link the
incident doc for any that get fixed, per the `archive/` convention in `CLAUDE.md`.

## Background

- `docs/archive/BUG_FIX_LIST.md`: sweep this entry in when a fix lands.
- Related earlier Claude Code work on Akuma: commit `4a927bf5` (wait4 thread-group fix for Claude
  Code's input stall).
