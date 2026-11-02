# Shanghai: build a new kernel from the akuma-tokio-kernel README

You are building **Shanghai**, a new kernel, from one brief:
`crates/akuma-tokio-kernel/README.md`. Read it first and follow it; it is the whole procedure. The sample it
describes is `osaka/` (a 10-line `main.rs`) — do not copy Osaka's directory, write Shanghai's files from the
README, with `shanghai` as the package name and `.name("Shanghai")`.

## Start

- Branch: `git fetch origin && git checkout -b shanghai origin/rust-org-il-experiment` (at `aafbb973` or later).
- You change only `shanghai/` and the one `members` line in the root `Cargo.toml`. No other file in the tree.

## Gates (in order; stop at the first that fails and write down why)

| # | gate | passes when |
|---|---|---|
| 1 | build | `cargo build -p shanghai --target x86_64-unknown-none --release` succeeds with no warnings |
| 2 | minimal image | the README's "smallest disk" steps produce `shanghai.img` with `/bin/sshd`, `/bin/busybox`, `/bin/sh`, `/etc/sshd/authorized_keys` |
| 3 | boot | the console shows `Shanghai/amd64 -- long mode reached` and `Shanghai: entering ring 3 -- /bin/sshd`, and no `[Fault]` or `PANIC` line |
| 4 | inside | an ssh command answers: `busybox uname -a` prints `Linux shanghai ...` |
| 5 | tokio | `tokioprobe` (README § "Run tokio on it") prints `RESULT PASS`, at one core and at `smp=4` (`workers=4`) |
| 6 | goose | on an image built by `scripts/kvm/taoxichuan_goose_image.sh`, `goose run` answers `PONG` |

Boot with whatever the machine you are on has: the README gives the QEMU line (TCG or KVM) and the `herd vm-run`
spec for Akuma's own hypervisor. Never kill a VM by pattern; stop only the one you started (its pid file).

## Commit and push

Commit after each gate that passes, on the `shanghai` branch, and **push to `litter`**
(`git push litter shanghai`) after every commit, so the work can be fetched from there. Never push to `origin`.
Never rewrite history.

## Report

Write `shanghai/RESULT.md`: each gate with the command you ran and its captured output; every place the README
was wrong, unclear or missing a step (quote it, say what you did instead); and the line count of
`shanghai/src/main.rs`. Commit and push it to `litter` too.
