# RUST.org.il talk — planning notes

Working notes for a talk at rust.org.il about self-hosting the Akuma kernel in Rust.
Audience: chill local meetup, not a conference. Focus is on consistently applying
what Rust was sold as, not on specific functions or technical problems.

## Speaker's own conclusions (verbatim, in the order stated)

- "the memes were right"
- "self hosting on amd64 story that requires way more than running on qemu"
- "rust shines in drivers, host level testing, the forced invention of inversion of control because of the big pre-porting refactor"
- "all of this would be a complete nightmare in c"
- "it's more about consistently apply what rust was sold as"
- "maybe i'll start with how it's exciting to be able to run things but running useful things forces you into a discipline because if you're doing things you should do them well"
- "now your own software stack can be built by open models in relatively short time with minimal investment of money"
- "the question is can reliable software be built with new tech, if yes, what enables it"
- "and rust definitely enables it in a variety of ways"
- "rust enables better discripline through the build system and tools"
- "the more reliable we make the process the faster we can move"
- "ease of debugging, strict compiler, easy host testing, easy extraction"
- "if i knew this sooner i would've moved faster"
- "rust compiler and cargo are great exercises for the whole system"
- "people in the audience care about their tools, so now that building your own tooling or even your own stack is possible with open source, open weights and open hardware, rust is a great tool to rely on"
- "and then kvm closes the self hosting loop one more time"
- "and with rust built drivers derived from open spec that allow networking that is essential to the whole thing"
- "many projects/companies are doing rust rewrite like google with argon and with google's tech track record i can trust they know what they are doing"

## Context

- Origin anecdote: first winter of userspace bring-up happened in Taoxichuan,
  Jingdezhen, during a ceramics residency. Not in the talk, but the kernel's
  origin story.
- Google Argon reference checked: Gemini agent migrating C/C++ to Rust, Zircon
  kernel among the targets. Precise claim: "Google pointed an AI at migrating
  kernel code to Rust and it cleared their audit bar" — not "Google rewrote
  their kernel in Rust."
- The Ethernet driver (rtl8169) is the datasheet-clean story; rtw89 WiFi is not.
- KVM in 2 days because Rust pays off.
- Composability argument and the minimal-kernel experiment brief:
  [`MINIMAL_SELF_HOSTING_EXPERIMENT.md`](MINIMAL_SELF_HOSTING_EXPERIMENT.md) —
  has a "Measurements to take before the talk" checklist; its numbers are
  derived until re-measured on a clean tag.

## Facts to consider (measured 2026-10-30)

Information, not a storyline. Measured on `rust-org-il-experiment` @ `9f1e333f` plus the
working tree (which held uncommitted taoxichuan work by another agent), with
`scripts/cloc_akuma.py --audit` — method in
[`AUDIT_SURFACE_METHODOLOGY.md`](AUDIT_SURFACE_METHODOLOGY.md). "Audit lines" = code lines
rustc read for the exact build, minus `#[cfg]`-false and test-only items; comments and blanks
never count. Re-measure on a clean tag before quoting: the taoxichuan numbers move daily.

**Composition**

- taoxichuan adds **7,674 lines** on top of an Akuma host: akuma-amd64 alone is 151,235 audit
  lines, the two together 158,909. **91%** of taoxichuan's live lines (77,090 of 84,761) are
  already live in the host, including all 41,748 of its third-party lines.
- taoxichuan's own crate is **7.8%** of its image (6,652 of 84,761); the rest is 54 workspace
  crates and 22 crates.io crates.
- The aarch64 kernel's own crate is **80** live lines; everything else is composed.
- A third of what the compiler reads is compiled out: 127,706 lines in, 84,761 live. smoltcp
  alone loses 47% (IPv6, 6LoWPAN, 802.15.4 sit in the same files).

**`unsafe`**

- **zerocopy has more `unsafe` lines (743) than all 54 workspace crates in the guest together
  (738).** Akuma names zerocopy in exactly two function signatures
  (`crates/akuma-virtio/src/transport.rs`, `read_config_space`/`write_config_space`), whose
  four trait bounds virtio-drivers' `Transport` trait requires. The linked ELF has **0**
  zerocopy symbols: it is all generic code, inlined. (Fair note: that `unsafe` is the crate's
  purpose, transmute proofs in one place, checked with Kani and Miri.)
- Third-party code is **49%** of the guest's lines and **67%** of its `unsafe` lines (1,872 of
  2,812).
- The allocator (`akuma-alloc`, 267 `unsafe` lines) has more `unsafe` than the kernel crate
  (taoxichuan, 202).
- A crate named `safe-mmio` contributes **168** `unsafe` lines (via virtio-drivers).
- 41 of the guest's 77 crates have zero `unsafe` lines; together they hold 41,476 of the
  84,761 lines.

**Dead weight and oddities**

- A **device-tree parser** is in the x86 PVH guest, which has no device tree: `fdt` (914 lines)
  arrives through `akuma-mmu -> akuma-fdt`. 0 symbols in the ELF (the linker drops it), but it
  is still source in the image's dependency set.
- **Two `bitflags` major versions** ship in one image: 1.3.2 via smoltcp, 2.13.1 via
  virtio-drivers. virtio-drivers also brings `thiserror` into a `no_std` kernel.
- **`syn` alone (20,716 lines) is 3x taoxichuan's own code**, and never runs in the guest. All
  build-time host code (proc-macros, build scripts, their dependencies) is 31,743 lines, 37% the
  size of the image.

**The experiment** (git and
[`MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md))

- 200 commits to `taoxichuan/` in 6.3 days (2026-10-24 to 10-30). Own code grew 2,246 -> 6,964
  production lines while direct `akuma-*` dependencies went 16 -> 34.
- `git blame` of `taoxichuan/src` at HEAD (non-blank, non-comment lines): 68.5% from commits
  named after goose sessions (s1...s8q, slices 1-3) that carry the user's identity and no AI
  trailer (git alone cannot prove they are goose-glm's); 31.4% from Claude Sonnet 5.5 commits,
  2,057 of them written on 10-30 alone; 32% of today's lines date from day one (10-24).
- Cost per tool call grows with session length: about 19k tokens under 40 calls, about 105k at
  130+. Six long sessions burned 71% of all tokens.
- Cost per milestone fell from 20-28M tokens to 5-11M after the methodology change; the
  cheapest was the process-model plan at 1.5M.
- About one brief in two carried a wrong premise, and goose's survey caught every one.
- The KVM host carried ~2,700 tool calls' worth of guest boots, faults and kills with no
  guest-caused host crash (to 2026-10-26).

**Tokenomics** (goose's `usage_ledger` from the Akuma partition, cross-checked against the z.ai
usage API; full method and tables in the agent log, "GLM tokenomics from the goose ledger")

- The whole experiment's GLM work: **6,989 calls, 569.4M tokens** (16.1M GLM-5.3 on day one, the
  rest GLM-5.3-Flash). The ledger matches z.ai's own daily totals to within 0.1M.
- **98.3%** of input tokens were cache hits; output was 0.4-1.8% of tokens. An agent mostly
  re-reads its own context.
- At z.ai's pay-as-you-go API prices the whole experiment is **$24.91**. Two hours of GLM-5.3 on
  day one are $4.86 of that: 20% of the cost for 3% of the tokens.
- On the $18/month Lite plan it took **3 weekly quota resets**. The quota readings imply ~50
  credits per million tokens (about 200M Flash tokens per Lite week); 569M x 50 = 2.85 weeks.
- z.ai's published credit formula, with this cache rate and the calls' timing (only 222 of 6,989
  were in peak hours, the rest at the documented 50% off-peak rate), predicts **~36** credits per
  million. Part of the gap is the model split and part is outside the ledger: the GLM-5.3 day
  costs ~1,000 credits more than Flash would (~0.1 Lite week), and 10-20/21's 115M Flash tokens
  (another machine) add ~0.4 week if they fell inside the reset weeks, bringing the formula to
  ~2.4 of the user's 3 weeks. **Not all of it:** the three quota readings were taken in
  Flash-only windows and still show ~50 credits per million against the formula's ~36.
- A refined-process milestone (after the 2026-10-26 methodology change) costs **1.6-46M tokens,
  $0.07-1.84** at API prices: `apk update` 6.3M ($0.26), `apk add curl` 7.0M, dynamic PIE 28.3M
  ($0.97), curl over HTTPS 46.4M ($1.84). The median execution milestone is about 16M, $0.60.
- One problem cost more than ten milestones: preemption on a single core (M4) took **159M tokens**,
  80% of a Lite week, against 186M for the ten refined milestones around it. The user stopped it.
- Cost per call grows with context: a 130+-call session averages ~105k tokens per call, a sub-40
  one ~19k.
- Pacing beats bursting: the refined days ran 57-125M tokens a day, so a Lite week lasted two or
  three days. One Lite month holds ~866M tokens, enough for a median-cost replay of the whole
  ladder (25-30 milestones, ~400-480M) at about 28M a day, one 5-hour window daily.
- The same median replay on the pay-as-you-go API is about **$15-20**, less than one Lite month,
  with no caps.
- **The supervisor out-consumed the worker.** Claude Code transcripts (`~/.claude-personal`, usage
  fields only): 1,221M Claude Sonnet 5.5 tokens on 10-24 -> 10-30 against GLM's 569M, 2.1x, and
  more than GLM on every single day; 10-30 alone was 340M (GLM's busiest day: 125M).
- **Claude executing** (10-29 18:00Z -> 10-30, 20 active hours, ~12 milestones from pn S1 to the
  CoW fork and the unsafe cut, 32 commits): 1,380 model calls, 1,571 tool calls, **406M tokens**.
  Its average call carried **294k** tokens of context; GLM's carries 56-77k.
- **The same work by GLM at 2-4x Claude's tool calls** (56-77k tokens per call): 2x 176-242M
  tokens, 3x 264-363M, 4x 352-484M (about 630M if the slowdown lands in long sessions, the M4
  pattern). That is 0.9-2.4 Lite weeks, $6-17 on the API. **At 2x slower GLM would spend fewer
  tokens than Claude did**, because each of its calls carries a quarter of the context.
- **What the supervisor's tokens bought** (each Claude call classified by its tools; keyword
  rules, good to a few points). Supervising goose (10-24 -> 10-29 18:00Z, 816M): **38% polling**
  (1,083 "nothing changed" reports at a 5-minute interval plus 230 sleeps/Monitors, 312M),
  ~30% real supervision (goose logs, steering, briefs, merges, verification), **16% Claude's own
  coding**. Executing (after the switch, 406M): **~76% coding** (reading source 40%, editing 20.5%,
  build/test and boot waits 16%), supervision 2.4%.
- **The "nothing changed" polls alone (257M tokens) are 45% of everything GLM spent on the
  experiment (569M).** Each re-sent ~237k tokens of context: about 2.8M tokens an hour to report
  that nothing happened, for ~90 hours.
- **Per milestone, supervising GLM cost Claude more than doing the work itself:** ~50M Claude
  tokens per supervised GLM milestone (10-26 -> 10-29, on top of GLM's own ~32M) against ~34M per
  milestone when Claude executed (and those were the harder ones). Token counts, not money: a
  Claude token and a GLM-Flash token are priced differently.
- **EXTRAPOLATION, not a measurement - the whole project on GLM alone, supervision not counted**
  (the user's position: with a proper process supervision is unnecessary or much cheaper). Claude's
  coding calls only (read source, edit, build/test, boot waits; plus the commits and docs the
  process requires): 1,645-1,796 model calls. Redone by GLM at 2-4x as many calls and 56-77k tokens
  per call: **184-553M GLM tokens**. Two independent methods agree at the low end: 2x gives
  184-277M, and Claude's ~12 milestones at GLM's refined median (16M each) give ~192M.
  Added to GLM's actual 569M (learning phase included): **753M-1.12B tokens, 3.8-5.6 Lite weeks,
  $32-45 on the pay-as-you-go API**; at 2x that is **about one $18 Lite month (~866M)**.
- **What keeps it an extrapolation:** no GLM run exists for the hardest class of milestone (rustc,
  cargo, threads, CoW fork - Claude executed those); the 2-4x factor is the user's assumption, not
  a measurement; the refined process was itself Claude-supervised; and the Lite quota charged ~40%
  more per token than z.ai's published formula, unexplained.
- **The user's assessment (2026-10-30):** the experiment was flawed and ultimately inconclusive, but
  the initial claim - that your own stack can be built by open models in a short time for little
  money - still looks plausible on these numbers.

**Linux comparison:** pending — `scripts/linux_audit_surface.sh` (firecracker / minimal /
host configs of 6.18) is building; numbers go in `AUDIT_SURFACE_METHODOLOGY.md` and here.
