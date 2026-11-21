- 2026-11-20 w-fed-faults, A-795 (the S3 base removed whole): READY, NOT LANDED. Branch agents/w-fed-faults-no-s3 at
  5747905 is one commit over main 68ce3d585 (58 files, +223/-10,875). Your step 4 asks for a Linux build of the server
  first (fuse.rs); adacachy has refused every start and sync since about 22:00, so it hasn't run. I land with land-once.sh
  the moment it builds. Reading the Linux-only diff turned up one break, now fixed: fuse.rs's A-509 test built FuseMount
  with the removed kept fields. macOS (Mac Studio, over 17bb639aa and recompiled over 68ce3d585): the touched crates' lib
  tests and the ipc and db wire_order goldens all passed. Server --lib: 1,038 passed, 11 failed; the 10 in repository,
  vcs_store paths and commits fail the same way on origin/main without this change (a submit refused Overloaded), and
  content_ranges' sweep test passes on its own. Two reds on main that aren't mine: (1) those 10 server tests; (2) A-796
  appended CheckMerge, ReadMergeCheck, ResolveMerge, MergeChecked and StaleMergeView, plus the MergeView enum, without
  publishing them, so slates-ipc --test wire_order fails on 68ce3d585. Kept pieces with users outside S3: BaseHost::Tree
  (A-774); the vendored aws-lc-rs and its MD5 (server config, svn commit drive); hg_base's refusal and short-read
  tests, now over EIO. Also marked A-524 removed (S3 part 3), with A-464, A-471, A-555 and A-791.
