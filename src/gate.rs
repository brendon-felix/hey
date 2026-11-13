/* -------------------------------------------------------------------------- */
/*                                  gate.rs                                   */
/* -------------------------------------------------------------------------- */

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use rig_agent::tool::server::ToolServerHandle;
use rig_core::effect::EffectId;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

/// Most of one reply's tool calls that run at once.
pub const CONCURRENCY: usize = 8;

/// Lets the tool calls of one reply run side by side where that is safe:
/// calls to tools whose annotations say they only read run together, and
/// any other runs alone. Calls are let in first come, first served, which is
/// the order the model made them, so a call that changes something sees what
/// the calls before it did, and the calls after it see what it did.
pub struct Gate {
    /// The agent's tools, as the model can call them.
    tools: ToolServerHandle,
    running: Arc<RwLock<()>>,
    held: Mutex<HashMap<EffectId, Admission>>,
}

/// A call's leave to run, given up when dropped.
#[allow(dead_code, reason = "each guard is held only to be dropped")]
pub enum Admission {
    Beside(OwnedRwLockReadGuard<()>),
    Alone(OwnedRwLockWriteGuard<()>),
}

impl Gate {
    pub fn new(tools: ToolServerHandle) -> Self {
        Self {
            tools,
            running: Arc::default(),
            held: Mutex::default(),
        }
    }

    /// Waits until a call to `tool` may run beside those running now.
    pub async fn admit(&self, tool: &str) -> Admission {
        let running = self.running.clone();
        match self.reads_only(tool) {
            true => Admission::Beside(running.read_owned().await),
            false => Admission::Alone(running.write_owned().await),
        }
    }

    /// Keeps `admission` until [`Self::release`] is called for dispatch `id`.
    pub fn hold(&self, id: EffectId, admission: Admission) {
        lock(&self.held).insert(id, admission);
    }

    pub fn release(&self, id: EffectId) {
        lock(&self.held).remove(&id);
    }

    fn reads_only(&self, tool: &str) -> bool {
        self.tools
            .snapshot()
            .definitions()
            .iter()
            .find(|definition| definition.name.as_str() == tool)
            .is_some_and(|definition| definition.annotations.read_only())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use futures::{FutureExt, poll};
    use rig_agent::tool::server::ToolServer;
    use rig_tools::Delete;
    use rig_tools::workspace::Local;

    use super::*;
    use crate::skills::Load;

    #[tokio::test]
    async fn reads_run_together_and_anything_else_alone_in_turn() {
        let tools = ToolServer::new().run();
        tools.add_tool(Load { skills: Arc::from([]) });
        tools.add_tool(Delete {
            workspace: Arc::new(Local),
            base: None,
            confine: false,
        });
        let gate = Gate::new(tools);

        let first = gate.admit("skill").await;
        let second = gate.admit("skill").now_or_never();
        assert!(second.is_some(), "reads run together");

        let mut delete = Box::pin(gate.admit("delete"));
        assert!(poll!(&mut delete).is_pending(), "a change waits for the reads before it");
        let mut read = Box::pin(gate.admit("skill"));
        assert!(poll!(&mut read).is_pending(), "a read after a change waits its turn");

        drop((first, second));
        let delete = delete.await;
        assert!(poll!(&mut read).is_pending(), "nothing runs beside a change");
        assert!(gate.admit("unknown").now_or_never().is_none(), "an unknown tool runs alone");
        drop(delete);
        read.await;
    }
}
