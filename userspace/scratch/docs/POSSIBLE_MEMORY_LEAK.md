# Scratch case for memory leak

**STATUS: FIXED (Feb 5, 2026)**
See [MEMORY_LEAK_FIX.md](MEMORY_LEAK_FIX.md) for details on the root causes and optimizations.

Runs out of memory on deflation + suspicious amount of free memory.

```bash
scratch clone https://github.com/netoneko/meow.git

skip

scratch: decompress done consumed=2313 output=7094
scratch: parsed object type=4 size=7094
scratch: parsing object 220 at pos 148238
scratch: decompress_with_consumed input_len=52879
scratch: InflateState created
OUT OF MEMORY!
  Net memory: 64937984 bytes (63416 KB)
  Total allocated: 79347712 bytes
  Total freed: 14409728 bytes
  Allocation count: 1420
[exit code: -1]
akuma:/> kthreads
  TID  STATE     STACK_BASE  STACK_SIZE  STACK_USED  CANARY  TYPE         NAME
   0  ready     0x41f00000    1024 KB      0 KB  0%  OK      cooperative  bootstrap
   1  ready     0x4200b028     512 KB     35 KB  6%  OK      preemptive   network
   2  running   0x4208b030     512 KB     37 KB  7%  OK      preemptive   system-thread
   8  ready     0x4238b060     128 KB     33 KB 25%  OK      preemptive   user-process
   9  ready     0x423ab068     128 KB     33 KB 26%  OK      preemptive   user-process

Total: 4 threads (ready: 3, running: 1, terminated: 0)
akuma:/> pmm leaks
DEBUG_FRAME_TRACKING is disabled.
Enable it in src/pmm.rs to track frame allocations.
akuma:/> pmm
Physical Memory Manager:

pages       MB
Total:      32512      127
Allocated:  16492       64
Free:       16020       62

Frame tracking: DISABLED
akuma:/> ps
  PID  PPID  STATE     NAME
   10     0  running   /bin/herd
   11     0  running   /bin/httpd
akuma:/> ls meow
.git/
akuma:/> rm -rf meow
Removed: /meow
akuma:/> free
Memory Statistics:

total       used       free
Mem:       32512 KB     6787 KB    25724 KB

Usage:       20%
Peak:        7850 KB
Allocs:      14923862
Heap size:   31 MB
akuma:/>
```
