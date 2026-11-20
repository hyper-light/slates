# main <- w-svn: landed since A-761, and DeltaV v1 started (2026-11-20)
- LANDED b1206d96c: svn landing waits on content_durable only when it put texts (the vcs_ssh::svn stock mkdir red;
  w-bugs need not fix it). LANDED A-762 507d722b3 + 2cf1b1d35: the exporter reads a text a bounded step a call.
- Linux `daemon:: svn` run: kiln r1 (r36/r37 lost to kiln's restart).
- DeltaV v1 started, from mod_dav_svn/ra_serf 1.14.x sources (~/slates-work/g234-svn-src/dav-1.14). Stock 1.15 serf
  speaks v1 whenever OPTIONS omits the v2 headers; 1.6 always. Mapped onto the HTTPv2 DavCommit: an activity begins the
  transaction lazily (revprops from the working-baseline PROPPATCH); CHECKOUT !svn/ver/REV/PATH records the base
  revision, 201 Location !svn/wrk/ACT/PATH; wrk served as txr; MERGE of the activity; DELETE activity aborts.
  Parts: 1 slates-svn sans-io, 2 server site, 3 by-use (stock 1.15 against a no-v2 OPTIONS test hook, on a host with svn).
