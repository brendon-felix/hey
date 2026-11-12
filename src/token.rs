/* -------------------------------------------------------------------------- */
/*                                  token.rs                                  */
/* -------------------------------------------------------------------------- */

//! The token a host and its clients share. Each side takes it from `token`,
//! from `token_file`, or, when neither is set, keeps it itself: a host in a
//! file it generates, a client in the system keychain, or a file where there
//! is none.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{self, TokenSource};
use crate::render;

const KEYCHAIN_SERVICE: &str = "hey";

/// The token a host accepts. A token file that does not exist yet is
/// generated, so a host needs no setup and a synced file reaches clients.
pub fn host(source: &TokenSource) -> Result<String> {
    let path = match source {
        TokenSource::Value(token) => return given(token, "`server.token`"),
        TokenSource::File(path) => config::expand_path(path)?,
        TokenSource::Kept => config::data_dir()?.join("server-token"),
    };
    if let Some(token) = read(&path)? {
        return Ok(token);
    }
    let token = generate()?;
    write_private(&path, &token)?;
    render::notice(&format!(
        "generated the token clients need in {}",
        path.display()
    ));
    Ok(token)
}

/// The token a client's config gives for a remote, or `None` when hey keeps it.
pub fn configured(source: &TokenSource) -> Result<Option<String>> {
    match source {
        TokenSource::Value(token) => given(token, "`token`").map(Some),
        TokenSource::File(path) => {
            let path = config::expand_path(path)?;
            read(&path)?
                .map(Some)
                .with_context(|| format!("no token in {}; put the host's token there", path.display()))
        }
        TokenSource::Kept => Ok(None),
    }
}

/// Says where a configured token comes from, for errors.
pub fn describe(source: &TokenSource) -> String {
    match source {
        TokenSource::Value(_) => "`token`".into(),
        TokenSource::File(path) => format!("`token_file` {path}"),
        TokenSource::Kept => "what hey kept".into(),
    }
}

/// The token hey kept for `remote`, if any.
pub fn kept(remote: &str) -> Result<Option<String>> {
    if let Ok(entry) = keychain(remote)
        && let Ok(token) = entry.get_password()
    {
        return Ok(Some(token));
    }
    read(&kept_file(remote)?)
}

/// Keeps `token` for `remote` in the system keychain, or a private file
/// where there is none, such as a server without a desktop session. Returns
/// where it went.
pub fn keep(remote: &str, token: &str) -> Result<String> {
    match keychain(remote).and_then(|entry| entry.set_password(token)) {
        Ok(()) => Ok("the system keychain".into()),
        Err(_) => {
            let path = kept_file(remote)?;
            write_private(&path, token)?;
            Ok(path.display().to_string())
        }
    }
}

fn keychain(remote: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &format!("remote/{remote}"))
}

fn kept_file(remote: &str) -> Result<PathBuf> {
    let name: String = remote
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    Ok(config::data_dir()?.join("tokens").join(name))
}

fn given(token: &str, what: &str) -> Result<String> {
    let token = config::expand(token)?.trim().to_owned();
    match token.is_empty() {
        true => bail!("{what} is empty"),
        false => Ok(token),
    }
}

/// The token in `path`, or `None` when the file does not exist.
fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => bail!("the token file {} is empty", path.display()),
        Ok(text) => Ok(Some(text.trim().to_owned())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn write_private(path: &Path, token: &str) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    writeln!(file, "{token}").with_context(|| format!("cannot write {}", path.display()))
}

fn generate() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("cannot generate a token: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_generates_its_token_file_once_and_privately() {
        let dir = std::env::temp_dir().join(format!("hey-token-test-{}", std::process::id()));
        let path = dir.join("synced").join("hey-token");
        let source = TokenSource::File(path.display().to_string());

        let token = host(&source).unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(host(&source).unwrap(), token, "reused, not regenerated");
        assert_eq!(configured(&source).unwrap(), Some(token));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        fs::write(&path, "  \n").unwrap();
        assert!(host(&source).is_err(), "an empty token is a mistake, not a prompt to replace it");
        fs::remove_dir_all(dir).unwrap();
    }
}
