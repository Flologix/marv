#!/usr/bin/env python3
"""A one-turn OpenAI-compatible model for packaging/smoke.sh: no network, no key, no cost.

    python3 packaging/stub_provider.py <port file> <bash command>

It binds 127.0.0.1 on a free port and writes the port to <port file>. `POST .../chat/completions` streams one `bash`
tool call running <bash command> while the conversation has no tool result yet, and then a closing text that quotes
the tool result back (so the smoke test can read what the command printed in `marv -p`'s output). `GET .../models`
lists nothing; anything else is a 404. It serves until it is killed.

scripts/screenshots/stub_provider.py is a different stub: a fixed multi-step demo turn on a fixed port, for the
README's screenshots. This one takes its command and picks a free port, so it can run on any CI runner.
"""

import json
import socketserver
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

COMMAND = sys.argv[2]
MODEL = "smoke/stub"


def chunk(delta, finish=None):
    return {
        "id": "chatcmpl-smoke",
        "object": "chat.completion.chunk",
        "model": MODEL,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }


def reply(messages):
    results = [m for m in messages if m.get("role") == "tool"]
    if not results:
        call = {
            "index": 0,
            "id": "call_smoke",
            "type": "function",
            "function": {"name": "bash", "arguments": json.dumps({"command": COMMAND})},
        }
        return [chunk({"role": "assistant", "tool_calls": [call]}), chunk({}, "tool_calls")]
    content = results[-1].get("content")
    if isinstance(content, list):
        content = "".join(p.get("text", "") for p in content if isinstance(p, dict))
    return [chunk({"role": "assistant", "content": "tool said: " + str(content)}), chunk({}, "stop")]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send(self, status, body, kind="application/json"):
        data = body.encode()
        self.send_response(status)
        self.send_header("content-type", kind)
        self.send_header("content-length", str(len(data)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(data)
        self.close_connection = True

    def do_GET(self):
        if self.path.split("?")[0].endswith("/models"):
            self.send(200, json.dumps({"data": []}))
        else:
            self.send(404, json.dumps({"error": {"message": "stub: not found"}}))

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length") or 0))
        if not self.path.split("?")[0].endswith("/chat/completions"):
            self.send(404, json.dumps({"error": {"message": "stub: not found"}}))
            return
        events = reply(json.loads(body).get("messages", []))
        usage = {"id": "chatcmpl-smoke", "object": "chat.completion.chunk", "model": MODEL, "choices": [],
                 "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}
        stream = "".join(f"data: {json.dumps(e)}\n\n" for e in events + [usage]) + "data: [DONE]\n\n"
        self.send(200, stream, "text/event-stream")


class QuickServer(ThreadingHTTPServer):
    """`HTTPServer.server_bind` looks up the host's fully qualified name, which took tens of seconds on the macOS 15
    runners; the stub only ever listens on 127.0.0.1, so it names itself that."""

    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


def main():
    server = QuickServer(("127.0.0.1", 0), Handler)
    # Written whole, then renamed into place: the smoke test reads the port as soon as the file isn't empty.
    tmp = sys.argv[1] + ".tmp"
    with open(tmp, "w") as f:
        f.write(str(server.server_address[1]))
    os.replace(tmp, sys.argv[1])
    print(f"stub: listening on 127.0.0.1:{server.server_address[1]}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
