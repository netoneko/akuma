# mmap_stress — C-only mmap control binary

Pure musl static ELF (no Go runtime). Mirrors the mmap loop shape of `forktest_child`
`runMmapStress` so you can tell kernel mmap faults from Go allocator bugs.

## Build (host)

From repo root, with `aarch64-linux-musl-gcc` on PATH:

```bash
cd userspace/forktest/c_stress
aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o mmap_stress mmap_stress.c
cp mmap_stress ../../../bootstrap/bin/
```

Or use `userspace/build.sh --with-forktest`, which builds Go forktest and this binary.

## Install on Akuma via `pkg install` (SSH)

`pkg` downloads `http://10.0.2.2:8000/bin/<name>` into `/bin/<name>` ([docs/PACKAGES.md](../../../docs/PACKAGES.md)).

On the **host**, serve the `bootstrap` directory (which contains `bin/mmap_stress`):

```bash
cd /path/to/akuma/bootstrap
python3 -m http.server 8000
```

In **SSH** to the guest:

```text
pkg install mmap_stress
```

Then run the parent with C children instead of Go:

```text
/bin/forktest_parent --use_c_child --duration 10s --mmap_test=true --mmap_alloc_mb=70
```

(`--mmap_test` only selects forwarded flags; the C binary always runs the mmap loop.)

If **this** crashes but plain Go children without mmap do not, the fault is likely in the kernel lazy-paging path. If **this** passes but Go **`--mmap_test`** fails, focus on the Go runtime / syscall errno paths.
