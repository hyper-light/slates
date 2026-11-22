import os, resource, subprocess, threading, sys
# rtkit refuses a process whose RLIMIT_RTTIME is unset or above its maximum (200 ms by default): set it first.
resource.setrlimit(resource.RLIMIT_RTTIME, (200_000, 200_000))
def ask():
    tid = threading.get_native_id()
    r = subprocess.run(["busctl", "call", "--system", "org.freedesktop.RealtimeKit1", "/org/freedesktop/RealtimeKit1",
                        "org.freedesktop.RealtimeKit1", "MakeThreadRealtimeWithPID", "ttu", str(os.getpid()), str(tid), "1"],
                       capture_output=True, text=True)
    pol = os.sched_getscheduler(tid)
    print(f"rtkit MakeThreadRealtimeWithPID: rc {r.returncode} out {r.stdout.strip()!r} err {r.stderr.strip()!r}; "
          f"policy after {pol} ({'SCHED_RR' if pol == os.SCHED_RR else 'other'})", flush=True)
    for prop in ("MaxRealtimePriority", "RTTimeUSecMax", "MinNiceLevel"):
        p = subprocess.run(["busctl", "get-property", "--system", "org.freedesktop.RealtimeKit1",
                            "/org/freedesktop/RealtimeKit1", "org.freedesktop.RealtimeKit1", prop], capture_output=True, text=True)
        print(f"  {prop}: {p.stdout.strip() or p.stderr.strip()}")
t = threading.Thread(target=ask); t.start(); t.join()
