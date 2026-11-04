# Autonomous agents for `proposals/INSTALL_SCRIPT.md`: setup log (2026-11-04)

**Status: in progress. This records what was set up and what was observed, not results.** The Qwen endpoint is not
verified; the ryzen goose server does not exist yet.

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
