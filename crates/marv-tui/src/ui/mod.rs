//! The UI components: one module per file in Marv's `src/ui/`.
//!
//! In Ink each was a React component; here each is a plain function or a ratatui `Widget` that
//! turns view-model state ([`crate::view`]) into lines or cells. Nothing here owns I/O or timers:
//! the [`crate::app`] event loop owns the clock and asks components for their next deadline.

pub mod agent_view;
pub mod approval;
pub mod command_palette;
pub mod diff_view;
pub mod markdown;
pub mod martian;
pub mod message_view;
pub mod model_picker;
pub mod notice_view;
pub mod plan_view;
pub mod prompt_input;
pub mod radio_view;
pub mod scroll_view;
pub mod select;
pub mod session_picker;
pub mod setup;
pub mod splash;
pub mod status_bar;
pub mod theme;
pub mod thinking_view;
pub mod transcript;
pub mod welcome;
