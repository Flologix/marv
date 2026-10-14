//! Mouse support. Ports `src/mouse.ts` (tests: `tests/mouse.test.ts`).
//!
//! Marv turns on the terminal's mouse reporting itself:
//! - 1002 = report presses, releases, and motion while a button is held (drags); the wheel counts
//!   as buttons 64/65.
//! - 1006 = SGR encoding: `ESC [ < button ; column ; row M` (press/motion) or `m` (release).
//!
//! Mouse mode is terminal-wide, so it's turned off on every exit path (`render_options`).
//!
//! What changes in Rust: Ink only understood keys, so Marv wrapped `stdin.read()` to cut mouse
//! codes out of each chunk before Ink saw them (and held a code split across reads, or it would be
//! lost or typed into the prompt). crossterm parses SGR mouse codes itself, including split ones,
//! so that wrapper (`filterMouseInput`, `extractMouse`) has no Rust counterpart; this module maps
//! crossterm's events onto Marv's smaller [`MouseEvent`] (wheel notches and the left button only;
//! middle/right are ignored, modifiers stripped). The SGR cases in `mouse.test.ts` become tests of
//! [`from_crossterm`].

use crossterm::event::{MouseButton, MouseEventKind};

/// Turns on 1002 + 1006 (written raw by `render_options::TerminalGuard`; see why there).
pub const MOUSE_ON: &str = "\x1b[?1002h\x1b[?1006h";
/// Turns them off again, in reverse order.
pub const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l";

/// How many transcript rows one wheel notch scrolls (Marv's `WHEEL_ROWS`, in ScrollView).
pub const WHEEL_ROWS: i32 = 3;

/// Cells are 0-based: `x` is the column, `y` the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    /// One wheel notch: -1 up, +1 down.
    Scroll {
        step: i8,
    },
    Press {
        x: u16,
        y: u16,
    },
    Drag {
        x: u16,
        y: u16,
    },
    Release {
        x: u16,
        y: u16,
    },
}

/// Marv's view of a crossterm mouse event: wheel notches and the left button; None for the rest
/// (middle/right buttons, plain motion, horizontal wheel). crossterm's cells are already 0-based
/// (Marv subtracted 1 from the SGR column and row), and modifiers are ignored.
pub fn from_crossterm(event: crossterm::event::MouseEvent) -> Option<MouseEvent> {
    let (x, y) = (event.column, event.row);
    match event.kind {
        MouseEventKind::ScrollUp => Some(MouseEvent::Scroll { step: -1 }),
        MouseEventKind::ScrollDown => Some(MouseEvent::Scroll { step: 1 }),
        MouseEventKind::Down(MouseButton::Left) => Some(MouseEvent::Press { x, y }),
        MouseEventKind::Drag(MouseButton::Left) => Some(MouseEvent::Drag { x, y }),
        MouseEventKind::Up(MouseButton::Left) => Some(MouseEvent::Release { x, y }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseEvent as Raw};

    fn raw(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> Raw {
        Raw { kind, column, row, modifiers }
    }

    #[test]
    fn turns_wheel_events_into_scroll_steps_negative_is_up() {
        let none = KeyModifiers::NONE;
        let got: Vec<_> = [MouseEventKind::ScrollUp, MouseEventKind::ScrollUp, MouseEventKind::ScrollDown]
            .into_iter()
            .filter_map(|k| from_crossterm(raw(k, 9, 4, none)))
            .collect();
        assert_eq!(got, [MouseEvent::Scroll { step: -1 }, MouseEvent::Scroll { step: -1 }, MouseEvent::Scroll { step: 1 }]);
    }

    #[test]
    fn reports_left_button_press_drag_and_release_with_0_based_cells() {
        // SGR "\x1b[<0;3;4M\x1b[<32;7;4M\x1b[<0;9;5m": crossterm already makes these 0-based.
        let none = KeyModifiers::NONE;
        let got: Vec<_> = [
            raw(MouseEventKind::Down(MouseButton::Left), 2, 3, none),
            raw(MouseEventKind::Drag(MouseButton::Left), 6, 3, none),
            raw(MouseEventKind::Up(MouseButton::Left), 8, 4, none),
        ]
        .into_iter()
        .filter_map(from_crossterm)
        .collect();
        assert_eq!(got, [MouseEvent::Press { x: 2, y: 3 }, MouseEvent::Drag { x: 6, y: 3 }, MouseEvent::Release { x: 8, y: 4 }]);
    }

    #[test]
    fn ignores_middle_and_right_buttons_and_plain_motion() {
        let none = KeyModifiers::NONE;
        for kind in [
            MouseEventKind::Down(MouseButton::Middle),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Right),
            MouseEventKind::Moved,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert_eq!(from_crossterm(raw(kind, 2, 3, none)), None, "{kind:?}");
        }
    }

    #[test]
    fn still_recognizes_events_with_modifier_keys_held() {
        // SGR 80 = wheel up + ctrl; 16 = left press + ctrl.
        let ctrl = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        assert_eq!(from_crossterm(raw(MouseEventKind::ScrollUp, 0, 0, ctrl)), Some(MouseEvent::Scroll { step: -1 }));
        assert_eq!(
            from_crossterm(raw(MouseEventKind::Down(MouseButton::Left), 0, 0, ctrl)),
            Some(MouseEvent::Press { x: 0, y: 0 })
        );
    }
}
