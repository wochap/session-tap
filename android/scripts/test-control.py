#!/usr/bin/env python3
"""HTTP bridge that lets instrumented tests drive test-hub.sh from the emulator.

Run inside `nix develop .#android-emulator` after `test-hub.sh start` (and
`TEST_HUB=2 test-hub.sh start` for two-hub tests):

    android/scripts/test-control.py            # serves 127.0.0.1:8930 (emulator: 10.0.2.2:8930)

  GET /run?hub=N&arg=..&arg=..   run `TEST_HUB=N test-hub.sh <args>`, answer its stdout
  GET /bg?hub=N&arg=..           same, in the background (for `answer y|n`)
  GET /listen?hub=N              first snapshot line of `sessiontap-hub listen`
"""
import os
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "test-hub.sh")
PORT = int(os.environ.get("TEST_CONTROL_PORT", "8930"))


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        url = urlparse(self.path)
        q = parse_qs(url.query)
        env = dict(os.environ, TEST_HUB=q.get("hub", ["1"])[0], HUB_BIN_PREBUILT="1")
        args = q.get("arg", [])
        code, out = 200, b""
        if url.path == "/run":
            p = subprocess.run([SCRIPT, *args], env=env, capture_output=True, timeout=120)
            code, out = (200 if p.returncode == 0 else 500), p.stdout + p.stderr
        elif url.path == "/bg":
            subprocess.Popen([SCRIPT, *args], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             start_new_session=True)
        elif url.path == "/listen":
            p = subprocess.Popen([SCRIPT, "hub", "listen"], env=env, stdout=subprocess.PIPE)
            out = p.stdout.readline()
            p.kill()
        else:
            code = 404
        self.send_response(code)
        self.send_header("content-type", "text/plain")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def log_message(self, fmt, *a):
        sys.stderr.write("test-control: " + fmt % a + "\n")


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
