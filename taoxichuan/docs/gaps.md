# Gaps — what the kernel does not do yet

The ladder (`README.md` § Status) is done through the fixed point; what is left is below. Each open entry names
what shows it. Rewritten 2026-11-02 against the tree after the cleanup plan's B11; the gap list this replaced
(no signals, no init process, no linker, no epoll, …) is in git history, and every one of those is served now.

## Open

| # | gap | what shows it |
|---|---|---|
| G2 | **Rare boot stall**: a boot that stops before the banner with no `[Fault]`. Not root-caused; not seen in the cleanup's TCG gate boots (2026-11-01..02, every gate boots fresh) | a boot that never prints the banner |
| G7 | **The guest sshd corrupts binary stdin and truncates large stdout** (not re-measured since 2026-10-30) | `gzip -c x \| ssh guest 'tar xz'` → `gzip: invalid magic`; pull binaries in `sed -n 'a,bp'` chunks and check the md5 (`TAOXICHUAN_FIXED_POINT.md` § pitfalls) |
| G8 | **`-j8` in-guest build fails; `-j4` is clean** (not re-measured since 2026-10-30) | 3 GiB: `mmap` ENOMEM, frames exhausted; 8 GiB: `#GP` in the dynamic linker, unexplained (`TAOXICHUAN_BUGS_FOUND_2026_10_30.md` §2) |
| G10 | **`mmap` is eager**: every page is populated at the call, anonymous and file-backed alike, and fork is the only copy-on-write. Building this kernel in the guest peaked at ≥ 2,460 MiB against Linux's 447 MiB, because each `rustc` pays for its whole 311 MB `librustc_driver` mapping | the in-guest build's peak; `archive/TAOXICHUAN_EFFICIENCY_PLAN_GLM.md` (top item) |
| G11 | **`mmap`'s `PROT_READ` and `PROT_NONE` are not enforced**: a mapping without `PROT_EXEC` is mapped user read-write (only a later `mprotect(PROT_READ)` makes it read-only), and `mprotect(PROT_NONE)` is a no-op, so a guard page does not fault | a write to a fresh `PROT_READ` mapping succeeds |
| G12 | **`sendfile` and `copy_file_range` answer `ENOSYS`**; busybox and Rust's `fs::copy` fall back to `read`/`write` | `[nosys] nr=40` in every battery run |
| G13 | **A full process table panics** (`register_process` is infallible) instead of answering `EAGAIN` | `MAX_PROCESSES` live processes; `docs/reference/subsystems/memory.md` OPEN item |
| G14 | **Inode times are 0**: the ext2 root's clock (`no_clock`) answers 0, so `utimensat` and every write stamp nothing | `ls -l` |

## Pinned exclusions (deliberate, load-bearing — not bugs)

| exclusion | why | what lifts it |
|---|---|---|
| One big kernel lock | `smp=N` runs N cores under one lock ([`reference/scheduling.md`](reference/scheduling.md) § SMP); only ring 3 runs in parallel. The rig's default is `smp=1`: the in-guest `-j4` build measured 1.8x *slower* on 4 CPUs on 2026-10-31 (`docs/archive/TAOXICHUAN_SMP.md` § Measurements) | finer-grained locking |
| W^X is enforced (stricter than Linux) | `W+X` refused; `PROT_EXEC` refused on any page written since mapping; `wx=0` restores the old widening. No JIT can run | `archive/TAOXICHUAN_WX_POLICY.md` |
| No containers | one box, the host: `akuma-isolation`'s `containers` feature is off ([`docs/reference/subsystems/containers.md`](../../docs/reference/subsystems/containers.md)) | a box syscall |
| No Akuma-native syscalls | glue's `akuma-native` feature is off: `SPAWN`, `WAITPID`, the terminal calls and the rest answer `ENOSYS`, so an image needs the `linux-abi` sshd | — |
| No pty, no users | everything runs as root on the serial console | a stage that needs them |
| No console input | the console is write-only; a read of its stdin is EOF | an input device and a reader |
| No PCI | Firecracker-style virtio-mmio is the pinned machine | a second machine |
| Identity map kept | the lower-half handoff has no customer | never required; task roots already lack it |
