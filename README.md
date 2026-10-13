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

## Install

A tagged release has static binaries for Linux x86_64 and aarch64 (no macOS build yet). The installer downloads the one for your machine, checks its SHA-256, and puts it at `~/.local/bin/marv`:

```sh
curl -fsSL https://github.com/FlologixAI/marv-rs/releases/latest/download/install.sh | sh
sh install.sh --prefix /opt/marv       # into /opt/marv/bin instead
sh install.sh --version v0.1.0         # a given release
sh install.sh --uninstall              # remove it, and move marv-ts back to marv
```

**TS Marv becomes `marv-ts`.** TS Marv's launcher is also called `marv`. If the installer finds a `marv` that isn't marv-rs in the same folder, it moves it to `marv-ts` and says so, so `marv-ts` still runs the TypeScript Marv. `--uninstall` moves it back. Running the installer again is safe: it replaces marv-rs and leaves `marv-ts` alone.

### From source

```sh
cargo install --path crates/marv-cli --root ~/.local   # builds ~/.local/bin/marv
```

**Caution:** `cargo install` overwrites `~/.local/bin/marv` without asking, which is TS Marv's launcher if you have one. Move it first (`mv ~/.local/bin/marv ~/.local/bin/marv-ts`), or build and let the installer do the switch:

```sh
cargo build --release -p marv-cli
sh install.sh --from target/release/marv
```
