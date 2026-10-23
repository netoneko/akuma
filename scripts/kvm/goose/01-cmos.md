# Task 01: find and fix what corrupts the machine's CMOS (firmware reports a bad clock / checksum)

Read `docs/archive/AKUMA_KVM_DEV_CYCLE.md` (your manual: the loop, the rules, the hosts) and `docs/archive/AKUMA_FIRECRACKER_KVM.md`
only if you want background. You work inside Akuma on the ryzen, on branch `cmos-fix`: start with `cd /src/github.com/netoneko/akuma && git checkout cmos-fix && git merge --ff-only dev-base`
(create it from `dev-base` if it does not exist). `dev-base` is where the laptop puts the current source.

## The problem
After Akuma has run on this laptop, the next boot stops in FIRMWARE (before any OS) on a clock / CMOS error and asks for F1 to
continue. Linux (Pop) then also complains about the clock. Something Akuma does leaves the CMOS (the 128 bytes behind ports
0x70/0x71, with the RTC in the first 14) in a state the firmware's checksum rejects. Nobody has found the cause yet.

## Suspects the owner already has (look at each, with evidence from the code, then say yes/no/maybe)
1. `write_rtc_from_sntp` in `amd64/src/clock.rs` and `crates/akuma-cmos-rtc/src/lib.rs` `write()`: it sets SET (status B bit 7),
   writes the date registers and the century (0x32), clears SET. A reset between "set" and "clear" leaves the RTC halted with SET=1.
2. `crumb()` in `amd64/src/kvm.rs`: writes a step number into CMOS byte **0x7F** during the SVM rehearsal and `kvmtest` paths. Byte
   0x7F is inside the range many firmwares checksum. Find every caller and when it runs (is it on every boot with `kvmctl`?).
3. The reboot itself: `reboot -f` (the `sc-reboot` syscall, `crates/akuma-boot`) can land in the middle of a CMOS access (index
   written, data not yet; or the SET window above).

## Things nobody has looked at (check these too; the owner wants to know about any other cause)
* The classic CMOS checksum covers bytes 0x10-0x2D and is stored in 0x2E-0x2F; byte 0x0E is the diagnostic status (bit 6 = bad
  checksum, bit 7 = power lost), byte 0x0F is the shutdown status the firmware reads after a reset. Does any Akuma write touch
  those, or does the reset path leave 0x0F in a state the firmware misreads?
* `kvm.rs` line ~484: the static `IOPM` for the rehearsal guests intercepts ONLY port 0x80, so those guests run port I/O natively on
  the real chip. The Linux/Akuma guests use `IOPM_ALL` (`kvm/linux.rs`), which intercepts everything; confirm that EVERY VMCB path
  (kvmtest guests, kvm0 guests, the `/dev/kvm` node in `amd64/src/kvm/api/`) uses the all-intercept map, and that the guest's port
  0x70/0x71 handling (`kvm/linux.rs` ~1130-1180) never forwards a write to the real ports. A guest Akuma kernel runs the SAME
  `clock.rs`, so a leaked CMOS write from a guest would hit the host chip.
* Anything else that writes ports 0x70-0x73 or the RTC registers: `grep -rn "CMOS\|0x70\|0x71\|0x72\|0x73" amd64 crates --include=*.rs`.
* Whether the century register, the 12/24h and BCD/binary format bits, or leaving status B / C / D different from what the
  firmware set could be seen as corruption.
* NMI-disable bit (index port bit 7) and the index latch left pointing at a register.

## What to build
A minimal, safe fix plus the evidence for it. Prefer removing a risky write over making it clever (for example: never write the
chip from SNTP while SET could be left set; or write it only once the machine is quiescent; move breadcrumbs somewhere that is not
CMOS). Keep pure logic in `crates/akuma-cmos-rtc` with host tests (`cargo test -p akuma-cmos-rtc` works on Akuma with
`. /etc/akuma-dev.env`; if host tests cannot run there, say so). Follow the repo rules in `CLAUDE.md` (no new `unsafe`, no
allocation on console paths, `safe_print!`/`tprint!` only).

## How you test (the loop, from the manual)
1. `kbuild --features no-tests`; copy the kernel to `/boot/akuma-guest-dev`; run it as the guest through herd (`vm-dev`: `herd enable vm-dev; herd start vm-dev`, and when done `herd stop vm-dev; herd disable vm-dev`; see task 00 and the manual).
   It must boot to `AKUMA-GUEST-DONE` with no `EXCEPTION`.
2. Install the same build as the HOST kernel for the next boot ONLY via `kinstall /root/ktarget/x86_64-unknown-none/release/akuma-amd64 /boot/akuma-amd64.verify`
   (never the default `/boot/akuma-amd64`).
3. `git add -A && git commit` on `cmos-fix` (message: what and why), then write `/root/dev-cycle/READY-FOR-REBOOT` containing the
   commit hash and a five-line summary of your change and of your reasoning about each suspect. STOP and wait. Do not reboot.
4. The orchestrator (outside Akuma) performs a CLEAN reboot test and writes `/root/dev-cycle/reboot-report.txt`
   (Pop returns, CMOS checksum verdict, Akuma dmesg). When you are resumed, read it. PASS plus an honest explanation of the
   mechanism is the finish line; FAIL means iterate.

## Rules
No Firecracker. One `/dev/kvm0` VM at a time (`herd stop` the others first, restore `vm-serve-mc` afterwards). Never `pkill`; kill
by pid. Never touch `/boot/akuma-amd64`, `/boot/akuma-guest`, p3 partitions, or the bootloader. Do not push anywhere; commits stay on
`cmos-fix`. If something is ambiguous, write the question in `/root/dev-cycle/QUESTIONS.md` and continue with the safest option.
