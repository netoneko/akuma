# Minimal self-hosting kernel "Taoxichuan" — agent log

**Status: in progress. Started 2026-10-24.**

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
