//! A vertical list: ↑/↓ to move (wrapping), Enter to choose. Ports `src/ui/Select.tsx`.
//! The active row is `❯ ` in the accent; a hint follows ` · ` dimmed. Labels and hints go through
//! `printable()` (approval scopes can quote model text).

use crossterm::event::KeyEvent;
use ratatui::text::Line;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem<T> {
    pub value: T,
    pub label: String,
    pub hint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Select<T> {
    pub items: Vec<SelectItem<T>>,
    pub index: usize,
}

impl<T: Clone + PartialEq> Select<T> {
    /// Starts on `initial` if it's there, else the first item.
    pub fn new(items: Vec<SelectItem<T>>, initial: Option<&T>) -> Self {
        let index = initial.and_then(|v| items.iter().position(|i| &i.value == v)).unwrap_or(0);
        Self { items, index }
    }

    /// ↑/↓ move; Enter returns the chosen value.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<T> {
        let _ = key;
        todo!("port: Select's useInput")
    }

    pub fn render(&self) -> Vec<Line<'static>> {
        todo!("port: Select rows (printable on label and hint)")
    }
}
