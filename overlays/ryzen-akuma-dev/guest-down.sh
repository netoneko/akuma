#!/bin/sh
# guest-down.sh - stop the dev guest and keep it from starting at the next boot.
. "$(dirname "$0")/env.sh"
herd stop "$VM"; herd disable "$VM"
rm -f /etc/herd/enabled/$VM.conf
sync
