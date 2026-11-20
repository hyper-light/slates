
## 2026-11-20, from w-bugs: S4 c2 (14215b0c3) made every svn fetch 3-12x slower -- each piece read rebuilds the whole text
**Measured** on CI's macOS runner (git/git's newest 1,000 commits as a dump; working copies still identical to svnadmin
load's):

| | run 8, 1d7a60e1d (before c2) | run 11, 477abbf73 (after), uninstrumented |
|---|---|---|
| svn:// checkout | 7.1 s | 25.0 s |
| DAV checkout | 7.6 s | 23.2 s |
| svn:// update (500 revisions) | 2.7 s | 32.5 s |
| DAV update | 3.6 s | 24.6 s |

Run 10 (477abbf73, each kind of step timed): a window step of the update drive averages ~100 us, against 4.4 us in run
9 (508b2be88, before c2). 7-11K window steps per conversation pass 1 ms, the longest 95 ms.

**Cause.** Since c2, `Repository::text(identity)` rebuilds the whole text from its skip-delta chain on every call
(`decode_all` and `apply_all` of up to log2 N + 1 deltas), and `ReadSource::text(identity, offset)` then copies the
rest (`from_offset`'s `Cow::Owned(rest.to_vec())`). The fetch paths read a window's pieces 1,024 bytes a step (A-751):
the update drive, the replay drive and get-file-revs, all through `report::drive::read_piece`. So every piece rebuilds
and copies the whole text: a text's read is O(size^2 / 1024 x chain).

**What would fit A-751's step budget** (your design; I've changed nothing in records.rs):
- A ranged read that doesn't rebuild per call. Either the text source keeps the text it last made (by identity) for
  the next reads of the same text, the made text charged and let go, or a reader handle (`text_reader(identity)`)
  made once and read a range at a time.
- Making a text from its chain in units. One rebuild of a 350 KB text through ~9 deltas is milliseconds, past the
  quantum in one step; the deltas are svndiff windows, so `svndiff::WindowMaking`'s decode-side twin could apply them
  a window at a time.
c3's HeadTexts should fix head reads (checkouts). Updates from an older revision, replay and get-file-revs read
history texts and would still go through the chain.

Repro: `cargo test --release -p slates-svn --lib -- --ignored window_step_times --nocapture` drives big.txt over the
test green (whole texts, so it doesn't show this); the CI runs are diag-w-bugs-svnfetch-8/9/10/11 (results branches
`*-results-*`). I'm telling main too.
