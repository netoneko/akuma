# Building a kernel with local AI: pain and suffering

A running log of trying to make a local model (Qwen3-8B, on an old i5 under Akuma) build and boot an Akuma kernel
through `proposals/INSTALL_SCRIPT.md`, with other models (GLM via goose) doing the setup around it. Started
2026-11-04. Newest sections are at the bottom; each one says what was believed, what was measured, and what was
wrong. Dated sections below are a diary, not reference: the rules they established go in `docs/reference/` when
they are proven.

**Status (2026-11-04, night): the Qwen is served and the first agent test against it is running. Not finished.**
(The rest of this section was written as the setup log; its "not verified" notes are superseded by Update 2.)

## Goal

Work through `proposals/INSTALL_SCRIPT.md` (steps I0-I6) with autonomous agents on the two boxes. The install script
is meant to be run by a *local qwen* under an agent harness, so the first job is a Qwen the LAN can reach: it is both
the proposal's end user (step I5, the stranger test) and the model the agents' output is judged against.

## Hosts as found

| box | state at start | notes |
|---|---|---|
| trashcan (HP, `192.168.1.123`) | booted into Akuma (`hpbox.py which` -> `akuma`) | 4 cores i5-4460 (AVX2, no AVX512), 16 GB RAM (~15 GB free), 53 GB free on `/`, no GPU. `/bin/llama-server`, `curl`, `git`, `gcc` present; no `goose`, no `.gguf` over 10 MB. `huggingface.co` answers HTTPS from the box. A herd conf already runs `llama-server` with `/models/smollm2-135m.gguf` on `127.0.0.1:8081`. |
| ryzen (`192.168.1.159`) | first probe: `ssh ... port 2222: Operation timed out` (it was on Pop, not Akuma); later `up 2 min` on Pop | rebooted to Akuma, see below |

## Decisions

- **Model: Qwen3-8B Q4_K_M (~5 GB), fallback Qwen3-4B.** CPU-only Haswell, 4 threads: an 8B at Q4 fits with room for
  context and should do single-digit tok/s; the 30B-A3B MoE at Q4 (~18 GB) does not fit in 16 GB. Expect slow; the
  measured number goes in the agent's RESULT file.
- **The GLM goose server runs on the Mac, not the box**, because ryzen was unreachable when the task started and
  because trashcan has no goose. Script: `scripts/kvm/goose/goose-glm-mac.sh serve --port 3286` (`glm-5.3-flash`,
  low effort, z.ai token from `~/.akuma/z.ai/token`, ACP key `~/.akuma/goose/acp_secret_key`). Probe (`pong`) passed.
  The agent reaches trashcan through `scripts/utils/hpbox.py ak '<cmd>'`.
- Brief delivered as a **file** with a three-line prompt (the runbook's rule: an 8 KB prompt stalled two sessions).
  The brief lives in the session scratchpad, not the repo; its content is summarised under "Brief" below.

## What was run

1. Read `proposals/INSTALL_SCRIPT.md`, `scripts/kvm/goose/SUPERVISOR_RUNBOOK.md`, `BRIEF_TEMPLATE.md`.
2. Surveyed trashcan over `hpbox.py ak` (numbers above).
3. Started the Mac goose-glm server on `127.0.0.1:3286` and probed it. The first probe failed with
   `KeyError: 'result'`: `session/new` needs a **Mac** `cwd` (`GOOSE_ACP_CWD`), the same trap the runbook records for
   the Kimi server. With it set the probe answered `pong`.
4. Sent the brief as session `qwen-trashcan` (`scripts/goose_acp_prompt.py`, 4 h budget). Log:
   `glm-qwen.log` in the session scratchpad.
5. Rebooted ryzen from Pop to Akuma: `hpbox.py rzr 'cd .../akuma && sh overlays/ryzen/arm.sh 29'` (GRUB entry 29;
   `next_entry=29` confirmed, reboot issued 17:18). **Not yet confirmed that it came up**; use `scripts/vm_ready.py`
   style ssh round-trip (`scripts/kvm/goose/ak.py 'uname -a'`), never a log grep.

## Brief (summary)

Download Qwen3-8B Q4_K_M to `/models/` (background `curl -C -`, size check against Content-Length); prove load and
generation on `127.0.0.1:8082` first and record load time and tok/s; create a new herd conf serving
`--host 0.0.0.0 --port 8081`, stopping and disabling only the smollm2 service that holds that port; verify from the
Mac with `/v1/models` and a chat completion, then `herd restart` and verify again; write
`scripts/kvm/goose/RESULT-qwen-trashcan.md` (sha256, conf, tok/s, memory, deviations). Forbidden: reboot, `pkill`,
git writes, sub-agents, touching console/sshd/kot/httpd/hda-capture services.

## Observed so far

- After ~45 s the agent had made 7 tool calls and was surveying the box. Its first finding: `which`/`df /` output
  looked wrong (`df /` showed only proc) and it went looking for `curl` and the real root fs. Unresolved at the time
  of writing; the box's own `df` earlier showed `/dev/sda1` 64 GB, so this may be an Akuma `df`/PATH quirk through the
  non-login ssh shell. Check the agent's log before trusting its conclusion.

## Open

- Qwen endpoint: unverified. Nothing was confirmed reachable from the LAN.
- Ryzen: needs the Akuma boot confirmed, then a `goose serve` in the Taoxichuan environment (herd service
  `goose-glm`, `192.168.1.159:3284`, per `SUPERVISOR_RUNBOOK.md` § Setup) and a first brief for the remaining
  `INSTALL_SCRIPT.md` steps. Not started; the user wants it begun when the Qwen is downloaded. Candidate split: I0
  (fresh-clone proof) and I1 (x86_64 `install.sh`) on ryzen where KVM and the Taoxichuan kit already work; I4
  (arm64 minimal disk) cannot be done there.
- Whether the Mac goose server should stay once ryzen's is up.

## Background

`proposals/INSTALL_SCRIPT.md`, `scripts/kvm/goose/SUPERVISOR_RUNBOOK.md`, `scripts/kvm/goose/goose-glm-mac.sh`,
`scripts/utils/hpbox.py`, `overlays/ryzen/arm.sh`.

## Update (2026-11-04, evening): what happened to the Qwen on trashcan

Times: Mac is IST; boxes and session logs are UTC.

1. **Single-stream download is too slow.** `huggingface.co` -> trashcan ran at ~240 KB/s (ETA 5 h 40 m for the
   5,027,784,224-byte `bartowski/Qwen_Qwen3-8B-GGUF` Q4_K_M file). The GLM agent switched to 8 parallel ranged `curl`
   parts (`/tmp/dlpar.sh`, parts in `/tmp/parts/part0..7`, 628,473,028 bytes each): ~2.1-2.2 MB/s aggregate.
2. **The box went hard-down at 35 %** (ssh and ping dead, no ARP entry, a /24 sweep found nothing). It was
   power-cycled by hand. Cause not established: the 8 concurrent appends into ext2 are the only thing that changed.
   After the restart all 8 parts were exactly full size (finished 16:02-16:06 UTC), so nothing was re-downloaded.
3. **Assembly failed with `cat: write error: I/O error`.** `cat part0..part7 > /models/qwen3-8b-q4_k_m.gguf` ran at
   ~7 MB/s, `stat` showed 1.29 GB at 18:34, and at 18:41 the process was gone, `/tmp/asm.log` held two
   `write error: I/O error` lines, and the output file was **4,222,104 bytes** (it shrank). No `sha256`, no `done`
   marker. `dmesg` (full of `[probe]` lines) showed no storage error. Uptime was continuous, so this was not a reboot.
   Parts written by `curl` at 2 MB/s were fine; one sequential writer at 7 MB/s was not.
4. **Not yet known:** whether this is the disk (hardware), the Akuma storage/ext2 path, or both. Two earlier incidents
   point at the same area (`project_amd64_ext2_lost_bitmap_writes`, the USB/xHCI disk), so the root filesystem should
   be treated as suspect until `e2fsck -n` says otherwise. Do not repair it blindly.
5. **Process slips worth keeping:** a watcher loop used `grep -q done` on the error text "can't open /tmp/qwen.done",
   so it fired early (match the success marker exactly, e.g. `test -f ... && echo FINISHED`). `df -h /` on Akuma
   shows only procfs; use `df -h /dev/sda1`. `curl` is `/usr/bin/curl`, not `/bin/curl`.

### Plan agreed with the user
- User boots trashcan into **Ubuntu**. From there: assemble the parts, compare sha256 with Hugging Face's published
  `54fffa050078e984116639c83dfb64b5aa6d4cd474e018b076777c632bbccccd`, run `e2fsck -n` on the Akuma root and read the disk's SMART/kernel
  log, then boot back into Akuma.
- **Update trashcan's Akuma kernel from main** while it is on Ubuntu (it reports `0.0.8 e61f9ee9-release-smp-shared`;
  ryzen runs `fca1f9ed`). Build on the box (a Mac-built kernel hung on ryzen metal,
  `docs/archive/AMD64_MAC_BUILT_KERNEL_METAL_HANG.md`), `--features no-tests`, install with `chmod +x`,
  `/root/stage_akuma.sh` is the existing recipe. Keep the old kernel as a GRUB fallback entry.
- Then finish the Qwen: proof run on `127.0.0.1:8082`, herd conf for `0.0.0.0:8081` (disable-then-enable), LAN check,
  `wait_qwen.py` prints `QWEN_READY=`.

### Ryzen session status at this point
`install-i1b`: 42 tool calls, G1 recorded PASS in `RESULT-I1.md` (the kernel package is `taoxichuan`, the README's
`-p osaka` is stale; the pinned 2026-09-19 toolchain is ignored because the box has no rustup), `scripts/install.sh`
exists with three run logs, G3 waiting on `wait_qwen.py`. It may push only `install-i1:install-script` to `litter`.

## Update 2 (2026-11-04, night): the Qwen is up on trashcan

**Result: `http://192.168.1.120:8081/v1` serves Qwen3-8B Q4_K_M from a herd service (`qwen`), ~4.6 generated tok/s,
~10 prompt tok/s, thinking off. Verified from the Mac (two completions, `/v1/models`), then `herd stop`/`start`: no
leaked process, memory back to 14.5 GB free, served again.** `wait_qwen.py` printed `QWEN_READY=`.

What was done, in order:
1. **From Ubuntu** (`sdb1` is the Akuma root, a 1 TB USB disk; SMART passes, 8 reallocated sectors, 0 pending):
   assembled the 8 parts (`cat`, 5,027,784,224 bytes) and checked **sha256 = Hugging Face's `x-linked-etag`
   (`54fffa05...cccd`)**. The Akuma-side `cat` had failed with an I/O error, Ubuntu's did not.
2. `e2fsck -fn` showed leaked bitmaps (blocks/inodes marked used, actually free) and one wrong link count (inode
   3014657). The user authorised `e2fsck -fy`; it exited 1 (errors fixed) and a second `-fn` pass was clean. The
   kernel, model, conf and everything written afterwards survived; a final `-fn` after all copies was clean too.
3. **Kernel updated from `origin/main` (`e80e6cd9`)**, built ON the box (`cargo build -p akuma-amd64 --target
   x86_64-unknown-none --release --features no-tests`, pinned nightly-2026-09-19 via rustup), installed as the Akuma
   partition's `/boot/akuma-amd64` (`chmod 755`, old one kept as `akuma-amd64.before-e80e6cd9`). It reports
   `0.0.9 e80e6cd9-release-smp-shared`. **The amd64 kernel needs the `crates/akuma-fbcon/vendor/spleen` submodule**
   (the first build failed on it); `proposals/INSTALL_SCRIPT.md`'s "no submodule" claim holds for Taoxichuan only.
   GRUB loads the kernel from the Akuma partition; `/root/stage_akuma.sh` (installs to Ubuntu's `/boot/akuma`) is stale.
4. **The image's `llama-server` was a generic x86-64 build** (`-march=x86-64`, AVX/AVX2/FMA off; `system_info` listed
   only LLAMAFILE and REPACK): **0.83 prompt / 0.88 gen tok/s** on Ubuntu. Rebuilt static musl with `-march=haswell`,
   `GGML_AVX/AVX2/FMA/F16C/BMI2=ON` (`/root/llama-build-haswell`, Ubuntu side), installed as `/bin/llama-server-hsw`
   (the stock `llama-server` is untouched). The kernel saves AVX state (`OSXSAVE`, `XCR0` = x87+SSE+AVX, `xsave64`),
   so this is safe. Linux with it: **4.6 gen / ~15 prompt tok/s**.
5. **Thread scaling on Akuma is poor above 3 threads** (same binary, same box, `-c 2048`, 32-token completions;
   gen tok/s, prompt tok/s in brackets):

   | model | 1 thread | 2 | 3 | 4 |
   |---|---|---|---|---|
   | Qwen3-8B | 2.29 (3.8) | 3.93 (7.6) | 3.59 (10.0) | 1.45 (7.8) |
   | Qwen3-4B | 4.25 (7.2) | 7.26 (14.4) | 5.80 (18.1) | 2.40 (10.7) |

   Linux ran the 8B at 4 threads at 4.6. Hypothesis, NOT measured: llama.cpp's per-layer thread barrier plus kernel
   work on one core (netpoll, timer) stalls the whole team. A Linux `llama-bench` sweep on the same box would turn
   this into a kernel report; it needs another reboot to Ubuntu. Served with **`-t 3`** (prompt-bound agent turns).
   `-t 2 -tb 3` is untested since the crash below was understood.
6. **Qwen3-4B Q4_K_M staged as the backup** (`/models/qwen3-4b-q4_k_m.gguf`, sha256 `fbe1d5ed...bae3` = HF's, `cmp`-checked
   after the copy; `/etc/herd/available/qwen4b.conf`, not enabled). Switch: `herd stop qwen; herd disable qwen;`
   copy `qwen4b.conf` over `qwen.conf`'s args; `herd enable qwen; herd start qwen`.
7. Service config (`/etc/herd/available/qwen.conf`, enabled):
   `command = /bin/llama-server-hsw`, `args = -m /models/qwen3-8b-q4_k_m.gguf --host 0.0.0.0 --port 8081 -c 8192 -t 3
   --parallel 1 --no-mmap --reasoning-budget 0`, `restart = true`, `restart_delay_ms = 5000`.

### Traps found (each cost time)
- **The box's DHCP address changed to `192.168.1.120`** (Ubuntu had been `.123`). `wait_qwen.py` now defaults to
  `.120`; it will go stale again. A DHCP reservation or a fixed address for the Akuma MAC would fix it.
- **`kill $!` on Akuma's `sh` did not kill the server**: two sweep servers (ports 8090, 8203) stayed alive, ~5 GB
  each. The herd-started `qwen` then died with **SIGSEGV (exit 139) five times while loading**; `dmesg` showed a
  user-mode write `#PF` inside a mapped RW anonymous region, i.e. the kernel failed to populate a page with 4.3 GB
  free of 16. `kill -9` on the two leaders freed 10 GB and the same config then started cleanly. **Open:** an
  allocation failure should not surface as SIGSEGV in a mapped region; that is a kernel finding to chase
  (repro: hold ~11.7 GB, start the 8B with `--no-mmap`). My first hypothesis (`-tb` resizing the pool) was wrong.
- `hpbox.py ak` has a 60 s limit: background anything longer (`setsid nohup ... &`) and poll for a marker file.
- Never `grep -q done` on error text; test for the file. `df -h /` on Akuma shows procfs only.

### Ryzen agent (`install-i1b`)
65+ tool calls; `scripts/install.sh` and its runbook and `bootstrap/public/install.txt` are pushed to
`litter/install-script` (`dabe4a5b`). A steer with the new URL was **not delivered** (`session/load` unanswered), so
its poller was restarted on `.120` instead and `/root/install-work/QWEN-URL.txt` holds the note; G3 starts when the
poller prints `QWEN_READY=` (it now has).

## Update 3 (2026-11-04, late night): mmap, the page cache, and the mistakes

### The crash is still a hypothesis
Believed: llama.cpp with `--no-mmap` writes ~5 GB into fresh anonymous memory; the kernel hands out frames lazily on
first touch (`mm::fault_in` -> `populate_page`), and when the PMM has none it logs `[MM] fault REFUSED ... why=oom`
and the fault falls through to SIGSEGV (`idt.rs` `page_fault_trap` -> `user_fault`). No OOM killer, no ENOMEM, no
SIGBUS. Evidence: exit 139 five times with 4.3 GB free and a model needing ~5 GB; a write `#PF` inside a mapped RW
anonymous region; a clean start after 10 GB was freed. **Not yet seen:** the `why=oom` line (the dmesg ring had wrapped).
The alternative is a race of two cores faulting one page. The repro (two 8B servers fill RAM, a third starts; read
`why=` and the `[memwatch-at-kill] pmm_free=` line) is written but **not run**: I stopped the served Qwen once to run it,
which broke the ryzen agent's first G3 attempt, so it waits until that test is done.

### mmap (the 4B Q4_K_M, `-t 2`, same binary and box)
| | `--no-mmap` | mmap (default) |
|---|---|---|
| generated tok/s | **7.26** | 2.54, 2.45 (two requests in a row, so not first-touch) |
| prompt tok/s | 14.4 | 6.3, 2.2 |
| load | ~40 s polling | 9 s (pages already cached) |
| buffers | all weights copied to anonymous memory | `CPU_Mapped` 2362 MiB (the file) + `CPU_REPACK` 1683 MiB |
| memory | "used" +2.5 GB | "used" +2.0 GB, "buff/cache" +2.4 GB |

- The cache is real: it **stayed** (2,726 MB) after the server was killed, and `available` counted it. That is the
  amd64 kernel's `akuma-fpcache`, shared across processes and evictable by `reclaim_clean_file_pages`.
- mmap is ~3x slower for generation, steady state. **Cause unknown.** Candidates, none tested: file-backed PTEs
  mapped with different attributes; the unrepacked tensors read straight from cache-hot-but-4K-mapped file pages;
  the shared-page path taking a lock per fault. `perf`-style counters would settle it; a `fault REFUSED`-style
  counter for the mapped region would too.
- So mmap **does not** fix the throughput, but it changes the failure: with file pages evictable, memory pressure
  should mean eviction and re-read, not SIGSEGV. **Untested; this is the experiment that matters.**

### Taoxichuan and memory (facts from the tree, nothing run)
- `taoxichuan/docs/reference/memory.md` says: "there is no page cache, so each mapping has its own copy". `taoxichuan/Cargo.toml`
  has no `akuma-fpcache` dependency. The amd64 kernel and `akuma-tokio-kernel-amd64`/`-core` (`lazy.rs`, `mm.rs`) do.
- `akuma-fpcache` is 558 lines, `forbid(unsafe)`, keyed `(inode, mount id, offset)`, refcounted through the CoW count,
  configured by an `FpcacheConfig` at `init`. **Adding it to Taoxichuan is plausibly small, but it rides on B21's move of
  the memory code into `akuma-tokio-kernel-core`**; check how much of `lazy.rs` Taoxichuan can adopt before calling it small.
- Guest RAM: the README's example guest has `ram_mib=3072`; the running `vm-dev` on ryzen has `vm_ram_mib = 768`.
  A 4.7 GB model does not fit either. With no page cache an mmap'd model would be a private copy in each process.
  Only Qwen2.5-0.5B (491 MB) is plausible inside a 768 MiB guest, and not with a kernel-side cache of its own.
  The sane split stays: model server on a real host, the Taoxichuan guest is the client.

### Mistakes made today (so nobody repeats them)
1. **Stopped the Qwen service while an agent was using it.** The repro needed the memory; the ryzen agent's G3 attempt
   hit `Could not connect to 192.168.1.120:8081`. Check who is using a shared service before touching it.
2. `grep -q done` matched the error text `can't open /tmp/qwen.done`; test for the file.
3. `kill $!` on Akuma's `sh` does not kill the server; two 5 GB servers leaked and caused the crash above. Find the
   leader pid by its `--port` in `ps` and `kill -9` that.
4. Wrote the Qwen URL as `.123` into `wait_qwen.py` and the agent's brief; the box had moved to `.120`.
5. A steer to the ryzen agent was **not delivered** (`session/load` unanswered). Do not assume a steer arrived;
   put the correction in a file or the poller the agent already reads.
6. An `e2fsck -fy` was refused by the permission classifier until the user said so explicitly.

### Where it stands
Ryzen agent `install-i1b` (80 tool calls) pushed `scripts/install.sh`, the runbook and `install.txt` to
`litter/install-script` and is in G3, driving a goose session against the Qwen on trashcan. Verdict pending; a
watcher is on the RESULT file. Next: G3 verdict, then the OOM repro, then mmap-under-pressure (does eviction replace
the SIGSEGV?), then the Linux `llama-bench` thread sweep for the Akuma scaling cliff.

## Update 4 (2026-11-04, night): Infinity pushed and shipped; aarch64 Taoxichuan runs goose

**Infinity `no-aws`.** GLM's port (`docs/archive/INFINITY_ON_TAOXICHUAN.md`) had never left the ryzen guest. It is
now branch `no-aws` on `github.com/netoneko/infinity`, carried out of the guest's disk as a git bundle from
Pop and pushed from the Mac as tama. Built on the Mac as static musl release binaries: 38 s for amd64 and 53 s
for aarch64, against about 18 minutes of debug build inside the guest. The amd64 build is on the ryzen at
`/root/infinity-no-aws/` (binaries, guest config, a README with the staging steps, checksums), for another
agent to stage onto a Taoxichuan disk and run. It has not run on Taoxichuan yet.

**aarch64 Taoxichuan** (B21, `docs/archive/TAOXICHUAN_B21_AARCH64.md`) now boots 4 cores and runs goose 1.52.0
against GLM: PONG in 6.7 s, then a shell-tool turn that wrote a file and answered `aarch64, 4`, with no kernel
change. Infinity does not run there yet: the aarch64 half never grows the user stack, and `infinity` goes past
the first 64 KiB at start (details and the fix in the Infinity doc). Firecracker for aarch64 Taoxichuan is being
handled by another agent.

### Traps found (each cost time)
1. **Lima mounts the Mac's repo read-only.** Firecracker opens its drive read-write and refuses with
   `Read-only file system`. Copy the disk into Lima's own filesystem (`/var/tmp`) first. QEMU got away with
   it only through `snapshot=on`.
2. **`overlays/devbox-firecracker/guest-setup.sh` never started dnsmasq.** Its `pgrep -f 'dnsmasq.*tap0'`
   runs inside `sh -c '<that check>'`, so it matched its own command line and reported "already serving".
   Fixed in the script with `[d]nsmasq`.
3. **A tar made on macOS carries AppleDouble `._*` files.** Delete them on the far side, or use
   `COPYFILE_DISABLE=1 tar`.
4. **The ryzen was in Pop**, so the Akuma address (`.159`) timed out. Pop is `.126`. The Infinity guest's disk
   can be read from Pop by mounting the Akuma partition read-only, which is only safe while Akuma is not running.
5. **The amd64 rule is not the aarch64 rule.** A binary that runs on one Taoxichuan can fault on the other: amd64
   grows the stack and aarch64 does not. Parity is per behaviour, and the B21 log's status table is where it is
   tracked.

## Update 5 (2026-11-05): roles change; the script works; a 4B Qwen and Gemma pass the dev trial on Linux (Claude)

**Who does what now.** GLM wrote the first `install.sh`, runbook and `install.txt` and was tuning them between Qwen attempts. That is
tuning on the test set, so GLM was stopped; Claude owns the script and the runbook, and a local model only *executes* the runbook. GLM's
work is on `litter/install-script`. Ryzen stays in Pop for now (user decision): both harnesses are checked on Linux before Akuma.

**The oracle (a human drives the script).** On Pop, as an unprivileged user, `install.sh up --vmm firecracker` built the Taoxichuan kernel
and `sshd` from a fresh shallow clone, made the image, booted it under Firecracker and `ssh 'busybox uname -a'` returned
`Linux taoxichuan 0.1.0-ae1ead4-taoxichuan ae1ead4-release x86_64 GNU/Linux`. GLM's Firecracker path had **never been run** and had
five bugs, found in this order:
1. It created the tap with `ip tuntap add`, which needs root; the proposal's rule is no `sudo`. Now it uses a tap that already exists,
   creates one only if it is root, and otherwise stops and prints the one command an administrator runs once.
2. It never made the private disk copy (`disk-run.img`) the Firecracker JSON names, so Firecracker exited at once and the script
   still printed `boot:` and waited 180 s. It now copies, checks the VMM is alive two seconds after launch, and the wait loop fails
   fast (exit 50) with the console text if the VMM dies.
3. It waited on ssh port 2222 (the QEMU hostfwd port); a Firecracker guest is reached on 22 over the tap.
4. `ssh`, `status` and `down` had to be told `--vmm`/`--dir` again; the boot stage now records the VMM in `state/vmm`.
5. `install.sh ssh '<cmd>'` was rejected as an unknown flag; the first non-option argument now starts the command.
Added: `--kernel-bin PATH` (boot a supplied kernel), and `check` now fails for Firecracker when `/dev/kvm` is not writable.

**Host facts (Pop).** `netoneko` has rustup, `~/bin/firecracker`, qemu, `ssh-keygen`; it is **not** in the `kvm` group. An ACL granted by
hand (`setfacl -m u:netoneko:rw /dev/kvm`) **was reset by the system within the hour** and cost a trial. The harness re-grants it (an
administrator action, never the model's); a durable fix is `usermod -aG kvm netoneko` and a new login. The tap `akuma-tap-2222`
(10.0.2.2/24) is also created by hand and does not survive a reboot. Pop's root disk was 96 % full; clones and `cargo clean` freed ~1.3 GB.

**The harness** `scripts/kvm/goose/qwen_install_trial.py` (design: `proposals/INSTALL_SCRIPT_QWEN_EXPERIMENT.md`): resets, ships the script and
`bootstrap/public/install.txt`, runs `goose run` as `netoneko` against a model endpoint, then judges by itself (live VM, a real ssh
`uname`, no `sudo`/package manager in the transcript). It runs a preflight (`install.sh check`) and refuses to start a trial that
cannot boot.

**Where the model runs now.** The trashcan Qwen3-8B (4.6 tok/s, ~11 minutes for goose's 6.5k-token opening prompt) was too slow
to iterate on, so trials use the Mac's own ollama (`qwen3:4b`, `gemma4:e4b`) through a reverse ssh tunnel (Pop `127.0.0.1:21434` to the
Mac's `11434`), with nothing exposed on the LAN. Goose's **generic OpenAI client cannot decode ollama's stream** (`Stream decode error`);
its native `ollama` provider works. (The Mac has 48 GB RAM but 14 GB free disk, so the 35B Qwen from `../yolo` cannot be pulled.)

| trial | model | outcome |
|---|---|---|
| qwen3-8b trashcan (2 runs) | Qwen3-8B Q4 | invalid: server ran with `n_ctx 8192` not 16384 (herd started before reload), then aborted as too slow |
| qwen3-4b via second ollama | qwen3:4b | invalid: goose openai provider stream decode error |
| qwen3-4b-ollama-2 | qwen3:4b | invalid: goose process had no `~/bin` on PATH; also used `sleep 600` (shell tool cuts at 300 s) |
| qwen3-4b-ollama-3 | qwen3:4b | invalid: `/dev/kvm` ACL had been reset, `up` failed at boot |
| **qwen3-4b-ollama-4** | qwen3:4b | **PASS 319 s**, 5 shell calls, polled at 60 s, harness confirmed VM + ssh |
| **gemma4-e4b-ollama-1** | gemma4:e4b | **PASS 318 s**, 7 shell calls, three 60 s polls, harness confirmed VM + ssh |

**What this does and does not show.** Both passes are *development* trials (`--keep-build`: the clone and the cargo build are reused, so the
10-minute build was not exercised), n = 1 each, with a runbook written once by hand and not tuned on their transcripts. They show the
chain works end to end once for two small local models. They do not show reliability: that needs the frozen protocol (N = 10, fresh
builds, held-out variant). Four of the six trials were lost to *environment* faults of mine, all visible in `up.log` the whole time;
the preflight exists because of them.

**Next:** a full clean run (no `--keep-build`) for each model; Infinity with the Qwen on Linux (static binaries from
`/root/infinity-no-aws` on Pop, run in a throwaway `HOME`); then Akuma on ryzen. Open: `check` prints `mke2fs -d: no` in
trial runs but `yes` by hand (the probe runs against `$CACHE`, which may not exist yet).

## Update 6 (2026-11-05): Infinity on Linux, a false pass caught, the Akuma backend (Claude)

**Infinity (the `no-aws` build) works on Linux with both small models.** Static binaries from the other agent's
`/root/infinity-no-aws` on Pop, run in a throwaway `HOME` (`/home/netoneko/demo/inf`) so a real `~/.infinity` is never touched.
`infinity -H '<task>'` hands the task to its daemon and returns at once; the transcript is the thread JSON the daemon writes.
Smoke tests: `qwen3:4b` answered `PONG`; `gemma4:e4b` called `run_command` first time and returned the output; `qwen3:4b` once
guessed a tool called `shell` (wrong), recovered through `shell_list_tools`, and once ended with `rig::providers::openai::completion::streaming:
There is no choice` (a stream chunk with no `choices`; seen once, not reproduced).
**Trap:** the daemon keeps its provider process, so a running daemon keeps the model and URL it was first started with; the harness
runs `infinity daemon stop` before each trial.

**A false pass, retracted.** The first Infinity trial reported PASS in 208 s. Its transcript shows the model ran `cat install.txt`, `check`,
a backgrounded `up` and a `sleep`, and **never ran the ssh step**; the harness ended it at the first "final message" and the VM was
already up because the backgrounded `up` had finished on its own. The judge only checked the end state. Two fixes in
`scripts/kvm/goose/qwen_install_trial.py`: an Infinity trial ends when the session goes idle with no clients, and a PASS now requires
the transcript to show the model itself ran `install.sh ssh ... busybox uname`. A second harness bug followed: a baseline taken from the old
daemon log was higher than anything the new log could reach (the log is per daemon start), so a trial that had already finished
waited for its budget. The log is now deleted after the daemon stops and counted from zero. Trials from before these fixes are renamed
`INVALID-*` on Pop.

| pair on Linux (Pop) | model | result |
|---|---|---|
| goose | qwen3:4b | PASS 319 s (dev) |
| goose | gemma4:e4b | PASS 318 s (dev) |
| infinity | qwen3:4b | **PASS 562 s** (dev; the model ran every step, claimed DONE, saw `AKUMA_READY`; harness confirmed VM + ssh) |
| infinity | gemma4:e4b | running |

All are `--keep-build` single trials. Still no scored result.

**The Akuma backend (`install.sh --vmm akuma`).** On Akuma the Linux `firecracker` binary has no network (no tap devices), so the only
ssh-reachable guest is Akuma's own hypervisor through herd (`vm = true`, `vm_type = kvm`, `vm_forward = <port>:22`), the path the
Taoxichuan dev guest uses. The backend writes the herd conf to both `available/` and `enabled/`, runs `herd disable` then `enable`
(herd only re-parses then), waits for a new service to be known (reload tick, ~20 s), starts it, and reaches the guest with Akuma's own
`ssh` (**no `-o` flags**: it reads them as a host name). It refuses to touch another VM (kvm0 allows one) and prints the command a human
runs. New options `--kernel-bin` and `--sshd-bin` skip the clone and the 10-minute build. `bootstrap/public/install.akuma.txt` is the
Akuma runbook. Not yet run on Akuma.
Akuma's disk now holds `/root/demo/{bins,inf,scripts}`; the Infinity paths were rewritten from Pop's.

**Blocked:** the Akuma host cannot reach the Mac's ollama (loopback only). A LAN forwarder on the Mac was refused by the permission
check, correctly: it would expose the user's ollama, unauthenticated, to the LAN and had not been approved by name. Order of
preference: a reverse ssh tunnel (loopback on Akuma only, needs Akuma's sshd to support remote forwarding, untested), then the
user's explicit approval of a short-lived forwarder, then trashcan's Qwen3-8B.

## Update 7 (2026-11-05): all four pairs pass on Linux; Akuma is blocked on two decisions (Claude)

**Linux, valid development trials (`--keep-build`, n = 1):** goose x qwen3:4b 319 s, goose x gemma4:e4b 318 s, infinity x qwen3:4b 562 s,
infinity x gemma4:e4b 253 s. In each the model ran every step itself and the harness confirmed the VM and the ssh line.

**On Akuma (ryzen booted with `arm.sh 29`; `0.0.8 fca1f9ed`):** the staged files are in place (`/root/demo/{bins,inf,scripts}`) and
`install.sh check --vmm akuma ...` passes. Findings:
1. **Akuma's ssh client does not read OpenSSH keys.** `-i` takes "Akuma's raw 32-byte format", takes no `-o` flags, and there is no
   `ssh-keygen` on the box. The Akuma backend now makes its own 32 random bytes and derives the authorized_keys line with an embedded
   RFC 8032 routine (checked against the RFC's test vector `9d61b19d... -> d75a9801...`); python3 is the one extra tool. The user's
   own `/root/.ssh/id_ed25519` is never read.
2. **Akuma's sshd refuses remote port forwarding** (`remote port forwarding failed for listen port`), so the loopback-only reverse tunnel
   used for Pop cannot give Akuma a model endpoint.
3. **kvm0 is exclusive and `vm-dev` holds it.** The script refuses to touch another VM; stopping `vm-dev` (the user's Taoxichuan dev guest)
   was refused by the permission check because it had not been approved by name.

**Needed from the user:** (a) permission to stop `vm-dev` for the test and restart it after; (b) a model endpoint for Akuma: approve a
short-lived forwarder that puts the Mac's ollama on its LAN address (unauthenticated while up), or accept trashcan's Qwen3-8B
(slow, no Gemma).

## Update 8 (2026-11-05): Akuma's ssh client now takes standard OpenSSH Ed25519 keys (Claude)

Asked by the user after the Akuma backend had to derive a key by hand. `ssh -i` on Akuma took only "Akuma's raw 32-byte format"; an
`ssh-keygen` key was rejected silently and the client fell back to a default identity, which looks like an unexplained authentication
failure. Now an identity file may also be an **unencrypted OpenSSH private key** (`openssh-key-v1`, Ed25519).
- **Parser** `akuma_ssh_crypto::keys::parse_openssh_private_key_ed25519` (pure, `no_std`, in the host-tested crate). It checks, and
  rejects the whole file otherwise: the magic; cipher and KDF both `none` (`Encrypted` is its own error, with the remedy); exactly one key;
  both check integers equal; the outer public blob, the private section and the key derived from the seed all agree; the trailing public
  half of the 64-byte secret; the 1,2,3,... padding; no trailing bytes.
- **Client** `userspace/sshd/src/client/keys.rs::try_load_key`: raw 32 bytes first, then the PEM; an OpenSSH file it recognises but cannot use
  says why on stderr before the fallback. Help text updated.
- **Tests:** 17 new host tests, 49 in the crate, all green: real `ssh-keygen` output (with and without a comment, a passphrase-protected one),
  a synthetic container builder that breaks exactly one field at a time, CRLF and surrounding whitespace, and every truncation of a real key
  (none accepted, none panic). Two older clippy warnings in the same file are not mine and were left.
- **On the ryzen** (new client at `/root/demo/bins/ssh`, the installed `/bin/ssh` is unchanged until the image is rebuilt), against `vm-dev`'s
  guest sshd on :2224, which authorises the box's key: raw key logs in; **the OpenSSH PEM of the same key logs in with no fallback message**;
  the old client rejects that PEM and silently falls back; the passphrase-protected key prints `... is passphrase-protected ...`; an RSA key
  prints `... is not an ssh-ed25519 key`. The PEM was built on the box by a script that printed nothing secret, and every test key, PEM and
  helper was deleted afterwards.
- **Not done:** passphrase-protected keys (bcrypt-pbkdf plus aes256-ctr), other key types, `ssh-keygen`-style certificates.
- **Follow-up:** `install.sh --vmm akuma` still derives its own key with an embedded RFC 8032 routine. With this client it could instead
  take any OpenSSH key made on another machine (`--key` plus the private file), and drop the python3 dependency; not changed yet.

## Update 9 (2026-11-05): `ssh-keygen` on Akuma, install.sh generates keys the usual way, a banner file (Claude)

User's scope calls: one key type (Ed25519), no passphrases, "if someone wants the full feature list they can use OpenSSH".
- **`ssh-keygen` is the `ssh` binary** (busybox-style multicall): `ssh --keygen ...`, or invoked as `ssh-keygen` (a symlink; `amd64/mkdisk.sh` now
  creates `/bin/ssh-keygen -> ssh`). `-t ed25519 -N '' [-C c] [-f path] [-q]` writes an OpenSSH-format key (0600) and `.pub`, refuses to
  overwrite, rejects other types and non-empty passphrases with a message; `-y` prints a key's public line. Tolerates a dropped empty `-N ''`.
  Code: `userspace/sshd/src/client/keygen.rs`; the container writer is `akuma_ssh_crypto::keys::encode_openssh_private_key_ed25519`
  (3 new tests, 52 in the crate). **Cross-checked against the real tool both ways:** OpenSSH's `ssh-keygen -y` reads a key written by the Rust
  encoder and returns exactly RFC 8032's public key; keys made by `ssh-keygen` on the ryzen and on trashcan are read by the Mac's `ssh-keygen -y`
  and give the same public key as the `.pub` Akuma wrote.
- **Installed** on trashcan and the ryzen: `/bin/ssh` replaced (sha256 `0da31159...`, 124,128 bytes, the old one kept as
  `/bin/ssh.orig-2026-11-05`), `/bin/ssh-keygen` linked. Verified there: both forms work, overwrite refused, `rsa` and `-N secret` refused.
- **`install.sh`:** it detects which `ssh` it has by asking `ssh -h` (Akuma's client names itself; **never `-V`**, which Akuma's client takes
  for a host name and tries to connect to) and calls Akuma's with only `-i`/`-p`. The key is now made with plain `ssh-keygen -t ed25519 -N ''`
  on every host; the python RFC 8032 routine from Update 6/7 is gone. `check` needs `ssh-keygen` (on Akuma that means the new `ssh`; the
  message says so). **Busybox:** run for real under the ryzen's `/bin/sh` (busybox ash): `check`, and `up` through fetch (sha256-verified
  download), key, image (`debugfs` recipe), then the boot stage refusing cleanly (exit 50) because `vm-dev` holds kvm0. Not yet run to a
  booted VM on Akuma.
- **Banner** (user request): the server banner gains `part of Akuma Bohemian Operating System` and the repo link under the welcome line.
  It is the **server's** banner (`sshd`), so a box shows it only after `sshd` is replaced. It now lives whole in `userspace/sshd/src/banner.txt`
  (art, dividers, text; open it to see the layout); `protocol.rs` just includes it. Output checked byte-identical to the old banner plus
  the two lines. The new `sshd` is staged at `/root/demo/bins/sshd` on the ryzen; the running `/bin/sshd` is untouched.
- **Docs:** `userspace/sshd/README.md` (Keys and `ssh-keygen`, scope), `docs/SSH_CLIENT.md` (the stale "raw only" note replaced).

## Update 10 — first VM booted by install.sh on Akuma itself (2026-11-05)

`vm-dev` stopped with the user's go-ahead; kvm0 free. `install.sh up --vmm akuma` on the ryzen (Akuma) now boots a Taoxichuan guest through herd's
`vm = true` and `install.sh ssh … busybox uname -a` prints `Linux taoxichuan 0.1.0-ae1ead4-taoxichuan … x86_64 GNU/Linux`. Three things stood in the way,
none of them the model's fault:

1. **Port 2222 is the host's own sshd.** `vm_forward = 2222:22` collided, so the "guest" login was the host rejecting the key
   (`publickey authentication failed`). On `--vmm akuma` the default port is now **2230** (`--port` still wins).
2. **Akuma's `ssh` asks yes/no for a new host key** and takes no `-o`. `install.sh` feeds `yes` on stdin for the Akuma flavor and deletes a stale
   `127.0.0.1:<port>` line from `~/.ssh/known_hosts` before waiting (a fresh VM is a fresh host key; the client refuses a changed one).
3. **The sshd must be the `linux-abi` build.** The default-ABI sshd spawns shells with Akuma's private `SPAWN` syscall (guest log `[nosys] nr=4397`),
   Taoxichuan has none, so every session fell to the built-in paws ("builtins only, no exec"; auth worked, `uname -a` "worked" because it is a paws
   builtin and printed `Akuma 0.1.0`). Proved by booting busybox as init in the same image (exec and the `sh` symlink fine), then by swapping in the
   older installed sshd (same failure), then by the linux-abi build (works). Pop's trials had the linux-abi sshd, which is why they passed.
   **`libakuma/linux-abi` is now on for every sshd build** (`userspace/sshd/Cargo.toml`, user decision "for alignment").

Also: `/dev/kvm` (Firecracker node) is present on this boot — Firecracker reached `CREATE_VM` and got `EBUSY` while a kvm0 VM existed; the herd
path uses `/dev/kvm0`. Not run further; Firecracker-on-Akuma has no network, so no ssh.
**GLM-4.7-flash smoke:** goose (native ollama provider, Mac ollama over LAN) ran `echo hello-from-glm` and answered DONE on the ryzen.
**Next:** Akuma trials (goose/infinity × qwen3:4b / gemma4:e4b / glm-4.7-flash) against this setup.

### Update 10b — ssh `-y`, no retry on an answer; first Akuma trial (GLM) was a model give-up

- Akuma `ssh`: `-y` / `SSH_ACCEPT_NEW_HOSTKEY=1` accepts a new host key; **declining (`no`), a changed key, a bad host-key signature and a refused login are
  now `ClientError::Fatal`: returned at once.** Before, every error went through the 10-attempt reconnect loop, so `no` re-asked the question up to ten
  times and a refused key retried five-plus times (seen: `publickey authentication failed … reconnecting in 10s (attempt 5/10)`). Checked on the ryzen:
  `echo no | ssh …` exits immediately; `-y` records and proceeds.
- Trial `glm-goose-1` (goose + glm-4.7-flash, Akuma host, 75 s): **not a pass, a model give-up.** `install.sh check` printed `qemu-system-x86_64: MISSING`
  and `firecracker: MISSING` (irrelevant for `--vmm akuma`), GLM read them as blockers and stopped ("missing tools: cargo, qemu, firecracker"). The
  check report now says `not needed for --vmm akuma` for tools the chosen path does not use. Rerun pending.

### Update 10c — GLM-4.7-flash on the Akuma host, goose vs Infinity (2026-11-05)

Host: ryzen booted into Akuma; VM = Taoxichuan guest booted by `install.sh up --vmm akuma` (herd `vm = true`, kvm0, forward 2230→22); model:
`glm-4.7-flash:latest` on the user's Mac ollama over the LAN; runbook `bootstrap/public/install.akuma.txt`; wrapper `scripts/kvm/goose/akuma_trial.sh`
(harness-judged: live VM + a real `ssh … uname` returning `Linux taoxichuan` + the model itself ran `install.sh ssh`).

| run | harness | verdict | wall | tool calls | tokens in/out/total |
|---|---|---|---|---|---|
| glm-goose-1 | goose | fail: read `MISSING qemu/firecracker` lines of `check` as blockers and stopped (fixed: check says "not needed for --vmm akuma") | 75 s | – | – |
| glm-goose-2 | goose | fail: hit the 40-turn cap, ran `ssh` by hand instead of `install.sh ssh`; `publickey authentication failed` (the same script booted and logged in cleanly on a fresh dir right after, so this was the run, not the script) | ~20 min | 37+ | not logged (`--no-session`) |
| glm-goose-3 | goose | model passed; harness mis-judged (`tail -1` hit a blank line) | 94 s | 5 | 24,503 / 485 / 24,988 |
| glm-goose-4 | goose | **PASS** | 109 s | 4 | 24,425 / 443 / 24,868 |
| glm-infinity-1 | infinity | model passed (5 tool calls, DONE + real line); harness mis-judged | 112 s | 5 | not recorded |
| glm-infinity-2/3/4 | infinity | **invalid**: the agent never sent a model request (0 `chat` spans; the thread went idle in the first second); wait loop then hung or fired at once | – | 0 | – |

Notes. (1) The harness bugs found and fixed: blank trailing line in the ssh output; the Infinity "ran the ssh step" check matched the runbook text echoed by
`cat`, now it reads `tool_call` commands from the thread JSON. (2) **Tokens:** goose records `accumulated_*_tokens` in its session db (needs a stored
session: use `--name`, not `--no-session`), about 24.9k per run, ~98% prompt re-sent each turn. Infinity's thread JSON and daemon log hold **no usage numbers**
in this build (its protocol has `token_usage`, nothing persisted); exact counts need a counting proxy between harness and ollama. (3) **Open:** after
the first Infinity run, later runs against the same daemon dir never reach the model; cause not found (daemon/provider restart handling suspected). `going idle` in
the daemon log is not a completion signal (it is also printed when a thread starts). (4) `vm-dev` was stopped with the user's go-ahead and is still halted;
the trial VM `akuma-install-2230` is running. Not run: the Linux/Pop Firecracker workload (needs the ryzen booted into Pop).

## Update 11 (2026-11-06): can a local model compose a fresh kernel? qwen3:4b no, glm-4.7-flash builds and boots one (Claude)

The experiment: give a local model one document, `crates/akuma-tokio-kernel/README.md`, and ask it to write a new city kernel from it
(`scripts/kvm/goose/kernelgen-brief.md`, driver `scripts/kvm/goose/kernelgen_trial.sh`). The new directory `{city}/` is the only thing it may add, plus one
`members` line. Five gates: build, minimal disk image, boot (long mode + `entering ring 3 -- /bin/sshd`, no `[Fault]`, its own ASCII banner), `ssh uname`,
`tokioprobe RESULT PASS`. Each trial: goose (`--max-turns 120`, `developer` builtin) against ollama on the Mac, 5,400 s budget, clean checkout from `git archive`,
on the ryzen while it is booted into Pop (no KVM for the model's own qemu: TCG only). The trials run one after another; the **harness**, not the model, judges
afterwards (build the produced crate in a clean copy, boot it under the real Firecracker on a throwaway dm-snapshot of the Akuma root).

| trial | model | city | shell calls | stopped by | produced | build | boots |
|---|---|---|---|---|---|---|---|
| Nagoya | qwen3:4b | `nagoya` | 14 | budget (exit 124) | `main.rs` of 2 lines: `println!("{}");` (malformed), no dependency, no `no_std`; also **rewrote the root `Cargo.toml`** (twice), which breaks the whole workspace | no (workspace fails to load: `workspace.lints` not defined) | - |
| Kyoto | glm-4.7-flash | `kyoto` | 70 | budget (exit 124) | `kyoto/{Cargo.toml, build.rs, src/main.rs, src/banner.txt}` + one `members` line | **yes**, 8 s | **yes** |

**qwen3:4b failed outright.** It never reached gate 1 in a form that could pass: the file it wrote is not a kernel, and its edits to the root manifest broke
every `cargo` command in the tree, which it then spent its remaining turns trying to diagnose. No commit, no `RESULT.md`. The size of the model is the likely
reason (the README is long and the dependency on `akuma-tokio-kernel`'s builder API is the whole task), but this run does not prove it: one trial, one prompt.

**glm-4.7-flash got a working kernel.** `kyoto/src/main.rs` is ten lines: `akuma_tokio_kernel::entry!(main)` and a `Kernel` builder chain
(`.name("Kyoto").banner(include_str!("banner.txt")).console().disk().net().smp().run_init()`), the same composition the README describes for Shanghai. The
`build.rs` is the README's linker-script hand-off, with the aarch64 fallback it also describes. Harness result: `cargo build -p kyoto --target
x86_64-unknown-none --release -j4` finishes (the shared crates were already built), and under Firecracker (1 vCPU, root = snapshot of the Akuma root partition,
`init=/bin/sshd`) the console shows `Kyoto/amd64 -- long mode reached`, `Kyoto 0.1.0 (akuma-tokio-kernel, amd64)`, `Kyoto: init spawn -- init=/bin/sshd`,
`Kyoto: entering ring 3 -- /bin/sshd` and `[SSHD] Listening on 0.0.0.0:22...`, with no `[Fault]` or panic line. That is gates 1 and 3 of the brief.

What glm did **not** do, and the model stopped on: it never committed (`commits.txt` is only `base`), never wrote `RESULT.md`, and its last 15 minutes of output
are a hunt for a disk image: `ls taoxichuan/mkdisk.sh` (no such file), `find -name "*.img"`, `grep kyoto|sshd|disk.img`. So **gate 2 (the "smallest disk"
steps) is where it ran out**, which makes the README's disk section the first thing to check for a missing or misnamed script before blaming the model. Gates
2, 4 and 5 were therefore not exercised by either trial.

Reading the pair: the composition API is small enough that a 30B-class local model can use it from the README alone (it wrote the right ten lines without
copying `shanghai/`), and a 4B cannot; the part that needs real multi-step shell work (disk image, ssh, a tokio probe) is still beyond what either did in
90 minutes on TCG. Caveats, so nobody over-reads the table: one trial per model, the brief names the API and the sample kernel, the base tree for glm was the
tree at the time (`base.tar`, older than the unsafe-reduction work of the same day), and TCG slowness is part of the cost.

### Configuration of the two trials (read off the rig, 2026-11-06)

| | glm-4.7-flash (Kyoto) | qwen3:4b (Nagoya) |
|---|---|---|
| model | `glm-4.7-flash:latest`, **29.9 B** parameters, MoE (`glm4moelite`), GGUF **Q4_K_M**, ~21 GB resident | `qwen3:4b`, **4.0 B** dense, GGUF **Q4_K_M** |
| capabilities (ollama) | completion, **tools**, thinking | completion, tools, thinking |
| context | declared 202,752; **loaded at 32,768** (`/api/ps`, the ollama default, not raised) | declared 262,144; loaded size not observed (not resident when checked) |
| sampling | the model's own defaults: temperature 1, top_p 0.95, min_p 0.01, repeat_penalty 1; nothing overridden by the harness | model defaults |
| where it ran | ollama on the Mac over the LAN, no cloud | same |
| harness | **goose 1.52.0**, `goose run --with-builtin developer -i BRIEF --name kg-<label> --max-turns 120`, provider `ollama`, `GOOSE_MODE=auto` (no confirmation prompts), telemetry off, keyring disabled | same |
| isolation | per-trial `XDG_CONFIG_HOME` / `XDG_DATA_HOME` under `results/<label>/`, clean checkout `wt/`, `CARGO_TARGET_DIR` shared | same |
| budget | 5,400 s `timeout` | same |
| tool calls (goose log) | 70 `shell`, 1 `edit`, 4 `write` | 14 `shell`, 6 `edit`, 26 `write` |
| harness noise | 2 x `-32600: Tool 'read' was not advertised for this model turn` (goose asked for a tool the turn did not list; non-fatal, it fell back to `shell cat`) | none |

Two differences in *behaviour* are visible in the call mix and are worth more than the headline pass/fail. glm explored: 70 shell commands, reading the README
and the tree, then writing five files. qwen wrote: 26 file writes against 14 shell commands, i.e. it produced a lot of text and checked little of it, which is
how a workspace manifest ends up overwritten twice. The one harness defect seen (the unadvertised `read` tool) did not affect either result; goose's builtin
`developer` extension is a shell plus an editor, so `cat` is a complete substitute.

For comparison, the sample kernel `shanghai/` was built **end to end by `glm-5.3-flash`** (a hosted model, run through goose) from an earlier revision of the
same README, on the ryzen booted into Akuma, and reached gate 6 (goose running inside the guest): `shanghai/docs/EXPERIMENT_RESULT.md`. The local
glm-4.7-flash (30 B, Q4, 32 k context) got through build and boot (gates 1 and 3) before the budget ran out and then spiralled on the disk image. Not directly
comparable (different model generation, hosted vs local, different revision of the brief, TCG instead of the ryzen's KVM), but the same harness family and the
same README.

### glm's 75 tool calls, split at the working kernel (Kyoto, from `goose.out`)

**Calls 1-13: the kernel (13 calls, 8 shell + 4 write + 1 edit).** This is the whole of what produced a bootable kernel.

| calls | what |
|---|---|
| 1 | `cat crates/akuma-tokio-kernel/README.md` (the only input it read) |
| 2-6 | `mkdir kyoto/src`; write `Cargo.toml`, `banner.txt`, `build.rs`, `main.rs` |
| 7-11 | five calls to find the `members` line of the root `Cargo.toml` (`grep`, `cat`, `grep -A5`, `head`, `sed`); the first one hit the unadvertised-`read` error, the rest re-read the same line |
| 12 | one `edit`: add `"kyoto"` to `members` (a one-line change, unlike qwen's rewrites of the whole file) |
| 13 | `cargo build -p kyoto --target x86_64-unknown-none --release -j4`: **Finished in 12 s**. Gate 1 passes; the model says so and moves on |

No call in this stretch failed except the tool-name error. It never opened `shanghai/`; the README was enough.

**Calls 14-75: the spiral (62 calls, all shell, no file written to the tree).** Gate 2 (disk image) was never completed. In order:

| calls | what it tried | why it went nowhere |
|---|---|---|
| 14-25 | its own recipe: `mke2fs` + a `debugfs -f` command file + `curl` busybox from busybox.net; a path from the brief's Mac-flavoured text (`/opt/homebrew/.../mke2fs`) failed first, then `/usr/sbin/mke2fs` worked | `debugfs` could not `write` the `sshd` binary because it did not exist at the path in the command file |
| 15, 26-38 | build `sshd` (`cargo build -p sshd --target x86_64-unknown-none`: "Finished" in 3.5 s) and then **look for the output**: `find`, `ls userspace/target/...` x9 | the binary was never found where it looked; `userspace/` is its own workspace and its target dir is not the one the model searched; it kept re-running the build and re-searching instead of reading `cargo`'s output path |
| 39-52 | switch to `--target aarch64-unknown-none` and `userspace/build.sh` (`rustup target add aarch64-unknown-none`, a full `build.sh`) | **wrong architecture for an amd64 kernel**; the aarch64 build also needs `aarch64-linux-musl-gcc` for its C probes (not installed) |
| 53-58 | find `amd64/mkdisk.sh` (a real disk builder) and read it | right idea, found on the 54th call |
| 59-63 | run `mkdisk.sh`: `hello.elf not found`, so build `akuma-amd64`, which fails on the missing `spleen` font submodule (`base.tar` has no submodules) | the dependency was for a different kernel; `git submodule update` cannot work in a `git archive` checkout |
| 64-75 | back to `ls`/`find` for `*.img`, `taoxichuan/mkdisk.sh` (does not exist, tried twice), `bootstrap/bin/sshd` (does not exist, tried three times) | the same three dead ends repeated; goose's `--max-turns 120` was never reached, the 5,400 s budget was |

What the spiral is made of: **three** distinct dead ends (find the built sshd, an aarch64 detour, a submodule that cannot exist in the checkout), each re-tried
several times after it had failed (`taoxichuan/mkdisk.sh` twice, `bootstrap/bin` three times, the sshd `find` over a dozen). There is no sign of a plan being
revised: after each failure it ran a nearly identical command. The README's section the brief points at ("smallest disk") is the thing to verify; three of the
failures (where the sshd lands, which `mkdisk.sh` to use, the submodule) are exactly what a README for this task has to state.

### Context for the result, and what is notable about Kyoto

**The model is old and the context was small.** glm-4.7-flash is an earlier generation than the `glm-5.3-flash` that built Shanghai, and it ran at a **32 k
context** (ollama's default; the model declares 202 k). The 5.2/5.3 flash models do not fit in the Mac's RAM, so the local trials are limited to what fits
(~30 B at Q4, ~21 GB resident for this one); the hosted comparison is not reproducible locally. A small, older model, at a quarter-megatoken-class context cap,
still read the README once and wrote a kernel that builds and boots. That is a good result for the size, whatever happened after gate 1.

**Kyoto against Shanghai and Taoxichuan.**
* `kyoto/src/main.rs` is the **same builder chain as Taoxichuan's, call for call** (`.name .banner .console .disk .net .smp .run_init`), with no
  `middleware` feature arm like Shanghai's. The model took the minimal composition and added nothing; there is nothing in it to be wrong.
* `build.rs` has both arms of the README's recipe, and one more than `shanghai/build.rs`: the aarch64 branch also passes
  `KERNEL_PHYS_BASE_OVERRIDE=0x80300000` under `CARGO_FEATURE_PLATFORM_FIRECRACKER` (the README does not mention it: `grep` finds no such line there, so the model
  got it from the tree while reading around, or from memory of the Firecracker path; the aarch64 build was not tried).
* The banner is the **weak spot**. The brief asks for a small skyline; the file is six lines of figlet-style lettering whose glyphs are cut off (it reads as
  the tail of "...ey", not "Kyoto"). It prints and does not break anything; it does not meet the brief. Shanghai's is a 19-line skyline.
* `Cargo.toml` is the shape the README gives (workspace lints, one dependency with `no-tests`), no features.
* Footprint: five files, one `members` line. No commit, no `RESULT.md` (both required by the brief), so it did not follow the reporting half of the task
  even though it did the building half.

Trial three, **gemma4:e4b** (`sapporo`), was started after these two and is not part of this update; append its row here when it finishes.

Housekeeping found on the way: a trial's `wt/` checkout is shared, so judge a finished trial from its own `results/<label>/<city>.tar` extracted into a clean
copy of `base.tar` (that is what was done), never from `wt/` while the next trial is running.
