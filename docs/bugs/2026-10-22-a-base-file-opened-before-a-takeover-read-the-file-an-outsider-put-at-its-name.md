# A base file opened before a takeover read the file an outsider put at its name

**Status:** fixed 2026-10-22 (A-151, approved by Ada in plan mode). The root cause was confirmed by temporary logging on
the box (Linux 7.2.9).

**Severity:** a process reads bytes from a file it never opened. After a FUSE daemon takeover (A-61), a process
reading an untouched base file it opened before the takeover reads another file's bytes once an outsider replaces the
file's name on the disk, as an editor's save or a `git checkout` does. A read that spans the replacement is torn
across two files. Through a write it is worse, though not reproduced: the write's copy-up would take the outsider's
file as the base the write was made against, and the edit would land on a file the writer never saw. Goal item 3 of
2026-10-22 (GAPS, A-147's siblings).

## Reproduction

`crates/cli/tests/cli.rs` `a_base_file_opened_before_a_takeover_never_reads_the_file_an_outsider_put_at_its_name`, on
the box:
1. a volume over a base directory holding `notes.txt`, mounted through FUSE;
2. `notes.txt` opened and read;
3. a file created through the mount, whose barrier publishes the open file's inode, so the successor knows its number
   (A-150) and what is tested is the open file, not number reuse;
4. `SIGKILL` of the daemon; the anchor's successor takes the mount over;
5. on the disk, another file written beside it and renamed over `notes.txt`;
6. `notes.txt` looked up through the mount, the open file's cached pages dropped, and a read through the descriptor
   opened in step 2.

It failed: "the descriptor opened before the takeover read the file an outsider put at its name". The read returned
the outsider's bytes.

The control, `a_base_file_kept_open_reads_the_file_it_opened_after_an_outsider_replaces_its_name`, runs the same steps
without the kill and passes: the descriptor reads the bytes it opened. So the takeover is the variable.

## Evidence

Temporary `BASEDBG` lines in `prune_unloaded` and `descriptor` (`crates/vfs/src/base.rs`), the box at load average
55–85:

```
BASEDBG descriptor no=281474976710658 opened by its home name=notes.txt: disk ino Ok(50547211)
slates-server: shard 0: recovered 1 volumes from their images (0 refused), ... replayed 1 logged writes
BASEDBG prune no=281474976710658 name=notes.txt open=false held=false listed_ino=50547213 -> refreshed in place
BASEDBG descriptor no=281474976710658 opened by its home name=notes.txt: disk ino Ok(50547213)
```

The dead daemon held inode 281474976710658 on disk inode 50547211. The successor counted it neither open nor holding a
descriptor, so its relist refreshed the inode in place onto the outsider's disk inode 50547213. It then reopened the
name and read that file. In the control run the same relist saw `open=true held=true`, and the inode left the name.

## Root cause

A relist (`prune_unloaded`) keeps an untouched base inode on the file it was made from only when the inode is open
(`Volume::is_open`) and the descriptor it holds names another file than the listing (`names_another_file`). Otherwise
the inode is refreshed in place: it takes the listed file's attributes, and its next read opens the name again
(`descriptor`, with no check that the name still names the inode's file).

The decision rests on state that does not survive the daemon, and the inode records nothing that does:
- the open counts (`Volume::opens`) are never published;
- the descriptors (`BasePlane::descriptors`) die with the process;
- an untouched base inode records no identity for the disk file it was made from (`BaseBody`).

So the successor cannot tell the file an old handle opened from the file now at its name.

## Siblings (found reading the same code; each gets a test that is red first in the fix)

- **NFS keeps no open state** (`crates/bridge-nfs` never calls `Bridge::open`). For an NFS client, every relist after an
  outsider's replacement refreshes the inode in place, with no takeover at all. A handle the client holds then reads
  the new file. A real NFS server answers that handle `ESTALE`, since the file it named is gone.
- **An open racing a replacement binds the new file.** `open_base` opens the name by its home without relisting, and
  nothing compares the opened file with the one the inode was made from. A replacement between the lookup and the open
  gives the inode the listed file's attributes and the new file's descriptor.
- **A device and inode number pair is not an identity once the descriptor is gone.** ext4 and XFS hand a freed inode
  number to the next file. Two editor saves in a row leave the name at the first save's number with other bytes. So
  any fix that compares `(dev, ino)` without a held descriptor needs the file's birth time too.
- **`VfsError::StaleHandle` reaches the FUSE kernel as `EIO`** (`crates/bridge-fuse/src/bridge.rs` `errno`). A-61's
  text says such a node is answered `ESTALE`. `EIO` also denies Linux the retry it makes on `ESTALE` (a path walk is
  repeated with revalidation).
- **A node id the successor does not know answers `ENOENT`** (A-150's refusal). `ESTALE` is the honest errno for an
  operation on a node id. Recorded, not fixed here.

## The fix (A-151)

An untouched base inode is bound to the disk file it was made from. When the disk gives the name to another file, the
name becomes a new entry: the old inode leaves it, open or not, held by its references, and serves the file it was made
from only through a descriptor it holds. Without one it is refused `StaleHandle` (`ESTALE`). A replaced file gets a new
number, as on a real filesystem.
- **The identity** is the device, the inode number and the birth time, digested to 56 bits (`FileTag`) in seven bytes
  the base body had as padding: `Inode` stays 168 bytes. A held descriptor decides by device and number, since a number
  it holds cannot be reused.
- **A descriptor opened by name** must find the inode's file, else it is closed and refused. That closes the
  open-versus-replace race.
- **A detached inode no handle holds open** has its descriptor closed at once, so the descriptor budget is what it was.
- **On a filesystem that records no birth time**, a recovered volume's first listing of each directory renumbers its
  untouched entries, and until then none is opened by name (`Listing::restored`).
- **The FUSE edge answers `StaleHandle` as `ESTALE`** (it had fallen to `EIO`).

Why not durable open counts (the earlier plan):
- an `OPEN` is answered before any publication, so the count misses opens in that window;
- once the descriptor is gone, a count still cannot tell that the name was replaced;
- NFS keeps no opens.

## Exact edits

- `crates/vfs/src/inode.rs`: `Fingerprint::birth_ns`; `BaseBody::file`, `FileTag` (the splitmix64 finalizer, cited).
- `crates/vfs/src/base.rs`:
  - `materialize` records the tag;
  - `prune_unloaded` detaches on another file, open or not, and on an unprovable restored entry;
  - `names_another_file` decides by the held descriptor, else the tag;
  - new `unprovable`, `release_detached`, `made_from` and `in_restored_listing`;
  - `refresh_unloaded` loses its descriptor-closing branch;
  - `descriptor` refuses a detached inode, a mismatched file and an unproven restored one;
  - `Listing::restored`.
- `crates/vfs/src/base_recovery.rs`: a recovered listing is `restored`.
- `crates/vfs/src/recover.rs`: `BodyImage::Base::file`; `IMAGE_VERSION` 20.
- `crates/vfs/src/host/sim.rs`:
  - birth times (never zero);
  - `without_birth_times`;
  - `replace_file_with_number` (ext4's reuse; refused while a descriptor holds the number);
  - `close_files` (a process death).
- `crates/base/src/unix.rs`: `statx` with `STATX_BTIME` for every Linux fingerprint (paired `fingerprint_at` and
  `fingerprint_of`); `st_birthtime` on macOS.
- `crates/base/src/windows.rs`: the creation time.
- `crates/bridge-fuse/src/bridge.rs`: `ESTALE`.
- `crates/land/src/verdict.rs`: its fixture's birth time.
- Tests:
  - `crates/vfs/tests/base_identity.rs` (six, new);
  - `crates/server/tests/nfs_mount.rs` (the NFS sibling);
  - `crates/cli/tests/cli.rs` (the takeover test demands `ESTALE`; the control);
  - `crates/bridge-core/tests/invalidation.rs` (a replaced file is a new inode).
- Docs: `SLATES_DESIGN.md` (§4.5, A-61, A-151), GAPS, CLAUDE.md (the simulated host's gotcha).

## Verification

| | before | after |
|---|---|---|
| the CLI takeover test (the box) | read the outsider's bytes | refused `ESTALE`; the control still reads the opened bytes |
| `a_closed_base_file_an_outsider_replaced_is_a_new_inode_and_its_old_number_refuses` | the same number | green |
| `a_handle_opened_before_a_takeover_is_refused_once_an_outsider_replaces_its_file` | the same number | green |
| `a_number_the_filesystem_reused_never_passes_for_the_file_a_recovered_handle_opened` | read the second save | green; red again with the birth time left out of the tag |
| `an_open_racing_a_replacement_is_refused_never_bound_to_the_new_file` | `Ok(())` | green |
| `an_nfs_handle_of_a_base_file_an_outsider_replaced_never_reads_the_new_file` (macOS) | read the outsider's bytes | refused |
| `a_handle_opened_before_a_takeover_reads_its_file_while_the_name_still_names_it` (the image round trip) | green | green |
| `tree_heap` | `Inode` 168 B; npm 300, pip 336, cargo 386 B an entry | the same |

- **macOS:**
  - the vfs, bridge, land and base suites: 935 of 935;
  - the server's `nfs_mount` (40), `recovery` (39), `nfs_hostile` (17), `virtiofs` (8) and `attach_forms` (9);
  - workspace clippy `-D warnings`, the doc gate, `cargo xtask check` and fmt.
- **The box (Linux 7.2.9, load average 30 to 44):**
  - workspace clippy `-D warnings`;
  - the vfs, bridge, land and base suites: 966 of 966;
  - the CLI suite: 54 of 54;
  - the server's `fuse_mount` (3), `fuse_idle` (1), `nfs_mount` (40), `recovery` (41) and `virtiofs` (18);
  - `coherence_mount` (7) and the guest tests (3).
- **Windows:** the base crate's Windows code cannot be cross-linted from the Mac (`zstd-sys` needs a C toolchain), so
  the CI lane judges it.
- **One change of rule a test had pinned:** `crates/bridge-core/tests/invalidation.rs` expected a replaced file to keep
  its number ("D-4, refreshed"). It now expects a new inode, and the old number gone.
