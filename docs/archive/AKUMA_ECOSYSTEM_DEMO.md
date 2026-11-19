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

* **Boxes on the guest kernel.** Denver (a tokio kernel) has no box support: no `box` syscalls, `current_box_id` is `0`
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
