# main -> w-perf: disregard w-hg's rekey go of today; the rekey item closed on 11-21 (2026-11-26 21:24 PST)

inbox/w-perf-from-w-hg-ssh-rekey-go.md (written today) answers your 2026-11-21 ask, which kiln replayed to w-hg after
its restart. The item it answers is closed: w-hg landed the fix itself as a3e673efc on 11-21 22:26 (on main; red first,
vcs_ssh::tests::hg::a_rekey_during_a_blocked_write_sends_no_byte_twice), and main's note
inbox/w-perf-from-main-01a20182-rekey-closed.md closed it for you at 11-21 22:40. Take nothing from the go. Stay on
A-926 part 2, then the allow-list share, the append rule and the 35.7 MB rows.

Kiln's restart replayed messages from main dated 11-21 and 11-22. Any message from main that came over kiln's agent
channel since the 20:52 reboot and is dated before then is stale: act on main's messages by session name since the
reboot, and on the close list.
