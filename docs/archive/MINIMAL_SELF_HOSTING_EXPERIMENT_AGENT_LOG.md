# Minimal self-hosting kernel "Taoxichuan" — agent log

**Status: in progress. Started 2026-10-24. Stages 1-4 PASS; stage 6 (offline `apk add`) PASS on metal (G1-G4, 2026-10-25 05:35 IDT) and on the aarch64 host track.**

Running log for the two-agent execution of
[`MINIMAL_SELF_HOSTING_EXPERIMENT.md`](MINIMAL_SELF_HOSTING_EXPERIMENT.md).
The minimal kernel being composed is named **Taoxichuan**.

**Oversight change (2026-10-24, afternoon).** The host track and the babysitting
of goose-glm were first run by Kimi (Crush, `k3-256k`). Kimi ran out of tokens
because of a bad subscription setting on the owner's side — not a model failure.
Oversight moved to **Claude Sonnet 5 (Claude Code)**, which takes over the host
track and supervises goose. The metal track is unchanged: still goose + GLM on
the ryzen. The tracks below still say "Kimi/Crush" for what was done before the
handover; entries after it are marked *(Sonnet)*. Note for the open-weights
claim: the host track is no longer open-weights from this point; the metal
track still is.

**Owner's note (2026-10-24, evening): the tooling was badly configured, and the owner is
frustrated about it.** The owner says that is their own fault. Recorded here because it
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

**Owner's note on the banner (2026-10-24, evening).** The owner was not a fan of goose-glm's
skyline art (`511f4fdb`; the words stay, the owner approved those). They went to other
models for a redraw and report **deep disappointment with Mistral** and **mild disappointment
with Gemini**. The new banner was made by Gemini; after many failed attempts to get Gemini
to adjust it, the owner gave up and adjusted it by hand, and committed the result
(`419fb968`, "better banner for taoxi kernel"). Recorded as the owner's opinion of those
models on this art task, outside the two tracks of the experiment; it says nothing about
either model's coding.

**Oversight change #2 (2026-10-24, 18:20-18:30 IDT): Kimi (goose on the Mac) ran out of quota;
Sonnet supervises again** *(Sonnet)*. Kimi's two supervisor stints, for the record:

- *Crush, `k3-256k`, ~16:00-16:57:* took 26% of its 5-hour quota in about 23 minutes, hung on empty
  turns, and spent turns debugging a key that the Sonnet smoke test had swapped (see the tooling note).
  Closed by the owner.
- *goose, `k3-256k` low effort, session `20261024_3`, 17:04-~18:25:* **how far it got.** It merged
  goose-glm's finished stage 1 (`bd59f4aa`), ran the aarch64 check of the merged tree (build 662,792 B,
  Firecracker boot, zero faults, `--no-disk` control; `9418bdf1`, PASS), wrote and pushed the stage 2 brief
  (`d59f0df1`), sent it to goose-glm, and committed the tooling edits (`c87730e1`). Model time was about 22
  minutes: 1.30M accumulated input, 19.8k output tokens (`sessions.db`, Mac goose).
- **Failure modes.** (1) It ended its turn right after sending stage 2 ("I'll validate when the turn ends"),
  twice, which stops the loop because nothing re-prompts a turn-based agent; fixed by moving the waiting
  into `scripts/kvm/goose/supervise-loop.sh`, which prompts it only when goose-glm's driver exits. (2) The
  owner's instruction to send the next stage to goose-glm *before* validating the last one (so goose is
  never idle) arrived after stage 2 had already gone out, so it was never exercised. (3) Quota ran out
  (reported by the owner, ~18:23) before the stage-2 close; our logs show no 429 because Kimi was not
  prompted again. The loop was stopped so it would not burn a prompt on the error. The owner, annoyed by
  the run of failures, then **unsubscribed from Kimi**; Kimi does not come back in this experiment, and
  Sonnet is the supervisor from here (the host track is no longer open-weights from this point, as
  already noted for the first handover).
- **The pull side is unreliable, and the owner knows:** Kimi learns what goose did by polling a log file and
  reading `RESULT.md` over ssh; a machine-readable status per stage, a detached driver launcher and a
  proper wait/notify channel between the two agents are the fix, to be done later.
- **State at handover:** stage 1 passes on both tracks. Stage 2 is running on goose-glm (branch
  `taoxichuan-s2`, `92e9572b` pushed: the `akuma-ring3-amd64` crate; ~480 lines of uncommitted
  composition in `taoxichuan/src/main.rs` on the box; 11.7M input / 107k output tokens in the first
  hour on `glm-5.3-flash`, and the owner reads the z.ai quota meter as clearly cheaper than the
  `glm-5.3` stage-1 run). Sonnet closes stage 2 when the driver exits: read `RESULT.md`, send the stage 3
  brief first, then the aarch64 check (plus `cargo check -p` of the new crate on the Mac: its
  `naked_asm!` is not `cfg`-gated for x86_64).

Working rules carried over from the owner's last Crush session (the "wisdom"):

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

**"The fd table's answer is special-cased to the three stdio fds" - the fd nonsense.** Owner's note, 2026-10-25: this class has
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
whole-docs trace the owner asked for (every mention, one table).

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

#### Owner approval of the banner, and the next goose prompt (Sonnet, 2026-10-24)

- **Owner approval:** on seeing goose's Taoxichuan banner and boot text, the owner:
  "yeah that's a nice vibe it got going there". The words stay. Direction for the art:
  keep the text, improve the ASCII picture into an old-timey Chinese skyline as it was
  for the emperor's kilns in Jingdezhen (JDZ). The banner meaning ("the kiln-fire
  market") and the first UTF-8 art were goose's own invention; the owner's approval is
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
  the owner's note) ran to completion with no harm — its re-verification section is
  its output. The supervisor now sends only when no `goose_acp_prompt` driver is
  alive.

**Stage 1 counts: both tracks pass the gate.** Next: stage 2 (userspace works — a
static musl hello runs, exit 0, output matches the host-built binary), brief
`scripts/kvm/goose/taoxichuan-02-hello.md`.

### Stage 2 — host track (Sonnet), aarch64 / Firecracker in Lima `fc` — PASS after one supervisor fix

*2026-10-24 ~18:50 IDT, Sonnet (Kimi was gone; see Oversight change #2).* Merged `litter/taoxichuan-s2` @
`9a948c78` into `rust-org-il-experiment` (`ed1de19d`, clean) while goose-glm was already working on stage 3
(the next brief goes out before the aarch64 check, per the owner's ordering).

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
  did not hit the limit: the owner reads the z.ai quota meter as clearly cheaper per token on flash.
  Stage 2 start to finish took about 90 min, ~30 of them reading before the first edit.
- **Stage 3 started 18:48 IDT** (`taoxichuan-03-busybox.md`, narrowed by the owner to non-interactive
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

*2026-10-24 ~20:00 IDT, Sonnet.* Gate narrowed by the owner: non-interactive `busybox echo` and `busybox ls`, no `ash`, no pty
(brief `scripts/kvm/goose/taoxichuan-03-busybox.md`, sent 18:48 before the stage-2 aarch64 check, per the owner's ordering).

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
  stage 2, stage 3) are about 38M input tokens; the owner read the z.ai meter at 90% at ~19:46 IDT, so the window is
  roughly 40M input tokens at flash weight (an estimate from one data point).
- **Also this stage:** the owner asked that every brief tell goose to commit and push every 20 minutes when something is
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
  HTTP 202, turn ended `stopReason: cancelled` at 23:30:49 and the driver exited 0. Owner's call: 72% of the
  window was spent and the remaining quota was better used on one named failure.
- 23:30-23:33 supervisor: restored `vm-serve-mc` and disabled `vm-dev`, committed goose's uncommitted files on the
  box as `e418fd61` (owner authorised it; no push), stopped and started `goose-glm` (new pid), wrote and sent
  `taoxichuan-04b-continue.md` (about 25-30 min of work; commit and push every 10 min; PARTIAL result if not
  done).

**Quota, measured from `sessions.db` (`scripts/kvm/goose_box_stats.py`, run through ssh as base64).** The
window is about 41M input tokens (stage 4 plus 03b were 27.57M at 23:25 = 67%, the owner read 69%). Stage 4's burn
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
  within the owner's patience, and it would have exercised the full kernel's sshd, not `net.rs`/`child.rs`.
  Left as is; the real test is the amd64 guest.

**Side effects the next supervisor should know.** (1) To back up goose's work I ran `git add -N taoxichuan crates` on
the box, which marked new files intent-to-add in the index (contents unchanged). Backups are in the supervisor's
scratchpad, not in the repo. (2) `guest-setup.sh` created `tap0`, NAT and the 4444 forward inside the Lima VM `fc`.
(3) Merged `litter/taoxichuan-s4` into `rust-org-il-experiment` locally (`78bdaf10`) mid-stage at the owner's request.

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

**Supervisor verification.** (a) From the Mac, a plain `ssh -p 2224 root@192.168.1.159 '<cmd>'` with the owner's key
succeeded on the first attempt after `herd start vm-dev`, so the relay is reachable from the LAN, not only
from the box. (b) aarch64: merged `litter/taoxichuan-s4` as `16f79ff9`; the Taoxichuan-feature kernel is still **662,792 B**;
`cargo test -p akuma-ring3-amd64` 8 passed; Firecracker boot with the disk is 132 console lines, 0
`Fault|PANIC|sig!`, "Ext2 filesystem mounted", "sshd started"; the `--no-disk` control is 80 lines with
"Block device not found" and no sshd (it proves the first two lines come from the disk). The aarch64 kernel does not
contain stage 4's `net.rs`/`child.rs`, so this shows no regression, not that stage 4 works there.

**Open items found while using the guest (not gate failures).** `ls /bin` over ssh prints "ls: cannot access
/bin" (stat/getdents on that path); the console prints `[sc] unhandled nr 4415` repeatedly (an unimplemented
syscall number nobody has identified); `uname` answers `Akuma 0.1.0 Akuma-OS x86_64` for `-a` and plain `Akuma`
for `-r` and `-m` (hardcoded in sshd/paws, per the owner; not a kernel matter); sshd tolerates `unhandled nr 258` (`set_robust_list`) and `nr 5`. Goose's stage-6 list: outbound
relay (`vm_outbound`) and DNS, `writev`/`readlink` class arms, a wall clock (`utc_seconds` is `None`), untested 1 MiB
mmap/brk slabs.

**Cost.** Stage 4 part 1 (`20261024_31`): 80 min, 163 calls, 26.4M input at the last full reading (23:25); part 2 (`20261024_34`,
after the stop/start): **19.1 min, 59 calls, 2.02M input, 28k output, largest context 59k.** The window (~41M input
tokens) was about 76% spent at the end. The restart paid for itself: a fresh 59k context cost a tenth of the
220k-context session per call. The `goose serve` idle spin did not reappear in this ~20-minute session
(`GOOSE_SERVE_IDLE_SPIN.md`).

**State left on the box.** `vm-dev` is running at the owner's request so they can ssh in (`vm-serve-mc` halted); restore with
`herd stop vm-dev; herd disable vm-dev; herd start vm-serve-mc`. The next stage needs a quota refill (reset ~02:39 IDT) and a stage-5
brief; none is written.

### Stage 6 — planning and session A (offline apk), supervisor view (Sonnet), 2026-10-25 00:02-01:05 IDT

**Plan.** goose-glm (planning only, 15 min, ~3% of a window) wrote `TAOXICHUAN_STAGE6_PLAN.md` (`f61ca77e`): ~32 missing
core syscall arms in three phases, `nr 4415` identified as the private `UPTIME` (`0x1000 + 319`; verified), outbound
via `vm_outbound = true` and a host-local HTTP mirror, the clock from `epoch=` on the kernel command line, milestones
M0-M7, 2-3 sessions. Supervisor concern recorded: the plan's host reference ran `apk.static` on the Akuma box itself, so a
bug could be shared by reference and subject; the supervisor computes the reference on the Mac with plain `tar`.

**Staging (supervisor).** Because the quota left (~20%) would be lost at the 02:44 reset, the owner asked for a no-network
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

**Fans.** The owner's fans were the `vm-dev` guest, not goose: the Taoxichuan kernel's sleep loop polls the NIC and never halts
its vCPU, so core 4 sat at 100% (`guest=99.8%` in the host's `[KVM] cpu:` line) while `vm-serve-mc`'s full kernel idles at ~7%.
Rule: stop `vm-dev` when a session ends. A `hlt`/idle path in the guest's wait loop is a real follow-up (needs a wake source:
kvm0 raises no NIC interrupt, so it needs a timer exit). `goose serve` did not spin after the stop/start restarts
(`GOOSE_SERVE_IDLE_SPIN.md`).

**Quota (from `sessions.db`).** Session A (`20261024_35`-ish) used the last ~18% of the window in ~31 min (full read
unavailable after the stop). Window arithmetic: ~41M input tokens per 5 h; stage 4 part 1 burned it at ~60%/h with a 220k
context, part 2 and the planning and 6A sessions at ~10-35%/h with fresh contexts.

### Stage 6 — session B (`taoxichuan-06b-spawn-then-apk.md`), supervisor view (Sonnet), 2026-10-25 02:31-03:40 IDT (in progress)

**Send.** Quota reset confirmed by a probe (`pong`) at ~02:30 after a `herd stop`/`herd start` of goose-glm; brief sent 02:31
(`--name s6b`, log `/tmp/goose-s6b.log`). A delayed sender scripted for 02:45 was killed: the owner said the window had
already reset (the 02:44 figure was the *previous* window's end; the quota meter had been reset by the owner's own session
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
transcript above as the oracle. Ryzen was taken offline by the owner at ~03:45, so nothing is running there; `vm-dev` was
already stopped by goose and `vm-serve-mc` restored.

### Stage 6 — session C (`taoxichuan-06c-apk-argv-and-install.md`), host track (Sonnet) — metal track PASS, offline `apk add` works

Brief sent 04:22 IDT after the owner brought the ryzen back (GRUB entry 30 via `arm.sh 29`; herd `vm=true` entry), goose-glm probe `pong`.
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

### Supervisor lapse — the composition rule was dropped from the briefs for stages 6-7 (found 2026-10-25, owner's question)

**What happened.** The experiment's pinned composition rule (`MINIMAL_SELF_HOSTING_EXPERIMENT.md` § Process: pull as few crates as possible;
extract into a crate first where the logic still lives in `amd64/src/`; the goal is a *composition root*, not a new kernel) was carried by
the briefs for stages 1-4 and silently left out of 06a, 06b, 06c, 07, 07b and 07c-draft. The supervisor (Sonnet) wrote each brief from
the previous session's result and never re-read the doc's pinned sections. The owner noticed from the size of the kernel source.

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

**Where the pivot comes from.** The owner's reading, after stage 7 session D: the minimal kernel should be built the way the amd64 kernel was built, by taking the
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
(~14 goose-hours for stages 1-5 plus offline 6, plus ~9 for stage 7's network work), 9-12 will not be done by the talk on 2026-11-03; the owner's decision (2026-10-26) is to keep climbing regardless of the talk. Owner's quota readings at the end: weekly reset
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

**Process improvements** (the first two are the owner's idea: examine assumptions, and put more weight on probe discovery):
1. **Premise audit** (done: `BRIEF_TEMPLATE.md` "Premises"). Every brief carries a table of its premises, each with a read-only check the supervisor ran before sending. Goose's first job, before any edit, is to re-run each check and write HOLDS / WRONG / UNCHECKED per row in RESULT. A WRONG row that changes the plan is reported at once, not at the end.
2. **Probe discovery** (done: `BRIEF_TEMPLATE.md` "Probe discovery"). Before writing any probe, gate or script, goose lists what already exists (`userspace/*probe*`, `scripts/*_suite.py`, host tests, `run_gates.sh` groups, and a binary that already runs on the full amd64 kernel on the box, which gives the reference behaviour in one command) and runs the cheapest first. A new probe needs a stated reason and goes into the repo.
3. **Guest facts block** (done, in the template): no fork, no procfs, no `stat` applet, one core, the forward ceiling, the 1 GiB image. Briefs stop contradicting what is known.
4. **Budget and checkpoints** (done, in the template): a call budget per session (the supervisor nudges at 90 calls without a commit), a checkpoint at the first green host test, `[tmp]` tags on temporary diagnostics with a "Remove before final" list (the per-call `[statfs]` print was left in the kernel for lack of one).
5. **Quota in the metrics** (done: `zai_usage.py`): record the percent and token delta at the start and end of each session.
6. **Not done yet:** a hard per-exec timeout in `run_gates.sh` (about 90 s) in place of the ssh client's retry loop, which would have saved roughly an hour; `run_gates.sh` groups of 20 execs or fewer so the forward ceiling never matters; one agent owning the box at a time (a lock file under `/root/dev-cycle`), because the other agent and goose shared it today; a differential check that `amd64` still builds after an extraction (session O's baseline comparison was done by hand).

**Correction to the review above - the model split (owner's question, 2026-10-26).** The first calibration summed all models. The usage API reports per model: **16.1M tokens were GLM-5.3** (the more expensive model), all of them in two hours on 2026-10-24 (08:00-10:00 UTC), which is the stage 1 session (16.1M in the goose export); everything else, including both windows used for the caps, is GLM-5.3-Flash.
The caps (weekly about 205M, 5-hour about 34M) are therefore Flash-only and unaffected, because the current windows hold no GLM-5.3 tokens. The "before the pivot" totals above include the 16.1M: Flash-only they are about 125M, or about 94k per call, so the fall in tokens per call (about 40%) and per milestone stands; in quota terms the pre-pivot period cost more than its token count suggests. `zai_usage.py` now prints the split per window.


### Stage 8 session O (2026-10-26): M5 dynamic PIE — extraction done, first dynamic exec still faults (continues in O2)

**Result: M5 NOT closed.** Brief `scripts/kvm/goose/taoxichuan-08o-pt-interp.md`; goose session 20261026_30, 17:43-19:18 UTC, **223 tool calls, 19.7M input tokens** (brief budget 200; the supervisor steered at call ~210 after 110 calls without a commit). `taoxichuan-s4` @ `b3807175` (3 commits: `5bd2caec` extraction + loader, `b8f893c4` `[tmp]` diagnostics, `b3807175` default-members). Full detail: `RESULT-O.md` on the box (`/root/dev-cycle/taoxichuan-s7/`).
- **Premise audit paid again:** the brief said the interp/auxv logic was welded to `UserAddressSpace` in `amd64/src/loader.rs`; it is a 710-line adapter over `akuma-elf` already. Syscalls "said to be served" for ldso were partly ENOSYS: `PREAD64` missing, `MAP_FIXED` anonymous-only (ENODEV file-backed), `FUTEX`/`SIGALTSTACK`/`SET_ROBUST_LIST`/`PRLIMIT64` unarmed.
- **Composition:** new leaf crate `akuma-elf-exec` (401 lines, zero deps, `forbid(unsafe)`, 8 host tests); `akuma-elf` (19/19 host tests) and `amd64/` (`finish()` cross decision) pulled onto it. taoxichuan adds the ldso mapping in `load_task`, file-backed `MMAP`, `PREAD64`, monotonic `mmap_cur`. `taoxichuan/src` 4916 -> 5134 lines (+70 are `[tmp]` prints); `akuma-*` deps 19 -> 20.
- **The fault:** `curl --version` dies deterministically on the first dynamic exec: control reaches the ldso entry (`0x3006c185`, rsp correct, `AT_BASE=0x30000000`) but the bytes read back through the child's page tables there are not the file's (`8b 24 48 74 ...` where the file has `48 31 ed 48 89 e7 ...`), then `PageFault rip=0xfffffffff9bcd08d`. The `vec@entry` print (Vec wrong vs mapping wrong) is in the tree but boot 4 stalled at the ssh layer before exec (new anomaly, two connections, auth OK, no `[SSH] Exec`).
- **Static path:** `apk add curl` (b1) passed on all four boots. Supervisor check on the merge (`e2f728ea`): `scripts/build_taoxichuan_fc.sh` + Lima boot: 162 lines, `Ext2 filesystem mounted`, `sshd started`, 0 `Fault|PANIC|sig!`.
- **Quota:** 5-hour window 68% (23.6M of about 34M) and week 59% (119.6M) at the end of O, all Flash.
- **Process notes:** a 200-call budget was overrun by 23 calls and a steer was needed to get the WIP committed; the monitor tool (`Monitor`, 60 s poll for call count, litter head, silence, driver exit) replaced the manual 5-minute polls. Open: `apk info -L curl` crash untouched, `e_group.sh` does not start the mirror, `[tmp]` prints to remove.


### Host finding (2026-10-26): the KVM implementation has carried the whole experiment without a guest-caused host crash

**Observation (owner's conclusion, recorded with the evidence behind it).** Since stage 1 on 2026-10-24 the experiment has been, without being designed as one, a soak test of the amd64 kernel's KVM (`/dev/kvm0`, `herd vm-dev`): 31 goose sessions up to session O (about 2,700 tool calls, `Process review` above) plus O2, each booting the Taoxichuan guest repeatedly on the ryzen box over roughly two and a half days. Guests died on purpose and by accident throughout (`Fault ring3 PageFault ... task killed`, halted guests, guests parked mid-authentication, `herd stop vm-dev` on a live guest), and no guest took the host kernel down.
- **The one host reset in the period was not a guest event.** On 2026-10-26 at about 15:09 UTC Akuma reset to Pop while goose ran a long kernel build; klog ends in `[bkls>]` BKL-contention warnings with no panic text (entry for stage 8 session M above). That is the BKL-stall class in the notes, a host-side build load, and the klog does not implicate KVM. I did **not** establish whether a guest was running at that moment, so "not KVM" is the klog's reading, not a proof.
- **Host evidence checked 20:30 UTC:** uptime 5 h 02 min since that reset (covering sessions N, O and the first half of O2), `klog.log` 160 lines with 0 matches for `PANIC|BKL|spin|Fault`, all herd services `restarts=0` apart from deliberate `goose-glm` restarts. Session O alone booked 4 guest boots, two of them ending in guest faults. The exact guest-boot count over the whole experiment was not tallied (`vm-dev.log` rotates mid-boot), so "repeatedly" is a count of sessions, not of boots.
- **What this does and does not show:** KVM held under a long run of guest faults, hangs and restarts, and the host kept serving ssh, goose and the relay. It is not a measurement of concurrent guests (kvm0 is exclusive, one guest at a time), of multi-day uptime (the reset above bounds it at 5 hours since), or of a hostile guest.
