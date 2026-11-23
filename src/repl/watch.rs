/* -------------------------------------------------------------------------- */
/*                               repl/watch.rs                                */
/* -------------------------------------------------------------------------- */

//! What the host's threads are doing, kept current in the background: for
//! the prompt and the thread picker to show, and to wake the line editor
//! when a run starts on this terminal's thread elsewhere.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use reedline::RepaintSignal;

use crate::backend::Backend;
use crate::host::HEARTBEAT;
use crate::protocol::{Activities, Activity};
use crate::sessions::SessionId;

/// How long the activity stream may stay silent before it counts as lost.
const SILENCE: Duration = HEARTBEAT.saturating_mul(2);
const RETRY: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct Watch(Arc<Mutex<State>>);

struct State {
    activities: Activities,
    /// The thread this terminal shows.
    current: SessionId,
    /// Whether this terminal follows a run of it already, which needs no
    /// waking for.
    following: bool,
    /// The line editor's break signal.
    wake: Arc<AtomicBool>,
    repaint: Option<RepaintSignal>,
    /// The task that listens to the host, until another host replaces it.
    listener: Option<tokio::task::AbortHandle>,
}

impl Watch {
    pub fn new(current: SessionId, wake: Arc<AtomicBool>) -> Self {
        Self(Arc::new(Mutex::new(State {
            activities: Activities::new(),
            current,
            following: false,
            wake,
            repaint: None,
            listener: None,
        })))
    }

    /// Keeps the activities current from `backend`, repainting the prompt
    /// with `repaint` as they change and while a spinner turns.
    pub fn start(&self, backend: Backend, repaint: RepaintSignal) {
        self.lock().repaint = Some(repaint);
        tokio::spawn(self.clone().tick());
        self.listen_to(backend);
    }

    /// Follows `backend` from now on, instead of the host before it.
    pub fn listen_to(&self, backend: Backend) {
        let task = tokio::spawn(self.clone().listen(backend));
        let mut state = self.lock();
        if let Some(old) = state.listener.replace(task.abort_handle()) {
            old.abort();
        }
        state.activities = Activities::new();
    }

    pub fn set_current(&self, thread: SessionId) {
        self.lock().current = thread;
    }

    pub fn set_following(&self, following: bool) {
        self.lock().following = following;
    }

    /// What `thread` last did on the host, as the host last said.
    pub fn activity(&self, thread: &SessionId) -> Option<Activity> {
        self.lock().activities.get(thread).copied()
    }

    /// Forgets a wake-up the editor has not taken, before looking at what
    /// woke it, so one that arrives after the look is kept.
    pub fn rest(&self) {
        self.lock().wake.store(false, Ordering::Relaxed);
    }

    async fn listen(self, backend: Backend) {
        loop {
            if let Ok(mut activity) = backend.activity().await {
                while let Ok(Some(Ok(activities))) =
                    tokio::time::timeout(SILENCE, activity.next()).await
                {
                    self.update(activities);
                }
            }
            // Unknown until the host answers again, rather than stale.
            self.update(Activities::new());
            tokio::time::sleep(RETRY).await;
        }
    }

    /// Wakes the editor when this terminal's thread has a run it has not
    /// heard of, however short: one that came and went between two updates
    /// still leaves its id.
    fn update(&self, activities: Activities) {
        let mut state = self.lock();
        let was = state
            .activities
            .get(&state.current)
            .and_then(|activity| activity.run);
        let now = activities
            .get(&state.current)
            .and_then(|activity| activity.run);
        if now.is_some() && now != was && !state.following {
            state.wake.store(true, Ordering::Relaxed);
        }
        state.activities = activities;
        if let Some(repaint) = &state.repaint {
            repaint.request_repaint();
        }
    }

    /// Repaints the prompt each second while a run is followed, so the
    /// time it shows counts live.
    async fn tick(self) {
        loop {
            tokio::time::sleep(TICK).await;
            let state = self.lock();
            if state.following
                && let Some(repaint) = &state.repaint
            {
                repaint.request_repaint();
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ThreadStatus;

    fn id(text: &str) -> SessionId {
        text.parse().unwrap()
    }

    #[test]
    fn wakes_the_editor_once_for_each_run_on_its_thread_however_short() {
        let wake = Arc::new(AtomicBool::new(false));
        let watch = Watch::new(id("mine"), wake.clone());
        let activity = |run: Option<u64>, status| Activity {
            run: run.map(crate::protocol::RunId),
            status,
            archived: false,
        };

        watch.update([(id("other"), activity(Some(1), ThreadStatus::Running))].into());
        assert!(!wake.load(Ordering::Relaxed), "another thread's run");

        watch.update(
            [
                (id("mine"), activity(Some(2), ThreadStatus::Running)),
                (id("other"), activity(Some(1), ThreadStatus::Unseen)),
                (id("older"), activity(None, ThreadStatus::Unseen)),
                (
                    id("shelved"),
                    Activity {
                        archived: true,
                        ..activity(None, ThreadStatus::Unseen)
                    },
                ),
            ]
            .into(),
        );
        assert!(wake.load(Ordering::Relaxed));

        watch.rest();
        watch.update([(id("mine"), activity(Some(2), ThreadStatus::Idle))].into());
        assert!(!wake.load(Ordering::Relaxed), "the same run, ended");

        watch.update([(id("mine"), activity(Some(3), ThreadStatus::Idle))].into());
        assert!(
            wake.load(Ordering::Relaxed),
            "a run that started and ended unseen by the watch"
        );
    }
}
