# box

CLI for container/box management: `box run` (Docker images), `box use`,
`box open`, OCI image pull. Command reference:
[`userspace/box/README.md`](../../userspace/box/README.md).

Docs live at [`userspace/box/docs/`](../../userspace/box/docs/):
- `OCI_IMAGE_PULL.md` — pulling images and the content-addressed layer store.
- `BOX_RUN.md` — containers, the overlay root, what is and is not docker-compatible.
- `TESTING.md` — test procedures.

See also: [`../reference/subsystems/containers.md`](../reference/subsystems/containers.md)
-> "OCI images and the overlay root", the runbook
[`../runbooks/run-docker-image.md`](../runbooks/run-docker-image.md), and the
2026-08-11 write-up [`../archive/BOX_DOCKER_COMPAT.md`](../archive/BOX_DOCKER_COMPAT.md).
