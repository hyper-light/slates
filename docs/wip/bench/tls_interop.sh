#!/bin/bash
# Interoperability of slates' SecP384r1MLKEM1024 with OpenSSL 3.5 (A-93), both directions plus a negative control. Run
# in a throwaway container with the Linux release build of `slates-transport`'s `tls_interop` example at /target:
#   docker run --rm -v <linux-target>:/target:ro -v "$PWD/docs/wip/bench":/i:ro debian:trixie bash /i/tls_interop.sh
# The only files written are OpenSSL's throwaway certificate and logs, inside the container's own filesystem, which is
# discarded with it; slates writes nothing.
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq openssl >/dev/null 2>&1
openssl version
B=/target/release/examples/tls_interop
cd /tmp
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout k.pem -out c.pem -days 1 -subj /CN=interop >/dev/null 2>&1
echo "== slates client -> OpenSSL server"
openssl s_server -accept 4433 -tls1_3 -groups SecP384r1MLKEM1024 -cert c.pem -key k.pem -naccept 1 -quiet > srv.log 2>&1 &
sleep 1
timeout 20 $B client 127.0.0.1:4433; echo "client exit $?"
sleep 0.5; grep -i "group\|error" srv.log | head -3
echo "== OpenSSL client -> slates server"
timeout 20 $B server 4434 &
sleep 1
echo hi | timeout 10 openssl s_client -connect 127.0.0.1:4434 -tls1_3 -groups SecP384r1MLKEM1024 2>&1 | grep -iE "Negotiated TLS1.3 group|Server Temp Key|error|Protocol" | head -4
wait
echo "== negative: OpenSSL offering only X25519MLKEM768 against the slates server"
timeout 20 $B server 4435 > neg.log 2>&1 &
sleep 1
echo hi | timeout 10 openssl s_client -connect 127.0.0.1:4435 -tls1_3 -groups X25519MLKEM768 2>&1 | grep -iE "alert|error|Negotiated" | head -2
wait; cat neg.log | tail -2
