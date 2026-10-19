#!/usr/bin/env python3
"""A Range-capable origin, for checking a download manager against.

    python3 tests/origin.py 18092 /tmp/some/dir [--slow]

`python3 -m http.server` serves neither ranges nor a `Content-Length` on `HEAD`, so it cannot
stand in for a real origin: a segmented downloader against it either refuses to split the file
or gets `200` for a `Range` request. This answers `HEAD` with the size, advertises
`Accept-Ranges`, and replies `206` with the right `Content-Range` for a ranged `GET` — the
three things Riptide's probe and its segment workers need in order to be tested for real.

`--slow` dribbles the body out a chunk at a time, so a transfer is still running when something
else asks the daemon to stop. That is what makes it possible to check a shutdown mid-download
against a process outside the test harness.

`tests/segmented.rs` has its own in-process server for the Rust tests; this one exists for
`scripts/check-headless.sh`, which drives the *binary* from a shell.
"""
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ARGS = [a for a in sys.argv[1:] if not a.startswith("--")]
SLOW = "--slow" in sys.argv
PORT = int(ARGS[0])
ROOT = os.path.abspath(ARGS[1] if len(ARGS) > 1 else ".")
DRIBBLE = 64 * 1024  # bytes per write when --slow
PAUSE = 0.05  # seconds between writes when --slow


class Origin(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # every request would drown the script's own output
        pass

    def local_path(self):
        rel = self.path.split("?")[0].lstrip("/")
        return os.path.join(ROOT, rel) if rel else None

    def size_of(self, path):
        try:
            return os.path.getsize(path)
        except OSError:
            return None

    def reply(self, code, length, headers, body=b"", dribble=False):
        self.send_response(code)
        self.send_header("Content-Length", str(length))
        for key, value in headers:
            self.send_header(key, value)
        self.end_headers()
        if not body:
            return
        if not dribble:
            self.wfile.write(body)
            return
        # A client that gives up (a cancelled job) leaves this writing to a closed socket.
        # That is not this server's problem; it just stops.
        try:
            for start in range(0, len(body), DRIBBLE):
                self.wfile.write(body[start : start + DRIBBLE])
                self.wfile.flush()
                time.sleep(PAUSE)
        except OSError:
            return

    def do_HEAD(self):
        path = self.local_path()
        size = self.size_of(path) if path else None
        if size is None:
            return self.reply(404, 0, [])
        self.reply(
            200,
            size,
            [("Accept-Ranges", "bytes"), ("Content-Type", "application/octet-stream")],
        )

    def do_GET(self):
        path = self.local_path()
        size = self.size_of(path) if path else None
        if size is None:
            return self.reply(404, 0, [])
        start, end = 0, size - 1
        ranged = self.headers.get("Range")
        if ranged and ranged.startswith("bytes="):
            first, _, last = ranged[len("bytes=") :].split(",")[0].partition("-")
            start = int(first) if first else 0
            end = min(int(last) if last else size - 1, size - 1)
        if start > end or start >= size:
            return self.reply(416, 0, [("Content-Range", f"bytes */{size}")])
        length = end - start + 1
        with open(path, "rb") as f:
            f.seek(start)
            body = f.read(length)
        headers = [("Accept-Ranges", "bytes"), ("Content-Type", "application/octet-stream")]
        if ranged:
            headers.append(("Content-Range", f"bytes {start}-{end}/{size}"))
        self.reply(206 if ranged else 200, length, headers, body, dribble=SLOW and ranged)


if __name__ == "__main__":
    print(f"serving {ROOT} on 127.0.0.1:{PORT}", flush=True)
    ThreadingHTTPServer(("127.0.0.1", PORT), Origin).serve_forever()
