# A Windows checkout rewrote the bytes a doc-truth test compares

**Status:** fixed (`.gitattributes`, this change).
**Found:** 2026-10-20, by the first push of the session's 250 commits to `main`
(run 39527867650, job 118986658787, `aarch64 windows (native, by test)`).
**Platforms:** every Windows lane. macOS and Linux were never affected.

## What happened

The arm64 Windows lane went red on one test:

```
test observe::tests::the_recorded_chokepoint_table_is_the_registry ... FAILED
thread '...' panicked at crates\wire\src\observe.rs:841:8:
the record has the chokepoints:begin marker
test result: FAILED. 31 passed; 1 failed; 1 ignored
```

The message is the `expect` on a `find`, so it reads as "the marker is missing
from `docs/wip/observability.md`". The marker is not missing. It is on line 12
of that file, and has been since the record was written.

## Root cause

`crates/wire/src/observe.rs:750` searches for the marker *and the newline after
it*:

```rust
const TABLE_BEGIN: &str = "<!-- chokepoints:begin -->\n";
```

The repository had no `.gitattributes`. GitHub's Windows runner images ship git
with `core.autocrlf=true`, so a Windows checkout rewrites every LF in a text
file to CRLF. The line in the checked-out file therefore ends `-->\r\n`, and a
search for `-->\n` cannot match it.

Confirmed by construction rather than by inspection — the committed file and the
same file with CRLF endings, searched for the same string:

```
the file as committed (LF):
  contains the marker string: True
the same file as a Windows checkout would write it (CRLF):
  contains the marker string: False
  contains '<!-- chokepoints:begin -->' at all: True
```

The last line is the point: the marker is present, and the search string is what
cannot match it.

The byte-for-byte comparison behind the marker search would have failed next for
the same reason. The test's whole premise — that the block between the markers
equals `Chokepoint::registry_table()` byte for byte — is only meaningful if the
file on disk is the file that was committed.

## Impact

No product defect. slates writes nothing here; this is a test reading a document
from the source tree. The impact is a red Windows lane and a misleading failure
message that accuses the document of missing a marker it contains.

Eight sibling sites read a document the same way and were latent, not red:

| Site | Why it was not red |
|---|---|
| `crates/ipc/src/protocol.rs:3139`, `:3168` | identical marker pattern over the same record; the arm64 lane runs `cargo test -p slates-ipc --test rings`/`rendezvous`/`delivery`, never `--lib`, so the lib's doc-truth tests never ran on Windows |
| `crates/conformance/tests/matrix.rs:69`, `:157` | parses `docs/wip/conformance.md`; the conformance lane is Linux and macOS only |
| `xtask/src/main.rs:1037` | reads `SLATES_DESIGN.md`; `cargo xtask` gates run on the `gates` matrix, which is ubuntu and macos |
| `xtask/src/capabilities.rs:368`, `:435` | reads `README.md`; same lanes |

So the count is one failing site and eight that would fail the moment a Windows
lane ran them.

## The fix

Declare the tree's own text as LF in `.gitattributes`, so every platform checks
out the same bytes. This is the rule the determinism gates already hold the build
to, applied to the checkout: a test that compares a file's bytes needs those
bytes to be the committed ones.

The alternative — teaching each reader to strip carriage returns — was rejected.
It is nine edits instead of one, it leaves the tenth reader to rediscover this,
and it makes each test quietly tolerant of a checkout that is not the commit,
which is the opposite of what a doc-truth test is for.

`vendor/**` carries `!text !eol`, which *unspecifies* both attributes rather than
setting them, restoring exactly the state vendor was in before this file existed:
no opinion from slates, upstream's own nested `.gitattributes` the only one that
speaks.

**The first attempt at that line was `-text`, and it was wrong.** With it in
place, a `git add --renormalize .` staged 24 files under
`vendor/aws-lc-rs/third_party/NIST/SHAVS/` — 263 insertions and 263 deletions
each, which is every line, which is the line endings. Upstream had anticipated
exactly this:

```
# vendor/aws-lc-rs/third_party/NIST/.gitattributes
# Preserve the line endings on the NIST test vectors so that we can use
# sha256sum and friends to verify that they haven't been tampered with.
*.rsp eol=crlf
```

So the first fix for a line-ending bug would have changed the line endings of
NIST's crypto test vectors, and with them every checksum a tampering check reads.
`git check-attr` reported `text: unset` on those files at the time, so the
declaration looked correct and was not; only staging the change and reading what
it staged showed otherwise. Under `!text !eol`, `git add --renormalize .` stages
zero files.

Nothing in the index needed renormalizing: every tracked file outside `vendor/`
is already LF, apart from seven genuine binaries (five AppleDouble `.bin`
vectors and two brand `.png` files), which `text=auto` leaves alone and which are
declared binary explicitly besides.

## Exact edits

- `.gitattributes` — new file.
- `docs/wip/GAPS.md` — the entry.

## Verification

- `git add --renormalize .` stages zero files, so the declaration rewrites
  nothing that is already committed — in particular nothing under `vendor/`.
- `git check-attr text eol -- docs/wip/observability.md` reports `text: auto`,
  `eol: lf`; the same query over a NIST vector reports only upstream's
  `eol: crlf`, and over `vendor/aws-lc-sys/builder/prebuilt-nasm.bat` reports
  nothing at all, which is its pre-change state.
- **Proven.** Run 39528936826, the first push carrying `.gitattributes`:
  `aarch64 windows (native, by test)` completed **success**, where run
  39527867650 on the parent commit failed on
  `the_recorded_chokepoint_table_is_the_registry`. The other three Windows
  lanes (`native clippy + tests`, `windows`, both cross-lint squares) are green
  on the same run. The bug could not be reproduced on this machine — the
  Windows relay here dies in a WSL shim — so CI is the arbiter, and it ruled.
