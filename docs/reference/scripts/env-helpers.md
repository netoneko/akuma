# Container / environment helpers

Grade: — (index)

| Script | What it does |
|---|---|
| [`alpine.sh`](../../../scripts/alpine.sh) | Drops you into an Alpine `linux/arm64` Docker shell with the repo mounted at `/akuma` — a quick cross-arch sandbox for testing a command before scripting it into a build step. |
| [`build_static_curl.sh`](../../../scripts/build_static_curl.sh) | Builds a fully static `curl` (mbedTLS backend) inside an Alpine builder — used when a devbox/bootstrap image needs a static `curl` binary rather than the busybox/wget one. |

Back to [`README.md`](README.md).
