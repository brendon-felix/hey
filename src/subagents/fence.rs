/* -------------------------------------------------------------------------- */
/*                             subagents/fence.rs                             */
/* -------------------------------------------------------------------------- */

use std::ffi::{OsStr, OsString};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use rig_core::tool::ContentHash;
use rig_tools::process::{Launcher, Process, Spawn};
use rig_tools::workspace::{Contents, Excerpt, Listing, Local, Stat, Workspace};

/// This machine's files, but for those in `excluded` directories, which
/// every operation refuses and no listing shows. A command started
/// elsewhere may still reach them.
#[derive(Debug)]
pub struct Fenced {
    excluded: Vec<PathBuf>,
}

impl Fenced {
    pub fn new(excluded: Vec<PathBuf>) -> Self {
        Self { excluded }
    }

    fn check(&self, path: &Path) -> io::Result<()> {
        let canonical = Local.canonical(path);
        match self.fence(&canonical) {
            Some(excluded) => Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "{} is in {}, which the user's configuration keeps subagents out of",
                    path.display(),
                    excluded.display()
                ),
            )),
            None => Ok(()),
        }
    }

    /// The excluded directory a canonical `path` is in.
    fn fence(&self, path: &Path) -> Option<&Path> {
        self.excluded
            .iter()
            .find(|excluded| path.starts_with(excluded))
            .map(PathBuf::as_path)
    }
}

impl Workspace for Fenced {
    fn stat(&self, path: &Path) -> io::Result<Stat> {
        self.check(path)?;
        Local.stat(path)
    }

    fn read(&self, path: &Path, limit: u64) -> io::Result<Contents> {
        self.check(path)?;
        Local.read(path, limit)
    }

    fn read_files(&self, paths: &[PathBuf], limit: u64) -> Vec<Option<Vec<u8>>> {
        paths
            .iter()
            .map(|path| {
                self.check(path).ok()?;
                Local.read_files(std::slice::from_ref(path), limit).pop().flatten()
            })
            .collect()
    }

    fn excerpt(&self, path: &Path, first: usize, count: usize) -> io::Result<Excerpt> {
        self.check(path)?;
        Local.excerpt(path, first, count)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.check(path)?;
        Local.write(path, bytes)
    }

    fn replace(&self, path: &Path, bytes: &[u8], limit: u64) -> io::Result<io::Result<Contents>> {
        self.check(path)?;
        Local.replace(path, bytes, limit)
    }

    fn write_if(&self, path: &Path, bytes: &[u8], held: &ContentHash) -> io::Result<Option<Contents>> {
        self.check(path)?;
        Local.write_if(path, bytes, held)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.check(path)?;
        Local.remove(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check(from)?;
        self.check(to)?;
        Local.rename(from, to)
    }

    fn canonical(&self, path: &Path) -> PathBuf {
        Local.canonical(path)
    }

    fn join(&self, base: &Path, path: &Path) -> PathBuf {
        Local.join(base, path)
    }

    fn files(&self, root: &Path, glob: Option<&str>, listing: Listing) -> io::Result<Vec<PathBuf>> {
        self.check(root)?;
        let canonical_root = Local.canonical(root);
        let files = Local.files(root, glob, listing)?;
        // Files come as `root` joined with their path below it, so their
        // place is known without resolving each one.
        let inside = |file: &PathBuf| match file.strip_prefix(root) {
            Ok(below) => self.fence(&canonical_root.join(below)).is_none(),
            Err(_) => self.check(file).is_ok(),
        };
        Ok(files.into_iter().filter(inside).collect())
    }

    fn spawn(
        &self,
        launcher: &Launcher,
        dir: Option<&Path>,
        program: &OsStr,
        args: &[OsString],
        how: Spawn,
    ) -> io::Result<Process> {
        if let Some(dir) = dir.or(launcher.cwd.as_deref()) {
            self.check(dir)?;
        }
        Local.spawn(launcher, dir, program, args, how)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_directories_are_refused_and_left_out_of_listings() {
        let root = std::env::temp_dir().join(format!("hey-fence-{}", std::process::id()));
        std::fs::create_dir_all(root.join("secret")).unwrap();
        std::fs::write(root.join("open.txt"), "open").unwrap();
        std::fs::write(root.join("secret/key.txt"), "key").unwrap();
        let canonical = root.canonicalize().unwrap();
        let fenced = Fenced::new(vec![canonical.join("secret")]);

        assert!(fenced.read(&root.join("open.txt"), 100).is_ok());
        let refused = fenced.read(&root.join("secret/../secret/key.txt"), 100).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::PermissionDenied);
        assert!(fenced.write(&root.join("secret/new.txt"), b"x").is_err());
        assert!(!root.join("secret/new.txt").exists());

        let listed = fenced.files(&root, None, Listing::Visible).unwrap();
        assert_eq!(listed, [root.join("open.txt")]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
