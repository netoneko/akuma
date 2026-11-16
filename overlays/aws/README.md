# Direct EC2 bootstrap

This overlay targets a future direct-boot EC2 image, separate from the
Firecracker-on-AWS host overlay. The kernel must start `/bin/herd` as init;
herd runs `sshd` and the `instance-key` service. The service fetches the
Ed25519 launch key from instance metadata and installs it for sshd. sshd reads
the key file on login, so the services need no ordering. This is a metadata
bootstrap, not an implementation of cloud-init user-data.

Run `python3 -m unittest discover -s overlays/aws/tests -v` for local IMDSv2
tests. For a different test server, set `METADATA_URL` to `http://HOST:PORT`,
`AUTHORIZED_KEYS` to a disposable path, and optionally `INSTANCE_KEY_TRIES`.

An AMI builder must install the overlay and curl, provide herd and sshd, select
herd as init, and omit baked host and authorized keys. The overlay is not yet
wired into an AMI builder; ENA and direct EC2 boot remain under development.
