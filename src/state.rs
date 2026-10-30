//! Per-user state files: `~/.flowy`, written owner-only and replaced atomically.

use anyhow::{Context, Result};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

/// `~/.flowy`, holding the per-user flowy state files.
pub(crate) fn state_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".flowy"))
}

/// Write a small state file with owner-only permissions, replacing it atomically so a
/// crash never leaves a half-written file behind.
pub(crate) fn write_private(path: &Path, body: &[u8]) -> Result<()> {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        set_mode(directory, 0o700)?;
    }
    let temporary = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&temporary)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        file.write_all(body)?;
        file.sync_all()?;
    }
    set_mode(&temporary, 0o600)?;
    fs::rename(&temporary, path)
        .with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("could not restrict permissions on {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("flowy-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        directory.join("browser.json")
    }

    #[test]
    fn writes_owner_only_files() {
        let path = scratch("private");
        write_private(&path, br#"{"session_id":"flowy-1"}"#).unwrap();
        assert_eq!(fs::read(&path).unwrap(), br#"{"session_id":"flowy-1"}"#);
        let directory = path.parent().unwrap();
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn replaces_an_existing_file() {
        let path = scratch("replace");
        write_private(&path, b"first").unwrap();
        write_private(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert!(!path.with_extension("tmp").exists());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn state_lives_under_the_home_directory() {
        // HOME is always set for a normal process, and the tests must not touch it.
        if let Some(directory) = state_dir() {
            assert!(directory.ends_with(".flowy"));
        }
    }
}
