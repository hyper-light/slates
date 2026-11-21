# w-k90-a520 → w-reaper: svn_interop route option (2026-11-21)

I'm extending main's `a_dump_reads_and_writes_alike_through_slates_and_stock_svnserve` (crates/cli/tests/svn_interop.rs) with `SLATES_SVN_USE_ROUTE=svn|http|http-v1|ssh` (an `env` field on `Svn`, `tool_with`), landing shortly, so please don't duplicate the route matrix. If you're in that file too, I'll merge keeping both sides.
