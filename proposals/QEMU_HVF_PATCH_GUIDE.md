# Building a Patched QEMU for HVF on Apple Silicon

## Background

Stock QEMU 10.x crashes when a bare-metal AArch64 guest accesses the GIC
distributor under HVF (`-accel hvf`). The crash is:

```
Assertion failed: (isv), function hvf_vcpu_exec, file hvf.c
```

This is QEMU GitLab issue [#2312](https://gitlab.com/qemu-project/qemu/-/issues/2312).
The GIC memory region at `0x0800_0000` exits through a special hypervisor
mechanism that always produces `ISV=0` in the data-abort ESR, regardless of
instruction type. QEMU's HVF handler cannot decode ISV=0 faults and asserts.

An RFC patch series by Joelle van Dyne (UTM maintainer) fixes this by falling
back to TCG single-step emulation for ISV=0 data aborts.

## Step 1: Fork and Clone QEMU

```bash
# Fork https://gitlab.com/qemu-project/qemu on GitLab
# Then clone your fork:
git clone https://gitlab.com/<your-username>/qemu.git
cd qemu
git checkout v10.0.0   # match your installed version
git checkout -b hvf-isv-fix
```

## Step 2: Download the Patches

The series is **"[PATCH RFC 0/4] hvf: use TCG emulation to handle data aborts"**.
You need patches 1-3 (patch 4 enables VGA, optional).

| # | Title | Mail Archive Link |
|---|-------|-------------------|
| 1/4 | cpu-exec: support single-step without debug | https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094012.html |
| 2/4 | cpu-target: support emulation from non-TCG accels | https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094008.html |
| 3/4 | hvf: arm: emulate instruction when ISV=0 | https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094009.html |

On each mail-archive page, download the raw message (look for the "raw" link
at the top). Save as `01.patch`, `02.patch`, `03.patch`.

## Step 3: Apply the Patches

```bash
git am 01.patch 02.patch 03.patch
```

If `git am` fails due to version conflicts:

```bash
git am --abort
git am --3way 01.patch 02.patch 03.patch
```

If that also fails, apply manually:

```bash
git apply --check 01.patch   # dry-run to see conflicts
git apply --3way 01.patch    # apply with 3-way merge
# Resolve conflicts, then:
git add -A && git am --continue
# Repeat for 02.patch, 03.patch
```

## Step 4: Build QEMU

```bash
mkdir build && cd build
../configure \
  --target-list=aarch64-softmmu \
  --enable-hvf \
  --enable-slirp
make -j$(sysctl -n hw.ncpu)
```

Verify:

```bash
./qemu-system-aarch64 --version
./qemu-system-aarch64 --accel help   # should list "hvf"
```

## Step 5: Push Your Fork

```bash
cd ..
git push origin hvf-isv-fix
```

## Step 6: Configure Akuma

### Point cargo_runner.sh at your patched QEMU

In `scripts/cargo_runner.sh`, replace:

```bash
exec qemu-system-aarch64 \
  -accel tcg \
```

with:

```bash
exec /path/to/your/qemu/build/qemu-system-aarch64 \
  -accel hvf \
```

### Enable HVF workarounds in the kernel

In `src/config.rs`, set:

```rust
pub const QEMU_HVF_FIX_ENABLED: bool = true;
```

This activates:
- Virtual timer (`CNTV_*`) instead of physical timer (`CNTP_*`)
- Page table flush to PoC (`DC CIVAC`) before PTE installs
- Skipping GIC init (still needed even with patched QEMU? test and remove if full GIC works)

### Build and run

```bash
MEMORY=2048 cargo run --release
```

## Alternative: Minimal One-Line QEMU Patch

If the full patch series doesn't apply cleanly, there is a simpler (partial)
fix. In `target/arm/hvf/hvf.c`, find the `EC_DATAABORT` case and replace:

```c
assert(isv);
```

with:

```c
if (!isv) {
    break;  /* retry -- stage-2 mapping already fixed */
}
```

This only helps for RAM-based ISV=0 faults (page table walks, dirty tracking).
It does NOT fix MMIO ISV=0 (like the GIC distributor) -- those will retry
infinitely. For MMIO to work, you need the full Joelle van Dyne patch series
which falls back to TCG single-step emulation.

## References

- QEMU GitLab Issue #2312: https://gitlab.com/qemu-project/qemu/-/issues/2312
- Patch cover letter: https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094011.html
- Peter Maydell review: https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094114.html
- Platform vGIC series (future): https://www.mail-archive.com/qemu-devel@nongnu.org/msg1173071.html
- Akuma HVF doc: `docs/APPLE_M4_QEMU_HVF.md`
