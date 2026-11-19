#!/usr/bin/env bash
# Makes slates-git's golden vectors for protocol v2's `object-info` and `bundle-uri` and for `packfile-uris` (A-624),
# with stock git's server answering on its own wire:
# - the capability advertisement with `object-info` and `bundle-uri` advertised (transfer.advertiseObjectInfo,
#   uploadpack.advertiseBundleURIs) and `fetch` offering sideband-all and packfile-uris;
# - `object-info` requests written here byte for byte, stateless as an HTTP server answers them, with upload-pack's
#   answers: the sizes of a blob, a tree, a commit and an object the server lacks; ids without `size`; `size` with no
#   id; an id in upper case; and the sizes in a SHA-256 repository. git 2.50.1 ships no `object-info` client
#   (protocol-caps.c serves it, nothing in fetch-pack.c, transport.c or cat-file asks for it), so the requests follow
#   the document's grammar as serve.c and protocol-caps.c read it;
# - `bundle-uri`: a clone by stock git with transfer.bundleURI on, its whole session copied by a tee (the client asks
#   for the server's bundle list, downloads the bundle from a URI relative to the server's, and fetches the rest),
#   and the same request written here, stateless;
# - `packfile-uris`: fetches from a server that names a blob's pack by URI (uploadpack.blobPackfileUri), the client
#   taking https with sideband-all, taking http only, taking https without sideband-all, and taking ftp and https;
# - every object of the SHA-1 repository as a loose object's bytes, header and content, uncompressed, for the test that
#   answers `object-info` from the store.
# Then SHA256SUMS: every file's SHA-256, which the tests check before they read a byte.
# Deterministic: no configuration but this script's, a fixed identity, every date 2005-04-07T22:13:13Z, packs made on
# one thread, no progress, and no path of this machine in any byte (the bundle's URI is relative).
# Usage: make-v2-rest-fixtures.sh OUTDIR
#
# Sources: git's Documentation/gitprotocol-v2.adoc ("object-info", "bundle-uri", "fetch"'s packfile-uris),
# technical/bundle-uri.adoc, config/bundle.adoc, config/uploadpack.adoc, config/transfer.adoc; protocol-caps.c,
# bundle-uri.c, serve.c, upload-pack.c, builtin/pack-objects.c (want_object_in_pack_mtime), fetch-pack.c.
set -euo pipefail
OUT=$(cd "$1" && pwd)
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE GIT_PROTOCOL
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_AUTHOR_DATE='1112911993 +0000' GIT_COMMITTER_DATE='1112911993 +0000'
WORK=$(mktemp -d "$HOME/.cache/slates-git-v2rest.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"; mkdir -p "$HOME"
git --version > "$OUT/git-version.txt"
pkt() { printf '%04x%s' $(( ${#1} + 4 )) "$1"; }
v2() { GIT_PROTOCOL=version=2 git upload-pack --stateless-rpc .; }
adv() { GIT_PROTOCOL=version=2 git upload-pack --http-backend-info-refs --stateless-rpc .; }
setup() { git config commit.gpgsign false; git config tag.gpgsign false; git config pack.threads 1; }
# The server: two commits, a small file and a larger one.
git init -q -b main "$WORK/server"; cd "$WORK/server"; setup
echo one > a; for i in $(seq 1 40); do echo "line $i of the larger file"; done > big
git add a big; git commit -q -m one
echo two >> a; git add a; git commit -q -m two
git log --format='%H %s' --all > "$OUT/log.txt"
TIP=$(git rev-parse main); ONE=$(git rev-parse main~1); TREE=$(git rev-parse 'main^{tree}')
A=$(git rev-parse main:a); BIG=$(git rev-parse main:big)
UNKNOWN=$(printf 'not in the server\n' | git hash-object --stdin)
for object in $(git rev-list --objects --all | cut -d' ' -f1); do
  printf '%s %s %s\n' "$object" "$(git cat-file -t "$object")" "$(git cat-file -s "$object")"
  printf '%s %s\0' "$(git cat-file -t "$object")" "$(git cat-file -s "$object")" >> "$OUT/objects.bin"
  git cat-file "$(git cat-file -t "$object")" "$object" >> "$OUT/objects.bin"
done > "$OUT/objects.txt"
git config transfer.advertiseObjectInfo true
git config uploadpack.advertiseBundleURIs true
git config uploadpack.allowSidebandAll true
# object-info, stateless.
info() { { pkt $'command=object-info\n'; pkt $'object-format=sha1\n'; printf 0001
  for line in "$@"; do pkt "$line"$'\n'; done; printf 0000; } > "$OUT/$NAME.request.bin"
  v2 < "$OUT/$NAME.request.bin" > "$OUT/$NAME.response.bin"; }
NAME=object-info-sizes info size "oid $A" "oid $TREE" "oid $TIP" "oid $UNKNOWN" "oid $BIG"
NAME=object-info-no-size info "oid $A" "oid $TIP"
NAME=object-info-no-oid info size
NAME=object-info-upper info size "oid $(echo "$A" | tr a-f A-F)"
# bundle-uri: a bundle of the first commit, offered by a URI relative to the server's.
git branch base main~1; git bundle create -q "$WORK/base.bundle" base; git branch -q -D base
git config bundle.version 1; git config bundle.mode all; git config bundle.heuristic creationToken
git config bundle.base.uri ../base.bundle; git config bundle.base.creationToken 1
{ pkt $'object-format=sha1\n'; pkt $'command=bundle-uri\n'; printf 0001; printf 0000; } > "$OUT/bundle-uri.request.bin"
v2 < "$OUT/bundle-uri.request.bin" > "$OUT/bundle-uri.response.bin"
cat > "$WORK/capture" <<'SH'
#!/bin/sh
if [ -e "$CAPTURE.request.bin" ]; then echo "a second upload-pack for $CAPTURE" >&2; exit 1; fi
tee "$CAPTURE.request.bin" | git upload-pack "$1" | tee "$CAPTURE.response.bin"
SH
chmod +x "$WORK/capture"
(cd "$WORK" && CAPTURE="$OUT/bundle-clone" git -c protocol.version=2 -c transfer.bundleURI=true clone -q --no-local \
  --upload-pack="$WORK/capture" "file://$WORK/server" bundled)
test "$(git -C "$WORK/bundled" rev-parse main)" = "$TIP"
git -C "$WORK/bundled" for-each-ref --format='%(objectname) %(refname)' > "$OUT/bundle-clone.refs.txt"
# packfile-uris: the larger blob named by a pack of its own at a URI.
URIPACK=$(echo "$BIG" | git pack-objects -q "$WORK/uri")
git config uploadpack.blobPackfileUri "$BIG $URIPACK https://example.invalid/$URIPACK.pack"
uris() { { pkt $'command=fetch\n'; pkt $'object-format=sha1\n'; printf 0001
  pkt $'ofs-delta\n'; pkt $'no-progress\n'; for line in "$@"; do pkt "$line"$'\n'; done
  pkt "want $TIP"$'\n'; pkt $'done\n'; printf 0000; } > "$OUT/$NAME.request.bin"
  v2 < "$OUT/$NAME.request.bin" > "$OUT/$NAME.response.bin"; }
NAME=uris-https uris sideband-all 'packfile-uris https'
NAME=uris-http uris sideband-all 'packfile-uris http'
NAME=uris-no-sideband-all uris 'packfile-uris https'
NAME=uris-ftp-https uris sideband-all 'packfile-uris ftp,https'
printf '%s https://example.invalid/%s.pack\n' "$URIPACK" "$URIPACK" > "$OUT/uri-pack.txt"
# The advertisement, with every feature this piece reads on: object-info, bundle-uri, sideband-all and packfile-uris.
adv > "$OUT/advertisement.bin"
# A SHA-256 repository's object-info.
git init -q -b main --object-format=sha256 "$WORK/sha256"; cd "$WORK/sha256"; setup
echo one > a; git add a; git commit -q -m one
git config transfer.advertiseObjectInfo true
git rev-list --objects --all | cut -d' ' -f1 | while read -r object; do
  printf '%s %s %s\n' "$object" "$(git cat-file -t "$object")" "$(git cat-file -s "$object")"; done > "$OUT/sha256-objects.txt"
{ pkt $'command=object-info\n'; pkt $'object-format=sha256\n'; printf 0001; pkt $'size\n'
  pkt "oid $(git rev-parse main:a)"$'\n'; pkt "oid $(git rev-parse main)"$'\n'; printf 0000; } > "$OUT/object-info-sha256.request.bin"
v2 < "$OUT/object-info-sha256.request.bin" > "$OUT/object-info-sha256.response.bin"
cd "$OUT"
find . -type f ! -name SHA256SUMS ! -name make-v2-rest-fixtures.sh | LC_ALL=C sort | sed 's|^\./||' | xargs shasum -a 256 > SHA256SUMS
