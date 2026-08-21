# The Akuma root filesystem as an OCI image.
#
# There is no base image and nothing is executed: the image IS the ext2 contents,
# in a form a registry can store, dedup and version. The consumer does not run it
# as a container -- it pulls it, `docker save`s it, and extracts the layers into a
# freshly mkfs'd ext2 image (see ../README.md and
# ../../docs/runbooks/dump-firecracker-fdt.md for the host it runs on).
#
# The build context is staged by build-rootfs-image.sh, not the repo root: the
# merge (which /etc wins, what the profile includes, where the ssh key comes from)
# is that script's job, and a 1.2 GB repo-root context would be sent to the daemon
# for nothing.
#
# One COPY per top-level tree, deliberately: each becomes its own layer, so
# bin/ -- 163 MB and near-static -- stops being re-uploaded every time /etc moves.
# Ordered least- to most-volatile so the stable layers stay cached.
FROM scratch

COPY root/usr/ /usr/
COPY root/bin/ /bin/
COPY root/etc/ /etc/

# Whatever the profile added beyond those three: root/, public/, srv/, tmp/, var/.
COPY root/ /
