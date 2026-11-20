//! Writing files so a crash or a full disk never leaves one half-written,
//! and reading ones that may have been damaged.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Writes `bytes` to `path` by way of a temporary file beside it, which is
/// synced and then renamed over `path`: a reader sees the old file or the
/// new one, never part of either. The temporary name is per process, so two
/// heys writing the same file do not share one.
pub fn write_atomic(path: &Path, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let temporary = path.with_file_name(name);
    let written = File::create(&temporary).and_then(|mut file| {
        file.write_all(bytes.as_ref())?;
        file.sync_all()
    });
    match written.and_then(|()| fs::rename(&temporary, path)) {
        Ok(()) => Ok(()),
        Err(error) => {
            _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// Moves a file that cannot be read aside as `<name>.bad`, so the next write
/// starts afresh and the damaged one is still there to look at.
pub fn set_aside(path: &Path) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(".bad");
    let aside = path.with_file_name(name);
    fs::rename(path, &aside).ok().map(|()| aside)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_replace_whole_and_leave_no_temporary() {
        let dir = std::env::temp_dir().join(format!("hey-fsio-{}", std::process::id()));
        let file = dir.join("deep/a.json");
        write_atomic(&file, "one").unwrap();
        write_atomic(&file, "two").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "two");
        assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
        let aside = set_aside(&file).unwrap();
        assert!(aside.exists() && !file.exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
