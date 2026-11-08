# A FORGET after the last publication leaked its lookup reference

**Found** 2026-10-23 while fixing A-154 (GAPS, A-154's "Open, a sibling"). Fixed 2026-11-08 (slabs).

**Description.** A FUSE mount's kernel holds lookup references on the inodes it has looked up, and gives them back
with `FORGET` (or `BATCH_FORGET`) when it lets an inode go. A `FORGET` takes no reply, so nothing waits for a
publication behind it, and the kernel never resends one. A daemon that served a `FORGET` after its last publication
and then died left its successor the references that publication recorded: the successor counted lookups the kernel
no longer held, and an unlinked file the kernel had let go stayed referenced, its bytes counted, until the mount ended.

**Root cause.** The write log (A-63, `crates/server/src/write_log.rs`) carried only writes, and the image only what
its publication captured; nothing between the two recorded a `FORGET`.

**Impact.** A bounded leak per takeover (A-61's stated limits): each `FORGET` served between a daemon's last
publication and its death kept its references, and an unlinked file among them kept its RAM, until the mount ended.

**The edits.**
- `crates/server/src/write_log.rs`: every record now carries its kind; a `FORGET` record holds the attachment, the
  inode and the count (`Logged::Forget`, `Forgotten`, `append_forget`); a record of another kind, or a `FORGET` of the
  wrong length, reads as everything lost rather than short.
- `crates/bridge-fuse/src/channel.rs`: a serve state whose owner logs `FORGET`s (`logs_forgets`) collects each one's
  node and count as it is delivered, taken every turn (`take_forgotten`), so it holds one request's at most.
- `crates/server/src/fuse.rs`: every FUSE mount logs its turn's `FORGET`s under its attachment (`log_forgets`); a log
  with no room forces a publication; one still refused is counted (`write_log.forget_unlogged`), never marked as an
  overflow, which would report every taken-over file as having lost writes.
- `crates/server/src/verbs.rs`: the replay gives each logged `FORGET` back in its place among the writes, as its
  attachment's own (`replay_forget`, `Volume::forget_for`); a refused one is counted
  (`recovery.forget_replay_refused`); `relog_remaining` keeps both kinds.
- Tests: `a_forget_is_replayed_in_its_place_among_the_writes` (`crates/server/src/write_log.rs`) and
  `the_lookups_a_dead_daemon_was_given_back_after_its_last_publication_stay_given_back` (`crates/cli/tests/cli.rs`,
  Linux FUSE, a `SIGKILL` under a kept mount).

**Format.** The log's record layout changed. A daemon reads only the log its own image wrote, since the anchor starts
and restarts its own executable (A-145), so no older layout is read.
