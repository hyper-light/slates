#!/usr/bin/env bash
# Records what stock Mercurial 7.2 hands a server's `pretxnchangegroup` and `changegroup` hooks for a push over SSH
# (A-657 part 4; §4.17.3 "Hooks"); slates' builders (crates/server/src/vcs_hooks.rs, `hg_pretxnchangegroup`,
# `hg_changegroup`) are held to it. For each hook, POINT.MODE.env.txt lists every variable hg set or changed beyond the
# environment the server started with, as `env NAME=VALUE` in byte order; the values that name this machine are
# placeholders: `$HG` hg's executable, `$ROOT` the server repository's root, `$TXNID` the transaction's id (checked here
# to be `TXN:` and forty lower-case hexadecimal digits, and the same in both hooks of a push). MODE is `draft` (a
# non-publishing server) or `public` (a publishing one, 7.2's default). The server starts with neither `HG` nor
# `HGPLAIN`, as sshd starts it, so what the hooks see of them is hg's own.
# Deterministic: no configuration but this script's, user `test`, date `0 0`, the client at 192.0.2.7 (TEST-NET-1);
# two changesets pushed, so the first and the last differ.
# Usage: make-hook-fixtures.sh OUTDIR [HG]    (HG: stock 7.2's `hg`; default the one on the search path)
# Run 2026-11-19 on the MacBook (macOS 26.4, arm64) with Homebrew's Mercurial 7.2:
#   crates/hg/fixtures/hooks/make-hook-fixtures.sh crates/hg/fixtures/hooks
set -euo pipefail
OUT=$(cd "$1" && pwd)
HGBIN=${2:-hg}
"$HGBIN" --version | head -n 1 | grep -q '(version 7\.2)$' || { echo "make-hook-fixtures: not Mercurial 7.2" >&2; exit 1; }
WORK=$(mktemp -d "$HOME/.cache/hg-hookfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HGRCPATH= HGENCODING=utf-8 TZ=UTC LC_ALL=C
unset HG HGPLAIN HGPLAINEXCEPT HGUSER
# A hook: the whole environment it was handed, in byte order, named by its type.
cat > "$WORK/record.sh" <<'HOOK'
#!/bin/sh
env | LC_ALL=C sort > "$RECORD_DIR/$HG_HOOKTYPE.env"
HOOK
# An ssh stand-in: runs its last argument, the server's command, as sshd would for a client at 192.0.2.7. It drops
# what the client's hg put in its environment (`HG`, `shellenviron`'s; `HGPLAIN`, this script's) and records the
# environment the server starts with.
cat > "$WORK/ssh.sh" <<'SSH'
#!/bin/sh
for command; do :; done
unset HG HGPLAIN
SSH_CLIENT='192.0.2.7 50000 22'
export SSH_CLIENT
env | LC_ALL=C sort > "$RECORD_DIR/server.env"
exec sh -c "$command"
SSH
client() { HGPLAIN=1 "$HGBIN" --config ui.username=test --config devel.default-date='0 0' "$@"; }
for mode in draft public; do
  export RECORD_DIR="$WORK/$mode-record"
  mkdir "$RECORD_DIR"
  "$HGBIN" init "$WORK/$mode-server"
  publish=False
  [ "$mode" = public ] && publish=True
  printf '[phases]\npublish = %s\n[hooks]\npretxnchangegroup.record = sh %s\nchangegroup.record = sh %s\n' \
    "$publish" "$WORK/record.sh" "$WORK/record.sh" > "$WORK/$mode-server/.hg/hgrc"
  client init "$WORK/$mode-client"
  printf 'base\n' > "$WORK/$mode-client/a"
  client -R "$WORK/$mode-client" commit -A -q -m base
  printf 'second\n' >> "$WORK/$mode-client/a"
  client -R "$WORK/$mode-client" commit -q -m second
  client -R "$WORK/$mode-client" push -q --ssh "sh $WORK/ssh.sh" --remotecmd "$HGBIN" "ssh://fixture/$WORK/$mode-server"
  root=$(cd "$WORK/$mode-server" && pwd -P)
  txnids=$(cat "$RECORD_DIR/pretxnchangegroup.env" "$RECORD_DIR/changegroup.env" | sed -n 's/^HG_TXNID=//p' | sort -u)
  printf '%s\n' "$txnids" | grep -qxE 'TXN:[0-9a-f]{40}' \
    || { echo "make-hook-fixtures: $mode: not one transaction id of TXN: and forty digits: $txnids" >&2; exit 1; }
  for point in pretxnchangegroup changegroup; do
    comm -13 "$RECORD_DIR/server.env" "$RECORD_DIR/$point.env" | grep '^HG' \
      | sed -e 's|^HG=.*|HG=$HG|' -e "s|^HG_PENDING=$root\$|HG_PENDING=\$ROOT|" -e 's|^HG_TXNID=.*|HG_TXNID=$TXNID|' \
      | sed 's/^/env /' > "$OUT/$point.$mode.env.txt"
  done
done
