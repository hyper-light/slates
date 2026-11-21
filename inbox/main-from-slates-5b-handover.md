# Handover: old main (kiln 01a14885, session slates-5b on adas-macbook-pro-2) -> new main 01a1ff31 (2026-11-21 ~10:05 PST)

I stand down as coordinator, as briefs/common.md now says. Why you may have seen me as "not answering": since last night kiln has
held most mail TO 01a14885 behind a stuck admission lease (one message "admitted", never consumed; the window is 1). Messages to
me through kiln never arrived. I was working the whole time through session messages, goals and inbox files. Every ruling and
landing since 06:30 is in focus-84-90.md with `date` stamps.

## Runs I own, still in flight or owed
- r26 (main's): the 100k-client load_scale rerun on the big Studio (d1323898), for w-fed-move's "next 100k wall"
  (drop sites, merge.work.answered rows). Its log lands at ~/.cache/main-load-100k-*.log there. Send the rows to w-fed-move.
- OWED, NOT RUN, for w-svn (A-820's condition 3 and P7 finding (2)). Both need git-2000.dump (437 MiB), which is ONLY on the
  MacBook (~/.cache/w-svn-gitgit/git-2000.dump). kiln's sync stalled at 4.4 MB carrying it to Studio-2 (r60), so run them on
  the MacBook when its disk has ~25 GB to spare:
  (a) copy A/B: A=3bf21428b, B=be9a33e34, `SLATES_SVN_COPY_DUMP=<dump> cargo test --release -p slates-server --lib --
      --ignored measure_copies_from_older_revisions --nocapture`, interleaved, 3 rounds (inbox/main-from-w-svn-a820-runs.md);
  (b) heap attribution: the SLATES_SVN_IMPORT_HOLD hook (30d2e4100), MallocStackLogging=1, then at the hold `vmmap -summary`,
      `heap -s`, `malloc_history <pid> -allBySize`; the hook's patience is 120 s.
- w-rewrite-ops' A-818 piece 2b A/B (history_layers_bench, branch against main, before it lands): run-requests.md.

## Faults and the routes that work
- kiln mail stuck "admitted": message MacBook sessions with SendMessage (slates-work-19 w-hg, -7a w-svn, -be w-perf, -7e w-git,
  -9f w-interop, -83 w-shared; map names via ~/.claude/sessions/<pid>.json). Remote agents: goals (plan_big is OFF on all
  42 agents) or a reworded active goal (goals.set resends it at once). A turn from any route releases the stuck lease.
- The big Studio can't resolve github.com and has no GitHub credential. Its agents push through kiln to the MacBook or through
  w-rewrite-ops' bundle server. w-engine, w-fed-move and w-work-cow held finished commits locally. Their harnesses were restarted
  at ~09:05; still check that they push.
- R2: adacachy's stray 192.168.1.77 makes runners' QUIC fall back to the tunnel, which drops on Wi-Fi blips. The big Studio,
  Studio-2 and jupiter dropped repeatedly today. Ada's fix: `sudo ip addr del 192.168.1.77/24 dev enp9s0`. w-k90-pin-2 builds
  the kiln-side fix (IP_PKTINFO).
- MacBook disk: it hovered at 9-50 GB. Levers that aren't slates': Mantle's e2e temp (its session fixed its harness at ~09:48),
  kiln-opt-target and kiln-move-stuck-target (~24 GB each), Docker.raw 140 GB (~87 GB reclaimable). Run clean-aggressive.sh
  under bash with nullglob (zsh's no-match aborts the list).
- A-numbers: next free is A-826 (main assigned A-823, A-824 to w-git and A-825 to w-shared; g228 claims A-826).

## Open rulings and who holds what (details and evidence in focus-84-90.md)
- w-git: piece 3 part 2 (v1/v2 retired; old logs refused typed). NEXT and top priority: tags naming blobs or tags are refused,
  and every whole clone of git/git fails (w-interop's r16); then the push at 50 s against 10 s and the shallow clone at 4.2x.
- w-hg: the whole-text place fix (one rule; w-perf told the lines), then lookup pieces 1 and 2, then step 2.
- w-svn: A-822 (stage C, bounded by the import's admission, refusal names the last landed revision and is resumable), then
  P7 (2) and (3), KeptText last.
- w-perf: A/Bs r51-r53 (cpus=8 idle); the fan-out (sha1dc kept, hashes on idle shards) lands only on r51.
- w-rewrite-ops: A-818 2b on a branch, landing on the A/B. w-fed-faults: A-821's step bound in debug waits on 2b.
- w-work-cow: the 1 GiB / 100k-file single-submit slicing (step 2 of its red plan), plus w-shared's 320 MiB case.
- w-shared: 3a's tail (320 MiB exit 4 to w-work-cow; is 512 MiB Overloaded a bound or a gap?), then 3b.
- g228: the busy-harvest interval A/B alone in four conditions; 85a stays open (Linux ~95-140 us, macOS 156-163 us at the median).
- w-bugs: five items, including t_a752 (asserts timing, not the rule) and the pair-dialer flake.
- w-work-mount: the Windows store lane, green run on jupiter (disk ask resized). w-lint: its rounds and the Windows checks.
- w-interop: reruns the git comparison after w-git's ref fix.

## Ada's open items (unchanged today)
- adacachy's .77 address.
- The four stuck kiln CI lanes.
- w-store's restore (entry 01a1fc635009).
- UI goals to mark: "kiln vendors slates" and goal 92's Shards part.
- Studio-4's root-owned dead NFS mount kiln-01a1f9e974cb... (sudo umount -f).
- Docker and kiln targets on the MacBook.
- fseventsd at 14.9 GB.
