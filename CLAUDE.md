# Working on marv-rs

marv-rs is a rebuild of Marv (TypeScript) in Rust. **The TypeScript repo is the spec.** Before porting a module:
1. Read its `//!` header. It names the Marv file it ports.
2. Read that file, its `*.test.ts`, and the matching bullet in Marv's `CLAUDE.md`.
3. Read `docs/subsystems/<crate>.md` for port notes.

## Rules
- **Keep the shape.** Use the same module boundaries, names (snake_case) and constants as Marv, unless a doc note says why not.
- **Port the comments that explain *why*.** They record bugs that were really hit (CRLF, Ollama reusing `call_0`, cut-off replies, the prompt cache).
- **Port the tests with the code.** A module isn't "ported" until its TypeScript tests have Rust equivalents.
- **Behaviour changes need a note.** Where marv-rs deliberately differs from Marv (the security fixes, the Sprint 5 items), the module doc says so and gives the Marv issue number.
- **Security rules in `docs/SECURITY.md` are not optional.** Each rule gets a guard test in `tests/guards/` that fails when the guard is removed.
- **Prompt cache:** history is append-only, the system prompt is byte-stable within a session, tool specs keep a stable order, and `ToolCall::arguments` is never re-serialized.
- **No `unsafe`**, except the one audited `sigaction` query in `crates/marv-cli/src/signals.rs` (`unsafe_code` is `deny`; a test keeps it the only one). Model or tool text never reaches the terminal without `printable()`.
- **Done means** `cargo check` and `cargo test` pass, there are no new warnings, the module's `docs/subsystems/*.md` row is updated, and `PORTING.md` is ticked.

## Build order
See `docs/PORTING.md`. In short:
1. core and provider
2. sandbox and tools
3. agent loop
4. session engine with headless `marv -p`
5. the oracle passes
6. TUI
7. MCP
8. mesh v1, then v2 (Herdr/tmux), then v3 (AgentMesh)

## Commands

```sh
eval "$(mise env -s bash)"            # if cargo isn't on PATH
cargo check -p <crate>
cargo test -p <crate>
cargo clippy --all-targets
grep -rn 'todo!("port' crates/<crate>  # what's left in a crate
```
