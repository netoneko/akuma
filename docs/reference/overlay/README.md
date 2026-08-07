# Devbox overlays

Grade: B (verify behaviour — the overlay tree changes with the profile/feature
work it tracks)

`overlays/` holds the "sit down and develop **inside** Akuma" distro images —
reproducible SSH-in workstations, distinct from the plain `bootstrap/` tree
the default `cargo run` disk uses. Each overlay is a profile + feature
pairing (see [`../build-system.md`](../build-system.md) for that model in
general) plus its own `/etc` and run script.

There are two overlay directories, and they answer different questions:

| Overlay | Doc | Answers |
|---|---|---|
| `overlays/devbox/` | [`devbox.md`](devbox.md) | "give me a daily-driver dev VM" — this is where `run-smoltcp.sh` (the current default) and `run.sh` (the deferred rump path) both live |
| `overlays/devbox-smoltcp/` | [`devbox-smoltcp.md`](devbox-smoltcp.md) | "hold everything constant except the network stack" — a rump-free A/B control build, a *separate* overlay directory from `overlays/devbox/run-smoltcp.sh` above despite the near-identical name |

The name collision between `overlays/devbox/run-smoltcp.sh` and the
standalone `overlays/devbox-smoltcp/` directory is real and not a typo in
this doc — see [`devbox-smoltcp.md`](devbox-smoltcp.md) for how the two
differ and why both exist.

## Background

- [`../../archive/SCRIPTS.md`](../../archive/SCRIPTS.md) §"Overlays" — how the
  two `devbox-smoltcp` things came to exist ~45 minutes apart on 2026-07-19.
- [`../subsystems/rump-stack.md`](../subsystems/rump-stack.md) — the rump-tax
  measurement `overlays/devbox-smoltcp/` exists to produce a clean control for.
