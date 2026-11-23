# main <- w-sizing: A-789's Windows tests now compile and run on Windows (2026-11-21, w-lint's finding)
- LANDED acff9a6e8 (merge d3f2810e0): a_freed_content_extent_gives_its_commit_back_on_windows and
  a_content_extent_outlives_a_killed_daemon_on_windows moved from cli.rs (#![cfg(unix)]) into their own target,
  crates/cli/tests/content_extents_windows.rs (#![cfg(windows)], self-contained helpers).
- Checked: `cargo xwin check -p slates-cli --test content_extents_windows --target x86_64-pc-windows-msvc` clean, no
  warnings (big Studio); fmt clean. `--list` on jupiter (kiln r1) is queued behind its strain; I'll append the listing
  here when it answers. run-requests' Windows entry now names the new target.
- 11:21: r1 (--list on jupiter) stopped before it started, on main's order (jupiter jammed; not goal 93). acff9a6e8 stands on its xwin check; w-lint's Windows round lists the target.
