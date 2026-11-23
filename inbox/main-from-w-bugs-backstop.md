# main, from w-bugs: the descriptor backstop, platform half (A-837)

## Parts 1 and 2 landed: 417c3394e and 59dfc2d77 (main 9fefaa054), 2026-11-21
- **Part 1, the raise.** `slates_machine::facts::raised_descriptor_soft` (cfg-free: min(hard, the platform's cap), only
  raising) and `raise_descriptor_limit` (Linux capped at fs.nr_open, macOS at kern.maxfilesperproc, Windows none).
  It replaces the server's own `limits` raise, which capped macOS at OPEN_MAX (10,240) and on Linux under an
  unlimited hard limit asked u64::MAX, refused past fs.nr_open, so it raised nothing and said nothing. The daemon's
  start and the anchor log `descriptor_limit = DescriptorRaise { before, hard, cap, after, refused }` as a boot-profile
  line. Tests: the decision on every host; a re-invoked child lowered to 64 and raised (macOS: accepted past OPEN_MAX).
- **Part 2, the learned ceiling.** `slates_machine::descriptors`: the count held at the first EMFILE/ENFILE (Windows
  ERROR_TOO_MANY_OPEN_FILES, ERROR_NO_SYSTEM_RESOURCES) met by a host open, counted without a descriptor (macOS
  proc_pidinfo, Windows GetProcessHandleCount, Linux through a spare held from the start); `ceiling()` = the lower of it
  and the rlimit. unsafe-budget slates-machine 101 -> 104, the three sites named. Tests: 2 (lower bound, Windows codes;
  the first exhaustion counted and kept).
- **Linux and Windows rounds:** in run-requests.md.

## Part 3, in two pieces
- 3a (base opens a typed `DescriptorsExhausted` refusal, counted, the shard serving; the two arms you allowed in
  vfs/base.rs): written, on its focused run with the daemon-level red (kiln r4).
- 3b (accept, memfd, eventfd typed the same way: new RtError and MemError kinds): after 3a.

## 2026-11-21 21:20: part 3a waiting on a machine (no answer needed)
- 3a (typed `DescriptorsExhausted` for base opens, counted; your two base.rs arms) is written and compiles past every
  exhaustive match (land's engine and `target_of_host` found by the runs). Branch agents/w-bugs-backstop-6, c907d2174.
  Its one focused run (the daemon-level test + red check, kiln r8) has sat queued since ~15:30: the big Studio's link is
  down. It lands when that run is green; 3b (accept, memfd, eventfd) follows.
