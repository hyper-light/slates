# main, from w-bugs, 2026-11-20: A-770 (git deepen) landed; svn diag run 13

## A-770: git deepen landed 02a478233 (merged 618220deb)
- Reproduced on w-interop's stock-git v2.0.0 rows (release, local). Two refusals in turn:
  1. "Needs more room": the shallow depth walk kept an entry for each commit. It now walks level by level on
     `Held` sets. A depth-100 deepen of a 4,000-commit merge-heavy history took 65,445 bytes before and 23,373 now.
     Red at a 32 KiB room.
  2. "Bad pack header": the pack plan charged 16 bytes a run of the negotiation's bit table, asking 525,472 bytes of
     278,528 (traced). w-git's 7ffddbdf0 (three forms) landed while I worked. On top of it, `Planned::of` now charges
     the set's `bytes()`. Red: a 32,000-run plan asked 512,000 bytes in 64 KiB; it is now admitted within 12 KiB.
- Matrix at cbf7c84c3:

  | row | result |
  |---|---|
  | push-all | 97 s |
  | shallow-clone | 3 s |
  | deepen | 217 s (was exit 128) |
  | partial-clone | 50 s |
  | partial-blame | pass |

- Tests: slates-git 577 passed; slates-server vcs_http::git, daemon_edge, vcs_ssh::git and vcs_store::upload
  passed. On base 0916cf5b4, fmt is clean and the git upload tests pass.
- Partial clone: r2's "bad pack header" was the lazy fetch's room (4136fcd89, edbc3838c). It passes here.
- Owed to the stream's owner (w-git): `read_objects` calls `run_pairs()` on each window, so a bit table's runs are
  all made again every turn. deepen's 217 s is slow beside stock git; I've asked w-interop for their time.
- w-interop and w-k90-vendor have been told.

## svn diag run 13 (diag-w-bugs-svnfetch-13, base 2d3f218cb, after 2502607c4)
Working copies are identical to svnadmin's on both edges.

| | run 8 | runs 10/11 | run 13 |
|---|---|---|---|
| svn:// checkout | 7.1 s | 25.0 s | 8.0 s |
| svn:// update | 2.7 s | 32.5 s | 5.6 s |
| DAV checkout | 7.6 s | 23.2 s | 7.5 s |
| DAV update | 3.6 s | 24.6 s | 4.6 s |

Longest step per kind in run 13:

| step kind | longest |
|---|---|
| file:window-unit | 3.19 ms (svn://), 18.8 ms (DAV) |
| dav:resume | 18.8 ms |
| svn:answer | 4.39 ms |
| file:text | 3.7 ms (DAV) |
| dav:write-call | 3.0 ms |
| file:properties | 1.6 ms |
| dir:targets | 0.86 ms |
| dir:listing | 0.74 ms |
| dir:properties | 0.14 ms (run 9: 19.8 ms) |

The 2502607c4 fix holds: runs 10/11's 25–32 s are back near run 8. Updates remain 1–3 s over run 8, and DAV's
window units reach 18.8 ms, the largest step left.
