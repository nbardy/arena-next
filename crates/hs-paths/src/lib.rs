#![deny(unsafe_op_in_unsafe_fn)]

//! macOS Hearthstone path discovery.
//!
//! Discovery is intentionally read-only. A caller must explicitly invoke the
//! log-config crate before ArenaNext changes a user-owned configuration file.

use std::{
    env, fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};
use serde::Serialize;

pub const LOG_COMPONENTS: [&str; 5] = ["LoadingScreen", "Power", "Zone", "Arena", "Asset"];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GamePaths {
    pub install_dir: Option<PathBuf>,
    pub log_config: Option<PathBuf>,
    pub log_roots_checked: Vec<PathBuf>,
    pub log_root: Option<PathBuf>,
    pub latest_session: Option<PathBuf>,
}

pub trait GameLocator {
    fn locate(&self) -> Result<GamePaths>;
}

#[derive(Clone, Debug, Default)]
pub struct MacOsGameLocator;

impl GameLocator for MacOsGameLocator {
    fn locate(&self) -> Result<GamePaths> {
        discover_macos()
    }
}

pub fn discover_macos() -> Result<GamePaths> {
    let home = home_dir().context("could not determine a home directory")?;
    let install_dir = install_candidates(&home)
        .into_iter()
        .find(|path| path.exists());
    let log_roots_checked = log_root_candidates(&home);
    let log_root = log_roots_checked
        .iter()
        .find(|path| path.is_dir() && has_log_components(path))
        .cloned()
        .or_else(|| log_roots_checked.iter().find(|path| path.is_dir()).cloned());

    let log_config = log_config_candidates(&home, install_dir.as_deref())
        .into_iter()
        .find(|path| path.exists())
        .or_else(|| {
            // Keep the conventional location visible in `inspect`, even before
            // the user has ever enabled Hearthstone file logging.
            install_dir
                .as_ref()
                .map(|path| path.join("log.config"))
                .or_else(|| Some(home.join("Library/Preferences/Blizzard/Hearthstone/log.config")))
        });

    let latest_session = log_root.as_deref().and_then(latest_log_session);

    Ok(GamePaths {
        install_dir,
        log_config,
        log_roots_checked,
        log_root,
        latest_session,
    })
}

pub fn latest_log_session(log_root: &Path) -> Option<PathBuf> {
    let mut candidates = vec![log_root.to_path_buf()];
    let Ok(first_level) = fs::read_dir(log_root) else {
        return None;
    };

    for entry in first_level.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        candidates.push(path.clone());
        if let Ok(second_level) = fs::read_dir(&path) {
            for child in second_level.flatten() {
                if child.path().is_dir() {
                    candidates.push(child.path());
                }
            }
        }
    }

    candidates
        .into_iter()
        .filter(|candidate| has_log_components(candidate))
        .max_by_key(|candidate| {
            let coverage = LOG_COMPONENTS
                .iter()
                .filter(|component| candidate.join(format!("{component}.log")).is_file())
                .count() as u128;
            let modified = newest_component_mtime(candidate)
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .unwrap_or_default();
            // Coverage is the primary tiebreaker: a directory with all five
            // components is more useful than a new, incomplete session.
            coverage
                .saturating_mul(10_u128.pow(20))
                .saturating_add(modified)
        })
}

pub fn component_paths(session_dir: &Path) -> Vec<(String, PathBuf)> {
    LOG_COMPONENTS
        .iter()
        .map(|component| {
            (
                component.to_string(),
                session_dir.join(format!("{component}.log")),
            )
        })
        .filter(|(_, path)| path.is_file())
        .collect()
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn install_candidates(home: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(value) = env::var_os("ARENA_NEXT_HEARTHSTONE_DIR") {
        candidates.push(PathBuf::from(value));
    }
    candidates.extend([
        PathBuf::from("/Applications/Hearthstone/Hearthstone.app"),
        PathBuf::from("/Applications/Hearthstone.app"),
        home.join("Applications/Hearthstone/Hearthstone.app"),
    ]);
    candidates
}

fn log_root_candidates(home: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(value) = env::var_os("ARENA_NEXT_LOG_DIR") {
        candidates.push(PathBuf::from(value));
    }
    candidates.extend([
        PathBuf::from("/Applications/Hearthstone/Logs"),
        PathBuf::from("/Applications/Hearthstone.app/Logs"),
        home.join("Library/Preferences/Blizzard/Hearthstone/Logs"),
        home.join("Library/Application Support/Blizzard/Hearthstone/Logs"),
        home.join("Library/Application Support/Blizzard/Hearthstone"),
    ]);
    candidates
}

fn log_config_candidates(home: &Path, install_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(value) = env::var_os("ARENA_NEXT_LOG_CONFIG") {
        candidates.push(PathBuf::from(value));
    }
    if let Some(install_dir) = install_dir {
        candidates.extend([
            install_dir.join("log.config"),
            install_dir.join("Contents/Resources/log.config"),
        ]);
    }
    candidates.extend([
        home.join("Library/Preferences/Blizzard/Hearthstone/log.config"),
        home.join("Library/Application Support/Blizzard/Hearthstone/log.config"),
    ]);
    candidates
}

fn has_log_components(path: &Path) -> bool {
    LOG_COMPONENTS
        .iter()
        .any(|component| path.join(format!("{component}.log")).is_file())
}

fn newest_component_mtime(path: &Path) -> Option<SystemTime> {
    component_paths(path)
        .into_iter()
        .filter_map(|(_, path)| fs::metadata(path).ok()?.modified().ok())
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{Duration, SystemTime},
    };

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("arena-next-{name}-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn finds_latest_complete_session() {
        let root = unique_temp_dir("sessions");
        let old = root.join("old");
        let current = root.join("current");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&current).unwrap();
        fs::write(old.join("Power.log"), "old").unwrap();
        fs::write(current.join("Power.log"), "current").unwrap();
        fs::write(current.join("Zone.log"), "current").unwrap();
        std::thread::sleep(Duration::from_millis(2));
        fs::write(current.join("Arena.log"), "current").unwrap();

        assert_eq!(latest_log_session(&root), Some(current));
        fs::remove_dir_all(root).unwrap();
    }
}
