#!/usr/bin/env bash
# Makes slates-git's fsck golden vectors (A-590; §4.17.2 "Pieces (git)", piece 4) with stock git:
# - in a SHA-1 and a SHA-256 repository, trees, commits and tags as git's own commands write them, and the malformed
#   ones git's own t/t1450-fsck.sh makes (`git hash-object --literally`), with one more for each check fsck.c makes;
# - in the SHA-1 repository, one object for each name git judges by spelling: every spelling of `.gitmodules`,
#   `.gitattributes`, `.gitignore` and `.mailmap` in git's t/t0060-path-utils.sh (each a symbolic link, the entry fsck
#   reports by name), spellings of `.git` that HFS+ and NTFS resolve and some they do not (each a file), and tag names
#   on either side of refs.c's rules (each a tag).
# For each object, what stock git says of it.
#
# Usage: make-fsck-fixtures.sh OUTDIR (OUTDIR must hold no sha1/ or sha256/ yet).
# Writes OUTDIR/<format>/<name> (an object's content, no header) and one line of OUTDIR/manifest.txt for it:
#   <format> <name> <type> <object id> <git fsck> <git fsck --strict> <git hash-object>
# Each `git fsck` column is `-` when git said nothing of the object, `unparsed` when it could not parse it ("object
# could not be parsed", no message id), or its findings in order, each `<error|warning>:<message id>`, comma-separated.
# The last column is `ok` or `refused`, whether `git hash-object -t <type>` wrote the object (it checks with fsck's own
# code, strict, on the bytes alone), then `:<the first message id it printed>` when it printed one.
# Then OUTDIR/SHA256SUMS (every file's SHA-256) and OUTDIR/table.rs (the objects compiled into the tests, never read
# at run time).
# Deterministic: no configuration but this script's, a fixed identity, every date 2005-04-07T22:13:13Z. A second run
# writes the same bytes.
#
# Sources (git v2.50.1): t/t1450-fsck.sh (each of its malformed objects below names its test), t/t0060-path-utils.sh
# (the spellings), fsck.c and fsck.h (the checks and their message ids), refs.c (check_refname_format),
# Documentation/gitformat-signature.txt (gpgsig, mergetag), git-mktree, git-hash-object.
set -euo pipefail
OUT=$(cd "$1" && pwd)
if [ -e "$OUT/sha1" ] || [ -e "$OUT/sha256" ]; then
  echo "make-fsck-fixtures.sh: $OUT already holds sha1/ or sha256/; give it an empty directory" >&2
  exit 1
fi
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
export GIT_AUTHOR_NAME='A U Thor' GIT_AUTHOR_EMAIL=author@example.com
export GIT_COMMITTER_NAME='C O Mitter' GIT_COMMITTER_EMAIL=committer@example.com
export GIT_AUTHOR_DATE='1112911993 +0000' GIT_COMMITTER_DATE='1112911993 +0000'
WORK=$(mktemp -d "$HOME/.cache/slates-git-fsckfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
export HOME="$WORK/home"; mkdir -p "$HOME"
git --version > "$OUT/git-version.txt"
: > "$OUT/manifest.txt"

# What `git fsck $1` says of object $oid of type $type (both from keep's scope).
findings() {
  local said ids
  said=$(git fsck $1 --no-dangling --no-progress --no-reflogs 2>&1 || true)
  if grep -q "^error: $oid: object could not be parsed" <<<"$said"; then echo unparsed; return; fi
  ids=$(sed -E -n "s/^(error|warning) in $type $oid: ([a-zA-Z0-9]+):.*/\1:\2/p" <<<"$said" | paste -sd, - || true)
  echo "${ids:--}"
}
# Whether `git hash-object` writes object $oid of type $type, and the first message id it printed.
hash_verdict() {
  local said verdict=ok first
  said=$(git cat-file "$type" "$oid" | git hash-object -t "$type" --stdin 2>&1 >/dev/null) || verdict=refused
  first=$(sed -E -n 's/^error: object fails fsck: ([a-zA-Z0-9]+):.*/\1/p' <<<"$said" | head -1 || true)
  echo "$verdict${first:+:$first}"
}
# Records object $3 of type $2 under name $1: its content, and what git says of it.
keep() {
  local name=$1 type=$2 oid=$3
  git cat-file "$type" "$oid" > "$OUT/$FORMAT/$name"
  echo "$FORMAT $name $type $oid $(findings '') $(findings --strict) $(hash_verdict)" >> "$OUT/manifest.txt"
}
lit() { git hash-object --literally -w -t "$1" --stdin; }
# Its input without its last byte (macOS's head takes no negative count).
drop_last() { local f; f=$(mktemp "$WORK/cut.XXXXXX"); cat > "$f"; head -c "$(( $(wc -c < "$f") - 1 ))" "$f"; }
hex2oct() { sed 's/\(..\)/\\x\1/g'; }
# A tree's entry: mode $1, the name $2 as printf %b reads its escapes, the object id $3 in raw bytes.
entry() { printf '%s %s\0' "$1" "$(printf '%b' "$2")"; printf "$(echo "$3" | hex2oct)"; }

# The spellings each name check judges, as printf %b reads them; git's own verdict on each is what the manifest keeps.
# `.git` (fsck_tree: is_hfs_dotgit || is_ntfs_dotgit, and is_ntfs_dotgit after each backslash): file entries.
DOTGIT=('.git' '.GIT' '.Git' '.gIt' 'git~1' 'GIT~1' 'Git~1' '.git.' '.git..' '.git ' '.git . .' '.git:'
  '.git::$INDEX_ALLOCATION' '.git:foo' 'git~1.' 'git~1 ' 'git~1:x' '.git\\x' 'x\\.git' 'x\\git~1'
  '.\xe2\x80\x8cgit' '.g\xe2\x80\x8cit' '.git\xe2\x80\x8c' '.git\xef\xbb\xbf' '.git\xff' '.git\xef\xbf\xbe'
  '.git\xed\xa0\x80' '.git\xc0\xaf' '.git/x' '.gitx' '.gi' 'git~2' 'git~1x' '..git' ' .git' '.git x' 'x.git'
  '.g\xc3\xaft' '.git\xc2\xa0' '.git\xef\xbf\xbd' '.git\xe2\x80\x8cx' '.\xc5\x87it')
# t0060 "match .gitmodules": its matches, then its refusals (after its `--not`), then a backslash's either side.
GITMODULES=('.gitmodules' '.git\xe2\x80\x8cmodules' '.Gitmodules' '.gitmoduleS'
  '.gitmodules ' '.gitmodules.' '.gitmodules  ' '.gitmodules. ' '.gitmodules .' '.gitmodules..' '.gitmodules   '
  '.gitmodules.  ' '.gitmodules . ' '.gitmodules  .'
  '.Gitmodules ' '.Gitmodules.' '.Gitmodules  ' '.Gitmodules. ' '.Gitmodules .' '.Gitmodules..' '.Gitmodules   '
  '.Gitmodules.  ' '.Gitmodules . ' '.Gitmodules  .'
  'GITMOD~1' 'gitmod~1' 'GITMOD~2' 'gitmod~3' 'GITMOD~4'
  'GITMOD~1 ' 'gitmod~2.' 'GITMOD~3  ' 'gitmod~4. ' 'GITMOD~1 .' 'gitmod~2   ' 'GITMOD~3.  ' 'gitmod~4 . '
  'GI7EBA~1' 'gi7eba~9' 'GI7EB~10' 'GI7EB~11' 'GI7EB~99' 'GI7E~100' 'GI7E~101' 'GI7E~999' '~1000000' '~9999999'
  '.gitmodules:$DATA' 'gitmod~4 . :$DATA'
  '.gitmodules x' '.gitmodules .x' ' .gitmodules' '..gitmodules' 'gitmodules' '.gitmodule' '.gitmodules x '
  'GI7EBA~' 'GI7EBA~0' 'GI7EBA~~1' 'GI7EBA~X' 'Gx7EBA~1' 'GI7EBX~1' 'GI7EB~1' 'GI7EB~01' 'GI7EB~1X'
  '.gitmodules,:$DATA'
  'x\\.gitmodules' 'x\\GITMOD~1' '.gitmodules\\x' 'GITMOD~5' 'x\\.gitmodules\\x' 'x\\gitmod~1\\x' 'x\\y\\.gitmodules')
# t0060 "match .gitattributes", ".gitignore" and ".mailmap", then spellings each refuses.
GITATTRIBUTES=('.gitattributes' '.git\xe2\x80\x8cattributes' '.Gitattributes' '.gitattributeS' 'GITATT~1' 'GI7D29~1'
  '.gitattribute' 'gitattributes' 'GITATT~5' 'GI7D29~0')
GITIGNORE=('.gitignore' '.git\xe2\x80\x8cignore' '.Gitignore' '.gitignorE' 'GITIGN~1' 'GI250A~1'
  '.gitignor' 'gitignore' 'GITIGN~5')
MAILMAP=('.mailmap' '.mail\xe2\x80\x8cmap' '.Mailmap' '.mailmaP' 'MAILMA~1' 'MABA30~1' '.mailma' 'mailmap' 'MAILMA~5')
# Tag names: refs/tags/<name> by check_refname_format with no flags (refs.c), on either side of each rule.
TAGS=('v1' 'v1.0' 'a/b' 'a-b_c' '@' 'a@b' 'a{b' 'a}b' '\xc3\xa9' '-a' 'a.b.c' 'a/b/c' 'a.lockx'
  '' '.a' 'a/.b' 'a.' 'a/b.' 'a..b' 'a.lock' 'a/b.lock' 'a.lock/b' 'a@{b' 'a*' 'a b' 'a~1' 'a^' 'a:b' 'a?' 'a['
  'a\\b' 'a\x7f' 'a\x01' 'a//b' 'a/' '/a' '@{')

for FORMAT in sha1 sha256; do
  mkdir -p "$OUT/$FORMAT"
  rm -rf "$WORK/repo"; git init -q -b main --object-format=$FORMAT "$WORK/repo"; cd "$WORK/repo"
  git config core.protectNTFS false; git config core.protectHFS false
  ZERO=$(printf '%0*d' "$(git rev-parse --show-object-format=storage | sed 's/sha1/40/;s/sha256/64/')" 0)
  blob=$(echo blob | git hash-object -w --stdin)
  exec_blob=$(printf '#!/bin/sh\n' | git hash-object -w --stdin)
  link=$(printf 'target' | git hash-object -w --stdin)
  sub=$(printf 'a submodule commit\n' | git hash-object --stdin)   # not in the repository: a gitlink names another's commit
  # Well formed, as git's own commands write them: every mode git writes, and the order a directory's implied '/' makes.
  inner=$(printf '100644 blob %s\tfile\n' "$blob" | git mktree)
  tree=$( { printf '100644 blob %s\tlib-x\n' "$blob"; printf '100644 blob %s\tlib.rs\n' "$blob"
            printf '040000 tree %s\tlib\n' "$inner"; printf '100755 blob %s\trun\n' "$exec_blob"
            printf '120000 blob %s\tlink\n' "$link"; printf '160000 commit %s\tsub\n' "$sub"; } | git mktree --missing)
  keep ok-tree tree "$tree"
  keep ok-tree-empty tree "$(git mktree </dev/null)"
  root=$(echo 'first' | git commit-tree "$inner"); keep ok-commit-root commit "$root"
  side=$(echo 'side' | git commit-tree "$inner" -p "$root")
  merge=$(echo 'merge' | git commit-tree "$tree" -p "$root" -p "$side"); keep ok-commit-merge commit "$merge"
  enc=$(printf 'caf\xe9\n' | git -c i18n.commitEncoding=ISO-8859-1 commit-tree "$inner" -p "$root")
  keep ok-commit-encoding commit "$enc"
  # t1450 "commit with multiple signatures is okay": gpgsig and gpgsig-sha256, each with continuation lines.
  git cat-file commit "$root" > basis
  { sed -e '/^committer/q' basis
    printf 'gpgsig -----BEGIN PGP SIGNATURE-----\n  VGhpcyBpcyBub3QgcmVhbGx5IGEgc2lnbmF0dXJlLg==\n  -----END PGP SIGNATURE-----\n'
    printf 'gpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n  VGhpcyBpcyBub3QgcmVhbGx5IGEgc2lnbmF0dXJlLg==\n  -----END PGP SIGNATURE-----\n'
    echo; sed -e '1,/^$/d' basis; } > signed
  keep ok-commit-gpgsig commit "$(git hash-object -t commit -w --stdin <signed)"
  # An annotated tag, and one whose message ends with a signature block (gitformat-signature).
  git update-ref refs/heads/main "$merge"
  git tag -a -m 'version one' v1 "$root"; keep ok-tag tag "$(git rev-parse v1)"
  { git cat-file tag v1; printf -- '-----BEGIN PGP SIGNATURE-----\n\nbm90IGEgc2lnbmF0dXJl\n-----END PGP SIGNATURE-----\n'; } > tag-signed
  signed_tag=$(git hash-object -t tag -w --stdin <tag-signed); keep ok-tag-signed tag "$signed_tag"
  # A merge of that tag: its mergetag header carries the whole tag, each line after the first a continuation.
  { sed -e '/^committer/q' <(git cat-file commit "$merge")
    printf 'mergetag '; sed -e '2,$s/^/ /' tag-signed; echo; echo 'merge tag v1'; } > merged
  keep ok-commit-mergetag commit "$(git hash-object -t commit -w --stdin <merged)"
  { sed -e '/^committer/q' basis; echo 'x-extra one value'; echo; sed -e '1,/^$/d' basis; } > extra
  keep ok-commit-extra commit "$(git hash-object -t commit -w --stdin <extra)"
  keep ok-commit-nomessage commit "$(sed -e '/^committer/q' basis | git hash-object -t commit -w --stdin)"
  keep ok-email-without-at commit "$(sed 's/@/AT/' basis | git hash-object -t commit -w --stdin)"   # email without @ is okay
  # fsck_ident skips spaces and tabs before a date, and a date git's 64-bit time_t holds is fine (date_overflows).
  keep ok-date-after-blanks commit "$(sed "s/^\(author .*>\) /\1 $(printf '\t') /" basis | lit commit)"
  keep ok-date-time-t-max commit "$(sed 's/^\(author .*>\) [0-9]*/\1 9223372036854775807/' basis | lit commit)"

  # t1450's malformed commits, each named for its test.
  keep bad-email-gt commit "$(sed 's/@[a-z]/&>/' basis | lit commit)"          # email with embedded > is not okay
  keep bad-email-no-lt commit "$(sed 's/<//' basis | lit commit)"              # missing < email delimiter
  keep bad-email-missing commit "$(sed 's/[a-z]* <[^>]*>//' basis | lit commit)" # missing email
  keep bad-name-gt commit "$(sed 's/ </> </' basis | lit commit)"              # > in name is reported
  keep bad-date-overflow commit "$(sed 's/^\(author .*>\) [0-9]*/\1 18446744073709551617/' basis | lit commit)"
  keep bad-nul-header commit "$(sed 's/author ./author Q/' basis | tr Q '\000' | lit commit)"
  keep bad-multiple-authors commit "$(sed 's/^author .*/&,&/' basis | tr , '\n' | lit commit)"
  keep warn-nul-in-commit commit "$(printf 'initial commitQNUL after message\n' | git commit-tree "$inner" | xargs git cat-file commit | tr Q '\000' | lit commit)"
  # Each of fsck_commit's and fsck_ident's other refusals (fsck.c), one per object.
  keep bad-missing-tree commit "$(sed '1d' basis | lit commit)"
  keep bad-tree-sha1 commit "$(sed '1s/^tree ./tree z/' basis | lit commit)"
  keep bad-parent-sha1 commit "$(git cat-file commit "$enc" | sed 's/^parent ./parent z/' | lit commit)"
  keep bad-missing-author commit "$(sed '/^author /d' basis | lit commit)"
  keep bad-missing-committer commit "$(sed '/^committer /d' basis | lit commit)"
  keep bad-zero-padded-date commit "$(sed 's/^\(author .*>\) /\1 0/' basis | lit commit)"
  keep bad-timezone commit "$(sed 's/^\(author .*\) +0000$/\1 +000/' basis | lit commit)"
  keep bad-space-before-date commit "$(sed 's/^\(author .*>\) /\1/' basis | lit commit)"
  keep bad-name-before-email commit "$(sed 's/^author [^<]*/author /' basis | lit commit)"
  keep bad-space-before-email commit "$(sed 's/^\(author A U Thor\) /\1/' basis | lit commit)"
  keep bad-date commit "$(sed 's/^\(author .*>\) [0-9]*/\1 x/' basis | lit commit)"
  keep bad-date-trailing commit "$(sed 's/^\(author .*> [0-9]*\) /\1x /' basis | lit commit)"
  keep bad-date-past-time-t commit "$(sed 's/^\(author .*>\) [0-9]*/\1 9223372036854775808/' basis | lit commit)"
  keep bad-email-lt commit "$(sed '/^author /s/@/</' basis | lit commit)"
  keep bad-unterminated-header commit "$(sed -e '/^committer/q' basis | drop_last | lit commit)"

  # t1450's malformed trees.
  e=$(git hash-object -w --stdin </dev/null)
  t=$(printf '100644 blob %s\tx\n' "$e" | git mktree)
  keep bad-tree-duplicate tree "$( (git cat-file tree "$t"; git cat-file tree "$t") | lit tree)"
  x2=$(printf '100644 blob %s\tx.2\n' "$blob" | git mktree)
  n=0; for names in 'x x.1 x/' 'x x.1.2 x.1/ x/' 'x x.1 x.1.2 x/'; do n=$((n + 1))
    keep "bad-tree-duplicate-names-$n" tree "$(for name in $names; do case $name in
      (*/) printf '040000 tree %s\t%s\n' "$x2" "${name%/}";; (*) printf '100644 blob %s\t%s\n' "$blob" "$name";; esac; done | git mktree)"
  done
  junk=$(printf '%s' twenty-bytes-of-junk-twelve-more | head -c "$(( ${#ZERO} / 2 ))")
  keep bad-tree-unparseable tree "$(printf "100644 \0$junk" | lit tree)"         # unparseable tree object
  keep info-tree-bogus-mode tree "$(printf "100000 foo\0$(echo "$blob" | hex2oct)" | lit tree)"
  keep warn-tree-null-blob tree "$(printf "100644 file\0$(echo "$ZERO" | hex2oct)" | lit tree)"
  keep warn-tree-null-gitlink tree "$(printf "160000 submodule\0$(echo "$ZERO" | hex2oct)" | lit tree)"
  long=$(printf 'n%.0s' $(seq 1 4097))
  keep warn-tree-large-name tree "$(printf "100644 $long\0$(echo "$blob" | hex2oct)" | lit tree)"
  n=0; while read -r path; do n=$((n + 1))
    keep "warn-tree-dot-$n" tree "$(printf '100644 blob %s\t%s' "$blob" "$path" | git mktree)"
    keep "warn-tree-dot-$n-dir" tree "$(printf '040000 tree %s\t%s' "$inner" "$path" | git mktree)"
  done <<-EOF
	.
	..
	.git
	.GIT
	.gI$(printf '\342\200\214')T
	.Git
	git~1
	.git.
	.\\.GIT\\foobar
	.git\\foobar
	EOF
  keep ok-tree-not-dotgit tree "$(printf '100644 blob %s\t.\305\207it' "$blob" | git mktree)"   # fsck allows .Ňit
  # Each of fsck_tree's other findings (fsck.c), one per object.
  keep warn-tree-zero-padded tree "$(printf "040000 dir\0$(echo "$inner" | hex2oct)" | lit tree)"
  keep warn-tree-full-path tree "$(printf "100644 a/b\0$(echo "$blob" | hex2oct)" | lit tree)"
  keep bad-tree-unsorted tree "$(printf "100644 b\0$(echo "$blob" | hex2oct)100644 a\0$(echo "$blob" | hex2oct)" | lit tree)"
  keep ok-tree-mode-664 tree "$(printf "100664 old\0$(echo "$blob" | hex2oct)" | lit tree)"
  keep bad-tree-gitmodules-symlink tree "$(printf "120000 .gitmodules\0$(echo "$link" | hex2oct)" | lit tree)"
  keep info-tree-gitignore-symlink tree "$(printf "120000 .gitignore\0$(echo "$link" | hex2oct)" | lit tree)"
  keep info-tree-gitattributes-symlink tree "$(printf "120000 .gitattributes\0$(echo "$link" | hex2oct)" | lit tree)"
  keep info-tree-mailmap-symlink tree "$(printf "120000 .mailmap\0$(echo "$link" | hex2oct)" | lit tree)"
  keep bad-tree-truncated tree "$(printf "100644 cut\0$(echo "$blob" | hex2oct)" | drop_last | lit tree)"
  # A tree's walk decodes the next entry before it judges this one's mode and order (fsck_tree): a second entry it
  # cannot decode is badTree, reported in the walk, ahead of the first entry's name findings, and the first entry's
  # mode is never judged.
  keep bad-tree-second-entry tree "$( { entry 100644 .git "$blob"; entry 10x644 b "$blob"; } | lit tree)"
  keep bad-tree-mode-unjudged tree "$( { entry 100000 a "$blob"; entry 100644 '' "$blob"; } | lit tree)"
  # parse_mode keeps 16 bits (object.h): 1100644 reads as 100644.
  keep ok-tree-mode-wraps tree "$(entry 1100644 a "$blob" | lit tree)"
  # .gitmodules and .gitattributes as files: fsck keeps their ids to check the blobs (gitmodules_found).
  keep ok-tree-special-blobs tree "$( { entry 100644 .gitattributes "$blob"; entry 100644 .gitmodules "$exec_blob"; } | lit tree)"

  # t1450's malformed tags.
  head=$root
  keep warn-tag-name-and-tagger tag "$(printf 'object %s\ntype commit\ntag wrong name format\n\nThis is an invalid tag.\n' "$head" | lit tag)"
  keep bad-tag-tagger tag "$(printf 'object %s\ntype commit\ntag not-quite-wrong\ntagger Bad Tagger Name\n\nThis is an invalid tag.\n' "$head" | lit tag)"
  keep bad-tag-nul-header tag "$(printf 'object %s\ntype commit\ntag contains-Q-in-header\ntagger T A Gger <tagger@example.com> 1234567890 -0000\n\nThis is an invalid tag.\n' "$head" | tr Q '\000' | lit tag)"
  # Each of fsck_tag_standalone's other findings (fsck.c), one per object.
  tg='tagger T A Gger <tagger@example.com> 1234567890 -0000'
  keep bad-tag-missing-object tag "$(printf 'type commit\ntag v\n%s\n\nm\n' "$tg" | lit tag)"
  keep bad-tag-object-sha1 tag "$(printf 'object zz\ntype commit\ntag v\n%s\n\nm\n' "$tg" | lit tag)"
  keep bad-tag-missing-type tag "$(printf 'object %s\ntag v\n%s\n\nm\n' "$head" "$tg" | lit tag)"
  keep bad-tag-type tag "$(printf 'object %s\ntype comet\ntag v\n%s\n\nm\n' "$head" "$tg" | lit tag)"
  keep bad-tag-missing-tag tag "$(printf 'object %s\ntype commit\n%s\n\nm\n' "$head" "$tg" | lit tag)"
  keep ignore-tag-extra-header tag "$(printf 'object %s\ntype commit\ntag v\n%s\nx-extra 1\n\nm\n' "$head" "$tg" | lit tag)"
  # git checks a bad tagger's result only after the extra-header check, which is ignored and so overwrites it: git
  # prints the tagger's error and still writes the tag. slates refuses it (A-590).
  keep bad-tag-tagger-extra-header tag "$(printf 'object %s\ntype commit\ntag v\ntagger Bad Tagger Name\nx-extra 1\n\nm\n' "$head" | lit tag)"

  if [ "$FORMAT" = sha1 ]; then
    n=0; for name in "${DOTGIT[@]}"; do n=$((n + 1))
      keep "name-dotgit-$(printf %02d $n)" tree "$(entry 100644 "$name" "$blob" | lit tree)"; done
    n=0; for name in "${GITMODULES[@]}"; do n=$((n + 1))
      keep "name-gitmodules-$(printf %02d $n)" tree "$(entry 120000 "$name" "$link" | lit tree)"; done
    n=0; for name in "${GITATTRIBUTES[@]}"; do n=$((n + 1))
      keep "name-gitattributes-$(printf %02d $n)" tree "$(entry 120000 "$name" "$link" | lit tree)"; done
    n=0; for name in "${GITIGNORE[@]}"; do n=$((n + 1))
      keep "name-gitignore-$(printf %02d $n)" tree "$(entry 120000 "$name" "$link" | lit tree)"; done
    n=0; for name in "${MAILMAP[@]}"; do n=$((n + 1))
      keep "name-mailmap-$(printf %02d $n)" tree "$(entry 120000 "$name" "$link" | lit tree)"; done
    n=0; for name in "${TAGS[@]}"; do n=$((n + 1))
      keep "name-tag-$(printf %02d $n)" tag "$(printf 'object %s\ntype commit\ntag %s\n%s\n\nm\n' "$head" "$(printf '%b' "$name")" "$tg" | lit tag)"; done
  fi
  cd "$OUT"
done
( cd "$OUT" && shasum -a 256 git-version.txt manifest.txt sha1/* sha256/* > SHA256SUMS )
# The tests' table of the objects, so each is compiled in and none is read at run time.
{ echo '// Generated by fixtures/fsck/make-fsck-fixtures.sh: each object of manifest.txt, compiled in (A-590).'
  echo 'pub(crate) const FILES: &[(&str, &[u8])] = &['
  while read -r format name _; do
    printf '  ("%s/%s", include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/fsck/%s/%s"))),\n' \
      "$format" "$name" "$format" "$name"
  done < "$OUT/manifest.txt"
  echo '];'; } > "$OUT/table.rs"
