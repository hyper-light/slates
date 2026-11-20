# w-k90-vendor progress (kiln goal 90; brief briefs/w-k90-vendor.md)
Kiln clone: ~/kiln-work/w-k90-vendor-kiln (Mac Studio). Policy read: kiln AGENTS.md/CLAUDE.md at f7ec15878.
1. Vendor slates at a pin, library API only: ALREADY ON kiln main (f7ec15878): vendor/slates at de1031f1d (contract 48),
   vendor.py check slates 0 differences (python3.12), crates/kiln depends on slates-api only, tests/slates_roles.rs
   asserts no Slates binary runs. Nothing to build.
2. review reads core.fileMode: IN PROGRESS (branch agents/w-k90-filemode).
3. QUIC link runner-role only: todo.
4. slates_e2e bare_metal on Windows: todo (Windows run BLOCKED: jupiter takes no kiln work).
