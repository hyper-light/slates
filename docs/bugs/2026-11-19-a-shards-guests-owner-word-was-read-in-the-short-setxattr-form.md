# A Shards guest's owner word was read in the short SETXATTR form (2026-11-19, A-587)

## Description

kiln's macOS microVM drive (`crates/kiln/tests/slates_e2e.rs`'s `microvm::` drive; main's run r134 on
adas-macbook-pro.local, kiln 815a790c) failed its first agent: the stand-in harness in its Shards guest heard its task and
said `WRITE_FAIL [Errno 13]` where it writes its first file into its project, a slates volume the daemon serves its
guest over a Shards stream. The guest's own kernel refused the create `EACCES`.

## Root cause

A Shards guest's kernel checks every access itself: a guest's route asks it for `FUSE_POSIX_ACL` (A-314), which turns
`default_permissions` on. So a run of an unprivileged user writes its share only once Shards' init has named the run's
owner on the share's root with Shards' owner word (`trusted.shards.owner`, `UID:GID`), which the stream takes before its
bridge sees the request and from then on shows the root's owner as the run's (A-281).

A-378 made a guest's route negotiate `FUSE_SETXATTR_EXT`, after which the kernel sends every SETXATTR with the extended
`fuse_setxattr_in`: 16 bytes before the name where the short form has 8. Each transport tells its bridge the form, and
the dispatch reads it (`crates/bridge-fuse/src/bridge.rs`, `serve_setxattr`), but `crates/bridge-fuse/src/shards_fs.rs`'s
`owner_word` read a fixed 8-byte head (`const HEAD: usize = 2 * size_of::<u32>()`), and the stream called it with no form
(`owner_word(&request)`, `crates/server/src/shards.rs`). Shards' init sets the word after its kernel's INIT kept the
extended form, so the name was read from the setter's flags and the pad, all zero: the word never matched and went to
the dispatch as an ordinary attribute. The owner view stayed the identity, the guest saw its project owned by the host's
user (501 on a Mac), and its kernel refused the run's create.

## Impact

Every Shards guest whose run is a user other than the host's owner of the volume's root, on every host, since A-378
(slates e418f5cd2; kiln took it at 688cb98ce, 2026-11-18): its writes into its share refused `EACCES`. Hidden wherever the
run's uid equals the host's owner's. kiln's microVM engine (UI goal 88, W8) could not write an agent's volume on macOS.

## Edits

- `crates/bridge-fuse/src/shards_fs.rs`: `owner_word(request, extended)` reads `fuse_setxattr_in` by the stream's form,
  `SETXATTR_IN_BYTES` where `extended`, else `COMPAT_SETXATTR_IN_BYTES`; the unit tests
  `an_owner_word_in_the_extended_form_is_taken_where_the_stream_kept_that_form` and
  `an_owner_word_in_the_short_form_is_taken_where_the_stream_kept_no_extension`.
- `crates/server/src/shards.rs`: the serve loop passes `stream.setxattr_ext`, the form `kept_at_init` read from the
  guest's INIT reply; the unit test `an_owner_word_after_an_init_that_kept_the_extended_form_shows_the_root_as_the_runs`.
- `crates/bridge-fuse/tests/shards_fs.rs`: its short-form tests read through `owner_word_short`.
- Swept (none found): the stream's other intercepts read no negotiated form (`replayed` the in-header's unique;
  `dax::hold_for_ingest` and `release_forgotten` the in-header and the FORGET bodies; the INIT's kept flags through
  `init::kept_flags`, which reads `flags2` under `FUSE_INIT_EXT`); the virtio-fs device tells its bridge the form before
  every chain and intercepts no SETXATTR; no transport negotiates a request extension (`FUSE_SECURITY_CTX`,
  `FUSE_CREATE_SUPP_GROUP`), so SETXATTR's is the only request head a connection settles.
