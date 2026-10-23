# The KVM dev cycle: an agent builds and tests Akuma kernels on the ryzen, and a laptop does the reboots

**Date:** 2026-10-23. **Status:** first run under way (`goose-glm`, task 01, the CMOS corruption). **Branch:** `kvm-part-1`.

The point of this document is to make one thing routine before the next big experiment: **an agent that lives inside Akuma edits the
kernel, builds it there, proves it boots as a guest of Akuma's own hypervisor, and hands a candidate over for a clean reboot of
the real machine.** The first task given to it is the one bug nobody has been able to chase from outside: the firmware on the ryzen
complains about the clock (a CMOS checksum error that waits for F1) after Akuma has run.

## 1. Roles and topology

```
 laptop (Mac)                         ryzen (one machine, two systems)
 ┌───────────────────────┐            ┌───────────────────────────────┐   ┌─────────────────────────┐
 │ scripts/kvm/          │  ssh       │ Akuma (entry 30, p3)          │   │ Pop!_OS (192.168.1.126) │
 │   dev_cycle.py        │──────────▶ │ 192.168.1.159:2222            │   │ the way back and the    │
 │ goose/*.md (briefs)   │            │ /src/.../akuma  (the checkout)│   │ place to read the CMOS  │
 │ git (the real history)│◀─ bundle ──│ goose-glm  (the developer)    │   │ with Linux's /dev/port  │
 └───────────────────────┘            │ kbuild, kinstall, herd, kvm0  │   └─────────────────────────┘
                                      └───────────────────────────────┘
```

* **goose-glm** (z.ai GLM, `goose 1.52.0` via `/usr/local/bin/goose-glm`) works **inside Akuma**: reads and edits the checkout,
  runs `kbuild`, starts guests, commits on its own branch. It cannot reboot the machine usefully (a reboot kills it, and Akuma
  cannot boot itself back from Pop).
* **The laptop** (`scripts/kvm/dev_cycle.py`) ships source in, starts and polls goose sessions, brings goose's commits back
  **as a ref only** (nothing is merged or pushed for it), and performs the clean reboot test: Akuma → Pop → Akuma, reading the CMOS
  on Pop and `dmesg` on Akuma on the way.
* **The owner** decides what is merged. goose commits stay on `cmos-fix` inside Akuma until the owner pulls them with
  `dev_cycle.py collect cmos-fix`.

## 2. Guest kernels: `/dev/kvm0`, not Firecracker

Use the herd `vm = true` services (`/dev/kvm0`), never Firecracker. Firecracker support exists (`/dev/kvm` node, `/dev/net/tun`) but is
shaky: with a NIC the guest wedges after ~10 s ([`AKUMA_FIRECRACKER_KVM.md`](AKUMA_FIRECRACKER_KVM.md) § *The guest wedge ... theories*), there is
no RTC model (a guest cannot exercise the CMOS write path), and it is exclusive with `kvm0`. A `kvm0` guest is the Akuma kernel
itself, booted as a PVH ELF from a file, with a disk image and a console that herd logs.

The pieces on the ryzen (entry 30 = `kvmctl kvmmem=4096 kvmcores=4-7`, reserve and cores for guests):

| file | what |
|---|---|
| `/boot/akuma-guest` | the long-lived guest kernel behind `vm-serve-mc`. **Do not overwrite.** |
| `/boot/akuma-guest-dev` | the candidate guest kernel (a copy of `kbuild`'s output). |
| `/boot/akuma-guest-root.img` | a 16 MiB ext2 root whose `/etc/guest.sh` prints `AKUMA-GUEST-UP`, `uname`, the md5 of `/bin/busybox`, writes and re-reads a 256 KiB file, then `AKUMA-GUEST-DONE`. Pristine copy. |
| `/boot/akuma-dev-root.img` | the writable copy the dev guest boots from. |
| `/etc/herd/available/vm-dev.conf` | the service: `vm = true`, `vm_kernel = /boot/akuma-guest-dev`, `vm_disk = /boot/akuma-dev-root.img`, 768 MiB, 1 vCPU, `restart = false`. |
| `/var/log/herd/vm-dev.log` | the guest's console. |

**One `kvm0` VM at a time.** `herd stop vm-netbsd; herd stop vm-serve-mc; herd start vm-dev`, and restore `vm-serve-mc` afterwards.
PASS for a guest boot is `AKUMA-GUEST-UP`, `ebce43017d2cb316ea45e08374de7315  /bin/busybox`, `AKUMA-GUEST-DONE` in the log and no
`EXCEPTION` or `panic` in the host's `dmesg`.

## 3. The loop

```
 laptop                                     Akuma (goose)                         laptop
 ──────                                     ─────────────                         ──────
 1  dev_cycle.py sync          ─────────▶   dev-base checked out
 2  dev_cycle.py ask cmos goose/01-cmos.md ─▶ 3  edit, host tests
                                             4  kbuild --features no-tests
                                             5  cp → /boot/akuma-guest-dev; herd start vm-dev; read log
                                             6  kinstall ... /boot/akuma-amd64.verify
                                             7  git commit (cmos-fix); write /root/dev-cycle/READY-FOR-REBOOT
 8  dev_cycle.py status cmos   ◀─────────   (goose stops)
 9  dev_cycle.py reboot-test   ── clean reboot ──▶ Pop ── arm.sh 29 ──▶ Akuma    report → out/
10  dev_cycle.py ask cmos report.txt        resumes the same session, reads the report, iterates or finishes
11  dev_cycle.py collect cmos-fix           refs/akuma/cmos-fix for the owner to review
```

Commands (all from the repo root; output lands in `scripts/kvm/dev_cycle_out/`, git-ignored):

```sh
python3 scripts/kvm/dev_cycle.py sync                                   # bundle HEAD to Akuma, check out as `dev-base`
python3 scripts/kvm/dev_cycle.py ask warmup scripts/kvm/goose/00-warmup.md --new
python3 scripts/kvm/dev_cycle.py status warmup -n 60
python3 scripts/kvm/dev_cycle.py ask cmos scripts/kvm/goose/01-cmos.md --new
python3 scripts/kvm/dev_cycle.py reboot-test                            # the clean reboot test (section 5)
cp scripts/kvm/dev_cycle_out/<stamp>-report.txt /tmp/r.txt; python3 scripts/kvm/dev_cycle.py ask cmos /tmp/r.txt   # resumes the session
python3 scripts/kvm/dev_cycle.py collect cmos-fix
```

`ask` runs `goose-glm run -i <brief> -n <name> [-r] --max-turns 80` detached on Akuma with its log in `/root/dev-cycle/<name>.log`;
a second `ask` with the same name **resumes** that session (`-r`), which is how a report is handed back. (`goose serve` exists
for an ACP server over HTTP/WebSocket; the detached-`run`-plus-log shape was chosen because it needs nothing from Akuma's
sockets beyond ssh.)

## 4. Environment facts that cost time (read before improvising)

* **Akuma sessions have no environment.** Every command starts with `. /etc/akuma-dev.env;` (sets `PATH`, `LD_LIBRARY_PATH` for
  rustc, `CARGO_HOME`, `AKUMA_SRC=/src/github.com/netoneko/akuma`) and `export TERM=dumb GIT_PAGER=cat` (otherwise `git` waits on a pager).
* **`kbuild` is the build** (`-j1` default, `-j4` has corrupted builds; output in `/root/ktarget`, not the checkout; incremental
  builds take one to fifteen minutes; never `cargo clean`). `kinstall [elf] [dest]` writes a kernel to a dest, default
  `/boot/akuma-amd64`; **the dev loop installs only to `/boot/akuma-amd64.verify`**, which is menu entry 30's kernel (`arm.sh 29`).
  The default entry's kernel is the fallback and is never touched.
* **`git fetch` from GitHub hangs inside Akuma** (the known git helper-pipe issue, `AKUMA_FROM_SCRATCH.md` § 3.1). Source goes in as a
  `git bundle` (`sync`), history comes out as one (`collect`). Never run `git fetch/pull/clone` against a URL there.
* **ssh from agent shells on the laptop:** the plain `ssh` CLI is blocked; the scripts use Python `subprocess`. Akuma's own `ssh` takes no `-o`.
* **Rebooting Akuma lands in Pop**, not back in Akuma. The way back is `sh overlays/ryzen/arm.sh 29` as root on Pop (one-shot
  systemd-boot + GRUB entry), and Akuma answers ssh about 30-60 s later. The laptop does this; goose never reboots.
* **Pop never answering is a result, not a timeout to retry.** If the firmware stops at the CMOS error and waits for F1, Pop never
  comes up and a human must press F1. `reboot-test` reports exactly that and stops; do not loop it.
* Never `pkill` by name (the owner's other VMs); kill by pid. Never pipe a long-running command into `grep -q`; send output to a file and poll.
* Process listings: `ps` on Akuma shows threads as `{name}` rows; `kill -9 <pid>` the process row.
* A leaked `goose`/`claude` after an ssh drop keeps running (hangup is not delivered): check `status` before starting another.

## 5. The clean reboot test (`dev_cycle.py reboot-test`)

"Clean" means: no VM running, filesystems synced, the machine reset through the kernel's own `reboot(2)` path (`reboot -f` = `sc-reboot`
→ `akuma-boot`), no power cut and no watchdog. Steps, with what each proves:

1. Save `dmesg` of the running boot, `md5sum` of `/boot/akuma-amd64.verify` (what is about to be booted).
2. `herd stop` every VM, `sync`, `sync`, `reboot -f`.
3. Wait for Pop (default budget 240 s). **No Pop = FAIL** (probable firmware stop; needs a human; see section 4).
4. On Pop, as root, read the whole CMOS through `/dev/port` (ports 0x70/0x71, indices 0-0x7F, NMI bit never set) and report: the 128
   bytes, the classic checksum (bytes 0x10-0x2D summed against 0x2E-0x2F), status registers A/B/D, the diagnostic byte 0x0E, and the
   breadcrumb byte 0x7F. Also Pop's own kernel lines about rtc/cmos/clock and `hwclock --show`. **A checksum MISMATCH is a FAIL.**
5. `arm.sh <entry>`; wait for Akuma (default budget 300 s). Record `uname -a` (the build id shows which kernel booted) and
   `dmesg` lines for clock/cmos/rtc/exception/`[KVM]`.
6. Verdict: PASS only if Pop returned, the CMOS checksum agrees, Akuma came back, and `dmesg` has no `EXCEPTION`/`panic`.

**Acceptance for the CMOS task** (owner's rule, 2026-10-23): a clean reboot plus inspecting `dmesg` is enough to call the RTC
problem fixed, *unless the agent finds another cause besides the suspects below* — in which case it reports that too, with
evidence, before anything is called fixed. A before/after CMOS dump from `dev_cycle.py cmos` (taken on Pop) is the objective
evidence; run it once before the first Akuma test to know the starting state.

## 6. The first task: the CMOS corruption (`scripts/kvm/goose/01-cmos.md`)

**Symptom.** After Akuma runs, the next boot stops in firmware on a clock/CMOS error ("press F1 to proceed"); Linux then complains
about the clock as well. The owner's guess: a reboot interrupts a write to the chip.

**Suspects, owner's:**

1. `write_rtc_from_sntp` (`amd64/src/clock.rs` → `akuma_cmos_rtc::write`, SNTP time written back to the RTC the way Linux's
   `sync_cmos_clock` does). It sets SET (status B bit 7), writes the date/time registers and the century (0x32), clears SET. A reset in
   that window leaves the RTC stopped with SET=1 and a half-written date.
2. `crumb()` in `amd64/src/kvm.rs`: breadcrumb bytes written to CMOS **0x7F** during the SVM rehearsal/`kvmtest` paths (so a run that
   resets the machine can be diagnosed). Many firmwares checksum the whole CMOS range; 0x7F may be inside it.
3. The reboot landing in the middle of a CMOS access (index written, data not; or the SET window).

**Suspects added while writing this (for the agent to confirm or kill):**

4. **A guest writing the real chip.** `kvm.rs:484` builds the rehearsal guests' I/O permission map with only port 0x80 intercepted
   (`with_port([0; SIZE], 0x80, true)`): everything else, including 0x70/0x71, executes natively. The Linux/Akuma guests use the
   all-intercept map (`IOPM_ALL`, `kvm/linux.rs`). A guest Akuma kernel runs the same `clock.rs`, so any VMCB that is not
   all-intercept lets a guest's SNTP write land on the host's CMOS. Every VMCB path must be checked (`kvm.rs` rehearsals, `kvm0`
   machines, the `/dev/kvm` node).
5. The shutdown-status byte 0x0F and the diagnostic byte 0x0E: the firmware reads them after a reset; the `system_reset` path (8042 /
   ACPI reset register / triple fault, `akuma-boot`/`akuma-psci` neighbours) may leave them in a state it distrusts.
6. Format and register hygiene: the century byte (0x32) may lie inside the firmware's checksum; changing status B's data-mode bits
   (BCD/binary, 12/24 h) to the kernel's preference changes bytes the firmware also covers; leaving status A's divider bits or
   status B's periodic/alarm enables different from the firmware's.
7. NMI-disable (index bit 7) is stripped by `akuma_port::CMOS`; the index latch is left pointing at the last register written.
8. Nothing says the cause is in the kernel at all: a **dead or weak CMOS battery** gives the same firmware message, and the
   first write after a power loss is what makes it visible. The CMOS dump on Pop before any test tells which it is (a stored checksum
   that mismatches *before* the first Akuma run in the session is not Akuma's doing).

**What a fix may look like** (agent's choice, smallest first): stop writing the RTC from SNTP at all (the wall clock does not need
it), or write it only when SET cannot be left set; move breadcrumbs out of CMOS (the console ring / a reserved RAM word / the serial
log) or into a byte the firmware does not cover; make the reset path write 0x0F deliberately; make every guest VMCB all-intercept.

## 7. First-run log

*(to be filled as the loop runs)*

| when | step | result |
|---|---|---|
| | `sync` | |
| | task 00 warm-up | |
| | CMOS dump on Pop before any test | |
| | task 01 | |
| | `reboot-test` | |

## Background

[`AKUMA_FIRECRACKER_KVM.md`](AKUMA_FIRECRACKER_KVM.md) (the `/dev/kvm` node, affinity, `/dev/net/tun` and the parked guest wedge);
`docs/reference/subsystems/kvm.md` (`kvm0`, herd `vm = true`); `docs/runbooks/amd64-bare-metal-loop.md` (the older loop for the HP
box); `overlays/ryzen/{arm.sh,cycle.py}` (entry arming and the laptop-driven reboot loop for autorebooting entries).
