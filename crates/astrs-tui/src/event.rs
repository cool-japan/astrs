//! The input/tick source: a dedicated background thread blocking on
//! [`crossterm::event::poll`] so the render loop never busy-polls, feeding
//! a plain [`std::sync::mpsc`] channel of [`TuiEvent`]s.
//!
//! # Why a thread and not `crossterm::event::EventStream`
//!
//! `EventStream` needs crossterm's `event-stream` cargo feature, which the
//! workspace's `crossterm` pin does not enable (and adding it is a root
//! `Cargo.toml` edit best avoided while other waves are touching that
//! file concurrently). A blocking `poll(timeout)` on its own thread needs
//! no extra feature, costs one thread for the process's lifetime, and —
//! because [`EventLoop::recv`] is the only thing tests ever need to
//! substitute — keeps the actually-interesting state machine
//! ([`crate::app::handle_key`]) completely free of any dependency on this
//! thread existing at all.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEvent};

/// One input or timing event the render loop reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiEvent {
    /// A key was pressed.
    Key(KeyEvent),
    /// The terminal was resized to `(columns, rows)`.
    Resize(u16, u16),
    /// One redraw tick elapsed with no input — the render loop's cue to
    /// call [`crate::ClusterView::refresh`].
    Tick,
}

/// The background input/tick source.
///
/// Dropping this value drops the channel's receiving half, which makes the
/// background thread's next send fail and the thread exit on its own —
/// there is no separate shutdown signal to send.
#[derive(Debug)]
pub struct EventLoop {
    receiver: mpsc::Receiver<TuiEvent>,
}

impl EventLoop {
    /// Spawns the background thread and returns the event source.
    ///
    /// # Errors
    ///
    /// Whatever [`std::thread::Builder::spawn`] returns — this is the one
    /// path in this crate that can fail for reasons outside its control
    /// (the OS refusing to hand out one more thread), so it is propagated
    /// rather than assumed away.
    pub fn spawn(tick_rate: Duration) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("astrs-tui-input".to_owned())
            .spawn(move || input_loop(&sender, tick_rate))?;
        Ok(Self { receiver })
    }

    /// Blocks for the next event. Returns `None` once the background
    /// thread has exited (which only happens if `poll`/`read` themselves
    /// start failing — an unusable terminal, not a normal shutdown path;
    /// normal shutdown is the render loop simply stopping its own call to
    /// this method).
    pub fn recv(&self) -> Option<TuiEvent> {
        self.receiver.recv().ok()
    }
}

/// The thread body: alternates polling for input with a timeout bounded by
/// the tick rate, and sending a [`TuiEvent::Tick`] whenever that deadline
/// passes with nothing typed.
fn input_loop(sender: &mpsc::Sender<TuiEvent>, tick_rate: Duration) {
    let mut last_tick = Instant::now();
    loop {
        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        match event::poll(timeout) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    if sender.send(TuiEvent::Key(key)).is_err() {
                        return;
                    }
                }
                Ok(Event::Resize(columns, rows)) => {
                    if sender.send(TuiEvent::Resize(columns, rows)).is_err() {
                        return;
                    }
                }
                // Mouse/focus/paste events: not part of this tab set;
                // dropped rather than queued.
                Ok(_) => {}
                Err(_) => return,
            },
            Ok(false) => {}
            Err(_) => return,
        }
        if last_tick.elapsed() >= tick_rate {
            if sender.send(TuiEvent::Tick).is_err() {
                return;
            }
            last_tick = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn tui_events_compare_and_debug_print() {
        assert_eq!(TuiEvent::Resize(80, 24), TuiEvent::Resize(80, 24));
        assert_ne!(TuiEvent::Tick, TuiEvent::Resize(80, 24));
        assert!(format!("{:?}", TuiEvent::Tick).contains("Tick"));
    }

    #[test]
    fn an_event_loop_spawns_and_can_be_dropped_cleanly() {
        // This exercises the real background thread against the test
        // process's actual (non-interactive) stdin; it must spawn without
        // error and must not hang or panic when dropped immediately.
        let events = EventLoop::spawn(Duration::from_millis(5));
        assert!(events.is_ok());
    }
}
