# The NFS LINK reply carried the directory's attributes from before the link

Date: 2026-09-15
Area: `crates/bridge-nfs/src/procedures.rs` (`Export::do_link`, the `linkdir_wcc` of
`NFSPROC3_LINK`)
Severity: a coherence bug on the macOS mount: after `link(2)`, `stat` of the directory reported
the `ctime`/`mtime` of before the link for a whole attribute-cache period (`actimeo=1`), so a
program watching the directory's times missed the change.

## Symptom

pjdfstest `link/00.t` ("successful link(2) updates ctime": the file's `ctime`, the directory's
`ctime`, the directory's `mtime`) failed the two directory checks (cases 135, 136 for a regular
file; 177 for a symlink) while the file's own check passed. Through a live mount, 2026-09-15:

```
$ pjdfstest stat . ctime; pjdfstest stat . mtime; sleep 1; pjdfstest link f g
1789518864 1789518864
$ pjdfstest stat . ctime; pjdfstest stat . mtime
1789518864 1789518864          # unchanged
$ sleep 4; pjdfstest stat . ctime; pjdfstest stat . mtime
1789518865 1789518865          # the server had moved them; the client's cache had not
```

`create` in the same directory moved `mtime` at once (its `dir_wcc` is read after the create).

## Root cause

`do_link` took the directory's attributes from the permission check (`writable_directory`)
*before* calling the bridge, and put those in the reply's `linkdir_wcc` post-operation
attributes. RFC 1813 §3.3.15: `linkdir_wcc` is "weak cache consistency data for the directory",
its `after` the attributes following the operation; the macOS client stores them as the
directory's current attributes with a fresh cache stamp, so a stale set is served for another
`actimeo` — exactly the window pjdfstest's `sleep 1` then `stat` falls in. REMOVE, RENAME,
CREATE, MKDIR and SYMLINK read their directory attributes after the mutation; LINK alone did not.

## Fix

`do_link` runs the permission check, performs the link, then reads the directory's attributes for
the wcc — on success and on failure alike, as the other procedures do.

## Verification

- `crates/bridge-nfs/tests/procedures.rs::a_link_over_the_export_makes_a_second_name` now decodes
  the reply's `linkdir_wcc` and asserts its `ctime`/`mtime` equal a GETATTR of the directory
  after the link, and that they differ from a GETATTR before it (non-vacuity; the test's clock is
  the host's, with a 2 ms sleep before the link). Failed before the fix by the link's own
  duration (post-op `nseconds` 400824000, GETATTR 403468000); passes with it.
- pjdfstest through a live mount after the fix: the record in the same change.

## Sibling sweep

Every other directory-mutating procedure (`remove_result`, `do_rename`, `do_create` through
`finish_create`, `do_mkdir`, `do_symlink`, `do_mknod`) reads the directory's post-op attributes
after the bridge call; SETATTR reads the object's after the change. No other reply carries
pre-operation attributes in a post-operation slot.
