# Replayable `mmaplazy` traces

This pilot applies the method from Inanna Malick’s [Property Testing with Agent Swarms](https://recursion.wtf/posts/agents-and-property-tests/) to Akuma’s existing [`mmaplazy` probe](../../userspace/forktest/c_stress/mmaplazy.c). The article’s advice to generate interacting operations, deliberately break an invariant, and shrink the failure shaped the experiment.

## What ran

Each trace starts with a private, read-only mapping one page into the probe’s patterned file. It checks an early page, protects an interior range, unmaps across the protection boundary, restores the surviving part, and checks pages that had yet to be faulted in. The last page covers the partial end of the file and its zero-filled tail. Seeds vary the ranges and which pages are read. The probe checks every byte of each selected page against the pattern at its absolute file offset.

[`mmaplazy_trace.py`](../../scripts/mmaplazy_trace.py) runs each concrete trace on Linux and optionally ships the same static binary to Akuma through the existing [`mem_suite.py`](../../scripts/mem_suite.py) SSH helpers. It prints the full trace on a failure. On a byte mismatch it tries deleting one operation at a time and keeps deletions that preserve the exact mismatch. For a guest failure it also checks that Linux still passes the shortened trace. The probe rejects reads of protected or unmapped pages so the reducer cannot turn a byte mismatch into an invalid test.

## Results

The baseline was Akuma `3677587b`, built for amd64 with `headless,no-tests`. The guest ran on one vCPU under QEMU/KVM with an ext2 snapshot. The original `mmaplazy` checks passed on Linux. Generated seeds 0–127 passed on Linux, and seeds 0–23 passed on both Linux and baseline Akuma. A repeat run hit a silent SSH exit 255 before the probe ran. The runner retries that transport failure once and reports a repeated failure as an error. No baseline byte mismatch was observed.

For the positive control, a separate kernel build omitted `FileBacking::advance` for the surviving tail in [`mprotect_eager_regions_in_range`](../../crates/akuma-mmap/src/region.rs#L579). Seed 0 failed on the modified kernel. The runner reduced the seven operations to two.

```text
Original  C:1:1 N:11:3 U:10:3 R:13:1 C:22:1 C:13:1 C:23:1
Reduced   R:13:1 C:22:1
Mismatch  C:22:1 file_offset=94208 expected=89 actual=51
```

In the reduced case, `mprotect(PROT_READ)` on page 13 requests the protection already in effect. Akuma still splits the region record. It records the requested protection on the covered piece and preserves the original metadata on either side. The later read of page 22 uses the new tail record to find its file bytes, even though that page was outside the requested range.

This behavior is expected and correct for the tested mapping. At the syscall boundary the page stays readable and the file contents stay the same. Internally the split can do extra bookkeeping when the recorded protection already matches. The pilot measured correctness and did not measure the cost of the split. The reduced trace passed on Linux and baseline Akuma and produced the same mismatch on the deliberately modified kernel. The source change used for the positive control was reverted. The run found no existing mapping defect and exercised one vCPU, so it supplies no evidence of a race. [Linux `mprotect(2)`](https://man7.org/linux/man-pages/man2/mprotect.2.html) · [Region split implementation](../../crates/akuma-mmap/src/region.rs#L608)

## Reproduce

From the repository root, using a native musl cross compiler for the matching architecture:

```sh
x86_64-linux-musl-gcc -static -O2 -Wall -Wextra -Werror \
    -o /tmp/mmaplazy userspace/forktest/c_stress/mmaplazy.c
python3 scripts/mmaplazy_trace.py /tmp/mmaplazy --cases 128
python3 scripts/mmaplazy_trace.py /tmp/mmaplazy --port 42224 --identity "$AKUMA_SSH_KEY" --cases 24
python3 scripts/mmaplazy_trace.py /tmp/mmaplazy --replay R:13:1 C:22:1
```

The guest command assumes an amd64 Akuma guest with SSH forwarded to port 42224. The runner accepts `N` for `PROT_NONE`, `R` for `PROT_READ`, `U` for `munmap`, and `C` to check one page. Each operation takes a zero-based page index and a page count.

The pilot varies one operation template. The reducer deletes whole operations and leaves their arguments intact. It reports syscall errors and crashes separately, and the byte checks cover file offsets and EOF zero-fill. Access faults, CoW, and SMP interactions remain outside the generated cases.

## Denver JVM protection checks

HotSpot needs to make written code executable. Denver’s `wx-relaxed` build permits that transition. The strict build refuses it with `EACCES` and checks the whole range before changing any page. The existing Denver gates check direct RWX mappings and a single-page RW-to-RX transition. [Denver JVM changes](DENVER_BRANCH_FIXES.md) · [W^X policy](../../crates/akuma-syscalls-mem/src/wx.rs)

The new `wxrange` arm in [`jvmprobe.c`](../../denver/probes/jvmprobe.c) tests the range behavior through the syscall. It changes three clean pages to RX and back to RW, writes a small function into exactly one page, and takes that page through read-only and back to RW. It then requests RX for the whole range. Each run moves the written page through all three positions.

- **Strict Denver** returns `EACCES` and leaves all three pages writable. The probe verifies this with a store to each page, including pages that precede the written page in the range.
- **Denver with `wx-relaxed`** accepts the request, preserves the written bytes, and executes the function, which returns 42. Linux passes the same relaxed checks.

Both Denver runs reported `WXRANGE PASS` and exit 0 under one-vCPU amd64 QEMU/KVM. They used the same freshly compiled static probe and Denver source from `44e153e7`, with `wx-relaxed` as the only feature difference. Denver at the pilot’s base, `3677587b`, failed to build because `akuma-tokio-kernel-core` still calls older process and shared-page-cache APIs. The archived Denver build supplies the guest evidence. The pilot checkout’s pure policy tests also passed, five strict and three relaxed.

To reproduce, compile `denver/probes/jvmprobe.c` with `x86_64-linux-musl-gcc -static -O2 -Wall -Wextra -Werror`. Run `jvmprobe wxrange relaxed` on Linux. For Denver, put the same binary at `/bin/jvmprobe` on an ext2 guest image and boot with `init=/bin/jvmprobe initargs=wxrange,strict` or `initargs=wxrange,relaxed` for the corresponding build. Build `denver` at `44e153e7` with `cargo build -p denver --target x86_64-unknown-none --release`, adding `--features wx-relaxed` for the second build. This tests the JIT permission transition with a six-byte function. The JVM and SMP were outside this run.
