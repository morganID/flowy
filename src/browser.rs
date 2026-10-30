//! Stable Safari extension session id, and the Safari profile flowy is pinned to.
//!
//! safari-driver names its extension session after the calling process
//! (`safari-driver-<pid>-<nanos>`), so a fresh process owns no tab and has to open a
//! new one. flowy pins one id on disk instead: the extension then still recognises the
//! tab this user opened earlier and it can be adopted instead of duplicated.
//!
//! The profile matters for the same reason. safari-driver reads `SAFARI_PROFILE` from the
//! environment on every command, and an unset variable means "no verification" — the extension
//! then uses whichever profile its own worker belongs to. Running a command from a terminal
//! without the variable is enough to land in the personal profile while the automation profile
//! sits untouched, which looks exactly like "the tool did nothing".

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::state::{state_dir, write_private};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserState {
    /// Shared with the Safari extension, which keys tab ownership on it.
    pub session_id: String,
    /// The Safari profile to drive, as named in Safari. Empty means "whatever the extension
    /// resolves", which is how a run ends up in the wrong profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl BrowserState {
    /// The id to use for this process: `SAFARI_MCP_SESSION_ID` when the user pins one,
    /// otherwise the stored id, generated on first use.
    pub fn load() -> Self {
        if let Some(configured) = std::env::var("SAFARI_MCP_SESSION_ID")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| is_valid_session_id(value))
        {
            return Self {
                session_id: configured,
                profile: stored_profile(),
            };
        }
        if let Some(path) = Self::path() {
            match Self::load_from(&path) {
                Ok(Some(state)) => return state,
                Ok(None) => {}
                Err(error) => eprintln!(
                    "flowy: warning: ignoring the stored Safari session id ({error:#}); generating a new one"
                ),
            }
        }
        let state = Self {
            session_id: new_session_id(),
            profile: stored_profile(),
        };
        if let Some(path) = Self::path()
            && let Err(error) = state.save_to(&path)
        {
            eprintln!(
                "flowy: warning: could not store the Safari session id ({error:#}); a new tab may be opened"
            );
        }
        state
    }

    /// Pin the Safari profile. `None` clears the pin, which returns flowy to the extension's
    /// own resolution.
    pub fn pin_profile(profile: Option<&str>) -> Result<Option<String>> {
        let mut state = Self::load();
        state.profile = profile
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if let Some(value) = &state.profile
            && !is_valid_profile(value)
        {
            bail!("{value:?} is not a usable Safari profile name");
        }
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("there is no home directory"))?;
        state.save_to(&path)?;
        Ok(state.profile)
    }

    fn path() -> Option<PathBuf> {
        Some(state_dir()?.join("browser.json"))
    }

    fn load_from(path: &Path) -> Result<Option<Self>> {
        let contents = match fs::read(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        };
        let state: Self = serde_json::from_slice(&contents)
            .with_context(|| format!("could not parse {}", path.display()))?;
        Ok(is_valid_session_id(&state.session_id).then_some(state))
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        let body =
            serde_json::to_vec_pretty(self).context("could not encode the Safari session id")?;
        write_private(path, &body)
    }
}

fn is_valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Safari profile names are the user's own, so this only rules out values that cannot be a
/// name at all. The driver passes the string to AppleScript, which filters it further.
fn is_valid_profile(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.chars().count() <= 64 && !value.chars().any(char::is_control)
}

/// The profile recorded on disk, read directly so that [`BrowserState::load`] cannot recurse.
fn stored_profile() -> Option<String> {
    let contents = fs::read(BrowserState::path()?).ok()?;
    let state: BrowserState = serde_json::from_slice(&contents).ok()?;
    state.profile.filter(|value| is_valid_profile(value))
}

/// Hand the pinned profile to safari-driver, which reads `SAFARI_PROFILE` on every command.
///
/// Without it the extension resolves a profile on its own, and a run can silently drive the
/// personal profile while the automation profile sits untouched.
pub fn export_profile() {
    let already_set = std::env::var("SAFARI_PROFILE")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if already_set {
        return;
    }
    let Some(profile) = stored_profile() else {
        eprintln!(
            "flowy: warning: no Safari profile is pinned, so the extension chooses one — \
             pin it with `flowy profile <name>`"
        );
        return;
    };
    // Safe here: main() calls this before any thread or driver call exists, and the driver
    // reads the variable synchronously on each command.
    unsafe { std::env::set_var("SAFARI_PROFILE", profile) };
}

fn new_session_id() -> String {
    format!("flowy-{}", random_hex(16))
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    if let Ok(mut urandom) = fs::File::open("/dev/urandom")
        && urandom.read_exact(&mut buffer).is_ok()
    {
        return buffer.iter().map(|byte| format!("{byte:02x}")).collect();
    }
    // Fallback when /dev/urandom is unreadable: time mixed with the process id.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{:032x}", nanos ^ (u128::from(std::process::id()) << 64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("flowy-browser-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        directory.join("browser.json")
    }

    #[test]
    fn generated_ids_are_extension_safe() {
        let id = new_session_id();
        assert!(id.starts_with("flowy-"));
        assert!(is_valid_session_id(&id));
        assert_ne!(id, new_session_id());
    }

    #[test]
    fn round_trips_the_session_id() {
        let path = scratch("round-trip");
        let state = BrowserState {
            session_id: "flowy-0123456789abcdef".into(),
            profile: None,
        };
        state.save_to(&path).unwrap();
        assert_eq!(BrowserState::load_from(&path).unwrap(), Some(state));
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn keeps_the_pinned_profile_with_the_session() {
        let state = BrowserState {
            session_id: "flowy-0123456789abcdef".into(),
            profile: Some("BOT".into()),
        };
        let encoded = serde_json::to_vec(&state).unwrap();
        let decoded: BrowserState = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.profile.as_deref(), Some("BOT"));
        // A state written by an older build has no profile key at all.
        let legacy: BrowserState = serde_json::from_slice(br#"{"session_id":"flowy-aa"}"#).unwrap();
        assert_eq!(legacy.profile, None);
        assert!(is_valid_profile("BOT"));
        assert!(is_valid_profile("אוטומציות"));
        assert!(!is_valid_profile("   "));
    }

    #[test]
    fn rejects_unusable_stored_ids() {
        let path = scratch("invalid");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"session_id":"has space"}"#).unwrap();
        assert_eq!(BrowserState::load_from(&path).unwrap(), None);
        assert_eq!(BrowserState::load_from(&scratch("absent")).unwrap(), None);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
