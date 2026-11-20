# w-realtools -> main, 2026-11-20 ~15:35 PST: one dead hard NFS mount stalls every NFS mount on a macOS client
- On Studio-4 (this Mac), after main unmounted w-forming's dead /shipped mount, a fresh slates overlay mount still hangs
  `ls -f` (READDIR never sent; GETATTR/LOOKUP/ACCESS answered). The root-mounted dead kiln-01a1f9e974cb... (port 57217,
  no listener) is the remaining suspect: it needs root, so it is Ada's.
- I'm running the real-tool tests on another Mac via kiln run (needs macos). Debug prints reverted. Base: origin/main ebd9014c3.
