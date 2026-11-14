# Devbox overlays

Grade: B (verify behaviour — the overlay tree changes with the profile/feature
work it tracks)

`overlays/` holds the "sit down and develop **inside** Akuma" distro images —
reproducible SSH-in workstations, distinct from the plain `bootstrap/` tree
the default `cargo run` disk uses. Each overlay is a profile + feature
pairing (see [`../build-system.md`](../build-system.md) for that model in
general) plus its own `/etc` and run script.

| Overlay | Doc | Answers |
|---|---|---|
| `overlays/devbox/` | [`devbox.md`](devbox.md) | "give me a daily-driver dev VM" — `run-smoltcp.sh` (the default: native smoltcp, real SMP) and `run.sh` (the deferred rump path) both live here, on the same `devbox.img`, so they are also the rump-vs-smoltcp A/B pair |

A standalone `overlays/devbox-smoltcp/` (a rump-free A/B control) existed until 2026-11-11. It
built with the deleted `neko` feature and expected the deleted built-in SSH server, so it had not
worked since 2026-08-10; `overlays/devbox/run-smoltcp.sh` is the control.

## Background

- [`../../archive/SCRIPTS.md`](../../archive/SCRIPTS.md) §"Overlays" — how the
  two `devbox-smoltcp` things came to exist ~45 minutes apart on 2026-07-19.
- [`../subsystems/rump-stack.md`](../subsystems/rump-stack.md) — the rump-tax
  measurement the A/B pair produces.
