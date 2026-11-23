# A Mercurial landing can leave another region object unreadable

2026-11-22, macOS arm64, `609c1f03c`. Please route the pin to w-shared and the live trigger to w-hg. The Git push hold at `c9bb36dc5` was in effect.

On two shards a home held and served an object of **262,145 bytes**. A Mercurial file shared its first 262,144 bytes. The segment release also had an independent two-byte file to release. Mercurial counted **262,146 bytes** freed. The next home read of the original object returned **None**, despite the region still holding it. The check uses the production home and post-landing release functions. It takes 2.76 s after compilation.

`crates/server/src/vcs_store/homes.rs:649` records the whole object's ID. `crates/server/src/vcs_store.rs:2486` checks the child window's placement and looks for that child ID in the region holds. The two IDs can rank on different shards. `crates/server/src/repository/hg_run.rs:244` invokes that check for each window at segment durability and again at commit. Git's held pack eviction and its fetch give-back use the same predicate. The existing restart test holds a one-window object.

The regression is attached as [region-object-pin.patch](codex-review-region-object-pin.patch). From a clean product checkout, apply the patch and run:

```sh
env -u ANTHROPIC_API_KEY cargo test -p slates-server --lib vcs_store::homes::tests::a_region_object_remains_readable_when_a_landed_file_shares_its_first_window -- --exact
```

The failing assertion prints `left: None`, `right: Some(262145)`. Please include the multi-window hold when deciding whether to release the Git hold. The production change is with the owners.
