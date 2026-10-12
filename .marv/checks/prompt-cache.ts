// glob: src/**/*.ts, src/**/*.tsx
//
// The system prompt is built once per conversation (CLAUDE.md, "Prompt cache"): MarvSession.buildSystem() runs in the
// constructor and in clear() only, and nothing else assigns this.system. Rebuilding it anywhere else changes the
// prompt's prefix mid-conversation, and every request after it misses the provider's cache.
//
// A check for Marv's own repository (src/checks): prints one JSON line per violation, {file, line, message, why}.
const ALLOWED = new Set(["constructor", "clear"]);
const MEMBER = /^  (?:(?:private|public|protected|static|async|readonly|override|get|set)\s+)*#?(\w+)\s*(?:[(<]|=)/;
const WHY =
  "The system prompt is built once per conversation, in MarvSession's constructor and clear() (CLAUDE.md, Prompt cache): " +
  "rebuilding it elsewhere changes the prompt's prefix, and every request after that misses the provider's cache.";

for (const file of new Bun.Glob("src/**/*.{ts,tsx}").scanSync({ onlyFiles: true })) {
  const lines = (await Bun.file(file).text()).split("\n");
  let member = "";
  lines.forEach((text, i) => {
    const declared = MEMBER.exec(text);
    if (declared) member = declared[1]!;
    const builds = /\bbuildSystem\s*\(/.test(text) && !(declared && member === "buildSystem");
    const assigns = file === "src/session.ts" && /\bthis\.system\s*=[^=]/.test(text);
    if (!builds && !assigns) return;
    if (file === "src/session.ts" && ALLOWED.has(member)) return;
    const what = builds ? "buildSystem() called" : "this.system assigned";
    const where = file === "src/session.ts" ? `in ${member || "the module"}()` : "outside MarvSession";
    console.log(JSON.stringify({ file, line: i + 1, message: `${what} ${where}: the system prompt is built only in the constructor and clear()`, why: WHY }));
  });
}
