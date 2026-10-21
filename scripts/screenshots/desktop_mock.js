// A stand-in for Marv desktop's Tauri backend, for the README screenshot of app/ in headless Chromium.
// capture-desktop.sh injects it before the app's own script: the page then believes it runs in Tauri, and every
// command answers with the demo data below (a Marv folder with a few projects, three chats, one waiting for an
// approval). Nothing runs: no model, no files, no network.
(() => {
  const root = "/home/demo/Projects";
  const space = { id: "s1", name: "Projects", root, appsOff: [] };
  const info = { root, rootName: "Projects", hasKey: true, provider: "openrouter", ready: true,
    offerZai: false, offerChatgpt: false, hasZaiKey: false, chatgptAccount: null, spaces: [space], active: "s1" };
  const now = Date.now() / 1000; // chats' times are in seconds
  const chats = [
    { id: "c1", folder: `${root}/todo-api`, title: "Add a /health endpoint", created: now - 4 * 60 },
    { id: "c2", folder: `${root}/todo-api`, title: "Why is the 404 test slow?", created: now - 26 * 3600 },
    { id: "c3", folder: `${root}/site`, title: "Fix the nav on small screens", created: now - 12 * 60 },
  ];
  const tree = {
    "": [
      { name: "notes", dir: true },
      { name: "site", dir: true },
      { name: "todo-api", dir: true },
    ],
    "todo-api": [
      { name: "tests", dir: true },
      { name: "AGENTS.md", dir: false },
      { name: "README.md", dir: false },
      { name: "app.py", dir: false },
    ],
  };
  const line = (kind, text) => ({ kind, text });
  const transcript = [
    { kind: "user", text: "add a /health endpoint with a test, then commit it" },
    { kind: "thinking", text: "Routes are plain ifs in route(); the tests call route() directly." },
    { kind: "text", text: "I'll look at the routes and the tests first." },
    { kind: "tool", id: "t1", tool: "read_file", label: "app.py", done: true, summary: "34 lines", result: "" },
    { kind: "tool", id: "t2", tool: "read_file", label: "tests/test_app.py", done: true, summary: "21 lines", result: "" },
    {
      kind: "tool", id: "t3", tool: "edit_file", label: "app.py", done: true, summary: "+2 −0",
      diff: {
        more: 0,
        lines: [
          line("ctx", "def route(method, path):"),
          line("add", '    if method == "GET" and path == "/health":'),
          line("add", '        return 200, {"status": "ok", "todos": len(TODOS)}'),
          line("ctx", '    if method == "GET" and path == "/todos":'),
          line("ctx", "        return 200, list(TODOS.values())"),
        ],
      },
    },
    { kind: "tool", id: "t4", tool: "edit_file", label: "tests/test_app.py", done: true, summary: "+7 −0", result: "" },
    {
      kind: "tool", id: "t5", tool: "bash", label: "python3 -m unittest -q", done: true, summary: "exit 0",
      result: "----------------------------------------------------------------------\nRan 4 tests in 0.001s\n\nOK",
    },
    { kind: "text", text: "All 4 tests pass. Committing the change." },
    { kind: "tool", id: "t6", tool: "bash", label: 'git add app.py tests/test_app.py && git commit -m "Add /health endpoint"', done: false },
    {
      kind: "approval",
      request: {
        id: 1, tool: "bash", label: "git commit", title: "Run a command",
        command: 'git add app.py tests/test_app.py && git commit -m "Add /health endpoint"',
        note: "sandboxed · no network · changes the git repository", scope: "this command", once: false, reason: true,
      },
    },
  ];
  try {
    localStorage.clear();
    localStorage.setItem("marv-folder", JSON.stringify("todo-api"));
    localStorage.setItem("marv-transcript-c1", JSON.stringify(transcript));
    localStorage.setItem("marv-seq-c1", "40");
    localStorage.setItem("marv-epoch-c1", "demo");
    const model = { id: "z-ai/glm-5.3-flash", name: "Z.ai: GLM 5.3 Flash", context: 200000, input: 0.3, output: 1.2 };
    localStorage.setItem("marv-models-2", JSON.stringify({ at: Date.now(), models: [model] }));
  } catch {}

  const callbacks = {};
  let next = 1;
  const send = (channel, index, message) => callbacks[channel.id]?.({ index, message });
  const commands = {
    app_info: () => info,
    take_launch: () => null,
    list_dir: ({ rel }) => tree[rel] ?? [],
    list_chats: ({ folder }) => (folder === undefined ? chats : chats.filter((c) => c.folder === `${root}/${folder}`)),
    live_chats: () => [
      { id: "c1", busy: true, waiting: 1 },
      { id: "c3", busy: true, waiting: 1 },
    ],
    mcp_servers: () => [],
    plan_models: () => [],
    open_chat: ({ onEvent }) => {
      setTimeout(() => send(onEvent, 0, { type: "ready", epoch: "demo", busy: true, mesh: "todo-api-3f9a", commands: [] }), 10);
      return null;
    },
    "plugin:event|listen": () => next++,
  };
  window.__TAURI_INTERNALS__ = {
    metadata: { currentWindow: { label: "main" }, currentWebview: { windowLabel: "main", label: "main" } },
    transformCallback: (cb) => {
      const id = next++;
      callbacks[id] = cb;
      return id;
    },
    unregisterCallback: (id) => delete callbacks[id],
    invoke: async (cmd, args = {}) => (commands[cmd] ? commands[cmd](args) : null),
    convertFileSrc: (p) => p,
  };

  // A still picture: no fade-ins half done when the screenshot is taken.
  document.addEventListener("DOMContentLoaded", () => {
    const style = document.createElement("style");
    style.textContent = "*, *::before, *::after { animation: none !important; transition: none !important; }";
    document.head.append(style);
  });

  // Open the demo chat and its first edit, as a person would have.
  const open = setInterval(() => {
    const row = [...document.querySelectorAll(".chat-row")].find((r) => r.textContent.includes("/health"));
    if (!row) return;
    row.click();
    clearInterval(open);
    const edit = setInterval(() => {
      const details = [...document.querySelectorAll("details.tool")].find((d) => d.textContent.includes("edit_fileapp.py"));
      if (!details) return;
      details.open = true;
      clearInterval(edit);
      setTimeout(() => document.querySelector("main.scroll")?.scrollTo(0, 1e6), 100);
    }, 50);
  }, 50);
})();
