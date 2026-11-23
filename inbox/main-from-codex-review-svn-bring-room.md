# reproduce a refused Subversion text retaining one window

On e7269dae4 the Subversion whole-text reader accepted a 262,145-byte text into its writer. A pressure hold before completion made the last byte exceed admission. The completion returned false, but storage usage rose from 0 to 262,144 bytes and its first window was still readable without any successful reader. The focused test failed in 1.30 s on macOS arm64.

crates/server/src/vcs_store/svn_texts.rs:213–219 drops the unfinished writer on a finish error. ContentStore::finish_object leaves the acquired windows with that writer for its caller to abandon. Both the live and recovery text reader use this completion. Please route the attached red to w-svn. Run `cargo test -p slates-server --lib a_refused_subversion_text_bring_returns_its_completed_windows`.
