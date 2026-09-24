# herd

The service supervisor: reads `.conf` files from `/etc/herd/enabled/`, spawns
and restarts services, owns box/OCI-bundle lifecycle. `herd start`/`herd stop`
reach the running daemon over loopback TCP `127.0.0.1:7117`; `stop` kills and
reaps before it answers.

The main doc is [`userspace/herd/README.md`](../../userspace/herd/README.md)
(config keys, lifecycle, CLI, control socket). More in
[`userspace/herd/docs/`](../../userspace/herd/docs/):
- `CORE_AWARE_SCHEDULING.md` — multikernel core pinning.
- `SIGNAL_EXIT_HANDLING.md` — signal deaths vs clean exits, and real `kill`.

See also: [`../reference/subsystems/containers.md`](../reference/subsystems/containers.md) "herd — the supervisor".
