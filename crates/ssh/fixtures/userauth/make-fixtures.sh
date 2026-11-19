#!/bin/sh
# The user-key fixtures of slates-ssh's authentication tests (A-633), made once with stock OpenSSH 10.2p1 (macOS,
# LibreSSL 3.3.6) on the Mac Studio, 2026-11-19. A TEST KEY, generated for these tests alone and never a credential:
# its private half is kept so the test client can sign with the very key whose public blob OpenSSH encoded.
#   user_rsa.pub        ssh-keygen's public key line (RSA 3072, OpenSSH's default size)
#   user_rsa.pkcs8.b64  its private key as PKCS#8 (ssh-keygen -p -m PKCS8), the PEM body without its armor lines
#   fingerprint         SHA256:Wymbjagh5qjwNh2R9f44I8A1HSB7TrTf6LdcnH26LQ8 (ssh-keygen -l -E sha256)
# Usage: make-fixtures.sh OUTDIR (OUTDIR a new directory outside the tree; copy the files over these by hand).
set -eu
out="$1"
mkdir -p "$out"
ssh-keygen -q -t rsa -b 3072 -N '' -C 'slates-ssh test key (A-633)' -f "$out/user_rsa"
ssh-keygen -q -p -m PKCS8 -N '' -P '' -f "$out/user_rsa" > /dev/null
grep -v -- '-----' "$out/user_rsa" > "$out/user_rsa.pkcs8.b64"
rm "$out/user_rsa"
ssh-keygen -l -E sha256 -f "$out/user_rsa.pub"
