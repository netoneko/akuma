# 4 MB Stable Agent: First CGI Run Case Study

Running meow as a CGI process inside a 4 MB VM with an external ollama backend
(qwen3:4b). The kernel and agent stayed stable for the full run. This document
records what happened, what broke, and what needs fixing.

## Setup

- **RAM:** 4 MB total, ~1632 KB free at baseline after httpd+herd
- **Model:** qwen3:4b on ollama (host machine)
- **Transport:** HTTP POST to `/cgi-bin/meow`, meow streams response back
- **Duration:** 25m21s of model inference before connection close

## Prompt Sent

```
clone https://github.com/netoneko/akuma-playground.git using /bin/scratch as
git-compatible substitute and statically compile /akuma-playground/hello.c with
/bin/tcc, put binary in /tmp/h5mb and run it, run commands one by one using
shell tool
```

## Timeline

| Time    | Tool call                                              | Result            |
|---------|--------------------------------------------------------|-------------------|
| 1m48s   | `Shell /bin/scratch/git clone <url>`                  | not found (127)   |
| 3m41s   | `FolderCreate /bin/scratch`                           | failed (exists)   |
| 4m47s   | `FolderCreate /tmp/scratch`                           | success           |
| 5m42s   | `Shell git clone <url>`                               | not found         |
| 6m58s   | `Shell sudo apt-get update && apt-get install -y git` | not found         |
| 7m53s   | `HttpFetch github.com/.../master.zip`                 | 302 (no redirect) |
| 8m23s   | `Shell curl -L <url>`                                 | not found         |
| 10m11s  | `HttpFetch github.com/.../master.zip`                 | 302               |
| 12m36s  | `Shell apt-get install -y curl git`                   | not found         |
| 12m59s  | `HttpFetch github.com/.../master.zip`                 | 302               |
| 15m7s   | `Shell mkdir -p /bin/scratch`                         | not found         |
| 17m25s  | `Shell curl -L curl-7.81.0.tar.gz`                   | not found         |
| 19m48s  | `HttpFetch github.com/.../master.zip`                 | 302 (4th attempt) |
| **20m50s** | **`HttpFetch raw.githubusercontent.com/.../hello.c`** | **success, 77 B** |
| 21m14s  | `Shell /bin/tcc -static -o /tmp/h5mb hello.c`        | file not found    |
| 22m18s  | `FileWrite /tmp/hello.c`                              | success, 85 B     |
| 22m18s  | `Shell /bin/tcc -static -o /tmp/h5mb /tmp/hello.c`   | exit 0            |
| 22m29s  | `Shell /tmp/h5mb`                                     | spawn failed      |
| 22m48s  | `Shell chmod +x /tmp/h5mb`                            | not found         |
| 25m21s  | —                                                     | connection closed  |

## Memory (kernel log)

Sampled every ~3 seconds from `[Mem]` lines. Heap and RAM were flat throughout
the agent's inference loop — the model consumed zero guest memory.

| Elapsed | RAM free | Heap used | Notes                          |
|---------|----------|-----------|--------------------------------|
| baseline| 1632 KB  | 389 KB    | idle, pre-request              |
| ~32s    | 1100 KB  | 476 KB    | meow spawned via CGI           |
| ~8m     | 1036 KB  | 479 KB    | steady                         |
| ~17m    | 780 KB   | 512 KB    | peak pressure (child processes)|
| ~20m    | 876 KB   | 475 KB    | recovering                     |
| ~26m    | 1728 KB  | 356 KB    | meow exited, pages reclaimed   |

Post-exit free (`free` in shell):
```
              total      used      free
Mem:          4096 KB     2464 KB     1632 KB
Heap:          512 KB      392 KB      119 KB
```

## What Broke and Why

### 1. Ambiguous `/bin/scratch` interface

The model read "using /bin/scratch as git-compatible substitute" as a path
prefix and called `/bin/scratch/git clone ...`. The actual interface is
`/bin/scratch clone <url>`.

**Fix (prompt):** Be explicit: "run `/bin/scratch clone <url> <dest>` instead of
`git clone`".

### 2. No environment description

The model probed for `git`, `curl`, `sudo`, `apt-get`, `mkdir`, `chmod` — none
present. It retried the same dead ends multiple times (HttpFetch with the GitHub
redirect URL four separate times). About 19 of 25 minutes were spent on this.

**Fix (prompt):** List available binaries in the system prompt: `/bin/tcc`,
`/bin/scratch`, `HttpFetch`, `FileWrite`. Explicitly state: no package manager,
no curl, no git, no standard coreutils.

### 3. Wrong source path assumption

The prompt said "compile `/akuma-playground/hello.c`" which implied the path
would exist after a clone that never succeeded. The model caught this eventually
but wasted a tool call round-trip on it.

### 4. Binary failed to execute

The kernel log shows tcc (pid=6) hit an anonymous page allocation failure
mid-run (`anon alloc failed, 16 free pages`) with a data abort (EL0, ISS=0x47 —
write page fault). tcc still reported exit 0, but the binary was likely
incomplete or written without execute permission. `chmod` is not available.

The spawn error "Failed to spawn '/tmp/h5mb' (not found?)" is akuma's generic
exec failure, which fires on both missing file and bad ELF.

**Fix (kernel/tcc):** Propagate SIGSEGV exit code from child processes correctly.
**Fix (httpd/meow):** Document that compiled binaries may need explicit exec
permission handling, or make tcc set the exec bit on output.

## Meow Issues to Fix

### JSON escaping — FileWrite writes raw escape sequences (confirmed bug)

The model emitted Unicode-escaped characters in tool arguments:

```json
{"content":"#include <stdio.h>\n\nint main() {\n  printf(\"Hello, Akuma!\\n\");\n  return 0;\n}","filename":"/tmp/hello.c"}
```

`<` = `<`, `>` = `>`. The FileWrite tool wrote these escape sequences
**literally** to disk rather than decoding them. Confirmed by reading the file
from the VM after the run:

```
#include <stdio.h>

int main() {
  printf("Hello, Akuma!\n");
  return 0;
}
```

tcc received a file with a malformed `#include` directive. It silently continued
and produced a binary (exit 0) but without stdio — so the binary either crashed
immediately on the `printf` call or the exec failed for another reason (the OOM
at T1338s was also present). Either way, the binary was useless.

This was the actual cause of the run failing at the last step, not just the OOM.
The fix is in meow's FileWrite handler: JSON string values must be decoded before
being written to disk, not passed through as raw bytes from the JSON payload.

### Model selection via query string

Currently the model is hardcoded in meow's config. Planned improvement: pass the
model name as a query string parameter to `/cgi-bin/meow`, e.g.:

```
POST /cgi-bin/meow?model=qwen3:4b
```

This allows switching models per-request without rebuilding or reconfiguring,
which is useful for comparing model behavior on the same prompt.

## Additional Observations from Kernel Log

### Alloc spikes are a perfect tool-call signal

Every tool call execution produces a visible spike in kernel alloc rate — 1000–3000
allocs/second for ~3s, against a steady background of ~130/s. The background rate
is the chunked HTTP streaming parser receiving ollama tokens. The spikes correspond
to child process spawn + exec + result buffering.

The spikes also line up with the `[Mem]` Allocs counter between tool calls:

| Phase | Alloc rate |
|-------|-----------|
| Idle (waiting for ollama token stream) | ~130/s |
| Tool call execution | 1000–3000/s |
| Post-call (model processes result) | ~130/s |

This is a zero-cost profiler: grep `[Mem]` Allocs deltas to reconstruct the tool
call timeline without any instrumentation in meow.

### Heap peak grew to 562 KB at the worst moment

The heap peaked at 562 KB at T=16m35s, exactly when RAM was at its minimum (780 KB
free). This was during the longest inference gap — between the `mkdir` failure at
15m7s and the `curl` attempt at 17m25s. The model was generating a long response
while holding maximum accumulated context. Both pressures hit simultaneously.

Heap peak progression:

| Time | Heap peak |
|------|-----------|
| boot | 401 KB |
| meow spawn | 494 KB |
| first tool call | 496 KB |
| ~8m | 499 KB |
| 16m29s (RAM floor) | 528 KB → 562 KB |
| post-exit | 562 KB (historical) |

### Incremental page reclaim under pressure

The kernel reclaimed pages in 256 KB chunks as pressure built, not all at once:

| Time | Total reclaimed |
|------|----------------|
| 7m54s | 256 KB |
| 10m13s | 512 KB |
| 13m1s | 768 KB |
| 24m39s | 1024 KB |

By the time RAM hit its floor at 16m, only 768 KB had been reclaimed. The reclaimer
was conservative enough that RAM still dropped to 780 KB free before recovering.

### SSH stall distribution: bimodal at <1ms and 10–50ms

Out of 1010 SSH poll samples:

| Range | Count | % |
|-------|-------|---|
| <1ms | 564 | 55.1% |
| 1–10ms | 1 | 0.1% |
| 10–50ms | 451 | 44.0% |
| 50ms–1s | 7 | 0.7% |
| >1s | 1 | 0.1% |

The gap at 1–10ms is the 10ms preemptive timer quantization. When the SSH poller
doesn't find data immediately, it yields and gets rescheduled a full timer tick
later. The 7 samples at 50ms–1s correspond to child process spawns competing for
scheduler time. The single >1s sample (42 seconds) was a host machine sleep/wake —
the watchdog correctly identified it:

```
[WATCHDOG] Time jump detected: 42022ms (host sleep/wake)
[SSH] STALLED listening | stall_us=42023007
```

The VM survived the sleep intact and resumed normally.

### TCC OOM details

tcc (pid=6) mapped two 260 KB lazy regions (0x41000 each) for its output buffer.
It crashed on the first fault into the first region at offset 56 KB:

```
[DA-DP] pid=6 va=0x1047e020 anon alloc failed, 16 free pages
[Fault] Process 6 (/bin/tcc) SIGSEGV after 0.06s
```

16 free pages = 64 KB remaining at crash time. tcc needed ~56 KB to get to the
fault point, leaving 64 KB — tight but borderline. With slightly less context
pressure, it might have succeeded.

### Exit code 0 reported after SIGSEGV (kernel bug)

meow reported `Exit code: 0` for the tcc run despite tcc dying on SIGSEGV. The
correct POSIX exit status for a signal death encodes the signal number. Returning 0
is incorrect and means the parent process (meow) cannot distinguish a clean compile
from a signal-killed one. This masked the OOM failure entirely.

## Recommendations for Tight Places

### meow

**1. Fix FileWrite Unicode decoding** (critical, blocks code generation)
JSON `\uXXXX` sequences must be decoded before writing to disk. One line in the
FileWrite handler.

**2. Cap context window / use sliding window**
Context accumulated across 20+ failed tool calls was the main driver of the heap
peak. A rolling window that drops old turns would flatten memory and keep inference
faster (shorter context = fewer tokens to process per turn).

**3. Stream through rather than buffer ollama responses**
If meow buffers the full response before acting, switching to true streaming would
lower the heap peak during long inference gaps.

### kernel / syscalls

**4. Fix SIGSEGV exit code propagation**
`wait4()` must encode signal deaths correctly. Currently returning 0 hides crashes
from parent processes.

**5. Set executable bit on tcc output**
tcc should create its output binary with 0755 so it can be exec'd without chmod.
chmod is not available in the minimal environment.

**6. HttpFetch should follow at least one redirect**
The GitHub zip URL returned 302 four times and the model retried it identically
each time. One redirect hop would have resolved it on the first attempt, saving
~8 minutes of dead inference.

**7. Reclaimer trigger threshold**
Currently the reclaimer waits until pressure is high enough to force 256 KB chunks.
Triggering earlier (e.g., at <1200 KB free instead of <1100 KB) would leave more
headroom for tcc's mmap requests and might have prevented the OOM at 16m.

## What Worked

- Kernel and httpd stayed up for the full 26 minutes with no panics or OOMs at
  the OS level.
- meow's memory footprint was stable — the agent loop itself used no extra RAM
  beyond the initial spawn cost (~532 KB drop from 1632 to 1100 KB free).
- Page reclaim on meow exit worked correctly: free RAM went from 1100 KB back to
  1728 KB (above baseline, because the SSH session that was active at baseline
  had disconnected).
- The model eventually converged on the correct approach (HttpFetch raw content,
  FileWrite, tcc compile) without any explicit guidance — just took 20 minutes
  to get there.
- TCC ran inside a 4 MB VM and produced a binary (though the binary was broken
  due to the FileWrite escaping bug — see above).
