//! A queue you push to from callbacks and read as a stream. Ports Marv's `src/event-queue.ts`.
//!
//! It is how a session merges the events of a turn's main loop with its subagents' (callbacks that fire while the
//! main loop waits on them) into one ordered stream. One reader: a second would leave the first hanging, so it's
//! refused. No back-pressure: callbacks can't be told to wait, and the events are small.
//!
//! Port note: an unbounded tokio channel. Pushes after [`EventQueue::close`] are dropped (a subagent still winding
//! down after its turn ended), and the reader drains what was queued before it ends.

use std::sync::Mutex;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub struct EventQueue<T> {
    tx: Mutex<Option<UnboundedSender<T>>>,
    rx: Mutex<Option<UnboundedReceiver<T>>>,
}

impl<T> Default for EventQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> EventQueue<T> {
    pub fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self { tx: Mutex::new(Some(tx)), rx: Mutex::new(Some(rx)) }
    }

    /// Adds an item; dropped once the queue is closed (or its reader is gone).
    pub fn push(&self, item: T) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(item);
        }
    }

    /// No more items: the reader gets what's queued, then the stream ends.
    pub fn close(&self) {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// The one reader. A second call is refused, rather than silently stealing the first one's items.
    pub fn reader(&self) -> Result<UnboundedReceiver<T>, &'static str> {
        self.rx.lock().unwrap_or_else(|e| e.into_inner()).take().ok_or("EventQueue has one reader: it's already being read.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn drain<T>(mut rx: UnboundedReceiver<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let Some(item) = rx.recv().await {
            out.push(item);
        }
        out
    }

    #[tokio::test]
    async fn delivers_in_order_and_ends_once_closed_and_empty() {
        let q = EventQueue::new();
        q.push(1);
        q.push(2);
        let rx = q.reader().unwrap();
        q.push(3);
        q.close();
        assert_eq!(drain(rx).await, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn pushes_after_close_are_dropped() {
        let q = EventQueue::new();
        q.push(1);
        q.close();
        q.push(2);
        assert_eq!(drain(q.reader().unwrap()).await, vec![1]);
    }

    #[tokio::test]
    async fn closing_wakes_a_waiting_reader() {
        let q = std::sync::Arc::new(EventQueue::<u8>::new());
        let rx = q.reader().unwrap();
        let waiting = tokio::spawn(drain(rx));
        tokio::task::yield_now().await;
        q.close();
        assert!(waiting.await.unwrap().is_empty());
    }

    #[test]
    fn a_second_reader_is_refused() {
        let q = EventQueue::<u8>::new();
        let _first = q.reader().unwrap();
        assert_eq!(q.reader().unwrap_err(), "EventQueue has one reader: it's already being read.");
    }

    #[test]
    fn pushing_after_the_reader_left_doesnt_panic() {
        let q = EventQueue::new();
        drop(q.reader().unwrap());
        q.push(1);
        q.close();
    }
}
