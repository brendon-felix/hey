/* -------------------------------------------------------------------------- */
/*                               repl/watch.rs                                */
/* -------------------------------------------------------------------------- */

//! What the host's threads are doing, kept current in the background: for
//! the prompt and the thread picker to show, and to wake the line editor
//! when a run starts on this terminal's thread elsewhere.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures::StreamExt;
use reedline::RepaintSignal;

use crate::backend::Backend;
use crate::host::HEARTBEAT;
use crate::protocol::{Activities, Activity, ThreadStatus};
use crate::sessions::SessionId;

/// How long the activity stream may stay silent before it counts as lost.
const SILENCE: Duration = HEARTBEAT.saturating_mul(2);
const RETRY: Duration = Duration::from_secs(3);
const SPIN: Duration = Duration::from_millis(100);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone)]
pub struct Watch(Arc<Mutex<State>>);

struct State {
    activities: Activities,
    /// The thread this terminal shows.
    current: SessionId,
    /// The line editor's break signal.
    wake: Arc<AtomicBool>,
    repaint: Option<RepaintSignal>,
}

/// How many of the threads other than this terminal's are in each state.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Elsewhere {
    pub running: usize,
    pub unseen: usize,
}

impl Watch {
    pub fn new(current: SessionId, wake: Arc<AtomicBool>) -> Self {
        Self(Arc::new(Mutex::new(State {
            activities: Activities::new(),
            current,
            wake,
            repaint: None,
        })))
    }

    /// Keeps the activities current from `backend`, repainting the prompt
    /// with `repaint` as they change and while a spinner turns.
    pub fn start(&self, backend: Backend, repaint: RepaintSignal) {
        self.lock().repaint = Some(repaint);
        tokio::spawn(self.clone().listen(backend));
        tokio::spawn(self.clone().spin());
    }

    pub fn set_current(&self, thread: SessionId) {
        self.lock().current = thread;
    }

    /// What `thread` last did on the host, as the host last said.
    pub fn activity(&self, thread: &SessionId) -> Option<Activity> {
        self.lock().activities.get(thread).copied()
    }

    /// Counts the threads other than this terminal's that are answering or
    /// have something new, leaving out archived ones, as the browser does.
    pub fn elsewhere(&self) -> Elsewhere {
        let state = self.lock();
        let others = state
            .activities
            .iter()
            .filter(|(id, activity)| **id != state.current && !activity.archived);
        others.fold(Elsewhere::default(), |mut counts, (_, activity)| {
            match activity.status {
                ThreadStatus::Running | ThreadStatus::Asking => counts.running += 1,
                ThreadStatus::Unseen => counts.unseen += 1,
                ThreadStatus::Idle => {}
            }
            counts
        })
    }

    /// Forgets a wake-up the editor has not taken, before looking at what
    /// woke it, so one that arrives after the look is kept.
    pub fn rest(&self) {
        self.lock().wake.store(false, Ordering::Relaxed);
    }

    async fn listen(self, backend: Backend) {
        loop {
            if let Ok(mut activity) = backend.activity().await {
                while let Ok(Some(Ok(activities))) = tokio::time::timeout(SILENCE, activity.next()).await {
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
        let was = state.activities.get(&state.current).and_then(|activity| activity.run);
        let now = activities.get(&state.current).and_then(|activity| activity.run);
        if now.is_some() && now != was {
            state.wake.store(true, Ordering::Relaxed);
        }
        state.activities = activities;
        if let Some(repaint) = &state.repaint {
            repaint.request_repaint();
        }
    }

    /// Repaints the prompt so its spinner turns, while there is one.
    async fn spin(self) {
        loop {
            tokio::time::sleep(SPIN).await;
            if self.elsewhere().running > 0
                && let Some(repaint) = &self.lock().repaint
            {
                repaint.request_repaint();
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The spinner's frame now; every spinner shows the same one.
pub fn spinner() -> &'static str {
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    let ticks = START.elapsed().as_millis() / SPIN.as_millis();
    SPINNER[ticks as usize % SPINNER.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

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
                (id("shelved"), Activity { archived: true, ..activity(None, ThreadStatus::Unseen) }),
            ]
            .into(),
        );
        assert!(wake.load(Ordering::Relaxed));
        assert_eq!(watch.elsewhere(), Elsewhere { running: 0, unseen: 2 }, "archived left out");

        watch.rest();
        watch.update([(id("mine"), activity(Some(2), ThreadStatus::Idle))].into());
        assert!(!wake.load(Ordering::Relaxed), "the same run, ended");

        watch.update([(id("mine"), activity(Some(3), ThreadStatus::Idle))].into());
        assert!(wake.load(Ordering::Relaxed), "a run that started and ended unseen by the watch");
    }
}
