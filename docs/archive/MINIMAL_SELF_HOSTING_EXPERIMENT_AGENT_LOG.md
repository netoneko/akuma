# Minimal self-hosting kernel "Taoxichuan" — agent log

**Status: in progress. Started 2026-10-24. Stages 1-4 PASS; stage 6 (offline `apk add`) PASS on metal (G1-G4, 2026-10-25 05:35 IDT) and on the aarch64 host track.**

Running log for the two-agent execution of
[`MINIMAL_SELF_HOSTING_EXPERIMENT.md`](MINIMAL_SELF_HOSTING_EXPERIMENT.md).
The minimal kernel being composed is named **Taoxichuan**.

**Oversight change (2026-10-24, afternoon).** The host track and the babysitting
of goose-glm were first run by Kimi (Crush, `k3-256k`). Kimi ran out of tokens
because of a bad subscription setting on the user's side — not a model failure.
Oversight moved to **Claude Sonnet 5 (Claude Code)**, which takes over the host
track and supervises goose. The metal track is unchanged: still goose + GLM on
the ryzen. The tracks below still say "Kimi/Crush" for what was done before the
handover; entries after it are marked *(Sonnet)*. Note for the open-weights
claim: the host track is no longer open-weights from this point; the metal
track still is.

**User's note (2026-10-24, evening): the tooling was badly configured, and the user is
frustrated about it.** The user says that is their own fault. Recorded here because it
cost real tokens and time on both tracks and is easy to mistake for a model failure.
Facts, as found *(Sonnet)*:

- Kimi's first session ran out of tokens on a bad subscription setting (see above). The
  replacement Kimi session (Crush, `k3-256k`) used 26% of its 5-hour quota in about 23
  minutes, mostly re-reading a growing context on every call.
- Crush was given `reasoning_effort: low` for `k3-256k`; from then on about a quarter of its
  calls (10 of 38 in one hour) came back empty and were dropped ("unsupported reasoning
  metadata"), and one request hung for five minutes. Crush was closed.
- `goose-kimi` and `goose-glm` both write the same `secrets.yaml`, and `goose-glm-serve` reads
  it. A one-word smoke test of `goose-kimi` (by Sonnet) put Kimi's key under the live server
  and goose-glm answered every prompt with `Authentication required`; Kimi then burned
  turns debugging the key. Fixed by restoring the key and restarting `goose-glm-serve`.
- A `send_after_reset.py` left over from an earlier Sonnet session sent the `01b` brief to
  goose-glm a second time (session `20261024_24`, 16:44) while Kimi's own send
  (`20261024_22`, 16:39) was still running: two overlapping sessions on one checkout for
  about 13 minutes, until Kimi's driver died with Crush.
- The supervisor now runs as goose with Kimi on the Mac (`scripts/kvm/goose/goose-kimi-mac.sh`,
  an ACP server on `127.0.0.1:3285`, secured with a key kept in `~/.akuma/goose/acp_secret_key`),
  and Sonnet watches both and takes over if Kimi's quota runs out.

**User's note on the banner (2026-10-24, evening).** The user was not a fan of goose-glm's
skyline art (`511f4fdb`; the words stay, the user approved those). They went to other
models for a redraw and report **deep disappointment with Mistral** and **mild disappointment
with Gemini**. The new banner was made by Gemini; after many failed attempts to get Gemini
to adjust it, the user gave up and adjusted it by hand, and committed the result
(`419fb968`, "better banner for taoxi kernel"). Recorded as the user's opinion of those
models on this art task, outside the two tracks of the experiment; it says nothing about
either model's coding.

**Oversight change #2 (2026-10-24, 18:20-18:30 IDT): Kimi (goose on the Mac) ran out of quota;
Sonnet supervises again** *(Sonnet)*. Kimi's two supervisor stints, for the record:

- *Crush, `k3-256k`, ~16:00-16:57:* took 26% of its 5-hour quota in about 23 minutes, hung on empty
  turns, and spent turns debugging a key that the Sonnet smoke test had swapped (see the tooling note).
  Closed by the user.
- *goose, `k3-256k` low effort, session `20261024_3`, 17:04-~18:25:* **how far it got.** It merged
  goose-glm's finished stage 1 (`bd59f4aa`), ran the aarch64 check of the merged tree (build 662,792 B,
  Firecracker boot, zero faults, `--no-disk` control; `9418bdf1`, PASS), wrote and pushed the stage 2 brief
  (`d59f0df1`), sent it to goose-glm, and committed the tooling edits (`c87730e1`). Model time was about 22
  minutes: 1.30M accumulated input, 19.8k output tokens (`sessions.db`, Mac goose).
- **Failure modes.** (1) It ended its turn right after sending stage 2 ("I'll validate when the turn ends"),
  twice, which stops the loop because nothing re-prompts a turn-based agent; fixed by moving the waiting
  into `scripts/kvm/goose/supervise-loop.sh`, which prompts it only when goose-glm's driver exits. (2) The
  user's instruction to send the next stage to goose-glm *before* validating the last one (so goose is
  never idle) arrived after stage 2 had already gone out, so it was never exercised. (3) Quota ran out
  (reported by the user, ~18:23) before the stage-2 close; our logs show no 429 because Kimi was not
  prompted again. The loop was stopped so it would not burn a prompt on the error. The user, annoyed by
  the run of failures, then **unsubscribed from Kimi**; Kimi does not come back in this experiment, and
  Sonnet is the supervisor from here (the host track is no longer open-weights from this point, as
  already noted for the first handover).
- **The pull side is unreliable, and the user knows:** Kimi learns what goose did by polling a log file and
  reading `RESULT.md` over ssh; a machine-readable status per stage, a detached driver launcher and a
  proper wait/notify channel between the two agents are the fix, to be done later.
- **State at handover:** stage 1 passes on both tracks. Stage 2 is running on goose-glm (branch
  `taoxichuan-s2`, `92e9572b` pushed: the `akuma-ring3-amd64` crate; ~480 lines of uncommitted
  composition in `taoxichuan/src/main.rs` on the box; 11.7M input / 107k output tokens in the first
  hour on `glm-5.3-flash`, and the user reads the z.ai quota meter as clearly cheaper than the
  `glm-5.3` stage-1 run). Sonnet closes stage 2 when the driver exits: read `RESULT.md`, send the stage 3
  brief first, then the aarch64 check (plus `cargo check -p` of the new crate on the Mac: its
  `naked_asm!` is not `cfg`-gated for x86_64).

Working rules carried over from the user's last Crush session (the "wisdom"):

1. Every stage is a **separate goose prompt** (new ACP session); help it when
   stuck, but let it finish with KVM verification.
2. The new kernel gets a **new source tree** — a named thing in the repo, not
   only a filename on a binary. Each track records which shape it chose and why.
3. Sync through **GitHub** (host pushes `origin`, ryzen pushes `litter`), not
   bundles; reconcile the two tracks' changes into one kernel.
4. Say honestly **who built what** (host vs goose) in this log.
5. Document the process here as it goes; hypervisor disagreements are findings.
6. **Cost is tokens and time worked, not dollars**: goose-glm is on the lowest-tier
   GLM subscription. After every goose session, record wall time, LLM calls, input /
   output / cache-read tokens and the largest context in this log
   (`scripts/kvm/goose_box_stats.py`, run on the box).
7. Agents' shells need `HOME=/root` (ssh does not set it; git and logs depend on it).

Tracks:

- **Host track (Kimi/Crush on the Mac)**: builds the same kernel, verifies
  under Firecracker in the Lima VM `fc`. Commits on `rust-org-il-experiment`,
  pushes to `origin`.
- **Metal track (goose-glm on the ryzen, ACP server on :3284)**: builds and
  tests under Akuma's own KVM (`/dev/kvm0`, herd `vm-dev` pattern). Commits on
  its own branches, pushes to the `litter` remote only. Each stage is a
  separate goose prompt (new ACP session per stage).

A stage counts when BOTH tracks pass its gate. Hypervisor disagreements are
findings, recorded here, not averaged away.

## Environment baseline (verified 2026-10-24)

- Box checkout `/src/github.com/netoneko/akuma` on `rust-org-il-experiment`
  @ f8dd945f, experiment doc present. Dirty submodules
  (`userspace/rumpkernel/src-netbsd`, `userspace/tcc/tinycc`) and untracked
  `root/`, `rustc-ice-*.txt` — pre-existing, left alone.
- ACP probe from Mac: `pong` / `end_turn` against
  `http://192.168.1.159:3284/acp`; `/health` 200.
- Lima `fc` VM on the Mac: Running (vz, aarch64, 4 CPU, 8 GiB).

## Recurring bug classes (to be traced through the docs)

**"The fd table's answer is special-cased to the three stdio fds" - the fd nonsense.** User's note, 2026-10-25: this class has
resurfaced at least 4-5 times across the kernels, always looking like something else (a network hang, an `ar` failure, a pipe
that never closes, a tty that is not one). The stage 7B instance: the Taoxichuan guest's `writev` served fds 1 and 2 only and
answered `EBADF` for every other fd; musl stdio flushes through `writev`, so busybox `wget`'s HTTP request never left, stdio kept
the error in the stream's flag, and `wget` blocked forever reading a reply to a request nobody sent. TCP connect, the gateway and
the relay were all fine (`connect` returned in 52 ms; commit `6879eb47` on `litter/taoxichuan-s4`). The 7A session had called
the same symptom "a stall in the guest/gateway handshake" for an hour: **a silent hang after a good connect is a missing or
stdio-only arm until proven otherwise.**

Earlier occurrences found so far (seeds, not yet audited; follow each link, then grep for the rest):
- `docs/archive/BUG_FIX_LIST.md` ~l.177: `sys_writev` did not stop at a short write (the writev family's own history).
- `docs/archive/BUG_FIX_LIST.md` ~l.1732, `AKUMA_AMD64_4B_FOLD_BATCH3B.md` ~l.77, `AKUMA_SELF_HOSTING_AMD64.md` ~l.879: `fstat` on a
  socket fell to `_ => EBADF` on **both** kernels (no `Socket` arm in `fstat_fill`).
- `docs/archive/BUG_FIX_LIST.md` ~l.2237: `isatty(0)` decided tty-ness from the process's I/O channel, not the fd table.
- `docs/archive/AMD64_CONSOLE_NONBLOCK_READ.md` ~l.216: `FileDescriptor::DevTty` unmatched; only `Stdin`/`Stdout`/`Stderr` were.
- `docs/archive/RUST_TOOLCHAIN.md` § 4d: the CLOEXEC-pipe `EBADF` on `recvmsg` over a socketpair.
- This log, stage 3/4 (~l.829): nested `writev` to fd 1 going to the child pipe (musl flushes stdout as `writev`; `3c560be7`).

**How to use this:** when a Taoxichuan (or amd64) program hangs or silently loses output after a successful `open`/`connect`, before
instrumenting the stack, list the syscall arms that take an fd (`read`, `write`, `readv`, `writev`, `fstat`, `fcntl`, `ioctl`,
`lseek`, `poll`) and check each for a `_ => EBADF`/stdio-only fall-through. Proper follow-up (not done): one audit pass over every
fd-taking arm in `taoxichuan/src/main.rs` against `akuma-fdtable`'s vocabulary, so the kernel has one answer per fd kind; and the
whole-docs trace the user asked for (every mention, one table).

## Stage log

### Stage 1 — boot in KVM (gate: init spawn, banner, zero [Fault]/PANIC/[sig!])

- 2026-10-24 host track: brief prepared, sent to goose as ACP session
  (see below). Firecracker/Lima host-track setup in progress.

- 2026-10-24 *(Sonnet)*: handover. State found: goose-glm is 25+ min into the
  stage-1 turn, reading only (surveying `amd64/src` vs the crate tree), box on
  `taoxichuan-s1` @ 96b3585c, no commits, no `RESULT.md`. Kimi's host-track
  claims (aarch64 stage 1 PASS under Firecracker, an `init-script` stage-2 prep
  in `akuma-config` / `akuma-kernel-glue`, `taoxichuan-fc.bin`) are **not in the
  tree** (uncommitted, lost with the session): treated as unverified and to be
  redone, not as a pass.

- 2026-10-24 *(Sonnet)*: first cost reading of the stage-1 session (`20261024_9`, at
  09:30 UTC, still running): 52 min wall, 96 LLM calls, 8,391,298 input tokens of
  which 8,103,936 (96.6%) cache reads, 46,859 output, largest context 148,749. The
  cost is re-reading a growing history, not generation.

(Entries append below this line, newest stage last.)

### Stage 1 — host track (Sonnet), aarch64 / Firecracker in Lima `fc` — PASS

Built and run by Sonnet on the Mac, not by goose and not by Kimi (Kimi's earlier
claim was re-done, not trusted).

- **Composition root, now named in the tree:** Cargo feature `taoxichuan`
  (`no-tests`, `extreme`, `userspace-sshd`) + `platform-firecracker`, built by
  `scripts/build_taoxichuan_fc.sh`. Zero new Rust source: stage 1 is a feature
  selection over the existing crate tree. The image is **byte-identical**
  (`cmp`) to the one from Kimi's ad-hoc `--features no-tests,extreme,userspace-sshd,
  platform-firecracker` recipe, 662,792 bytes flat (729,136 ELF).
- **Boot:** `FC_KERNEL=taoxichuan-fc.bin overlays/devbox-firecracker/run.sh --no-net`,
  1 vCPU, 1 GiB, the existing 2 GiB ext2 `disk.img` on virtio-blk.
- **Gate evidence** (`/tmp/akuma-fc-boot.log` in the guest, 134 lines):
  `[FS] Ext2 filesystem mounted at /` -> `[Main] Starting userspace sshd (no
  supervisor)...` -> `[Main] sshd started (tid=8)`; kernel-spawned, no herd.
  `grep -c "Fault|PANIC|sig!"` = 0 (the only `fault` text is the `[TLB]
  stale_write_faults=0` counter). Banner: `Akuma Kernel starting...`.
- **Control:** with `--no-disk` the same image reaches only `Block device not
  found`/`Continuing without filesystem` and never spawns init — so "reaches init
  spawn" needs the disk; a diskless pass would have meant nothing.
- **Observations, not failures:** the banner says Akuma, not Taoxichuan (a
  kernel-visible name is still to do); `[unregister] pid=1 tid=8 has NO map owner`
  follows the sshd start, and `[EXCC] el0=29` shows sshd running at EL0 for a
  moment; with `--no-net` it has nothing to serve, which is stage 4's business.
- Metal track (goose, amd64, Akuma KVM) has not reported yet; the stage does not
  count until it does.

### Stage 1 — metal track (goose-glm), amd64 / Akuma KVM — reached the gate, not signed off

Built by goose-glm (GLM 5.3) on the ryzen box; supervised by Sonnet. Branch
`taoxichuan-s1`, commit `43edbffc`, pushed to `litter` by Sonnet (goose had not
pushed before it stopped).

- **What it composed:** a real new source tree. `taoxichuan/` is the composition
  root (own `main.rs`, `linker.ld`, `build.rs`; pulls 13 crates; PVH boot, ext2 root
  on polled virtio-blk, single vCPU, no scheduler/BKL/userspace). Two crates were
  extracted first, as the brief asks: `akuma-boot-amd64` (`boot.s`, `phys.rs`, the
  paging walker, the memory policy; `amd64/` now re-exports them) and
  `akuma-comconsole` (the 16550 console). Image: 162 KB. The `amd64/` kernel was
  refactored onto the shared crates (about 1,000 lines moved) and still *builds*;
  **it has not been booted since**: open item before merging.
- **Gate evidence** (`/root/dev-cycle/taoxichuan-s1/vm-dev-pass.log` on the box,
  09:55 UTC): banner box; `ext2 root mounted on /dev/vda`; `Taoxichuan: init spawn
  -- init=/bin/busybox argv=[sh,/etc/guest.sh]`; `init: 1131168 bytes on the ext2
  root, ELF magic ok`; `Taoxichuan: stage 1 gate reached`; 0 `fault|panic|sig!`
  lines. The guest then halts with interrupts off, as a stage-1 kernel with no
  userspace should.
- **The harness `FAIL` line is noise, not a finding.** `[KVM] akuma-vm guest: FAIL
  (banner=yes, init=no, net=...)` is herd's marker check; the guest exits by design
  and the line appears on clean main-kernel passes too (`AKUMA_KVM_DEV_CYCLE.md`,
  task 00 row). Corrects an earlier version of this entry that called it a
  disagreement. The stage-1 gate is the kernel's own `init spawn` line plus the
  zero-fault grep.
- **Stopped by quota, not by failure:** at 09:56 UTC z.ai answered `Usage limit
  reached for 5 hour. Your limit will reset at 2026-10-24 21:17:13` and the turn
  ended. No `RESULT.md` was written, and `taoxichuan/src/main.rs` has an
  uncommitted edit. The 5-hour window on the lowest GLM tier is a real constraint of
  this track: plan stages to fit it.
- **Cost (from `sessions.db`, session `20261024_9`):** 77.2 min wall, 142 LLM calls,
  16,057,961 input tokens (15,736,512 cache reads, 98.0%), 68,932 output tokens,
  largest context 181,081.
- **Reconfigured (Sonnet, 10:30 UTC):** `goose-glm-serve` now defaults to
  `glm-5.3-flash` at `GOOSE_THINKING_EFFORT=low` (was `glm-5.3`), on the box and in
  `overlays/ryzen/goose-glm-serve`; service restarted by pid, herd brought it back.
  Not yet verified: a probe still gets the 5-hour limit error, and **the limit
  applies to flash as well** (it is per account, not per model), so the new default
  can only be measured after the reset. Compare its tokens-per-stage against the
  16.1M input / 69k output above.

#### Where goose stopped, and the main-kernel check (Sonnet, 10:35-10:45 UTC)

- **Stopped at the paperwork, not mid-work.** Last moves (09:53-09:55): first boot
  passed all three gate conditions; made the banner ASCII-only (the host console
  relay turned UTF-8 into `???`), rebuilt, re-ran the gate clean, saved evidence to
  `/root/dev-cycle/taoxichuan-s1/`, tore down (`vm-dev` stopped+disabled,
  `vm-serve-mc` back). The next call (commit, log section, `RESULT.md`) hit the
  quota. Uncommitted at the stop: the ASCII-banner edit in `taoxichuan/src/main.rs`.
- **Main amd64 kernel after goose's boot-kit refactor: boots.** `kbuild -j 4
  --features no-tests` at `43edbffc` (37 s incremental), installed to
  `/boot/akuma-guest-dev` only, booted as a `kvm0` guest from a fresh copy of the
  pristine root: `AKUMA-GUEST-UP`, `uname` build id `43edbffc`, `AKUMA-GUEST-DONE`
  on the first poll, zero `[Fault]|PANIC|[sig!]`, host `dmesg` clean. Console saved
  as `/root/dev-cycle/main-amd64-after-refactor.log`. `vm-serve-mc` restored.
- **What this does not cover:** a `kvm0` guest enters by PVH. The refactor touched
  `multiboot2.rs`, `boot.s` and the paging/phys code, whose GRUB/multiboot2 and
  real-hardware path is not exercised by it. Needs the OVMF multiboot2 rig or a
  metal boot before the refactor merges to the main amd64 line.

#### Metal verification (Sonnet, 10:46-10:50 UTC) — PASS, both kernels

- **Main amd64 kernel on bare metal, after goose's boot-kit refactor: PASS.**
  Installed the `43edbffc` build (md5 `22910843...`, the one that booted as a guest) to
  `/boot/akuma-amd64.verify` only, after copying the old one to
  `.verify.pre-taoxi`; the default "known good" kernel was not touched. Ran
  `scripts/kvm/dev_cycle.py reboot-test --entry 29` (clean reboot, Akuma -> Pop -> GRUB
  entry -> Akuma). Result: Pop returned, CMOS checksum OK (`05e8` = `05e8`), Akuma came
  back with `uname` build id **`43edbffc`** on the ryzen's 8 cores (SVM enabled on
  cpus 1-7, `kvmmem` 4096 MiB reserved, wifi up, CMOS RTC seeded), and `dmesg` has
  zero `EXCEPTION|panic|[sig!]` lines. This is the GRUB/multiboot2 + real-hardware
  path the PVH guest test could not cover; the refactor of `boot.s`,
  `multiboot2.rs`, paging and phys is cleared on metal.
- **Services after the reboot came back on their own:** `goose-glm` (first clean
  start from boot, 0 restarts, `/health` 200, default model now `glm-5.3-flash`),
  `vm-serve-mc`, `sshd`, `wifiauto`.
- **Not tested on metal:** Taoxichuan itself. It is a PVH/virtio-mmio guest kernel
  (kvm0/Firecracker), not a GRUB/metal kernel; its gates are the guest ones.
- **aarch64 / Firecracker (host track), re-run on the pushed feature set:**
  `scripts/build_taoxichuan_fc.sh` image (662,792 B) boots in Lima `fc`: ext2
  mounted, `/bin/sshd` spawned by the kernel with no herd, 133 log lines, zero
  `[Fault]|PANIC|[sig!]`. The image differs from the first build by bytes; the
  kernel embeds the git id and HEAD moved. Not diffed further.
- **Still open for stage 1:** goose's uncommitted ASCII-banner edit, its
  `RESULT.md`, and a hand re-run of the Taoxichuan gate (after the quota resets at
  13:17 UTC). The two tracks have passed the same gate on different architectures
  (aarch64 + Firecracker by Sonnet, amd64 + Akuma KVM by goose), which is parity of
  intent, not the same kernel.

#### Integration (Sonnet, 10:55-11:05 UTC)

- Goose's last edit committed on its behalf as `107eb25c` (ASCII banner), pushed to
  `litter/taoxichuan-s1`, and `taoxichuan-s1` merged into `rust-org-il-experiment`
  (`0c41abbc`, clean). Workspace gained the `taoxichuan` member. **Merge broke the
  Mac's pre-commit hook**: it runs clippy/tests on every extracted crate, and
  `akuma-boot-amd64` assembled its AT&T x86 `boot.s` unconditionally (fine on goose's
  x86 box, `att_syntax` is x86-only on an aarch64 host). Fixed by gating that
  `global_asm!` on `target_arch = "x86_64"`; checked that the x86_64 rlib still
  carries `_start`/`_start_mb2`. Both new crates still have **no host tests**, only
  the KVM gate. Lesson for the next prompt to goose: its box is x86, so a crate that
  builds there can still fail the aarch64 host pass; tell it to `cfg`-gate asm.
- After the merge the aarch64 `build_taoxichuan_fc.sh` image still builds (662,792 B).
- Raw logs for the stage-1 entries: [`TAOXICHUAN_STAGE1_LOGS/`](TAOXICHUAN_STAGE1_LOGS/).

#### User approval of the banner, and the next goose prompt (Sonnet, 2026-10-24)

- **User approval:** on seeing goose's Taoxichuan banner and boot text, the user:
  "yeah that's a nice vibe it got going there". The words stay. Direction for the art:
  keep the text, improve the ASCII picture into an old-timey Chinese skyline as it was
  for the emperor's kilns in Jingdezhen (JDZ). The banner meaning ("the kiln-fire
  market") and the first UTF-8 art were goose's own invention; the user's approval is
  of the tone, not of a translation.
- **Next prompt:** `scripts/kvm/goose/taoxichuan-01b-finish.md` - merge the supervisor's
  fixes, redraw the banner (ASCII only, <= 78 columns, kiln skyline, words unchanged,
  `\n` escapes), re-run the gate by hand and boot the main kernel once more, then do
  the paperwork the quota cut off (commit, push to `litter`, log section, `RESULT.md`).
  It is sent as a new ACP session once the quota resets (13:17 UTC).


### Stage 1 — metal track (goose-glm), finish — 2026-10-24 — PASS, signed off

Second session of the metal track (GLM 5.3-flash on the ryzen box; the first
reached the gate but was cut by quota before the paperwork). Branch
`taoxichuan-s1` at `511f4fdb` = fast-forward merge of `origin/rust-org-il-experiment`
(`be04a01d`, carrying the supervisor's two fixes) + one banner commit. Pushed to
`litter` after every commit; nothing was left uncommitted this time.

- **Took the supervisor's fixes first:** merged `origin/rust-org-il-experiment`
  with zero conflicts. It gates the `global_asm!` of `boot.s` behind
  `#[cfg(target_arch = "x86_64")]` (AT&T x86 asm broke the aarch64 Mac host
  build) and adds the same two clippy allows `akuma-bkl`/`akuma-elf` carry to
  `akuma-boot-amd64`. Both verified in the diff before the merge.
- **The banner kept its words and got a picture:** same four text lines, same
  `Taoxichuan 0.1.0 (Akuma/amd64 crates) x86_64 -- stage 1` line, same status
  lines. Around them, a woodblock-print Jingdezhen: the Chang in front with two
  junks and kiln-fire glow (`~*~`) on the water, hills behind, and along the
  bank two big hulu kilns breathing smoke through tall chimneys, a house with
  upturned eaves, a pagoda, a small third kiln — and the crates kernel sitting
  on the bank where the wharf shed would be. ASCII-only (0x20–0x7E), 15 lines,
  67 columns max, still one `const ART: &str = concat!(...)` printed with one
  `console::puts(ART)`. The art was laid out on a column grid by a throwaway
  python script that also checked ASCII-onlyness and emitted the Rust literals,
  so the strings now carry `\n` escapes — the real newlines inside the previous
  `concat!` literals are gone. One extra allow,
  `clippy::too_long_first_doc_paragraph`, on the `taoxichuan` crate (same one
  the supervisor gave `akuma-boot-amd64`); it flags a pre-existing doc comment
  on the multiboot2 entry, not anything from this session.
- **The gate, re-run by hand — PASS.** `kbuild -j 4 -p taoxichuan --features
  no-tests` (166,368 bytes), installed to `/boot/akuma-guest-dev` only, the
  `vm-serve-mc` → `vm-dev` dance as in `AKUMA_KVM_DEV_CYCLE.md`, log at
  `/root/dev-cycle/taoxichuan-s1/vm-dev-pass.log` (373 lines). Kernel's own
  evidence: `Taoxichuan: init spawn -- init=/bin/busybox argv=[sh,/etc/guest.sh]`,
  `init: 1131168 bytes on the ext2 root, ELF magic ok`,
  `Taoxichuan: stage 1 gate reached -- init spawn (userspace is stage 2)`;
  0 `[Fault]`/`PANIC`/`[sig!]` lines. `vm-serve-mc` running again afterwards.
- **The merge did not break the main kernel:** built `akuma-amd64`
  (`kbuild -j 4 --features no-tests`, 46.7 s, 11,089,944 bytes), same
  install/dance, guest reached `AKUMA-GUEST-UP` → `AKUMA-GUEST-DONE` with zero
  fault lines in that boot's segment.
- **The banner as printed** (from the pass log):

```
     ~      _       _                      ~      _         _  ~
    ( )   __/ \_______/ \__/ \            ( )   ___/ \______/ (_)
      |                    /--\             |                  |
   .-----.       ,^.      //[]\\         .-----.              .--.
  ( |^^| )      /___\     | [] |        ( |^^| )             ( () )
   '-----'      |___|    /|____|\        '-----'              '--'
  +------------------------------------------------------+
  |   Taoxichuan  --  tao xi chuan, the kiln-fire market  |
  |   A kernel composed from Akuma's crates, one stage   |
  |   at a time, fired in Akuma's own kiln: booted under |
  |   Akuma's hypervisor, on Akuma's metal.              |
  +------------------------------------------------------+
  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~
        |\                            |\
   ~*~\__|_\______   ~  ~  ~  ~   ~*~\__|_\______   ~  ~  ~  ~  ~

  Taoxichuan 0.1.0 (Akuma/amd64 crates) x86_64 -- stage 1

Taoxichuan: stage 1 gate reached -- init spawn (userspace is stage 2)
```

- **Surprises, small:** (1) bare `cargo clippy -p taoxichuan` on this box dies
  inside dependencies (`scopeguard`, `log`, `zerocopy`) — it must run with
  `--target x86_64-unknown-none --offline`, then deps check clean and only the
  crate itself is linted; (2) herd's generic
  `[KVM] akuma-vm guest: FAIL (banner=yes, init=no, ...)` marker still prints on
  clean passes, exactly as the dev-cycle doc says; (3) the old dev log keeps
  previous boots appended, so fault-greps for the regression boot were scoped
  to that boot's segment of the file.

### Stage 1 — metal track (goose-glm), finish — independent re-verification — 2026-10-24 — PASS

The section above records one finish run; the same 01b brief was also executed
independently to the end, and the two runs agree on every gate fact. The second
pass contributed no source changes (the merge was already up to date and
`511f4fdb` was already on `litter` when it started); what it did:

- **Spec-checked the banner by script** (not by eye): the four cartouche lines
  byte-identical to `511f4fdb^`; every byte in 0x20–0x7E; 15 lines, 67 columns
  max; every literal ends in a `\n` escape (no real newlines inside `concat!`);
  the commit's diff touches only the ART block and its doc comment.
  `cargo clippy -p taoxichuan --target x86_64-unknown-none -- -D warnings`: clean.
- **Re-ran the gate by hand — PASS.** Reinstalled the same 166,368-byte build
  (md5 `6e82bcc6fc591c0472f1d4b5b8bbd258`) into `/boot/akuma-guest-dev` and did
  the `vm-serve-mc` → `vm-dev` dance again: init spawn, banner, `stage 1 gate
  reached`, guest halted with `exits=4355 ... exc=0`, 0 `[Fault]`/`PANIC`/
  `[sig!]` lines. Evidence: `/root/dev-cycle/taoxichuan-s1/vm-dev-pass.log`
  (746 lines — this copy was taken after `/var/log/herd/vm-dev.log` had
  accumulated *both* boots, hence 2 × 373; the kernel's gate line is line 740
  of that copy, and the fault-grep is 0 across both segments).
- **Merge-regression boot repeated:** `akuma-amd64` rebuilt (33.3 s incremental
  vs the 46.7 s cold build above; byte-identical output, md5 `2b9d36ef...`, the
  same bytes that were already installed), booted to `AKUMA-GUEST-UP` →
  `AKUMA-GUEST-DONE`, `exc=0`, zero fault lines
  (`/root/dev-cycle/taoxichuan-s1/vm-dev-main-amd64.log`; the `-20261024`
  sibling is a segment-trimmed copy of the same boot).
- `vm-serve-mc` running at the end. Nothing above rewritten, nothing left
  uncommitted.

### Stage 1 — host track (Kimi, goose on the Mac), aarch64 check of the merged tree — 2026-10-24 — PASS

The supervision moved from Crush to goose (`k3-256k`, low effort) on the Mac; this is
that session's step 4 for stage 1, run after goose-glm's 01b finish landed on `litter`.

- **Merge:** `litter/taoxichuan-s1` @ `d2781860` into `rust-org-il-experiment`
  (`bd59f4aa`) — clean, no conflict (the expected agent-log append conflict did not
  materialize; the two sides' appends were disjoint). Tree gained only the skyline
  banner in `taoxichuan/src/main.rs` and the two metal-track log sections.
- **Compiles (aarch64):** `scripts/build_taoxichuan_fc.sh` → `taoxichuan-fc.bin`,
  662,792 B, 12.9 s incremental. No portability fix needed this round; the merge did
  not touch the aarch64 feature-set path.
- **Boots (Lima `fc`):** `FC_KERNEL=taoxichuan-fc.bin overlays/devbox-firecracker/run.sh
  --no-net`; console `/tmp/akuma-fc-boot.log`, 164 lines. Banner `Akuma Kernel
  starting...` (line 12), `[FS] Ext2 filesystem mounted at /` (line 77), `[Main]
  Starting userspace sshd (no supervisor)...` / `sshd started (tid=8)` (lines
  117-118). `grep -c "Fault\|PANIC\|sig!"` = 0.
- **Control:** the same image with `--no-disk` stops at `[Block] Block device not
  found` / `Continuing without filesystem...` and spawns no sshd (count 0) — the
  gate still means something.
- **Disagreements:** none. Both tracks pass the stage-1 gate, on different kernels:
  goose's new `taoxichuan` crate (amd64, PVH, halts at the gate line) vs the
  feature-set composition (aarch64, Firecracker, spawns a real sshd). Parity of
  intent, as recorded before.
- **goose-glm cost (from `sessions.db` on the box):**
  - stage 1 main, session `20261024_9` (`glm-5.3`): 77.2 min wall, 142 LLM calls,
    16,057,961 input tokens (15,736,512 cache reads, 98.0%), 68,932 output,
    largest context 181,081.
  - 01b finish, two independent runs (`glm-5.3-flash`, low effort): session
    `20261024_22` = 23.2 min, 47 calls, 1,416,929 in / 32,453 out, max_ctx 46,778;
    session `20261024_24` = 24.3 min, 48 calls, 1,774,288 in / 42,094 out,
    max_ctx 60,907. The two runs agree on every gate fact (see the re-verification
    section above). Flash at low effort costs roughly a tenth of the input tokens
    of the `glm-5.3` run for a paperwork-sized task.
- **Tooling note:** the duplicate 01b send (the `send_after_reset.py` leftover, see
  the user's note) ran to completion with no harm — its re-verification section is
  its output. The supervisor now sends only when no `goose_acp_prompt` driver is
  alive.

**Stage 1 counts: both tracks pass the gate.** Next: stage 2 (userspace works — a
static musl hello runs, exit 0, output matches the host-built binary), brief
`scripts/kvm/goose/taoxichuan-02-hello.md`.

### Stage 2 — host track (Sonnet), aarch64 / Firecracker in Lima `fc` — PASS after one supervisor fix

*2026-10-24 ~18:50 IDT, Sonnet (Kimi was gone; see Oversight change #2).* Merged `litter/taoxichuan-s2` @
`9a948c78` into `rust-org-il-experiment` (`ed1de19d`, clean) while goose-glm was already working on stage 3
(the next brief goes out before the aarch64 check, per the user's ordering).

- **Finding 1, fixed: the aarch64 kernel build broke.** goose added `crates/akuma-ring3-amd64` to the
  workspace `default-members`, so `scripts/build_taoxichuan_fc.sh` (a plain `cargo build` at the root)
  compiled its x86 `naked_asm!` for `aarch64-unknown-none`: 55 assembler errors (`sysretq`,
  `mov rax, [rsp]`). The other amd64-only crates (`akuma-boot-amd64`, `akuma-comconsole`) are not in
  `default-members`; this one was copied in with the arch-neutral crates. Fix: delete that one line from
  `Cargo.toml` (supervisor-sized, step 5). The crate itself was not touched.
- **Finding 2, left to goose (stage 3 brief section 1):** the crate's two `naked_asm!` functions carry no
  `#[cfg(target_arch = "x86_64")]`. `cargo check -p akuma-ring3-amd64` passes on the Mac (check does not
  assemble), `cargo test -p akuma-ring3-amd64` fails with 38 assembler errors. Same class as the stage 1
  `boot.s` break, one level later: `cargo check` is not the aarch64 gate for a crate with inline asm; building
  or testing it is.
- **Compiles (aarch64):** `scripts/build_taoxichuan_fc.sh` -> `taoxichuan-fc.bin`, 662,792 B (byte count
  unchanged from stage 1: the aarch64 kernel does not use the amd64 composition crates), 10 s.
- **Boots (Lima `fc`, 1 vCPU, `--no-net`):** console 133 lines: `Akuma Kernel starting...` (line 11),
  `[FS] Ext2 filesystem mounted at /` (76), `[Main] Starting userspace sshd (no supervisor)...` /
  `sshd started (tid=8)` (116-117). `grep -c "Fault\|PANIC\|sig!"` = 0.
- **Control:** same image with `--no-disk`: 80 lines, `[Block] Block device not found` (69), `Continuing without
  filesystem...` (70), zero sshd starts, zero faults. A pass needs the disk.
- **Honest limit:** stage 2's gate is "a static musl hello runs, exit 0, output matches the host-built
  binary". The aarch64 track did **not** run a hello: its evidence is the static musl `sshd` running at EL0
  from the same kind of ext2 root. That is a parity observation (musl userspace on a feature-set kernel
  is already routine here), not the byte-compare; the byte-compare is the metal track's, and it passed
  (SHA256 `7e96c998...`).
- **Parity:** no disagreement between the hypervisors this stage. The two kernels differ on purpose: the
  aarch64 one is the existing kernel by feature selection (every capability already there), the amd64 one is
  composed from 14 crates and a 198,072 B image.
- **goose-glm cost for stage 2** (`sessions.db`, session `20261024_26`, `glm-5.3-flash`, low effort): 85.9 min
  wall, 169 calls, 19,015,708 input tokens (18,828,800 cache reads), 127,408 output, largest context 179,941.
  That is more tokens than stage 1 (16.1M input, `glm-5.3`, which hit the 5-hour limit at 77 min) and it
  did not hit the limit: the user reads the z.ai quota meter as clearly cheaper per token on flash.
  Stage 2 start to finish took about 90 min, ~30 of them reading before the first edit.
- **Stage 3 started 18:48 IDT** (`taoxichuan-03-busybox.md`, narrowed by the user to non-interactive
  `busybox echo` and `busybox ls`; no ash, no pty).

### Stage 2 — metal track (goose-glm)

*2026-10-24, `glm-5.3-flash` (low effort), branch `taoxichuan-s2` (head `f76ff171`
at the gate; paperwork commits after). Merge of `origin/rust-org-il-experiment`
(`d59f0df1`) onto `taoxichuan-s1` was a clean fast-forward.*

**What was composed.** One new crate, `crates/akuma-ring3-amd64` (one line pulled
into `taoxichuan/Cargo.toml`, per the composition rule: extract first, then pull).
It is the ring-3 crossing at minimum size, host-tested where pure:

- `Image` — the `PT_LOAD` walk over the already-read image bytes, bounds-checked
  throughout. This is `akuma-elf` without `akuma-mmu`: eager placement, no demand
  paging, no interpreter, so pulling it does not pull the MMU.
- `build_stack` — argc/argv/envp/auxv in the Linux order musl's `__init_libc`
  walks; `AT_RANDOM` (deterministic canary — no entropy source, and the crate
  says so) and `AT_EXECFN` are filled by the builder. 8 host tests cover the
  parse (including truncated-hostile images) and the stack walk order.
- `enable_syscalls` + a naked `syscall_entry` stub + `enter_user` — `EFER.SCE`,
  `STAR` from `akuma_gdt::star()` (the row-order contract is the table's compile-
  time assertions), `LSTAR`, `SFMASK=0x47700` (TF|IF|DF|IOPL|NT|AC; `syscall`
  clears `IF` itself). The stub stashes the user RSP in a kernel static, builds
  a `SyscallFrame` on the crate's own 16 KiB kernel stack, `call`s a registered
  dispatcher, and `sysretq`s back. No `swapgs` anywhere — no per-CPU block.
- The x86_64 nr/err/arch/mmap ABI tables. `akuma-syscalls-linux` is the
  AArch64/asm-generic numbering (`write`=64); the amd64 kernel translates, and
  so does this crate — stated in its docs.

`taoxichuan/src/main.rs` grew the task: a fresh root page table sharing the
kernel's upper half by PML4 copy (the kernel's own root keeps the identity map;
the task's root has none — that retirement can happen when the lower half earns
its keep), every `PT_LOAD` placed eagerly at its link address (USER_RO/RX/RW,
`.bss` from the zeroed frames), the initial stack block copied into the top page,
`brk`/`mmap` as fixed 1 MiB slabs registered up front so a user-pointer check is
one range scan, `mov cr3` (the write stays unsafe at the call site, like
`ctlreg`'s docs say), `sysretq`. Dispatcher: `write`/`writev` into
`console::puts`, `exit_group` captured and printed, `brk`, `arch_prctl` through
`akuma_cpu::msr::set_fs_base` (the closed wrmsr path — no new MSR writes),
`set_tid_address`/`getpid`/`getuid` family, `ioctl`→`-ENOTTY`, everything else
`-ENOSYS` with its number on the console. A ring-3 fault prints `[Fault] ring3 …`
and parks the core — the kill path, not a triple fault. Unsafe budget updated via
`check_unsafe_budget.py --update` (the new crate's sites carry their reasons).

**The gate, and the one surprise.** Host side: `musl-gcc -static -no-pie` hello,
output saved (`hello from taoxichuan ring 3\n`, 29 bytes). Guest side, first live
boot after the clippy-green build — everything crossed (the KVM MSR trace shows
EFER 0xd00→0xd01, STAR `0013000800000000`, LSTAR, FMASK, then musl's
`arch_prctl(ARCH_SET_FS)` as `wrmsr c0000100 <- 0x405698`), exit code 0 captured
… and **no hello line**. The surprise: musl's `__stdio_write` flushes through
**`writev`, even for a single buffer** (the empty `wbase` head is a zero-length
iovec). The table answered `-ENOSYS`; musl dropped the output and `main` still
returned 0. `writev` added (`f76ff171`), and the very next boot:

    hello from taoxichuan ring 3
    Taoxichuan: hello exit code 0
    Taoxichuan: stage 2 gate reached -- ring-3 ran, exited clean, output above

`grep -c -e '\[Fault\]' -e PANIC -e '\[sig!\]'` over the boot's log segment
(73 lines, `/root/dev-cycle/taoxichuan-s2/vm-dev-pass.log`): **0**. Byte-compare:
guest line (CR stripped — `akuma-comconsole` emits CR-before-LF, its documented
newline rule) vs host `reference.out`: **identical SHA256**
(`7e96c99853398d7c8b6b2bbe5b97514e29f190f4fa9e30bbdd00af4455517f3c`). Stage-1
gate lines still in the log verbatim (banner, `init spawn`, `stage 1 gate
reached`); the banner now reads `-- stage 2`.

**Merge regression, main amd64 kernel** (`kbuild -j 4 --features no-tests`,
36 s, 11,089,888 B): booted twice as the same guest. First with the stage-2
cmdline — the full kernel ran the same `/bin/hello` end to end (`-- running
/bin/hello --` … `hello from taoxichuan ring 3` … `-- init exited --`,
`exc=0`, log saved as `vm-dev-main-amd64-hello.log`); then with the original
busybox cmdline — `AKUMA-GUEST-UP` → `AKUMA-GUEST-DONE`, zero fault lines
(`vm-dev-main-amd64.log`). The same binary runs on both kernels; that is the
parity evidence the experiment wanted. herd's generic `FAIL (banner=yes …)`
marker appears on both clean boots, as documented in stage 1.

**Ops notes.** The environment has no loop devices and no debugfs, so `/bin/hello`
went onto the ext2 image (`/boot/akuma-dev-root.img`, backup kept) by a direct
layout-exact Python edit (`ext2addfile.py` in the evidence dir): free inode +
21 data blocks + 1 indirect, tail-entry split in `/bin`, free-count bookkeeping;
read-back hash equals the source, busybox untouched. `herd enable` snapshots
`available/` into `enabled/` — a conf edit after enabling needs a re-enable;
cost one boot. `available/vm-dev.conf` is back on the stage-1 busybox cmdline;
the gate boots used `init=/bin/hello`. `vm-serve-mc` running at the end.

**For stage 3 (busybox ash):** the gap is breadth, not new machinery. The
dispatcher's `-ENOSYS` prints its number — boot busybox once and the console
lists exactly what ash needs (expect `rt_sigaction`/`rt_sigprocmask`,
`gettimeofday`/`clock_gettime`, `read`/`readlink`/`newfstatat`, `openat` at
minimum; musl's malloc already survives on the brk/mmap slabs). The syscall
stub restores everything, so a task that faults dies loudly; but ash's
`fork`+`wait` needs more than one task-static — the experiment pins "no
scheduler", so decide early whether a script runs as a sequence of child tasks
the kernel reaps synchronously. The ext2 image now also has a working
`/bin/hello` for smoke tests, and `scripts/taoxichuan_hello.sh` rebuilds both
the binary and the reference output.

### Stage 3 — host track (Sonnet), aarch64 / Firecracker in Lima `fc` — PASS; metal track at the gate, paperwork pending

*2026-10-24 ~20:00 IDT, Sonnet.* Gate narrowed by the user: non-interactive `busybox echo` and `busybox ls`, no `ash`, no pty
(brief `scripts/kvm/goose/taoxichuan-03-busybox.md`, sent 18:48 before the stage-2 aarch64 check, per the user's ordering).

- **Merge:** `litter/taoxichuan-s3` @ `6c4602b7` into `rust-org-il-experiment` (`e67714f6`), clean.
- **goose's cfg gating is complete:** `cargo test -p akuma-ring3-amd64` on the Mac host (aarch64-apple-darwin): 8 passed.
  This is the stage-2 finding closed: goose gated nine items first thing in stage 3, unprompted beyond the brief's one line.
  No new amd64-only crate this stage (`akuma-fdtable` is the existing arch-neutral fd state machine), so the `default-members`
  problem did not recur.
- **Compiles / boots (aarch64):** `scripts/build_taoxichuan_fc.sh` -> 662,792 B (unchanged; the aarch64 kernel does not use the amd64
  composition crates), 12 s. Firecracker boot in Lima `fc`, `--no-net`: 133 lines, banner (line 11), `Ext2 filesystem mounted`
  (76), `sshd started` (117), zero `Fault|PANIC|sig!`. Control `--no-disk`: 80 lines, `Block device not found`, no sshd, zero faults.
- **Honest limit (as in stage 2):** the aarch64 track runs a musl userspace (`sshd`) on the feature-set kernel; it did not run
  `busybox echo`/`ls` and compare bytes. The byte-compare is the metal track's.
- **Metal track, verified by the supervisor from the box (not goose's RESULT.md, which does not exist yet):** the last gate boot
  printed `ls /bin` -> `busybox`, `hello`, `sh` (exit 0) and `echo hello-from-taoxichuan-stage-3` (exit 0), then
  `Taoxichuan: stage 3 gate reached -- sequential tasks ran clean, outputs above` and `all programs done`. In
  `/root/dev-cycle/taoxichuan-s3/refs/`: `ref-echo.out == guest-echo.out` and `ref-ls.out == guest-ls.out` (`cmp` identical;
  SHA256 `137128a9...` and `ddec525c...` in `SHA256SUMS.ref`). 15 crates (14 + `akuma-fdtable`), `taoxichuan/src/main.rs`
  1,790 lines, release binary 266,752 B (198,072 B at stage 2). Iterations on the way: `echo: out of memory`, then unhandled
  syscalls 228 and 96, fixed by goose from the `-ENOSYS` arm's printed numbers.
- **Stopped by quota, not by failure:** at 19:56 IDT z.ai answered `Usage limit reached for 5 hour. Your limit will reset at
  2026-10-25 02:39:00` (China time, UTC+8 = **21:39 IDT**). goose-glm had no `RESULT.md`, no agent-log section, no
  main-kernel regression boot and the services mid-cycle; the supervisor restored `vm-serve-mc` and disabled `vm-dev` (the brief's
  own cleanup) and prepared `scripts/kvm/goose/taoxichuan-03b-finish.md` (the `01b` pattern), to be sent after the reset.
- **Cost, stage 3 so far** (`sessions.db`, session `20261024_28`, `glm-5.3-flash` low): 66.3 min, 169 calls, 17,566,605 input
  tokens (17,392,000 cache reads), 96,131 output, largest context 167,308. The window's stages together (stage 1 finish x2,
  stage 2, stage 3) are about 38M input tokens; the user read the z.ai meter at 90% at ~19:46 IDT, so the window is
  roughly 40M input tokens at flash weight (an estimate from one data point).
- **Also this stage:** the user asked that every brief tell goose to commit and push every 20 minutes when something is
  uncommitted (stage 3 sat uncommitted ~45 min), and to gate all amd64-specific code; both are in the 3b brief.
- **Update 22:05 IDT (Sonnet):** goose's 03b run (sent 21:44 after the 21:39 reset, driver exit 0, `end_turn`, ~19 min) added
  only `RESULT.md` and the log section below (`3b930292`, docs only; no `taoxichuan/`, `crates/` or `Cargo.toml` change since
  `6c4602b7`). The aarch64 results above therefore stand and were not re-run. Fast-forwarded to `litter/taoxichuan-s3`;
  stage 4 (`taoxichuan-04-sshd.md`) sent at 22:04.

### Stage 3 — metal track (goose-glm), finish — 2026-10-24 — PASS

*2026-10-24 ~22:00 IDT, `glm-5.3-flash` (low effort), the 03b continuation session. Branch `taoxichuan-s3`;
gate commit `6c4602b7`, merged `origin/rust-org-il-experiment` (`f22e0d5d`) as a clean fast-forward before any
of this. No code changes this session — composition was done at the gate; this is the paperwork.*

**What was composed (recap of the gate commit).** Over stage 2's ring-3 crossing, the fd table pulled in as
`akuma-fdtable` (the arch-neutral fd-decision state machine, zero deps; this kernel supplies the effects,
16 fixed slots) and the syscalls busybox needs to answer a prompt and list a directory: `open`/`openat`
(absolute paths, `AT_FDCWD` only, read-only), `getdents64` (from the listing cached on the open description),
`stat`/`lstat` (the legacy x86_64 numbers musl maps to), `fcntl` fd-flag commands, `rt_sigaction`/
`rt_sigprocmask` accepted no-ops (no delivery exists), `clock_gettime`/`gettimeofday` zeroed (no clock,
honestly). The task model is sequential by design: `exit_group` no longer halts, it reports and chains the
next program from the cmdline (`init=`/`initargs=`, `then=`/`thenargs=`), kernel-reaped — `fork` was not
needed because the gate is two non-interactive programs, and not having it kept the address-space story at
"one image at a time, eager-mapped".

**The bug of the stage** (in the gate commit message, worth restating): `SyscallFrame` listed r10/r8/r9 in
push order; the stack holds r9/r8/r10 — args 4 and 6 were swapped for every syscall with more than three
arguments. Latent through stage 2 (nothing took >3 args), fatal at the first mmap: musl's flags arrived in
the offset slot, the anonymous check failed, malloc OOM'd. The first symptom was `echo: out of memory`, the
second the `-ENOSYS` arm printing unhandled nr 228 (`openat`) and 96 (`getdents64`) — both fixed from the
printed numbers. Struct fixed to memory order; the offset test now pins all 15 slots.

**Numbers:** 15 crates (14 + `akuma-fdtable`), `taoxichuan/src/main.rs` 1,790 lines, release binary
266,752 B (198,072 B at stage 2).

**Gate evidence, clean single boot from the merged tree** (`/root/dev-cycle/taoxichuan-s3/vm-dev-pass.log`,
4,205 B; the previous file was a 3 KB trim): `kbuild -j 4 -p taoxichuan --features no-tests`,
`install_kernel_amd64.sh` → `/boot/akuma-guest-dev` (266,752 B, md5 verified), `herd start vm-dev`
(stage-3 cmdline kept: `init=/bin/busybox initargs=echo,hello-from-taoxichuan-stage-3 then=/bin/busybox
thenargs=ls,/bin`). Console: `hello-from-taoxichuan-stage-3` (exit 0), `ls /bin` → `busybox`, `hello`, `sh`
(exit 0), `Taoxichuan: stage 3 gate reached -- sequential tasks ran clean, outputs above`, guest halts.
Fault grep (`page fault|panic|oops|unhandled`) over the segment: **0**; KVM `exc=0`.

**Byte-compare (done at the gate, verified by the supervisor, recorded here for the file):**
`ref-echo.out == guest-echo.out` and `ref-ls.out == guest-ls.out`, SHA256
`137128a9…` (echo) and `ddec525c…` (ls) in `refs/SHA256SUMS.ref`.

**Main amd64 kernel regression boot as guest** (`vm-dev-main-amd64.log`, 10,976 B): `kbuild -j 4 --features
no-tests` (akuma-amd64, 11,089,888 B) into the same `akuma-guest-dev` slot, cmdline back on the stage-1 line
`init=/bin/busybox initargs=sh,/etc/guest.sh`: banner, ext2 root mounted, SmolNet up (virtio-net, DHCP
configured, 10.0.2.15/24), `AKUMA-GUEST-UP`, busybox md5 `ebce43017d2cb316ea45e08374de7315`,
`AKUMA-GUEST-DONE`, init exited, guest halted; fault grep **0**, `exc=0`. DNS/SNTP timed out as expected
(no upstream). Services restored: `vm-serve-mc` running (pid 8815), `vm-dev` stopped and disabled,
`/etc/herd/available/vm-dev.conf` left on the stage-1 busybox line.

**New gotcha for the next VM dance:** the KVM loader does not read `/etc/herd/available/vm-dev.conf` at
start — it reads the compiled `/var/run/herd/vm-dev.vm`, and `herd enable` on an already-enabled service
does NOT re-emit it from the (updated) conf; the daemon keeps its own first-enable copy. Editing the conf
and stop/enable/start still booted the old cmdline twice before I found it. Fix: `sed` the `.vm` file
directly, then `herd start` (start does not reload config). The guest-side tell is cheap: wrong cmdline
prints `hello-from-taoxichuan-stage-3`, right one prints the busybox md5.

**Cost:** this session cannot see its own meter (no `sessions.db` readable from inside); the supervisor's
stage-3 numbers above (session `20261024_28`: 66.3 min, 169 calls, 17.57M input / 96K output) stand, and
this continuation adds the paperwork boots (two guest boots, one 53 s kernel build) plus docs on top.
`RESULT.md` written at `/root/dev-cycle/taoxichuan-s3/RESULT.md` with the stage-4 advice (sshd needs: a
network path — virtio-net exists in the main kernel's SmolNet, not in taoxichuan; sockets — none in
taoxichuan today; fork-or-not — taoxichuan is sequential-only, no fork/clone/execve; `/dev/null` and
`/dev/urandom` — no device layer, and no entropy source, `AT_RANDOM` is still the deterministic canary;
host key — no writable files, read-only `open` only).

### Stage 4 — supervisor interventions and measurements (Sonnet), 2026-10-24 21:44-23:35 IDT

Not a gate result. This is what the supervisor did while goose-glm worked the sshd stage, so the next supervisor
does not have to rediscover it. Times are IDT; the box's own clock and `/var/log/herd` are UTC (-3 h).

**Timeline.**
- 21:44 the detached sender fired `taoxichuan-03b-finish.md` after a probe returned "pong" (quota reset 21:39).
  03b ran 18 min, `end_turn`: RESULT.md PASS, main-kernel regression boots, log section (`3b930292`).
- 22:04 stage 4 (`taoxichuan-04-sshd.md`) sent before validating stage 3 (docs-only delta; the aarch64 results
  stood). Goose wrote `net.rs`, `child.rs`, `sock.rs` and the syscall arms from 22:25, first build 22:52, and
  made its first commit at 23:11 (`0f1c9926`, 67 min in, past the brief's 20-minute rule).
- 23:20 first real stage-4 boot (kernel 604,696 B, `init=/bin/sshd`, forward `2224:22`): the guest reaches
  `fs: ext2 root mounted on /dev/vda`, then the log goes silent (no banner, no init spawn); `vm-dev`
  `last_exit=1`. This is the lead handed to the continuation brief.
- 23:30:22 supervisor sent ACP `session/cancel` (connection and session ids are in the head of the driver log);
  HTTP 202, turn ended `stopReason: cancelled` at 23:30:49 and the driver exited 0. User's call: 72% of the
  window was spent and the remaining quota was better used on one named failure.
- 23:30-23:33 supervisor: restored `vm-serve-mc` and disabled `vm-dev`, committed goose's uncommitted files on the
  box as `e418fd61` (user authorised it; no push), stopped and started `goose-glm` (new pid), wrote and sent
  `taoxichuan-04b-continue.md` (about 25-30 min of work; commit and push every 10 min; PARTIAL result if not
  done).

**Quota, measured from `sessions.db` (`scripts/kvm/goose_box_stats.py`, run through ssh as base64).** The
window is about 41M input tokens (stage 4 plus 03b were 27.57M at 23:25 = 67%, the user read 69%). Stage 4's burn
rate rose as its context grew: 6.9M in the first 35 min, 13.3M at 49 min (16M/h, context 190k), 26.4M at 80 min
(+13.1M in 32 min = 25M/h, about 60% of a window per hour, context 220k). An earlier "5% an hour" estimate was
wrong: it averaged over the cheap 03b run and idle time. Lesson: a long stage's cost per hour is dominated by
the context it re-reads on every call, so a window lasts about 2 h of stage-4-sized work, not 5.

**Findings.**
- **`goose serve` pins a core even with nothing running** (one tokio worker, 1,001-1,002 ticks per 10 s,
  also with no prompt in flight); a stop/start cleared it. `herd restart` is not a command. Detail and the probe
  plan: `GOOSE_SERVE_IDLE_SPIN.md`.
- **`/proc/stat` per-cpu rows for cores 0-3 are not monotonic** (negative deltas); aggregate and cores 4-7 are
  sane. Separate accounting bug, recorded in the spin doc.
- **The cloc script's "links" footprint was wrong in three ways**, found while answering "are we checking linked
  code correctly?": (1) it read `path =` lines out of `Cargo.toml` text, which counts optional and other-arch
  dependencies (taoxichuan 29 crates by text, 27 by `cargo tree`); (2) it counted whole crates regardless of
  features; (3) a crate in the dependency graph is not code in the image. Check against an unstripped aarch64
  `taoxichuan,platform-firecracker` kernel: 85 crates in the graph, 52 put any symbol into the binary; 23 external
  crates in the graph, 7 contribute 10.5 KB (about 1% of 843 KB of symbols) although 66.5k external source lines
  were counted; `smoltcp` (24.7k lines), `zerocopy`, `heapless` contribute 0 bytes. The script now uses
  `cargo tree` and has `--external` (libraries) and `--elf` (symbol bytes per crate; needs
  `CARGO_PROFILE_EXTREME_SIZE_STRIP=none`, as extreme-size strips). Symbol attribution under-counts inlined code.
  Numbers: `LINE_COUNT_ANALYSIS_POST_AMD64.md` Stat 6.
- **The aarch64 path cannot reach goose's stage-4 code.** The aarch64 "Taoxichuan" kernel is the full Akuma
  kernel behind a feature; its Firecracker boot is green (662,792 B, 134 lines, 0 faults, sshd started; `--no-disk`
  control 80 lines) but an ssh attempt through the Lima tap (`guest-setup.sh` run, port 4444) did not connect
  within the user's patience, and it would have exercised the full kernel's sshd, not `net.rs`/`child.rs`.
  Left as is; the real test is the amd64 guest.

**Side effects the next supervisor should know.** (1) To back up goose's work I ran `git add -N taoxichuan crates` on
the box, which marked new files intent-to-add in the index (contents unchanged). Backups are in the supervisor's
scratchpad, not in the repo. (2) `guest-setup.sh` created `tap0`, NAT and the 4444 forward inside the Lima VM `fc`.
(3) Merged `litter/taoxichuan-s4` into `rust-org-il-experiment` locally (`78bdaf10`) mid-stage at the user's request.

### Stage 4 — host track (Sonnet), aarch64 / Firecracker in Lima `fc` — PASS; metal track PASS (goose-glm, 04b)

**Metal result (goose-glm, session `20261024_34`, `/root/dev-cycle/taoxichuan-s4/RESULT.md`).** PASS at
`taoxichuan-s4` @ `29d4b562`, pushed to `litter`. An ssh round trip into the Taoxichuan guest
(`init=/bin/sshd initargs=--port,22`, host 2224 -> guest 22 through the relay) runs `echo` and
`cat /etc/sshd/sshd.conf` with byte-equal output and zero `[Fault]`/`PANIC`/`[sig!]` lines; exec-only, no pty. 16
crates in the dep tree (stage 3: 15), image 604,712 B. The boot chain it proved: ext2 root, TSC calibrated against the PIT, virtio-net at slot 1
(polled), smoltcp + DHCP lease `10.0.2.15/24`, banner, init spawn, ring 3, sshd listen, key exchange, exec.

**Three stacked causes (goose's finding, verified in the commits):** (1) the 23:20 IDT "hang after the root mounts"
that the 04b brief described was a **stale guest binary** (`/boot/akuma-guest-dev` built before the supervisor's checkpoint), not the tree, so
the brief's lead was wrong and goose said so; (2) the real kernel bug: `mmap` demanded `fd == -1` for
`MAP_ANONYMOUS` while libakuma passes `fd = 0` and Linux ignores it, so sshd's first allocation returned `ENODEV`
(`31b934fa`, `29d4b562`); (3) the gate recipe's `ssh -T` is OpenSSH syntax and the box has only the akuma client
(`/bin/ssh -p 2224 root@localhost '<cmd>'`). Boot markers in `net_init` were left in (16 bytes).

**Supervisor verification.** (a) From the Mac, a plain `ssh -p 2224 root@192.168.1.159 '<cmd>'` with the user's key
succeeded on the first attempt after `herd start vm-dev`, so the relay is reachable from the LAN, not only
from the box. (b) aarch64: merged `litter/taoxichuan-s4` as `16f79ff9`; the Taoxichuan-feature kernel is still **662,792 B**;
`cargo test -p akuma-ring3-amd64` 8 passed; Firecracker boot with the disk is 132 console lines, 0
`Fault|PANIC|sig!`, "Ext2 filesystem mounted", "sshd started"; the `--no-disk` control is 80 lines with
"Block device not found" and no sshd (it proves the first two lines come from the disk). The aarch64 kernel does not
contain stage 4's `net.rs`/`child.rs`, so this shows no regression, not that stage 4 works there.

**Open items found while using the guest (not gate failures).** `ls /bin` over ssh prints "ls: cannot access
/bin" (stat/getdents on that path); the console prints `[sc] unhandled nr 4415` repeatedly (an unimplemented
syscall number nobody has identified); `uname` answers `Akuma 0.1.0 Akuma-OS x86_64` for `-a` and plain `Akuma`
for `-r` and `-m` (hardcoded in sshd/paws, per the user; not a kernel matter); sshd tolerates `unhandled nr 258` (`set_robust_list`) and `nr 5`. Goose's stage-6 list: outbound
relay (`vm_outbound`) and DNS, `writev`/`readlink` class arms, a wall clock (`utc_seconds` is `None`), untested 1 MiB
mmap/brk slabs.

**Cost.** Stage 4 part 1 (`20261024_31`): 80 min, 163 calls, 26.4M input at the last full reading (23:25); part 2 (`20261024_34`,
after the stop/start): **19.1 min, 59 calls, 2.02M input, 28k output, largest context 59k.** The window (~41M input
tokens) was about 76% spent at the end. The restart paid for itself: a fresh 59k context cost a tenth of the
220k-context session per call. The `goose serve` idle spin did not reappear in this ~20-minute session
(`GOOSE_SERVE_IDLE_SPIN.md`).

**State left on the box.** `vm-dev` is running at the user's request so they can ssh in (`vm-serve-mc` halted); restore with
`herd stop vm-dev; herd disable vm-dev; herd start vm-serve-mc`. The next stage needs a quota refill (reset ~02:39 IDT) and a stage-5
brief; none is written.

### Stage 6 — planning and session A (offline apk), supervisor view (Sonnet), 2026-10-25 00:02-01:05 IDT

**Plan.** goose-glm (planning only, 15 min, ~3% of a window) wrote `TAOXICHUAN_STAGE6_PLAN.md` (`f61ca77e`): ~32 missing
core syscall arms in three phases, `nr 4415` identified as the private `UPTIME` (`0x1000 + 319`; verified), outbound
via `vm_outbound = true` and a host-local HTTP mirror, the clock from `epoch=` on the kernel command line, milestones
M0-M7, 2-3 sessions. Supervisor concern recorded: the plan's host reference ran `apk.static` on the Akuma box itself, so a
bug could be shared by reference and subject; the supervisor computes the reference on the Mac with plain `tar`.

**Staging (supervisor).** Because the quota left (~20%) would be lost at the 02:44 reset, the user asked for a no-network
variant: `/boot/akuma-dev-root-apk.img` = the stage-4 root (16 MiB, fsck-clean, 2,053 blocks free after) plus
`/bin/apk.static` (apk-tools-static 3.0.8-r0, **static-PIE**, 5,470,888 B), `/pkgs/ca-certificates-bundle-20261020-r0.apk`
(real, signed, no deps, no scripts), the Alpine release keys and apk config. Built on the Mac with e2fsprogs `debugfs -w`
(not resized, to avoid new ext2 features the guest driver may not read), moved to the box over HTTP with matching sha256,
`.orig` kept for fresh-copy boots. Independent expected result: `/etc/ssl/certs/ca-certificates.crt` sha256 `5961c0a3...3f568b`
plus three symlinks (`/root/dev-cycle/taoxichuan-s6/EXPECTED.txt`).

**Session A (`taoxichuan-06a-offline-apk.md`, sent 00:33).** goose committed the ET_DYN static-PIE base in the ring-3 loader
plus phase A/C syscall arms (`06070926`, host tests green incl. a new ET_DYN test) in 20 min, booted the apk image
cleanly, then stopped on the quota at 01:04:08 (the error prints China time: reset `2026-10-25 07:44:08` = **02:44:08 IDT**,
not 02:39: the window opens at the first request). Services restored by the supervisor; tree clean; nothing lost.

**Finding that reframes stage 4.** `/var/log/herd/vm-dev.log` shows for every ssh exec: `'/bin/sh' failed to spawn,
retrying with the built-in emergency shell /bin/paws`, then `both failed to spawn, running built-in paws in-process`. This is in
the stage-4 boot too. So the stage-4 gate passed (`echo`, `cat`) on **paws builtins inside sshd; no external binary has ever
been spawned from an ssh session in the Taoxichuan kernel**, which also explains `ls /bin` failing. The gate as pinned in the
experiment doc (output byte-equal, exec-only) is satisfied; "sshd spawns a real command" is not. Fix is the first item of the
next brief (`taoxichuan-06b-spawn-then-apk.md`); G1 (`apk.static --version`) cannot pass before it.

**Fans.** The user's fans were the `vm-dev` guest, not goose: the Taoxichuan kernel's sleep loop polls the NIC and never halts
its vCPU, so core 4 sat at 100% (`guest=99.8%` in the host's `[KVM] cpu:` line) while `vm-serve-mc`'s full kernel idles at ~7%.
Rule: stop `vm-dev` when a session ends. A `hlt`/idle path in the guest's wait loop is a real follow-up (needs a wake source:
kvm0 raises no NIC interrupt, so it needs a timer exit). `goose serve` did not spin after the stop/start restarts
(`GOOSE_SERVE_IDLE_SPIN.md`).

**Quota (from `sessions.db`).** Session A (`20261024_35`-ish) used the last ~18% of the window in ~31 min (full read
unavailable after the stop). Window arithmetic: ~41M input tokens per 5 h; stage 4 part 1 burned it at ~60%/h with a 220k
context, part 2 and the planning and 6A sessions at ~10-35%/h with fresh contexts.

### Stage 6 — session B (`taoxichuan-06b-spawn-then-apk.md`), supervisor view (Sonnet), 2026-10-25 02:31-03:40 IDT (in progress)

**Send.** Quota reset confirmed by a probe (`pong`) at ~02:30 after a `herd stop`/`herd start` of goose-glm; brief sent 02:31
(`--name s6b`, log `/tmp/goose-s6b.log`). A delayed sender scripted for 02:45 was killed: the user said the window had
already reset (the 02:44 figure was the *previous* window's end; the quota meter had been reset by the user's own session
start). Lesson: probe first, do not schedule against a computed reset time.

**Root cause of "sshd never spawned an external binary" (found by goose, four stacked layers, each hiding the next).**
1. `read_link_target` (`taoxichuan/src/main.rs`) called `read_file` on a **fast symlink** (target stored in the inode's
   pointer words, no data blocks): the first target bytes were fed to the block mapper as a block number, SPAWN returned
   ENOENT, sshd fell back to built-in paws on every exec. Every busybox link on the image is a fast symlink. Fix:
   `read_symlink` handles both forms (`be128b97`).
2. `child_enter` (`child.rs`, naked asm) loaded `ef.rsi` early and kept dereferencing the loaded value (0): page fault
   cr2=0x20 on the first real SPAWN. It had never executed before layer 1 was fixed. Fix: the verbatim `enter_user`
   register order, frame `rsi` last (`e8c253d0`).
3. `stat(2)` did not follow the final symlink, so busybox ash's PATH search saw `S_IFLNK|0777`, judged it "not a program"
   and printed `Permission denied` **without calling fork or execve** (proved by the per-syscall child trace). Fix: STAT
   resolves through `resolve_effective` like `newfstatat` (`36b126a8`).
4. There was no `execve`; `sh -c '<one command>'` tail-call-execs. Added as **exec-replace in the nested child**
   (`child.rs::exec_replace`). Two traps: the second crossing overwrites the shared save slots (snapshot them), and every
   child syscall re-enters at the same nested stack top (lower it below the exec arm's own frame before crossing, or the
   exec'd child's first deep syscall rewrites `[RET_RSP]`: `ret` to `rip=0`). `load_task` now resets `regions`/`n_regions`;
   nested `writev` to fd 1 goes to the child pipe (musl flushes stdout as `writev`; `3c560be7`).

**Gates.** G1 PASS (`ssh -p 2224 root@localhost 'sh -c "echo hi"'` -> `hi`, rc 0, no "failed to spawn" line). G2 PASS
(`apk.static --version` -> `apk-tools 3.0.8-r0, compiled for x86_64.`; the 6A static-PIE loader base works, 18 startup
mmaps). G3 first FAILED with a guest PANIC on the first install write (`range start index 18446744073709551608 out of
range for slice of length 512`): `build_stack` sliced backwards from `buf.len()` and apk's long argv/env overran the
512-byte initial stack block by one pointer word; fixed by making it a full page (`28dc8ba2`). Next, `OPENAT` still went
through the read-only-era arm and answered EBADF to apk's open-the-db-dir-then-lock-relative-to-the-fd calls; it now goes
through `resolve_at` (`f4f119a2`). G3 then reached apk's own error: **apk 3 does not resolve a `.apk` file path argument**
("unable to select packages: apk.static (no such package)"). G4 not reached. Goose's `RESULT-B.md` (on the box in
`/root/dev-cycle/taoxichuan-s6/`) is being rewritten as it continues.

**Supervisor interventions.** None to the code. The aarch64 validation of the merged tree is pending until goose's
driver exits (the Mac was busy: `./ck3` ~5 cores).

**Quota.** 26% at 03:10, 37% at 03:20, 47% at 03:40 (~70 min in): ~40%/h early, ~60%/h at the 220k-context peak, back to
~30%/h. Leaves enough to reach the 07:30 reset only if the burn stays near 30%/h.

**Open.** `access`(21) arm; remove the temporary `[sc.c]` per-syscall trace before the clean gate boot; the `.apk` path
form versus a name-based `--repository /pkgs add ca-certificates-bundle`; sshd's cosmetic `Unknown argument: /bin/sshd`;
the 6A brief file was never committed (G1-G4 were reconstructed from `/pkgs/README`); a `hlt` idle path in the guest
wait loop (the `vm-dev` fan problem).

### Stage 6 — session B, host-track verification and the apk reference check (Sonnet), 2026-10-25 03:40-04:25 IDT

**Merged tree, aarch64 (Lima `fc`, Firecracker).** Goose's driver exited at 03:40 (`taoxichuan-s4` @ `f4f119a2`, merged as `21705006`).
`scripts/build_taoxichuan_fc.sh` -> `taoxichuan-fc.bin` 662,792 B (unchanged). Boot with disk: 163 lines, `Ext2 filesystem mounted`,
`sshd started`, 0 `Fault|PANIC|sig!`. `--no-disk` control: 125 lines, `Block device not found`, no sshd, 0 faults. PASS.
(Both runs write the guest-side `/tmp/akuma-fc-boot.log`, so fetch it with `limactl shell fc -- cat` between runs; a second run overwrites it.)

**How apk is supposed to work (real Linux, the same Alpine 3.23 aarch64 `apk.static` 3.0.8-r0 and the same ca-certificates-bundle).**
`apk.static --root R --initdb --no-network --no-cache --allow-untrusted add /abs/pkg.apk` and `... add ./pkg.apk` both work, but
**only with `--force-non-repository`** when `--no-cache` is given: without it apk refuses ("You tried to add a non-repository
package to system, but it would be lost on next reboot"). With it, both path forms install and `ca-certificates.crt` is
`5961c0a3...3f568b`, the value in `EXPECTED.txt`. The name form (`add ca-certificates-bundle` with `--repository DIR`) fails on
Linux too (no index), so it is **not** a way forward. Linux's error for a bad name is `unable to select packages: <name> (no such
package)`, the same text Akuma printed. Note the package it named on Akuma was `apk.static`, not the path: the positional
argument apk saw after `add` was its own program name, so the amd64 exec-replace / `build_stack` argv layout is the lead (an
argv shifted by one, or argv[0] repeated), not a path-resolution bug. G2 hid this because `--version` ignores positionals.
Both are hypotheses for the next session; neither is verified on the amd64 guest.

**Static apk on the aarch64 kernel.** On the *stock* aarch64 kernel (`FC_FEATURES=devbox-smoltcp,no-tests`, Lima `fc`, a copy of
`disk.img` plus the Alpine aarch64 `apk.static` and package, ssh on :4444): `--version` OK; the path-form install into a fresh
root works in both `/abs/pkg.apk` and `cd /pkgs && ./pkg.apk` forms, and the installed `ca-certificates.crt` is `5961c0a3...3f568b`.
Without `--force-non-repository` Akuma installed too, where Linux refuses (Akuma's `apk` does not detect the volatile root).
**On the Taoxichuan aarch64 kernel itself it cannot be run**: that build has no network ("built without the smoltcp feature",
`rx posted=0`, so no ssh) and `execve` is refused (`with_own_process_exclusive without the BKL — refused`, every exec from a
test driver spawned as `/bin/sshd`), so there is no way to launch `apk.static` there. The result above is therefore evidence that
apk-tools itself and the aarch64 syscall layer are fine; it says nothing about the amd64 Taoxichuan kernel.

**For the next brief.** Add `--force-non-repository` to the G3 command (or explain the refusal); dump `argv` as the new image
sees it before apk's parser runs (`apk.static --root / add <path>` should list exactly one positional); keep the Linux
transcript above as the oracle. Ryzen was taken offline by the user at ~03:45, so nothing is running there; `vm-dev` was
already stopped by goose and `vm-serve-mc` restored.

### Stage 6 — session C (`taoxichuan-06c-apk-argv-and-install.md`), host track (Sonnet) — metal track PASS, offline `apk add` works

Brief sent 04:22 IDT after the user brought the ryzen back (GRUB entry 30 via `arm.sh 29`; herd `vm=true` entry), goose-glm probe `pong`.
Goose ran 04:22-05:35 IDT (73 min, past the brief's 60-minute line, left running because it was committing real fixes) and wrote
`/root/dev-cycle/taoxichuan-s6/RESULT-C.md`: **G1-G4 all PASS in one clean boot** (`taoxichuan-s4` @ `0b33595a`, kernel sha256
`58353c90...47780`, fresh copy of `.orig` per boot, 0 `Fault|PANIC|sig!`).

- **G3:** `apk.static --no-log --root / --no-network --no-cache --allow-untrusted --force-non-repository add
  /pkgs/ca-certificates-bundle-20261020-r0.apk` -> `(1/1) Installing ca-certificates-bundle (20261020-r0)`, `OK: 174 KiB in 1 package`,
  rc 0. (`--initdb` is rejected by apk 3 as a global option; the image's `/pkgs/README` still shows it.)
- **G4:** guest-side `sha256sum /etc/ssl/certs/ca-certificates.crt` = `5961c0a3...3f568b` = `EXPECTED.txt`; the three symlinks exist and resolve.

**The argv problem (the supervisor's first suspect, one level down).** `load_task` declared `full_argv: [&str; MAX_ARGV + 1]`
pre-filled with `argv[0]` in every slot, copied only `argv.len()` entries over it and handed `&full_argv[..=n_args]` to
`build_stack`: one slot past the prefix, so the exec'd image saw a trailing duplicate of `argv[0]`. apk 3's `add` parsed it as a
package name ("unable to select packages: apk.static (no such package)"); G1 (the dup lands as `$0` after `-c`) and G2 (`--version`
ignores positionals) could not notice. The same fix removed sshd's cosmetic `Unknown argument: /bin/sshd`. `MAX_ARGV` was 8 against a
gate command of ~10 entries (the `.apk` path would have been dropped silently): now 16. Commit `ea2c5b2a`.

**Two more bugs in the same boot.** (1) `write_file_arm` applied `*offset += n` per iteration and also wrote at `*offset + done`, so
chunk 2 of a multi-chunk `write()` landed 1 KiB too far: apk's files came out as alternating 1 KiB data / 1 KiB zeros and 246,272 bytes
instead of 178,791. Fix: capture the start offset once (`18a7b193`); the extracted file is now byte-identical to a host extraction.
(2) `utimensat` was unhandled: apk's post-install "preserve mtime" pass counted ENOSYS as an error (rc 1 on a clean install). Arm added
(validate target, succeed without stamping: no wall clock) and the number fixed: `nr::UTIMENSAT = 88` is asm-generic, x86_64 is **280**
(`0b33595a`). `access`(21) also added, through `resolve_at`.

**The detour (ops, not kernel; cost the middle hour).** After the argv fix the gate said `apk.static: not found` and `ls` lacked
`/pkgs`, `/lib`, `/var`: it looked like ext2 directory corruption and cost a pass through parse/virtio instrumentation. The guest was
booting the wrong disk: **`herd enable vm-dev` is a no-op when already enabled**, so `/etc/herd/enabled/vm-dev.conf` still held session B's
end-of-session restore (the stage-4 ssh image). **Rule: edit both `/etc/herd/available/` and `/etc/herd/enabled/` (or disable+enable)
and confirm the boot from the `guest disk:` line in `/var/log/herd/vm-dev.log`.** Also: verify large payloads by guest-side hashes, not
by piping them out (a 246 KB `cat` over the ssh pipe came back with a 127-byte insertion).

**Host track.** Merged (`e91c5bdc`), `scripts/build_taoxichuan_fc.sh` -> `taoxichuan-fc.bin` 662,792 B (unchanged), Lima `fc` boot with
disk: 161 lines, `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`; `--no-disk` control: 125 lines, no sshd, 0 faults. PASS.

**Still missing.** `ls` through a symlink to a directory (`ls /etc/ssl1.1/certs`) fails rc 1 (file symlinks resolve through two hops;
look at `resolve_effective` + directory open); `dup2`/`dup3`(33) unhandled (busybox `dd` refuses); sshd logs `bridge_process: couldn't
open stdin` per session; the next ladder step (stage 7+) has no brief. State left: `vm-dev` stopped + disabled, both confs restored,
`vm-serve-mc` up. Nothing pushed to `origin` by goose; `litter` has `ea2c5b2a -> 18a7b193 -> 0b33595a`.

### Stage 7 — sessions A and B (outbound TCP), 2026-10-25 — H1's cause found; session B stopped by the z.ai weekly limit

**Session A** (`taoxichuan-07-outbound.md`, 53 min, 117 tool calls, 7.6M input tokens, `RESULT-A.md`): H1 failed, H3 passed, H2 partial.
Its brief used the gateway's own address `10.0.2.2` as a host alias. That was the supervisor's mistake (no guest ever used it that way;
`routable()` excludes the guest subnet by design), and it sent the agent to a "needs a host-kernel rebuild" dead end. The control run
to the host's LAN IP worked through the unchanged relay. The host kernel (`43edbffc`) is identical to latest for `amd64/` and
`crates/akuma-svm-amd64/`: nothing to rebuild.

**Session B** (`taoxichuan-07b-outbound-docs-first.md`, the first brief with a read-first docs list, the LAN-IP gate and an
event-based commit rule; 106 min, 275 tool calls, **27.8M input / 120k output tokens**, `litter` `11fe2194 -> 93fdc7c6 -> 1ee0dcdd ->
6879eb47 -> 2c494d96 -> b7646dd9`). Ended by `Rate limit exceeded: Weekly/Monthly Limit Exhausted` (resets 2026-10-31 01:05 China time)
with no `RESULT-B.md`.
- **Root cause of H1 (goose's own trace, not a supervisor hint):** `writev` served fds 1/2 only and answered `EBADF` for everything
  else. musl stdio flushes through `writev`, so busybox `wget`'s request never left, stdio swallowed the error and `wget` blocked
  reading a reply to nothing. `connect` returned in 52 ms; the handshake, gateway and relay were fine. After `6879eb47` the relay
  read `to_host=81 to_guest=201` and the host's `python -m http.server` logged `GET / HTTP/1.1 200`. The guest-side
  `hello-from-host` was not captured in a clean gate boot (exec output needed long timeouts on a cold guest). The same hole existed
  for regular files and was closed in `writev` too. See "Recurring bug classes" above.
- **H2:** `/dev/null` open (`OpenFile::Null`, `2c494d96`); gate output not captured. **H3:** `echo x > /tmp/f` creates the file but
  the bytes do not land; probes were being added to both file-write arms when the limit hit (`b7646dd9`, WIP committed by the supervisor at the stop).
- **Metrics (`scripts/goose_session_stats.py`):** A: 117 calls, 59 before the first edit, 1 `docs/` read, 4 boots. B: 275 calls, 45
  before the first edit, 11 `docs/` reads over 6 files (`kvm.md` 3, `KVM_OUTBOUND_NETWORKING.md` 3, `networking.md` 2), 76 source
  reads, 9 boots, 5 commits and 5 pushes by the agent. The commit rule held (a commit after the first clean build, one per fix).
  **What the numbers do not show:** that the docs sped anything up. The docs and a mid-session nudge both pointed at the NIC/rx
  ring, which was wrong; the cause came from the agent's per-lap trace. B also cost 3.6x A's input tokens. Three things changed
  between A and B (docs list, gate address, commit rule), so there is a difference, not a cause.
- **Supervisor errors this stage:** the `10.0.2.2` gate (A); a progress report claiming a 5-minute cadence I could not keep between
  turns (polling now happens inside the turn); a nudge that suggested the NIC when the bug was in the fd arm. Model used by goose
  throughout: `glm-5.3-flash`, `thinking_effort: low` (checked in `sessions.db`).
- **Ops state at the stop:** `vm-dev` stopped + disabled, both confs restored to the stage-4 conf, `vm-serve-mc` up, `:8123` listener
  killed, goose-glm restarted. **Next (stage 7C, after the quota reset):** clean gate boot for H1 (guest prints `hello-from-host`),
  H2 (`dd` to `/dev/null`), H3 (write-after-create), stage 6 G1-G3 regression; docs list only, no hints about the bug.
- **Host track (Sonnet):** merged `litter/taoxichuan-s4` @ `b7646dd9` (`d546b3db`), `scripts/build_taoxichuan_fc.sh` ->
  `taoxichuan-fc.bin` 662,792 B (unchanged), Lima `fc` boot with disk: 162 lines, `Ext2 filesystem mounted`, `sshd started`,
  0 `Fault|PANIC|sig!`. PASS (the tree still builds and boots on aarch64; the amd64-only changes are in `taoxichuan/`).

### Supervisor lapse — the composition rule was dropped from the briefs for stages 6-7 (found 2026-10-25, user's question)

**What happened.** The experiment's pinned composition rule (`MINIMAL_SELF_HOSTING_EXPERIMENT.md` § Process: pull as few crates as possible;
extract into a crate first where the logic still lives in `amd64/src/`; the goal is a *composition root*, not a new kernel) was carried by
the briefs for stages 1-4 and silently left out of 06a, 06b, 06c, 07, 07b and 07c-draft. The supervisor (Sonnet) wrote each brief from
the previous session's result and never re-read the doc's pinned sections. The user noticed from the size of the kernel source.

**What it cost (measured, `git diff --numstat 29d4b562 b7646dd9`).** Stages 6-7 added **+1,198 net newly written lines in `taoxichuan/`**
(`main.rs` +911, `sock.rs` +129, `child.rs` +158; `taoxichuan/src` 3,268 -> 4,466) against **+252 in `crates/`** (`akuma-ring3-amd64` +170 loader,
`akuma-net` +70 the connect-trace helper), **zero new crates, zero lines changed in `amd64/` or `src/`**: nothing was extracted, and
`dup2`/`dup3`, `utimensat`, `access`, the `writev` family, `/dev/null`, the socket arms and the exec/argv work were all written again for a
kernel whose full-amd64 sibling already had them. Two of the stage 7 bugs (`writev` served fds 1/2 only; `/dev/null` open returned EACCES)
were such re-implementations, and the first took the better part of a 106-minute session to find ("Recurring bug classes" above).
During the stage 7C planning session goose also noted that `taoxichuan/src/net.rs` hand-writes port `in`/`out`, which `CLAUDE.md` forbids
(`akuma-port` owns port I/O).

**Fix, and what was learned.**
- `scripts/kvm/goose/BRIEF_TEMPLATE.md` now has a mandatory "Composition rule" section (look in `crates/` and `amd64/src/` first, depend or
  extract, report *pulled / extracted / written new* and `wc -l taoxichuan/src/*.rs` before and after), and the protocol in the experiment doc
  lists it as rule 5.
- Stage 7C was converted into a **planning-only** session (`taoxichuan-07c-trim-plan.md`): inventory every arm against the existing crates, find
  the smallest H1-preserving rewire, plan H2/H3 inside it; the execution brief (`taoxichuan-07d-trim-and-gates.md`) is written from that plan.
- Process rule for the supervisor: **diff every brief against the template and the pinned doc sections before sending; verify each gate address
  and command against a doc or a passing run** (the stage 7A `10.0.2.2` gate was the same class of lapse). Saved to the supervisor's memory.
- Honest scope for the claim: stages 6-7 as run do not demonstrate "composed from existing crates"; they demonstrate a working but
  a kernel the model wrote new instead of assembling it from crates. The trim sessions are what recover the claim, and the line counts per session will show whether they do.

### Methodology pivot (2026-10-26): assemble from the leaf crates amd64 already uses; survey before every milestone

*Terminology: every line in this experiment is written by the model (goose-glm); "newly written" means code the model wrote new, as opposed to code reused from an existing crate by dependency, extraction or a feature cut.*

**Where the pivot comes from.** The user's reading, after stage 7 session D: the minimal kernel should be built the way the amd64 kernel was built, by taking the
existing crates and cutting them (seams or features) down to the requirement, not by growing `taoxichuan/src` with newly written code. That is the project's own method
(`akuma-exec-core`, `akuma-sntp` cut out of `akuma-syscalls-time` "so this target could reach it without also needing that crate's `akuma-exec` dependency",
`akuma-pipe`, `akuma-fdtable`, `akuma-syscalls-{poll,mem,net,sync}`), and it is why the amd64 kernel exists at all, so feasibility is not in question.

**What the evidence shows we were doing instead.** `taoxichuan/Cargo.toml` has 17 `akuma-*` dependencies; `amd64/Cargo.toml` has about 70. The crates amd64 uses and
Taoxichuan does not include exactly what the rest of stage 6 needs: `akuma-sntp` and `akuma-cmos-rtc` (clock), `akuma-syscalls-poll` (the `select`/`ppoll` apk waits on),
`akuma-syscalls-mem`, `akuma-syscalls-net`, `akuma-syscalls-{linux,abi}` (wire structs), `akuma-pipe`, `akuma-exec-core`, `akuma-port`, `akuma-tty`. The stage-7 trim
plan (session C) checked 11 crates and classed anything welded to `Process`/`akuma-exec` as "keep (no callable twin)"; it never considered cutting the weld, because the
brief offered only *depend / extract into a new crate / write new*. `akuma-syscalls-glue` itself has 28 path dependencies and every one of its features is a syscall-family
toggle; none makes `akuma-exec` optional, so that particular cut does not exist yet and someone has to make it (cost unmeasured). Session D then added ~160 lines of
gate-forced surface (`readv`, `F_DUPFD`, exec-keeps-fds, traces), each correct, each the kind of thing the crates already carry.

**What changes from here.**
1. **Survey first.** Every plan starts with the `diff` of amd64's `akuma-*` dependencies against Taoxichuan's, and names, per milestone, which unused crate covers it
   (BRIEF_TEMPLATE § Composition rule 3b; also sent to the running stage 7E session as a steer, since its brief predated this).
2. **Cut, don't copy.** Where the useful logic is welded to the process model, the plan says whether a pure leaf crate (as `akuma-sntp`) or a cargo feature frees it and what that
   costs; "keep (welded)" needs that justification, not a shrug.
3. **Count it.** Per session: crates pulled / extracted / cut / written new, `taoxichuan/src` lines before and after, and the number of `akuma-*` dependencies (17 at the start
   of this pivot; 4,608 lines after stage 7D). If the pivot works, dependencies go up and `taoxichuan/src` goes down.
4. **Honest limit.** The write-it-new route got stages 1-6 (offline) and H1 working in ~14 goose-hours; the pivot trades some of that speed for the claim the experiment
   exists to test. The next sessions (7E plan, then execution) are the first measurement of what that trade costs.

### Stage 7 sessions D-G (2026-10-26): execution, tooling, verdict, plan - all of stage 7's gates green; the cost drop

**Result.** After session D (trim steps 0-1 plus three gate-forced fd-family fixes: `readv`, `F_DUPFD`, exec keeps open fds; `taoxichuan-s4` @ `0ecb1611`), sessions E-G wrote the
gate tooling, got the definitive verdict and closed the last failing gate. `scripts/kvm/goose/run_gates.sh` on `2f2520a4` printed **`PASS` for G1 G2 G3 G4 H1 H2 H3a H3b H3c in one boot,
`restarts=0`**. The last fix was G4 (`lstat` resolved the *parent* through the symlink chain, `2f2520a4`), which made `taoxichuan/src` **smaller** (4,608 -> 4,581 lines; the collapse of a
duplicated `LSTAT` arm). `taoxichuan/src` over the whole stage: 4,450 (start of 7D) -> 4,608 (after the gate-forced arms) -> 4,581. `akuma-*` dependencies: 18 (17 plus `akuma-net-nic`).
Session G's plan for the rest of stage 6 (`docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md`) opens with the leaf-crate survey the pivot asked for: 60 amd64-only crates checked, the relevant ones
named per milestone.

**Cost (goose `sessions.db`, input tokens).** D: 218 tool calls, 25.2M, about 116k per call. E (tooling + the first battery): 53 calls, 1.4M, about 26k per call. F (two stalled starts and the
G4 fix): about 75 calls, 2.4M. G (leak answer, ops, plan): about 60 calls, 1.7M. **E+F+G together about 6.1M against D's 25.2M**, with more closed. What changed: (1) one `boot_dev.sh` and one
`run_gates.sh` instead of dozens of hand-driven boot/ssh calls (the gate/ops categories were about 45% of D's calls); (2) the brief's read cap (`| cut -c1-180 | head -60`; reading was 73% of D's
carried context cost); (3) fresh contexts per job; (4) a mock-verified script before burning a boot.

**What the tooling found (ops, all recorded for the next sessions).**
- herd re-parses a conf only on `disable` then `enable`, and a config reload can create a **second guest** about 20 s after `enable` (session E); `boot_dev.sh` re-probes for the survivor.
- The `vm-dev.conf.stage7-hostrelay` snapshot had been clobbered byte-identical to stage4 (no `vm_outbound`): the conf template is now tracked in git.
- The ssh client capture loses output; evidence is console-first (`[SSH] Exit status`), and the host forward degrades near 30 connections per boot. Session G's read-only answer to "which side leaks":
  the guest's close chain is complete in code (sshd drops the session, `close(fd)`, `sock::close_arm`, `remove_socket`) and one boot showed 15 accepts and 15 exits with the relay's two forward slots
  reused 15 times and no refusals; so the ceiling is upstream of guest accept, host-side as RESULT-D said. **Not settled by a counter**: the status line has no forward-slot counter; the
  settling experiment is `scripts/kvm/relay_stress.py` past 40 connections with a per-slot dump.

**What the supervisor got wrong, and the goose-side hangs (an hour lost).** Session F stalled four times with no output. Causes found: (a) `herd stop goose-glm` kills only the wrapper, so
`goose-cli-main` servers from earlier restarts survived and shared the session database ("Address in use" x241 in the log): stop must kill every `goose serve` pid; (b) the supervisor's own
`python3 -` / heredoc invocations became interactive REPLs on this box's shell and hung for hours (three of them), and a copy of the WAL file by `cat` over a dead ssh left an unkillable `cat`;
(c) sending the whole 8 KB brief as the *prompt* stalled the first model call of two sessions while the same text delivered as a file the agent reads in 60-line windows
(`/root/dev-cycle/taoxichuan-s7/BRIEF-*.md`, a 3-line prompt) started at once. The practice from here: **brief as a file on the box, short prompt, `python3 file.py < /dev/null`, one
clean goose server.**
- **Host track (Sonnet), merged tree `litter/taoxichuan-s4` @ `fa8a1c53` (merge `53e83e80`):** `scripts/build_taoxichuan_fc.sh` -> `taoxichuan-fc.bin` 662,792 B (unchanged: the
  stage 7 changes are all in the amd64-only `taoxichuan/` and `akuma-ring3-amd64`), Lima `fc` boot with disk: 162 lines, `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`. PASS.

### Supervisor handoff (2026-10-26, end of the first supervisor context)

**State.** Table stages 1-5 done; stage 6 (`apk add curl`) about half done (offline `apk add` + outbound TCP + all of stage 7's sub-gates green; `taoxichuan-s4` @ `fa8a1c53`
merged into `rust-org-il-experiment`, pushed to `origin` at `defbd2c9`). Next: the rest-of-stage-6 plan (`TAOXICHUAN_STAGE6_REST_PLAN.md`): M0 mirror + reference (host only), M1 clock anchor
(`epoch=` cmdline), M2 DNS, then trim step 2, M3 `apk update`, M4 `apk add curl`, trim step 3, M5 `PT_INTERP` + `curl --version`. Stages 7-12 of the table are not started and, on the pace here
(~14 goose-hours for stages 1-5 plus offline 6, plus ~9 for stage 7's network work), 9-12 will not be done by the talk on 2026-11-03; the user's decision (2026-10-26) is to keep climbing regardless of the talk. User's quota readings at the end: weekly reset
done at the start of the 26th, 5 h window 77% (sessions E-G cost ~6M tokens total). The supervisor loop, scripts and traps are in `scripts/kvm/goose/SUPERVISOR_RUNBOOK.md`.


### Stage 8 session H (2026-10-26): M0 LAN mirror + Linux reference, M1 wall clock - both PASS

**Result.** Brief `scripts/kvm/goose/taoxichuan-08-mirror-clock.md`, goose session 70 min (01:06-02:16 UTC), `taoxichuan-s4` @ `cbb6f4be` (4 commits, pushed to `litter`). Final battery on one clean
boot: `PASS G1 G2 G3 G4 H1 H2 H3a H3b H3c C1`, `restarts=0`, no `Fault|PANIC|sig!`. Details: `RESULT-H.md` on the box.
- **M0** (host only): 14-package curl closure frozen into `/srv/alpine-mirror` (15 files with the APKINDEX, `MANIFEST OK 15 files`), `mirror.sh` and `refroot.sh` written new; the reference root is built
  with the guest image's own `apk.static` (3.0.8 static-PIE). U1: `/etc/apk/keys` is present (three alpine-devel keys) so the signed index works without `--allow-untrusted`. U2: no
  `.post-install`/trigger scripts anywhere in the closure, so plan milestone M6 does not exist. U3: `curl` is a dynamically linked PIE, `PT_INTERP=/lib/ld-musl-x86_64.so.1`, so `curl --version` needs the loader work (M5).
- **M1**: the offset store was **pulled** (`akuma_primitives::clock`, already a dependency); ~44 newly written lines in `taoxichuan/src/main.rs` (the `epoch=` anchor and the `CLOCK_REALTIME`/`gettimeofday` arms;
  other clock ids unchanged). `taoxichuan/src` 4,581 -> 4,625; `akuma-*` dependencies 18 -> 18. `boot_dev.sh` injects `epoch=$(date +%s)` and re-syncs `$OUT/epoch` from `/var/run/herd/vm-dev.vm`.
- **Metrics** (`goose_session_stats.py`): 194 tool calls (62 before the first edit), 11.1M input / 78k output tokens, 11 doc reads over 4 files (the plan five times), 36 source reads, 6 `vm-dev` boots (the script's
  boot counter reads 0 for this session: it undercounts). 6 boots is two over the brief's cap of four, but each showed new evidence.

**What the supervisor got wrong.** The brief's C1 command (`d=$(date +%s); [ ... ] && [ ... ]`) needs two forks, and the guest kernel has no `fork` for `sh` (`can't fork: Function not implemented`, exit 2).
It was written from memory, not copied from a passing run: the same class of lapse as the stage 7A gate. Goose rewrote it as a single `awk systime()` command, which runs without forking.

**What goose found.** (1) Herd's reload can start the answering guest from a spec parsed before the conf write landed, so the epoch the script recorded was not the one the guest carried; fixed by re-syncing from the survivor's spec.
(2) A failed `kbuild` still led to a boot of the previous binary (boot 2); `boot_dev.sh` checks only that a kernel file exists. (3) Boot 1 printed no `[clock]` line and was not reproduced.

**Host track (Sonnet), merged tree @ `cbb6f4be`:** `taoxichuan-fc.bin` 662,792 B (unchanged: the changes are amd64-only), Lima `fc` boot: 162 lines, `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`. PASS.
A first boot showed 111 lines and stopped: a stale root-owned `firecracker` from the previous run was still alive (`pkill` as the Lima user fails, use `sudo pkill -9 firecracker`); the rerun was clean.


### Stage 8 sessions I-J (2026-10-26): M2 DNS from the guest - D1 and D2 PASS

**Result.** Brief `scripts/kvm/goose/taoxichuan-08i-dns.md` (session I, 297 tool calls, 28.8M input tokens, ended by the z.ai 5 h limit with no RESULT file) and its continuation
`taoxichuan-08j-dns-continued.md` (session J, 77 calls, 2.4M input tokens, 28 min). `taoxichuan-s4` @ `49d418de`. Final battery on one clean boot:
`PASS G1 G2 G3 G4 H1 H2 H3a H3b H3c C1 D1 D2`, `restarts=0`, `dns_answered 0 -> 2`, `dns_timeouts=0`, 0 `Fault|PANIC|sig!`. Details: `RESULT-J.md` on the box.
- **Session I** (five commits): `devroot-ensure` puts `/etc/resolv.conf` and `/bin/nslookup` on the per-boot image copy through the `akuma-ext2` host write path (composition rule: pulled, not a hand
  patcher; the `.orig` image stays untouched); connected-UDP send; `poll(2)` and `setsockopt`; `read(2)` on a datagram socket; and a `boot_dev.sh` fix (two boots had run a stale kernel).
- **Session J found no bug.** A fresh boot of session I's own last kernel already printed the answers (A and AAAA, rcode 0). The "No answer" was the state before `f7e9aac5`, whose `read(2)` fix
  was never re-tested because the quota ended the session. J removed the three temporary probe prints and a dead match arm (`taoxichuan/src` 4,811 -> 4,779), then ran the battery.
- **Metrics:** `akuma-*` dependencies of `taoxichuan/Cargo.toml`: 16 direct path deps by `grep -c '^akuma-'` (the "18" carried in earlier briefs does not match the manifest under that count; correct the
  briefs). Nothing written new in J, nothing extracted, nothing under `amd64/` or `src/` changed.
- **Ops trap:** the vm-dev log appends across boots; attribute evidence per boot boundary, not per file.

**Host track (Sonnet), merged tree @ `e98aef60`:** the merge conflicted once in `crates/akuma-net/src/socket.rs` (a doc comment on `DebugStream`, kept the already-split form). `taoxichuan-fc.bin` 662,792 B,
Lima `fc` boot: 163 lines, `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`. PASS. (A read at 75 s saw 111 lines: the run lasts 120 s; wait for the firecracker process to exit before counting.)

**Next:** trim step 2, then M3 `apk update`, M4 `apk add curl`, trim step 3, M5 `PT_INTERP` + `curl --version` (stage 6 of the table), then stages 7-12.


### Stage 8 session K (2026-10-26): trim step 2 - wire structs from the ABI crates, byte-identical - PASS

**Result.** Brief `scripts/kvm/goose/taoxichuan-08k-wire-dedup.md`, goose session 110 min (10:47-12:37 UTC), 120 tool calls, 7.4M input tokens, `taoxichuan-s4` @ `694cddd5` (8 commits).
Five families, one commit each (stat+statfs, getdents64, uname, time structs, `nr_name`). Final battery on one clean boot: `PASS G1 G2 G3 G4 H1 H2 H3a H3b H3c C1 D1 D2 W1a W1b W1c W1d`, `restarts=0`,
0 `Fault|PANIC|sig!`. `taoxichuan/src` 4,779 -> 4,744 lines; `akuma-*` dependencies 16 -> 18 (`akuma-syscalls-linux`, `akuma-syscalls-abi`). Details: `RESULT-K.md` on the box.
- **The plan's gate could not run.** The trim plan's step-2 gate is `abi_write_probe`, which `build.sh` builds with `aarch64-linux-musl-gcc`; the guest is x86_64. The supervisor replaced it with W0/W1: guest-side
  sha256 of `ls -lin /etc/hostname`, `df /`, `ls -l /etc`, `uname -a`, recorded on the pre-refactor kernel over three fresh boots (identical hashes every time, so determinism was proved before it could accuse a
  refactor) and committed in `scripts/kvm/goose/w1_expected_hashes.txt`. The busybox in the serve image has no `stat` applet (exit 127), hence the nearest applets. The file was not edited after W0 (supervisor-checked).
- **Family 2 W1c FAIL was the gate, not the code.** The forward degraded, the script restarted the guest on a fresh image that never ran G3's `apk add`, so `/etc` had no `ssl` and the hash differed. W1 now
  re-applies the two `/etc` mutations itself; family 2 then passed with `restarts=0` against the unchanged baseline.
- **Cuts:** `nr_name` 95 -> 21 lines (the ABI crate's `Syscall::from_x86_64`; names are now CamelCase, console-only), getdents -13, uname -7, time -3; the stat family went up by about 22 lines of documentation
  but lost the literal offset table. One local 5-line `struct_bytes()` helper was extracted once four wire structs went through it.
- **Supervisor-visible lapse (goose):** one commit (`1e665116`) was pushed with the build red (E0308) because a chained command's grep matched "error" yet exited 0; fixed forward in `cd93f821` before any boot.
- **Left un-pulled, for step 3's extraction pass:** nanosleep's timespec read, `sock.rs`'s timeout timevals, the iovec pairs. Candidates found by diffing `amd64/Cargo.toml` against `taoxichuan/Cargo.toml`
  (not acted on): `akuma-vfs-glue`, `akuma-user-access`, `akuma-pipe`/`akuma-pipes`, `akuma-elf`, `akuma-syscalls-time`, `akuma-syscalls-poll`.
- Host tests need `--target $HOST` here (the workspace `.cargo/config.toml` pins aarch64-unknown-none).

**Host track (Sonnet), merged tree @ `d0de0601`:** one merge, no conflicts. `taoxichuan-fc.bin` 662,792 B (unchanged), Lima `fc` boot: 162 lines, `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`. PASS.

**Next:** M3 `apk update` (repositories file + keys in the image, `vm-dev.conf.stage7-hostrelay`), then M4 `apk add curl`, trim step 3, M5 `PT_INTERP` + `curl --version`.


### Stage 8 session L (2026-10-26): M3 `apk update` from the guest - A1 A2 A3 PASS

**Result.** Brief `scripts/kvm/goose/taoxichuan-08l-apk-update.md`, goose session 75 min (12:44-14:00 UTC) plus a 15 min close-out session (the first one hung on a model call after the gates passed and was restarted
with a short prompt, per the runbook): 110 + 22 tool calls, 5.4M + 0.9M input tokens. `taoxichuan-s4` @ `7e3b422f`. Final battery on one clean boot:
`PASS G1 G2 G3 G4 H1 H2 H3a H3b H3c C1 D1 D2 A1 A2 A3 W1a W1b W1c W1d`, `restarts=0`, 0 `Fault|PANIC|sig!`. Details: `RESULT-L.md` on the box.
- **M3:** one unflagged `apk update` in the guest against `http://192.168.1.159:8123/main`, signed with the image's own three alpine-devel keys (no `--allow-untrusted`), rc 0; the mirror saw exactly one
  `GET /main/x86_64/APKINDEX.tar.gz`, and the cached index hashes to `c169fa06...`, byte-equal to the mirror's. `/etc/apk/repositories` is written on the per-boot COPY by `devroot-ensure` (the `.orig` holds only an
  "offline staging" comment line).
- **The one kernel change** (`12aebebf`): a `select(2)` arm (x86_64 nr 23), because apk's fetcher blocks in `select`, which busybox wget (H1's client) never used. Pulled: fd-set arithmetic from
  `akuma-syscalls-poll::fdset` (19th path dependency) and x86_64 `getpid` (39) from `akuma-syscalls-abi`; extracted in-kernel: readiness probing out of `poll_arm` into a shared `fd_events`.
  `taoxichuan/src` 4,744 -> 4,856 lines (it grew: the milestone bought the fetcher's syscall surface).
- **OPEN, found here: a refused outbound dial is never signalled to the guest.** With the repositories file live and no listener on :8123, the relay refuses the SYN (`gw_syns=9/10 gw_refused=9/10 resets=0`) but sends
  the guest no RST, so the guest's connect/select never returns: vCPU pinned `c4=100%`, console silent, and the host ssh client burns 10 retries (~13 min) per exec. Goose worked around it in the gate scripts
  only (mirror started first; H1's listener moved to :8124; genuinely-offline installs use `--no-network`). The relay itself (`amd64/src/kvm/relay.rs` / the gateway in `akuma-svm-amd64`) still needs to
  answer a refused dial with an RST. Any apk or curl against a dead mirror will hang the guest until then.
- **Other findings:** herd rotates `vm-dev.log` mid-boot (the old log holds the guest-disk line; `boot_dev.sh` now checks both sides, `4fc359de`); the host forward died early in about a third of the batteries over
  sessions K and L (script restart-once recovers it, but a restarted run is `restarts=1` and needs a clean rerun for the gate); `[sc] unhandled Ftruncate (77)` and nr 40 (sendfile) were tolerated and are the likely
  M4 arms.
- **Supervisor actions:** killed a wedged `run_gates.sh` and its ssh client (not the VM) after the second G3 hang, then sent goose a facts-only steer; restarted goose and resent a short close-out prompt after
  the final hang.

**Host track (Sonnet), merged tree @ `6923d620`:** one clean merge. `taoxichuan-fc.bin` 662,792 B (unchanged), Lima `fc` boot: 162 lines, `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`. PASS.

**Next:** M4 `apk add curl` (index proven; add exercises unpack and likely `ftruncate`/sendfile), then trim step 3 (net.rs hygiene, H1 as canary), then M5 `PT_INTERP` + `curl --version`. The relay RST fix is a
candidate for its own small session before M4 or inside it.


### Stage 8 session M (2026-10-26): M4 `apk add curl` in the guest - B1 B2 B3 B4 PASS

**Result.** Brief `scripts/kvm/goose/taoxichuan-08m-apk-add-curl.md`. The first context died with the box (it reset at about 15:09 UTC, see below) after 46 tool calls / 1.0M input tokens; the resumed context ran 105 calls / 5.8M input tokens
(15:30-17:17 UTC). `taoxichuan-s4` @ `9d39529a`. Final binary (`49521550`) passed the main battery (19 gates) in one boot and `B1 B2 B3 B4` in their own boot, both `restarts=0`, 0 `Fault|PANIC|sig!`. Details: `RESULT-M.md` on the box.
- **M4:** one unflagged `apk add curl` against the frozen mirror, rc 0, `OK: 11.8 MiB in 14 packages`, 16 mirror GETs; `/etc/apk/world` gains `curl`; the six reference sha256 prefixes match, including `/usr/bin/curl 9eb8284f`, `libcurl 1ee39403`
  and the image's own `/lib/ld-musl-x86_64.so.1 095b047c` (apk did not replace the loader with a different build). Nothing unpacked was executed (curl is a dynamic PIE; loader work is M5). No `.post-install`/trigger ran (U2 held).
- **Deviation (B4):** the brief's `apk verify curl` takes `.apk` file paths and the cache does not keep fetched packages, so B4 became `apk info -e curl` (exit 0); B3's hashes carry the integrity check.
- **Kernel changes** (`ef02ac68`): `fill_statfs` reads the real superblock through `Filesystem::stats` (`akuma-ext2`/`akuma-vfs`, pulled); a new `FTRUNCATE` (77) arm over `akuma-ext2`'s `truncate_by_inode` (pulled; about 20 lines of fd-table marshalling
  written new); a per-call `[statfs]` console line (written new, observability). `taoxichuan/src` 4,891 -> 4,916 lines (the 4,856 in the earlier entry predates this session's start); `akuma-*` dependencies stay 19.
- **FINDING - `W1b` never covered filesystem sizes.** The guest has no procfs and this busybox `df` enumerates mounts through `/proc/mounts`, so `df /` prints only its header line and exits 1 (a valid `/etc/mtab` changes nothing). `W1b`'s hash is the
  sha256 of that one header line, so the brief's "W1b's hash changes on purpose" premise was wrong. The statfs substance is on the console instead: `[statfs] blocks=262144 bsize=4096 free=258285` on a fresh boot (exactly 1 GiB) and `free=255096` after the
  unpack (about 12.4 MB consumed). The old fixed answer (2,053 free blocks, about 8 MiB) could not have held the 12.6 MB closure. `W1b` stays as a header-bytes canary until a procfs or a `df` substitute exists.
- **FINDING (open, pre-M5) - `apk info -L curl` kills the guest.** apk NULL-writes in ring 3 (`[Fault] ring3 PageFault ... cr2=0x0 code=0x6 -- task killed`, after a 5.4 MB kernel-heap allocation), and a ring-3 fault parks the whole guest by design (no reaper at this stage). It may be the
  same mmap path M5 must fix. Not fixed here.
- **Open housekeeping:** the per-call `[statfs]` print is debug output and must be removed (or cmdline-gated) before the kernel is final; the automated Statfs verdict is unreliable because herd replaces `vm-dev.log.old` mid-boot (evidence-collection flakiness, not a statfs fault);
  the relay still sends no RST for a refused dial (carried from session L).
- **The box reset (about 15:09 UTC):** Akuma had been up about 5 hours and was running goose's kernel build; its klog ends with repeated `[bkls>] core=2 owner=6 spins=1048576 / 2097152 / 4194304` warnings (the ticket numbers advanced, so long BKL contention, not a deadlock),
  then stops with no panic text; the machine came up on Pop (the GRUB default) at 17:09:02 local. Not diagnosed beyond that; it matches the BKL-stall class already in the notes. The goose session and its uncommitted work survived on disk; the supervisor rebooted into Akuma and resumed with a short prompt.
- **Supervisor actions:** cancelled one session start when the other agent turned out to be using the same box (its `boot_dev.sh` fix, `9202d190`/`e3f4e2f3`, explained the 856 MiB reading: herd started the stopped service during the 1 GiB image rewrite); sent one steer to stop gate polishing once B1-B4 had passed.

**Host track (Sonnet), merged tree @ `1df3ac77`:** one clean merge. `taoxichuan-fc.bin` 662,792 B (unchanged), Lima `fc` boot: 162 lines, `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`. PASS.

**Next:** a planning-only session for the process model (fork/clone/execve/wait4/pipes, `scripts/kvm/goose/taoxichuan-08n-process-model-plan.md`), then trim step 3, then M5 (`PT_INTERP` + `curl --version`), with the `apk info -L` crash looked at first.


### Stage 8 session N (2026-10-26): the process-model PLAN for stages 8-12 (planning only)

**Result.** Brief `scripts/kvm/goose/taoxichuan-08n-process-model-plan.md`, goose session 15 min (17:23-17:38 UTC), 58 tool calls, 1.5M input tokens, zero boots, zero source changes. Output: `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md`
(157 lines, seven sections, anchors and `cargo tree` counts checked in-tree; the supervisor spot-checked `sys_fork` at `amd64/src/usermode.rs:5520`, `fork_process` at `crates/akuma-exec/src/process/mod.rs:3194` and the 642-line `amd64/src/exec_runtime.rs`). `taoxichuan-s4` @ `d6d0d772`.
- **Load-bearing discovery:** the amd64 fork path is not welded shut. `sys_fork` is already a thin fold over `akuma_exec::process::fork_process`; amd64's whole consumption cost is one registration file (`exec_runtime.rs`) plus a scheduler hookup (`X86ArchHooks` over `akuma-threading`). `akuma-mmu` is dual-arch.
- **Recommended route:** pull `akuma-exec` whole (all pieces resolve for `x86_64-unknown-none`; node counts: exec 84, threading 40, mmu 29, pipe(s) 1-2, cow 1, exec-core 6). Fork/vfork/wait4/threads and pipes pulled; execve cut at the `install_image` seam keeping taoxichuan's eager loader; CoW deferred
  (eager-copy fork through the `fork_share_memory` hook, since taoxichuan has no page-fault handler and `fault_trap` kills). One honest write-new exception: a SPAWN-shaped blocking fork (about 150-250 lines) as a temporary scaffold that lifts the one-command gate before the scheduler lands.
- **Milestones:** M1 (about 10 lines) makes READ/WRITE/LSEEK/FSTAT consult the fd table first; M2 is the first gate that lifts the biggest limit: `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` prints `x` then `2`; M4 is the real fork (akuma-exec adoption); PT_INTERP sits at M5; M7 is the git-clone gate.
- **Biggest unknown (U1):** whether `akuma-threading` schedules correctly when driven by taoxichuan's own trap and timer plumbing; its x86 arms have only run against amd64's IDT and tick. Settle on paper before M4 boots. Also U6 (wait4 over child channels with SIGCHLD never delivered) and U5 (vfork parent suspension for git's helper protocol).
- **Bottom line:** stages 8-12 are reachable by composition, not reimplementation; the bulk is stage 8.

**Host track:** doc-only commit, no code change, so no Lima boot was run for it.
**Next:** the `apk info -L curl` NULL-write crash and trim step 3, then M5 (`PT_INTERP` + `curl --version`); the plan's M1/M2 (fd-table-first routing, then the `;` gate) can run in parallel since they touch different code.


### Process review (2026-10-26): what the sessions since the methodology change tell us

**Data.** `scripts/goose_session_stats.py` over the goose session database (31 sessions, stage 1 to session O; session H's 194 calls / 11.1M tokens are missing from the export) and the z.ai usage API (`scripts/kvm/goose/zai_usage.py`, new).
Stage 4 through 7D (before the pivot) against the 15 sessions after it:

| | before | after |
|---|---|---|
| sessions / tool calls | 10 / 1,474 | 15 / 1,224 |
| input tokens | 141M | 68M (about 80M with H) |
| tokens per call | about 96k | about 56k |
| tokens per hour of active work | 15M | 6.6M |
| a typical milestone | 20-28M | 5-11M |

- **Quota calibration** (percent against tokens inside the window, plan "lite"): weekly cap about 205M tokens (window = reset minus 7 days; the reset is 2026-11-01 19:35 UTC, so the week started 2026-10-25 19:35 UTC, not "the start of the 26th" as the supervisor handoff above says), 5-hour cap about 34M. z.ai's counts match the goose database
  (75M on 2026-10-26 by both), so the stats script is a usable proxy. At the old 15M/hour a 5-hour window was gone in about 2.3 hours (session I ended on that limit); at the recent 3-6M/hour it is not reached. That is the "5-hour windows are getting longer".
- **Cost per call rises with session length**, because every call re-sends the context: about 19k tokens per call under 40 calls, 30k at 40-79, 59k at 80-129, about 105k at 130 or more. Six sessions of 130+ calls account for 71% of all tokens (149M of 209M); only one of them is after the pivot (session I, 28.8M, which chased a bug its own last commit had already fixed).
- **The cheap milestones share a recipe:** a prebuilt gate script, a reference value measured before the session, one milestone, and a stop rule (`apk update` 6.3M, `apk add curl` 6.8M, wire dedup 7.4M, the process-model plan 1.5M).
- **Briefs carry a wrong premise about one time in two,** and goose's survey or evidence caught every one: a gate that needed `fork` (H), a probe that builds only for aarch64 and an applet missing from the image (K), a gate order that made G3 dial a mirror that was not up (L), a `df` hash that never covered sizes (M), a loader described as welded that was already an adapter and ldso syscalls described as served that were ENOSYS (O).
- **Hangs cost more than the work they interrupt:** the host forward died early in about a third of batteries and the ssh client's 10 retries cost about 13 minutes each time; a G3 hang cost about 40 minutes; one model call hung 8 minutes; the box reset once under a long kernel build (BKL spin warnings, then an abrupt reset to the Pop default).
- **What worked:** baselines recorded over several fresh boots before they could accuse a change (W0); gate groups that re-create their own state; verdicts re-run on the exact final binary; host tests in a new leaf crate catching bugs before any boot; evidence attributed per boot boundary.
- **Composition held:** `taoxichuan/src` 4,625 -> 4,916 lines while gaining DNS, `select`, `ftruncate`, real statfs and the stat/dirent/uname/time families; dependencies 16 -> 19; no new crate until session O's `akuma-elf-exec`.

**Process improvements** (the first two are the user's idea: examine assumptions, and put more weight on probe discovery):
1. **Premise audit** (done: `BRIEF_TEMPLATE.md` "Premises"). Every brief carries a table of its premises, each with a read-only check the supervisor ran before sending. Goose's first job, before any edit, is to re-run each check and write HOLDS / WRONG / UNCHECKED per row in RESULT. A WRONG row that changes the plan is reported at once, not at the end.
2. **Probe discovery** (done: `BRIEF_TEMPLATE.md` "Probe discovery"). Before writing any probe, gate or script, goose lists what already exists (`userspace/*probe*`, `scripts/*_suite.py`, host tests, `run_gates.sh` groups, and a binary that already runs on the full amd64 kernel on the box, which gives the reference behaviour in one command) and runs the cheapest first. A new probe needs a stated reason and goes into the repo.
3. **Guest facts block** (done, in the template): no fork, no procfs, no `stat` applet, one core, the forward ceiling, the 1 GiB image. Briefs stop contradicting what is known.
4. **Budget and checkpoints** (done, in the template): a call budget per session (the supervisor nudges at 90 calls without a commit), a checkpoint at the first green host test, `[tmp]` tags on temporary diagnostics with a "Remove before final" list (the per-call `[statfs]` print was left in the kernel for lack of one).
5. **Quota in the metrics** (done: `zai_usage.py`): record the percent and token delta at the start and end of each session.
6. **Not done yet:** a hard per-exec timeout in `run_gates.sh` (about 90 s) in place of the ssh client's retry loop, which would have saved roughly an hour; `run_gates.sh` groups of 20 execs or fewer so the forward ceiling never matters; one agent owning the box at a time (a lock file under `/root/dev-cycle`), because the other agent and goose shared it today; a differential check that `amd64` still builds after an extraction (session O's baseline comparison was done by hand).

**Correction to the review above - the model split (user's question, 2026-10-26).** The first calibration summed all models. The usage API reports per model: **16.1M tokens were GLM-5.3** (the more expensive model), all of them in two hours on 2026-10-24 (08:00-10:00 UTC), which is the stage 1 session (16.1M in the goose export); everything else, including both windows used for the caps, is GLM-5.3-Flash.
The caps (weekly about 205M, 5-hour about 34M) are therefore Flash-only and unaffected, because the current windows hold no GLM-5.3 tokens. The "before the pivot" totals above include the 16.1M: Flash-only they are about 125M, or about 94k per call, so the fall in tokens per call (about 40%) and per milestone stands; in quota terms the pre-pivot period cost more than its token count suggests. `zai_usage.py` now prints the split per window.


### Stage 8 session O (2026-10-26): M5 dynamic PIE — extraction done, first dynamic exec still faults (continues in O2)

**Result: M5 NOT closed.** Brief `scripts/kvm/goose/taoxichuan-08o-pt-interp.md`; goose session 20261026_30, 17:43-19:18 UTC, **223 tool calls, 19.7M input tokens** (brief budget 200; the supervisor steered at call ~210 after 110 calls without a commit). `taoxichuan-s4` @ `b3807175` (3 commits: `5bd2caec` extraction + loader, `b8f893c4` `[tmp]` diagnostics, `b3807175` default-members). Full detail: `RESULT-O.md` on the box (`/root/dev-cycle/taoxichuan-s7/`).
- **Premise audit paid again:** the brief said the interp/auxv logic was welded to `UserAddressSpace` in `amd64/src/loader.rs`; it is a 710-line adapter over `akuma-elf` already. Syscalls "said to be served" for ldso were partly ENOSYS: `PREAD64` missing, `MAP_FIXED` anonymous-only (ENODEV file-backed), `FUTEX`/`SIGALTSTACK`/`SET_ROBUST_LIST`/`PRLIMIT64` unarmed.
- **Composition:** new leaf crate `akuma-elf-exec` (401 lines, zero deps, `forbid(unsafe)`, 8 host tests); `akuma-elf` (19/19 host tests) and `amd64/` (`finish()` cross decision) pulled onto it. taoxichuan adds the ldso mapping in `load_task`, file-backed `MMAP`, `PREAD64`, monotonic `mmap_cur`. `taoxichuan/src` 4916 -> 5134 lines (+70 are `[tmp]` prints); `akuma-*` deps 19 -> 20.
- **The fault (CORRECTED 2026-10-26 by session O2, see below: the "wrong bytes in the child's page tables" below was an artifact of the diagnostic itself, and the ldso mapping was not at fault; the real defect was the copy's source index):** `curl --version` dies deterministically on the first dynamic exec: control reaches the ldso entry (`0x3006c185`, rsp correct, `AT_BASE=0x30000000`) but the bytes read back through the child's page tables there are not the file's (`8b 24 48 74 ...` where the file has `48 31 ed 48 89 e7 ...`), then `PageFault rip=0xfffffffff9bcd08d`. The `vec@entry` print (Vec wrong vs mapping wrong) is in the tree but boot 4 stalled at the ssh layer before exec (new anomaly, two connections, auth OK, no `[SSH] Exec`).
- **Static path:** `apk add curl` (b1) passed on all four boots. Supervisor check on the merge (`e2f728ea`): `scripts/build_taoxichuan_fc.sh` + Lima boot: 162 lines, `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`.
- **Quota:** 5-hour window 68% (23.6M of about 34M) and week 59% (119.6M) at the end of O, all Flash.
- **Process notes:** a 200-call budget was overrun by 23 calls and a steer was needed to get the WIP committed; the monitor tool (`Monitor`, 60 s poll for call count, litter head, silence, driver exit) replaced the manual 5-minute polls. Open: `apk info -L curl` crash untouched, `e_group.sh` does not start the mirror, `[tmp]` prints to remove.


### Host finding (2026-10-26): the KVM implementation has carried the whole experiment without a guest-caused host crash

**Observation (user's conclusion, recorded with the evidence behind it).** Since stage 1 on 2026-10-24 the experiment has been, without being designed as one, a soak test of the amd64 kernel's KVM (`/dev/kvm0`, `herd vm-dev`): 31 goose sessions up to session O (about 2,700 tool calls, `Process review` above) plus O2, each booting the Taoxichuan guest repeatedly on the ryzen box over roughly two and a half days. Guests died on purpose and by accident throughout (`Fault ring3 PageFault ... task killed`, halted guests, guests parked mid-authentication, `herd stop vm-dev` on a live guest), and no guest took the host kernel down.
- **User's observation (2026-10-26):** in the whole experiment the user has seen one connectivity issue and nothing remotely like a freeze of the guest or the host. The log agrees on the guest side: the hangs it records are in the transport (the host forward dying early in about a third of batteries, ssh-client retry loops, the relay never signalling a refused dial, boot 4's ssh-layer stall in session O), each with a live host and a console that kept answering; none is a frozen guest or host.
- **The one host reset in the period was not a guest event.** On 2026-10-26 at about 15:09 UTC Akuma reset to Pop while goose ran a long kernel build; klog ends in `[bkls>]` BKL-contention warnings with no panic text (entry for stage 8 session M above). That is the BKL-stall class in the notes, a host-side build load, and the klog does not implicate KVM. I did **not** establish whether a guest was running at that moment, so "not KVM" is the klog's reading, not a proof.
- **OPEN QUESTION (user, 2026-10-26): why did the box reboot at about 15:09 UTC?** To drill down later. What is known: Akuma had been up about 5 hours; goose was running a long kernel build; klog ends in repeated `[bkls>] core=2 owner=6 spins=1048576 / 2097152 / 4194304` warnings (ticket numbers advanced, so contention, not a deadlock) and then stops with no panic text; the machine came up on Pop (the GRUB default) at 17:09:02 local. Not known: whether a guest was running (`vm-dev`/`vm-serve-mc` state at that moment), whether the cause is the BKL-stall class in the notes or a hardware/firmware reset, and whether the klog was flushed (an abrupt reset leaves no panic text either way). Where to start: the preserved klog on the `nvme0n1p3` partition (mount read-only from Pop), Pop's boot records for the reset time, and `docs/reference/subsystems/smp-shared.md` for the BKL stall class.
- **Host evidence checked 20:30 UTC:** uptime 5 h 02 min since that reset (covering sessions N, O and the first half of O2), `klog.log` 160 lines with 0 matches for `PANIC|BKL|spin|Fault`, all herd services `restarts=0` apart from deliberate `goose-glm` restarts. Session O alone booked 4 guest boots, two of them ending in guest faults. The exact guest-boot count over the whole experiment was not tallied (`vm-dev.log` rotates mid-boot), so "repeatedly" is a count of sessions, not of boots.
- **What this does and does not show:** KVM held under a long run of guest faults, hangs and restarts, and the host kept serving ssh, goose and the relay. It is not a measurement of concurrent guests (kvm0 is exclusive, one guest at a time), of multi-day uptime (the reset above bounds it at 5 hours since), or of a hostile guest.


### Stage 8 session O2 (2026-10-26): M5 CLOSED — the first dynamically linked program runs on Taoxichuan (`curl --version`)

**Result.** Continuation of session O, goose session 20261026_32, 19:22-20:40 UTC, **128 tool calls**, 4 boots of 4, about 10.5M tokens (weekly counter 119.6M -> 130.1M; 5-hour window rolled over mid-session). `taoxichuan-s4` @ `6bde98eb`, merged here as the commit after `ea93fc23`. Detail: `RESULT-O2.md` on the box.
- **The defect:** the ldso page loop indexed the *whole-image* Vec with a vaddr-relative offset (`ldso[(from - seg.vaddr)..]`), dropping `p_offset`; every segment with `p_vaddr > 0` copied bytes from a file offset shifted down by its own `p_vaddr` (musl's text: the entry page got `ldso[0x58000..]` where `ldso[0x6c000..]` belonged). Fix: `akuma_elf_exec::LoadSeg` gained `offset`, the loop indexes `seg.offset + (v - seg.vaddr)`. The program loop never had it because `segment_bytes(seg)` re-bases the slice.
- **Proof it was that and nothing else:** a read-back inside the copy loop gave `e9 03 0f b6 c9 41 01 ca` = `ldso[0x58185]`; `e9` is `jmp rel32` and `0x3006c18a + signext(0xc9b60f03) = 0xfffffffff9bcd08d`, the exact fault rip. The shifted bytes executing *are* the crash.
- **Session O's "alien bytes at the entry" was a diagnostic bug:** `translate_in` returns frame + page offset and the read-back added the offset a second time, a stride-2 sampling of real ldso bytes. The page tables were correct all along. A reminder to verify a probe on a known value before trusting what it shows (a second instance of the bisect-the-readiness-syscall lesson).
- **Supervisor-verified on the box console:** E1 `[SSH] Exec: /bin/sh -c "curl --version > /tmp/cv"` then `Exit status: 0`; E2 `grep -cx '<the 160-byte reference line>' /tmp/cv` exit 0 (the line is `curl 8.23.0 (x86_64-alpine-linux-musl) libcurl/8.23.0 OpenSSL/3.5.9 ...`); group b B1-B4 PASS, `restarts=0` (`gates-E.txt`). **Goose-reported, not re-run by the supervisor:** main battery 19/19. Statfs FAIL is the known log-window false negative (the `[statfs]` line rotated out of the slice), unrelated to M5.
- **Host track:** merged build via `scripts/build_taoxichuan_fc.sh`, Lima `fc` boot: 163 lines (162 baseline plus one Firecracker VMM message about vCPU resume), `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`.
- **Size:** `taoxichuan/src` 5,134 -> 5,096 lines (the `[tmp]` prints came out); `akuma-*` deps 20. Quota after: week 64% (130.1M), 5-hour window 1%.
- **Process:** O2 was cheaper than O by a wide margin (128 calls and about 10M against 223 calls and 19.7M) on the same defect, because a fresh context started from O's RESULT file and one discriminating boot. The 200-call overrun and 110 calls without a commit in O were the cost of keeping a long context; the L2/M2 continuation pattern worked again.
- **Open / discrepancy:** O2 reports the host `akuma-amd64` package does not compile at this HEAD (5 x E0425 in `amd64/src/mem.rs`) and calls it fallout of the `b3807175` workspace commit; session O reported the *same five errors at clean `d6d0d772`*, before any of this work. The two reports disagree on cause; not resolved here. `apk info -L curl` repro is the next boot. `FUTEX`/`SIGALTSTACK`/`SET_ROBUST_LIST`/`PRLIMIT64` are still ENOSYS, and e1 raised no `[sc] unhandled`, so a multithreaded dynamic binary is the next loader frontier.
- **Next:** `apk info -L curl` NULL-write crash, then stage 7 (curl HTTPS) now that a dynamic curl runs, the probe-inventory session P, and the process-model M1/M2.


### Quota finding (2026-10-26): a full 5-hour window, a closed milestone, and no usage error

**Observation (user's conclusion; the numbers are below, with which are measured and which inferred).** The 5-hour window that ran 15:30-20:30 UTC on 2026-10-26 (plan "lite", cap about 34M tokens, all GLM-5.3-Flash) held the process-model plan (session N, 1.5M), session O (19.7M input tokens, 223 calls) and about nine tenths of session O2 (128 calls), and no `Usage limit reached` error came back at any point. That window delivered M5: the first dynamically linked program (`curl --version`) running on Taoxichuan, plus a new leaf crate, a loader shared with the amd64 kernel and the correction of a wrong diagnosis. Earlier in the experiment this much work would not have fitted: session I ended on the 5-hour limit, and at the old rate of about 15M tokens per active hour a window was gone in roughly 2.3 hours (`Process review` above).
- **Measured:** `zai_usage.py` read 68% (23.6M tokens, 327 calls) at 19:18Z with the reset at 20:30Z; the next window read 0.6M at 20:42Z and 1.3M at 20:52Z; the weekly counter went 119.6M -> 130.1M across session O2 (about 10.5M), now 64% (130.8M of about 205M, reset 2026-11-01 19:35Z).
- **Inferred, not measured:** no reading was taken just before the 20:30Z reset. Subtracting O2's post-reset share (about 0.6M) from its 10.5M and adding the 23.6M gives roughly **33M of 34M, about 97%**: the window closed with little margin, not with room to spare. The conclusion (no error hit) stands either way; the margin claim is an estimate. From here every session records the percentage at its start and end, so the next entry is measured.
- **What changed between the two regimes** (not an experiment with a control; the data in the `Process review` supports the direction, not each cause separately): the premise table and probe discovery in every brief, one milestone per session with a stop rule, a call budget with a supervisor nudge, the continuation pattern (a fresh context that starts from the RESULT file, which finished the same defect in 128 calls and about 10M tokens after 223 calls and 19.7M in the long context), `[tmp]` tags so diagnostics get removed, and the `Monitor` poll replacing hand-timed checks.


### Idea parked (2026-10-26): libakuma on plain Linux syscalls, so `userspace/scratch` could be the stage 8 git client

Supervisor check, no goose spend: `scratch` (a 7,011-line git client over Smart HTTP) builds unchanged for `x86_64-unknown-none` (285 KB static) and uses only Linux-numbered calls on x86_64 except three Akuma-private ones (`RESOLVE_HOST`, `UPTIME`, `TIME`) that Taoxichuan would answer with ENOSYS. Not run in a guest. **User's decision: do not cut this corner now**; real `git` is the better preparation for later stages, goose is left to find its own route to the stage 8 gate, and the libakuma Linux-compat change is written down as an idea in `proposals/LIBAKUMA_LINUX_COMPAT.md` (findings, the change, the first test). Revisit if the git gate stalls.


### Process finding (2026-10-27): goose reads source and adds probes by default; the debugging docs already described the bug

**Observation (user's, from watching session Q: "it should really look more at the docs for debugging, there are clues all over the place").** The `curl` over HTTPS gate failed on `Could not resolve host ... (Could not contact DNS servers)`, and `docs/README.md` (symptom matrix, line 100) has a row for exactly that error: c-ares `connect()`s its UDP socket and then `send()`s, and calls `getsockname`/`getpeername`; fixed on amd64 on 2026-09-18, with the sequence and a 40-line replay probe in `docs/archive/AKUMA_AMD64_DNS_CONNECTED_UDP.md`. Taoxichuan hit the same chain again, one syscall per boot.
- **Measured (goose session database, `gss.py`):** session Q, 165 calls: **3 docs reads, all one `kvm.md` section**, 38 source reads, 61 calls before the first edit. Session Q2, 100 calls: **0 docs reads**, 29 source reads. Neither opened `docs/README.md` or the DNS archive doc. Q's brief had dropped the template's pointer to the front door; that omission was the supervisor's.
- **Indicative only (goose narration; Q3 and Q4 were not in the export yet):** Q3 mentioned the symptom row and an archive doc only after a supervisor steer pointing at them (`docs/README.md` line 100 and the DNS doc); Q4's continuation prompt said to grep the symptom matrix first, and its narration shows `README`, the symptom row and archive docs. After the steer, Q3/Q4 took curl from exit 6 (no DNS) to exit 7 (connect) to exit 0 on the HTTPS fetch in about 150 cumulative calls, each fix being a call named in that doc's sequence (`getsockname`, `getpeername`, the `sin_family` byte order, `getsockopt`/`SO_ERROR`).
- **Cost of the miss:** Q and Q2 together were about 265 calls and about 15M tokens (the week counter 131.4M -> 146.0M across Q, Q2 and the start of Q3), most of it kernel-print probes and boots to rediscover the sequence.
- **Change made:** `BRIEF_TEMPLATE.md` has a new "Symptom first" step (commit `3c9180f7`): when a gate fails, `grep -n -i "<the error text>" docs/README.md docs/runbooks/*.md` and read the matching row and archive doc before any source read or new probe, and write the matching rows (or "no row") in RESULT-X.md. It is one grep per failure, not extra load. **Existing continuation prompts get it only if the supervisor puts it in the prompt**, as for Q3 and Q4.
- **Open:** re-measure docs reads for Q3/Q4 when the export includes them, and check whether the step is actually followed in the next fresh-context brief (the evidence so far is that goose reads docs when told and not otherwise).


### Stage 9 sessions Q, Q2, Q3, Q4 (2026-10-26/27): `curl` completes an HTTPS fetch in the guest; the body check (Q2) is blocked by a new post-fetch wedge

**Result.** Brief `scripts/kvm/goose/taoxichuan-08q-curl-https.md` (executable premises in `premises-08q.sh`: 8 of 8 held). Four goose contexts: Q 165 calls, Q2 100, Q3 88 (hung), Q4 about 104; about 457 calls and about 23M tokens (weekly counter 131.4M -> 154.7M; the 5-hour window was at 62% at the end). `taoxichuan-s4` @ `666c3e2d`, merged here. **Q1 PASS** on two boots (`curl -sS -o /tmp/h https://raw.githubusercontent.com/netoneko/akuma-playground/f70d39f1.../hello.c` exits 0 on the console; supervisor read it in `vm-dev.log`, no `unhandled` line after the exec). **Q2 NOT PASSED**: the hash of the saved body against the reference (`c563b63a...`, 77 bytes) was never checked, so exit 0 does not yet prove the body.
- **The chain, one syscall per boot (the fixes, in order):** `eventfd2` (libcurl's multi-stack wakeup fd; its absence surfaced as `curl: (27) Out of memory`), `getsockname` (c-ares on its DNS socket; exit 6 `Could not contact DNS servers`), `writev` of a datagram routed to the connect peer, `getsockname` `sin_family` in host byte order (`to_be_bytes` made c-ares read `0x0200`), `getpeername`, and `getsockopt(SO_ERROR)` after the non-blocking connect (EINPROGRESS then unhandled `getsockopt`; exit 7 became exit 0). DNS works in the guest from the `getpeername` boot on.
- **Composition:** `eventfd` pulled out of `akuma-syscalls-glue` into the pure crate `akuma-syscalls-eventfd` (6 host tests; changes one case: a write that would pass `u64::MAX - 1` now answers EAGAIN as Linux does, where glue saturated silently, so the aarch64 kernel changes too); `akuma-net` gained `socket_local_addr`; `getsockopt` mirrors `akuma-syscalls-glue::net::sys_getsockopt`'s answers (taoxichuan cannot depend on glue). `taoxichuan/src` 5,096 -> about 5,400 lines.
- **Supervisor-found defects in goose's commits** (all fixed in `1dd4c29a`): `socket_local_addr` was missing `#[cfg(feature = "smoltcp")]`, so the aarch64 extreme-size build broke at the merge (host tests ran with default features and could not see it; `scripts/build_taoxichuan_fc.sh` did, and I had pushed the broken merge for about 10 minutes because I chained the push after the build); the pre-commit clippy found 5 errors in the new eventfd crate and glue file (`must_use`, long doc paragraphs); `LoadSeg.offset` (session O2) had left two test initializers uncompilable (fixed in `a8d7b215`). Goose ran `cargo test` but neither clippy nor the feature-off build.
- **OPEN (new): the first exec after a completed HTTPS fetch wedges the guest's net path.** After Q1's `Exit status: 0`, the next exec (`sha256sum /tmp/h > /tmp/h.out`) is accepted but never exits; host side `gw_queued=5 gw_frames_from_guest` frozen, vCPU spinning, no Fault or PANIC, and a fresh `true` also never lands. Same on two boots. Goose's unverified suspects: the relay slot's reader after the connection closes, the guest `netpoll` cadence, a pending frame from the closed c-ares UDP socket. It blocks Q2.
- **Other open items:** E1/E2 not re-run this part (boot 3 hit the roughly 30-connection forward ceiling after the 19-exec battery); the main battery passed 19/19 on a clean separate boot (goose-reported, restarts=0, zero Fault); the Statfs gate row was a harness false negative, fixed by goose in `run_gates.sh`; `akuma-amd64` still does not build at this HEAD (the 5 x E0425 in `mem.rs`, pre-existing).
- **Ops traps recorded by goose:** the "mirror is up" handoff was stale (8123 not listening), so check the listener, not the note; a `timeout`-killed ssh client keeps retrying and lands ghost execs that also eat the connection budget (kill by PID, and the pkill pattern matches your own shell); `herd disable vm-dev` deletes `enabled/vm-dev.conf`, so the restore leaves only `available/` (same end state as session O).
- **Process:** two model calls hung (Q3 and the first Q4 attempt, 8 minutes each, nothing running on the box, no z.ai error), handled by killing the driver, a strict goose restart (kill every `goose` process, not just `goose serve`) and a short continuation; the docs-for-debugging finding above came out of this run, and the Q3/Q4 prompts carried it.
- **Host track:** merged build via `scripts/build_taoxichuan_fc.sh`, Lima `fc` boot: 162 lines, `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`.
- **Next:** the wedge first (Q2 depends on it); then q2a/q2 (`sha256sum` and `grep -cx`), E1/E2 and a clean-boot main battery, then stage 8 (git). Session P (probe inventory) is still unsent.


### Stage 9, sessions Q5-Q8 (2026-10-27): the post-fetch wedge, localized to "server finishes, client gets nothing, then a no-netpoll spin"; the spin site is still unknown

**Result.** Four goose contexts, about 270 calls, about 16M tokens (the whole Q family, Q through Q8, about 39M: week counter 131.4M -> 170.3M, 84% of the weekly cap). Q2 (the body hash) is still not passed. **Nothing from Q5-Q8 is merged into `rust-org-il-experiment`**: the litter branch `taoxichuan-s4` ends in `[tmp]` commits, one of which (the heartbeat, `8ddea8b2`/`b1876468`) makes the guest die at boot. **A next session must start from `ba2a04f4`** (instruments stripped, hooks kept) and `git revert` the heartbeat commits, not build on `b1876468`.
- **Q5** (symptom-first and `nettest-connect churn`, as the new template step asks): reproduced the wedge with the existing probe. Outbound connects are healthy before and after the fetch (5/5 each); only the inbound ssh path dies. The pump ran through auth and the exec and showed no `netpoll` laps after the child exited cleanly (`status=0x0`).
- **Q6:** the candidate fix (`blocking_relax`/`yield_now` call `netpoll()`) did **not** fix it (identical counters `gw_tx=368 gw_rx=0 in_flight=369`). It also showed Q5's "pump died" model was partly wrong: `akuma-net`'s `wait_until` pumps `smoltcp_net::poll()` directly and never bumps the wrapper's lap counter, so silent lap prints do not prove a dead pump. Its attempt to swap an instrumented sshd in (wget + killall) failed.
- **Q7:** baked the instrumented sshd into the image via `devroot-ensure` (`AKUMA_DEVROOT_BAKE_SSHD`, tagged `[tmp]`), and bisected the exec shape: post-fetch `cat /tmp/h` passes, **post-fetch `cat /tmp/h > /tmp/h2` wedges**, so the redirect alone is the trigger (pre-fetch redirects pass). sshd's bridge is cleared: every bridge reaped and printed its exit status. The server finishes the whole session, the client receives nothing (the exit report is queued and never flushed), and the guest then sits with LAPIC-only exits, `io=0` (netpoll never called) and the vCPU at 100% with `hlt=0`. The `np lap` print itself (16k io exits/s) flooded `vm-dev.log` and destroyed boot 1's capture; it is removed in `ba2a04f4`.
- **Q8:** tried to find the spin site with a 1 Hz LAPIC-timer heartbeat sampling the interrupted RIP. **The premise was the supervisor's and it was wrong:** the Q8 prompt said the guest takes about 99 LAPIC timer interrupts a second. `irq=99` in the host's `[KVM] cpu:` line is the host's own tick landing inside guest execution (`machine.rs`: `ExitCode::Other(0x60|0x61) => c.irq += 1`); the guest takes no interrupts (no LAPIC driver, IF never unmasked, the emulated LAPIC is SVR-disabled). So the heartbeat meant building a timer from scratch, and the instrumented kernel then **dies at the TSC calibrate on three builds** (`net: [1] calibrate begin`, `vm-dev` exit 1) with the code never executed; goose's leading hypothesis is a codegen/layout perturbation exposing a latent bug (or trap-crate registration state), and the VMM's stop reason does not survive in `vm-dev.log`. Ops were restored cleanly.
- **Process notes:** `boot_dev.sh` waits on `ssh echo __BOOT_OK__` against a guest that already exited (the known 10-retry loop; the supervisor killed the client by PID twice to unblock goose). A pre-existing helper trap: `/var/log/herd/sshd.log` is another service's console. The supervisor twice misread goose's narration (a `PASS/PASS` for `b1/q1` taken as the bisect shapes passing) and told the user a reversal was possible; corrected the same turn. Read the RESULT file before saying anything about a verdict.
- **Next (cheap and safe, no interrupt work):** start from `ba2a04f4`; add marker prints at session-teardown stages in the baked sshd (after `block_on(handle_connection)` returns, before and after `close`) and entry/exit prints in the kernel's socket close/shutdown path, with a once-per-event (not per-lap) rule; the last marker before silence localizes the spin. Then fix, strip the markers, `clippy` the touched crates, and the Q2 / E1/E2 / clean-boot main battery gates. If a RIP sample is still wanted, find out first why a registered-but-never-fired handler kills the calibrate (the VMM's fatal-stop set is in `machine.rs`).
- **Quota:** with about 35M tokens left in the week (resets 2026-11-01 19:35Z) and Q-sized debugging at 5-15M per session, plan for two or three more sessions, not for the git stage, unless the wedge falls quickly.


### Stage 9, session Q9 (2026-10-27): the "wedge" is not a frozen guest; the quiet component is the host VMM relay's slot pump (OPEN, host kernel, not licensed)

**Result.** Goose session 20261027_12, 68 calls, one boot, about 3M tokens; `taoxichuan-s4` @ `2619a243` (two reverts of the Q8 heartbeat, `35dfe1a0` and `5f7bf7b8`, plus `[tmp]` per-event markers). No gate moved. Q2 still not passed.
- **The race theory did not survive its own evidence** (the supervisor had asked for the timeline before any client change). Redirect-shape session (`cat /tmp/h > /tmp/h2`), client capped at 60 s: server-side complete, guest healthy, later sessions (`alive3`, `cat /tmp/h2`) served normally, `h2` holds the full file; the client's exit code 0 means it **received** the exit-status. The passing plain session's client, by contrast, left without it (`close-wait end peer_hung=true saw_close=false`). So "the client receives nothing" is not the wedge signature, and a client-only exit race does not explain the failure.
- **Q7's guest spin (`io=0 hlt=0`, netpoll never called) did NOT reproduce.** Candidates for Q7's terminal state, neither proven: stacked hung-client zombies exhausting the port-22 forward slots (so new clients never reach the guest: no Auth line, guest innocent), or a real guest-side spin that this boot did not show.
- **OPEN, host kernel, NOT licensed to change:** after the session, relay slot 0 froze (`up_pending=176`, `to_host` frozen, `host_send_blocked` climbing +500/s, `tcp_state=close_wait`) with no `[KVM] relay: host socket ... failed` line all boot. In `amd64/src/kvm/relay.rs::shuttle`, `socket_send` can EAGAIN forever and teardown requires `p.host_eof && p.guest_eof_passed && conn.is_closed()` while `guest_eof_passed` needs `p.up_len == 0`: a zombie slot with no error path and no bound. One defect, two plausible symptoms (a stuck client, slot exhaustion). A bound on the up-chunk (a retry cap and an abort in the `note_err` style) is the obvious fix but changes the host VMM kernel, which this experiment does not rebuild; **the user's decision**.
- **Marker design lesson:** the first marker set landed in the `fork-sessions` serve loop, which `amd64/mkdisk.sh:172`'s feature set compiles out; markers must go in the live single-process loop. `net.rs` has only per-lap print sites, so it stayed printless (the Q7 flood).
- **Next (Q10):** get the Q2 verdict from the console with bounded clients killed by PID (the verdict rule is the console's `Exit status`), q2a/q2 immediately after q1 on a fresh boot, relay slot state printed before each exec; then strip the markers, clippy, and the clean-boot E1/E2 and main battery.


### Stage 9, sessions Q10, Q11, R (2026-10-27): the curl stage closes on `curl -sS https://example.com`; the nested `sh -c` + redirect wedge is a GUEST bug, open

**Result.** Q10 and Q11 (goose, about 70 calls) and R (about 40 calls, 2 boots). **Gate (user's, replaces the sha256/hello.c body hash): `curl -sS https://example.com` in the guest prints the real page and the console shows `[SSH] Exit status: 0`.** Verified by the supervisor on the console in `vm-dev.log` (Q11) and by the R battery: `PASS M4curl`, `PASS M4web` (capture 705 B, `<title>Example Domain</title>`), 21/21 PASS, `GATES DONE restarts=0`, zero `Fault|PANIC|sig!` in the live log and the rotated one.
- **Q11 (control, no client killed before its console exit line):** the wedge still reproduced on `sh -c "sha256sum /tmp/h > /tmp/h.out"`, while plain `sha256sum /tmp/h` exited 0 in 5 s. The abandoned-client theory is refuted; the shape (nested `sh -c` plus a redirect; console `Exec: /bin/sh ["-c", "sh -c \"sha256sum ...\""]`) is what triggers it, and the guest vCPU spins (`c4=100% io=0`). The user's view, which the evidence supports: the relay is not the cause (the same curl works against Linux guests through the same relay), so this is a guest bug; the Q9 note that called the host relay slot pump "the user's decision" is withdrawn as a diagnosis (the frozen slot is what the host shows when the guest never closes its side). Shapes (b) and (c) of the Q10 bisect were not run. **Open.**
- **R:** `[tmp]` set stripped: `taoxichuan/src/sock.rs` and `userspace/sshd` main/protocol are byte-identical to `ba2a04f4`, so the Q9/Q10 edits there were probes only, no fixes. `run_gates.sh`: `gate()` waits for the console exit line (plus a 30 s drain grace) before any PID kill; the sha256 gate is replaced by M4curl/M4web (and B6 in the b group). Merged here as a plain diff of three files (`run_gates.sh`, `taoxichuan/src/net.rs`, `userspace/ext2probe/src/bin/devroot_ensure.rs`), not as a branch merge, so the `[tmp]` and heartbeat commits stay out of this history; the litter head is `cf4ea1c4`. `net.rs` keeps Q5's `netpoll()` in `blocking_relax` and `yield_now` (not a fix for the wedge, harmless and in the 21/21 run).
- **Not yet verified:** the `GROUP=b` B6 boot. R's `apk add curl` attempt 1 hit its 240 s cap once on boot 1 (the script's designed single restart fired); boot 2 had none.
- **Next:** stage 8 (git clone), per `TAOXICHUAN_PROCESS_MODEL_PLAN.md` M1 (fd dispatch order) and M2 (blocking fork plus a real `wait4`), with `GIT_MISSING_SYSCALLS.md` extracted into the brief. The nested `sh -c` + redirect spin is likely the same family (redirect and `sh` forking) and is revisited if it blocks a gate.

### Stage 8, sessions T-Y (2026-10-27): M2 blocking fork + `wait4` WORKS — F1 passes; the gate suite is closed here, not extended

**Result.** `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` (F1) runs in one exec on Taoxichuan: console `Exit status: 0` and the raw client bytes `x\n2 /tmp/f\n`; `cat /tmp/f` afterwards holds only `x`; `sh -c 'cat /tmp/f; cat /tmp/f'` exits 0. Verified on the console and the raw client capture (`od -c`, not `strings`, which hides lines under four characters) by the supervisor on three battery runs of `ee1e1874`: G1-G4, H1, H2, H3a-c, F1, C1, D1, D2, A1-A3 PASS every time, zero `Fault|PANIC|sig!`. Goose (glm-5.3-flash) found and fixed all four bugs below; the supervisor designed the A/B that separated two theories and corrected facts it had misread.
- **Merged here as the net diff of five files** (`taoxichuan/src/{child,main,sock}.rs`, `crates/akuma-ring3-amd64/src/lib.rs`, `scripts/kvm/goose/run_gates.sh`), from the box's `ee1e1874`, not as a branch merge: the litter head `260377ff` still carries the `[tmp]` prints, and `ee1e1874`, which removes them, was never pushed.
- **The four bugs, in order found.** (1) `cb19bcca` the fork's page-table clone walked the kernel half too and leaf-copied from frames above RAM (nested #PF at 0x30000000): clone only PML4 slots 0..256. (2) `0963376f` `fork_arm` did not save/restore the crossing statics, so a later in-place exec restored the fork's dead callee-saved registers. (3) `328ce120` + `c042dd01` + `a6822831`: `spawn_arm` and `fork_arm` lower the child's stack top by SIZE/2 and SIZE/4, which put the depth-2 exec band ~0xA00 BELOW the 32 KiB `SYSCALL_STACK` array into read-only data (a later indirect jump fetched rip=0); the array is now 64 KiB, and `exec_replace` keeps its saved registers per frame (`ExecSave`). 64 KiB scales the margin; it does not remove the layout limit, so a depth-3 exec would hit the floor again. (4) `260377ff` busybox ash saves fd 1 with `fcntl(1, F_DUPFD, 10)` around a redirect; fd 0/1/2 of the spawned shell have no table entry, the dup failed with EBADF, and the restore never ran, so cat/wc output leaked into the file. A dup of an unbound stdio fd now materializes `OpenFile::Session` through the `akuma-fdtable` API.
- **A wrong theory, kept for the record:** session T blamed `wc` being a busybox symlink. The A/B (`cat` twice vs `wc` twice) showed the failure follows the SECOND exec in one shell, not the file.

**The suite is closed, and why.** The 35-exec main battery takes 9-10 minutes without `apk add curl` and 25-30 with it (`PACE=15` s after every exec). The host forward dies near ~30 connections per boot, so a battery that long restarts the VM once and the gates after the restart (W1, `apk` state) then fail for state reasons, not kernel reasons. The restart moved with the connection count: at M4 with F1 in main, at M4 with F1 moved to the `b` group, at A2 with M4 removed. (An earlier note here called the connection-count theory refuted by the F1-moved run; it was not: main without F1 is still near the ceiling.) User's decision: this suite is finished and marked done; `apk add curl` / `curl example.com` (M4curl, M4web) count as passing from the earlier runs (R: 21/21 `restarts=0` on its second boot; run 2 of this session: both PASS) and leave the standing battery; the next stage starts a NEW small suite, with its own few gates on one boot. Rule adopted: once a stage has a few clean runs on record, its gates leave the standing battery; they run once more as a spot check before a merge to origin. Same for `git clone` and `cargo fetch` when they close. Not done: splitting `run_gates.sh` into boots of at most ~20 execs, and lowering `PACE` (15 s was set when the forward flaked; test, do not guess).

**Process findings.**
- **Model-stream errors.** About a third of goose sessions died with `Network error: Stream decode error: error decoding response body` after 10-20 calls (11 times today), losing all context. The box's wifi data plane froze for about a minute at one sample (rx/tx counters identical, lap counter 28,787 -> 5,897), which correlates but is not proven to cause it. A retry wrapper (restart goose, resend the prompt, tell it to read `RESULT-*.md` and `git status` first, 9-minute silence watchdog) made sessions finish; every goose-written result still has to be verified on the console by the supervisor.
- **Wifi data-plane freeze on the ryzen (host, not Taoxichuan).** Two ssh drops today. The Akuma partition's `klog-229` and `klog-231` (read from Pop) show the kernel alive until the reboot with `[rtw] link` counters frozen for 314 and 61 consecutive reports; in both, the last "group rekey" join precedes the freeze and none follows. The driver does not notice a dead link. To investigate later (`crates/akuma-rtw89*`).
- **Doc use after the methodology pivot** (`goose_session_stats.py`, 41 sessions): before the pivot 2,654 calls / 233M tokens, docs reads 5.0% of calls, source reads 23.9%; after, 1,364 calls / 76M, docs 5.3%, source 20.1%. Tokens per call fell about 40% (88k -> 56k), docs use did not change: the saving is session length and milestone scope, not more reading of docs. The script counts only shell commands that read `docs/*.md`.
- **Tokenomics.** Today (T-Y) about 14M GLM-5.3-Flash tokens, about 6% of the weekly cap (week 1% -> 6%).
- **Supervisor misreads, corrected openly:** claimed `ring3-amd64` was shared with the main amd64 kernel (only `taoxichuan` depends on it); read `tB`/`F1` consoles as empty when they held only host stats after a wedged boot; `strings` hid one-character client lines.

**Open.** (a) `cargo build -p akuma-amd64 --target x86_64-unknown-none --release` fails (5 x E0425, `amd64/src/mem.rs:117`): this branch's `43edbffc` boot-kit extraction left a test-only function using `PHYSMAP_LIMIT`/`PAGE_SIZE`; it builds with `--features no-tests`, which metal kernels use. (b) `ring3-amd64`'s 64 KiB array and the extracted crates (`akuma-boot-amd64`, `akuma-comconsole`, `akuma-elf-exec`, `akuma-ring3-amd64`, `akuma-syscalls-eventfd`) get a proper review after the experiment, user's call. (c) The `apk add curl` restart rate versus the pre-fork kernel was never measured (session Y was stopped by the user's decision to skip it). (d) Pipes and the real process model (`TAOXICHUAN_PROCESS_MODEL_PLAN.md` M3/M4) are the next real work; M2's blocking fork is throwaway scaffolding by design. (e) The trashcan (HP box) was offline; an amd64 `--features no-tests` deploy is staged but not done.

### Stage 8, session pa (2026-10-27): M3 pipes — `pipe2`/`pipe` + small pipelines; the new suite `run_pipes.sh` P1-P5 PASS

**Result.** `sh -c 'echo hi | cat'` and four more pipeline shapes run on Taoxichuan over the M2 blocking fork. Verified by the supervisor on the console (`[SSH] Exit status: 0` for P1-P5, zero `Fault|PANIC|sig!`) and on the raw client captures with `od -c`: after the client's own `[ssh] connecting to ...` line, `hi`, `3`, `2 2`, `hi`, `done`. F1 spot check exact (`x\n2 /tmp/f\n`). Lima aarch64 host-track boot: 164 lines (244 more are `[WATCHDOG]` host sleep/wake jumps), `Ext2 filesystem mounted`, `sshd started`, 0 faults. Merged as the net diff `ee1e1874..f2b15e6c` (taoxichuan/, Cargo.lock, run_pipes.sh), not a branch merge. Goose: 94 calls, 3 boots (two wasted on a stale kernel: the box build needs `kbuild -p taoxichuan`), about 4.7M tokens (week 7% -> 10%).
- **Pulled / written new.** `akuma-pipes` (+ `akuma-pipe`) pulled as a path dependency, no FIFO written. `taoxichuan/src/pipes.rs` (178 lines of glue) plus an `OpenFile::Pipe(u32, bool)` variant handled in `main.rs`/`child.rs`/`sock.rs`. `taoxichuan/src` 5,833 -> 6,123 lines.
- **Premise misses.** The brief assumed `pipe2` (293) was enough; musl's `pipe()` is the raw `SYS_pipe` (22), so boot 1 died with ash `can't create pipe: EBADF`. Premise 8 was WRONG as predicted: `child_exit` never released a fork child's fds, so a counted pipe end leaked and EOF never came; fixed with a sweep at the top of `child_exit`. Premise 6 HOLDS: fork counts P1 2, P2 3, P3 4, P4 2, P5 2.
- **A pre-existing bug found by P3.** Fork children took the spawn ring's blind `NEXT_SLOT++ % NSLOTS`; with `NSLOTS = 4` a four-fork pipeline wrapped onto the LIVE shell's slot and `SpawnSlot::new()` wiped its session buffer (the second `2\n` was lost). Fork now reuses a dead slot.
- **Not served:** `sendfile` (40) answers ENOSYS once per `cat` exec; busybox falls back to read/write. A cheap follow-up. Anything over 64 KiB, concurrent writer/reader and SIGPIPE are M4's.
- **Supervisor misses.** The brief said `pipe2` where musl's `pipe()` is syscall 22. `run_pipes.sh`'s raw `cmp` of the client capture could never pass: the client prints its connect line and appends a newline; the verdict now strips exactly those two additions (console exit line stays primary).
- **Ops note.** Goose copied `/etc/sshd/id_ed25519` to `/root/.ssh/id_ed25519` on the ryzen box so captures carry command output; not in the brief, same key the client already fell back to. Remove it if unwanted.

### Stage 8, session pb (2026-10-28): M4 foundation — `akuma-exec`/`akuma-threading` linked, a PIT-calibrated LAPIC tick under ring 3, the SMP-ready sched-lite plan

**User's decisions this session:** the kernel gets preemption (the cooperative-only design the supervisor first floated is withdrawn) and the design is SMP-ready, with booting extra cores a later slice. **Result.** (PB1) `akuma-exec` + `akuma-threading` link into `taoxichuan` (nm: 41 + 56 symbols; ELF 917,416 B); (PB3) `run_pipes.sh` P1-P5 and the F1 spot check still pass with the deps pulled; (PB5) `lapic: id=0 timer vector=32 periodic; calibrated=1 counts/tick=624937`, and one 200k-iteration ring-3 exec shows `[TICK pre] n=20337` -> `[TICK post] n=20361`, exit 0 (supervisor read the console, boot 2 starts at log line 12030 and the only `PANIC` is boot 1's at 12019); (PB4) `docs/archive/TAOXICHUAN_M4_SCHED_LITE_PLAN.md`: hook tables for `ThreadRuntime`/`ThreadConfig`/`ProcessHooks`/`X86ArchHooks`/`ExecRuntime`, the blocking syscalls and the switch in `timer_trap`, the `SYSCALL_STACK` interaction, the SMP list, the slice list. Merged as the net diff `f2b15e6c..c3a37692`. Lima aarch64 host-track boot: 163 lines (excluding `[WATCHDOG]`), mount + sshd, 0 faults. amd64 `cargo check --features no-tests` clean, clippy clean, unsafe budget ok.
- **Extracted.** `crates/akuma-lapic-amd64` from `amd64/src/lapic.rs` (1121 -> 719 lines; amd64 re-exports it, the IPI face stays there). `forbid(unsafe_code)`, budget 0. **It has no host tests** (the moved code had only in-kernel suites): a gap to close before the SMP slice.
- **Boot 1 panic:** `lapic_init` ran before the PMM/heap registration and `map_device` allocates page-table frames; moved after (`9dc684ef`).
- **Premise corrections (goose):** `ThreadRuntime` has 4 IPI/IRQ-bound fields, not 2; `fork_process`'s subject is `ProcessImage`, not `Process`. `dd` does not exist on the guest (no `/dev` at all); a shell loop is the tick gate. Goose: about 81 calls, 2 of 3 boots, quota in the entry above this one's session.
- **Open for M4 slice 1:** `forkprobe` is the named M4 gate (libakuma, builds x86_64 static); `timer_trap` is where the task switch lands, ring 3 and idle only, never ring 0.

### Stage 8, session pc (2026-10-28): M4 slices 1-2 — the runtime twin is registered and the crossing statics are per-slot state; behaviour unchanged, no switch yet

**Result.** `taoxichuan/src/exec_twin.rs` (371 lines) registers the `akuma-threading`/`akuma-exec` runtime twin per plan § 1 (slice 1); the crossing statics (`RET_RSP`, `SAVE_*`, `SPAWN_OUT_SLOT`, `PARENT_*`, `NESTED_*`) became per-slot state looked up through `current_slot()` (slice 2; `static mut` in `child.rs` 17 -> 8 by the supervisor's grep, 16 -> 4 by goose's count of "who am I" statics). Supervisor read the console: boot 3 starts at `vm-dev.log` line 13684, the last `[Fault]` is boot 2's at line 13676, P1-P5 `Exit status: 0`, F1 exact, ticks advance. `akuma-lapic-amd64` now has 7 host tests. Merged as the net diff `c3a37692..b1acc953` (conflicts in `lib.rs` and `main.rs` with the supervisor's own clippy edits; goose's versions taken). Lima aarch64 boot: 163 lines, mount + sshd, 0 faults.
- **Boot 2 caught a miscompilation, a real hazard of this design.** Moving the crossing restore into a function let the compiler use registers across the `asm!` that the restore is supposed to own; the child resumed with a wrong callee-saved register. Fix 1 (`f759944d`): explicit register bindings for the call. Fix 2 (`b1acc953`): the crossing-back tail is a `#[unsafe(naked)]` function, `exit_cross`, so the compiler cannot touch the registers around it. More hand-written asm, but the kind that cannot be miscompiled.
- **P3 stall on boot 3, explained by the console:** the first P3 connection was accepted but never reached `[SSH Auth]` (the known host-forward degradation); the second ran and exited 0 with `2\n2\n`. Not a kernel fault.
- **Open.** The hooks table in `RESULT-pc.md` is the slice 1-2 record; slice 3 (`fork_process` + `wait4` over child channels, gate `userspace/forkprobe`) is next, then preemption (slice 4) and the SMP slice. Goose: about 157 calls (past its 70-call budget), 3 boots.

### Process finding (2026-10-28): this supervising session deviated from the plan — too many tips in briefs and steers, instead of pointing at docs and giving general instructions

Recorded at the user's request. The runbook (`scripts/kvm/goose/SUPERVISOR_RUNBOOK.md` § One milestone = one session) says the brief carries **no hints about the bug: docs and probes yes**, and the handoff for this session said goose reads docs rarely and source heavily, so *quote the relevant doc passage or the probe list*. The supervisor (Sonnet 5.5) did the opposite in three of four briefs and in some steers:
- **Briefs pa, pb, pc** (`taoxichuan-09a-pipes.md`, `-09b-m4-deps.md`, `-09c-m4-twin.md`) carried source-level pointers (file and line ranges in `amd64/src/fd.rs`, `sched.rs`, `exec_runtime.rs`, `child.rs`, `akuma-threading`), pb and pc prescribed step lists, and pa named the answer: it told goose where reference counting belongs (`fork_copy` -> `clone_ref`, the `CLOSE` arm) and listed premise 8 as "the likely bug". Goose then confirmed exactly that. The premise table was meant to test the brief's claims, not to hand over the diagnosis.
- **Steer 1 of pd** told goose what to build (register a `Process` for the shell and call `fork_process` behind the flag) when goose had already found the problem itself (premise 2, WRONG) and the brief already required the selectable flag. The earlier pa/pb/pc steers were process-only (commit, write `RESULT-*.md`, hold the push).
- **Brief pd** was rewritten before sending, at the user's instruction ("point to docs, not source, it will find the source"): goal, gates, docs, no line numbers, no decomposition.
- **Where the plan was followed:** the composition-rule block stays in every brief (the template requires it); briefs went as files; sessions ran through `retry_goose.sh`; every result was verified on the console and with `od -c` by the supervisor, not from goose's narration.
- **Outcome.** The tips did not change a gate verdict that the supervisor could see, but they made the briefs long (9 KB for pa) and made goose's results harder to credit to goose. Going forward: briefs name the goal, the gates and the docs; steers are about process (commit, RESULT file, push rule, budget), not design.

### Stage 8, session pd (2026-10-28): M4 slice 3 — `fork_process` runs on Taoxichuan and the switch is written, but F1 through the new path is NOT green; nothing merged or pushed

**Result.** Not green. Goose (about 146 calls, 4 boots) built the premise-2 build-out and a first real switch, then hit its stop rule (F1 through the new path failed on two boots). Its five commits (`437a898e` .. `33378e5d`) were never pushed to litter and are NOT merged to origin: the default is `REAL_FORK = true` (flipped for the boot-1 refusal read) and the tree carries two `[tmp]` prints, so merging it would break the working M2/M3 path. They are kept as the local branch `taoxichuan-pd-wip` (fetched from the box by `git bundle`; local to the supervisor's checkout, not pushed). The M2 blocking arm stays compiled and selectable.
- **Proven by goose's own boots (console not re-read by the supervisor this time):** `fork_process` runs clean on Taoxichuan once the sshd-spawned shell is a registered `Process` (premise 2 was WRONG as predicted: `fork_process` opens with `current_process_shared().ok_or("No current process")?`; Taoxichuan registered none). Registration, the fork memory pass, publish and context capture are real; `switch_to`, the idle thread, the `wait4` park and real-task exit/exec arms landed in `bb36ab3a`.
- **Where it fails:** the forked child never reaches its first syscall dispatch (no `[tmp]` line from the dispatcher head), the shell's crossing nonetheless ends (`[TICK post]` without `[tmp] exit`), and a deterministic `rip=0` kernel fault kills the session. Goose's next reads: disassemble the never-before-executed naked `akuma_ring3_amd64::enter_user` against `child_enter`'s inline copy; trace why tid 0's crossing ended while slot 1 ran (`wait4_real`'s WNOHANG/EFAULT early returns).
- **Still open for the slice:** pipe park/wake (the dropped `_wakes` in `pipes.rs`), PK1 `yes | head -n 20000 | wc -l`, PK2 (`forkprobe`), and a SIGPIPE-default stand-in (`yes` needs to die on a broken pipe; today `-EPIPE` with no signals).
- **Premise 4 caveat.** Goose marked `forkprobe` as speaking the Linux x86_64 ABI from `userspace-layout.md` alone; unproven until the probe runs.
- **Supervisor errors, recorded.** (1) Goose's RESULT said ops were restored; they were not (`vm-dev` still enabled, `vm-serve-mc` halted: its final message was cut off at "Final bookkeeping:O"). The supervisor restored them by `herd disable vm-dev` + `herd start vm-serve-mc` after the session. (2) The supervisor ran **bare `herd`** on the box to read a usage line: `herd` with no arguments starts a second supervisor (`cannot listen on 127.0.0.1:7117`) which launched duplicate children (a second `goose serve`, `klogd`, `powersaved`). The supervisor killed them by PID; three entries (34096, 34098, 34100) still show in `ps` with 0:00 CPU after SIGKILL (parent gone, unreaped) and clear on the next reboot. Use `herd status`, never bare `herd`. (3) The two "finish at the next gate" steers to goose timed out without an ack, so it is unproven that goose read them; it stopped on its own stop rule.
- **Process.** From the user's feedback, the pd brief pointed at docs and the goal and left the code to goose; the one steer that gave design direction (register a `Process`, call `fork_process` behind the flag) was the same content the brief already required, and goose had already found the problem itself.
- **Correction (same day): pd probably ended on the 5-hour quota cap, not only on its stop rule.** `zai_usage.py` right after the session read the 5 h window at 100% (36.1M tokens, 356 calls; resets 04:13Z) and the week at 30% (57.5M). Goose's final message ended with "Please retry if you think this is a transient or recoverable error." and its ops-restore step was cut off mid-word, which is how a quota refusal looks. Earlier this day the supervisor said the window was "fine" from the morning's 12% reading and did not re-check between sessions: pc alone cost about 16M tokens, pd about 15M. Check `zai_usage.py` between sessions, not only at the start and end of the day.

### Stage 8, session pe (2026-10-28): M4 slice 3 again, plan first — the real fork child, `wait4` park, wake, switch back and reap WORK on three boots; the client verdict and pipes are not done

Plan and result are archived beside this log: [`TAOXICHUAN_M4_SLICE3_PE_PLAN.md`](TAOXICHUAN_M4_SLICE3_PE_PLAN.md), [`TAOXICHUAN_M4_SLICE3_PE_RESULT.md`](TAOXICHUAN_M4_SLICE3_PE_RESULT.md). Branch: `litter/taoxichuan-s4` at `983dc1c5` (local ref `taoxichuan-pe-wip`). **Not merged to `rust-org-il-experiment`**: `REAL_FORK` defaults to `true` in that tree, it still carries `[tmp]` prints, and the pipe arms are still the M2 ones, so merging would break P1-P5 exactly as pd's branch would have.

**Brief and method.** Brief `taoxichuan-09e-m4-fork-plan-first.md`: goal, gates, docs and probes, no source pointers and no diagnosis; the user's instruction was a plan from the pd checkpoint first. Goose wrote `PLAN-pe.md` after about 74 calls of docs and source, before any build or boot. Doc use for the final attempt (`gss.py`): 186 calls, 33 before the first edit (session Q: 61), 7 docs reads over 5 files, 38 source reads, 24.6M input tokens; the two earlier attempts are not in that figure.

**What the plan found, and the boots proved.** (1) pd's `rip=0` was the EXIT arm sending a real child's `exit_group` into the M2 `child_exit` (`nested_active()` is true for the child) and returning through a never-initialised crossing save; fix A reorders the test, fix A' makes `is_real_task()` a real discriminator (a bind-time bit). (2) A second, independent cause behind the next fault: the `sched_*` maps hardcoded the shell as threading tid 0, but its crate tid is its spawn-ring slot, so the wake switched back under a stale record; fix C keys on the live shell tid. pd's two hypotheses (`enter_user`, `wait4_real`) were killed on paper by the plan and never needed reopening. On the surviving consoles (checked by the supervisor in `vm-dev.log` and `.old`, three boots): `enter tid=1` -> `exit tid=1 code=0 wake=2` -> `sw to=2` -> `w4ret rax=0x5` (the child's pid) -> `rax=0` (the reap), no `Fault|PANIC|sig!` in those ranges. The M2 path with the flag off stays green (F1 exact, P1-P5 pass, boot B).

**Not done.** F1's client bytes and exit status through the new path, P1-P5 and PK1 through it, PK2 (`forkprobe` is a repo crate, not in the image). Goose's diagnosis, not verified by the supervisor: the ssh channel serves about two connections cleanly per boot and then stalls, and once an exec's output is orphaned the writev arm's EAGAIN loop (no backpressure, the socket cousin of K9) leaks the guest to death. Fix B (pipe park/wake) is the next step either way.

**Traps found (goose).** `boot_dev.sh` given a repo-relative path installs an old artifact: boot 1 was void, verify the install line's size/md5 against the fresh `/root/ktarget` artifact. `vm-dev.log` rotates at about 250 KB and whole-log fault greps match older boots' lines: attribute faults by a per-boot log mark.

**Supervisor findings, recorded openly.**
- **The watchdog was wrong.** `retry_goose.sh` killed a live boot battery on a flat 9 minutes of log silence (try 1 at 712 s and try 2), and the driver logged neither thinking chunks nor a running tool. The user's question exposed it. Fixed: the driver writes an `alive:` heartbeat while the model streams `agent_thought_chunk` (hundreds per 30 s were being dropped), and `retry_goose2.sh` allows 30 minutes while the last event is an unfinished tool call and passes `--budget 43200`.
- **The driver's 3-hour turn budget** (its default) ended the log on a live session; the goose server kept the turn running without a listener (the database call count kept rising) and the session finished normally. Found by reading the log tail, then confirmed through `watch_session.py`.
- **A box reset** about 56 calls into the first attempt (both addresses down, Akuma came back by itself); nothing was lost because the plan was to be committed before any build. Cause not determined.
- Smaller: the progress monitor read a stale log through a glob (fixed to the newest log); a second monitor kept running on the old log until stopped; goose did not push to `litter` after its boots although the brief said to (the supervisor pushed from the box at the end).
- **Process kept:** no source pointers in the brief; the only steer was the brief's own 60-call process nudge; every claim above was checked against a console or the git state.
- **Quota:** 5 h window 9%, week 49% (95.4M tokens) after the session; the user said to run it into the ground.

### User's decision (2026-10-28, during session pf): no more time or tokens on a cooperative kernel on a single core

**The statement, as the user put it:** they are personally not willing to waste time and tokens on a cooperative kernel on a single core.

**Context.** Session pf's first Fix B boot completed F1's `echo` and `cat` through the new path (fork, run, exit, wake, reap) and then wedged: the guest at 100% CPU, no system calls, the network starved, only timer ticks arriving. Goose's working theory is a task spinning in ring 3 on a kernel that has no timer preemption, so nothing else (the net thread, sshd) can run; it had named this the same event as pe's "ssh channel stalls after about two connections per boot", which pe had read as a channel problem. The pe brief's line "timer preemption is the NEXT slice (switch only where a task parks, never in ring 0) - do not reopen" had put the gates first and preemption second. The user's pb decision (preemption, SMP-ready design) already stood; what changes here is the order.

**What changes.** If the watchdog boot confirms the theory, goose is told to plan and build timer-driven preemption inside this session instead of stopping to report it, with a design that is SMP-ready, and to learn how it is done from the two kernels in this repo that already preempt and run SMP (the amd64 and aarch64 Akuma kernels, through the process and threading crates taoxichuan already depends on) and pull rather than copy. The gates (F1, P1-P5, PK1, PK2 with `REAL_FORK` on) are unchanged; preemption is what is expected to let them pass. Relayed to the running goose session as steer 2 of pf by the supervisor, marked as the user's decision and as superseding the brief. The cost of reordering is recorded here, not hidden: the next evidence for or against the cooperative theory is the watchdog boot, so if that boot shows a different holder of the core (a pick or map bug rather than a ring-3 spin), the preemption work does not start and this entry is corrected by date.

### Supervisor handoff (2026-10-28 evening)

The live state at the end of this supervising context; the fresh-session prompt is `scripts/kvm/goose/SUPERVISOR_PROMPT.md`. Times are box UTC unless marked.

**Where the work is.** Mac branch `rust-org-il-experiment` carries the logs, briefs and tooling (last commits: the tooling commit `8d406451`, the owner->user sweep `97490819`). Goose's kernel work is on `litter` branch `taoxichuan-s4`, at `ce32a336` when this was written (session pf's trace 5); sessions pe and pf are NOT merged to the Mac branch (`REAL_FORK` defaults to true, `[tmp]` prints remain, the pipe arms are not finished). `taoxichuan-pd-wip` and `taoxichuan-pe-wip` are local refs of older tips. Briefs: 09e (pe), 09f (pf), `kimi-plan-preemption.md` (Kimi).

**Running now.**
- *goose-glm, session pf, try 2*: goose session `20261028_21`, driver log `/tmp/goose-pf-2.log` on the Mac, wrapper `retry_goose2.sh` (tag `pf`, its output in the scratchpad `retry-pf.out`), box files `/root/dev-cycle/taoxichuan-s7/{BRIEF,PLAN,RESULT}-pf.md`. It had written Fix B (pipe park/wake), `run_fork.sh`, an opt-in PK2 probe bake and four `[tmp]` trace builds; its first Fix B boot showed `echo` and `cat` completing through the new path and then a wedge (guest at 100% CPU, only timer ticks), which it reads as a ring-3 spin on a kernel with no preemption. Two steers (symptom-first, then the user's preemption decision) were sent; the first attempts never arrived (a `timeout` wrapper killed the script silently), the combined one was acknowledged (message 14523 in the session database) but `PLAN-pf.md` had no preemption section yet when this was written, so whether goose acted on it is **not established**.
- *goose-kimi planner*: server `127.0.0.1:3285` (pid from 2026-10-24, reused), session `20261028_1`, driver log and worktree in the scratchpad (`kimi-plan.log`, `kimi-wt/`), output `kimi-wt/PLAN-kimi.md` (pending), brief `scripts/kvm/goose/kimi-plan-preemption.md`. It started with a read-only brief; the user pointed out that a planner should be allowed to run host-side cargo on the crates it considers, so the rules were relaxed in the archived brief, but the mid-turn steer could not be delivered (see the runbook: no answer to `session/load` from the Mac goose 1.41.0 while a turn runs). Plan: when its turn ends, send the relaxed rules as a follow-up with `goose_acp_say.py`.
- Scratchpad: `/private/tmp/claude-502/-Users-netoneko-github-com-netoneko-akuma/92196a57-f57d-4d69-8b74-b0b562261a61/scratchpad`. Monitors are bound to the supervising session and must be re-armed in a new one.

**Quota (2026-10-28 ~22:20 IDT).** z.ai: 5 h window 42% (14.0M tokens), week 55% (108.8M), weekly reset 11-03 11:51Z; the user said to run it into the ground. Kimi: 5 h window 100/100 remaining at start, month 69.8% used, reset 2026-10-30 00:00Z.

**Open.** (1) Whether pf's watchdog boot confirms the ring-3-spin theory, and if so goose building preemption (the user's decision); the log is the place to correct this entry by date if the holder of the core is something else. (2) Kimi's plan -> check against the docs -> `KIMI-PLAN.md` on the box -> tell goose-glm. (3) Slice 3's gates (F1, P1-P5, PK1, PK2 with `REAL_FORK` on) are still unproven through the new path; the process model's core (fork child, `wait4` park, wake, switch back, reap) is proven on consoles from session pe. (4) Merging pe/pf into the Mac branch waits for green gates and no `[tmp]`. (5) Why the Mac goose never answers `session/load` mid-turn. (6) The unexplained box reset during pe attempt 1.

**What this context got wrong, for the next one.** It reported two steers as sent without confirming (fixed: `steer.py`); its watchdog killed live boot batteries and ignored the model's thinking stream (fixed: heartbeat, tool-call allowance, 12 h budget); it limited Kimi to read-only until the user pointed out the repo and its crates are the point; it used "owner" for the user (swept).

### Kimi planner dropped (2026-10-28, late)

The user has been dissatisfied with Kimi repeatedly, not once: goose had trouble with it on amd64 (raised before the planner was started); the first planning session stopped on the 5-hour limit after 37 tool calls with nothing written; that one session also took 13 points of the monthly allowance (82.8% used); the quota is counted in requests, so the brief had to be rewritten around "write early, batch calls, 35-call cap"; and a mid-turn steer could not be delivered to it at all (`session/load` unanswered on the Mac goose). After the context compaction the user said: "ok fuck kimi their setup is ridiculous, we'll have to make it with glm i guess."

Decision: **no more Kimi sessions in this experiment.** The waiter `kimi_when_ready.py` was killed and the local Kimi `goose serve` stopped; no `PLAN-kimi.md` exists. The middleman step in the runbook is moot; goose-glm plans and builds preemption on its own from the repo's amd64 and aarch64 kernels, as already ordered. The Kimi sections of `SUPERVISOR_RUNBOOK.md` and `SUPERVISOR_PROMPT.md` are history, not instructions.

Also noted: goose's commit `8d024ab2` (timer-driven switching, pushed to `litter/taoxichuan-s4`) says "per owner decision" in its message; the pushed history is left as is.

### Session pf and pg (2026-10-28 evening), brief 09h

**pf** (brief 09f): measured the starvation (the shell spinning in musl's `__stdio_write` writev retry loop, one thread READY and never picked), built timer-driven preemption as the user ordered (ring-3-only switch, per-slot trap stacks, armed at the first spawn crossing, requiring a live slotted current), and bisected two silent boot deaths from console evidence. No gate verdict was readable: the `sw to=` print rotated the console log every ~30 s. Archived: `TAOXICHUAN_M4_SLICE3_PF_{PLAN,RESULT}.md`.

**pg** (brief 09g, ~245 tool calls, ~2 h, 9 boots): the brief asked it to grep the archive and compare with the amd64 and aarch64 kernels before instrumenting (the user's instruction). It did: both route the same busybox through `akuma-syscalls-glue`'s park-with-backpressure channel arm, which never answers 0, and that did not explain the loop. It then instrumented and found three bugs, each hidden behind the one before: (1) `switch_to` loaded the incoming CR3 but not `TASK_SLOT`, so after a park-resume the fork child's writev gather read zeros from the wrong address space and answered 0 (the spin); (2) the M2 exec arm overwrote the spawn crossing's saved sshd frame, so every `sh -c`-wrapped command left the session hung with its bytes undrained; (3) a real fork child's cross record kept its defaults, so its output went to slot 0's buffer. Plus an image fix (`/bin/yes` was missing). **F1 is green** (`[SSH] Exit status: 0`, client `x\n2 /tmp/f\n`, zero faults, `REAL_FORK` on). PK1, PK2 and P1-P5 have no trustworthy verdict: the supervisor's reading of the capture files (PK1 38 client bytes, PK2 no PASS line, P clients "connection lost") may predate the `yes` bake. Archived: `TAOXICHUAN_M4_SLICE3_PG_{PLAN,RESULT}.md`. Brief 09h (`taoxichuan-09h-m4-close-slice3.md`) asks for readable verdicts first, then the remaining gates, then the `[tmp]` strip.

**Supervisor notes.** The driver log is blind during a long tool call (a boot battery); the monitor now resets the watchdog clock when files on the box are changing, which worked. `steer.py` computed the repo root one directory too high (fixed in `081fd8e1`), and the box's goose server did not answer `session/load` mid-turn, so the user's "grep the archive and look at the other kernels" instruction went into brief 09g instead of a live steer. A quota-window time was misread once (local time against UTC); the user corrected it. Quota after pg: z.ai week 75% (149M tokens), 5 h window 46%.

### Session ph and ph2 (2026-10-28 night to 10-29 morning), brief 09h

**ph** (~175 tool calls, 5 boots) found and fixed from console evidence: the slot recycler race (`reserve_slot`), pipe refcounts on fd-table clone (`161f55a1`, `ff698f61`), then ran into the z.ai 5 h limit with the `[tmp]` strip uncommitted. **ph2** (resume brief, ~110 calls, started by the waiter script the moment the window opened) committed the strip (`f47e87be`) and found: the P2 "failure" was never the pipeline but a connection that never authenticated because the guest had gone silent (`aba585a1` netpoll tick backstop); every guest wait path burned the one pinned host core with `pause`, starving the relay thread (`60658556`, `net::wait_lap` = `sti;hlt;cli`); and PK2's probe crashed because an in-place exec entered the new image with the old TLS base (`cde2247d`, both exec crossings zero `fs_base`).

**Verified by the supervisor from the box captures, one boot (ph-9):** F1 `x\n2 /tmp/f\n`, PK1 `20000\n`, P1-P5 exact bytes, every console `[SSH] Exit status: 0`; `REAL_FORK` on by default, no `[tmp]` prints left (two comment mentions in `exec_twin.rs`). **PK2 is not green**: the probe is in the image and exits 0, but its client capture is empty (the output path stalled). The runner's "faults" FAIL is a stale mark file (`logmark-pf.txt` still holds ph-7's grandfork line), not a fault in ph-9. The runner's summary line says FAIL for that reason; goose reported it honestly.

**Open:** a residual intermittent stall (ph-7 clean, ph-8 stalled at exec 1, ph-9 clean): an un-converted spin on the single pinned host core. Next brief 09i: find it with the amd64/aarch64 idle contract as reference, switch the runner to the boot's own mark, get PK2's ALL PASS, then merge.

**Supervisor notes.** The waiter (`ph_resume.sh`) worked unattended across the quota gap. The monitor's direct ssh to the box failed (the box is not on port 22; `ak.sh` is the route); the watchdog's 1800 s tool limit covered the boot batteries anyway. Quota at the end: 5 h 19%, week 90% (177.6M of ~205M).

### First compiler probes on Taoxichuan (2026-10-29): tcc works; gcc and rustc blocked at known gaps

Run on the Mac, no ryzen needed: `taoxichuan-s4` at `cf8a5468` built with `cargo build -p taoxichuan --target x86_64-unknown-none --release --features no-tests` (12 s), booted under `qemu-system-x86_64 -M microvm,accel=tcg` with a hand-built ext2 root (Alpine 3.22 x86_64 minirootfs, plus the `tcc`, `tcc-libs`, `tcc-dev`, `musl-dev`, `gcc`/`binutils` apks, plus the nightly-x86_64-unknown-linux-musl toolchain, the same `1.100.0-nightly (420ed2a0c)` as the trashcan), `mke2fs -d`, no root needed. The guest takes virtio-mmio slots from the cmdline (`virtio_mmio.device=512@0xfeb00000:5` is QEMU microvm's spelling; the disk must sit on `bus=virtio-mmio-bus.0`; at most 8 entries are read).

- **tcc works.** `init=/usr/bin/tcc initargs=-o,/work/hello,/work/hello.c` compiled and linked inside the guest (exit 0, a 3,506-byte ELF written to the ext2 root), and a second boot with `init=/work/hello` printed `hello from tcc in taoxichuan`, exit 0. Alpine's `tcc` package ships no `libtcc1.a`; an empty archive (`!<arch>\n`) at `/usr/lib/tcc/libtcc1.a` is enough for a program that needs no helper routines. So a **C environment can be used on this kernel if needed** (a compiler that writes a file, then the file runs).
- **`tcc -run` faults**: `PageFault ... code=0x15` at an address in the mmap slab: an instruction fetch on a present page. The `MPROTECT` arm accepts and ignores every call, so no page ever becomes executable. First real gap (JIT, `tcc -run`, any runtime that mprotects code).
- **gcc**: `cannot execute .../cc1: posix_spawn: Operation not permitted`. fork and exec work only for children of the spawn crossing (the sshd-style path), not for init. A gcc probe needs sshd (or a small spawner) as init.
- **rustc**: `Error loading shared library librustc_driver-...so: Function not implemented` is the `$ORIGIN` problem (no `/proc/self/exe`, the same reason `/etc/akuma-dev.env` sets `LD_LIBRARY_PATH` on the trashcan), and init has an empty environment. With `LD_LIBRARY_PATH` given, the error becomes `Out of memory`: the mmap slab is a fixed 64 MiB (`SLAB` in `main.rs`) and the driver library is 312,554,744 bytes (about 298 MiB).
- **Smaller notes:** a flood of `[E2-EOF] inode=... caller believed the file extended past off` lines while loading a shared object (one per page past EOF; harmless but noisy); `fchmod` is ignored, so a created file is 0644 (exec does not check); the banner printed is the old hard-coded art, since no brief ever told goose to use the user's `taoxichuan/src/banner.txt` (the user is including it by hand; the change is theirs).

### The pivot (2026-10-29, evening): glm plans, the supervisor executes

**The user's note.** Token constraints, bad planning and subpar tooling pushed the user to pivot once again. From here **glm keeps the planner role for as long as that can be kept going**: it reads the docs and the source and writes the plan; the supervising Claude session executes the plan locally, on the Mac, in a QEMU loop. Recorded as the user's decision and reason.

**What led to it (the day's evidence, all re-checkable).**
- Sessions pi, pj and ph-ph2 closed the main gates (F1, PK1, PK2, P1-P5 verified by the supervisor from the box captures on pj's merge tree, kernel md5 `26de094f`), but each cost 100-175 tool calls and 15-35M z.ai tokens (the 5-hour window of 34M was hit twice, the weekly cap once), and the stall that wastes every boot battery stayed open (2 of 7 boots for pj; the supervisor's `apk add git` over ssh stalled 2 of 3 fresh boots).
- **Planning.** No brief asked for documentation, so the kernel had none (only archive incident docs); the user found this on 2026-10-29 and had Kimi write `taoxichuan/docs/` (it ran out of quota before finishing). The brief template now has a Docs section. The user's own `banner.txt` was never wired in (no brief mentioned it). The stall was handed to glm as a hunt on a rate of 1 in 4, with a gate script whose verdict window was the whole boot (the supervisor's own probe script repeated the mistake and killed its clients early).
- **Tooling.** Every glm hypothesis cost a box cycle (a build, `boot_dev.sh` ~50 s, a battery, `PACE=15` after every exec, log rotation, ops to restore). The supervisor then built a local loop: `cargo build -p taoxichuan` in 12 s, `qemu-system-x86_64 -M microvm` TCG with the disk on `virtio-mmio-bus.0`, `virtio_mmio.device=` cmdline entries, a user-mode NIC with port forward, ssh from the Mac through Python, serial console to a file. On it: tcc compiled and ran a C program in the guest; `git --version` ran; `git clone` of a public repo got as far as the first fork, with the legacy x86_64 syscalls (`mkdir` 83, `rename` 82, `unlink` 87, `rmdir` 84, `readlink` 89, `chmod` 90, `chown` 92/94, `symlink` 88) answering `ENOSYS`; and on the merged tip the stall reproduced 3 of 3 boots (`[SSH] Exec:` then silence), with the first tick after the SPAWN crossing yielding from a slot whose pool state is `INITIALIZING`, which `x86_yield_now` does not treat as runnable (leading hypothesis, not yet confirmed by a fix).
- **The kernel's private surface.** Taoxichuan serves Akuma-private syscalls (`SPAWN`, `WAITPID`, `CLOSE_CHILD_STDIN`, `UPTIME`, `RESOLVE_HOST`, `TIME`) because sshd was reused unchanged and the boot task was never registered as a process; `libakuma`'s `linux-abi` mode (fork + pipe2 + dup3 + execve) exists but is off by default. A `linux-abi` sshd (built `--no-default-features --features akuma,libakuma/linux-abi`) authenticates over ssh and fails at the shell with `clone` returning `EPERM` from init: the missing piece is a registered init `Process`.

**The new shape.** Two separate planning sessions, in this order: `pl` (brief 09l, running: the stall; harness, then cause and fix) and `pm` (brief 09m: plan the switch of Taoxichuan to the standard process slots, scheduler and syscall families from the crates, the way amd64 composes them, deleting `child.rs`/`exec_twin.rs` machinery as it goes). The supervisor executes slices locally and uses the box only for the stall-rate and gate batteries. Stage 8 (`git clone`) waits on both; brief 09j stays as written. Docs are the shared memory: `taoxichuan/docs/` is updated by whoever changes the behaviour.

**Supervisor mistakes this stretch, for the record.** Seeded the docs folder under `docs/taoxichuan/` and moved nothing when the user moved it (the user's move reached the repo later; references were repointed); a probe script with a whole-boot verdict window; `pkill -x qemu-system-x86_64` proposed once (blocked: it could hit the user's other VMs; killed by pid/pattern after); steered glm for the banner while it was mid-turn (undeliverable, as the runbook already says); described the Mac's `disk.img`/`devbox.img` as possible guest roots before checking they were aarch64.

### Kimi's docs pass (2026-10-29): quality note, and a measurement against it

The user wrote the first `taoxichuan/docs/` pass with Kimi (via crush) and records their opinion of the result as poor ("shitty quality"), consistent with their repeated dissatisfaction recorded on 2026-10-28; the pass also stopped when its quota ran out. The supervisor's check of one file against a real run, so the opinion rests on evidence:
- `reference/rust-toolchain.md` states that the kernel serves "6 of 10 (60%)" of what `rustc hello.rs` needs. That is a count of rows in an old archive list (`RUST_TOOLCHAIN.md`, 2026-05-31) scored against `syscalls.md`, with no run behind it; its own second table scores a cargo build at 1 of 10, so 7 of 20 rows overall. The list has no threads, but `rustc 1.100.0-nightly (420ed2a0c)` makes a thread at start-up (its ctrl-c handler) and `clone` with thread flags is refused; it marks `readlinkat` served while the legacy `readlink` (89) that the run hit answers `ENOSYS`.
- Measured instead (`rustc --version` as init under QEMU, kernel `[err]`/`[census]` trace): mmap slab 64 MiB fixed against a 312,554,744-byte driver library, empty init environment (`$ORIGIN` needs `LD_LIBRARY_PATH`), thread `clone` refused, `sigaltstack`, `membarrier`, `readlink` `ENOSYS`, `mprotect` accepted and ignored; about 25 distinct syscalls before the first thread.
- The docs are kept (the structure, the diagrams and most reference pages are usable) and are to be corrected against the code by glm under the Docs rule in the brief template; the other files were not audited by the supervisor. Brief 09n corrects the headline.

### Plans pl, pm, pn and the first execution (2026-10-29 night to 2026-10-30): who decided what

For the story. Each line names the decider. "User" is the owner of the experiment, "supervisor" the Claude Code session on the Mac, "glm" the GLM planner on the ryzen box, "Kimi" the docs writer. Evidence is in the files named.

**Sessions and outcomes.**
- **`pl` (glm, brief 09l; stall).** Built a stall-rate harness, killed the preemption hypothesis (the stall reproduces with the switch off; 3 of 6 valid apk boots), added an exec-ladder instrument. The supervisor could not steer it mid-turn (the box server does not answer `session/load` during a turn), so on the user's word ("finish investigation and do a report, no fix needed") the supervisor killed the driver and ran a short wrap-up session `plw` (glm), which wrote `RESULT-pl.md` and committed the leftovers. Finding: the apk child runs after `EXE loaded` and never exits; the crossing back into the parent never resumes; nothing parks; the net pump is alive. Cause not found.
- **Supervisor's local check.** Same kernel (43ce32dd) under QEMU TCG, local mirror, `apk.static`: 10 of 10 boots clean. The stall needs the box's timing; it was not reproduced and its cause is unconfirmed. The syscall trace the probe was built for never printed.
- **`pm` (glm, brief 09m).** Plan to move Taoxichuan onto the standard process slots and scheduler (`PLAN-pm.md`, `taoxichuan/docs/reference/switch-to-crates.md`). glm found one premise of the supervisor's brief wrong (`akuma-scheduler` is a simulator, not the policy) and said the code wins. It claims the switch removes the stall at its slice S3; that is a design argument, not a test.
- **`pn` (glm, brief 09n).** Plan for the rest of the Rust syscalls (`PLAN-pn.md`, `taoxichuan/docs/reference/rust-syscalls.md`). It corrected Kimi's claim that the kernel was "60% there" (an inventory, not a measurement) with a census: rustc needs a thread at startup.

**Decisions.**
- User: the three planning sessions, their separation, and glm as planner. Stop investigating the stall, report only. (2026-10-29)
- Supervisor: ran `pn` right after `pm` without asking, reading "then start the next thing" as the agreed order; committed and pushed the 09m brief edit under the user's standing permission for this branch.
- User, answering the plans' open questions (2026-10-30): (1) `linux-abi` is switched on by a **cargo feature**; (2) keep `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)` working through `fork_process`, move it to "not implemented" if it breaks; (3) **FPU/SSE state must be saved** on a switch; (4) **init is herd** built with `libakuma/linux-abi`, with the non-Linux parts turned off by the same feature in herd if it lacks one (this replaces pm's "first boot-list program is pid 1" and its dedicated-init alternative); (5) if it works under TCG, confirm on metal in the background (the supervisor's QEMU loop is the arbiter, the box a later confirmation); (6) slab constants now, the memory model later; (7) the cargo gate may use a vendored crate for now; (8) MAP_SHARED writeback before signals, and a signals crate if one exists, else extract; (9) init before threads; (10) start `pn` S1 locally.
- Supervisor: `BRK_SLAB` is 256 MiB, not pn's 512 MiB (the brk window starts at `PIE_BASE` 0x1000_0000 and must stay under `INTERP_BASE` 0x3000_0000). Added `initenv=`/`thenenv=` boot tokens so init can carry an environment (rustc needs `LD_LIBRARY_PATH`). Corrected pn's S1 gate: `rustc --version` cannot exit 0 before the thread slice; the gate is "no loader OOM, first failure is the ctrlc thread". A/B on one image: 64 MiB slabs gave exit 127 (OOM loading `librustc_driver`), the new slabs exit 101 at the ctrlc handler. Commit 5a828e28. Source: the plan's gate was glm's, the correction is the supervisor's.
- User: "keep working autonomously, adjust the plans as you go but let me know and log who made which decision". The supervisor edits `PLAN-pm.md` and `PLAN-pn.md` in place under a dated "decisions" section and logs it here.

### pn S2 and a local finding about the stall (2026-10-30, supervisor)

Decided and done by the supervisor under the user's "keep working autonomously" (commit fcfd5f4d):
- **Legacy path fold extracted** from `amd64/src/usermode.rs` into `akuma_syscalls_abi::legacy` (ten x86-only numbers, host tests, the `symlink`-dirfd-second and `AT_REMOVEDIR`/`AT_SYMLINK_NOFOLLOW` cases pinned); amd64 and Taoxichuan both call it. This is pn's route C for the legacy numbers, done as the plan said.
- **Real `mprotect`** (re-map each present page with the requested R/W/X; `PROT_NONE` stays a no-op), `sigaltstack` (per-task record read back), `membarrier` (decode from `akuma-syscalls-mem`, one fence), and a `[nosys] nr=N` console line once per unserved number (the gap list for free). A/B on `tcc -run`: old kernel `PageFault code=0x15` (instruction fetch on an RW page), new kernel prints.
- `rustc --version` after S2: no `[nosys]` lines left; it dies only at the ctrlc handler thread (needs S4).
- **A bug the fold exposed, fixed:** the `UNLINKAT` arm read its flags from arg4; `unlinkat(dirfd, path, flags)` has them third, so `rmdir` never worked on this kernel.
- **A local finding about the stall (not the box's).** The experiment branch (preemption switch ON) hangs after `[SSH] Exec:` on EVERY ssh exec under QEMU TCG; the pl tree (switch gated OFF) never does. Cause as read from the code and confirmed by the fix: `spawn_arm` runs the child on a ring-slot number the pool still marks INITIALIZING (`reserve_slot`), the picker never takes that state, and the first tick that preempts the child switches away from a slot it cannot pick again. Fix: mark the slot RUNNING around `child_enter` and TERMINATED after. With it: 3 of 3 ssh boots, F1, PK1, `ln -s`+`readlink`, `apk add git`, `git --version` all pass with preemption ON. **This does not explain the box's stall**, which pl reproduced with the switch OFF; the two may be different bugs. Whether the box still stalls with this fix is for the metal confirmation the user asked to run in the background.
- **Another pre-existing limit, measured, not fixed:** after about 15 ssh execs in one boot, `fork` fails (`Failed to allocate x86 task stacks`: the claim picks a slot number at or above the 12-slot ring) and, without the fix above, `spawn` falls back to sshd's built-in shell. The box harness's "at most 8 execs per boot" is this limit. It dies with the ring in pm's S3.
- Who: all the above supervisor; the plan rows (S2 list) are glm's `PLAN-pn.md`; the order is the user's.

### cwd, the clone gap, and why git clone stops at a thread (2026-10-30, supervisor)

- **Working directory** (`chdir`/`fchdir`/`getcwd`, a per-task `cwd`, inherited by a fork child, reset by a fresh spawn, kept across exec) and relative-path resolution in the legacy `open`/`stat`/`lstat` arms. Before this every relative path resolved against `/`, so `cd /tmp; echo y > ../f` wrote the wrong file and `git` (which `chdir`s into the new repository) hit `[nosys] nr=80`. Gate: `cd`, `pwd`, `../` reads and writes pass in the battery.
- **`--features trace-err`** prints `[err] sN nr=X -E` for every failing syscall (slot-tagged, capped); `[nosys]` is the always-on half. These two lines are the gap finder.
- **How far `git clone` gets now, local QEMU, no TLS:** over `git://` (a host `git daemon`) the clone connects, then dies with `error: cannot create async thread: Resource temporarily unavailable` - `clone(56)` answers EPERM because only fork-shaped clones are served; git's sideband demultiplexer is a pthread. Over dumb HTTP it fetches `info/refs` and `HEAD`, runs `git-remote-http` to a listing, and the `fetch` phase faults in the helper (`cr2=0x20`, a null struct pointer in the curl slot loop, not diagnosed; a dumb-HTTP server is not what GitHub speaks, so this path is low value). So **threads (`pn` S4) are what stand between us and `git clone` and `rustc`**; the user's order (init as a Process first, `pm` S1) is being followed, though the code suggests threads inside an sshd-spawned (already registered) process may not strictly need it - to be checked before S4.
- **A second pre-existing bug seen, not fixed:** a task killed by a fault does not wake its parent's `wait4` (the clone hung after the helper faulted).
- Who: supervisor throughout; the "init before threads" order is the user's answer 9.

### Threads, futex, and the first `git clone` (2026-10-30, supervisor)

**Result: `git clone git://...` works on Taoxichuan** under the supervisor's QEMU loop (TCG, user-mode NIC, a host `git daemon`): the clone connects, fetches the pack through git's sideband thread, `index-pack` runs, the files appear, `git log` prints the commit, `git status` is clean. Not yet run on the box or over HTTPS. The same boot also passes F1, PK1, `ln -s`/`readlink`/`rmdir`, `apk add git` and a `tcc -run` program with four threads contending a mutex (`threads ok counter=4000`).

**Decision (supervisor, logged because it reorders the user's plan):** the user answered "init before threads" (question 9). The supervisor built threads first, for one reason found in the code: `clone_thread` needs the *caller* to be a registered Process, and every program an ssh session starts already is (the shell and its fork children); only the boot task (init) is not. So `git clone` and `rustc` run from a shell do not need `pm` S1. Programs run AS init (`init=.../rustc`) still do and answer EPERM to a thread `clone`; `pm` S1 stays on the list for that, for `herd` as pid 1, and for the user's Q4.

**What was built** (all in `taoxichuan/src`, commits in this and the next two entries):
- `futex.rs`: the effects half over `akuma-syscalls-sync` (op decode, `(tgid, uaddr)` keys, waiter table, deadlines), mirroring amd64's split; park on `schedule_blocking`, wake by the threading waker.
- `CLONE_THREAD` through `akuma_exec::process::clone_thread`; the bind hook gives a thread its creator's task record (`TASK_OF`), so regions, brk/mmap cursors, fds and the cwd are one copy; `exit` ends a thread (`CLONE_CHILD_CLEARTID` zeroed and woken: `pthread_join`), `exit_group` ends the group; `set_tid_address`, `gettid`.
- **One pid allocator.** `clone_thread` draws pids from `akuma_exec::process::allocate_pid`; Taoxichuan had its own counter, so a thread and a fork child got the same pid and one table row. Now the crate's counter serves both (`reserve_pid_floor(2)`). This was the first thread fault (a new thread started with an exec's entry context).
- A ring-3 fault in a real fork child or thread now kills that task alone (wait status SIGSEGV, `rc=139`) instead of halting the machine.

**Bugs found on the way, each a one-line cause (all fixed):** a socket `dup`+`close` destroyed the connection behind the other fd (git dups its socket and closes the copy; the sideband thread then read EBADF) - sockets are now closed by the last reference across all task records, and an exiting task releases its sockets (they leaked); no `/dev/urandom` (git's temp-file names); `open(O_CREAT)` in a missing directory answered EACCES instead of ENOENT, so git never created `objects/xx`; `fsync`/`fdatasync`/`sync` were ENOSYS (now `fs.sync()`); `fstat` of a pipe said "regular file", which made `index-pack` read a pipe as a file and die "pack has junk at the end" (pipes, sockets and null/random now report FIFO, socket and char device).

**Still open:** `[nosys]` after the battery: `sendfile` (40), `mount`/`umount2` (165/166, from apk's busybox trigger); `sched_getaffinity` (204); dumb-HTTP `git-remote-http` faults in its slot loop (low value); `git clone` over HTTPS needs TLS in the guest (the box). Metal confirmation of the TCG results is still to run.

### State at the end of the 2026-10-29/30 stretch (supervisor, written before the user compacts the session)

**Where things stand.** On the supervisor's QEMU/TCG loop (the arbiter of record, user's answer 5): `git clone git://` completes; `rustc --version` prints from an ssh shell; `rustc hello.rs` runs all codegen and reaches the link step; threads, futex, socketpair, real mprotect, cwd, fsync, /dev/urandom, stderr to the client all work; the old battery (F1, PK1, legacy fs numbers, `apk add git`, `git --version`) is green. Nothing is confirmed on the metal yet: glm's `pq` session (brief 09o) is running that check (stall rate with the new kernel, the old battery, three `git clone`s from a host `git daemon`); its result is `/root/dev-cycle/taoxichuan-s7/RESULT-pq.md`.

**Later in the day (supervisor):** stderr of session programs joins the ssh stream (fd 2 used to hit the serial console); polling a pipe's write end no longer panics the kernel (the readiness value of the pipe arm was returned as the poll KIND, and POLLOUT=4 is the eventfd kind: `eventfds()[usize::MAX]`); spawn argument limits raised (a `sh -c` string over 255 bytes silently fell back to sshd's built-in shell `paws`; now 4 KiB strings, 128 argv/env entries); `socketpair(AF_UNIX)` as two cross-wired pipes (`OpenFile::Pair`) because the trace-err kernel named `socketpair(1, SOCK_SEQPACKET|CLOEXEC, 0)` (nr 53) as the syscall behind rustc's `could not exec the linker cc: Function not implemented`. Whether the link then succeeds is the next result (the run was in flight when this was written).

**Decisions, by decider.** User: init is herd with `linux-abi` (Q4), keep `clone` through `fork_process`, FPU/SSE save needed, cargo feature for `linux-abi`, TCG is the arbiter and metal confirms in the background, slab constants now, vendored crate for the cargo gate, MAP_SHARED writeback before signals, "keep working autonomously, log who decided". glm: plans pl, pm, pn and the stall investigation facts; `pq` is its confirmation job. Supervisor: building threads before `pm` S1 (reasoned from `clone_thread`'s need of a registered caller), BRK_SLAB 256 MiB, the legacy-fold extraction into `akuma-syscalls-abi`, one pid allocator, socket refcounting, the preemption-hang fix, killing a faulting task instead of the machine, merging stderr into the session stream, `OpenFile::Pair`.

**Open, in the order the supervisor would take them:** the `rustc` link result and then `./hello`; `sendfile`/`mount`/`sched_getaffinity`/`prctl` arms; commit of the work after 49de01f2 (stderr, poll fix, spawn limits, socketpair, docs) - the supervisor commits it as the last act of this stretch; `pm` S1 (init as a Process, herd as pid 1); the metal result of `pq`; HTTPS clone (TLS in the guest, a real remote); writable `MAP_SHARED` for `cargo`; signals; FPU save before preemption matters for SSE-heavy guests. The "metal stall" (pl: apk child never exits, 3 of 6 boots) is still unexplained; the TCG hang fixed here is a different, deterministic bug.

### 2026-10-30 (supervisor): rustc link reaches gcc; a lesson about smaller models and probes

- **Decided by the supervisor, steered by the user:** `socketpair` ends answer `sendto`/`recvfrom`/`shutdown`/`setsockopt` (Rust's spawn error pipe); a musl `posix_spawn` clone (`CLONE_VM|CLONE_VFORK`) goes through `fork_process` entered at the child's stack, the way the amd64 kernel does (`RUST_TOOLCHAIN_AMD64.md`, row for `posix_spawn`); the fork depth ceiling is 1..8; `ioctl(FIONBIO)` sets the nonblock bit (std's `wait_with_output` died on ENOTTY); path `lstat` reports the file's real mode.
- **Where it stands:** `rustc hello.rs` now compiles, execs `cc` (gcc) from the vfork-style child, and the child loads and reaches `child_enter` but runs no syscall; the parent parks in `poll`. The same musl `posix_spawn` of the same gcc from a static C program passes (`userspace/forktest/c_stress/spawnprobe.c` built for amd64), so the defect is specific to a spawn from a *thread* of a multithreaded parent with piped stdio.
- **glm `pq` (metal confirm) is done:** battery green, `git://` transport clean on the metal, but the `apk add git` exec-crossing stall reproduces on the metal at 7 of 8 boots (pl: 3 of 6). The frozen mirror's git lacks `index-pack`, so no full clone there. The supervisor's TCG runs do not stall, so the metal rate and the TCG rate disagree.
- **Lesson for the story (the user's observation):** smaller models are too easily stopped by things like a platform label. The probes in `userspace/` (`forkprobe`, `elftest`, `forktest/c_stress/spawnprobe.c`, `dynspawn.c`, ...) are AArch64 or Linux-agnostic C and read as "not for us"; they build for amd64 in one command (`x86_64-linux-musl-gcc -static`, Rust and C, nothing outlandish). The supervisor itself nearly made the same mistake: it said the probes were aarch64 and reached for writing a new one, and the user said to just build them for amd64. Briefs for smaller models should say so: *the probes exist, rebuild them for the target, do not stop at the platform name.*

### 2026-10-30 (supervisor, autonomous on the branch per the user): rustc link, the four bugs behind "gcc hangs" and "ld: no input files"

Decided and done by the supervisor; the user's steers are marked (user).

1. **A task polling in the kernel starved every other task.** `net::sleep_until` (the loop under `poll`) halted with interrupts masked and never yielded, so rustc's forked `gcc` child sat READY forever while the parent polled its pipes (qemu idle at 1% CPU, no fault, no syscall from the child). `sleep_until` now yields to a runnable task (keeping the syscall stub's user-rsp stash across the yield), for slotted tasks only. Found by printing every syscall after the gcc exec and noticing that only the parent ever made one.
2. **The initial stack was a single 4 KiB page with argv capped at 16 entries and env at 8.** `collect2` hands `ld` ~50 arguments; `ld` saw 16 and said `no input files`. It affected every `exec` (a 60-argument `busybox echo` printed 14 words). `load_task` now takes argv/env as slices of any length and builds an 8-page block; `akuma-ring3-amd64`'s string table limit went 32 -> 512.
3. **The fd table had 16 slots** (`SLOT_CEILING`), so `ld` opening ~20 rlibs got EMFILE (`cannot find ...rlib: No file descriptors available`). Now 1024; the table is a tree, so the number is only a bound.
4. **`ioctl(FIONBIO)`** sets the nonblock bit (Rust std's `wait_with_output` died on ENOTTY), **musl `posix_spawn`'s `clone(CLONE_VM|CLONE_VFORK)`** goes through `fork_process` entered at the child's stack (the amd64 kernel's documented answer, `RUST_TOOLCHAIN_AMD64.md`), the fork depth ceiling is 1..8, **path `lstat` reports the file's real mode** (it said 0644 for everything), **socketpair ends answer `send`/`recv`**.

Also learned: `cc1` (38 MB) cannot be exec'd, `exec` reads the whole image and the read fails (ENOENT); rustc does not need it, C compilation will. The ryzen is unreachable from this session (macOS Local Network permission, the user is looking at it); everything here is TCG.

### 2026-10-30 (supervisor): MILESTONE - `rustc hello.rs -o hello && ./hello` runs on taoxichuan (TCG)

`hello from rustc on taoxichuan`, exit 0, 4.6 MB static-pie linked by Alpine's gcc driver + collect2 + GNU ld inside the guest (no host involvement), `rustc -C codegen-units=1 -C opt-level=0`. The last four defects between "ld runs" and "the binary runs", all found by the supervisor from the trace-err kernel and the console, none needing a model:

- `read`/`write`/`readv`/`writev` returned EFAULT past 1 MiB (the WRITE_LIMIT bound); ld flushes its 4.6 MB output in one `writev`, and the failure surfaced as BFD's "final link failed: No space left on device" (misleading: the disk had 340k free blocks). Now a short transfer, which POSIX allows.
- `fchmodat`/`chmod` were no-ops; ld chmods its output executable. Now real (ext2 `chmod`). `fchmod` on an fd stays a no-op (no path behind the fd).
- `execve` refused any relative path (`./hello` -> ENOENT, which busybox sh printed as "not found"). Resolved against the cwd now; the path buffer is 512 bytes, not 128.
- Debug scaffolding used to find these (per-syscall print after an exec, a pool-state dump in the idle lap, argv dumps) is NOT in the commit; `trace-err` skips ENOENT/ECHILD/ENOTTY noise now so its 3000-line cap lasts a whole link.

Known remaining gaps seen in the same run: `prctl`(157), `sched_getaffinity`(204), `prlimit64`(302) and `getrlimit`(97) answer ENOSYS (rustc and ld shrug), `getrusage`(98), `statx`/`newfstatat`(326), `sendfile`(40); `cc1` (38 MB) cannot be exec'd (the exec path reads the whole image). Next, by the user: cargo.

### 2026-10-30 (supervisor): MILESTONE - `cargo build` of a hello project runs on taoxichuan (TCG)

`cargo build` (nightly 1.100, `-j1`, offline, no dependencies) compiles and links `cg` in 19.7 s of TCG time inside the guest and `./target/debug/cg` prints `hello from cargo on taoxichuan`. The user asked for cargo as soon as rustc worked; the supervisor took it straight on. What cargo needed that rustc did not:

- **Executables over 16 MiB.** `Filesystem::read_file` refuses files over 16 MiB (its own comment: "use read_at"); cargo is 47 MB, cc1 38 MB. The exec path reads them in 1 MiB steps through the inode (`read_image`), still into one heap block because the loader wants a slice. A lazy-mapped loader would be the real answer for large binaries.
- **A growing stack.** Every image started with a fixed 64 KiB of stack and cargo's main thread went past it (`[Fault]` at rsp == cr2). A not-present fault just under the stack region now maps the pages down to the fault, up to 8 MiB (`STACK_MAX`, reported as RLIMIT_STACK).
- **`fcntl` record locks** (`F_GETLK`/`F_SETLK`/`F_SETLKW` and the OFD variants): answered granted; SQLite (cargo's global cache tracker) reported error 3850 (`IOERR_LOCK`) on the EINVAL.
- **`ftruncate` was matched under the AArch64 number** (46, which is `sendmsg` on x86_64): x86 77 was ENOSYS, so `Cargo.lock` could not be written. `truncate` (76) added too.
- **`pwrite64`** (SQLite again, error 778), **`sched_yield`**, `fchown`, **`sched_getaffinity`** (one CPU), **`getrlimit`/`prlimit64`** (unlimited, 8 MiB stack).

Still unserved and seen: `statx`/`newfstatat` (326), `sendfile` (40), `linkat` (265, cargo falls back to copying), `tkill` (200), `getrusage` (98), `prctl` (157), `copy_file_range`-style fast paths. Not yet tried: a build with dependencies (needs a vendored crate, user answer 7), `cargo build -j4`, HTTPS fetch from crates.io.

**State when the supervisor session was handed over (2026-10-30, still TCG):** the first `cargo build` of a hello project works, but a second cargo invocation in the same boot dies of exhaustion (`fork` EAGAIN: `Failed to allocate x86 task stacks` / `Failed to create address space` / `user-half clone failed`, then `memory allocation failed`). Cause not yet confirmed: nothing in taoxichuan frees an exited task's user address space, kernel stacks or ring slot (`rustc` maps the 312 MB `librustc_driver.so` by copying, so a handful of leaked runs eat the 3 GB). `exec_real_task` itself leaked the executable image and argv until it dropped them before the crossing (fixed). Next: measure `akuma_pmm::free_count()` across an exit, free the address space on `wait4` reap, then rerun build/run/release in one boot.

### 2026-10-30 (supervisor, autonomous on the branch per the user): MILESTONE - `cargo build; cargo run; cargo build --release` pass in ONE boot

The handover suspected "exited tasks never free their address space, kernel stacks or ring slot". Measured, it was three separate leaks plus one capacity limit, none of them where the suspicion pointed. Instrument: `akuma_pmm::free_count()` printed at each reap and at each spawn-crossing return (`[reap]`/`[spawn-reap]`, `trace-err` builds only), run per command on the TCG rig.

1. **The twin owns every page-table root, and nothing freed them.** `wait4` only set `reaped`; the registered `Process` held a `new_shared` view (drop frees nothing) of a root the loader built by hand with raw `alloc_page_zeroed` + `map_page_in`. `rustc hello.rs` kept ~107 000 pages (420 MB) per run. `free_user_root` (main.rs) now walks a root's lower half (leaf frames through `free_page`, so a CoW count still drops; table frames; the root), and the reap, the spawn crossing's return and each re-exec park roots for it (`OWNED_ROOT`, `PENDING_ROOT`, child.rs).
2. **The eager fork clone is untracked.** `clone_user_half` fills the crate's fresh `UserAddressSpace` with raw frames its ledger never saw, so dropping the owner returned only the L0: a forked 100k-page `rustc` left 104k pages per fork (the cc spawn). `disown_space` swaps in a shared view and `forget`s the owned space (leaks its small ledger bookkeeping, not frames); the twin frees the whole root. The PMM `drain_retired` hook was a `|| 0` stub; it now drains retired `Process` rows under pressure, and each reap retires + drains.
3. **A spawn-crossing slot stayed `ON_CPU` forever.** `spawn_arm` marks the ring slot TERMINATED when the child returns, but `ON_CPU` is only lowered by the next switch's tail, which never comes; `slot_reusable` refuses it, so every ssh command burned a ring slot and `x86_claim_slot` ran dry after ~6 commands (`Failed to allocate x86 task stacks` is the *prepare_task_slot* refusal for a slot >= NSLOTS, i.e. no claimable slot). New `akuma_threading::x86_release_on_cpu`, called right after the crossing returns.
4. **NSLOTS 12 -> 48.** Real concurrency: cargo + rustc + rustc's worker threads + the shell filled 11 slots at the `ld` spawn (`collect2: posix_spawnp: Resource temporarily unavailable`). The hand-written 12/16-entry array literals became const repeats; `PARENT_OF` 32 -> 256 (it is keyed `pid % len`). Idle tid is NSLOTS = 48 < MAX_THREADS 64.

Result on TCG, one boot: `cargo build` 7.5 s, `cargo run`, `cargo build --release` 3.6 s, zero `[forkp] refused`; also gcc x3 + rustc x3 + a pipeline with steady free memory (399 pages per command is the sshd-side shell image, returned at the crossing). Regression battery (F1, PK1, fs, apk add git, git --version) identical to the pre-change run.

Left open: the ledger bookkeeping of a disowned fork-child `Process` leaks a little heap (a few hundred bytes per fork-exec; the heap read 2.5-4 MB across the runs); `exec_replace`/failed-load paths still leak a half-built root on a load failure; vendored dependencies, `-j4`, HTTPS clone are next.

### 2026-10-30 (supervisor, autonomous on the branch per the user): MILESTONE - a vendored serde/serde_json/fastrand project builds with `cargo build -j1` AND `-j4` (Pop OS, KVM)

**Rig.** `scripts/kvm/taoxichuan_pop.sh` (user's idea, "just use the partition Akuma uses today"): on the ryzen booted into Pop OS it builds the kernel natively from the pushed branch (11 s) and boots it under KVM with `/dev/nvme0n1p3` - the same ext2 root the metal kernel boots, with sshd, gcc and the 981 MB nightly toolchain already on it - attached through a **persistent qcow2 overlay** (p3 is never written; the overlay keeps a staged project across kernel rebuilds). The guest is reached from the Mac with the normal key at `192.168.1.126:2224`. The vendored project (7 MB) is served from the ryzen over HTTP because the guest's sshd ignores stdin and HTTPS to crates.io is the next milestone. TCG is ~30x slower and starved sshd's keepalive; KVM turned a 30-minute experiment into ~20 s.

**What the vendored build found (nine defects; the user's rule - grep `docs/archive` for every symptom - found the cause of four at once):**
1. `open` used a 128-byte path buffer (`PATH_BUF` is 1 KiB now): cargo's build-script object paths are ~150 bytes, EFAULT, "could not write output ... Bad address".
2. `rt_sigaction`/`rt_sigprocmask` were pure no-ops that never wrote `oldact`/`oldset`. musl's `signal()` returns the old handler read back from the stack, so std's child-side `signal(SIGPIPE, SIG_DFL)` saw `SIG_ERR` whenever the garbage happened to be -1 and `Command::spawn` failed with a *stale* errno ("File exists" - the parent's last EEXIST `mkdir`). Found with a syscall ring that showed the child doing dup2/chdir/sigaction and then writing 8 bytes to the CLOEXEC pipe without ever calling `execve`. Real per-task handler table and mask now.
3. The mmap area was a 1 GiB bump allocator with a no-op `munmap`; a big `rustc` compile ran out of address space (ENOMEM) with 300k pages free. `munmap`/`mremap`/`MAP_FIXED` now free frames and reuse holes (first-fit hole table).
4. `readlink("/proc/self/exe")` answered EINVAL: rust-lld's wrapper (`current_exe`) and musl's `$ORIGIN` need it (the caveat is in `docs/archive/RUST_TOOLCHAIN_AMD64.md`). Per-task `exe` path.
5. `rust-lld` writes its output through a writable `MAP_SHARED` file mapping (the archive's `AKUMA_SELF_HOSTING.md`): the file stayed empty, cargo then got `Exec format error` running the linked build script. Writable shared file mappings are eager and written back on `munmap`, `msync`, exec and exit.
6. After a failed build cargo **hung forever**: the jobserver crate `tkill`s (SIGUSR1) its helper thread out of a blocking `poll`/`read` and joins it; `tkill`(200) was ENOSYS. `tkill`/`tgkill` now set the thread's interrupt bit; `poll` and pipe `read` return EINTR.
7. **No x87/SSE state was saved at context switches.** The twin's `switch_to` ignored `from` and never ran `fxsave`/`fxrstor`, so any preemption or park handed the outgoing task's vector registers to the next one. Symptoms were rustc ICEs that looked like memory corruption (an index spelling "loss", a musl `free()` of a misaligned pointer, "panic during codegen/LLVM phase") and only in multi-threaded compiles - it had been mistaken for a bug in the new mmap code for hours. A bisect knob (`mmdbg=N` on the kernel command line) showed the mmap rework was innocent. Per-slot 512-byte areas, power-on state `FCW 0x37F / MXCSR 0x1F80`, reset on slot reuse.
8. `wait4` matched a child's parent by the *calling thread's* pid, so a child forked by one cargo job thread and waited on by another got ECHILD: parentage is the thread group (`Process::tgid`).
9. An exited process's ring slot was recycled by the next fork before its parent had waited, erasing the zombie's status (again ECHILD, `-j4` only): `hold_exited_thread_slot`, released by the reap's `unregister_process`.

**Result.** `cargo build --offline -j1` and `-j4` of the vendored project (proc-macro2, syn, serde_derive, serde, serde_json, fastrand, itoa, memchr, zmij) build and run: `vendored ok: {"name":"taoxichuan","n":1} -> P { ... }`, 2 m 45 s on one vCPU under KVM (`-j4` buys nothing on one core, it proves the concurrency). The one-boot cargo trio and the battery are unchanged on TCG. `cargo clippy` on the kernel is clean with and without `trace-err`.

**Diagnostics added to `trace-err` (not in the shipped kernel):** per-syscall-number cap on `[err]` lines (a single global cap let futex ETIMEDOUT and readlink EINVAL use the whole budget before the failing call), a 1024-entry syscall ring dumped on the child's `NOEX` error-pipe write or on `tkill`, `[stderr]` echo of what tasks write to fd 2, an mmap-event ring dumped on a user fault.

**Left open:** HTTPS (`git clone https://...`, crates.io) needs TLS/CA certs in the guest; `[nosys]` still lists statx/newfstatat 326, sendfile 40, linkat 265, getrusage 98, prctl 157, mount 165/166; a lazy-mapped loader for executables over 16 MiB; the hole allocator and the sigaction table are pure logic that should move into crates with host tests (`akuma-mmap`, a small `akuma-syscalls-signal`) once stable.

### 2026-10-30 (supervisor, autonomous): MILESTONE - HTTPS `git clone` and `cargo build -j4` straight from crates.io (Pop OS, KVM)

`git clone https://github.com/netoneko/akuma-playground.git` works in the guest, and a project with `serde`, `serde_json`, `fastrand` from **crates.io** (index update, downloads, proc-macros, no vendoring) builds and runs under `cargo build -j4` in 2 m 46 s (one vCPU, 8 GiB guest). Each defect below was found by reading the console, not by guessing, and four of them were already in `docs/archive/` (the user's rule: grep the archive for every symptom).

1. **`certificate is not yet valid`** - the guest believed it was 1970. The rig passes `epoch=$(date +%s)` on the command line now (an RTC/SNTP read is still to do).
2. **`recvmsg` (47) and `sendmsg` (46) were unserved.** cargo's libcurl uses c-ares, which receives its DNS replies with `recvmsg`; "Could not resolve host" in cargo while the musl-resolver `curl` worked (`docs/archive/AKUMA_AMD64_DNS_CONNECTED_UDP.md` is the c-ares checklist). Minimal versions over `recvfrom_arm`/`sendto_arm`: a header, a name, iovecs, no control data.
3. **`O_NONBLOCK` was ignored on pipes and socketpairs.** A read of an empty nonblocking socketpair (c-ares' / curl's wakeup pair) parked forever; found by recording the syscall each parked thread is *inside* (the `[hang]` dump). `EAGAIN` now.
4. **`sendfile` (40) / `copy_file_range` (326)** for file-to-file and file-to-pipe, **`link`/`linkat`** through the VFS (it was a stub that said "no hardlink arm yet"; the VFS has one).
5. **`stat`/`lstat` reported `st_ino = 0` for every file**, so busybox `cp -r` (which remembers `(dev, ino)` of what it has copied) hard-linked `Cargo.toml` onto `main.rs`. The inode comes from `resolve_inode` now (ext2's `metadata()` leaves it 0).

Tooling that earned its keep: a **hang dump** in `trace-err` (after 20 s of idle: every live slot's in-flight syscall, last syscalls and the global futex order), a per-syscall-number cap on the `[err]` trace, the `mmdbg=` and `TX_APPEND` knobs, and the KVM rig's persistent overlay (a project staged once survives kernel rebuilds).

**What is not done / known:** fork still copies the whole address space eagerly (no CoW), which is why `-j4` needs the 8 GiB guest (`-m 3072` runs out of memory with four rustc + rust-lld at once); one vCPU, so `-j4` shows concurrency, not speed; the guest clock comes from the command line; remaining `[nosys]`: getrusage 98, prctl 157, mount/umount2 165/166, preadv2 328; the hole allocator and the sigaction table should become crates with host tests; a lazy-mapped loader for executables over 16 MiB; the HTTP-served tarball is only needed for the vendored variant now.

## 2026-10-30: copy-on-write fork (decided: user asked; planned by goose-glm on the Mac, implemented by the supervisor)
glm planned (`TAOXICHUAN_COW_FORK_PLAN_GLM.md`, 21 tool calls); implementation and gate in `TAOXICHUAN_COW_FORK.md`. Leaf frames are shared at
fork, writable ones demoted RO + PTE bit 9, one break path (`cow_resolve`) for the write fault, `copy_to_user` and `mprotect`.
`scripts/kvm/goose/cow_leak_gate.py`: 60 fork rounds, free pages back to baseline exactly (654000), `cowrefs=0`. Battery and the one-boot cargo trio green on TCG.
Not yet run on the ryzen: `-j4` in a 3 GB guest.

### 2026-10-30: two more glm plans (user asked; planned by goose-glm on the Mac, unreviewed by the user yet)
`TAOXICHUAN_EFFICIENCY_PLAN_GLM.md`: lessons from amd64/AArch64 - top slices: demand-paged mmap, then the fpcache for RO file pages (both aimed at -j4 in a 3 GB guest), syscall-floor audit, read(2) stage profiler, wake budget/parked netpoll.
`TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md`: shared crates - `akuma-mmap::holes`, `akuma-syscalls-signal`, `akuma-user-tree` (CoW share/free plans), sockaddr marshalling into `akuma-syscalls-net`, `akuma-fpu-amd64`; child table/dispatcher left to pm S1-S7.

### 2026-10-30 (evening): extraction slices 1-2 done, unsafe-reduction plan
`akuma_mmap::HoleSet` (fe29eb00) and `akuma-syscalls-signal` (dae17630) landed, both verified on ryzen/KVM (`-j4`, 3 GiB, 3m06-3m09) and the TCG battery.
`TAOXICHUAN_UNSAFE_REDUCTION_PLAN_GLM.md` (user asked; glm, 26 tool calls): ~131 real sites in 7 classes; first finding - `scripts/check_unsafe_budget.py` does not
scan `taoxichuan/src`, so the gate is blind there until its slice 0/1 adds the path. Proposed `akuma-per-slot` (borrow-word cells for the `static mut` tables, ~71 sites),
`TableFrame`/`Frame` views in `akuma-boot-amd64::phys` (~21 + 2 sites), adoption of existing `akuma-cpu`/`akuma-port` wrappers for the safe-to-execute asm.

### 2026-10-30 (supervisor): GLM tokenomics from the goose ledger

**Source.** goose's `sessions.db` on the Akuma root (`/dev/nvme0n1p3`), copied from a read-only mount (separate mountpoint,
unmounted at once; a QEMU guest was reading p3 through its overlay at the time) to `/root/linux-audit/goose-sessions.db` on
the ryzen's Pop side. Its `usage_ledger` table records every model call: input, cache-read and output tokens per model. It
agrees with the z.ai usage API's daily totals to within 0.1M (10-28: 124.1M ledger, 124.0M API), so it is the whole of the
experiment's GLM use. The 115M z.ai records on 10-20/21 are not in it (another machine). Credits use z.ai's published formula
(`docs.z.ai/devpack/overview`): Flash (fresh input x 2.3 + cached x 0.56 + output x 8) / 10,000, GLM-5.3 (6.9, 1.7, 24), half
rate outside Mon-Fri 06:00-10:00 UTC. API prices from `docs.z.ai/guides/overview/pricing`: Flash $0.15 / $0.03 cached / $0.50
output per million, GLM-5.3 $1.4 / $0.26 / $4.4.

| UTC day | calls | tokens | credits (formula) | API $ |
|---|---:|---:|---:|---:|
| 10-16 | 96 | 5.5M | 180 | 0.20 |
| 10-23 | 79 | 4.1M | 144 | 0.16 |
| 10-24 | 1,002 | 96.1M | 4,053 | 7.62 |
| 10-25 | 1,144 | 106.4M | 3,335 | 3.65 |
| 10-26 | 1,750 | 116.3M | 3,775 | 4.17 |
| 10-27 | 1,116 | 57.4M | 2,591 | 2.51 |
| 10-28 | 1,137 | 125.0M | 3,996 | 4.40 |
| 10-29 | 634 | 57.1M | 2,110 | 2.13 |
| 10-30 | 31 | 1.5M | 67 | 0.08 |
| **total** | **6,989** | **569.4M** | **20,251** | **24.91** |

- **Mix:** 98.3% of input tokens were cache reads; output 0.4-1.8% of tokens per day. 222 of 6,989 calls were in peak hours.
- **The quota charges more than the formula.** The formula gives 35.6 credits per million tokens for this mix and timing. The
  quota readings in this log (95.4M = 49%, 130.1M = 64%, and 23.0M = 12% on 10-30) give ~50 credits per million, about
  200M Flash tokens per Lite week (10,000 credits). The user's account of the week agrees with the readings, not the formula:
  the experiment used **3 weekly resets** of the Lite plan, and 569.4M x 50 / 10,000 = 2.85 weeks. Partly explained (user's
  point): GLM-5.3's day-one 16.1M tokens cost ~1,540 credits (off-peak Saturday), ~1,000 more than the same tokens on Flash
  (~0.1 week, already inside the formula's 20,251); 10-20/21's 115M Flash tokens are not in this ledger and add ~4,100
  credits (~0.4 week) if they fell inside the reset weeks - ~2.4 weeks by the formula against 3. Not all of it: the three
  readings above were taken in Flash-only windows and still give ~50 credits per million against ~36; the off-peak discount
  not applying as documented would account for most of that.
- **Refined-process milestones** (sessions from 10-26, grouped by milestone from the ledger's session names and times):

| milestone | tokens | API $ |
|---|---:|---:|
| plans N / pm / pn | 1.6 / 2.4 / 2.0M | 0.07 / 0.10 / 0.09 |
| pipes (pa) | 4.8M | 0.20 |
| `apk update` (L) | 6.3M | 0.26 |
| `apk add curl` (M) | 7.0M | 0.29 |
| wire dedup (K) | 7.5M | 0.27 |
| M0 mirror + M1 wall clock (H) | 11.2M | 0.40 |
| blocking fork + `wait4` (M2, 21 short sessions) | 20.4M | 0.94 |
| the stall, report only (pl) | 23.3M | 0.88 |
| dynamic PIE (O, O2) | 28.3M | 0.97 |
| DNS (I, J) | 31.3M | 1.06 |
| curl over HTTPS (Q-R, 13 sessions) | 46.4M | 1.84 |
| M4 preemption on one core (pb-ph, stopped by the user) | 159.1M | 5.65 |

  Median execution milestone about 16M tokens, $0.60, 8% of a Lite week. M4 alone is 80% of a Lite week and more than the ten
  refined milestones above it together (186M).
- **Pacing.** The refined days ran at 57-125M tokens a day, so a Lite week (~200M) lasted two to three days; hence the resets.
  One Lite month is ~866M tokens. A median-cost replay of the whole ladder (25-30 milestones, ~400-480M) fits in it at ~28M a
  day, one 5-hour window daily; a worst-case one (~40M per milestone, 1.0-1.2B) does not. On the API the median replay is
  about $15-20.
- **Not in these numbers:** the Claude supervisor's tokens (briefs, steering, console verification), and since the 10-29 switch
  Claude executing the rustc, cargo, git clone and CoW-fork milestones. No GLM cost exists for that class of milestone.

**The Claude side (same day, from the Claude Code transcripts in `~/.claude-personal`, usage fields only; streamed chunks
deduped by message id).** Supervisor tokens per UTC day, 10-24 -> 10-30: 160M, 84M, 211M, 198M, 116M, 112M, 340M; total
**1,221M against GLM's 569M**, more than GLM on every day (cache reads 98-99%). The execution stretch after the 10-29 switch
(18:00Z -> 10-30, 20 active hours, ~12 milestones, 32 commits): 1,380 model calls, 1,571 tool calls, 406M tokens, ~294k per
call. Extrapolated to GLM doing the same work at 2-4x the tool calls and the refined process's 56-77k tokens per call:
176-242M (2x), 264-363M (3x), 352-484M (4x), ~630M if the slowdown comes as long sessions; 0.9-3.1 Lite weeks, $6-23 on the
API. An extrapolation, not a measurement: no GLM run exists for this class of milestone.

**What the supervisor's tokens bought (same day).** Every Claude call from 10-24 to 10-30 classified by its tool calls
(keyword rules on tool names and shell commands; edits made through shell heredocs counted as edits; good to a few points):

| | supervising goose (10-24 -> 10-29 18:00Z) | executing (10-29 18:00Z -> 10-30) |
|---|---:|---:|
| calls / tokens | 3,471 / 816M | 1,380 / 406M |
| polling: text-only "nothing changed" reports + sleep/Monitor | **38%** (1,313 calls, 312M) | 9% (boot waits) |
| supervision: goose logs, steering, RESULT/BRIEF, merges, usage checks | 22% | 2% |
| briefs and docs written, docs and plans read | 13% | 9% |
| verification on the box, git | 9% | 5% |
| Claude's own coding: read source, edit, build/test | **16%** | **67%** (76% with boot waits) |

The 1,083 text-only turns are the 5-minute polls, each re-sending ~237k tokens of context; they total 257M, 45% of GLM's
whole-experiment spend. Waking on the driver's exit instead (`scripts/kvm/goose/supervise-loop.sh`, one wake per goose
session, ~150) would have saved roughly 200M. Per milestone: ~50M Claude tokens per supervised GLM milestone (10-26 ->
10-29) against ~34M per Claude-executed one.

**EXTRAPOLATION (same day; not a measurement): the project on GLM alone, supervision not counted.** The user does not count
the supervision tokens: with a proper process (event-driven waiting, briefs that hold) supervision is unnecessary or much
cheaper. Claude's coding model calls from the classification above - executing stretch 1,056 (read source, edit, build/test,
boot waits) or 1,207 with the commits and doc updates the process requires, plus 589 coding calls during the supervision era -
are 1,645-1,796. At the user's assumed 2-4x as many calls for GLM, and the refined process's 56-77k tokens per call:

| GLM slower by | GLM calls | GLM tokens | + GLM's actual 569M | Lite weeks | API $ (total) |
|---|---:|---:|---:|---:|---:|
| 2x | 3,290-3,592 | 184-277M | 753-846M | 3.8-4.2 | 32-35 |
| 3x | 4,935-5,388 | 276-415M | 845-984M | 4.2-4.9 | 35-40 |
| 4x | 6,580-7,184 | 368-553M | 937M-1.12B | 4.7-5.6 | 38-45 |

Cross-check: Claude's ~12 executed milestones at GLM's refined median (16M) give ~192M, inside the 2x row. One Lite month is
~866M, so at 2x the whole project so far fits in about one $18 month of GLM quota, learning phase included.
**Why it stays an extrapolation:** no GLM run exists for the rustc / cargo / threads / CoW-fork class of milestone; the 2-4x
factor is assumed; the refined process was itself Claude-supervised; the Lite quota's ~50 credits per million (against the
formula's ~36) is unexplained. **User's assessment:** the experiment was flawed and ultimately inconclusive, but the initial
claim - an open model can build your own stack in a short time for little money - still looks plausible on these numbers.
