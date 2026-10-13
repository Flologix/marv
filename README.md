# marv-rs

```
 ●       ●
  ▀▄   ▄▀
 ▄███████▄
▄██ ███ ██▄
 ███▀▀▀███
 ▀███████▀
  ▀▀   ▀▀
```

Marv, rebuilt in Rust. Marv is a terminal coding agent that runs every command in a sandbox. marv-rs adds a **mesh**: wired, approved lines of communication between sandboxed agents, including agents running in Herdr or tmux panes.

**Status: skeleton.** The crates have the shape of the TypeScript Marv (`FlologixAI/marv`): types, constants, signatures and module docs. Bodies are `todo!("port: …")`. To see what's left:

```sh
grep -rn 'todo!("port' crates | wc -l
```

## Read first
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): crates, runtime shape, conventions.
- [docs/SECURITY.md](docs/SECURITY.md): the sandbox, protected paths, provenance and taint. These are requirements from day one.
- [docs/MESH.md](docs/MESH.md): plugs, receptacles and the switchboard.
- [docs/PORTING.md](docs/PORTING.md): every Marv TypeScript file and its Rust home, plus the build order.
- [AGENTS.md](AGENTS.md): how to work in this repo, for people and agents.

## Build

```sh
cargo check
cargo test
cargo run -p marv-cli   # the `marv` binary (once ported)
```

The toolchain is stable Rust (1.99+), installed with `mise use rust@stable`.
