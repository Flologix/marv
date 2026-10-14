//! marv-tui: Marv's terminal UI on ratatui + crossterm.
//!
//! Ports `src/app.tsx`, `src/ui/*.tsx`, `src/mouse.ts`, `src/selection.ts`, `src/clipboard.ts` and
//! `src/render-options.ts` from Marv (TypeScript, Ink/React). One Rust module per TS file; the UI
//! components live under [`ui`] like they do under `src/ui/`.
//!
//! The crate never imports the engine. Its input boundary is [`view`]: the engine (marv-session)
//! implements [`view::SessionPort`] and sends [`view::UiEvent`]s; the TUI only renders them.
//!
//! **Security:** model and tool text never reaches the terminal without [`marv_core::printable`].
//! Every render function that draws such text takes it raw and cleans it itself, at render time,
//! so the conversation the model sees is untouched (`docs/SECURITY.md`).
//!
//! See `docs/subsystems/tui.md` for the file map, status and port notes.

pub mod app;
pub mod clipboard;
pub mod mouse;
pub mod render_options;
pub mod selection;
pub mod ui;
pub mod view;

pub use app::App;
pub use view::{SessionPort, UiEvent};
