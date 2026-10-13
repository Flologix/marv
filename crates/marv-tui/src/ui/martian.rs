//! Marv's mascot: a sturdy little martian, drawn in solid blocks, with antennae, arms and feet.
//! Ports `src/ui/Martian.tsx` (fully) and its tests in `tests/martian.test.tsx`.
//!
//! - One colour (the theme accent); the eyes and mouth are cut out of the fill, so the terminal's
//!   background shows through. The widget therefore sets only the foreground, never a background.
//! - Every frame is the same 12×7 block (the raised arm uses the last column), so swapping frames
//!   never moves the layout.
//! - The animation is a [`MartianAnimation`] state machine driven by the app's clock: it plays
//!   [`GREETING`] once (a wave hello), then loops [`IDLE_LOOP`] forever (mostly still, with a blink,
//!   a look around, a wiggle, and the odd wave). It never sleeps or spins itself: it exposes its
//!   next [`deadline`](MartianAnimation::deadline) and the event loop wakes for it, so a still
//!   martian costs nothing between frames (Marv #68: Ink's timers + full re-renders cost 40–60% CPU).
//! - Difference from Marv: Marv re-armed a `setTimeout` after each render, so its steps drifted by
//!   the render time. Here each deadline is the previous one plus the step's ms (no drift), and a
//!   wake after a long stall (suspend, a blocked terminal) resynchronises instead of replaying every
//!   missed step.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;

use super::theme;

/// Width of every frame, in terminal columns.
pub const WIDTH: u16 = 12;
/// Height of every frame, in rows.
pub const HEIGHT: u16 = 7;

const ANTENNAE: [&str; 2] = [" ●       ●  ", "  ▀▄   ▄▀   "];
const HEAD: &str = " ▄███████▄  ";
const FACE: &str = "▄██ ███ ██▄ ";
const MOUTH: &str = " ███▀▀▀███  ";
const BODY: [&str; 2] = [" ▀███████▀  ", "  ▀▀   ▀▀   "];

/// A frame: seven rows, top to bottom.
pub type Frame = [&'static str; 7];

/// The poses. Names match Marv's `FRAMES` keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameName {
    Idle,
    /// Half-height eye slits.
    Blink,
    /// Eyes and mouth shift a column together.
    LookLeft,
    LookRight,
    /// Both antennae lean the same way; alternating the two is the wiggle.
    TiltLeft,
    TiltRight,
    /// The right arm raised; alternating the hand between ▞ (out) and ▌ (up) is the wave.
    WaveOut,
    WaveUp,
}

impl FrameName {
    pub const ALL: [FrameName; 8] = [
        FrameName::Idle,
        FrameName::Blink,
        FrameName::LookLeft,
        FrameName::LookRight,
        FrameName::TiltLeft,
        FrameName::TiltRight,
        FrameName::WaveOut,
        FrameName::WaveUp,
    ];

    /// The frame's rows (Marv's `FRAMES[name]`).
    pub const fn frame(self) -> &'static Frame {
        match self {
            FrameName::Idle => &IDLE,
            FrameName::Blink => &BLINK,
            FrameName::LookLeft => &LOOK_LEFT,
            FrameName::LookRight => &LOOK_RIGHT,
            FrameName::TiltLeft => &TILT_LEFT,
            FrameName::TiltRight => &TILT_RIGHT,
            FrameName::WaveOut => &WAVE_OUT,
            FrameName::WaveUp => &WAVE_UP,
        }
    }
}

const IDLE: Frame = [ANTENNAE[0], ANTENNAE[1], HEAD, FACE, MOUTH, BODY[0], BODY[1]];
const BLINK: Frame = [ANTENNAE[0], ANTENNAE[1], HEAD, "▄██▄███▄██▄ ", MOUTH, BODY[0], BODY[1]];
const LOOK_LEFT: Frame = [ANTENNAE[0], ANTENNAE[1], HEAD, "▄█ ███ ███▄ ", " ██▀▀▀████  ", BODY[0], BODY[1]];
const LOOK_RIGHT: Frame = [ANTENNAE[0], ANTENNAE[1], HEAD, "▄███ ███ █▄ ", " ████▀▀▀██  ", BODY[0], BODY[1]];
const TILT_LEFT: Frame = [" ●     ●    ", "  ▀▄    ▀▄  ", HEAD, FACE, MOUTH, BODY[0], BODY[1]];
const TILT_RIGHT: Frame = ["   ●     ●  ", "  ▄▀   ▄▀   ", HEAD, FACE, MOUTH, BODY[0], BODY[1]];
const WAVE_OUT: Frame = [ANTENNAE[0], ANTENNAE[1], " ▄███████▄ ▞", "▄██ ███ ███▘", MOUTH, BODY[0], BODY[1]];
const WAVE_UP: Frame = [ANTENNAE[0], ANTENNAE[1], " ▄███████▄ ▌", "▄██ ███ ███▘", MOUTH, BODY[0], BODY[1]];

/// One step of an animation script: show `frame` for `ms` milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub frame: FrameName,
    pub ms: u64,
}

const fn step(frame: FrameName, ms: u64) -> Step {
    Step { frame, ms }
}

use FrameName::*;

// Marv builds these with `repeat`/`wiggle`/`wave`; spelled out here so they can be consts.
// wave(n) = n × [waveOut 220, waveUp 220]; wiggle(n) = n × [tiltLeft 160, tiltRight 160].

/// Played once when the martian appears: a wave hello (`[...wave(3), idle 2500]`).
pub const GREETING: [Step; 7] = [
    step(WaveOut, 220),
    step(WaveUp, 220),
    step(WaveOut, 220),
    step(WaveUp, 220),
    step(WaveOut, 220),
    step(WaveUp, 220),
    step(Idle, 2500),
];

/// Then this loops forever: mostly still, with a blink, a look around, a wiggle, and the odd wave.
pub const IDLE_LOOP: [Step; 19] = [
    step(Blink, 150),
    step(Idle, 3500),
    step(LookLeft, 700),
    step(LookRight, 700),
    step(Idle, 3000),
    step(Blink, 150),
    step(Idle, 200),
    step(Blink, 150),
    step(Idle, 3500),
    // wiggle(2)
    step(TiltLeft, 160),
    step(TiltRight, 160),
    step(TiltLeft, 160),
    step(TiltRight, 160),
    step(Idle, 4000),
    // wave(2)
    step(WaveOut, 220),
    step(WaveUp, 220),
    step(WaveOut, 220),
    step(WaveUp, 220),
    step(Idle, 4000),
];

/// The step at position `n` of GREETING followed by IDLE_LOOP; past the greeting, it wraps around the loop.
pub fn step_at(n: usize) -> Step {
    if n < GREETING.len() { GREETING[n] } else { IDLE_LOOP[(n - GREETING.len()) % IDLE_LOOP.len()] }
}

/// Total length of one pass of [`IDLE_LOOP`].
pub fn idle_loop_duration() -> Duration {
    Duration::from_millis(IDLE_LOOP.iter().map(|s| s.ms).sum())
}

/// The martian's animation: which step it's on and when the next one is due.
///
/// Tick-based, no timers of its own: call [`tick`](Self::tick) whenever the event loop wakes and
/// sleep until [`deadline`](Self::deadline).
#[derive(Debug, Clone)]
pub struct MartianAnimation {
    step: usize,
    /// When the current step ends; `None` when animation is off (it holds still on `idle`).
    deadline: Option<Instant>,
}

impl MartianAnimation {
    /// Starts on the first greeting frame at `now`.
    pub fn new(now: Instant) -> Self {
        Self { step: 0, deadline: Some(now + Duration::from_millis(step_at(0).ms)) }
    }

    /// Animation off (Marv's `animate={false}`): always `idle`, never wakes the loop.
    pub fn still() -> Self {
        Self { step: 0, deadline: None }
    }

    pub fn animating(&self) -> bool {
        self.deadline.is_some()
    }

    /// Position in GREETING followed by IDLE_LOOP.
    pub fn step_index(&self) -> usize {
        self.step
    }

    /// The pose to draw now.
    pub fn frame(&self) -> FrameName {
        if self.animating() { step_at(self.step).frame } else { Idle }
    }

    /// When the frame next changes; the event loop sleeps until then (or another event).
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Advances past every step that has ended by `now`. Returns whether the frame changed, i.e.
    /// whether a redraw is needed.
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(mut deadline) = self.deadline else { return false };
        if now < deadline {
            return false;
        }
        let before = self.frame();
        // Woken long after the deadline (suspend, a stalled terminal): don't replay every missed
        // step, just carry on from the next one with a fresh clock.
        if now.duration_since(deadline) > idle_loop_duration() {
            self.step += 1;
            self.deadline = Some(now + Duration::from_millis(step_at(self.step).ms));
            return self.frame() != before;
        }
        while now >= deadline {
            self.step += 1;
            deadline += Duration::from_millis(step_at(self.step).ms);
        }
        self.deadline = Some(deadline);
        self.frame() != before
    }
}

/// One pose, drawn in the accent colour (Marv's `MartianFrame`). Clipped to the area it's given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MartianWidget {
    pub frame: FrameName,
}

impl MartianWidget {
    pub fn new(frame: FrameName) -> Self {
        Self { frame }
    }
}

impl Widget for MartianWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Foreground only: the cut-out eyes and mouth must show the terminal's own background.
        let style = Style::new().fg(theme::ACCENT);
        for (i, row) in self.frame.frame().iter().enumerate() {
            let y = area.y + i as u16;
            if i as u16 >= area.height {
                break;
            }
            buf.set_stringn(area.x, y, row, area.width as usize, style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn every_frame_is_12_by_7_so_the_banner_never_shifts() {
        for name in FrameName::ALL {
            let frame = name.frame();
            assert_eq!(frame.len(), HEIGHT as usize, "{name:?}");
            for row in frame {
                assert_eq!(row.width(), WIDTH as usize, "{name:?}: {row:?}");
            }
        }
    }

    #[test]
    fn frames_are_byte_exact_with_marv() {
        assert_eq!(
            IDLE.join("\n"),
            " ●       ●  \n  ▀▄   ▄▀   \n ▄███████▄  \n▄██ ███ ██▄ \n ███▀▀▀███  \n ▀███████▀  \n  ▀▀   ▀▀   "
        );
        assert_eq!(WAVE_UP[2], " ▄███████▄ ▌");
        assert_eq!(WAVE_OUT[3], "▄██ ███ ███▘");
        assert_eq!(TILT_RIGHT[1], "  ▄▀   ▄▀   ");
    }

    #[test]
    fn the_scripts_have_positive_durations() {
        for s in GREETING.iter().chain(IDLE_LOOP.iter()) {
            assert!(s.ms > 0);
        }
        assert_eq!(GREETING.iter().map(|s| s.ms).sum::<u64>(), 6 * 220 + 2500);
        assert_eq!(idle_loop_duration(), Duration::from_millis(150 + 3500 + 1400 + 3000 + 500 + 3500 + 640 + 4000 + 880 + 4000));
    }

    #[test]
    fn greeting_plays_once_then_the_loop_wraps() {
        assert_eq!(step_at(0), step(WaveOut, 220));
        assert_eq!(step_at(GREETING.len() - 1), step(Idle, 2500));
        assert_eq!(step_at(GREETING.len()), IDLE_LOOP[0]);
        assert_eq!(step_at(GREETING.len() + IDLE_LOOP.len()), IDLE_LOOP[0]);
        assert_eq!(step_at(GREETING.len() + IDLE_LOOP.len() + 3), IDLE_LOOP[3]);
    }

    #[test]
    fn starts_on_the_first_greeting_frame_and_then_moves() {
        let t0 = Instant::now();
        let mut m = MartianAnimation::new(t0);
        assert_eq!(m.frame(), WaveOut);
        assert_eq!(m.deadline(), Some(t0 + Duration::from_millis(220)));
        assert!(!m.tick(t0 + Duration::from_millis(219)));
        assert!(m.tick(t0 + Duration::from_millis(220)));
        assert_eq!(m.frame(), WaveUp);
        assert_eq!(m.deadline(), Some(t0 + Duration::from_millis(440)));
    }

    #[test]
    fn ticks_through_the_greeting_into_the_loop_and_around() {
        let t0 = Instant::now();
        let mut m = MartianAnimation::new(t0);
        let greeting: u64 = GREETING.iter().map(|s| s.ms).sum();
        m.tick(t0 + Duration::from_millis(greeting));
        assert_eq!(m.step_index(), GREETING.len());
        assert_eq!(m.frame(), Blink);
        let full = greeting + idle_loop_duration().as_millis() as u64;
        m.tick(t0 + Duration::from_millis(full));
        assert_eq!(m.step_index(), GREETING.len() + IDLE_LOOP.len());
        assert_eq!(m.frame(), IDLE_LOOP[0].frame);
        assert_eq!(m.deadline(), Some(t0 + Duration::from_millis(full + IDLE_LOOP[0].ms)));
    }

    #[test]
    fn a_long_stall_resyncs_instead_of_replaying() {
        let t0 = Instant::now();
        let mut m = MartianAnimation::new(t0);
        let late = t0 + Duration::from_secs(3600);
        m.tick(late);
        assert_eq!(m.step_index(), 1);
        assert!(m.deadline().unwrap() > late);
    }

    #[test]
    fn holds_still_when_animation_is_off() {
        let mut m = MartianAnimation::still();
        assert_eq!(m.frame(), Idle);
        assert_eq!(m.deadline(), None);
        assert!(!m.tick(Instant::now() + Duration::from_secs(10)));
    }

    #[test]
    fn drawn_in_a_single_colour_with_no_background() {
        for name in FrameName::ALL {
            let area = Rect::new(0, 0, WIDTH, HEIGHT);
            let mut buf = Buffer::empty(area);
            MartianWidget::new(name).render(area, &mut buf);
            for (i, row) in name.frame().iter().enumerate() {
                let drawn: String = (0..WIDTH).map(|x| buf[(x, i as u16)].symbol().to_string()).collect();
                assert_eq!(&drawn, row);
            }
            for cell in buf.content() {
                assert_eq!(cell.fg, theme::ACCENT);
                assert_eq!(cell.bg, Color::Reset);
            }
        }
    }

    #[test]
    fn clips_to_a_small_area() {
        let area = Rect::new(0, 0, 5, 3);
        let mut buf = Buffer::empty(area);
        MartianWidget::new(Idle).render(area, &mut buf);
        let row: String = (0..5).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(row, " ▄███");
    }
}
