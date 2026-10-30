#!/bin/sh
# premises-08q.sh - the premise table of BRIEF-Q (curl over HTTPS), as one read-only script.
# Prints "HOLDS n ..." or "WRONG n ... (what it saw)" per row; exits 1 if any row is WRONG.
# Run on the box, any cwd:  sh /root/dev-cycle/taoxichuan-s7/premises-Q.sh < /dev/null
export TERM=dumb HOME=/root GIT_PAGER=cat
. /etc/akuma-dev.env
REPO=/src/github.com/netoneko/akuma
G=$REPO/scripts/kvm/goose
R=/tmp/refroot
URL=https://raw.githubusercontent.com/netoneko/akuma-playground/f70d39f17a7edb1c7ef7245c93cbc8fa59ec12ab/hello.c
WANT=c563b63a9149349aee48b15b5eee4966a8a46a65888967b02aa0952f6c4b916a
bad=0
row() { # n description rc [saw]
  if [ "$3" = 0 ]; then echo "HOLDS $1 $2"; else echo "WRONG $1 $2 (saw: ${4:-rc=$3})"; bad=1; fi
}

rm -f /tmp/q-ref.c
$R/lib/ld-musl-x86_64.so.1 --library-path $R/usr/lib:$R/lib $R/usr/bin/curl -sS \
  --cacert $R/etc/ssl/certs/ca-certificates.crt -o /tmp/q-ref.c "$URL" < /dev/null 2>/tmp/q-ref.err
got=$(sha256sum /tmp/q-ref.c 2>/dev/null | cut -c1-64)
[ "$got" = "$WANT" ]; row 1 "the reference fetch (refroot curl over HTTPS, the same binary the guest installs) of hello.c at the pinned commit has sha256 ${WANT%${WANT#????????}}..." $? "$got $(head -c 120 /tmp/q-ref.err)"
[ "$(wc -c < /tmp/q-ref.c 2>/dev/null)" = 77 ]; row 2 "the reference body is 77 bytes" $? "$(wc -c < /tmp/q-ref.c 2>/dev/null) bytes"
ls /srv/alpine-mirror/main/x86_64/ca-certificates-bundle-*.apk >/dev/null 2>&1; row 3 "the frozen mirror carries ca-certificates-bundle (the CA file curl reads by default)" $?
grep -q '^vm_outbound = true' $G/vm-dev.stage7.conf; row 4 "the stage-7 vm conf routes guest TCP and DNS through the host (vm_outbound = true)" $?
grep -q "rgroup D2 'd2=nslookup alpinelinux.org'" $G/run_gates.sh; row 5 "run_gates.sh already proves guest DNS for a real name (group D2: nslookup alpinelinux.org)" $?
grep -q 'Exit status: 0' $G/e_group.sh; row 6 "e_group.sh is the model for a one-exec-per-connection group with console-verdicts (usage: e_group.sh name=cmd ...)" $?
grep -q 'sha256sum /etc/ssl/certs/ca-certificates.crt' $G/run_gates.sh; row 7 "the guest has a sha256sum applet and the CA file at /etc/ssl/certs/ca-certificates.crt (gate G4 already hashes it)" $?
[ -f /srv/alpine-mirror/MANIFEST.sha256 ]; row 8 "the apk mirror (the B group's package source) exists on the box" $?
exit $bad
