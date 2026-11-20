/* -------------------------------------------------------------------------- */
/*                                  sync.rs                                   */
/* -------------------------------------------------------------------------- */

//! Copies of the user's files, folders and git repositories, kept on the
//! host so its agents can go on working on them while the client is away.
//!
//! The client pushes each configured path to the host (one way: client to
//! host), first on connecting and then every interval. Whenever a push fails,
//! because the host or the network is gone, the client forgets what the host
//! held, so the first push that succeeds again compares against the host's
//! copy and sends whatever differs: that is the sync on reconnect.
//!
//! A git repository is copied like a folder, `.git` included, so the host
//! can run git there; everything else that git would ignore stays behind.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use ignore::WalkBuilder;

use crate::client::Client;
use crate::config;

/// The largest file synced; bigger ones (packfiles, media) stay behind.
pub const MAX_FILE: u64 = 16 * 1024 * 1024;

/// Hashes of each file by its path under the synced root.
pub type Manifest = BTreeMap<String, u64>;

/// FNV-1a, which both sides compute alike with no dependency.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A name that is safe as one path component.
fn component(name: &str) -> Result<&str> {
    let fine = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    match fine {
        true => Ok(name),
        false => bail!("`{name}` is not a valid machine or sync name"),
    }
}

/* ---------------------------------- host ---------------------------------- */

/// Where the host keeps what clients synced, as `<dir>/<machine>/<name>/…`.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn root(&self, machine: &str, name: &str) -> Result<PathBuf> {
        Ok(self.dir.join(component(machine)?).join(component(name)?))
    }

    /// `rel` under the synced root, refusing anything that would leave it.
    fn path(&self, machine: &str, name: &str, rel: &str) -> Result<PathBuf> {
        let mut path = self.root(machine, name)?;
        let rel = Path::new(rel);
        if rel.as_os_str().is_empty()
            || !rel
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            bail!("`{}` is not a path inside the synced folder", rel.display());
        }
        path.push(rel);
        Ok(path)
    }

    /// What the host holds of `name` from `machine`.
    pub fn manifest(&self, machine: &str, name: &str) -> Result<Manifest> {
        let root = self.root(machine, name)?;
        let mut manifest = Manifest::new();
        if root.is_dir() {
            for (rel, path, _) in scan(&root, true) {
                if let Ok(bytes) = std::fs::read(path) {
                    manifest.insert(rel, fnv(&bytes));
                }
            }
        }
        Ok(manifest)
    }

    pub async fn put(&self, machine: &str, name: &str, rel: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path(machine, name, rel)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, bytes).await?;
        Ok(())
    }

    pub async fn remove(&self, machine: &str, name: &str, rel: &str) -> Result<()> {
        let path = self.path(machine, name, rel)?;
        match tokio::fs::remove_file(path).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }
}

/* --------------------------------- client --------------------------------- */

/// A path the user syncs, under the name it has on the host.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub root: PathBuf,
}

/// The entries `paths` name; two with one folder name get `-2`, `-3`…
pub fn entries(paths: &[String]) -> Result<Vec<Entry>> {
    let mut taken: HashMap<String, usize> = HashMap::new();
    let mut entries = Vec::new();
    for path in paths {
        let root = config::expand_path(path)?;
        let root = root.canonicalize().unwrap_or(root);
        let base: String = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".into())
            .chars()
            .map(
                |c| match c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    true => c,
                    false => '_',
                },
            )
            .collect();
        let count = taken.entry(base.clone()).or_default();
        *count += 1;
        let name = match *count {
            1 => base,
            n => format!("{base}-{n}"),
        };
        entries.push(Entry { name, root });
    }
    Ok(entries)
}

/// Files under `root` as `(path under root, full path, size)`. Honors
/// `.gitignore` unless `all`, and always takes `.git`. Skips files over
/// [`MAX_FILE`] and anything that is not a plain file.
fn scan(root: &Path, all: bool) -> Vec<(String, PathBuf, u64)> {
    let walk = WalkBuilder::new(root)
        .hidden(false)
        .parents(false)
        .git_ignore(!all)
        .git_global(false)
        .git_exclude(!all)
        .ignore(false)
        .require_git(false)
        .follow_links(false)
        .build();
    walk.flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() || meta.len() > MAX_FILE {
                return None;
            }
            let rel = entry.path().strip_prefix(root).ok()?;
            let rel = rel
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            Some((rel, entry.into_path(), meta.len()))
        })
        .collect()
}

#[derive(Default)]
struct State {
    /// `(size, modified, hash)` of each file as last read, to skip rereading
    /// files that did not change.
    seen: HashMap<String, (u64, Option<SystemTime>, u64)>,
    /// What the host holds, once known; `None` after a failed push.
    remote: Option<Manifest>,
}

/// Pushes every entry now and every `interval` after, for as long as the
/// process lives.
pub fn spawn(client: Client, entries: Vec<Entry>, interval: Duration) {
    tokio::spawn(async move {
        let mut states: Vec<State> = entries.iter().map(|_| State::default()).collect();
        loop {
            for (entry, state) in entries.iter().zip(&mut states) {
                if let Err(error) = push(&client, entry, state).await {
                    state.remote = None;
                    tracing::warn!(name = %entry.name, error = format!("{error:#}"), "sync failed; will retry");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

async fn push(client: &Client, entry: &Entry, state: &mut State) -> Result<()> {
    let root = entry.root.clone();
    let mut seen = std::mem::take(&mut state.seen);
    let (local, seen) = tokio::task::spawn_blocking(move || {
        let mut local = Manifest::new();
        let mut fresh = HashMap::new();
        for (rel, path, size) in scan(&root, false) {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            let hash = match seen.remove(&rel) {
                Some((s, m, hash)) if s == size && m == modified && m.is_some() => hash,
                _ => match std::fs::read(&path) {
                    Ok(bytes) => fnv(&bytes),
                    Err(_) => continue,
                },
            };
            fresh.insert(rel.clone(), (size, modified, hash));
            local.insert(rel, hash);
        }
        (local, fresh)
    })
    .await?;
    state.seen = seen;

    let mut remote = match state.remote.take() {
        Some(remote) => remote,
        None => client.sync_manifest(&entry.name).await?,
    };
    for (rel, hash) in &local {
        if remote.get(rel) != Some(hash) {
            let bytes = tokio::fs::read(entry.root.join(rel)).await?;
            client.sync_put(&entry.name, rel, bytes).await?;
            remote.insert(rel.clone(), *hash);
        }
    }
    let gone: Vec<String> = remote
        .keys()
        .filter(|rel| !local.contains_key(*rel))
        .cloned()
        .collect();
    for rel in gone {
        client.sync_remove(&entry.name, &rel).await?;
        remote.remove(&rel);
    }
    state.remote = Some(remote);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_cannot_leave_the_synced_folder() {
        let store = Store::new(PathBuf::from("/data/sync"));
        assert!(store.path("laptop", "proj", "src/a.rs").is_ok());
        assert!(store.path("laptop", "proj", "../a").is_err());
        assert!(store.path("laptop", "proj", "/etc/passwd").is_err());
        assert!(store.path("..", "proj", "a").is_err());
        assert!(store.path("laptop", "a/b", "a").is_err());
    }

    #[tokio::test]
    async fn a_put_file_shows_in_the_manifest_and_a_removed_one_does_not() {
        let dir = std::env::temp_dir().join(format!("hey-sync-{}", std::process::id()));
        let store = Store::new(dir.clone());
        store.put("m", "p", "x/y.txt", b"hi").await.unwrap();
        let manifest = store.manifest("m", "p").unwrap();
        assert_eq!(manifest.get("x/y.txt"), Some(&fnv(b"hi")));
        store.remove("m", "p", "x/y.txt").await.unwrap();
        assert!(store.manifest("m", "p").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
