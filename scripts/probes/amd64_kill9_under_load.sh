#!/bin/sh
# `kill -9` processes blocked inside a syscall, repeatedly, under real SMP load.
#
# The repro `proposals/AMD64_SWITCH_FREED_CR3_UAF.md` asks for: killing a
# process that is parked in the kernel is the path most likely to run teardown
# against a thread that is not at a clean point, which is how a task slot ends
# up naming a page-table root the free path has already returned. Run it on the
# bare-metal box (`/root/killprobe.sh`) with a build going as the load:
#
#     kbuild -c -j 1                                  # ~23 min of SMP=4 load
#     busybox setsid sh scripts/probes/amd64_kill9_under_load.sh 24 &
#
# Two victim shapes, because the original crash correlated with the second:
#   - `sleep`, parked in nanosleep with a trivial address space
#   - `find`, blocked in read(2) on the USB root, so there is a real address
#     space with real file mappings to tear down
#
# No `pgrep` and no `timeout(1)` on this box, so victims are matched with
# `ps | awk` and the bracket trick — an unbracketed pattern matches the awk
# command's own argv.
#
# Reading the result (2026-09-18, on the fixed kernel: all four were clean):
#
#   ssh akuma "dmesg" | grep -a 'FREE[D]-CR3'     # the tripwire
#   ssh akuma "dmesg" | grep -a '\[Faul]t'        # any kernel fault
#
# **Bracket those patterns too.** `sshd` logs the whole command line it runs
# (`[SSH] Exec: /bin/sh ["-c", …]`) into the same ring `dmesg` serves, so a
# literal pattern matches its own argv and reports a hit that is your grep.
# That is the `pkill -f` self-match trap in a new costume, and it cost one
# false positive the day this was written.
#
# A box that stops answering ssh here is usually NOT dead — four cores building
# plus this probe can starve sshd of a completed handshake for minutes. Check
# with a TCP connect to 2222 (the banner proves the kernel is alive) and a ping
# before concluding anything; the failure this hunts looks different, and takes
# both.
ROUNDS=${1:-24}
i=0
while [ $i -lt $ROUNDS ]; do
    sleep 600 &
    sleep 600 &
    find /root/akuma -type f > /dev/null 2>&1 &
    find /usr -type f > /dev/null 2>&1 &
    sleep 3
    for p in $(ps | awk '/[s]leep 600|[f]ind \/root|[f]ind \/usr/ {print $1}'); do
        kill -9 "$p" 2>/dev/null
    done
    echo "round $i done $(date +%s)" >> /root/.killprobe.log
    i=$((i + 1))
    sleep 5
done
echo DONE >> /root/.killprobe.log
