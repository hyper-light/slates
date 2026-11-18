# The receiving end of kill_fsync_macos.sh's SINK: run on another machine, it appends every line any sender sends to
# FILE and fsyncs it before reading the next, and prints it, so a sender's kernel panic loses nothing it sent.
#   python3 log_sink.py PORT FILE
import os
import socket
import sys
import threading

port, path = int(sys.argv[1]), sys.argv[2]
log = open(path, "a")
lock = threading.Lock()


def serve(conn, peer):
    with conn:
        for line in conn.makefile("rb"):
            text = line.decode("utf-8", "replace")
            with lock:
                log.write(text)
                log.flush()
                os.fsync(log.fileno())
                sys.stdout.write(f"{peer[0]} {text}")
                sys.stdout.flush()


listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("0.0.0.0", port))
listener.listen()
print(f"log_sink: port {port}, appending to {path}", flush=True)
while True:
    conn, peer = listener.accept()
    threading.Thread(target=serve, args=(conn, peer), daemon=True).start()
