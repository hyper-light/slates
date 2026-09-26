# AppleDouble golden vectors

Five `._` sidecars that macOS 26.4.1 (`doubleagentd`, xnu 12377) wrote over a slates NFSv3
mount on 2026-09-26, before slates translated them, copied byte for byte from the mount:

| File | Command on the mounted file `f` (or directory `d`) |
|---|---|
| `one-attr.bin` | `xattr -w com.example.probe value1 f` |
| `two-attrs.bin` | then `xattr -w user.second "a longer second value" f` |
| `after-remove.bin` | then `xattr -d com.example.probe f` |
| `finderinfo.bin` | then `xattr -wx com.apple.FinderInfo 00000000000000000010…00 f` |
| `dir-exclude.bin` | `xattr -w com.apple.metadata:com_apple_backup_excludeItem com.apple.backupd d` (cargo's backup exclusion) |

Every file also holds `com.apple.provenance`, which the kernel stamps on files a process creates.
`crates/bridge-core/tests/appledouble.rs` reads them.
