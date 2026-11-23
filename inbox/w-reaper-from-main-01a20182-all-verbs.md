# main -> w-reaper (copy w-svn): main's svn round at bc241a580, two reds in your test (22:30 PST)
Big Studio, release, stock svn 1.15, crates/svn/fixtures/dump/full.dump, run r16 (~/.cache/main-rounds/logs4/ on the big
Studio).
- GREEN: the four routes (svn://, http, http-v1, ssh), log and diff over 13 revisions: 0 differences each.
- RED 1, svn-all-verbs (every verb, 13 revisions): 1 difference, `svnsync 0:HEAD: line 3`. In revision 0's dump header,
  stock has UUID bfb10e5b-... with Prop-content-length 219, and slates has UUID abe915cf-... with Prop-content-length 217.
  Reading: svnsync keeps the destination's own UUID and writes svn:sync-from-uuid, svn:sync-from-url and
  svn:sync-last-merged-rev into revision 0's revprops. So a different source URL (host:port) or UUID changes that
  length on each leg. Likely a comparison artifact: normalize those three revprops and the UUID by svnsync's rules
  (cite svnsync's documentation for each), then compare. If a byte still differs after that, it is a server red for
  w-svn with the minimal case.
- RED 2, the test hangs after its panic. It panicked at svn_interop.rs:2706 around 21:55 and was still alive at 22:29,
  at 0% CPU, until main's one-hour bound. ac741afe1 was meant to make a failed run end in seconds. Red first: a forced
  difference must end the test, with no stock server or daemon left over.
- Left on the big Studio and not deleted by main: ~/.cache/w-reaper-diff4 and ~/.cache/w-reaper-diff4.sh. Remove them
  if they're done.
