/* -------------------------------------------------------------------------- */
/*                               host/attach.rs                               */
/* -------------------------------------------------------------------------- */

use std::fmt;
use std::path::Path;

use anyhow::{Result, bail};
use rig_core::message::Message;

use super::{Host, Run};
use crate::agent::{self, Model};
use crate::mention::{self, Context, Item, Reference, SHARED, SKILL};
use crate::protocol::{Attachment, Event};
use crate::sessions::SessionId;
use crate::skills::{self, Found, Skill};
use crate::steer::{Steer, Steering};
use crate::transcript;

/// What the host attaches for a mention: a file or directory of its shared
/// repository, a saved conversation, or a skill the user invoked.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    Shared { path: String, item: Item },
    Thread(SessionId),
    Skill(Skill),
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
            Self::Skill(skill) => write!(f, "{SKILL}{}", skill.name),
        }
    }
}

impl Host {
    /// `prompt` as the model gets it: what was typed, then what the client
    /// attached and what the host's mentions attach. Tells `run` of each,
    /// so every client shows them.
    pub(super) async fn message(
        &self,
        prompt: &str,
        attachments: &[Attachment],
        skills: &Found,
        model: &Model,
        run: &Run,
    ) -> Message {
        let (context, told) = self.context(prompt, attachments, skills, model).await;
        told.into_iter().for_each(|event| run.emit(event));
        context.message(prompt)
    }

    /// What `prompt` sent to `run` while it answers attaches, for the model
    /// to be handed with it. What failed to attach is told at once; what
    /// did is not, as the message itself shows only once handed over.
    pub(super) async fn steering(&self, steer: Steer, run: &Run) -> Steering {
        let model = Model::connect(run.model(), &self.providers);
        let context = match &model {
            Ok(model) => {
                let skills = self.skills();
                let (context, told) = self.context(&steer.prompt, &steer.attachments, &skills, model).await;
                told.into_iter()
                    .filter(|event| matches!(event, Event::Notice { .. }))
                    .for_each(|event| run.emit(event));
                context.text()
            }
            Err(error) => {
                run.emit(Event::Notice {
                    text: format!("nothing attached: {error:#}"),
                });
                None
            }
        };
        Steering {
            prompt: steer.prompt,
            context,
        }
    }

    /// What the client attached to `prompt` and what the host's mentions in
    /// it attach, a conversation as `model` sums it up, and the `skills` it
    /// invokes; and the events that tell of each, in order.
    async fn context(
        &self,
        prompt: &str,
        attachments: &[Attachment],
        skills: &Found,
        model: &Model,
    ) -> (Context, Vec<Event>) {
        let mut context = Context::default();
        let mut told = Vec::new();
        for attachment in attachments {
            told.push(Event::Attached {
                name: attachment.name(),
            });
            context.attach(attachment);
        }
        for target in self.targets(prompt, skills) {
            told.push(Event::Attached {
                name: target.to_string(),
            });
            let attached = match &target {
                Target::Shared { item, .. } => item.load().map(|loaded| context.attach(&loaded)),
                Target::Thread(id) => self
                    .summary(id, model)
                    .await
                    .map(|summary| context.summary(id, &summary)),
                Target::Skill(skill) => skill.content().map(|content| context.skill(content)),
            };
            if let Err(error) = attached {
                told.push(Event::Notice {
                    text: format!("{target} not attached: {error:#}"),
                });
            }
        }
        (context, told)
    }

    /// Every path of the shared repository, for completion.
    pub fn shared(&self) -> Vec<String> {
        mention::walk(&self.shared)
            .take(mention::MAX_INDEXED)
            .collect()
    }

    /// What `prompt` attaches: the skill it starts with as `/name`, then
    /// what it mentions, each once.
    fn targets(&self, prompt: &str, skills: &Found) -> Vec<Target> {
        let skill = |name| skills.get(name).cloned().map(Target::Skill);
        let invoked = skills::invoked(prompt).and_then(skill);
        let mentioned = mention::mentions(prompt).filter_map(|mention| {
            mention.resolve(|reference| match reference {
                Reference::Local(_) => None,
                Reference::Shared(path) => Some(Target::Shared {
                    item: shared(&self.shared, path)?,
                    path: path.to_owned(),
                }),
                Reference::Thread(id) => self.sessions.find(id).map(Target::Thread),
                Reference::Skill(name) => skill(name),
            })
        });
        let mut targets = Vec::new();
        for target in invoked.into_iter().chain(mentioned.map(|(target, _)| target)) {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    }

    async fn summary(&self, id: &SessionId, model: &Model) -> Result<String> {
        let transcript = transcript::thread(&self.sessions.messages(id)?);
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
