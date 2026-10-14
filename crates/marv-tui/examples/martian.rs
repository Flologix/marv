//! The animated martian, full screen, until `q` (or Esc / ctrl+c).
//!
//! ```sh
//! cargo run -p marv-tui --example martian
//! ```
//!
//! A miniature of the App's event loop design (see `marv_tui::app`): the loop blocks in the kernel
//! until either input arrives or the martian's next frame is due, and draws only when the pose
//! changed or the terminal resized. Between frames it uses no CPU at all (check with `top`).
//! The terminal is restored on exit and on panic (`render_options::TerminalGuard`).

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use marv_tui::render_options::{RenderOptions, TerminalGuard};
use marv_tui::ui::martian::{self, MartianAnimation, MartianWidget};
use marv_tui::ui::theme;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

fn main() -> io::Result<()> {
    // No mouse here: nothing to click, and it keeps the terminal's own selection working.
    let _guard = TerminalGuard::enter(RenderOptions { alternate_screen: true, mouse: false })?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut animation = MartianAnimation::new(Instant::now());
    let mut dirty = true;

    loop {
        if dirty {
            let frame = animation.frame();
            terminal.draw(|f| {
                let area = f.area();
                let x = area.x + area.width.saturating_sub(martian::WIDTH) / 2;
                let y = area.y + area.height.saturating_sub(martian::HEIGHT + 2) / 2;
                f.render_widget(
                    MartianWidget::new(frame),
                    Rect::new(x, y, martian::WIDTH.min(area.width), martian::HEIGHT.min(area.height)),
                );
                let hint = Line::styled("q to quit", Style::new().fg(theme::DIM));
                let hy = y + martian::HEIGHT + 1;
                if hy < area.bottom() {
                    let hx = area.x + area.width.saturating_sub(hint.width() as u16) / 2;
                    f.render_widget(hint, Rect::new(hx, hy, area.right().saturating_sub(hx), 1));
                }
            })?;
            dirty = false;
        }

        // Sleep until the next frame is due, or input arrives, whichever is first.
        let timeout = animation.deadline().map_or(Duration::from_secs(3600), |at| at.saturating_duration_since(Instant::now()));
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let quit = matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        break;
                    }
                }
                Event::Resize(..) => {
                    terminal.autoresize()?;
                    dirty = true;
                }
                _ => {}
            }
        }
        dirty |= animation.tick(Instant::now());
    }
    Ok(())
}
