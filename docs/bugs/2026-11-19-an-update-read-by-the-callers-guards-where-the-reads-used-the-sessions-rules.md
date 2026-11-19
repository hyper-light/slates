# An update read by the caller's guards where the read commands used the session's rules (2026-11-19, A-656 continued (2))

## Description

Found by g297 (A-613's eighth slice, step 6) and given to g234 by main. The svn:// connection
(`crates/svn/src/serve.rs`) judged a session's reads by two sources. The read commands asked the rules the
repository's authorization file gives the principal, compiled by the connection (A-606). `update` asked the caller's
`Guards::may_read`, for its target and, through the report drive (`crates/svn/src/report/drive.rs`), for the edit's
root, every entry and every copy source. For one principal and one path the two could answer differently: with guards
that hid less than the rules, an anonymous session was refused `check-path secret` beneath `/trunk` ("Authorization
failed", 170001) but could `update` it, and an update of `/trunk` sent `secret` whole.

## Root cause

The report drive (A-616 part 2) was built before the read commands' authorization (A-606, A-613's eighth slice) and
took its read rule from the caller's guards, as `svnserve`'s `authz_check_access_cb` stands for its own rules. When
the connection gained compiled rules for the read commands, `update` and its drive kept asking the guards: two places
for one principal's rule.

## Impact

A caller whose guards disagreed with the repository's authorization file would have sent, through `update`, `switch`,
`status` and `diff`, paths the read commands refuse, or hidden paths they serve. No caller in slates implements
`Guards` yet (the daemon's listener is A-656 part 2, owed): the test fixtures' guards hid `/trunk/secret` by hand,
matching the rules by care, so no served session was affected.

## Edits

- `crates/svn/src/authz.rs`: `may_read(rules, path)`, a session's one read rule (`authz_check_access` for
  `svn_authz_read`), moved from the read commands' `readable`.
- `crates/svn/src/serve.rs`: `ReadRules::for_principal` compiles the principal's rules once and again only after an
  authentication changes the principal; `Established::compiled_read_rules` hands that set to `update`'s two target
  checks and to the drive's sources, as `reads_session` hands it to the read commands.
- `crates/svn/src/report/drive.rs`: `Guards` keeps `lock_token` alone; `Sources` gains `rules`; `Drive::next` takes
  the `Sources`; the root, entry and copy-source checks ask `may_read` with them. `report/exchange.rs` hands its
  sources on.
- `crates/svn/src/reads.rs`: `readable` asks `authz::may_read`.
- Tests: `serve::tests::an_update_and_the_reads_judge_one_path_by_the_one_rule_set`, failing first;
  `report/drive/green.rs`'s `FixtureGuards` holds locks alone, and its `rules_of` compiles the recorder's `AUTHZ` for
  a principal, which the drive's and the exchange's tests judge reads by; `serve/tests.rs` takes `AUTHZ` from there.

## Sweep

Every read decision in `crates/svn` was read on 2026-11-19: the read commands (`reads::readable`), `update`'s target
(two sites in `serve.rs`) and the drive (root, entries, copy sources) now ask the one rule. A-655's commit drive
judges read and write access by rules it compiles itself from the same `Policy::authz` and principal
(`commit::drive::Setup::authz`): the same answers, compiled a second time, left as it is. Reported to main, not
changed: the locks a session holds have two sources too, `Guards::lock_token` for the drive and A-602's `LockTable` in
`serve::Sources::locks`.
