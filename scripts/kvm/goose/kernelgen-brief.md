# Build a new kernel, {CITY}, from the akuma-tokio-kernel README

You are building **{CITY}**, a new bare-metal x86_64 kernel whose job is to run Linux programs, and above all
**tokio** programs (tokio is the dependency that makes Linux binaries run on it). Your whole brief is
`crates/akuma-tokio-kernel/README.md` in the checkout you are standing in. Read it completely first and follow it. The
sample kernel it describes is `shanghai/` (a short `main.rs`): do not copy Shanghai's directory, write {CITY}'s files
from the README, with `{LOWER}` as the package name and `.name("{CITY}")`.

**The banner.** Draw a small {CITY} skyline in plain ASCII, at most 60 columns wide and 12 lines tall. Save it as
`{LOWER}/src/banner.txt` and wire it with `.banner(include_str!("banner.txt"))`. It prints just before init starts.

## Rules

- You change only the new `{LOWER}/` directory and add one `members` line to the root `Cargo.toml`. No other file in the tree.
- Run every command with your shell tool. Do not ask questions; nobody is there. Never use sudo or a package manager.
- Environment for every cargo command: `export PATH=/home/netoneko/.cargo/bin:$PATH`; the target dir is already set
  (`CARGO_TARGET_DIR`), do not change it. Use `-j4`.
- You have `qemu-system-x86_64` but **no KVM** (use the README's TCG boot line; it is slow, give it minutes). Stop only a VM you started
  (use its pid file); never `pkill` by pattern.
- Commit in this checkout after each gate that passes (`git add {LOWER} Cargo.toml Cargo.lock && git commit`). Never push.

## Gates (in order; stop at the first that fails and write down why)

| # | gate | passes when |
|---|---|---|
| 1 | build | `cargo build -p {LOWER} --target x86_64-unknown-none --release -j4` succeeds |
| 2 | minimal image | the README's "smallest disk" steps produce `{LOWER}.img` with `/bin/sshd`, `/bin/busybox`, `/bin/sh`, `/etc/sshd/authorized_keys` |
| 3 | boot | the console shows the kernel's long-mode line and `entering ring 3 -- /bin/sshd`, with no `[Fault]` or `PANIC` line, and your banner |
| 4 | inside | an ssh command answers: `busybox uname -a` prints `Linux {LOWER} ...` |
| 5 | tokio | the README's "Run tokio on it" section: `tokioprobe` prints `RESULT PASS` |

## Report

Write `{LOWER}/RESULT.md`: each gate with the command you ran and its captured output; every place the README was
wrong, unclear or missing a step (quote it, say what you did instead); and `wc -l {LOWER}/src/main.rs`. Commit it.
If you cannot finish, still write RESULT.md with how far you got and the exact error you stopped on.
