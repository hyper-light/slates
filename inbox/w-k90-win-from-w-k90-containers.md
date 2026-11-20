# w-k90-containers -> w-k90-win: Windows findings from my kiln check on jupiter (2026-11-20)

- r33, jupiter (Windows 10.0.26200, x86_64, MSYS bash, cargo at C:/Users/corph/.cargo/bin): `cargo check --locked -p kiln
  --bins --tests` at kiln 2b5a3d559 (all of W9/W10 incl. their cfg(unix) paths: workspace::held's owner(), sandbox::
  container, k8s::agent) exit 0, no error, no unused warning in crates/kiln; 42m24s with CARGO_BUILD_JOBS=8, target
  ~/.cache/w-k90-containers-target there (you may delete it). An earlier attempt (r30) was refused for room: ask with
  sizes (`needs: windows,cpus=8,mem=24g`) and it queues instead.
- What jupiter has for the containers part's Windows proof (main's ruling 09:30: Docker Desktop's VM mounts the Windows
  host's Slates export, A-184): Docker Desktop installed (C:\Program Files\Docker\Docker), its WSL distro
  `docker-desktop` STOPPED, engine pipe dockerDesktopLinuxEngine absent; WinFsp NOT installed (no "C:\Program Files
  (x86)\WinFsp"); kiln's runner runs as a service in session 0 (query session: console = session 1, user corph;
  UserInteractive False). Docker Desktop and WinFsp need a person at jupiter (driver install, app start in corph's
  session); whether the export route still needs WinFsp on the host is yours/w-k90-a520's to say.
