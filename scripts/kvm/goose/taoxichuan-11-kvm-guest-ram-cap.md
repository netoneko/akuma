# Taoxichuan stage 11, session X: lift the 1 GiB guest-RAM cap of the host's KVM node (target 4 GiB)

You are goose-glm on the ryzen box. Previous session: the extraction-plan brief (`RESULT-X10.md`). **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
The host Akuma kernel refuses any VM with more than 1 GiB of RAM, and the Taoxichuan self-hosting fixed point needs >= 2.5 GiB (it needs 3 GiB; 4 GiB is the target). The cap is
structural, not a setting: `docs/reference/subsystems/kvm.md` ("Reserved memory") says guest RAM is bounded at 1 GiB "by the nested page table (one directory of 2 MiB leaves)", and the
number is enforced in three places: `MAX_RAM_MIB = 1024` in `crates/akuma-vm-spec/src/spec.rs`, `userspace/herd/src/vm.rs` (parses through the spec; its test expects `vm_ram_mib=4096` to be `OutOfRange`), and the
NPT builder in `amd64/src/kvm*.rs` / `crates/akuma-svm-amd64` plus the `SET_USER_MEMORY_REGION` path in the `/dev/kvm` node ("Guest RAM <= 1 GiB"). **Your job: make guest RAM up to 4096 MiB work, in code, with host tests,
and write down exactly what the supervisor must build and boot.** The supervisor builds the metal kernel **on the ryzen** (Mac-built kernels hang on metal), commits, reboots and tests; you do none of that.
You change no file under `taoxichuan/`, and you never touch `/boot`, `/etc/herd/` or a running VM.

## Read first (in order; `sed -n` the named sections, never whole files)
1. `docs/reference/subsystems/kvm.md` - "Reserved memory (`kvmmem=`)", "Guest RAM is a slice of the `kvmmem=` reserve", the `vm_ram_mib` row of the herd table, "Kernel parameters".
2. `crates/akuma-svm-amd64/src/` - the NPT encodings (`grep -n 'npt\|Npt\|NPT\|2 MiB\|PDPT\|pd' ` first). Find where the single directory page of 2 MiB leaves is built and its size.
3. `crates/akuma-vm-spec/src/{spec.rs,arena.rs}` - `MAX_RAM_MIB`, and the first-fit arena over the `kvmmem=` reserve (check it is not hard-wired to 1 GiB either).
4. `amd64/src/kvm.rs`, `amd64/src/kvm/` - `grep -n 'npt\|NPT\|ram\|RAM\|1 << 30\|0x4000_0000' amd64/src/kvm*.rs amd64/src/kvm/*.rs`: every place that assumes guest-physical space ends at 1 GiB (the NPT root, `SET_USER_MEMORY_REGION`, the MMIO window: where do the LAPIC/IOAPIC/virtio-mmio/UART windows sit? RAM above 3 GiB must not overlap them, and RAM may have to be split around the PCI hole as a real PC does; say which you chose and why).
5. `crates/akuma-svm-amd64/` PVH loader / `e820` map: the guest must be told about all of its RAM (and the hole), or it will use only the first 1 GiB.
6. `userspace/herd/src/vm.rs` (docs table + tests), `docs/reference/subsystems/kvm.md` herd row.

## Premises (examine these before you edit anything; write one line per row in RESULT-X.md: HOLDS / WRONG (what you saw) / UNCHECKED)
| # | premise | read-only check |
|---|---|---|
| 1 | `MAX_RAM_MIB` is 1024 | `grep -n MAX_RAM_MIB crates/akuma-vm-spec/src/spec.rs` |
| 2 | the NPT is one directory page of 2 MiB leaves | `grep -n` in `crates/akuma-svm-amd64/src/` for the directory/leaf builder |
| 3 | the ryzen's boot entry already reserves >= 4096 MiB (`kvmmem=`) | `grep -rn kvmmem overlays/ryzen/ docs/runbooks/ scripts/` (the entry-30 line); the supervisor will confirm on the box |
| 4 | nothing else in `amd64/src/kvm*` assumes <= 1 GiB | the grep in step 4 above |
| 5 | host tests for the crates run on the Mac/box | `cargo test -p akuma-vm-spec -p akuma-svm-amd64 --target $(rustc -vV \| sed -n 's/host: //p')` |

## Composition rule
Extend the existing crates; do not add a second NPT builder. The NPT encoding and the RAM layout (which guest-physical ranges are RAM, which are holes) are *decisions*: they belong in `akuma-svm-amd64` / `akuma-vm-spec` as pure, host-tested functions
(`forbid(unsafe_code)` stays); `amd64/src/kvm*.rs` keeps the page allocation and the `unsafe`. If the NPT grows from one directory page to several, allocate them from the same place the single one comes from and free them with it. Report per change: *pulled / extracted / written new (why)*.

## Gates
- G1: host tests pass for `akuma-vm-spec` and `akuma-svm-amd64`, including NEW tests: the NPT maps every 2 MiB leaf of a 4096 MiB layout exactly once, nothing maps inside the device windows, the e820/PVH memory map for 4096 MiB sums to the RAM size and names the hole, `ram_mib=4096` parses and `ram_mib=4097`/`ram_mib=0` are refused (`MAX_RAM_MIB` is the single source; herd's test uses it, not a literal).
- G2: `userspace/herd` host tests pass (`cd userspace && cargo test -p herd --lib --no-default-features --target $HOST` if that is its recipe; read `docs/reference/userspace-layout.md`) with the old `4096 -> OutOfRange` case moved to `MAX_RAM_MIB + 1`.
- G3: `cargo check --release --target x86_64-unknown-none` for `amd64/` is clean (the metal kernel itself is built by the supervisor on the ryzen), and `python3 scripts/check_unsafe_budget.py` does not grow (or you ran it with `--update` and say why in RESULT).
- G4: `docs/reference/subsystems/kvm.md` says the new bound and why (current behaviour only; the incident story goes in a new `docs/archive/KVM_GUEST_RAM_CAP_LIFTED.md`); `docs/archive/BUG_FIX_LIST.md` per `docs/runbooks/update-bug-fix-list.md` if the runbook calls for a row.
- Out of scope: SMP changes, `taoxichuan/`, any boot or VM, the epoll work, raising the cap past 4096.

## Ops traps
- **Never run `python3 -`, `python3 <<EOF` or python from stdin on this box** (REPL hang). Write a file, run `python3 /path/x.py < /dev/null`.
- Never write `/boot/akuma-amd64*`, never touch `/etc/herd/`. Never glob the repo root.
- Gate amd64-only code with `cfg`; amd64-only crates stay out of `default-members` (this broke the aarch64 build twice).
- Allocation rule (`CLAUDE.md` § Kernel conventions): a larger NPT must not add a heap allocation on the fault path; use the existing page source and say what the extra pages cost (4 GiB of 2 MiB leaves = 4 directory pages, 32 KiB).

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` at the first green host test and after every crate you finish. WIP commits fine. Never push `origin`.

## Budget
About 70 tool calls; at 35 without a commit the supervisor nudges you. Past budget with the gates open: commit, write RESULT, stop.

## When to stop
G1-G4 hold, or 70 calls, or the quota error. Write `/root/dev-cycle/taoxichuan-s8/RESULT-X11.md`: premise lines, the diff summary, and a **"Supervisor: build and boot"** section with the exact steps (what to build on the ryzen, the `kvmmem=` value the boot entry needs, the herd conf to use for a 3072 MiB guest with 4 vCPUs, and what the console prints when the guest sees all its RAM: e.g. the guest's `free -m`). Then end the turn.
