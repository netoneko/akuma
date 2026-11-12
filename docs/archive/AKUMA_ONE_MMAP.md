# One `mmap` for both kernels

*2026-11-12, branch `akuma-multiarch-attempt-0`. Step 5d of
[`proposals/AKUMA_MM_CONVERGENCE.md`](../../proposals/AKUMA_MM_CONVERGENCE.md).*

## What changed

amd64 dispatches `mmap` to glue's `sys_mmap`. It keeps only what is its own device:
`/dev/fb0` (`mm::mmap_framebuffer`, write-combining) and the `/dev/kvm` census, both
in its dispatcher ahead of the glue call.

Deleted from amd64:
- `sys_mmap` (~450 lines);
- the file-descriptor probes only it used (`file_identity`, `file_bytes_at`,
  `is_regular_file`, `write_mode_refusal`, `path_is_dir`);
- `populate_file_page`;
- the `[MMT]` region trace and the `[mmap-t]` timing print;
- the `mm-forensics` feature, which checked amd64's own placer.

amd64's fault path, readahead (16 pages), eager fill and shared writable table
(`shmpages`) are unchanged.

**Glue's regions work on amd64's fault path as they are.** `mm::fault_in` asks a region
three things: its recorded protection, its `file`, and its `shared_write`. Glue's lazy
regions answer all three; the `lazy` marker is never read.

**One new hook, `shared_write_backing(fd, offset)`.** A writable `MAP_SHARED` file
mapping is kept coherent either by glue's write-back records (AArch64 answers
`SharedWrite::Glue`) or by the kernel's shared writable table. In the second case the
kernel returns the `SharedWriteBack` record the region must carry (amd64,
`SharedWrite::Table`). Step 6 moves the table into `akuma-fpcache-rw` and deletes the
hook.

**Per-kernel kernel hole.** `ProcessMemory::kernel_hole()` is the RAM identity map on
AArch64 and empty on amd64. Both placement and the `MAP_FIXED` guard read it. Before
this, the guard would have refused amd64's `ld.so` load address (1 GiB), and the placer
would have skipped `[1 GiB, kernel_va_end())` for nothing.

## Divergences, and what both kernels do now

| | AArch64 before | amd64 before | now |
|---|---|---|---|
| `MAP_FIXED_NOREPLACE` over a mapping | mapped over it (divergence 6) | `EEXIST` | `EEXIST`, checked against the regions and the page table, with a reservation |
| `MAP_FIXED` + `MAP_FIXED_NOREPLACE` | fixed | fixed | no-replace (Linux) |
| `MAP_FIXED` at `addr == 0` | treated as a hint | `EINVAL` | `EINVAL` |
| `MAP_FIXED` past the user half | walked | `EINVAL` | `EINVAL` |
| file mapping at an unaligned offset | mapped | `EINVAL` | `EINVAL` |
| file mapping of a socket/pipe/dir/`/dev` node | zero-filled anonymous memory | `EACCES` | `EACCES` (`/dev/zero` still maps as anonymous) |
| writable `MAP_SHARED` of an `O_RDONLY` fd | mapped, writes dropped at write-back | `EACCES` | `EACCES` |
| large `MAP_SHARED\|MAP_ANONYMOUS` | lazy, and the `shared_anon` marker was dropped, so fork copied it | eager | eager (lazy fallback on OOM) |
| eager file fill | `read_at(path)` | by inode | by inode (`read_at_open_file`) |
| `len` bound | user half | the 112 TiB placement window | user half |
| fd-dependent refusals with no process | `ESRCH` | `EACCES` | `ESRCH` (the fd table is the process's) |

amd64's placement window moves from its own `[4 GiB, 112 TiB)` to
`ProcessMemory`'s `[max(image end + 256 MiB, 4 GiB), stack bottom - 1 MiB)`.
`mmap_framebuffer` and `mmap_frames` (KVM) still place in the old window through the
shared placer and cursor.

## Verification

- clippy, all nine configurations; host tests.
- AArch64 HVF boot suite SMP 1/2/4: the same counts as 5c.
- amd64 TCG suite SMP 1/2/4: 822/832/832, 0 failed. Three smoke checks changed
  expectation: the length bound is now the user half, and the two fd-dependent
  refusals answer `ESRCH` with no process.
- `mem_suite` 13/13 on the amd64 guest (SMP=2) and the AArch64 devbox (SMP=4).
- amd64 guest: `shmwrite` 13/13 rungs, `shmcoh`, `shmshare` 8/8, `shmvar`,
  `shmregionprobe`, `mapnoreplace`, `altstackexec`, all pass.
- AArch64 devbox: `mapnoreplace` FAILed on the 5c kernel ("over the stack" was mapped)
  and passes now. `shmcoh`, `shmshare` (4/8), `shmwrite` (11 failing), `shmvar` and
  `shmregionprobe` give byte-identical logs on the 5c and 5d kernels. Those failures
  are AArch64's lack of a coherent shared file page, which is step 6.
- In-VM AArch64 self-host `-j4`: EXIT=0 in 1m02s, no tripwires.
- Production lines: −306 against 5c, −1 798 against `main`.

## Open

- **The amd64 self-host lane has not run on this change.** It is the gate the proposal
  names for steps 5–6, and the placement-window move is the part it would catch.
- `mremap`, `madvise`, `msync`, `brk` are still amd64's own (5e).
