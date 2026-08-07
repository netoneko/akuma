# Log & crash analysis scripts

Grade: — (index)

| Script | What it does |
|---|---|
| [`analyze_crash.py`](../../../scripts/analyze_crash.py) | Parses a kernel crash/serial log for context-switch irregularities around a `SwitchEvent`. `python3 scripts/analyze_crash.py crash132.log`. |
| [`capture_serial_forktest_mmap.sh`](../../../scripts/capture_serial_forktest_mmap.sh) | Captures QEMU's `mon:stdio` serial to a file while you drive `forktest_parent`/mmap probes over SSH in another terminal; prints the grep pattern for `[mmap]`/`[WILD-DA]`/`[Fault]` lines afterward. |
| [`ext2read.py`](../../../scripts/ext2read.py) | Minimal read-only ext2 extractor — pulls one file out of a disk image without mounting it (useful when a VM is wedged and you need a log/artifact off `disk.img`). |

Back to [`README.md`](README.md).
