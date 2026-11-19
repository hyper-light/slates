"""A recorded responder for the stock git-lfs client (A-610; slates-git's LFS golden vectors).

It answers the Git LFS batch, basic-transfer and locks APIs as git-lfs's own documents describe them (v3.8.1,
docs/api/batch.md, basic-transfers.md, locking.md) and writes every request it reads and every answer it sends to a JSON
lines file, so slates-git's codecs can be held to what the stock client really sends and accepts. Standard library only;
run it with `python3 -I responder.py PORT_FILE LOG_FILE`. It listens on an ephemeral loopback port, writes the port to
PORT_FILE, and serves until it is killed. Every value it chooses is fixed (lock ids are numbered, every lock was taken at
LOCKED_AT by OWNER), so two runs over the same client differ only in the port inside the URLs it hands out.
"""

import base64
import hashlib
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qsl, urlsplit

LFS = "/repo.git/info/lfs"
LOCKED_AT = "2026-11-19T00:00:00Z"
OWNER = {"name": "test"}
EXPIRES_AT = "2099-01-01T00:00:00Z"
CONTENT_TYPE = "application/vnd.git-lfs+json"
# One lock a page, so every listing of two or more locks is followed by a cursor.
PAGE = 1


class State:
    def __init__(self, log_path):
        self.mutex = threading.Lock()
        self.objects = {}
        self.locks = {}
        self.next_lock = 1
        self.seq = 0
        self.log = open(log_path, "a", encoding="utf-8")

    def record(self, entry):
        with self.mutex:
            self.seq += 1
            entry["seq"] = self.seq
            self.log.write(json.dumps(entry, sort_keys=True) + "\n")
            self.log.flush()


STATE = None


def text_or_digest(data):
    try:
        text = data.decode("utf-8")
        if all(ch.isprintable() or ch in "\n\t" for ch in text):
            return {"text": text}
    except UnicodeDecodeError:
        pass
    return {"base64": base64.b64encode(data).decode("ascii"), "sha256": hashlib.sha256(data).hexdigest()}


def page_of(locks, cursor, limit):
    ids = [lock["id"] for lock in locks]
    start = ids.index(cursor) if cursor in ids else 0
    size = min(PAGE, limit) if limit else PAGE
    page = locks[start:start + size]
    following = locks[start + size:start + size + 1]
    return page, (following[0]["id"] if following else None)


def batch(request, base):
    out = []
    for obj in request.get("objects", []):
        oid, size = obj["oid"], obj["size"]
        entry = {"oid": oid, "size": size}
        have = oid in STATE.objects
        if request.get("operation") == "upload":
            if not have:
                entry["actions"] = {
                    "upload": {"href": base + "/objects/" + oid, "header": {"X-Slates-Test": "upload"},
                               "expires_in": 3600},
                    "verify": {"href": base + "/verify", "header": {"X-Slates-Test": "verify"}, "expires_in": 3600},
                }
        elif have:
            entry["authenticated"] = True
            entry["actions"] = {"download": {"href": base + "/objects/" + oid,
                                             "header": {"X-Slates-Test": "download"}, "expires_at": EXPIRES_AT}}
        else:
            entry["error"] = {"code": 404, "message": "Object does not exist"}
        out.append(entry)
    return 200, {"transfer": "basic", "objects": out, "hash_algo": "sha256"}


def create_lock(request):
    path = request["path"]
    for lock in STATE.locks.values():
        if lock["path"] == path:
            return 409, {"lock": lock, "message": "already created lock", "request_id": "slates-test"}
    lock_id = "lock-%d" % STATE.next_lock
    STATE.next_lock += 1
    lock = {"id": lock_id, "path": path, "locked_at": LOCKED_AT, "owner": OWNER}
    STATE.locks[lock_id] = lock
    return 201, {"lock": lock}


def list_locks(query):
    params = dict(parse_qsl(query, keep_blank_values=True))
    locks = list(STATE.locks.values())
    if params.get("path"):
        locks = [lock for lock in locks if lock["path"] == params["path"]]
    if params.get("id"):
        locks = [lock for lock in locks if lock["id"] == params["id"]]
    page, following = page_of(locks, params.get("cursor"), int(params.get("limit") or 0))
    body = {"locks": page}
    if following:
        body["next_cursor"] = following
    return 200, body


def verify_locks(request):
    page, following = page_of(list(STATE.locks.values()), request.get("cursor"), request.get("limit") or 0)
    body = {"ours": page, "theirs": []}
    if following:
        body["next_cursor"] = following
    return 200, body


def unlock(lock_id):
    lock = STATE.locks.pop(lock_id, None)
    if lock is None:
        return 404, {"message": "Not found", "request_id": "slates-test"}
    return 200, {"lock": lock}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def read_body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def route(self, method, url, body):
        base = "http://127.0.0.1:%d" % self.server.server_address[1]
        path = url.path
        if method == "POST" and path == LFS + "/objects/batch":
            return batch(json.loads(body), base)
        if method == "PUT" and path.startswith("/objects/"):
            oid = path.rsplit("/", 1)[-1]
            if hashlib.sha256(body).hexdigest() != oid:
                return 422, {"message": "content does not match its oid"}
            STATE.objects[oid] = body
            return 200, None
        if method == "POST" and path == "/verify":
            obj = json.loads(body)
            if len(STATE.objects.get(obj["oid"], b"")) == obj["size"] and obj["oid"] in STATE.objects:
                return 200, None
            return 404, {"message": "Object does not exist"}
        if method == "GET" and path.startswith("/objects/"):
            data = STATE.objects.get(path.rsplit("/", 1)[-1])
            return (200, data) if data is not None else (404, {"message": "Object does not exist"})
        if method == "POST" and path == LFS + "/locks":
            return create_lock(json.loads(body))
        if method == "GET" and path == LFS + "/locks":
            return list_locks(url.query)
        if method == "POST" and path == LFS + "/locks/verify":
            return verify_locks(json.loads(body))
        if method == "POST" and path.startswith(LFS + "/locks/") and path.endswith("/unlock"):
            return unlock(path[len(LFS + "/locks/"):-len("/unlock")])
        return 404, {"message": "Not found"}

    def handle_method(self, method):
        url = urlsplit(self.path)
        body = self.read_body()
        status, payload = self.route(method, url, body)
        if isinstance(payload, bytes):
            data, content_type = payload, "application/octet-stream"
        elif payload is None:
            data, content_type = b"", None
        else:
            data, content_type = json.dumps(payload).encode("utf-8"), CONTENT_TYPE
        self.send_response(status)
        if content_type:
            self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        STATE.record({
            "request": {"method": method, "path": url.path, "query": url.query,
                        "accept": self.headers.get("Accept"), "content_type": self.headers.get("Content-Type"),
                        "body": text_or_digest(body)},
            "response": {"status": status, "content_type": content_type, "body": text_or_digest(data)},
        })

    def do_GET(self):
        self.handle_method("GET")

    def do_PUT(self):
        self.handle_method("PUT")

    def do_POST(self):
        self.handle_method("POST")


def main():
    global STATE
    port_file, log_file = sys.argv[1], sys.argv[2]
    STATE = State(log_file)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    with open(port_file + ".tmp", "w", encoding="ascii") as handle:
        handle.write(str(server.server_address[1]))
    import os
    os.replace(port_file + ".tmp", port_file)
    server.serve_forever()


if __name__ == "__main__":
    main()
