#!/usr/bin/env python3
"""A tiny OpenAI-compatible stub for README screenshots: no real model, no key, no cost.

Serves `GET /v1/models` (one model, with a price, so the status bar has something to show) and
`POST /v1/chat/completions` as an SSE stream. The reply is picked by how many assistant messages
follow the last user message, so extra requests (a suggestion, a retry) can't shift the script.

    python3 stub_provider.py 8765
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = "z-ai/glm-5.3-flash"

APP_OLD = '''    if method == "GET" and path == "/todos":
        return 200, list(TODOS.values())
'''
APP_NEW = '''    if method == "GET" and path == "/health":
        return 200, {"status": "ok", "todos": len(TODOS)}
    if method == "GET" and path == "/todos":
        return 200, list(TODOS.values())
'''
TEST_OLD = '''if __name__ == "__main__":'''
TEST_NEW = '''class HealthTest(unittest.TestCase):
    def test_health_reports_ok(self):
        status, body = route("GET", "/health")
        self.assertEqual(status, 200)
        self.assertEqual(body["status"], "ok")


if __name__ == "__main__":'''

# One entry per step of the turn: (reasoning, text, [(tool name, arguments)]).
STEPS = [
    (
        "The user wants a health endpoint. I should look at how routes are written and how the tests call them.",
        "I'll look at the routes and the tests first.",
        [("read_file", {"path": "app.py"}), ("read_file", {"path": "tests/test_app.py"})],
    ),
    (
        "Routes are plain ifs in route(); tests call route() directly. Add the route and a test.",
        "",
        [
            ("edit_file", {"path": "app.py", "old_string": APP_OLD, "new_string": APP_NEW}),
            ("edit_file", {"path": "tests/test_app.py", "old_string": TEST_OLD, "new_string": TEST_NEW}),
        ],
    ),
    ("", "", [("bash", {"command": "python3 -m unittest -q"})]),
    (
        "",
        "All 4 tests pass. Committing the change.",
        [("bash", {"command": 'git add app.py tests/test_app.py && git commit -m "Add /health endpoint"', "git_write": True})],
    ),
    (
        "",
        "Done. `GET /health` returns `{\"status\": \"ok\", \"todos\": <count>}`, with a test, committed as "
        "\"Add /health endpoint\".",
        [],
    ),
]


def chunk(delta, finish=None):
    return {"id": "chatcmpl-stub", "object": "chat.completion.chunk", "model": MODEL,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


def pieces(text, size=12):
    return [text[i:i + size] for i in range(0, len(text), size)]


def reply_events(body):
    messages = body.get("messages", [])
    if not body.get("tools"):  # a side request (suggestion, title): a short answer
        return [chunk({"role": "assistant", "content": "Thanks!"}), chunk({}, "stop")]
    last_user = max((i for i, m in enumerate(messages) if m.get("role") == "user"), default=0)
    step = sum(1 for m in messages[last_user:] if m.get("role") == "assistant")
    reasoning, text, calls = STEPS[min(step, len(STEPS) - 1)]
    events = [chunk({"role": "assistant", "reasoning": p}) for p in pieces(reasoning)]
    events += [chunk({"role": "assistant", "content": p}) for p in pieces(text)]
    for i, (name, args) in enumerate(calls):
        events.append(chunk({"tool_calls": [{"index": i, "id": f"call_{step}_{i}", "type": "function",
                                             "function": {"name": name, "arguments": ""}}]}))
        events.append(chunk({"tool_calls": [{"index": i, "function": {"arguments": json.dumps(args)}}]}))
    events.append(chunk({}, "tool_calls" if calls else "stop"))
    prompt = 9000 + 1200 * step
    events.append({"id": "chatcmpl-stub", "object": "chat.completion.chunk", "model": MODEL, "choices": [],
                   "usage": {"prompt_tokens": prompt, "completion_tokens": 180, "total_tokens": prompt + 180,
                             "prompt_tokens_details": {"cached_tokens": prompt - 1500}, "cost": 0.0011}})
    return events


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def send_json(self, status, payload):
        data = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.split("?")[0].endswith("/models"):
            self.send_json(200, {"data": [{
                "id": MODEL, "context_length": 200000,
                "pricing": {"prompt": "0.0000003", "completion": "0.0000012", "input_cache_read": "0.00000006"},
                "supported_parameters": ["tools", "reasoning"], "reasoning": {"mandatory": True},
                "top_provider": {"max_completion_tokens": 32000},
            }]})
        else:
            self.send_json(404, {"error": {"message": "stub: not found"}})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        if not self.path.split("?")[0].endswith("/chat/completions"):
            return self.send_json(404, {"error": {"message": "stub: not found"}})
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("connection", "close")
        self.end_headers()
        for event in reply_events(body):
            self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
