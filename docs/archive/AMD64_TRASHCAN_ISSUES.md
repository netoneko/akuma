# amd64 bare metal ("the trashcan") — open issues

**Status: OPEN.** Live defects on the HP 500-502nj bare-metal target and its
Firecracker/QEMU stand-ins. **Stability: C** — these are active, and at least one
has had its "root cause" overturned twice.

As of 2026-09-19 §§1-4 are fixed, §§5-7 are open, and §7b is an incident report rather than a defect of this machine's. Two of the entries here are
**not kernel bugs at all** (§4 a client timeout, §6 a model and its tool
payload) and they are kept in this file on purpose: both presented as "the box
is broken", and the record of how each was pushed off the kernel is the part
worth having next time.

This is the **investigation log**. It was carved out of
[`AKUMA_FROM_SCRATCH.md`](AKUMA_FROM_SCRATCH.md) on 2026-09-18: that document is
the *aspirational manual* for building the system on the metal, and a goal that
has not been reached yet should not be buried under three passes of debugging
narrative. The manual keeps the one-line statement of what must work; the
evidence lives here.

The runbook for driving the machine is
[`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md),
and its first rule applies to everything below: **grep `docs/archive/` before
forming a theory.** This target is a port, so most of what breaks here already
broke once on AArch64 — and when the archive says "fixed", the next question is
**"fixed where?"**, because a fix that landed in AArch64-only code leaves the
bug live here.

---

## 1. `git clone` over HTTPS hangs

**Status: FIXED 2026-09-19** — `execve` did not close `FD_CLOEXEC`
descriptors on this target. Four distinct causes wore this one signature; the
signature discriminates none of them, which is why the history below is kept in
full. **The fix is §1.5**; §§1.1–1.4 are how it was found.

**`git clone` over HTTPS from inside Akuma.** `git` 2.54.0 is installed on the
metal and works locally.

### Pass 1 — hung, blamed on DNS

**First attempt, 2026-09-18: hung — no DNS.** `git clone --depth=1
https://github.com/netoneko/akuma.git` sat with three processes
(`git clone`, `git remote-https`, `git-remote-https`) at **0:00 CPU across
12 s**. That signature is worth knowing because it is *blocked on I/O*, not
the SMP wedge — which also shows 0:00 but with `cargo`/`rustc` and after real
work. A hung resolver looks exactly like a stuck clone.

The bare-metal root needs the same two things the `box` rootfs needed for DNS
and HTTPS, and they are separate failures:

| missing | symptom |
|---|---|
| `/etc/resolv.conf` | clone **hangs** at 0:00 CPU, no error |
| CA bundle (`ca-certificates.crt`) | clone **fails with a TLS error** — git verifies github's certificate in its own stack |

Stage both on sdb1 before concluding anything about git's TLS support.

### Pass 2 — root-caused: connected-UDP syscalls, FIXED

**It was not TLS — it was the kernel.** `/etc/resolv.conf` was present and correct the whole time, and
`nslookup github.com` resolved fine. `curl` and `git` use **c-ares**, which
`connect()`s its UDP socket where musl's resolver does not, and three
syscalls on that connected path were wrong: `send()` answered `EBADF`
(a null `sendto` destination fell into the TCP path) and
`getsockname`/`getpeername` answered `ENOSYS`. Full account:
[`AKUMA_AMD64_DNS_CONNECTED_UDP.md`](AKUMA_AMD64_DNS_CONNECTED_UDP.md).

So this step's real lesson is the diagnostic, not the fallback list:
**`nslookup` working proves nothing about whether `git` can resolve.**
The CA-bundle row above is still untested — it simply never got reached.

### Pass 3 — the symptom returns: a third cause

**2026-09-18, later: the symptom came back, and it is a THIRD cause.**
Same three processes, same 0:00 CPU. The DNS fix is not regressed — it is
verified working on the kernel that shows this: `/root/probes/resprobe2`
passes every connected-UDP step (errno=0 throughout), and `git`'s **own
helper**, driven by hand, does the entire job in under 8 s —
`printf 'capabilities\nlist\n' | GIT_CURL_VERBOSE=1
/usr/libexec/git-core/git-remote-https origin <url>` resolves github.com,
connects to 20.217.135.5:443, completes a TLS 1.3 handshake and returns
response headers.

What hangs is `git` ↔ helper. Under `GIT_TRACE=1` the trace stops dead at
`start_command: git-remote-https` and `GIT_CURL_VERBOSE=1` yields **zero**
curl lines, with no outbound socket for the whole hang — the helper is
started and never receives its command. Both sides sit `State: R` at 0:00,
i.e. runnable-and-idle, not `D`.

Ruled out with numbers rather than reasoning: pipe exhaustion
(`[PIPES] live=4 high=16 refused=0 cap=256` in the idle report) and a
general fork/exec wedge (`( ls; ls )` grandchildren and `git --version`
both fine). Candidate to test first: the lost-wakeup mechanism the runbook's
OPEN "ssh needs one extra event" entry lists — a pipe reader never woken by
the first write deadlocks exactly like this.

**So this row's lesson has a second half: the 0:00-across-three-processes
signature now has three distinct causes** (a hung resolver, the SMP wedge,
and this), and it discriminates none of them. Drive the helper by hand
before blaming the network — it separates all three in one command.
Caveat on the evidence: the box had ~10 stuck git processes that `kill -9`
would not clear when this was narrowed, so re-run it on a fresh boot.
Why those processes never went away:
[`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) §3.1.

### Pass 4 — reproduced on Firecracker; lost-wakeup RULED OUT

**Reproduced on a fresh Firecracker boot, and the lost-wakeup candidate is
RULED OUT.** Same signature (`git ls-remote`, 25 s,
`GIT_TRACE=1` stops dead after `start_command: git-remote-https`), on a
kernel built that day, with the network proven good in the same session:
the helper driven by hand completed a TLS 1.3 handshake and got
`HTTP/2 200` with `content-type: application/x-git-upload-pack-advertisement`
in **0.9 s**.

The new evidence is the **slot census**, which the earlier pass did not
read. All four processes park at the *same* site — `fs.rs:740`, the
`pipe_check_set_reader` / `park_indefinitely` arm of the blocking pipe read
in `akuma-syscalls-glue` — with `sc=0` (x86_64 `read`):

```
[SLOT]  8 ... sc=0 scn=578 park=fs.rs:740 pid=Some(78)   git ls-remote / clone
[SLOT] 10 ... sc=0 scn=153 park=fs.rs:740 pid=Some(80)   git remote-https
[SLOT] 11 ... sc=0 scn=341 park=fs.rs:740 pid=Some(81)   git-remote-https
```

**`pick=` advances between consecutive censuses** (e.g. pid 78: 451 -> 481,
pid 81: 453 -> 483). A lost wakeup leaves a thread parked and never picked;
these are being scheduled, re-testing their condition, finding the pipe
empty and parking again. So the wake path is working and the ~500 lines of
sticky-`WOKEN_STATES` reasoning in `amd64/src/sched.rs` § "There is no
`prepare_block`" are not implicated. **The data is simply not in the pipe.**

That moves the question from "who failed to wake whom" to "where did the
first helper command go". `git` is parked *reading* the helper's stdout,
which means it believes it already wrote `capabilities\n` to the helper's
stdin; the helper is parked reading that same stdin having never seen it.
Note `git remote-https` (pid 80) is parked in `read`, not `wait4` — git's
`execv_dashed_external` runs the helper via `run_command` and should be
waiting on it, so that park site is itself unexplained and is the next
thread to pull.

Next probe, and it is cheap: a parent that writes to a pipe and *then*
forks+execs twice, with the reader at the second level — the shape git uses
and the shape `printf | git-remote-https` (which works) does not.

### Where it stands

Network, DNS and TLS are **proven good** on the kernel that shows the hang. The
wake path is **proven live**. What is unexplained is where git's first helper
command goes, and why `git remote-https` parks in `read` rather than `wait4`.

---

## 1b. Bare-metal confirmation of §1

**Status: CONFIRMED 2026-09-19 on the metal**, not only in the Firecracker
guest. Kernel `020b4f16` + the fix, booted from `/boot/akuma-amd64` on the box's
own ext2 root, self-tests **775 passed / 0 failed**:

| | result |
|---|---|
| `git ls-remote https://github.com/octocat/Hello-World` | **1 s**, real refs |
| `git clone --depth=1` the same repo | **1 s**, 29 files |
| `git clone --depth=1 https://github.com/git/git` | **42 s**, **4852 files**, `git status` clean |

The last row is the one worth keeping: a ~20 MB packfile fetched over TLS,
indexed, and checked out onto the USB root, with `git status` clean afterwards.

---

## 2. `resolve_host` fails for an IP literal

**Status: FIXED 2026-09-19.** Found by `meow`, which could not reach an
inference server on the LAN while `busybox wget` reached the same address
perfectly — the tell that the two use different resolvers.

`meow --debug` names it exactly:

```
[meow:debug] resolving 192.168.1.203:8080
[meow:debug] connect error (attempt 0): DNS resolution failed for: 192.168.1.203
```

**"Fixed where?" again.** The AArch64 resolver has always short-circuited
`localhost` and a dotted quad — twice, in fact, once inside
`akuma_net::dns::resolve_host` and once inside `resolve_host_blocking`.
`amd64/src/dns.rs::resolve_a` is a *separate* A-record client (written because
`smoltcp_net::dns_query` hung on this target, §3.30 of
`AKUMA_FIRECRACKER_AMD64.md`) and it had neither. So `resolve_host("192.168.1.203")`
went out and asked a resolver for an A record **named** `192.168.1.203`, got
NXDOMAIN, and returned `ENOENT`.

It only bites `no_std` programs: anything on musl (busybox, git, `nca`) resolves
for itself and never issues this syscall. Everything on `libakuma` — `meow`,
`hget`, anything using `libakuma-tls` — goes through it.

**The fix:** `akuma_net::dns::resolve_literal(host) -> Option<[u8; 4]>`, one
implementation, called by all three sites. Host-tested in `akuma-net`, including
the near misses that must still reach a resolver (`192.168.1`, `192.168.1.256`,
`192.168.1.203:8080`).

---

## 3. `poll_input_event` read the UART, so every TUI was dead over ssh

**Status: FIXED 2026-09-19.** `meow` and `nca`'s full-screen modes did not work
on this target, and neither did `paws`: the shell printed its banner and its
prompt and then ignored everything typed at it, while `busybox sh -i` and `cat`
on the same ssh session worked normally.

That split is the whole diagnosis. `read(2)` on fd 0 goes through
`akuma-syscalls-glue`'s `Stdin` arm, which reads the process's
`ProcessChannel` — the thing `sshd` actually feeds. `poll_input_event` had a
**local copy** in `amd64/src/fd.rs` that, for anything that was not a pipe,
looped on `crate::input::getb()` — the **serial port**. An ssh session's
keystrokes are never there. `meow`'s TUI blocked on its terminal-size probe
before painting a single frame; `paws` blocked on its first keystroke.

The same function also took `_timeout_us` and dropped it. That is not cosmetic:
the timeout **is** a TUI's frame tick (`meow` polls with 50 ms and repaints when
it expires) and it is what lets the terminal-size probe give up after 500 ms and
fall back to 100x25 instead of waiting for a cursor report nobody will send.

**The fix:** `amd64/src/usermode.rs` arm 313 hands the call to
`akuma-syscalls-glue`'s `sys_poll_input_event` whenever the process has a
channel — the shared implementation AArch64 has always used, which reads the
channel, honours the timeout, registers an input waker and re-resolves the
channel across a `box grab`. The local arm stays for the two cases glue answers
`ENOMEM` for — a redirected fd 0 that is a pipe, and a process with no channel —
and now honours `timeout_us` on both of its paths too.

**Verified on the metal, over `ssh -tt`:**

| | before | after |
|---|---|---|
| `paws`, typing `echo PAWS-OK` | prompt, then nothing | runs it, prints `PAWS-OK` |
| `meow` TUI | 14 bytes (the size probe), then dead | full layout, streams a reply, `/quit` exits |
| `nca` TUI | — | ratatui layout, streamed **"Paris"** from z.ai, `out:44 $0.0007` |

---

## 3b. …and the rest of the terminal family was `ENOSYS`

**Status: FIXED 2026-09-19.** §3 made the TUIs *reachable*. They were still
wrong, and the screenshot is the whole bug report: `meow`'s footer printed
**once per 50 ms frame tick, scrolling down the screen**, dozens of copies of
`[Provider: mlx] [Model: …] [MEOW] awaiting user input...` instead of one row
repainted in place.

`amd64/src/usermode.rs`'s Akuma-private match implemented 300, 301, 302, 303,
313, 315-319, 322, 324-326 — and **nothing between 307 and 314**. Everything
else in that range fell through to `_ => errno::ENOSYS`:

| | | what its absence looks like |
|---|---|---|
| 307 | `set_terminal_attributes` | no raw mode: the session stays cooked, so input is line-buffered and the kernel echoes it on top of the layout |
| 308 | `get_terminal_attributes` | nothing to restore on exit |
| **309** | **`set_cursor_position`** | **the screenshot** — the footer prints wherever the cursor happens to be |
| 310/311 | `hide_cursor`/`show_cursor` | the caret flickers through every repaint |
| 312 | `clear_screen` | the TUI opens on top of whatever was there |
| 314 | `get_cpu_stats` | — |

All seven have real implementations in `akuma-syscalls-glue`'s `term.rs`, over
the same `ProcessChannel` + `TerminalState` pair §3 folded arm 313 onto. They
are now folded the same way and for the same reason, gated on the process
having a channel.

**The lesson is about the test, not the kernel.** §3's verification drove the
TUI over a pty and scored it on its *transcript* — the layout appeared, a reply
streamed, `/quit` exited — and every one of those is true of a TUI that cannot
position its cursor. The repeated footer was **in that capture**, and the
harness filtered it out as noise. A full-screen program has to be judged on the
escape sequences it emits, not on the words:

| | before | after |
|---|---|---|
| `ESC[r;cH` cursor positioning | 0 from the syscall path | **1980** in 25 s |
| hide/show-cursor pairs | 0 | **326 / 327** — one per footer paint |
| rows the footer paints to (41-row terminal) | wherever the cursor was | **37-41**, 744 paints at row 41 |
| one keystroke, no Enter | ignored (cooked) | redraws, renders the character |

That last row is `set_terminal_attributes` working: raw mode is what makes a
keypress an event instead of part of a line.

---

## 4. `nca`: `[custom stream error: error decoding response body]` against z.ai

**Status: FIXED 2026-09-19 — but it was not the whole story.** `nca`'s own HTTP
client gives up on a stream that goes quiet, and `reqwest` reports that as a
body error. That is a real bug and the fix below is proven. It is **not** why
z.ai is unusable from this box today — see §5, which the fix uncovered by
removing the error that had been masking it.

`crates/core/src/provider/custom.rs` built its client with
`.read_timeout(Duration::from_secs(60))`. That is an **inter-read** timeout: it
restarts on every byte, and expires when 60 s pass with none. A streaming chat
completion is exactly the shape that trips it — a few tokens, the model thinks,
the rest — and `glm-5.3-flash` through z.ai is sparse enough from this box to
cross it. Both failing runs died **116 s and 119 s** after the request, having
streamed a little first.

**The measurement**, with a server on the LAN that pauses mid-stream on purpose
(`gapserver.py` in the session scratchpad — SSE, chunked, plain HTTP):

| mid-stream gap | server side | `nca` |
|---|---|---|
| 40 s | `stream complete`, connection healthy | received it all |
| 70 s | `stream complete`, connection healthy | `"Before the gap. "` then **the error** |

The server finishing normally is the whole point: nothing closed, nothing reset,
no truncation. The client stopped listening at 60 s.

**Fixed** by one named constant, `provider::STREAM_READ_TIMEOUT_SECS = 300`,
used by all five providers — they all carried the same `60`.

### Why this cost a session, and what to do differently

**The error message names the wrong layer.** "error decoding response body"
reads as *the server sent something malformed*, which sends you to TLS, chunked
framing and the network stack. Three things were ruled out before the client was
even suspected: `git clone` moves 20 MB over HTTPS from a WAN host (so TLS and
the WAN path are fine), `nca` streams a 400-word answer from a LAN server (so
`nca`'s stack and long streams are fine), and an 18 KB `POST` to httpbin takes
1 s (so a large request body over a high-RTT link is fine).

**And a test server can fake this exact bug.** The first version of
`gapserver.py` sent `Transfer-Encoding: chunked` and then wrote unframed bytes.
`hyper` reported `error decoding response body` — the symptom under
investigation — and the server saw a broken pipe, which looked like the box
killing an idle connection. It was the harness. The tell was the timestamps: the
client failed *immediately*, not after the gap. **Check that a repro reproduces
the timing, not just the message.**

### Not this either: "the account is being throttled"

With the timeout raised, z.ai returned **nothing at all** — no error, no tokens,
for five minutes — and the first guess was rate limiting from the volume of test
requests. It was wrong twice over, and the second correction is the useful one:

* The account is healthy. Asked directly from a laptop, `glm-5.3-flash` answers
  in **2.5 s** on `https://api.z.ai/api/coding/paas/v4/chat/completions` and in
  **3.9 s** on `https://api.z.ai/api/anthropic/v1/messages`.
* The `429 {"code":"1113","message":"Insufficient balance or no resource
  package"}` that looked like proof of throttling came from asking the **wrong
  endpoint**: a Coding Plan key gets 1113 on the pay-as-you-go
  `/api/paas/v4` even with an active plan. Reading it as a billing problem
  cost a guess.
* And the box was holding a **stale API key** — a different md5 from the
  current one. Refreshing it changed nothing, which is what moved the
  investigation to §5.

### Not this: the 240 s ssh disconnect

Long commands over ssh to the box return `rc=255` at almost exactly **240 s**
(measured: 150/180/210 s fine, 240 s dead, twice). That is **the local ssh
client**, not the box: `~/.ssh/config` sets `ServerAliveInterval 60` and
`ServerAliveCountMax` defaults to 3, so the fourth unanswered probe lands at
240 s. `userspace/sshd` does answer `SSH_MSG_GLOBAL_REQUEST` — when it gets to
read one; it is not servicing the transport while a session command runs. Run
anything long **detached** (`( cmd > log 2>&1 ) &`, then read the log), which is
also what keeps a session teardown from being confused with the failure under
test.

---

## 5. TLS to `api.z.ai` — two of three stacks cannot talk to it. **OPEN**

**Status: OPEN 2026-09-19.** With §4 fixed and a current API key, `nca` still
gets nothing from z.ai. The failure is **per TLS stack**, not per network, and
that is what makes it worth writing down:

| from the box, to `api.z.ai:443` | result |
|---|---|
| TCP connect (`busybox nc`) | instant |
| **OpenSSL/musl** (`git ls-remote https://api.z.ai/`) | **1 s, a real HTTP 404 comes back** — full handshake, request and response |
| **rustls** (`nca`, tokio) | `connected to 8.2…` and then **nothing**, indefinitely |
| **libakuma-tls** (`hget`, `meow`) | **TLS handshake failed** in 1 s |

The controls matter as much as the rows: `hget` reaches **github** over TLS
perfectly, `git` moves 20 MB from github, and every one of these runs on the
same box, the same NIC and the same LAN. So the path, the NIC and this kernel's
TCP are all fine *for this peer* — OpenSSL proves the whole exchange works.
Something about that server's handshake defeats the other two stacks, and the
two fail differently: one errors, one hangs.

Next instruments, in order of cost: the handshake as OpenSSL sees it
(`git ls-remote` works, so the certificate chain and the cipher suite are
obtainable from the box), then `meow --debug` for libakuma-tls's own refusal,
then the kernel's `strace` boot flag against `nca` to see whether the socket is
being read at all during the stall.

**Do not re-derive the ruled-out parts.** §4's table covers the client timeout,
and a 70 s mid-stream silence over plain HTTP on the LAN is delivered intact —
so "idle connections get dropped" is answered and the answer is no.

---

## 6. `meow`'s repetition loop is the tool payload, not this machine

**Status: NOT A KERNEL BUG, root-caused 2026-09-19.** `meow` answering a plain
question with the same paragraph over and over — the symptom that opened this
whole thread — reproduces **laptop to `mlx_lm.server`, with the box nowhere in
the path**.

The request `meow` actually sends was captured by putting a logging proxy in
front of the inference server (`logproxy.py` in the session scratchpad; `meow`
stages its body in a temp file it does not leave behind, so sitting in the path
is the only reliable way to see it). Replaying that exact JSON:

| request | result |
|---|---|
| as `meow` sends it — system prompt + **17 tools** | 12 005 chars, **degenerate** |
| …plus `temperature: 0.7` | 12 001 chars, **degenerate** |
| same prompt, **tools removed** | 930 chars in 7.9 s, fine |
| bare prompt, no system prompt, no tools | 1 207 chars in 8.3 s, fine |

Per tool, each sent alone with the same prompt: `FileMove`, `FileReadLines` and
`CodeSearch` each degenerate on their own; `FileEdit` does not; the first eight
(`FileRead`/`Write`/`Append`/`Exists`/`List`/`Delete`/`FolderCreate`/`FileCopy`)
are fine together. So it is not a count threshold and not one malformed entry —
several of the definitions tip `Qwen3-Coder-30B` over by themselves.

**Sampling is not the cause**, which is worth stating because it is the obvious
first guess: `meow` sends **no** sampling parameter at all
(`"stream":true,"max_tokens":16384,"tools":…` and nothing else), so the server's
default applies — but forcing `temperature: 0.7` degenerates just as hard. There
is also no config knob for one, which is a gap worth closing independently.

The runaway guard in `userspace/meow/src/api/client.rs` is therefore working as
designed: `Stream cut off: model stuck in a repetition loop` at 8 KB is it
correctly ending a stream that really has gone degenerate. **The guard is not
the bug and neither is this kernel** — check the model and the tool payload
before either.

---

## 7. `busybox --install` — `link(2)` is not dispatched

**Status: OPEN, with a working alternative.** Every applet reports
`Function not implemented`:

```
busybox: /usr/bin/[: Function not implemented
busybox: /sbin/acpid: Function not implemented
```

That is `ENOSYS`. Plain `busybox --install` makes **hard links**, so it calls
`link(2)` — x86_64 **86**, one of the legacy spellings with no asm-generic twin
(the class `amd64/src/usermode.rs` keeps a shim list for: `open`, `stat`,
`unlink` 87, `symlink` 88, `readlink` 89 …). 86 has no arm, so it falls through
to `_ => errno::ENOSYS`. Confirmed directly: `busybox ln /bin/hello /tmp/x`
gives `Function not implemented`, while `ln -s` creates a real symlink.

**Use `busybox --install -s`** — symlinks, 402 applets, works today.

**Wiring 86 is not a one-liner, and that is the point.**
`akuma-syscalls-glue`'s `sys_linkat` **copies the file**: it `read_file`s the
source and `write_file`s the destination. There is no hard-link primitive in the
VFS or in `akuma-ext2` for it to call. Dispatching 86 to it would make
`busybox --install` write ~400 copies of a 1.1 MB binary and call them links —
a silent wrong answer, and `link(2)`'s callers use it precisely for the identity
and atomicity semantics a copy does not have. A real implementation needs a
directory entry pointing at an existing inode, `links_count` incremented, and
`unlink` decrementing it and freeing only at zero.

---

## 7b. The NIC watchdog fix bricked the boot — **twice**

**Status: self-inflicted 2026-09-19. Two dark boots, two trips to the machine.**
The defect is one line; the reason it shipped twice is the part worth keeping.

§2-era work left the RTL8169 stall watchdog firing on an idle link (`[rtl]
stall #3: kick misc 0x3f -> 0x3f mpc=0` — no missed packets, and the kick
changed nothing). The fix was to require evidence from the chip before calling
silence a stall: `INT_RDU` latched, or `MPC` advancing. The *signal* was right.
The **cost** was never asked about.

### Break 1 — the read on the idle path

```rust
let mpc = self.nic.snapshot().mpc;   // every lap, before deciding anything
```

`snapshot()` is **eleven MMIO register reads**. This is the receive poll loop —
thousands of laps a second, under the BKL — so that is eleven PCI transactions
per lap to answer a question that matters once every five seconds. The box
booted into it and never came back: no ping, 22 and 2222 both closed.
Networking comes up before `sshd` does, so a poll loop that cannot keep up never
reaches a state anyone can log in to.

### Break 2 — the fix that was not one

The reads were then moved behind the quiet window and shipped again:

```rust
let stalled = quiet && { let mpc = self.nic.snapshot().mpc; /* … */ };
```

**`quiet` is true on every lap once the window passes.** Nothing clears it while
there is no backpressure — `last_rx_us` is only refreshed when a frame arrives
or a stall fires — so on an idle link, which is this machine's normal state, the
eleven reads came back *permanently*, five seconds after boot. Same symptom,
same walk to the machine.

The comment shipped above that line read "never per lap". It was written from
intent rather than from the code, and asserting the property instead of checking
it is what sent a second dark kernel to the one target with no remote recovery.
**A claim in a comment is not a test.** The first patch was reasoned about; the
second was rushed *because* the first had already cost something — which is
backwards. A failure that costs a physical trip is the moment to slow down.

### The fix: deleted, not relocated

`MPC` is gone from the poll path entirely. `INT_RDU` already arrives in the
`ISR` that `take_interrupts()` harvests **every lap regardless**, so the
evidence costs nothing — and it is the better signal anyway: `RDU` means "the
ring ran dry with a frame waiting", which is exactly the condition being
detected. `MPC` added nothing that justified a register read.

The idle path is now: one saturating add, the `now_us()` that was already there
in the kernel which ran all afternoon, and one bool test. **The delta against
the last known-working build is a boolean** — which is the kind of argument that
should have been available before either of the two bad kernels was installed.

| kernel | change on the idle path | result |
|---|---|---|
| `65f00daf` | — | fine all afternoon, 775/0 |
| `30f1588c` | 11 MMIO reads per lap | **dark** |
| `dbde1114` | 11 MMIO reads per lap, 5 s after boot | **dark** |
| `50db3fc2` | one bool | to be verified |

### This already happened once, on AArch64

[`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) §11.7, "Measurement discipline this
section cost us to learn", ends with:

> **Instrument with O(1) counters.** `iter().count()` per poll made the meter a
> material part of what it measured — ~0.9 us/poll at 128 slots, ~14 us at 2048,
> which inflated the first 2048-slot experiment.

Same loop, same class of mistake, one architecture earlier. The difference is
only in what the per-lap work costs: an in-memory `count()` inflated a
*measurement*, while eleven MMIO reads across a PCI bus took out the *machine*.
So the rule generalises past instrumentation — **nothing goes on the per-lap
path that the lap does not need**, and a device register is the most expensive
thing you can put there.

The runbook's first rule is to grep `docs/archive/` before forming a theory. It
is worth reading as also covering the code you are about to write: this entry
existed, and finding it took one `grep` *after* the box was already dark.

### What let it through, both times

Every gate this tree has was green on both kernels that bricked the boot:
`cargo check`, `clippy`, and **1463 host tests passing**. None of them execute
the poll loop, and **no host test can see a livelock** — the failure is a timing
property of code running against real MMIO.

Nor would the fast lane have caught it, which is the uncomfortable part: local
QEMU is `-M microvm` with virtio-MMIO and the box's Firecracker is virtio too,
so **neither target runs `rtl8169.rs` at all** — the driver is behind a feature
only bare metal enables, and the Realtek exists on exactly one machine. Measured
after the fact: the corrected kernel passes the local trial **767/0 in 22 s**,
and both dark kernels would almost certainly have passed it too.

So for this one file there is no test anywhere that substitutes for reading the
code. What was available, and skipped: asking what a call costs on a loop that
runs thousands of times a second, and the `grep docs/archive/` the runbook opens
with — §11.7 below was already in the tree and took one `grep` to find *after*
the box was dark.

Recovery cost more than it should have, too: `Akuma/amd64 (known good)` pointed
at a **three-week-old** kernel, so each recovery landed the box without that
day's `git clone`, DNS, `poll_input_event` and terminal fixes, all of which had
to be reinstalled before work could resume. `.good` is worth exactly as much as
the kernel behind it — the runbook now says to promote it **after a kernel
proves itself**, never at install time (promoting at install would have
overwritten the fallback with the kernel about to hang).

For everything that *is* reachable by the fast lane, the runbook already
prescribes the order and it was skipped:

```
fast lane  ->  amd64_trials.py      (local QEMU + the box's Firecracker, no reboot)
   then    ->  hpbox.stage()
   then    ->  the metal
```

**A change to a driver hot path or the poll loop must go through the fast lane
first** — with one honest caveat, added after running it: **the fast lane would
not have caught *this* one.** Both of its targets are virtio (local QEMU is
`-M microvm`, the box's Firecracker likewise), and `rtl8169.rs` is behind a
feature only bare metal enables, so the changed code does not execute on either.
The corrected kernel passes the local trial **767/0 in 29 s**; the kernel that
bricked the box would very likely have passed it too. For this one file the
metal is the only target that runs the code, which makes reasoning about per-lap
cost the *only* defence there is — and is why §11.7 above is the load-bearing
part of this entry rather than the process note.

For everything else on this target the ordering still holds: Not because the metal is precious, but because it is the one target
with no remote way back: `/boot/akuma-amd64` *is* the default GRUB entry, so a
kernel that cannot serve ssh can only be replaced by someone standing at the
machine. The second menu entry, `Akuma/amd64 (known good)`, exists for exactly
this and is what recovers it — along with `/boot/akuma-amd64.prev`, which the
install step writes every time.

---

## 8. What works on the metal as of 2026-09-19

Kernel `020b4f16` plus §§1-3 and §3b. All over ssh to the box's own hardware.
What is **not** here is z.ai (§5) and hard links (§7).

* `git clone` over HTTPS — §1b.
* `meow -c` against an mlx server on the LAN: 657 ms to first token. The
  repetition-loop guard fires on the prompt that started this whole thread
  ("what do you know about this operating system where the agent runs") —
  `Stream cut off: model stuck in a repetition loop` at 8 KB / 32 s, instead of
  printing forever.
* `meow` TUI and `nca` TUI, both interactive — launched from **inside the login
  shell**, which is busybox (`/etc/sshd/sshd.conf` says `shell = /bin/sh`, set
  that way by `amd64/mkdisk.sh` since 2026-09-05; `paws` is on the disk but is
  nothing's default here). A bare `ssh -tt` session typed at human speed runs
  `echo`/`busybox`/`uname`/`exit` normally, `meow` paints over it and `/quit`
  returns to the prompt. `paws` is what *found* §3 — it is the smallest program
  that reads with `poll_input_event` — but busybox is the shell the box is
  actually used through, and it reads with `read(2)`, which is why it kept
  working throughout and makes the control half of that diagnosis.
* `nca` itself runs, one-shot and in its full-screen mode, and streams fine
  from a **LAN** provider (`TokensStreamed` deltas, cost reported). Against
  **z.ai** it no longer errors but returns nothing — that is §5, and it is the
  one thing on this list that does not work.

  Two operational notes either way: run `nca` **from `/`**, because
  `config.local.toml` is looked up at `<cwd>/.nca/` and `HOME` is unset on this
  target (nca logs `workspace data migration skipped error=HOME is not set` and
  carries on); and run anything long **detached**, per §4's ssh note.

Two things it still reports and neither is fatal: `IPC disabled: socket bind
failed: Address family not supported by protocol (os error 97)` — nca's AF_UNIX
control socket — and an `nca` process that outlives its one-shot turn.

## Background

- [`AKUMA_FROM_SCRATCH.md`](AKUMA_FROM_SCRATCH.md) — the goal this blocks; §3
  keeps the one-line statement of the requirement.
- [`AKUMA_AMD64_DNS_CONNECTED_UDP.md`](AKUMA_AMD64_DNS_CONNECTED_UDP.md) — pass 2's fix.
- [`AKUMA_AMD64_NO_SLOT_RECYCLER.md`](AKUMA_AMD64_NO_SLOT_RECYCLER.md) §3.1 — why
  the stuck git processes never went away, which contaminated pass 3's evidence.
- [`GIT_CLONE_STALE_ITIMER_SIGALRM.md`](GIT_CLONE_STALE_ITIMER_SIGALRM.md),
  [`GIT_MISSING_SYSCALLS.md`](GIT_MISSING_SYSCALLS.md) — the AArch64 git bugs.
  Both fixed **there**; `src/syscall/time.rs` is not code this target runs, so
  "fixed" needs checking against `amd64/` before it counts here.
- [`AKUMA_AMD64_STREAM_END_STALL.md`](AKUMA_AMD64_STREAM_END_STALL.md) — the
  other open "an edge that should have woken a reader did not" on this target.
- [`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md) —
  how to drive the machine, and the rigs that need no reboot.

### Pass 5 — ROOT CAUSE: `execve` never closed `FD_CLOEXEC` fds

**Fixed 2026-09-19.** Not a pipe bug, not a wakeup bug, not the network.

The kernel's own `strace` (boot flag, with `init=/usr/bin/git` on the networked
Firecracker rig) ends the parent's trace at:

```
[sc>] task=2 nr=3 a1=0x8      close(8)
[sc>] task=2 nr=0 a1=0x7      read(7)      <- never returns
```

fds 7/8 are the **notify pipe** git's `start_command` creates. The contract is
pure POSIX: the child's copy of the write end is `FD_CLOEXEC`, the parent closes
its own copy and blocks reading the read end, and a **successful exec closes the
child's copy**, draining the last writer so the read returns EOF. That EOF *is*
how the parent learns the exec worked. (A failed exec instead writes `errno` to
it.)

This target never closed those fds, so the write end survived into the exec'd
image, the pipe kept a writer forever, and the read never returned. git
therefore never sent `capabilities` to the helper it had just started — which is
why every observation looked like a pipe problem and none of them was:

* `[PIPE-DUMP]` showed `bytes=0 writers>0` on **every** live pipe — "the kernel
  is behaving, nobody wrote", which was exactly true.
* `pick=` advanced across censuses, so no thread was stranded.
* Driving the helper by hand worked in 0.9 s: no notify pipe is involved.
* `printf | git-remote-https` and `printf | git remote-https` both worked, and a
  two-level `sh -c 'sh -c cat'` pipeline worked — none of them exec a child
  whose exec-success the parent learns by EOF.

**Why it was here and not on AArch64.** `Process::close_cloexec_fds` is shared
code and had exactly one caller: `akuma-syscalls-glue`'s `sys_execve`, the
AArch64 path. `amd64/src/usermode.rs`'s `sys_execve` had no sweep at all. This
is the runbook's "when the archive says fixed, ask **fixed where?**" in its
purest form — nothing was regressed, the call site simply never existed here.

**The fix**, at the POSIX point of no return (after `install_image` commits,
never before — a failed `execve` must leave the fd table untouched):

* `akuma-exec`: `release_fd_entry` factored out of `SharedFdTable::close_all`,
  and `Process::close_cloexec_fds_releasing()` built on it. `close_cloexec_fds`
  alone only removes the *names*; the references still have to be dropped, and a
  second inline copy of that list is how the two kernels drifted apart.
* `amd64/src/usermode.rs::sys_execve`: calls it.

Also landed on the way, because the bug was invisible without it:
`amd64/src/pipe.rs::report` now calls `glue::pipe_dump()`. The per-pipe
breakdown existed but its only callers were in `akuma-kernel-glue` — AArch64
again — so the one diagnostic that separates a lost wakeup from an unwritten
pipe was unreachable on this target.

**Verified on the box's Firecracker guest, kernel built from the fix:**

| | before | after |
|---|---|---|
| `git ls-remote <small repo>` | hang (>50 s, killed) | **1.2 s**, real refs |
| `git clone <small repo>` | hang | **6.4 s**, `README` checked out |
| `git clone --depth=1` Akuma's own repo | never attempted | **~21 s**, 1617 files, `status` clean, HEAD `e30fda9` (tag v0.0.8) |

That last row is §3.1 of [`AKUMA_FROM_SCRATCH.md`](AKUMA_FROM_SCRATCH.md) — the
gate the whole document waits on.
