# Taoxichuan stage N, session X: <one line: the gate>

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session:
<RESULT file, commit>. **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
<What already works elsewhere / what must NOT be changed / what the last session got wrong. One paragraph.>

## Read first (in order; `sed -n` the named sections, never whole files)
1. `<doc>` - <section> - <what it tells you>
2. ...
**Probes already exist - use one before writing a new one.** `userspace/` holds purpose-built guest-side probes (list and
one-liners: `docs/reference/userspace-layout.md`): `nettest` (libcurl and `std::net` + `poll(2)` client probes, built for a
network hang), `abiprobe`, `epollprobe`/`memprobe`/`futexprobe` (per-syscall cost and behaviour), `ncaprobe`, `ext2probe`.
Name the one that fits this gate, and how to build it (`userspace/build.sh --<name>-only`).
(Docs are in the checkout under `docs/`; the front door is `docs/README.md`, its symptom matrix lists the right doc.)

## Premises (examine these before you edit anything)
Every claim this brief makes that you did not see pass on this box is a premise, and premises have been wrong in about one brief in two (a gate that needed `fork`, a probe that builds only for aarch64, an applet that is not in the image, a hash that never covered what it was named for,
a loader described as welded that was already an adapter, syscalls described as served that were ENOSYS). The supervisor fills this table before sending and has run each check once; **your first job, before the first edit, is to re-run each check yourself and write one line per row in RESULT-X.md: HOLDS / WRONG (what you saw) / UNCHECKED.**
A WRONG row that changes the plan is reported to the owner as soon as you commit, not discovered at the end. About 10 tool calls for the whole table.
| # | premise | read-only check (exact command) | supervisor saw |
|---|---|---|---|
| 1 | <e.g. the reference value is X> | `<command>` | <output> |

## Symptom first (when a gate fails, before any source read or new probe)
`grep -n -i "<the error text you saw>" docs/README.md docs/runbooks/*.md` and read the matching row and the archive doc it links. The symptom matrix records fixed bugs with the exact syscall sequence that exposed them; session Q spent three sessions and about 15M tokens re-finding what the row for `Could not contact DNS servers` (`docs/README.md`) and `docs/archive/AKUMA_AMD64_DNS_CONNECTED_UDP.md` already say (c-ares `connect`s a UDP socket and then `send`s, and calls `getsockname`/`getpeername`), and its three docs reads were all one `kvm.md` section. Write the matching rows (or "no row") in RESULT-X.md before you add a probe.

## Probe discovery (before you write any probe, gate or script)
**Use a pre-provided probe; it saves the most time of anything in this brief, and you find it yourself.** `ls userspace/*probe* userspace/nettest/rust/ userspace/forktest/c_stress/`, then the probe table in `docs/reference/userspace-layout.md` (what each checks, how to build it for x86_64: Rust probes `cargo build --release --target x86_64-unknown-linux-musl`, no Docker; C probes `. /etc/akuma-dev.env; gcc -static -O2 -o X X.c` on the box, or `x86_64-linux-musl-gcc` on the Mac), and `userspace/nettest/README.md` for the network ones (it picks the probe by symptom). Run the one that matches the failing syscall sequence before adding any kernel print.

List what already exists that exercises this capability, and run the cheapest one first. Search, in this order, and write the hit list (path, what it checks, how to run it, what it printed) in RESULT-X.md: (1) `userspace/*probe*` and `userspace/forktest/c_stress/` (syscall behaviour, differential against Linux); (2) `scripts/*_suite.py` and
`scripts/benchmarks/` (futex, epoll, mem suites run the same static binary on Linux and Akuma); (3) host tests in the crate that owns the logic (`cargo test -p <crate> --target $HOST`); (4) `scripts/kvm/goose/run_gates.sh` groups (reuse a gate's shape; do not invent a new verdict style); (5) a binary that already runs on the full amd64 kernel on this box (the **reference behaviour** for a gate can be measured there
in one command, as `curl --version` was). A new probe is allowed only when this list says why none of the existing ones fits, and it goes into `userspace/` or `scripts/kvm/goose/`, never `/tmp`. When a gate fails, find the existing probe that isolates the failing syscall before reading kernel code.

## Guest facts (verified; do not rediscover, and do not write gates that contradict them)
No `fork`/`clone`: `sh` runs one simple command per ssh exec (no `;`, `&&`, `$(...)`, pipes). No procfs: `ps`/`df` print only their header. busybox 1.35 in the image has no `stat` applet. `apk.static` 3.0.8; static and static-PIE binaries run, dynamic ones do not yet. One core. The host forward degrades near 30 ssh connections per boot (use gate groups of 20 or fewer). The root image is 1 GiB;
herd rotates `vm-dev.log` mid-boot. Update this block when a session proves a new fact.

## Composition rule (mandatory; stages 1-4 had it, stages 6-7 lost it)
`taoxichuan/` is a **composition root**, not a kernel written from scratch. At the start of stage 7 it was 4,450 newly written lines
(`main.rs` 2,928) next to 101 existing crates and a full amd64 kernel that already implements nearly every syscall you will meet.
Before you write any syscall arm, subsystem or helper of more than about 20 lines:
1. **Look first**: `grep -rn "<name>" crates/ amd64/src/` for the syscall or concept; skim `docs/reference/crate-safety.md` and
   `ls crates/` for the crate that owns it (`akuma-syscalls-glue` and the `akuma-syscalls-*` family, `akuma-fdtable`, `akuma-vfs`,
   `akuma-ext2`, `akuma-net`, `akuma-exec`, `akuma-ring3-amd64`, `akuma-svm-amd64`).
2. **It exists in a crate**: add a path dependency and call it. Do not copy it.
3. **It exists only in `amd64/src/`**: extract it into a crate first (`akuma-<thing>`, named for what it owns; keep `#![forbid(unsafe_code)]`
   where it can, `scripts/check_unsafe_budget.py` gates new `unsafe`), make `amd64/` depend on the crate too, then depend on it from
   `taoxichuan/`. One implementation, two consumers.
3b. **It exists, but welded to `Process`/`akuma-exec`**: that is a seam to cut, not a reason to write it again. The tree's own method is extracting the pure
   part into a leaf crate the process model does not reach (`akuma-exec-core`, `akuma-sntp` split out of `akuma-syscalls-time`, `akuma-pipe`, `akuma-fdtable`); a cargo
   feature that makes the heavy dependency optional is the other way. Say which, and what it costs (`akuma-syscalls-glue` has 28 path deps; every one of its features is a
   syscall-family toggle and none cuts `akuma-exec`, so the cut does not exist yet).
   **Survey first**: diff `amd64/Cargo.toml`'s `akuma-*` dependencies against `taoxichuan/Cargo.toml`'s. The ones amd64 uses and Taoxichuan does not (e.g. `akuma-sntp`,
   `akuma-cmos-rtc`, `akuma-syscalls-{linux,abi,poll,mem,net}`, `akuma-pipe`, `akuma-exec-core`, `akuma-port`, `akuma-tty`) are the first candidates for every milestone.
4. **It exists nowhere**: write it, in a crate with host tests where the logic is pure, and say why in the RESULT file.
In every RESULT file report, per arm or feature you added: *pulled* / *extracted* / *written new (why)*, and `wc -l taoxichuan/src/*.rs`
at the start and the end. A session that closes a gate and makes `taoxichuan/src` smaller is better than one that closes it larger.

## Gates
- <ID>: <exact command> -> <exact expected output>. Source of the command/address: <doc or passing run>.
- Zero `Fault|PANIC|sig!` in a passing boot; re-run the previous stage's gates once at the end.
- Out of scope: <list>.

## Ops traps
- Guest kernel installs only with an explicit `/boot/akuma-guest-dev` destination; never write `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs; confirm the
  `guest disk:` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot; restore everything at the end.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box**: Akuma's shell turns it into an interactive REPL that waits forever and hangs your tool call (three of
  the supervisor's own did, and a session of yours stalled right after one). Write the script to a file (the `write` tool) and run `python3 /path/x.py < /dev/null`.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after every boot cycle that taught you something and after every fix that builds.
WIP commits are fine. Never push `origin`. Gate arch-specific code with `cfg`; keep amd64-only crates out of default-members.
**Checkpoint at the first green host test, and again before every kernel build that touches more than one file**: the box has reset once under a long build and uncommitted work survived only because it was on disk.
Anything you add only to see (a debug `safe_print!`, a probe line) carries the tag `[tmp]` in its text and is listed under "Remove before final" in RESULT-X.md.

## Budget (measured: cost per call is about 19k tokens under 40 calls and about 105k over 130 calls, because every call re-sends the context)
About <N> tool calls for this session, and at 90 calls without a commit the supervisor will nudge you. A 5-hour z.ai window is about 34M tokens and a week about 205M on this plan; one session should stay well under 10M. If you are past the call budget without the gate passing, write what you have and stop: a fresh context with a short continuation prompt is cheaper than a long one.

## When to stop
Gate passes in a clean boot, or <N> boots with no new evidence, or the quota error. Count boots, not minutes. Write
`/root/dev-cycle/<dir>/RESULT-<X>.md` (the premise table results, the probe hit list, gates, exact failing step with console evidence, what is next) and end the turn.
