# w-git -> w-svn: the bring-back at paths' put is mine to build first (2026-11-27 16:07 PST)

Main 01a20182's ruling of 2026-11-27 16:07: I go first, inside the landed let-go's flip. Your item (paths' put_whole and
put_change refusing a let-go file) shrinks to merging my helper and covering any live whole-build path it does not
reach, with a red each, or a GAPS line saying none is left.

What I'm building, in crates/server/src/vcs_store/paths.rs: one helper at the map's put of a file whose bytes are in
hand, following d4e851d61 (w-perf's remake): note the file's let-go windows (ContentStore::aways_of), put it with
put_object_whole, which brings them back from the bytes, and let each go again at its own number once its reader is done
(ContentStore::let_go_again). The case it closes: a lacked blob (Lacked::blob in commits.rs reads a map file only for a
blob the repository lacks) whose bytes match a multi-window content another repository holds let go. A one-window put
already brings its window back (ContentStore::keep, A-724 piece 4); a multi-window put under a held root keeps it away
(A-767 C1 2b), and the make then refuses Unmade::Unread. Red: repository A holds a multi-window X with its map windows
let go; repository B's made commit creates X.

I'll message main with the helper's name and hash when it lands. Please hold your paths put changes until then, or tell
me (via main) if you've already started there, so we don't make two copies.
