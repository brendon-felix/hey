/* -------------------------------------------------------------------------- */
/*                               host/attach.rs                               */
/* -------------------------------------------------------------------------- */

use std::fmt;
use std::path::Path;

use anyhow::{Result, bail};
use rig_core::message::Message;

use super::{Host, Run};
use crate::agent::{self, Model};
use crate::mention::{self, Context, Item, Reference, SHARED};
use crate::protocol::{Attachment, Event};
use crate::sessions::SessionId;

/// What the host attaches for a mention: a file or directory of its shared
/// repository, or a saved conversation.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Shared { path: String, item: Item },
    Thread(SessionId),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shared {
                path,
                item: Item::Directory(_),
            } if !path.ends_with('/') => write!(f, "{SHARED}{path}/"),
            Self::Shared { path, .. } => write!(f, "{SHARED}{path}"),
            Self::Thread(id) => write!(f, "conversation {id}"),
        }
    }
}

impl Host {
    /// `prompt` as the model gets it: what was typed, then what the client
    /// attached and what the host's mentions attach, a conversation as
    /// `model` sums it up. Tells `run` of each the host attaches.
    pub(super) async fn message(
        &self,
        prompt: &str,
        attachments: &[Attachment],
        model: &Model,
        run: &Run,
    ) -> Message {
        let mut context = Context::default();
        attachments.iter().for_each(|attachment| context.attach(attachment));
        for target in self.targets(prompt) {
            run.emit(Event::Attached {
                name: target.to_string(),
            });
            let attached = match &target {
                Target::Shared { item, .. } => item.load().map(|loaded| context.attach(&loaded)),
                Target::Thread(id) => self
                    .summary(id, model)
                    .await
                    .map(|summary| context.summary(id, &summary)),
            };
            if let Err(error) = attached {
                run.emit(Event::Notice {
                    text: format!("{target} not attached: {error:#}"),
                });
            }
        }
        context.message(prompt)
    }

    /// Every path of the shared repository, for completion.
    pub fn shared(&self) -> Vec<String> {
        mention::walk(&self.shared)
            .take(mention::MAX_INDEXED)
            .collect()
    }

    fn targets(&self, prompt: &str) -> Vec<Target> {
        let mut targets = Vec::new();
        let found = mention::mentions(prompt).filter_map(|mention| {
            mention.resolve(|reference| match reference {
                Reference::Local(_) => None,
                Reference::Shared(path) => Some(Target::Shared {
                    item: shared(&self.shared, path)?,
                    path: path.to_owned(),
                }),
                Reference::Thread(id) => self.sessions.find(id).map(Target::Thread),
            })
        });
        for (target, _) in found {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    }

    async fn summary(&self, id: &SessionId, model: &Model) -> Result<String> {
        let transcript = mention::transcript(&self.sessions.messages(id)?);
        if transcript.is_empty() {
            bail!("nothing was said in it");
        }
        agent::summarize(model, &transcript).await
    }
}

/// What `path` names in the shared repository at `root`; never anything
/// outside it, since clients choose `path`.
fn shared(root: &Path, path: &str) -> Option<Item> {
    if path.is_empty() {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let found = root.join(path).canonicalize().ok()?;
    found.starts_with(&root).then(|| Item::at(found)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_paths_stay_inside_the_repository() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert!(matches!(shared(&root, "main.rs"), Some(Item::File(_))));
        assert!(matches!(shared(&root, "repl/"), Some(Item::Directory(_))));
        assert_eq!(shared(&root, ""), None);
        assert_eq!(shared(&root, "../Cargo.toml"), None);
        assert_eq!(shared(&root, "/etc/hosts"), None);
    }
}
