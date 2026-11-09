# Execs fusermount3, holds the exited helper unreaped (waitid WNOWAIT), reads its AppArmor label from /proc, then reaps.
import os, sys
pid = os.posix_spawnp("fusermount3", ["fusermount3", "-V"], dict(os.environ))
info = os.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT)
print(f"uid {os.getuid()} euid {os.geteuid()}: helper {pid} exited code {info.si_status}, unreaped")
for attr in ("attr/apparmor/current", "attr/current"):
    path = f"/proc/{pid}/{attr}"
    try:
        with open(path) as f:
            print(f"  {path}: {f.read().strip()!r}")
    except OSError as e:
        print(f"  {path}: {type(e).__name__} errno {e.errno} {e.strerror}")
os.waitpid(pid, 0)
print("  reaped")
