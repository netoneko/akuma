#!/bin/sh
set -e
apk add --no-cache build-base mbedtls-dev mbedtls-static zlib-dev zlib-static pkgconf >/dev/null
cd /tmp
wget -q https://curl.se/download/curl-8.11.1.tar.gz
tar xf curl-8.11.1.tar.gz
cd curl-8.11.1
# NOTE: no -static in CFLAGS (breaks configure's link probe). Static link via make LDFLAGS=-all-static.
./configure --disable-shared --enable-static \
  --with-mbedtls \
  --without-libpsl --disable-ldap --disable-docs --disable-manual --disable-alt-svc \
  CFLAGS="-Os" >/tmp/cfg.log 2>&1 || { echo CFGFAIL; tail -25 /tmp/cfg.log; exit 1; }
make -j4 LDFLAGS="-all-static" >/tmp/make.log 2>&1 || { echo MAKEFAIL; tail -40 /tmp/make.log; exit 1; }
cp src/curl /out/curl-static
strip /out/curl-static || true
file /out/curl-static; ls -la /out/curl-static
echo "BUILD_OK"
