#!/bin/sh
# Install the env files and wrappers into a running denver guest (run ON the guest, sources in $1, default /work/env).
# Jars must already be in /opt/clojure, /opt/scala, /opt/groovy (see ../README.md).
S=${1:-/work/env}
for f in java clojure scala groovy; do cp "$S/$f.env" /etc/$f.env; done
cp "$S/profile" /etc/profile
mkdir -p /usr/local/bin
for b in javac clojure scalac scala groovy; do cp "$S/bin/$b" /usr/local/bin/$b; chmod +x /usr/local/bin/$b; done
# the JDK launchers on the default PATH (sshd sets none, and /usr/local/bin is on it): java, keytool, jfr, ...
for b in /opt/jdk/bin/*; do ln -sf "$b" /usr/local/bin/$(basename "$b"); done
echo installed
