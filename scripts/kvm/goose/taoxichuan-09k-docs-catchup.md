# Taoxichuan: write the kernel's own documentation (`docs/taoxichuan/`), with diagrams - no kernel changes this session

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Your tree is the tip of `taoxichuan-s4` (`git log --oneline -3` first). **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. This session does not boot anything and does not edit `taoxichuan/src/` or any crate; it writes documentation only.

## The goal
The kernel has grown to about 8,000 lines in `taoxichuan/src/` across seven stages and eight sessions, and its documentation is the archive: incident write-ups and session results. Nothing says what the kernel IS now. The user wants it the way Akuma itself is documented: its own folder, a front door, one reference per subsystem, diagrams, all kept current by you from now on so that you (and the next session) can read the docs instead of re-reading the source.
`docs/taoxichuan/README.md` exists (the user's supervisor seeded it): read it first. It names the folder layout, the status table, the five figures and the doc rules. **Fill it in**:
1. `reference/boot.md`, `memory.md`, `processes.md`, `scheduling.md`, `pipes.md`, `syscalls.md`, `devices.md` - what is true of the code now, with the crate that owns each decision linked (`crates/<name>`).
2. `diagrams/` - the five figures in the README, as mermaid, each under 40 lines, each checked against the source (a diagram that contradicts the code is worse than none). `docs/reference/subsystems/memory.md` shows the repo's mermaid style.
3. `reference/syscalls.md` - one row per syscall arm the dispatcher serves (`grep -n "ring3::nr::" taoxichuan/src/main.rs` and the files in `taoxichuan/src/`): number, name, which crate decides it (or "local"), what it does today in one line, known divergences from Linux. Rows for what is answered `ENOSYS` or accepted-and-ignored (for example `mprotect`, `munmap`, `fchmod`) say so plainly.
4. `gaps.md` - ranked, each with the probe that shows it. Seed list from the supervisor's probes (verify each before you keep it): `mprotect` is accepted and ignored so a page never becomes executable (`tcc -run` faults with an instruction-fetch page fault); `fork` and `exec` work only for children of the spawn crossing, not for init; init starts with an empty environment and there is no `/proc/self/exe` (musl's `$ORIGIN` expansion fails; `rustc`'s shim cannot find its library); the mmap slab is a fixed `SLAB` (64 MiB) shared in size with brk, while rustc's driver library alone is about 300 MB; `clone` with thread flags is refused (`EPERM`; musl's `pthread_create` reports `EAGAIN`); `sigaltstack`, `membarrier`, and the plain `readlink` number answer `ENOSYS`; loading a shared object floods the console with `[E2-EOF]` lines; the console relay turns non-ASCII into `?`.
5. `runbooks/` - build, boot (`boot_dev.sh`), run the gates (`run_fork.sh`, `run_pipes.sh`), and boot under QEMU TCG on a Mac (the supervisor's recipe: `qemu-system-x86_64 -M microvm`, the disk on `bus=virtio-mmio-bus.0`, `virtio_mmio.device=512@0xfeb00000:5` on the cmdline). Each ends with a **Verify** section.
6. Add the folder to `docs/README.md` (done by the supervisor: check the row), update the README's status table from the last RESULT files on the box, and link the archive docs each reference doc was written from in a "Background" footer.

## How to work
Read the code in windows (`sed -n` of 60 lines or less); read `taoxichuan/Cargo.toml` for the crate list, then `docs/reference/subsystems/` for how Akuma's docs read. The experiment's own docs are `docs/archive/TAOXICHUAN_*.md` and the agent log's last sections. **Do not copy archive prose**: state the current rule, link the incident. If you find the code and an archive doc disagree, the code wins; say so in the reference doc and note it in `gaps.md` if it is a defect. Write each file as you finish reading its subsystem and commit after each (small commits).

## Composition rule
You change no code, so none applies; but every reference doc says, for each piece, "pulled from `crates/<name>`" or "written in `taoxichuan/src/<file>`".

## Commit rule (you have no clock)
Commit after each doc. **Run `git push litter taoxichuan-s4` after every few commits.** Never push `origin`. Commit messages say "the user", not "owner". The pre-commit hook scans staged `.md` files for secrets; if it rejects, fix the file.

## Budget
About 70 tool calls. Docs only: no boots.

## When to stop
All of items 1-6 written and committed, or the budget. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pk0.md` (list of files, what you could not verify, what you think the next session should know), and end the turn.
