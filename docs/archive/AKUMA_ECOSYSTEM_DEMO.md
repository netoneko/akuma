# Akuma ecosystem demo: four Codex agents across two Denver guests

## The pitch

A single host agent brings up two Denver virtual machines. Two agents in separate
boxes on the first guest independently build and run Rust and Scala HTTP services.
A fourth agent on the second guest writes and runs Python integration tests against
both services over the network. The host can be Linux, using Firecracker or QEMU,
or Akuma using its virtualization support. The same workload and observable
results should work across host backends.

This is a **demo plan**, not a claim that these paths already work. In particular,
Denver needs an optional Codex-capable build and suitable guest images before the
agent workflow can run. Validate each host/backend combination independently.

```text
                         model providers / APIs
                           ^   ^   ^   ^
                           |   |   |   |  Codex model requests
+--------------------------|---|---|---|-----------------------------+
| Host: Linux (Firecracker or QEMU) OR Akuma (VM backend)             |
|                                                                    |
|  Agent A: Codex + Luna                                            |
|  coordinator: build/select images, launch, observe, intervene       |
|       | control / logs                 | control / logs             |
|       v                                v                           |
|  +---- Guest 1: Denver ----------------+  +---- Guest 2: Denver ---+|
|  |                                    |  |                        ||
|  | +-- box: rust -------------------+ |  | Agent D: Codex +       ||
|  | | Agent B: Codex + Luna          | |  | GLM-5.3-flash          ||
|  | | writes & runs Rust service     | |  | writes & runs Python   ||
|  | | HTTP on 0.0.0.0:8081         | |  | integration tests      ||
|  | +--------------------------------+ |  |          |             ||
|  |                                    |  |          | HTTP        ||
|  | +-- box: scala ------------------+ |  |          |             ||
|  | | Agent C: Codex + GLM-5.3-flash | |  |          |             ||
|  | | writes & runs Scala service    | |  |          |             ||
|  | | HTTP on 0.0.0.0:8082         | |  |          |             ||
|  | +--------------------------------+ |  |          |             ||
|  |       Guest 1 reachable IP          |  |          |             ||
|  +-----------------^------------------+  +----------|-------------+|
|                    +--------------------------------+              |
|                    :8081 Rust / :8082 Scala                        |
+--------------------------------------------------------------------+
```

A/B use Luna and C/D use GLM-5.3-flash: two agents per model, all using
Codex. Exact model identifiers, provider configuration, access and Codex
compatibility must be checked before the demo; “Luna” is the requested model
name, not a verified Codex model ID. Each agent needs a separate session and
workspace. Model calls require working outbound access and credentials from the
host or guests as appropriate.

## What to build first

1. **Opt-in Denver Codex support.** Add a feature/profile that composes the
   existing Denver JVM capabilities with whatever kernel features Codex, its
   runtime, subprocesses, TLS, networking and box execution actually require.
   Keep the existing default and JVM demo intact. Run the real Codex binary and
   workloads, capture failures, and add capabilities based on evidence. Confirm
   the chosen architecture, binary, model provider and authentication work.
2. **Guest images.** Create reproducible, versioned images with Codex and its
   runtime, shell and required utilities. Guest 1 needs `box`, Rust and Scala/JDK
   toolchains and dependencies available to the two boxes; guest 2 needs Python
   and a test runner. Preload dependencies or provide working package access.
   Allocate writable, separate workspaces and enough CPU, memory and disk for
   concurrent compilation and agent sessions. Never bake credentials into images;
   inject short-lived credentials at launch and keep them out of logs/artifacts.
3. **Networking and control.** Provide guest 1 with an address reachable from
   guest 2 and ensure both box services can listen on its external interface.
   Verify actual box networking and any forwarding needed; do not assume Docker
   `-p` semantics. Give A a usable control channel to start and observe the
   agents, plus guest outbound access to the configured model APIs. Firecracker,
   QEMU and Akuma may need different network setup; the launcher must make the
   same service addresses available to D for every supported backend.
4. **Launcher and preflight.** A single host script selects backend and either
   builds or uses pinned prebuilt kernel/guest images, creates independent writable
   disks, starts both guests, waits for readiness and launches the agents. Check
   virtualization support, architecture, images, capacity, credentials, provider
   connectivity, guest-to-guest routing and cleanup before starting the timed run.
   Preserve guest consoles, agent transcripts, service logs and test results.
   Prebuilt images must identify their source revision and digest. Do not claim a
   backend is supported until this entire path has passed on that backend.

The existing [Denver overlay](../../overlays/denver/README.md) is a starting
point for building and booting Denver, but its image directly starts a Clojure
server and does not provide this agent environment. The [box documentation](../../userspace/box/README.md)
and [multi-VM scripts](../reference/scripts/multi-vm.md) are useful starting
points, not proof that the planned topology works unchanged.

## Shared service contract

Give B, C and D the same contract up front, so independent implementation has a
clear target. Each service listens on all guest interfaces at its assigned port:

- `GET /health` returns HTTP 200 and JSON `{"status":"ok"}`.
- `POST /add` accepts JSON with integer fields `a` and `b`, each in the range
  -1000000 through 1000000, and returns HTTP 200 with JSON `{"result": <sum>}`.
- Missing fields, non-integers, out-of-range values and invalid JSON return HTTP
  400; the response body need not have a particular shape. Booleans are not integers.
- Services use `Content-Type: application/json` for successful responses and
  remain running for the test phase.

The Rust and Scala implementations must be independently written and run in
their respective boxes. The services are made reachable at guest 1's external
IP on ports 8081 and 8082; how this is achieved is backend/box dependent and
must be demonstrated. The launcher passes D the actual two base URLs, rather
than relying on hard-coded VM addresses.

## Demo sequence

1. A runs preflight, selects the backend and builds or selects the prepared
   images. The launcher boots both Denver guests and reports addresses and logs.
2. A starts B and C concurrently in separate guest 1 boxes, giving each the
   contract and its own workspace. They write, compile and run their respective
   services and report readiness and the commands/artifacts they used.
3. A checks that both endpoints are reachable from guest 2, then starts D there
   with the contract and actual URLs. D independently writes and runs a Python
   suite against both implementations, including health, positive and negative
   sums, boundary values and invalid requests. It reports per-service results
   and exits nonzero on failure.
4. A monitors progress and intervenes only if needed. Record interventions and
   failures honestly. Show the live agent work, both running services, requests
   crossing from guest 2 to guest 1, and the final test report.
5. Save source, commands, image identities, logs and test output. Stop the
   agents, services and guests and remove demo-created networking resources;
   preserve the results for review.

A successful run shows all four Codex sessions with the requested model split,
two separately boxed services answering on guest 1, and D's suite passing
against both from guest 2. Repeat the same acceptance run on Linux/Firecracker,
Linux/QEMU and Akuma with a supported VM backend; record the actual backend,
architecture and results for each. Any unimplemented or failing combination
remains an explicit gap, not a demonstrated success.

## Review, scope and estimate (2026-11-19)

**Nested boxes are not required.** Every box here is created by a supervisor that is already allowed to (box 0 of the
guest, or the launcher A drives): B and C are two sibling boxes, D is a process in another VM. Nothing creates a box from
inside a box. Nesting becomes relevant only when each agent should *sandbox its own tools* (Codex running bubblewrap inside
its box), which is the later step in `AKUMA_NESTED_BOXES.md`; the demo can ship without it and gains a stronger story when it
lands.

**What it does need, found while reading the tree:**

* **Boxes on the guest kernel.** *(Correction 2026-11-22: Denver has an opt-in (solid, not experimental) `containers` feature, `DENVER_BOX_WIRING.md`; the sentence below is the 2026-11-19 planning state and is stale for Denver. Taoxichuan and Shanghai still have none.)* Denver (a tokio kernel) had no box support: no `box` syscalls, `current_box_id` is `0`
  (`KERNEL_CONVERGENCE.md`). So "box: rust" and "box: scala" on a Denver guest do not exist today. Three ways out, in order of
  cost: (a) guest 1 is the **main amd64 kernel** (boxes, `box`, Codex already run there) if a JDK/Scala toolchain works on it,
  unverified; (b) B and C run as plain processes with separate workspaces and the demo says so; (c) wire the box registry into
  Denver (convergence step 4). Pick (a) or (b) first.
* **Thread capacity.** A JVM plus Codex plus compilers on one guest will want more than the tokio kernels' 48 task slots
  (`threads_128` is an expected failure in the gate). Measure a Scala build and a Codex session against it early.
* **Two guests that can reach each other.** QEMU user networking (SLIRP) gives each guest its own private network; guest 2
  reaching guest 1's :8081/:8082 needs a shared segment (a bridge/tap, QEMU `-netdev socket`, or a forward through the host).
  Decide this first; it is the likeliest day-one surprise. Firecracker needs its own tap setup.
* **Codex and the model names.** "Luna" and "GLM-5.3-flash" are unverified Codex model ids (the doc says so); the provider
  config and auth are part of the first day, not the last.

**Estimate (the author's, not measured):** about one day to a working end-to-end run on one backend (Linux QEMU, main-kernel
guests, plain processes for B and C if needed), then several days of presentation and polish: the launcher, preflight, the
transcript and results capture, a repeatable acceptance run per backend. Cut first if time runs short: Firecracker and the
Akuma-host backend, then the Scala service (keep Rust and Python).

**What running it validates on main**, so it doubles as a pre-merge acceptance: Codex on the main amd64 kernel, `box`
isolation with concurrent agents, cross-VM TCP, JVM and Rust toolchains under load, many threads and processes, TLS to a model
API, and a Python test run over the network. Anything that fails is a gate probe waiting to be written
(`docs/runbooks/merge-gate.md`).

## Build order and the dry run (decided 2026-11-19)

### Backend order

1. **QEMU, on the ryzen** (this doc's first target): build the images and the launcher here, where the whole loop is local
   and fast (KVM, `scripts/gate.py`-style boots on a snapshot of a read-only image).
2. **Akuma VMs** (the SVM hypervisor on the ryzen), once the same images pass on QEMU.
3. **Firecracker**, last, as polish: its network and block setup differ and it is the backend with the most to re-learn.

Nothing moves to the next backend until the dry run below passes on the current one.

### Set everything up beforehand, then prove an agent can do the job

An agent that fails because the image lacked `git` or the dependencies were not cached tells us nothing about the agent.
So **the demo is rehearsed by us, by hand, before any agent sees it**, on the exact images the agents will get:

1. **Stage the environment** (no agent): build the guest images and the launcher; give each box its tools, its dependency
   cache and an empty workspace.
2. **We write the three services ourselves** (Rust, Scala, and the Python test suite from the contract below). Stage the
   sources into the boxes the way an agent would receive a task, compile them, run them, and run the Python suite from guest 2
   against both. Fix every gap the image, the box or the kernel shows. Keep these sources: they are the reference solution
   and the oracle for the agents' results.
3. **Hand each agent an empty box** with the same tools and ask it to repeat what we did: write, build, run, commit. Compare
   against the reference. A failure here is either an agent failure (record it) or an environment failure (fix the image and
   rerun from step 2; the dry run is not done until step 3 needs no environment fixes).

The gate's `gateprobe` pattern applies: each thing the rehearsal needed that was missing becomes a probe or an image check, so
the next image build proves it.

### Roots and tools

* **One root per language box.** The Rust box and the Scala box each get their own root filesystem (own `box` image/layers,
  own writable upper), not a shared one with both toolchains: the Rust root carries `rustc`/`cargo` and its registry cache;
  the Scala root carries the JDK, Scala and its dependency cache. Isolation, image size and "what the agent can see" all
  follow from that, and it exercises `box`'s per-box roots rather than a shared tree. Guest 2 (Python) is a third image.
* **Agents get ordinary developer tools**, not a stripped sandbox: `bash`, `git` (so they can commit their work in their
  workspace; the transcript includes the log), `apk` (so they can install what they find missing, with the package mirror or a
  prepopulated local cache reachable), plus coreutils, `curl`, an editor-less workflow is fine. Never bake credentials in; the
  git identity is a throwaway one set at launch.
* The workspace is a **git repository per agent**, created empty at launch, so "the agent's work" is a commit history that
  survives the run and can be diffed against the reference solution.

### Checklist before step 3 (agents)

| check | how | done when |
|---|---|---|
| image per root builds reproducibly | a build script; source revision and digest recorded | rebuilt twice, same digest |
| tools present in every root | `bash`, `git`, `apk`, `curl`, the language toolchain run in each box | a script runs them, exits 0 |
| dependencies reachable offline or via the mirror | build the reference service with the network policy the agent will have | clean build from an empty workspace |
| the reference Rust and Scala services pass the Python suite from guest 2 | manual run, then scripted | 100% of the contract's cases |
| guest 2 reaches guest 1's `:8081`/`:8082` | the launcher's preflight | `curl /health` from guest 2 |
| an empty box can be handed out and reset | launcher | box recreated from the image in seconds, workspace empty |
| the kernel handles the load | `scripts/gate.py`-style probes for what the rehearsal exercised (threads, processes, TLS, big builds) | no kernel-side surprises during step 2 |

### Decision: guest 1 is the main amd64 kernel (2026-11-19)

Guest 1 runs **Akuma (the main amd64 kernel)**, not Denver: it has herd, `box` and the bootstrap overlay, which the boxes
for B and C need and Denver lacks (`KERNEL_CONVERGENCE.md`). This resolves option (a) above; (b) and (c) are dropped for now.
The open risk it leaves is whether a JDK and Scala run on it: stage 1 tests that before building the Scala image, and a
failure is reported with the failing syscall rather than answered by switching kernels. Guest 2 (Python tests) uses the same
kernel and bootstrap so one image mechanism covers the demo; revisit only if Python cannot run there.

## Stage 1 outcome (2026-11-19, branch `ecosystem-demo-stage1`)

**Status: stage 1 passes on QEMU+KVM on the ryzen with the main amd64 kernel as both guests; no agent, no demo application.**
Code and runbook: [`overlays/ecosystem-demo/`](../../overlays/ecosystem-demo/README.md). This section records what is true now; the
sections above are the plan it executed.

### What passed

`overlays/ecosystem-demo/launch.py verify` (36 checks, 0 failed on the last run): tampered bootstrap refused; bootstrap verified and run
on both guests; secrets root-only (700/600) and fetched exactly once; rust (`rustc`/`cargo` 1.87), scala (OpenJDK 21, Scala 3.3.6, a gson
jar from the dependency cache) and python (3.12, pytest 8) toolchains run **each in its own box**, alongside bash, git, apk, curl and GNU
coreutils; one-line compile-and-run in each box (toolchain checks only); `git init` + commit in the empty `/work` with the throwaway
identity; `apk add jq` from the pinned local mirror; a box destroyed and recreated from its image with an empty workspace in **0.3 s**;
from inside a box the metadata address is unreachable (the host root reaches it; the server logged nothing for the box); a box cannot
read another agent's secret (host `/var/lib/agents`, `/proc/*/environ` of a running sibling, its own filesystem); no secret value in
either disk image or any launcher/serial log; guest 2 (and its python box) reaches guest 1 `:8081`/`:8082`, served from the rust and
scala boxes by stock `busybox-extras httpd`.

Boot to ssh-ready for both guests in parallel: 21 s. Scala hello (compile + run): 3.2 s. A Scala build beside a JVM with 150 threads and
60 sleeper processes, plus a cargo build: both pass; the kernel logged **113 live user threads at its last high-water step of 512**
(a lower bound, E6).

### Kernel gaps, and what was done

| gap | syscall / errno | status |
|---|---|---|
| **JVM dies before `main`**: `SIGSEGV at pc=0xffffffffff600800` | `getcpu` (x86_64 nr 309) returned `-38 ENOSYS` (`strace_err`: `nr=309 -> -38`); HotSpot then falls back to the legacy vsyscall page, which is not mapped | **fixed**: one arm in `crates/akuma-kernel-amd64/src/usermode/dispatch.rs` calling the existing, host-tested `akuma_syscalls_glue::sched::sys_getcpu`; gate probe `getcpu` added. Kernels that depended on the old behaviour: only the main amd64 kernel was changed; taoxichuan/shanghai/denver amd64 are unchanged (denver wires it behind its `getcpu` feature) |
| box child stdout not delivered to an ssh exec session | none (no errno; output lost) | **open**, E1 in `proposals/FUTURE_WORK.md`; `eco-box` captures via a file |
| path through a relative directory symlink inside an overlay box fails | `ENOENT` | **open**, E2; the scala root has no such link |
| no outbound route / NAT for the bridge | n/a | **open**, E3 |

### Images

Identity of a root is the sha256 of its uncompressed, name-sorted tar (owner 0, fixed mtime). Rebuilt twice from the pinned inputs, **all four
roots are byte-identical**; the **guest disk images are not** (E4: `mkfs.ext2 -d` copies host ctime/atime). `MANIFEST.json` in the output
directory records the source revision, the spec and lock digests, the akuma userspace inputs and every digest. This run:

| root | diff id (sha256 of tar) | packages |
|---|---|---|
| host (herd, sshd, box, curl, busybox) | `416c80905e54a8762e2c863b8304dbc574cf8bfbfd66e56efe57514a87c10723` | 21 |
| rust (rustc, cargo, gcc, musl-dev; no cargo cache by default) | `06ada2e3466d4ee9301f58928a09d3c71200e8a4ea9edb7e14851d8a481eba14` | 62 |
| scala (OpenJDK 21, Scala 3.3.6, gson) | `4dad3b8f157bf3db0409b9638cfca2405f21f9794dad5321422086603f5549a1` | 71 |
| python (python3, pytest, requests) | `8c45712eccada082a30232134867cb1214442f2c5a214441860b9b8d22780cb5` | 78 |

Digests move with the source revision of the akuma userspace binaries (herd embeds its revision) and with the pinned lock; the numbers
above are from a tree with this branch's uncommitted-at-build changes, so re-run `build-images.py all` and read `MANIFEST.json` rather than
trusting this table after the next commit.

### Decisions that changed the plan

* The guests share a **bridge + tap inside an unprivileged namespace** (README there explains why not SLIRP, `-netdev socket` or a host bridge).
  Consequence: no internet in the guests (E3), so `apk` uses a pinned local mirror.
* Cargo's offline registry cache is **off by default** (requested); `build-images.py --with-cargo-cache` vendors `images/rust-deps`.
* The Scala "dependency cache" is the distribution's own jars plus `/opt/deps/gson-2.11.0.jar`; there is no sbt/scala-cli or Maven cache. The
  reference services in `../akuma-demo-reference-impl` use `scala-cli` and a pinned git crate, so **they do not build in these images yet**; adding
  `scala-cli` and its caches (or vendoring the `gitserver` crate with `--with-cargo-cache`) is the next stage's job.
* "Only the launcher of the agent that owns a secret passes it on" is `eco-box exec NAME ...` reading `/var/lib/agents/NAME/secrets/*`. The value is
  visible in the `box run -e` argv of the host root while it runs.

### Reproduce

```sh
cargo build -p akuma --target x86_64-unknown-none --release --no-default-features --features headless,no-tests
overlays/ecosystem-demo/build-images.py fetch && overlays/ecosystem-demo/build-images.py all
overlays/ecosystem-demo/build-images.py repro
overlays/ecosystem-demo/launch.py verify
scripts/gate.py
```

## Stage 2 outcome (2026-11-19, branch `ecosystem-demo-stage1`): internet, then the reference services

Goal: give the boxes internet, download dependencies there (Maven for the Scala board, cargo + a git dependency for the Rust git
server), run the reference services and the reference acceptance suite (`akuma-demo-reference-impl/tests/test_remote.py`) from
guest 2. Driver: `overlays/ecosystem-demo/refdemo.py`. **Status: passes (2026-11-19, after the E7 detach fix below): both services run and the reference acceptance suite
passes from guest 2 (`refdemo.py`: 19 ok/info, 0 failed).** Before the fix the Scala board could not be started because a guest
kernel panicked (E7).

### Networking was replaced, not extended

The bridge + unprivileged-namespace design had no way out and the box has no network setting of its own (a box uses its
guest's stack), so the launcher was the place to fix it. Instead of a userspace NAT (nft redirect + relay), each guest now uses
QEMU user networking: internet through QEMU's NAT; `hostfwd` for ssh and guest 1's :8081/:8082; `guestfwd` for the metadata
address (one server per guest); `host=169.254.0.2` is the host as seen by a guest, which is how guest 2 reaches guest 1 and
how a service pushes to the git service on the same guest. The SLIRP network is 169.254.0.0/16 because `guestfwd` only accepts
addresses inside it. The stage-1 checks (`launch.py verify`) were ported and re-run on it.

### What passed

* `https://example.com` (DNS + TLS) from the rust, scala and python boxes; the rust box reaches `index.crates.io` and Maven Central.
  The guest's host root had no `/etc/resolv.conf` (boxes get one injected), so `curl` there failed until the root got one.
* **Rust git server**: `cargo build --release --locked` of the reference `git-server` in the rust box, with crates.io and the
  pinned GitHub git dependency, **~305-365 s** with `-smp 2`; the server starts, `GET /healthz` answers from guest 2, the
  reference suite's git test passes, a box on guest 1 pushes to it through `169.254.0.2:<port>`.
* **scala-cli** (JVM jar, behind a launcher) runs and compiles the task board's dependencies from Maven Central, once the JVM
  has trust anchors (below).

### What was wrong, and what was done

| symptom | cause | action |
|---|---|---|
| `cargo` refused crates: rustc 1.87 | Alpine's rust is older than the pinned crates need | the official `rust-1.96.0` musl toolchain (sha256-pinned) replaces Alpine's `rust`/`cargo` in the rust root |
| scala-cli (native) `Failed to create the main Isolate (code 8)` | GraalVM native image fails to start here | scala-cli runs as its JVM jar (`scala-cli.jar`, pinned) |
| scala-cli jar: `directories are not supported on Akuma` | `uname` sysname `Akuma` becomes `os.name` | launcher passes `-Dos.name=Linux` (E8) |
| every JVM https download failed (`trustAnchors ... non-empty`) | Alpine builds `cacerts` in a trigger; triggers do not run in these roots | `build-images.py` writes `/etc/ssl/certs/java/cacerts` (JKS, fixed timestamp, byte-reproducible) from the root's CA bundle |
| `cargo build` sometimes: `extern location ... .rlib does not exist` | not understood (E9) | recorded; `refdemo.py` retries once and prints attempt 1 |
| **guest 1 kernel panic** during `scala-cli run` of the board | `execve` from a non-leader thread replaced the group's address space under its siblings (`[execve] from a non-leader thread with live siblings`, then a ring-0 `#PF` in `sys_mmap`) | **fixed by detach**: the exec runs as a new process and the old group ends (E7, `docs/archive/EXEC_FROM_THREAD_DETACH.md`); the Linux-style handler is future work (E7b) |

### Open

* The exec'd program has a new pid (E7 detach); the Linux-style `execve` from a thread is E7b in `proposals/FUTURE_WORK.md`.
* Firecracker still needs the same shape (tap + MMDS) in stage 3.

## Stage 3 trial: three Codex agents (2026-11-22, branch `demo-trial-run`)

Driver: `overlays/ecosystem-demo/agents.py` (A, the coordinator, is the script, not a model). Each agent gets its **own brief**
(`overlays/ecosystem-demo/briefs/`, the contract section(s) it needs appended), delivered through the metadata service and copied into
its box; Codex runs inside the box on the main amd64 kernel (`sandbox_mode = danger-full-access`, `code_mode_host`). Credentials are
per-agent secrets: the Z.ai key for the GLM agents, the Codex ChatGPT login (base64) for B. Z.ai serves the Responses API at
`https://api.z.ai/api/v1` (`wire_api = "responses"`; the coding-plan chat endpoint is not what Codex 0.169 can use).

| agent | box / guest | model | result |
|---|---|---|---|
| B | rust, guest 1 | gpt-6.1-terra | Git Smart HTTP service, 1 commit, **734 s**; passes the reference git test |
| C | scala, guest 1 | glm-5.3-flash | task board, 9 commits, **1878 s**; **fails** the reference board test (PATCH with an invalid value returns 404, contract says 400) |
| D | python, guest 2 | glm-5.3-flash | 46-test suite, 5 commits, **455 s**; its `REPORT.md` found the same board defects (7 failing checks) and passed all 5 git checks |

D's independent verdict and the reference suite agree. Two services reachable from guest 2 across the SLIRP host alias, started by A
from each agent's `/work/run.sh`.

Environment faults found by the trial (the dry run's purpose): the guest disks had 400 MB headroom, so the first run died with
`ENOSPC` inside Codex; `build-images.py` now takes `ECO_HEADROOM_MB` (used 6144). Alpine's APKINDEX had moved, so
`fetch --relock` was needed. Stage-1 `verify` still fails one capacity check (a Scala build beside 150 JVM threads gets `EAGAIN`
from `pthread_create`; peak 257 of 512 kernel threads), not exercised by the agents' runs. No agent was intervened on.
