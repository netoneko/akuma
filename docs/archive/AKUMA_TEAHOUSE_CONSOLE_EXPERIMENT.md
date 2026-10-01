# The teahouse builds a TV console — experiment record (opened 2026-10-01)

**Status: PLANNED — nothing below the "Prep done" line has been run on a live
member yet.** This page is the plan and the log. Where a fact is unverified it
says so; update the log as steps happen and date any correction.

## Why this experiment

The 09-25 loop had meow build and boot a kernel a human wrote
([`AKUMA_SELF_HOSTING_AMD64.md`](AKUMA_SELF_HOSTING_AMD64.md)). The next rung
named there is **cats that submit patches, and several cats on one task**. The
first attempt, Intel HDA audio ([`../runbooks/add-intel-hda-audio.md`](../runbooks/add-intel-hda-audio.md)),
ran out of tokens and its "verified" readbacks turned out to be a phantom
widget; a human rewrote the driver. A new hardware driver was too hard a first
patch. This is the easier one.

**The feature:** an interactive console on the TV attached to the trashcan.
**Acceptance (the user's words): "I want to see `/bin/sh` after the trashcan
boots"** — a prompt on the screen that takes keystrokes from the keyboard and
runs commands, with no ssh involved.

## What already exists (read from the tree 2026-10-01, not run)

- A **framebuffer console** under GRUB/multiboot2 (`amd64/src/multiboot2.rs`,
  `FbConsole`); `serial::puts` already mirrors output to it. The TV is already
  the box's only console.
- A **keyboard**: `amd64/src/kbd.rs`, i8042 scancode set 1, polled, working
  through the firmware's USB-to-PS/2 emulation (no USB stack involved). No
  arrow or function keys, no repeat handling beyond the keyboard's own.
- A **console input pump** (`amd64/src/console.rs`, `input.rs`) and a shared
  `TerminalState` line discipline, so a process attached to the console reads
  keystrokes and echoes to the screen.
- Boot runs `init=/bin/herd`, which starts `sshd` (and `hda-capture` on this
  box). **Hypothesis, unverified:** nothing starts a `/bin/sh` on the console,
  so the TV shows the boot log and then nothing. If true, the minimum feature
  is a herd service (or init argument) that runs `/bin/sh` on the console, plus
  whatever the shell needs from the line discipline to be usable.

Whether the hypothesis holds is the cats' first deliverable, not an
assumption.

## The team

| cat | seat | model | job in this experiment |
|---|---|---|---|
| meow | Akuma, bare metal (the trashcan, `dumpster-akuma-amd64`) | Kimi Code, via `kot --kimi` (new, below) | owns the kernel/userspace change **on the box**: `kbuild`, `kinstall`, reboot, look at the screen's state through `dmesg` |
| tama | Linux, ryzen (`ryzen-linux-amd64`) | Kimi Code | reviews, researches, and can run the kernel under QEMU on ryzen to check a change before it ever reaches the metal |

Both rows were GLM `glm-5.3-flash` at reasoning `low`
([`../../../akuma-miot/docs/FLEET.md`](../../../akuma-miot/docs/FLEET.md)). The
point of the switch is to see whether a stronger model gets further than the
HDA attempt did.

**Model choice.** The ask was "Sonnet-level equivalent". Kimi Code is reached
through one alias, `kimi-for-coding` (`miot_llm::KIMI_MODEL`), which the
service maps to whatever it currently serves; I have not been able to list its
models, so *what that alias is* is unmeasured. It gets set per cat in
`overlays/deploy/deploy.py` (`Agent.model`) and can be changed by redeploy.

## Prep done 2026-10-01 (in `akuma-miot`, uncommitted)

- `miot_llm::Llm::kimi` — OpenAI-compatible `https://api.kimi.com/coding/v1/`,
  token from a file, `max_tokens` 16384, no `reasoning_effort` sent.
- `kot run|chat --kimi` (`MIOT_KIMI`, token file `~/.akuma/kimi/token` locally,
  `/root/kot/kimi.token` on a member), default model `kimi-for-coding`.
- `deploy.py`: an `llm="kimi"` kind that writes `MIOT_KIMI*` into the member's
  env and ships the token file (mode 600), like the GLM one.
- `cargo check -p kot -p miot-llm` is clean. **Smoke-tested 2026-10-01** with
  `kot chat --kimi` on the mac: the OpenAI route answers, a tool call
  (`AboutMe`) and its result round-tripped, and usage reports cached tokens
  (2,816 of 3,044 on the second turn). The alias identifies itself as
  `kimi-for-coding`; what model is behind it is still unmeasured.

**OpenAI vs Anthropic route:** the OpenAI route works (above), so the
Anthropic-compatible one is not needed. It would only matter for explicit
`cache_control`, and the OpenAI route already reports cache hits.

## State of the mesh found 2026-10-01 ~10:30 local

From kuro (`kot peers`, quorum 4 of 7, last checkpoint #54182): **no leader.**
kuro and tama are up and stuck at `pre-candidate`, term 305; yuki and shiro
were last heard ~20 minutes earlier and `kot.akuma.sh:9441` does not answer
from the mac; sora's last sighting is ~14.5 hours old; meow's is ~18 hours
old; mimi is absent. The trashcan itself is **up on Akuma** (uptime 21 h, ssh
on `.120:2222` answers) but its `/etc/herd/enabled` holds only `hda-capture`
and `sshd` — **kot is not enabled there** (`/root/kot/kot.conf.prev` exists),
so meow is not a member right now.

Consequences: a chain write — including **compaction** and the planning
prompt — cannot land until four members are up and elect a primary. Putting
meow back (needed anyway) and tama gives three reachable with kuro; one of the
AWS pair or sora/mimi is still needed.

## Why the AWS pair was down (Kirill, 2026-10-01)

The AWS box ran out of memory: **no memory discipline had been applied to the
agents** (406 MiB host, two 128M containers, ParityDB chains of ~140 MB each).
Observed: yuki's container `SIGKILL`ed at 06:28:40 UTC with ~20 MB free and 62 MB
of swap in use, then a restart that failed on a stale machine registration.
The OOM itself was not confirmed from a kernel log (my `dmesg` grep returned
nothing); the cause is the user's, the symptoms are mine. A fresh chain starts
small, but nothing in this experiment yet bounds how big it grows — see the
open item below.

## Steps

1. ~~Smoke-test Kimi locally~~ — done, passed.
2. **Redeploy meow** (`deploy.py up dumpster-akuma-amd64`) with the Kimi kind.
   Reinstalls kot's herd conf on the trashcan.
3. **Redeploy tama** (`deploy.py up ryzen-linux-amd64`). *ryzen is the user's
   live box; the user approves this step explicitly before it runs.*
4. **Reach quorum** (four of seven with a primary) — see the state above.
5. **Compact the chain** (`kot compact`, root-only) so the cats start the task
   with a short history instead of weeks of teahouse chatter.
6. **Post the planning prompt (below) to meow and tama. They discuss; no code
   yet.** The user and Claude read the plan and approve or amend it.
7. Only after approval: open the task(s) on chain and let the cats work.

## The prompt for the cats (step 6)

Posted 2026-10-01 to tama and meow as two targeted `say`s. A message is capped
at 2048 bytes (`MaxMessage`), so this is the compressed form; the first
draft, at 2228 bytes, was refused `TooLong`.

> You two (meow on the trashcan, tama on ryzen) will build one feature together. Before any code, I want your plan for cooperating on it. Discuss it here and agree.
>
> FEATURE: boot the trashcan, look at the TV, see a working /bin/sh prompt that takes keystrokes from the keyboard and runs commands. No ssh. The framebuffer already shows the boot log and kbd.rs reads a PS/2 keyboard via firmware emulation. Nothing is known to start a shell on the console; nobody has confirmed the keyboard works. Check both first.
>
> LOOK AT: amd64/src/{console,input,kbd,multiboot2}.rs, usermode.rs run_init, /etc/herd/enabled, docs/runbooks/amd64-bare-metal-loop.md ("Working on the box"). grep docs/archive before theorising.
>
> RULES (from the HDA attempt): a readback is evidence only if you know the thing it reads exists. "It printed OK" is not "it works"; say how you would tell. Commit locally before every install; do not push, touch credentials, or force anything. If a kernel won't boot, a human picks the old GRUB entry.
>
> REPLY WITH:
> 1. The real gap, and what you will run to confirm it before changing anything.
> 2. Who does what. meow can build/reboot on the metal (minutes per reboot, bad kernel needs a human); tama has a fast box and QEMU but cannot see the TV. Put tama's checks before meow's reboots.
> 3. How you hand work over (chain? commits? branch?) and avoid editing the same file.
> 4. What "done" looks like on the screen, and your fallback if the keyboard does not reach the shell.
> 5. Each of your smallest first steps, and what would make you stop and ask a human.
>
> Do NOT start building. Post your plan; a human will approve it.

## Log

- 2026-10-01 — doc opened; Kimi provider wired in `akuma-miot` (uncommitted),
  compiles and passes a live smoke test. Mesh found leaderless; meow's kot service found disabled. Nothing
  deployed, nothing posted, chain not compacted.
- 2026-10-01 ~13:40 IDT — built `dist/x86_64/kot` locally (static musl) and
  deployed over HTTP: **meow** (herd `kot.conf` re-enabled, process up, heard
  by the mesh) and **tama** (systemd, `deploy.py up`, user-approved). Both on
  `kimi-for-coding`, window 262144 (assumed), `MIOT_COMPACT_EARLY=128000`. The
  token went over the ssh channel (`put_secret`), not HTTP. `deploy.py` gained
  an HTTP path for the `linux` shape's binary.
- 2026-10-01 — quorum still short: kuro, tama, meow see each other (3 of 4
  needed). On the AWS box (`kotctl list`): **yuki and shiro were `failed`**
  since 06:29 UTC. The yuki container was `SIGKILL`ed at 06:28:40 (cause not
  confirmed; the box has 406 MiB with ~20 MB free) and both restarts failed
  with `Failed to register machine: Machine 'kot-yuki' already exists`.
  `reset-failed` + start brought **shiro** to `active`, but its kot still does
  not answer on `:9442`. **yuki** still fails: `machinectl terminate kot-yuki`
  leaves the stale registration; further inspection of the AWS box was
  declined by the permission classifier and is left for the user.
- Not done: chain compaction and the planning prompt (both need a primary).
- 2026-10-01 ~13:55 IDT — **decision: stop everything, back up the old chain
  on ryzen, delete it everywhere else, start a fresh chain** (the mesh could
  not elect a primary: 3 of the 4 needed were up).
  - Stopped: tama (ryzen), kuro (Lima `fc`), yuki + shiro (AWS containers).
  - **Backup:** tama's chain, the highest head of any member (67812), moved to
    `/root/kot/db.pre-fresh-2026-10-01` on ryzen — 152 files, 139,791,368
    bytes, every sha256 verified against `/root/kot/db.pre-fresh-2026-10-01.sha256`.
    tama's live `db/` is empty.
  - **Deleted:** kuro's chain (`/root/kot/db` in `fc`), yuki's (142 MB) and
    shiro's (139 MB) under `/var/lib/machines/kot-*/kot/db` on the AWS box.
  - **Not reachable, so NOT wiped:** meow (the trashcan went off the network
    during the stop command: no ping; whether its `kot.conf` enable was
    removed first is unknown), mimi (Firecracker guest not running, `:4444`
    refused) and sora (guest unreachable). Their old chains are still on their
    disks.
  - **Why the genesis must change:** a store records a hash of root, leader
    and roster (`node.rs` `genesis_fingerprint`) and *refuses* a different
    one, but a store with no record is adopted with a warning. A fresh chain
    on the *same* genesis would accept those three stale chains if the
    machines come back. Changing the leader or the roster makes them refuse
    loudly instead.
- 2026-10-01 — new genesis (leader yuki -> tama, all 7 seats kept) written to
  `mesh.env` (old copy: `mesh.env.pre-fresh-2026-10-01`), `deploy.py` and the
  AWS box's `/etc/kot/genesis.env` (old copy beside it). tama and kuro are up
  on it with empty chains; shiro and yuki containers restarted `active` and
  shiro is following tama (term 1). meow, sora, mimi still down; quorum needs 4.
- **Open: memory discipline for the agents** — per-container limits, how large
  a chain may grow before compaction, and the AWS box's swap. Not designed yet.
- 2026-10-01 ~14:05 IDT — **contamination, caught and cleaned.** The user
  power-cycled the trashcan; herd started its *old* `kot` (old `start.sh` env,
  old 65k-block chain). It won an election (term 307) and the freshly wiped
  members began pulling the old chain back from it. A genesis fingerprint is
  only checked against a node's *own* store, never exchanged between peers, so
  the new-genesis members did not refuse blocks from an old-genesis leader.
  Fix: killed meow's `kot` (SIGTERM ignored; `kill -9`), removed its
  `kot.conf` enable, wiped meow, tama, kuro, yuki and shiro again (ryzen
  backup re-verified: 152 files, 0 bad checksums), then started tama, kuro,
  the AWS pair and — via `deploy.py up`, which rewrites `start.sh` — meow.
  **sora and mimi still hold old chains and are down; if either comes back
  before it is wiped it can do the same thing.** A real fix is for the mesh
  status exchange to carry the genesis fingerprint and drop a mismatching peer
  (not built).
- 2026-10-01 ~14:10 IDT — **fresh chain running**: leader tama, term 1,
  checkpoint #0; tama, meow, kuro, yuki, shiro in (5 of 7, quorum 4). Planning
  prompt posted to tama and meow (blocks 30, 32). Both replied within ~90 s
  and neither built anything: tama's message (block 33) and meow's artifact 1
  ("Plan: console shell on the trashcan"). Awaiting human approval.
- 2026-10-01 — **correction (user):** the cats may push to the litter remote;
  it is their playground and their hosts hold the keys. The prompt's "do not
  push, touch credentials" line was carried over from the HDA brief and
  contradicted `overlays/deploy/context/projects.md` (push to
  `cats/<name>/<topic>` on `litter`; no main, no force, no deletes). A
  correction was sent to both cats; the handoff between them is a pushed
  branch (`cats/tama/console-shell` -> meow fetches). The "Rules" paragraph of
  the prompt above is superseded on that one point.
- 2026-10-01 — **plans approved** by the user (tama's block-33 message, meow's
  artifact 1, with the push correction). Go-ahead posted to both cats with
  guardrails: QEMU on ryzen under `MemoryMax=3G`/no swap, one VM at a time;
  tama clones upstream `akuma` into `/root/src/akuma` if it has no checkout;
  meow reboots the metal one vetted commit at a time and stops if the box does
  not return; both push to `cats/<name>/<topic>` on `litter`; milestone
  reports on chain. Status from here is in the cats' reports and on
  `litter`'s `cats/*` branches.
- 2026-10-01 ~14:35 IDT — **stopped: Kimi quota exhausted, cats were retrying.**
  After the approval neither cat did anything (meow turn 6, tama turn 14,
  unchanged). meow's `/var/log/herd/kot.log` shows every model call answered
  `403 Forbidden ... You've reached your 5-hour usage limit` and `kot`
  retrying it. Both cats share one token, so both were blocked; that is a
  retry loop on a dead quota, not a stall. Put both to sleep (nodes stay up,
  no model calls): meow `export MIOT_ASLEEP=true` in `/root/kot/start.sh`
  (original: `start.sh.pre-asleep`), tama `MIOT_ASLEEP=true` appended to
  `/root/kot/kot.env` (original: `kot.env.pre-asleep`). **No work was started;
  nothing was built or pushed.** The quota's reset time is unknown (the 5-hour
  window's start is not visible to us). To wake them: restore those two files
  and restart `kot` — or `deploy.py up` once `AGENTS` rows are as wanted.
  Noted while there: meow's local `transcript.jsonl` is 14.5 MB and its
  `tasks.json` is from 2026-09-29 — old local state outlived the fresh chain.
- 2026-10-01 ~18:05 IDT — **quota: `/usages` says 100 per 5 h; the UNIT is
  UNVERIFIED (see the correction below — first written here as "requests, not
  tokens", which the docs do not support).** Read from Kimi Code's own client source
  (`MoonshotAI/kimi-code`, `packages/oauth/src/managed-usage.ts`):
  `GET https://api.kimi.com/coding/v1/usages` with the bearer token returns
  `limits[0] = {window: 300 min, limit "100", used "100", resetTime
  15:19:54Z}` (= 18:19:54 IDT) plus `limit_month_total` at 6.2 %. The cats'
  logs fit: tama 39 model calls + meow ≥10 (its journal is only a tail) + the
  mac smoke test ≈ 100 between ~13:20 and 14:12 IDT. Every tool round trip is
  one request, so token size and reasoning effort do not matter; ~50 requests
  per cat per 5 h is the budget. The earlier "tokens/reasoning" hypotheses in
  this thread were wrong (`/usages` is the check: `used`/`limit`).
  - Other findings from `GET /models` and the source: `kimi-for-coding` is
    **K2.8 Preview, 1,048,576-token window** (our 262,144 was the
    k3-256k/highspeed figure); thinking is `only` (cannot be disabled) with
    efforts `low`/`high`/`max`, default `max`, sent as `thinking: {type,
    effort}` in the body — **not** `reasoning_effort`, which the endpoint may
    ignore. Kimi's client also sends `prompt_cache_key`.
  - `deploy.py`: `KIMI_CONTEXT_WINDOW` -> 1,048,576; new `Agent.no_local_nag`
    (default on -> `MIOT_NO_LOCAL_NAG=true`, `kot --no-local-nag`) so idle
    LocalTask nudges stop spending requests. Not yet redeployed; cats are
    still asleep. Chain-task nudges (`work_nag`) are consensus config and
    untouched. Check-ins before idle and held-result releases still cost a
    request each.
- 2026-10-01 ~18:20 IDT — **correction: Kimi's docs do not say "100 requests".**
  Read `kimi-code/{membership,error-reference,faq}.html` and the `docs/en/`
  tree of `MoonshotAI/kimi-code`. They say: a "rolling 5-hour rate window";
  all devices and API keys share one quota; CLI, editor and third-party tools
  all count; new members have no weekly cap, legacy plans refresh every 7 days;
  a monthly total freezes the quota when hit. They do **not** give the unit
  (requests/tokens/credits), per-tier numbers, or per-model weights beyond one
  line that `k3` uses about twice the quota of `k3-256k`. Extra-usage pricing
  examples (~¥0.03 simple request, ~¥1.6 complex task) hint at **cost-weighted**
  units, which would make context size and thinking effort matter after all.
  The only source for "100" is the `/usages` JSON (`limit "100"`, `used "100"`,
  no unit); the match with ~50 logged calls is circumstantial. **Test (cheap):**
  after the 18:19:54 IDT reset read `/usages`, make one tiny call, read again,
  then one large-context call, read again.
  Also from their docs: a quota `403` is a distinct error (their own client
  "fails fast ... instead of silently retrying for ~3 minutes"; kot retries 5x),
  429 is rate limiting (back off), and a *concurrent request limit* 403 is a
  risk-control policy.
- 2026-10-01 — (user) `limit "100"` / `used "100"` is probably a **percent**, not
  a count: no unit, and `used_ratio` is a fraction elsewhere in the payload. If
  so the budget is undisclosed and likely cost-weighted, which revives the
  token/thinking-effort hypotheses. The planned test then reads as a price per
  call in percent: tiny call, ~100k-token call, and the same with
  `thinking.effort: low`. Nothing decided until it runs.
