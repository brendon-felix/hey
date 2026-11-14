/* -------------------------------------------------------------------------- */
/*                               repl/outbox.rs                               */
/* -------------------------------------------------------------------------- */

//! Messages sent while an answer is in progress, until it takes them: those
//! steering it, which the host hands the model at its next tool result, and
//! those queued, which each start an answer once the one before has ended.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::render::{self, palette};

#[derive(Debug, Default)]
struct Messages {
    steering: Vec<String>,
    queued: VecDeque<String>,
}

/// Shared with the prompt, which shows what waits above the input.
#[derive(Debug, Clone, Default)]
pub struct Outbox(Arc<Mutex<Messages>>);

impl Outbox {
    pub fn steering(&self, message: String) {
        self.lock().steering.push(message);
    }

    /// The model was handed `prompt`.
    pub fn steered(&self, prompt: &str) {
        let steering = &mut self.lock().steering;
        if let Some(at) = steering.iter().position(|message| message == prompt) {
            steering.remove(at);
        }
    }

    pub fn queue(&self, message: String) {
        self.lock().queued.push_back(message);
    }

    /// Queues `message` before the rest, to be sent first.
    pub fn queue_first(&self, message: String) {
        self.lock().queued.push_front(message);
    }

    pub fn next(&self) -> Option<String> {
        self.lock().queued.pop_front()
    }

    pub fn has_queued(&self) -> bool {
        !self.lock().queued.is_empty()
    }

    /// The messages steering, which the model will now not be handed, or
    /// which no longer concern this terminal.
    pub fn unsteered(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().steering)
    }

    /// The message queued last, taken back.
    pub fn unqueue(&self) -> Option<String> {
        self.lock().queued.pop_back()
    }

    /// Every message still waiting, oldest first: those steering, which the
    /// model will now not be handed, then those queued.
    pub fn take(&self) -> Vec<String> {
        let mut messages = self.lock();
        let queued = std::mem::take(&mut messages.queued);
        std::mem::take(&mut messages.steering).into_iter().chain(queued).collect()
    }

    /// A row for each message, its first line cut to `width` columns.
    pub fn rows(&self, width: usize) -> Vec<String> {
        let palette = palette();
        let messages = self.lock();
        let steering = messages.steering.iter().map(|message| ("↳ steering", message));
        let queued = messages.queued.iter().map(|message| ("+ queued  ", message));
        steering
            .chain(queued)
            .map(|(label, message)| {
                let room = width.saturating_sub(label.chars().count() + 4);
                format!(
                    "  {} {}",
                    palette.muted.paint(label),
                    palette.input.paint(&render::clip(message, room))
                )
            })
            .collect()
    }

    fn lock(&self) -> MutexGuard<'_, Messages> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
