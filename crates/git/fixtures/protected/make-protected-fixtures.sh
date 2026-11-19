#!/usr/bin/env bash
# Makes slates-git's golden vectors for protected branches (A-644; §4.17.2 "Protected branches, in git's terms"): pushes
# stock git 2.50.1 makes to stock `git receive-pack` serving a protected `main`, the bytes each side sent captured by a
# wrapper between them (`git push --receive-pack=<wrapper>`), as fixtures/push's script captures its pushes:
# - <scenario>.request.bin: what `git push` sent, its commands and its pack;
# - <scenario>.response.bin: what `git receive-pack` answered, its ref advertisement and its report;
# - <scenario>.before.refs and <scenario>.before.pack: the server's refs before the push, and every object it held as one
#   pack, from which a test makes the repository the push met.
# The server protects `refs/heads/main` the ways git can: `receive.denyNonFastForwards` and `receive.denyDeletes`, git's
# own; and an `update` hook for what git leaves to hooks, a linear history (no merge commit among the commits the push
# brings to `main`) and commits signed by a trusted key (`git verify-commit` against `gpg.ssh.allowedSignersFile`).
# The hook's words are the ones slates answers with; git adds its own after them (`hook declined to update <ref>`) and
# reports `hook declined`.
# The scenarios, in order, on one SHA-1 server (each push by id, so the client's branches never move):
# - create: `main` made at a history of two commits the trusted key signed;
# - signed: `main` fast-forwarded to a third signed commit;
# - unsigned: a fast-forward to an unsigned commit, refused;
# - untrusted: a fast-forward to a commit another key signed, refused;
# - openpgp: a fast-forward to a commit whose signature is OpenPGP's armour, refused (OpenPGP is a later piece);
# - x509: the same with X.509's armour, refused;
# - merge: a fast-forward to a signed merge commit, refused;
# - feature: the same merge pushed to `feature`, which nothing protects;
# - non-ff: `main` forced to a signed commit beside its head, refused by git (non-fast-forward);
# - delete: `main` deleted, refused by git (deletion prohibited).
# Then on a SHA-256 server protected the same way: sha256-create (a signed history) and sha256-unsigned (an unsigned
# commit on it, refused), the signature in `gpgsig-sha256`.
# And what git's verifier reads of the signed scenario's commit and of sha256-create's head (<scenario>.signature and
# <scenario>.payload): the signature and the payload it covers, split as commit.c's parse_buffer_signed_by_header
# splits them, each pair checked by `ssh-keygen -Y verify` under the trusted key before it is kept.
# Every key is made fresh by this run, as A-321's golden signatures were; only the public halves are kept
# (trusted.pub, other.pub), so a run makes new signatures and new ids for every signed commit. All else is fixed: no
# configuration but this script's, a fixed identity, fixed dates, one thread of delta search. Then SHA256SUMS lists
# every file made.
# Usage: make-protected-fixtures.sh OUTDIR
#
# Sources: git's Documentation/gitprotocol-pack.adoc ("Report Status"), config/receive.adoc (denyNonFastForwards,
# denyDeletes), githooks.adoc ("update"), config/gpg.adoc (gpg.format, gpg.ssh.allowedSignersFile),
# builtin/receive-pack.c (update(), run_update_hook(), copy_to_sideband()) and gpg-interface.c, v2.50.1.
set -euo pipefail
OUT=$(cd "$1" && pwd)
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE GIT_PROTOCOL
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
WORK=$(mktemp -d "$HOME/.cache/slates-git-protected.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"; mkdir -p "$HOME"
git --version > "$OUT/git-version.txt"
ssh -V 2> "$OUT/ssh-version.txt"
# The trusted signer and another key, made fresh; the server trusts the first alone.
ssh-keygen -q -t ed25519 -N '' -C trusted -f "$WORK/trusted"
ssh-keygen -q -t ed25519 -N '' -C other -f "$WORK/other"
cp "$WORK/trusted.pub" "$OUT/trusted.pub"
cp "$WORK/other.pub" "$OUT/other.pub"
printf 'test@example.com namespaces="git" %s\n' "$(cut -d' ' -f1,2 "$WORK/trusted.pub")" > "$WORK/allowed_signers"
# The update hook: `main` keeps a linear history of commits the trusted key signed, in slates' words.
HOOK="$WORK/update"
cat > "$HOOK" <<'SH'
#!/bin/sh
ref=$1 old=$2 new=$3
[ "$ref" = refs/heads/main ] || exit 0
case $new in *[!0]*) ;; *) exit 0 ;; esac
case $old in *[!0]*) range="$old..$new" ;; *) range=$new ;; esac
merge=$(git rev-list --merges -n 1 $range)
if [ -n "$merge" ]; then
  printf 'error: denying merge commit %s on %s (linear history required)\n' "$merge" "$ref" >&2
  exit 1
fi
for commit in $(git rev-list $range); do
  armour=$(git cat-file commit "$commit" | sed -n '/^$/q;s/^gpgsig\(-sha256\)\{0,1\} //p')
  case $armour in
    '') line="denying unsigned commit $commit on $ref (signed commits required)" ;;
    '-----BEGIN SSH SIGNATURE-----')
      git verify-commit "$commit" > /dev/null 2>&1 && continue
      line="denying commit $commit on $ref: its signer is not trusted (signed commits required)" ;;
    '-----BEGIN PGP SIGNATURE-----' | '-----BEGIN PGP MESSAGE-----')
      line="denying commit $commit on $ref: its signature is OpenPGP, which this server does not verify (signed commits required)" ;;
    '-----BEGIN SIGNED MESSAGE-----')
      line="denying commit $commit on $ref: its signature is X.509, which this server does not verify (signed commits required)" ;;
    *) line="denying commit $commit on $ref: its signature is in no format git signs with (signed commits required)" ;;
  esac
  printf 'error: %s\n' "$line" >&2
  exit 1
done
exit 0
SH
chmod +x "$HOOK"
WRAP="$WORK/wrap.sh"
cat > "$WRAP" <<'SH'
#!/bin/sh
tee "$CAPTURE.request.bin" | git receive-pack "$@" | tee "$CAPTURE.response.bin"
SH
chmod +x "$WRAP"
# A protected server of object format $1 at $2.
serve() {
  git init -q --bare -b main --object-format="$1" "$2"
  git -C "$2" config receive.denyNonFastForwards true
  git -C "$2" config receive.denyDeletes true
  git -C "$2" config gpg.ssh.allowedSignersFile "$WORK/allowed_signers"
  cp "$HOOK" "$2/hooks/update"
}
# A client of object format $1 at $2, signing with the trusted key when asked.
client() {
  git init -q -b main --object-format="$1" "$2"
  git -C "$2" config gpg.format ssh
  git -C "$2" config user.signingkey "$WORK/trusted"
  git -C "$2" config commit.gpgsign false
}
at=1112911993
# A tree of one file holding $1.
tree() { printf '100644 blob %s\tfile\n' "$(printf '%s\n' "$1" | git hash-object -w --stdin)" | git mktree; }
# A commit of the tree holding $2, with the parents after it, signed as $1 says: `-S` by the trusted key,
# `-S<key>` by another, `--no-gpg-sign` not at all.
commit() {
  sign=$1 text=$2; shift 2
  at=$((at + 60))
  parents=(); for parent in "$@"; do parents+=(-p "$parent"); done
  GIT_AUTHOR_DATE="$at +0000" GIT_COMMITTER_DATE="$at +0000" \
    git commit-tree "$sign" "$(tree "$text")" ${parents[@]+"${parents[@]}"} -m "$text"
}
# A commit on $1 holding $2 whose signature header holds the armour $3 ... $4 around a made-up body, written by hand
# as a signing program would leave it (git keeps any signature as written; only `verify-commit` reads one).
armoured() {
  at=$((at + 60))
  { printf 'tree %s\nparent %s\n' "$(tree "$2")" "$1"
    printf 'author test <test@example.com> %s +0000\ncommitter test <test@example.com> %s +0000\n' "$at" "$at"
    printf 'gpgsig %s\n \n iQIzBAABCAAdFiEEAAAAAAAAAAAAAAAAAAAAAAAAAAAFAmZmZmYACgkQAAAAAAAAAAAA\n %s\n' "$3" "$4"
    printf '\n%s\n' "$2"; } | git hash-object -t commit -w --stdin
}
# Splits commit $1's signature in header $2 from the payload it covers into $3.signature and $3.payload, as
# parse_buffer_signed_by_header does: the header's lines and their continuations are the signature, any other line
# beginning `gpgsig` is left out with its continuations, everything else is the payload; then checks the pair.
split_signed() {
  git cat-file commit "$1" | awk -v own="$2" -v sig="$OUT/$3.signature" -v payload="$OUT/$3.payload" '
    body { print > payload; next }
    insig && substr($0, 1, 1) == " " { print substr($0, 2) > sig; next }
    index($0, own " ") == 1 { print substr($0, length(own) + 2) > sig; insig = 1; other = 0; next }
    { insig = 0 }
    index($0, "gpgsig") == 1 { other = 1; next }
    other && substr($0, 1, 1) == " " { next }
    { other = 0; if ($0 == "") body = 1; print > payload }'
  ssh-keygen -Y verify -f "$WORK/allowed_signers" -I test@example.com -n git -s "$OUT/$3.signature" \
    < "$OUT/$3.payload" > /dev/null
}
# The server's refs and objects before a push, then the push through the wrapper; a refused push is a capture too.
push() {
  scenario=$1 server=$2; shift 2
  git -C "$server" for-each-ref --format='%(objectname) %(refname)' > "$OUT/$scenario.before.refs"
  git -C "$server" rev-list --objects --all | cut -d' ' -f1 \
    | git -C "$server" -c pack.threads=1 pack-objects -q --stdout > "$OUT/$scenario.before.pack"
  CAPTURE="$OUT/$scenario" git -c pack.threads=1 push -q --receive-pack="$WRAP" "$server" "$@" || true
}
SERVER="$WORK/server.git"
serve sha1 "$SERVER"
client sha1 "$WORK/client"
cd "$WORK/client"
first=$(commit -S first)
second=$(commit -S second "$first")
push create "$SERVER" "$second:refs/heads/main"
third=$(commit -S third "$second")
push signed "$SERVER" "$third:refs/heads/main"
split_signed "$third" gpgsig signed
push unsigned "$SERVER" "$(commit --no-gpg-sign unsigned "$third"):refs/heads/main"
push untrusted "$SERVER" "$(commit "-S$WORK/other" untrusted "$third"):refs/heads/main"
push openpgp "$SERVER" \
  "$(armoured "$third" openpgp '-----BEGIN PGP SIGNATURE-----' '-----END PGP SIGNATURE-----'):refs/heads/main"
push x509 "$SERVER" \
  "$(armoured "$third" x509 '-----BEGIN SIGNED MESSAGE-----' '-----END SIGNED MESSAGE-----'):refs/heads/main"
side=$(commit -S side "$second")
merge=$(commit -S merge "$third" "$side")
push merge "$SERVER" "$merge:refs/heads/main"
push feature "$SERVER" "$merge:refs/heads/feature"
push non-ff "$SERVER" --force "$(commit -S rewritten "$second"):refs/heads/main"
push delete "$SERVER" ":refs/heads/main"
SERVER256="$WORK/server256.git"
serve sha256 "$SERVER256"
client sha256 "$WORK/client256"
cd "$WORK/client256"
first=$(commit -S first)
second=$(commit -S second "$first")
push sha256-create "$SERVER256" "$second:refs/heads/main"
split_signed "$second" gpgsig-sha256 sha256-create
push sha256-unsigned "$SERVER256" "$(commit --no-gpg-sign unsigned "$second"):refs/heads/main"
cd "$OUT" && shasum -a 256 git-version.txt ssh-version.txt ./*.pub ./*.bin ./*.refs ./*.pack ./*.signature \
  ./*.payload | sed 's| \./| |' > SHA256SUMS
