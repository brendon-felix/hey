/* -------------------------------------------------------------------------- */
/*                             browser/projects.rs                            */
/* -------------------------------------------------------------------------- */

//! The host's projects in a full-screen list: where each lives, whether the
//! host may keep a copy, and the tool permissions that apply while working
//! on it.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use anyhow::{Result, anyhow};
use crossterm::event::{
    Event as Input, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use futures::StreamExt;
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use yansi::Paint;

use super::field::Field;
use super::{Note, REFRESH, fit};
use crate::backend::Backend;
use crate::permissions::Permission;
use crate::projects::{Change, Consent, Project};
use crate::render::palette;
use crate::ui::Screen;

/// Lists the host's projects to change them until the list is closed.
pub async fn browse(backend: &Backend) -> Result<()> {
    super::ensure_terminal("the project list")?;
    let mut browser = Browser {
        backend,
        projects: backend.projects().await?,
        mode: Mode::Browse,
        selected: None,
        scroll: 0,
        note: None,
    };
    let _screen = Screen::enter()?;
    browser.run().await
}

struct Browser<'a> {
    backend: &'a Backend,
    projects: BTreeMap<String, Project>,
    mode: Mode,
    /// Kept by name, so it stays put as the list refreshes.
    selected: Option<String>,
    scroll: usize,
    note: Option<Note>,
}

enum Mode {
    Browse,
    Edit(Draft),
    ConfirmRemove(String),
}

struct Draft {
    kind: Kind,
    field: Field,
}

#[derive(Clone, Copy)]
enum Kind {
    Location,
    Tool,
}

impl Kind {
    fn hint(self) -> &'static str {
        match self {
            Kind::Location => "location: machine path (machine alone forgets it)",
            Kind::Tool => "permission: tool allow|confirm|deny (tool alone clears it)",
        }
    }
}

fn consent_name(consent: Consent) -> &'static str {
    match consent {
        Consent::Unasked => "host copy: not asked",
        Consent::Granted => "host copy: allowed",
        Consent::Never => "host copy: refused",
    }
}

fn permission_name(permission: Permission) -> &'static str {
    match permission {
        Permission::Allow => "allow",
        Permission::Confirm => "confirm",
        Permission::Deny => "deny",
    }
}

/// `text` as the change `kind` of draft makes.
fn parse(kind: Kind, text: &str) -> Result<Change> {
    let mut words = text.trim().splitn(2, char::is_whitespace);
    let first = words.next().unwrap_or("").to_owned();
    let rest = words.next().map(str::trim).filter(|rest| !rest.is_empty());
    if first.is_empty() {
        return Err(anyhow!("{}; esc cancels", kind.hint()));
    }
    Ok(match kind {
        Kind::Location => Change::Location {
            machine: first,
            path: rest.map(str::to_owned),
        },
        Kind::Tool => Change::Tool {
            tool: first,
            permission: match rest {
                None => None,
                Some("allow") => Some(Permission::Allow),
                Some("confirm") => Some(Permission::Confirm),
                Some("deny") => Some(Permission::Deny),
                Some(other) => return Err(anyhow!("`{other}` is not allow, confirm or deny")),
            },
        },
    })
}

impl Browser<'_> {
    async fn run(&mut self) -> Result<()> {
        let mut input = EventStream::new();
        let mut refreshes = interval_at(Instant::now() + REFRESH, REFRESH);
        refreshes.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            self.draw()?;
            tokio::select! {
                _ = refreshes.tick(), if matches!(self.mode, Mode::Browse) => self.refresh().await,
                input = input.next() => match input {
                    Some(Ok(Input::Key(key))) if key.kind != KeyEventKind::Release => {
                        if self.key(key).await.is_break() {
                            return Ok(());
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                },
            }
        }
    }

    async fn key(&mut self, key: KeyEvent) -> ControlFlow<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return ControlFlow::Break(());
        }
        self.note = None;
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::ConfirmRemove(name) => {
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    self.remove(name).await;
                }
            }
            Mode::Edit(mut draft) => match key.code {
                KeyCode::Enter => self.save(draft).await,
                KeyCode::Esc => {}
                _ => {
                    draft.field.key(key);
                    self.mode = Mode::Edit(draft);
                }
            },
            Mode::Browse => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.step(-1),
                KeyCode::Down | KeyCode::Char('j') => self.step(1),
                KeyCode::Home | KeyCode::Char('g') => self.step(isize::MIN / 2),
                KeyCode::End | KeyCode::Char('G') => self.step(isize::MAX / 2),
                KeyCode::Char('c') => self.cycle_copy().await,
                KeyCode::Char('l') => self.draft(Kind::Location),
                KeyCode::Char('p') => self.draft(Kind::Tool),
                KeyCode::Char('d') | KeyCode::Delete => {
                    if let Some(name) = self.selected() {
                        self.mode = Mode::ConfirmRemove(name);
                    }
                }
                KeyCode::Char('r') => self.refresh().await,
                KeyCode::Char('q') | KeyCode::Esc => return ControlFlow::Break(()),
                _ => {}
            },
        }
        ControlFlow::Continue(())
    }

    fn names(&self) -> Vec<&String> {
        self.projects.keys().collect()
    }

    fn position(&self) -> usize {
        self.names()
            .iter()
            .position(|name| Some(*name) == self.selected.as_ref())
            .unwrap_or(0)
    }

    fn selected(&self) -> Option<String> {
        self.names().get(self.position()).map(|name| (*name).clone())
    }

    fn step(&mut self, by: isize) {
        let names = self.names();
        if names.is_empty() {
            return;
        }
        let to = (self.position() as isize).saturating_add(by);
        let to = to.clamp(0, names.len() as isize - 1) as usize;
        self.selected = Some(names[to].clone());
    }

    fn draft(&mut self, kind: Kind) {
        if self.selected().is_some() {
            self.mode = Mode::Edit(Draft {
                kind,
                field: Field::new(""),
            });
        }
    }

    async fn refresh(&mut self) {
        match self.backend.projects().await {
            Ok(projects) => self.projects = projects,
            Err(error) => self.note = Some(Note::failed(error)),
        }
    }

    async fn apply(&mut self, name: &str, change: Change) -> Result<()> {
        match self.backend.change_project(name, &change).await? {
            true => Ok(()),
            false => Err(anyhow!("no project `{name}` any more")),
        }
    }

    async fn cycle_copy(&mut self) {
        let Some(name) = self.selected() else { return };
        let next = match self.projects[&name].host_copy {
            Consent::Unasked => Consent::Granted,
            Consent::Granted => Consent::Never,
            Consent::Never => Consent::Unasked,
        };
        match self.apply(&name, Change::HostCopy(next)).await {
            Ok(()) => {
                self.note = Some(Note::said(format!("{name} {}", consent_name(next))));
                self.refresh().await;
            }
            Err(error) => self.note = Some(Note::failed(error)),
        }
    }

    /// Saves the draft, or keeps editing it when that fails.
    async fn save(&mut self, draft: Draft) {
        let Some(name) = self.selected() else { return };
        let result = match parse(draft.kind, draft.field.text()) {
            Ok(change) => self.apply(&name, change).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                self.note = Some(Note::said(format!("Saved {name}")));
                self.refresh().await;
            }
            Err(error) => {
                self.note = Some(Note::failed(error));
                self.mode = Mode::Edit(draft);
            }
        }
    }

    async fn remove(&mut self, name: String) {
        match self.backend.remove_project(&name).await {
            Ok(removed) => {
                self.note = Some(Note::said(match removed {
                    true => format!("Forgot project {name}"),
                    false => "Already forgotten".into(),
                }));
                self.step(1);
                if self.selected.as_deref() == Some(name.as_str()) {
                    self.step(-1);
                }
                self.refresh().await;
            }
            Err(error) => self.note = Some(Note::failed(error)),
        }
    }

    fn draw(&mut self) -> Result<()> {
        let (width, height) = super::size()?;
        let rows = super::page();
        let tally = match self.projects.len() {
            1 => "1 project".to_owned(),
            count => format!("{count} projects"),
        };
        let mut lines = vec![
            super::header(
                self.backend,
                "Projects",
                &[palette().muted.paint(&tally).to_string()],
                width,
            ),
            match &self.mode {
                Mode::Edit(draft) => format!(
                    " {} {}  {}",
                    palette().accent.paint("›"),
                    draft.field.view(width.saturating_sub(4 + draft.kind.hint().chars().count())),
                    palette().muted.paint(draft.kind.hint())
                ),
                _ => String::new(),
            },
        ];
        let at = self.position();
        let mut body = Vec::new();
        let mut at_line = 0;
        for (i, (name, project)) in self.projects.iter().enumerate() {
            let selected = i == at;
            if selected {
                at_line = body.len();
            }
            let pointer = if selected { "›" } else { " " };
            let head = fit(
                &format!("{pointer} {name}  ({})", consent_name(project.host_copy)),
                width,
            );
            body.push(match selected {
                true => head.invert().to_string(),
                false => head,
            });
            if selected {
                let muted = |text: String| format!("    {}", palette().muted.paint(&fit(&text, width.saturating_sub(4))));
                if let Some(remote) = &project.remote {
                    body.push(muted(format!("remote  {remote}")));
                }
                for (machine, path) in &project.locations {
                    body.push(muted(format!("on {machine}  {path}")));
                }
                for (tool, permission) in &project.tools {
                    body.push(muted(format!("tool {tool}  {}", permission_name(*permission))));
                }
            }
        }
        if body.is_empty() {
            body.push(format!(
                "  {}",
                palette().muted.paint("No projects yet; they register as you work in them.")
            ));
        }
        self.scroll = super::scrolled(self.scroll, at_line, rows);
        lines.extend(body.into_iter().skip(self.scroll).take(rows));
        super::paint(lines, self.footer(width), height)
    }

    fn footer(&self, width: usize) -> String {
        let keys = match &self.mode {
            Mode::ConfirmRemove(name) => {
                let prompt = format!(" Forget project {name}? y forgets it, any other key keeps it");
                return super::question(&prompt, width);
            }
            Mode::Edit(_) => " ⏎ save · esc cancel",
            Mode::Browse => {
                " ↑↓ move · c host copy · l location · p permission · d forget · r refresh · q quit"
            }
        };
        super::footer(self.note.as_ref(), keys, width)
    }
}
