# Dash

## Building a binary

```bash
CC=aarch64-linux-musl-gcc LDFLAGS=-static ./configure --host=aarch64-linux-musl --enable-static --disable-glob --disable-test-workaround --disable-lineno --without-libedit

make
```

## Known issues

```bash
akuma:/> dash
/bin/dash: 0: can't access tty; job control turned off
$
akuma:/>
```

Associated kernel log:

```bash
[ELF] Segment: VA=0x00400000 filesz=0x2e738 memsz=0x2e738 flags=R-X
[ELF] Segment: VA=0x0043ff78 filesz=0x298 memsz=0x1a90 flags=RW-
[ELF] Loaded: entry=0x4002f8 brk=0x441a08 pages=50
[ELF] Heap pre-alloc: 0x442000 (16 pages)
[ELF] Stack: 0x3ffc0000-0x40000000, SP=0x3fffff00, argc=1
[Process] PID 15 memory: code_end=0x442000, stack=0x3ffc0000-0x40000000, mmap=0x10000000-0x3fec0000
[syscall] Unknown syscall: 172 (args: [0x440000, 0x3fffff08, 0x3ffffe80, 0x412688, 0x0, 0x441300])
[syscall] Unknown syscall: 175 (args: [0xa, 0x440130, 0x4401ce, 0x4401b0, 0x441170, 0x27])
[syscall] Unknown syscall: 173 (args: [0x440190, 0xfffffec7, 0x4, 0x4401d0, 0xffffffffffffffff, 0x303c434d48534f4e])
[exception] Process 15 (/bin/dash) exited, calling return_to_kernel(0)
```
