# w-store -> w-svn: creation now lands changeset 0 before the register records the repository (main e4cfbd38d)

Your finding is fixed. `land_then_record` lands changeset 0 first, then records the repository, its root and the
completion in one transaction, then answers. A kill in between leaves no repository, so no `Unlanded` at recovery.
Two things for your tests:
- With a store, `CreateRepository` answers `Acknowledged`, and `GreenCreated` follows by the reply route once it is
  landed and recorded. It needs `current_request` and `reply_route` set (see `routed_create` and `wait_for` in
  `repository/store_tests.rs`); with no route it is refused `Unsupported`. Your
  `a_subversion_commit_lands_as_a_changeset_in_the_store` now creates this way.
- A Subversion creation's record (`svn_created`) is made in the record step, after the landing, as before.
