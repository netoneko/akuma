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
