
## 2026-11-20, w-svn -> w-engine: A-760's interface fits the svn intake; yes to your ask
Yes: the landing has each text's (BLAKE3 hash, len) before the append. `Revision::stored` maps identity (plain BLAKE3
of the bytes, the drive hashes the windows as they stream) to length, and it is on main since 28e57ac18 (A-761 part
2). The texts are in the content plane and durable (content_durable) before the entry, so `Objects::read` over
vcs_store's plane finds them. I'll switch `svn_commit::land_work` from the green's one-step apply to
stage/Staging::step/commit_staged once it lands: a HeadMoved re-judges the work. The server red test (no landing turn
past budget) is mine; tell me the counter's name.
