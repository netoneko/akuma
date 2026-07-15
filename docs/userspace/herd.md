# herd

The service supervisor: reads `.conf` files from `/etc/herd/enabled/`, spawns
and restarts services, owns box/OCI-bundle lifecycle.

Docs live at [`userspace/herd/docs/`](../../userspace/herd/docs/):
- `CORE_AWARE_SCHEDULING.md` — multikernel core pinning.

See also: [`../reference/subsystems/containers.md`](../reference/subsystems/containers.md) "herd — the supervisor".
