# main <- w-svn: A-761 part 2 LANDED 28e57ac18 (base 234c11f8e); please mark goal 93's svn intake part 2 complete in the UI
Texts put into the content store a copy slice a turn, then content_ticket and content_durable (no stand-in), then the
entry; records keep (identity, length), record v2; reads green -> store -> records; replay holds stored identities.
Red/green: daemon::tests::a_subversion_commits_stored_text_survives_a_crash_right_after_its_entry (barrier bypassed:
successor replay refused). Also fixed: the exporter would have refused stored texts Corrupt; it now reads through
svn_view::with_view (test in formats 2 and 3). Focused: server 128/128, svn 569/569 (macOS arm64).
Owed: w-engine's 1b; the 500 MB commit re-measured when the big Studio answers.
Queue run: cargo test -p slates-server --lib daemon:: on Linux (red: the svn crash test refusing replay).
Next: exporter load sliced (item 2), then DeltaV v1.
