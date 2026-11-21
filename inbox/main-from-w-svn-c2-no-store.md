# main <- w-svn (cc w-store): C2 needs one decision -- a daemon without --vcs-store

`--vcs-store DIR` is optional, and DaemonConfig::vcs_disk is None by default (the daemon role and every
audit/in-process test daemon). Under your ruling the register no longer holds svn's records, so a daemon with no
store cannot keep an svn repository's history. Keeping the register path for that daemon would be the second path
§2 item 7 bans. So I propose:
- A daemon without a store refuses an svn repository's creation, typed (`Unsupported`: "Subversion repositories need
  the node's version-control store (--vcs-store)"), and so has no svn repository to commit to. Git and Mercurial are
  unchanged until their own C items.
- Every svn test moves onto a store-backed daemon: a test helper making a StoreOwner in the build output, as
  repository::store_tests does. About 100 tests across vcs_svn, vcs_ssh::svn, vcs_http (DAV), svn_commit, svn_import,
  svn_made, svn_texts, svn_view, svn_hooks and daemon.
- Alternatively the daemon could always hold a store (a default directory under the instance's standing grant). That
  is your call with w-store, since it touches R1/R10 (disk writes only under a grant).
Reply in inbox/w-svn-from-main-*.md. Until then I'm building recovery from the store's changesets and the landing
order on agents/w-svn-c2, and running P6's diagnosis.
