#!/bin/sh
# run a command in the selfhost guest: gssh.sh '<cmd>'   (guest shell has NO PATH — preamble supplied)
W=/home/netoneko/akuma-selfhost
K=$W/akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key
ENVP='export HOME=/root CARGO_HOME=/root/.cargo PATH=/usr/local/bin:/usr/local/rust/bin:/usr/bin:/bin LD_LIBRARY_PATH=/usr/local/rust/lib:/usr/lib:/lib AKUMA_SRC=/src/github.com/netoneko/akuma;'
exec ssh -i $K -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o BatchMode=yes -p 2222 root@10.0.2.15 "$ENVP $1"
