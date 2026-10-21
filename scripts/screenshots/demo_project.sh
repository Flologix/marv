#!/bin/sh
# Writes the demo project (a tiny todo API in Python) to $1 and commits it.
set -eu
dir=$1
mkdir -p "$dir/tests"
cat >"$dir/app.py" <<'EOF'
"""A tiny todo API: route() is the whole app, serve() puts it on a port."""

import json
from http.server import BaseHTTPRequestHandler, HTTPServer

TODOS = {1: {"id": 1, "title": "Write the README", "done": False}}


def route(method, path):
    if method == "GET" and path == "/todos":
        return 200, list(TODOS.values())
    if method == "GET" and path.startswith("/todos/"):
        todo = TODOS.get(int(path.rsplit("/", 1)[1]))
        return (200, todo) if todo else (404, {"error": "not found"})
    return 404, {"error": "not found"}


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        status, body = route("GET", self.path)
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(data)


def serve(port=8000):
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
EOF
cat >"$dir/tests/__init__.py" <<'EOF'
EOF
cat >"$dir/tests/test_app.py" <<'EOF'
import unittest

from app import route


class TodosTest(unittest.TestCase):
    def test_lists_todos(self):
        status, body = route("GET", "/todos")
        self.assertEqual(status, 200)
        self.assertEqual(body[0]["title"], "Write the README")

    def test_gets_one(self):
        self.assertEqual(route("GET", "/todos/1")[0], 200)

    def test_unknown_is_404(self):
        self.assertEqual(route("GET", "/todos/9")[0], 404)


if __name__ == "__main__":
    unittest.main()
EOF
cat >"$dir/AGENTS.md" <<'EOF'
# todo-api

Run the tests with `python3 -m unittest -q`. Keep routes in `route()`.
EOF
cat >"$dir/README.md" <<'EOF'
# todo-api

A tiny todo API, stdlib only.
EOF
cd "$dir"
git init -q -b main
git add -A
git -c user.name=Demo -c user.email=demo@example.com commit -q -m "Initial todo API"
