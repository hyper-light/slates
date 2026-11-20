# A recorded responder for capturing stock svn's ra_serf request bodies (A-609 golden vectors).
# It records every request (method, path, headers, body) and answers each with a canned, plausible mod_dav_svn reply,
# so the client goes on to its next request. Its replies are not evidence of anything; only the recorded requests are.
import base64, http.server, json, socketserver, sys, urllib.parse
import xml.etree.ElementTree as ET

ROOT = "/repo"
UUID = "6b4a5e2c-1d3f-4a7b-9c8e-0f1a2b3c4d5e"
YOUNGEST = 5
DIRS = {"", "trunk", "trunk/d", "branches", "branches/b"}
FILES = {"trunk/f.txt": b"hello\n", "trunk/d/g.txt": b"world\n", "branches/b/f.txt": b"hello\n"}
DAV = "DAV:"
V = "http://subversion.tigris.org/xmlns/dav/"
CAPS = ["1,2", "version-control,checkout,working-resource",
        "merge,baseline,activity,version-controlled-collection"] + [
    V + "svn/" + c for c in ["depth", "log-revprops", "atomic-revprops", "partial-replay", "inherited-props",
                             "inline-props", "reverse-file-revs", "list", "mergeinfo", "replay-rev-resource",
                             "ephemeral-txnprops", "svndiff1", "svndiff2", "put-result-checksum"]]
HEAD = '<?xml version="1.0" encoding="utf-8"?>\n'
capture = open(sys.argv[1], "ab")
portfile = sys.argv[2]


def rel_of(path):
    path = urllib.parse.unquote(urllib.parse.urlsplit(path).path)
    if not path.startswith(ROOT):
        return None
    rest = path[len(ROOT):].strip("/")
    if rest.startswith("!svn/"):
        parts = rest.split("/")
        if parts[1] in ("rvr", "bc", "ver") and len(parts) >= 3:
            return "/".join(parts[3:])
        if parts[1] in ("txr",) and len(parts) >= 3:
            return "/".join(parts[3:])
        return ""
    return rest


def kind_of(rel):
    if rel in DIRS:
        return "dir"
    if rel in FILES:
        return "file"
    return None


def prop_xml(ns, name, rel, kind):
    href = urllib.parse.quote(ROOT + "/!svn/ver/5/" + rel)
    table = {
        (DAV, "resourcetype"): "<D:resourcetype>%s</D:resourcetype>" % ("<D:collection/>" if kind == "dir" else ""),
        (DAV, "version-name"): "<D:version-name>5</D:version-name>",
        (DAV, "checked-in"): "<D:checked-in><D:href>%s</D:href></D:checked-in>" % href,
        (DAV, "version-controlled-configuration"):
            "<D:version-controlled-configuration><D:href>%s/!svn/vcc/default</D:href>"
            "</D:version-controlled-configuration>" % ROOT,
        (DAV, "baseline-collection"): "<D:baseline-collection><D:href>%s/!svn/bc/5/</D:href></D:baseline-collection>" % ROOT,
        (DAV, "creationdate"): "<D:creationdate>2026-11-19T00:00:00.000000Z</D:creationdate>",
        (DAV, "creator-displayname"): "<D:creator-displayname>alice</D:creator-displayname>",
        (DAV, "getcontentlength"): "<D:getcontentlength>%d</D:getcontentlength>" % len(FILES.get(rel, b"")),
        (DAV, "getlastmodified"): "<D:getlastmodified>Wed, 19 Nov 2026 00:00:00 GMT</D:getlastmodified>",
        (DAV, "lockdiscovery"): "<D:lockdiscovery/>",
        (V, "baseline-relative-path"): "<V:baseline-relative-path>%s</V:baseline-relative-path>" % rel,
        (V, "repository-uuid"): "<V:repository-uuid>%s</V:repository-uuid>" % UUID,
        (V, "md5-checksum"): "<V:md5-checksum>b1946ac92492d2347c6235b4d2611184</V:md5-checksum>" if kind == "file" else None,
        (V, "deadprop-count"): "<V:deadprop-count>0</V:deadprop-count>",
    }
    return table.get((ns, name))


def multistatus(entries):
    out = [HEAD, '<D:multistatus xmlns:D="DAV:" xmlns:V="%s" xmlns:S="http://subversion.tigris.org/xmlns/svn/">\n' % V]
    for href, found, missing in entries:
        out.append("<D:response>\n<D:href>%s</D:href>\n" % href)
        if found:
            out.append("<D:propstat>\n<D:prop>\n%s\n</D:prop>\n<D:status>HTTP/1.1 200 OK</D:status>\n</D:propstat>\n"
                       % "\n".join(found))
        if missing:
            out.append("<D:propstat>\n<D:prop>\n%s\n</D:prop>\n<D:status>HTTP/1.1 404 Not Found</D:status>\n"
                       "</D:propstat>\n" % "\n".join(missing))
        out.append("</D:response>\n")
    out.append("</D:multistatus>\n")
    return "".join(out).encode()


REPORTS = {
    "log-report": '<S:log-report xmlns:S="svn:" xmlns:D="DAV:">\n<S:log-item>\n<D:version-name>5</D:version-name>\n'
                  '<S:creator-displayname>alice</S:creator-displayname>\n<S:date>2026-11-19T00:00:00.000000Z</S:date>\n'
                  '<D:comment>change</D:comment>\n</S:log-item>\n</S:log-report>',
    "dated-rev-report": '<S:dated-rev-report xmlns:S="svn:" xmlns:D="DAV:">\n<D:version-name>3</D:version-name>'
                        '</S:dated-rev-report>',
    "get-locks-report": '<S:get-locks-report xmlns:S="svn:" xmlns:D="DAV:">\n</S:get-locks-report>\n',
    "get-locations": '<S:get-locations-report xmlns:S="svn:" xmlns:D="DAV:">\n<S:location rev="2" path="/trunk/f.txt"/>\n'
                     '</S:get-locations-report>\n',
    "get-location-segments": '<S:get-location-segments-report xmlns:S="svn:" xmlns:D="DAV:">\n'
                             '<S:location-segment path="trunk" range-start="1" range-end="5"/>\n'
                             '</S:get-location-segments-report>\n',
    "mergeinfo-report": '<S:mergeinfo-report xmlns:S="svn:" xmlns:D="DAV:">\n</S:mergeinfo-report>\n',
    "get-deleted-rev-report": '<S:get-deleted-rev-report xmlns:S="svn:" xmlns:D="DAV:">\n'
                              '<D:version-name>4</D:version-name></S:get-deleted-rev-report>',
    "inherited-props-report": '<S:inherited-props-report xmlns:S="svn:" xmlns:D="DAV:">\n</S:inherited-props-report>\n',
    "list-report": '<S:list-report xmlns:S="svn:" xmlns:D="DAV:">\n</S:list-report>\n',
    "update-report": '<S:update-report xmlns:S="svn:" xmlns:V="%s" xmlns:D="DAV:" send-all="true" inline-props="true">\n'
                     '<S:target-revision rev="5"/>\n<S:open-directory rev="5">\n<D:checked-in>\n'
                     '<D:href>/repo/!svn/ver/5/trunk</D:href>\n</D:checked-in>\n'
                     '<S:set-prop name="svn:entry:committed-rev">5</S:set-prop>\n'
                     '<S:set-prop name="svn:entry:committed-date">2026-11-19T00:00:00.000000Z</S:set-prop>\n'
                     '<S:set-prop name="svn:entry:last-author">alice</S:set-prop>\n'
                     '<S:set-prop name="svn:entry:uuid">%s</S:set-prop>\n</S:open-directory>\n'
                     '</S:update-report>\n' % (V, UUID),
    "file-revs-report": '<S:file-revs-report xmlns:S="svn:" xmlns:D="DAV:">\n'
                        '<S:file-rev path="/trunk/f.txt" rev="5">\n<S:rev-prop name="svn:author">alice</S:rev-prop>\n'
                        '<S:txdelta>U1ZOAAAABgIGAYYFaGVsbG8K\n</S:txdelta></S:file-rev>\n</S:file-revs-report>\n',
    "replay-report": '<S:editor-report xmlns:S="svn:">\n<S:target-revision rev="1"/>\n<S:open-root rev="0"/>\n'
                     '<S:close-directory/>\n</S:editor-report>\n',
}


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def read_body(self):
        if "chunked" in self.headers.get("Transfer-Encoding", "").lower():
            data = b""
            while True:
                size = int(self.rfile.readline().split(b";")[0].strip() or b"0", 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    return data
                data += self.rfile.read(size)
                self.rfile.readline()
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def reply(self, code, body=b"", headers=(), ctype='text/xml; charset="utf-8"'):
        self.send_response(code)
        for key, value in headers:
            self.send_header(key, value)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def handle_any(self):
        body = self.read_body()
        kept = {k: v for k, v in self.headers.items()
                if k.lower() not in ("host", "user-agent", "connection", "accept-encoding", "te", "date")}
        capture.write(json.dumps({"method": self.command, "path": self.path, "headers": kept,
                                  "body": base64.b64encode(body).decode()}).encode() + b"\n")
        capture.flush()
        try:
            getattr(self, "answer_" + self.command)(body)
        except Exception as error:  # the capture matters, not the reply
            self.reply(500, ("<error>%s</error>" % error).encode())

    def answer_OPTIONS(self, body):
        headers = [("DAV", c) for c in CAPS] + [
            ("SVN-Youngest-Rev", str(YOUNGEST)), ("SVN-Repository-UUID", UUID),
            ("SVN-Repository-MergeInfo", "yes"), ("SVN-Repository-Root", ROOT),
            ("SVN-Me-Resource", ROOT + "/!svn/me"), ("SVN-Rev-Root-Stub", ROOT + "/!svn/rvr"),
            ("SVN-Rev-Stub", ROOT + "/!svn/rev"), ("SVN-Txn-Root-Stub", ROOT + "/!svn/txr"),
            ("SVN-Txn-Stub", ROOT + "/!svn/txn"), ("SVN-VTxn-Root-Stub", ROOT + "/!svn/vtxr"),
            ("SVN-VTxn-Stub", ROOT + "/!svn/vtxn"), ("SVN-Allow-Bulk-Updates", "Prefer"),
            ("SVN-Supported-Posts", "create-txn"), ("SVN-Supported-Posts", "create-txn-with-props"),
            ("MS-Author-Via", "DAV"), ("Allow", "OPTIONS,GET,HEAD,POST,DELETE,TRACE,PROPFIND,PROPPATCH,COPY,MOVE,"
                                                 "LOCK,UNLOCK,CHECKOUT")]
        reply = (HEAD + '<D:options-response xmlns:D="DAV:">\n<D:activity-collection-set><D:href>%s/!svn/act/</D:href>'
                 '</D:activity-collection-set></D:options-response>\n' % ROOT).encode()
        self.reply(200, reply, headers)

    def answer_PROPFIND(self, body):
        rel = rel_of(self.path)
        kind = kind_of(rel) if rel is not None else None
        if kind is None:
            return self.reply(404, b"")
        wanted = []
        if body.strip():
            for prop in ET.fromstring(body).iter("{DAV:}prop"):
                for child in prop:
                    ns, name = child.tag[1:].split("}", 1)
                    wanted.append((ns, name))
        if not wanted:
            wanted = [(DAV, "resourcetype"), (DAV, "version-name"), (DAV, "checked-in")]
        targets = [(rel, kind)]
        if self.headers.get("Depth") == "1" and kind == "dir":
            prefix = rel + "/" if rel else ""
            for child in sorted(DIRS | set(FILES)):
                if child.startswith(prefix) and child != rel and "/" not in child[len(prefix):]:
                    targets.append((child, kind_of(child)))
        entries = []
        for target, target_kind in targets:
            found, missing = [], []
            for ns, name in wanted:
                value = prop_xml(ns, name, target, target_kind)
                if value:
                    found.append(value)
                else:
                    missing.append('<g0:%s xmlns:g0="%s"/>' % (name, ns))
            href = urllib.parse.quote(urllib.parse.urlsplit(self.path).path.rstrip("/") +
                                      ("/" + target[len(rel):].lstrip("/") if target != rel else ""))
            entries.append((href, found, missing))
        self.reply(207, multistatus(entries))

    def answer_REPORT(self, body):
        name = ET.fromstring(body).tag.split("}", 1)[-1]
        if name not in REPORTS:
            return self.reply(501, b"")
        self.reply(200, (HEAD + REPORTS[name]).encode())

    def answer_GET(self, body):
        rel = rel_of(self.path)
        if rel in FILES:
            return self.reply(200, FILES[rel], [("ETag", '"5//%s"' % rel)], "text/plain")
        self.reply(404, b"")

    def answer_POST(self, body):
        self.reply(201, b"", [("SVN-Txn-Name", "5-1"), ("Location", ROOT + "/!svn/txn/5-1")])

    def answer_MKCOL(self, body):
        self.reply(201, b"")

    def answer_PUT(self, body):
        self.reply(201, b"")

    def answer_PROPPATCH(self, body):
        self.reply(207, multistatus([(urllib.parse.urlsplit(self.path).path, [], [])]))

    def answer_DELETE(self, body):
        self.reply(204, b"")

    def answer_MERGE(self, body):
        reply = (HEAD + '<D:merge-response xmlns:D="DAV:">\n<D:updated-set>\n<D:response>\n'
                 '<D:href>%s/!svn/vcc/default</D:href>\n<D:propstat><D:prop>\n'
                 '<D:resourcetype><D:baseline/></D:resourcetype>\n<D:version-name>6</D:version-name>\n'
                 '<D:creationdate>2026-11-19T00:00:00.000000Z</D:creationdate>\n'
                 '<D:creator-displayname>alice</D:creator-displayname>\n</D:prop>\n'
                 '<D:status>HTTP/1.1 200 OK</D:status>\n</D:propstat>\n</D:response>\n</D:updated-set>\n'
                 '</D:merge-response>\n' % ROOT).encode()
        self.reply(200, reply)

    do_OPTIONS = do_PROPFIND = do_REPORT = do_GET = do_POST = do_MKCOL = do_PUT = handle_any
    def answer_LOCK(self, body):
        reply = (HEAD + '<D:prop xmlns:D="DAV:">\n<D:lockdiscovery>\n<D:activelock>\n'
                 '<D:locktype><D:write/></D:locktype>\n<D:lockscope><D:exclusive/></D:lockscope>\n'
                 '<D:depth>0</D:depth>\n<D:owner>alice</D:owner>\n<D:timeout>Infinite</D:timeout>\n'
                 '<D:locktoken>\n<D:href>opaquelocktoken:6b4a5e2c-0000-4000-8000-000000000001</D:href>\n'
                 '</D:locktoken>\n</D:activelock>\n</D:lockdiscovery>\n</D:prop>\n').encode()
        self.reply(200, reply, [("Lock-Token", "<opaquelocktoken:6b4a5e2c-0000-4000-8000-000000000001>"),
                                 ("X-SVN-Creation-Date", "2026-11-19T00:00:00.000000Z"),
                                 ("X-SVN-Lock-Owner", "alice")])

    def answer_UNLOCK(self, body):
        self.reply(204, b"")

    do_PROPPATCH = do_DELETE = do_MERGE = do_LOCK = do_UNLOCK = handle_any


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


server = Server(("127.0.0.1", 0), Handler)
with open(portfile, "w") as out:
    out.write(str(server.server_address[1]))
server.serve_forever()
