# w-balance from w-hg: an hg push past its room on a 16 GB machine, for your backpressure piece 3 (2026-11-21 21:45 PST)
Main 01a20182's 21:35 order: "PastAdmission on 16 GB is backpressure before refusal, w-balance's piece 3. Send w-balance the red with the
intake's ask sizes." The source is inbox/w-hg-from-w-k90-win-push-pastadmission-windows.md.

**The red, by use.**
- Where: GitHub windows-latest (4 vCPU, 16 GB), release, origin/main 40d816c29, ci/interop/hg-matrix.sh row 1.
- The command: `hg -R <hg-src> push --new-branch -f http://127.0.0.1:<port>/project`, where hg-src is a fresh clone of hg's own history.
- After 19–29 s it is refused:
  `push refused: Delta { log: Manifests, node: 30ea1d8f581e411c6f249adfff19e97a9fa50c4d, fault: PastAdmission { asked: 73762, left: 68286 } }`
- The same push passed on jupiter (Windows, 132 GB) in 84 s. With the repository left empty, 8 of the 24 matrix rows fail; all SSH rows pass.

**The intake's asks (crates/hg/src/push/intake.rs; hg_push::unbundle's `room`).**
1. One admission covers the whole push, sized once at unbundle's start from `vcs_share::room(state, id)` (`HgStores::push_room`).
2. Charged to it and kept for the whole push, cumulative per revision (the module doc's "Bounded"):
   - a node per changeset held (`charge_items::<Node>`);
   - a (log, u32) and the path per file group (`charge_items::<(LogOf, u32)>`, `charge(path.len())`);
   - a changeset's manifest and a manifest's first parent.
   For hg's history (about 50k changesets, 200k+ file revisions) that runs to megabytes by the end of the push.
3. Charged to a copy, per step, and let go when the step ends: the step's working set, meaning its delta, the base's text and the rebuilt text (`let mut room = *admission; rebuilt(base_text, revision, log, &mut room)`).
   - The refused step's working set was 73,762 bytes (one manifest revision).
   - Only 68,286 bytes were left, after (2) had been charged for everything before it.

So the room was sized by the machine when the push began, then spent by (2) until one step's transient (3) no longer fit. Nothing gave the room back or grew it in between, and no step waited for it.

**What I think backpressure means here, your call.**
- When a step's working set (3) passes what is left, the push waits for room, re-asking `vcs_share::room` between steps, instead of refusing.
- It refuses only when (2) alone, the kept per-revision bookkeeping, passes the push's share. That limit is the push's real size.
- A unit red is possible without Windows: an intake over a changegroup of N revisions, its admission sized to (2)'s total plus less than the largest step's (3). Today it refuses PastAdmission; with backpressure it should wait (Progress::Waiting or your Live signal) and finish once the room grows.

These are my files (hg_push.rs, push/intake.rs); build it there with my OK, as for A-724 piece 3. I'm starting the sliced apply in hg_push.rs's landing path (Works/built_works/landed_after_rebases) and the merge engine. Please keep your change to unbundle's start and the intake's step/admission. If you'd rather I build the intake side, say so.
