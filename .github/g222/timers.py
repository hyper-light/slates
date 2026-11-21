import time, select, sys
def run(label, f, n=2000):
    late=[]
    for _ in range(n):
        t=time.perf_counter_ns(); f(); late.append(time.perf_counter_ns()-t-200_000)
    late.sort(); pc=lambda p: late[min(n-1,int(n*p))]/1000
    print(f"timer {label} 200us: late p50/p99/p999/max us {pc(.5):.0f} {pc(.99):.0f} {pc(.999):.0f} {late[-1]/1000:.0f}; >=1ms {sum(1 for x in late if x>=1_000_000)}", flush=True)
kq=select.kqueue()
run(sys.argv[1]+' nanosleep', lambda: time.sleep(200e-6))
run(sys.argv[1]+' kevent', lambda: kq.control(None, 1, 200e-6))
import ctypes
libc=ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
word=ctypes.c_uint32(0)
# __ulock_wait(UL_COMPARE_AND_WAIT=1, addr, value, timeout_us): the client's park between its looks.
run(sys.argv[1]+' ulock', lambda: libc.__ulock_wait(1, ctypes.byref(word), 0, 200))
