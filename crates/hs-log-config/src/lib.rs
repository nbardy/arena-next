#![deny(unsafe_op_in_unsafe_fn)]

//! Safe inspection and explicit updating of Hearthstone's `log.config`.
//!
//! `inspect` never writes. `enable_file_logging` is intentionally separate so
//! a CLI can require an explicit `--write` acknowledgement before touching a
//! user-owned Blizzard configuration file.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;

pub const REQUIRED_COMPONENTS: [&str; 5] = ["LoadingScreen", "Power", "Zone", "Arena", "Asset"];

/// Per-component file size cap in kilobytes written by `enable_file_logging`.
///
/// Hearthstone's default cap is 10000 KB (10 MB). Observed failure mode
/// (2026-08): when `Zone.log` hits that cap the client prints "Truncating log,
/// which has reached the size limit" but the truncation fails on macOS and
/// every component writer stalls for the rest of the session while the game
/// keeps running. Raising the cap gives a long play session room to log
/// without ever triggering that path. The observer surfaces the eventual
/// stall as `LogStaleness` regardless.
pub const LOG_FILE_SIZE_KB: u32 = 200_000;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoggingStatus {
    pub path: PathBuf,
    pub exists: bool,
    pub components: Vec<ComponentLoggingStatus>,
    /// `true` only when `enable_file_logging` wrote a different configuration
    /// during this operation. Read-only inspection always reports `false`.
    pub configuration_changed: bool,
    pub change_required: bool,
    /// `true` only if this operation changed `log.config`, so the running
    /// Hearthstone process must be restarted by the user to read it.
    pub hearthstone_restart_required: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentLoggingStatus {
    pub component: String,
    pub section_present: bool,
    pub log_level_one: bool,
    pub file_printing: bool,
    pub verbose: Option<bool>,
    pub file_size_kb: Option<u32>,
    pub enabled: bool,
}

/// A read-only preview of the precise configuration ArenaNext would write if
/// the user later opts in to enabling file logging.
///
/// The preview deliberately carries both statuses: current describes the file
/// on disk, while proposed describes the generated replacement. It never
/// creates a parent directory, a temporary file, or a backup.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoggingDiff {
    pub path: PathBuf,
    pub exists: bool,
    pub would_change: bool,
    pub current: LoggingStatus,
    pub proposed: LoggingStatus,
    pub unified_diff: String,
}

/// The result of explicitly restoring one ArenaNext-created backup.
///
/// preserved_current_backup is a fresh, app-created backup of the config that
/// was replaced. This makes restore reversible without deleting the selected
/// source backup.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoggingRestore {
    pub path: PathBuf,
    pub restored_from: PathBuf,
    pub preserved_current_backup: Option<PathBuf>,
    pub configuration_changed: bool,
    pub hearthstone_restart_required: bool,
}

pub fn inspect(path: impl AsRef<Path>) -> Result<LoggingStatus> {
    let path = path.as_ref().to_path_buf();
    let exists = path.is_file();
    let source = if exists {
        fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?
    } else {
        String::new()
    };
    Ok(status_from_source(path, exists, &source))
}

/// Builds a read-only preview of the required logging patch.
///
/// This is intentionally separate from enable_file_logging, so command line
/// callers can show users the exact proposal before any write path is reached.
pub fn preview_file_logging(path: impl AsRef<Path>) -> Result<LoggingDiff> {
    let path = path.as_ref().to_path_buf();
    let exists = path.is_file();
    let original = if exists {
        fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))?
    } else {
        String::new()
    };
    let updated = with_required_sections(&original);
    let would_change = updated != original;
    let current = status_from_source(path.clone(), exists, &original);
    // If this proposal were applied, a config file would exist even when the
    // current inspection found no file. This is only a virtual status; no
    // directory or file is created by previewing it.
    let proposed = status_from_source(path.clone(), true, &updated);

    Ok(LoggingDiff {
        path: path.clone(),
        exists,
        would_change,
        current,
        proposed,
        unified_diff: unified_diff(&path, &original, &updated),
    })
}

/// Returns the newest backup that matches ArenaNext's strict, timestamped
/// backup filename for this exact log.config path. Unrelated files, links,
/// and backups for a different configuration are never considered.
pub fn latest_arena_next_backup(path: impl AsRef<Path>) -> Result<Option<PathBuf>> {
    let path = path.as_ref();
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    if !parent.is_dir() {
        return Ok(None);
    }

    let mut latest = None::<(u128, PathBuf)>;
    for entry in
        fs::read_dir(parent).with_context(|| format!("could not list {}", parent.display()))?
    {
        let entry = entry.with_context(|| format!("could not read {}", parent.display()))?;
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // DirEntry::file_type does not follow symlinks. A symlink with a
        // convincing filename is deliberately not an acceptable backup.
        if !file_type.is_file() {
            continue;
        }
        let candidate = entry.path();
        let Some(timestamp) = arena_next_backup_timestamp(path, &candidate) else {
            continue;
        };
        if latest
            .as_ref()
            .is_none_or(|(current_timestamp, _)| timestamp > *current_timestamp)
        {
            latest = Some((timestamp, candidate));
        }
    }
    Ok(latest.map(|(_, path)| path))
}

/// Restores a config only from a valid ArenaNext-created backup in the same
/// directory. The destination must be an existing regular file. The current
/// config is first copied to a fresh app-created backup, and the actual
/// replacement is atomic. No process is restarted by this function.
pub fn restore_from_backup(
    path: impl AsRef<Path>,
    backup: impl AsRef<Path>,
) -> Result<LoggingRestore> {
    let path = path.as_ref();
    let backup = backup.as_ref();
    require_regular_file(path, "log.config")?;
    validate_arena_next_backup(path, backup)?;

    let current =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let restored = fs::read_to_string(backup)
        .with_context(|| format!("could not read backup {}", backup.display()))?;
    let configuration_changed = current != restored;
    let preserved_current_backup = if configuration_changed {
        let preserved = create_backup(path)?;
        let parent = path
            .parent()
            .context("log.config has no parent directory")?;
        write_atomically(path, parent, &restored)?;
        Some(preserved)
    } else {
        None
    };

    Ok(LoggingRestore {
        path: path.to_path_buf(),
        restored_from: backup.to_path_buf(),
        preserved_current_backup,
        configuration_changed,
        // Hearthstone reads log.config on its own startup. We only report
        // this; callers must never terminate or restart the game for users.
        hearthstone_restart_required: configuration_changed,
    })
}

fn status_from_source(path: PathBuf, exists: bool, source: &str) -> LoggingStatus {
    let sections = parse_sections(&source);
    let components = REQUIRED_COMPONENTS
        .iter()
        .map(|component| component_status(component, sections.get(*component)))
        .collect::<Vec<_>>();
    let change_required = components.iter().any(|component| !component.enabled);

    LoggingStatus {
        path,
        exists,
        components,
        configuration_changed: false,
        change_required,
        // Inspection never changes Hearthstone's configuration, therefore it
        // can never itself make a restart necessary.
        hearthstone_restart_required: false,
    }
}

/// Enables the five log components using an atomic replace and preserves a
/// timestamped backup if a file already existed. Callers must obtain explicit
/// user intent before invoking this function.
pub fn enable_file_logging(path: impl AsRef<Path>) -> Result<LoggingStatus> {
    let path = path.as_ref();
    let original = if path.exists() {
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?
    } else {
        String::new()
    };
    let updated = with_required_sections(&original);
    let configuration_changed = updated != original;
    if configuration_changed {
        let parent = path
            .parent()
            .context("log.config has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        if path.exists() {
            create_backup(path)?;
        }
        write_atomically(path, parent, &updated)?;
    }
    let mut status = inspect(path)?;
    status.configuration_changed = configuration_changed;
    // A running client only reads this configuration at startup. Do not
    // restart or terminate it; merely report the consequence of an explicit
    // successful write.
    status.hearthstone_restart_required = configuration_changed;
    Ok(status)
}

fn component_status(
    component: &str,
    settings: Option<&BTreeMap<String, String>>,
) -> ComponentLoggingStatus {
    let section_present = settings.is_some();
    let log_level_one = setting_is(settings, "LogLevel", "1");
    let file_printing = setting_is(settings, "FilePrinting", "true");
    let verbose = (component == "Power").then(|| setting_is(settings, "Verbose", "1"));
    let file_size_kb = settings
        .and_then(|settings| settings.get("filesize"))
        .and_then(|value| value.parse::<u32>().ok());
    let enabled =
        section_present && log_level_one && file_printing && verbose.is_none_or(|value| value);
    ComponentLoggingStatus {
        component: component.to_owned(),
        section_present,
        log_level_one,
        file_printing,
        verbose,
        file_size_kb,
        enabled,
    }
}

fn setting_is(settings: Option<&BTreeMap<String, String>>, key: &str, expected: &str) -> bool {
    settings
        .and_then(|settings| settings.get(&key.to_ascii_lowercase()))
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn parse_sections(source: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut sections = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut active = None::<String>;
    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim().to_owned();
            sections.entry(name.clone()).or_default();
            active = Some(name);
        } else if let (Some(section), Some((key, value))) =
            (active.as_deref(), line.split_once('='))
        {
            sections.entry(section.to_owned()).or_default().insert(
                key.trim().to_ascii_lowercase(),
                value
                    .find([';', '#'])
                    .map(|index| &value[..index])
                    .unwrap_or(value)
                    .trim()
                    .to_owned(),
            );
        }
    }
    sections
}

fn with_required_sections(original: &str) -> String {
    let line_ending = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut sections = split_sections(original);
    for component in REQUIRED_COMPONENTS {
        let settings = required_component_settings(component);
        let mut found = false;
        for section in &mut sections {
            if section
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(component))
            {
                found = true;
                for (key, value) in &settings {
                    upsert_setting(&mut section.lines, key, value);
                }
            }
        }
        if !found {
            let mut lines = vec![format!("[{component}]")];
            for (key, value) in &settings {
                upsert_setting(&mut lines, key, value);
            }
            sections.push(IniSection {
                name: Some(component.to_owned()),
                lines,
            });
        }
    }
    sections
        .into_iter()
        .flat_map(|section| section.lines)
        .collect::<Vec<_>>()
        .join(line_ending)
        + line_ending
}

/// The settings ArenaNext guarantees for one required component. `FileSize`
/// is included so a normal long play session does not hit Hearthstone's
/// default 10 MB cap and trigger the stall described on [`LOG_FILE_SIZE_KB`].
fn required_component_settings(component: &str) -> Vec<(&'static str, String)> {
    let mut settings = vec![
        ("LogLevel", "1".to_owned()),
        ("FilePrinting", "true".to_owned()),
        ("FileSize", LOG_FILE_SIZE_KB.to_string()),
    ];
    if component == "Power" {
        settings.push(("Verbose", "1".to_owned()));
    }
    settings
}

#[derive(Clone, Debug)]
struct IniSection {
    name: Option<String>,
    lines: Vec<String>,
}

fn split_sections(source: &str) -> Vec<IniSection> {
    let mut sections = vec![IniSection {
        name: None,
        lines: Vec::new(),
    }];
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            sections.push(IniSection {
                name: Some(trimmed[1..trimmed.len() - 1].trim().to_owned()),
                lines: vec![line.to_owned()],
            });
        } else if let Some(section) = sections.last_mut() {
            section.lines.push(line.to_owned());
        }
    }
    sections
}

fn upsert_setting(lines: &mut Vec<String>, key: &str, value: &str) {
    let mut found = false;
    for line in lines.iter_mut().skip(1) {
        let Some((existing_key, _)) = line.split_once('=') else {
            continue;
        };
        if existing_key.trim().eq_ignore_ascii_case(key) {
            *line = replacement_setting_line(line, key, value);
            found = true;
        }
    }
    if !found {
        lines.push(format!("{key}={value}"));
    }
}

fn replacement_setting_line(line: &str, key: &str, value: &str) -> String {
    let indentation = &line[..line.len() - line.trim_start().len()];
    let (_, existing_value) = line
        .split_once('=')
        .expect("replacement is called only for assignment lines");
    // Preserve a familiar INI trailing comment, including its spacing. A
    // commented-out assignment is never matched above because its key starts
    // with `;` or `#`.
    let comment = existing_value
        .char_indices()
        .find_map(|(index, character)| {
            matches!(character, ';' | '#').then(|| {
                let prefix = &existing_value[..index];
                let whitespace = &prefix[prefix.trim_end().len()..];
                format!("{whitespace}{}", &existing_value[index..])
            })
        })
        .unwrap_or_default();
    format!("{indentation}{key}={value}{comment}")
}

fn backup_path(path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("log.config");
    path.with_file_name(format!(
        "{filename}.arena-next-backup-{}-{timestamp}",
        std::process::id()
    ))
}

fn create_backup(path: &Path) -> Result<PathBuf> {
    let backup = backup_path(path);
    let mut source = fs::File::open(path)
        .with_context(|| format!("could not open {} for backup", path.display()))?;
    let mut destination = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&backup)
        .with_context(|| format!("could not create backup {}", backup.display()))?;
    io::copy(&mut source, &mut destination).with_context(|| {
        format!(
            "could not back up {} to {}",
            path.display(),
            backup.display()
        )
    })?;
    destination
        .sync_all()
        .with_context(|| format!("could not flush backup {}", backup.display()))?;
    Ok(backup)
}

fn require_regular_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {label} {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "refusing to use {}: it must be an existing regular file",
            path.display()
        );
    }
    Ok(())
}

fn validate_arena_next_backup(config: &Path, backup: &Path) -> Result<()> {
    require_regular_file(backup, "backup")?;
    let config_parent = config
        .parent()
        .context("log.config has no parent directory")?;
    let backup_parent = backup.parent().context("backup has no parent directory")?;
    let config_parent = fs::canonicalize(config_parent)
        .with_context(|| format!("could not resolve {}", config_parent.display()))?;
    let backup_parent = fs::canonicalize(backup_parent)
        .with_context(|| format!("could not resolve {}", backup_parent.display()))?;
    if config_parent != backup_parent {
        bail!(
            "refusing backup {}: ArenaNext backups must live beside this log.config",
            backup.display()
        );
    }
    if arena_next_backup_timestamp(config, backup).is_none() {
        bail!(
            "refusing backup {}: it is not an ArenaNext timestamped backup for {}",
            backup.display(),
            config.display()
        );
    }
    Ok(())
}

fn arena_next_backup_timestamp(config: &Path, candidate: &Path) -> Option<u128> {
    let config_filename = config.file_name()?.to_str()?;
    let candidate_filename = candidate.file_name()?.to_str()?;
    let prefix = format!("{config_filename}.arena-next-backup-");
    let suffix = candidate_filename.strip_prefix(&prefix)?;
    let (process_id, timestamp) = suffix.split_once('-')?;
    if process_id.is_empty()
        || !process_id.bytes().all(|byte| byte.is_ascii_digit())
        || timestamp.is_empty()
        || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    process_id.parse::<u32>().ok()?;
    timestamp.parse::<u128>().ok()
}

fn unified_diff(path: &Path, original: &str, updated: &str) -> String {
    if original == updated {
        return String::new();
    }

    let before = logical_lines(original);
    let after = logical_lines(updated);
    let mut output = String::new();
    let _ = writeln!(&mut output, "--- {}", path.display());
    let _ = writeln!(&mut output, "+++ {} (proposed)", path.display());

    // log.config is normally tiny. Cap the LCS table anyway so a malformed
    // user file cannot make a read-only logging diff consume unbounded RAM.
    const MAX_LCS_CELLS: usize = 1_000_000;
    let Some(cells) = before.len().checked_add(1).and_then(|left| {
        after
            .len()
            .checked_add(1)
            .and_then(|right| left.checked_mul(right))
    }) else {
        append_large_diff_notice(&mut output, original, updated);
        return output;
    };
    if cells > MAX_LCS_CELLS {
        append_large_diff_notice(&mut output, original, updated);
        return output;
    }

    let width = after.len() + 1;
    let mut lcs = vec![0_u32; cells];
    for before_index in (0..before.len()).rev() {
        for after_index in (0..after.len()).rev() {
            let index = before_index * width + after_index;
            lcs[index] = if before[before_index] == after[after_index] {
                lcs[(before_index + 1) * width + after_index + 1] + 1
            } else {
                lcs[(before_index + 1) * width + after_index]
                    .max(lcs[before_index * width + after_index + 1])
            };
        }
    }

    let mut before_index = 0;
    let mut after_index = 0;
    while before_index < before.len() || after_index < after.len() {
        if before_index < before.len()
            && after_index < after.len()
            && before[before_index] == after[after_index]
        {
            let _ = writeln!(&mut output, " {}", before[before_index]);
            before_index += 1;
            after_index += 1;
        } else if before_index < before.len()
            && (after_index == after.len()
                || lcs[(before_index + 1) * width + after_index]
                    >= lcs[before_index * width + after_index + 1])
        {
            let _ = writeln!(&mut output, "-{}", before[before_index]);
            before_index += 1;
        } else {
            let _ = writeln!(&mut output, "+{}", after[after_index]);
            after_index += 1;
        }
    }
    if before == after {
        let _ = writeln!(&mut output, "\\ No newline at end of current configuration");
    }
    output
}

fn logical_lines(source: &str) -> Vec<&str> {
    source
        .split_terminator('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

fn append_large_diff_notice(output: &mut String, original: &str, updated: &str) {
    let _ = writeln!(
        output,
        "@@ full line diff omitted: configuration is too large for an in-memory LCS preview @@"
    );
    let _ = writeln!(output, "-{} bytes in current configuration", original.len());
    let _ = writeln!(output, "+{} bytes in proposed configuration", updated.len());
}

fn temporary_path(path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("log.config");
    path.with_file_name(format!(
        ".{filename}.arena-next-{}-{timestamp}.tmp",
        std::process::id()
    ))
}

/// Writes beside the original, flushes the new file, then atomically renames
/// it into place. Existing file permissions are retained when the platform
/// exposes them. A failed write leaves the original configuration untouched.
fn write_atomically(path: &Path, parent: &Path, updated: &str) -> Result<()> {
    let permissions = path
        .is_file()
        .then(|| fs::metadata(path).map(|metadata| metadata.permissions()))
        .transpose()
        .with_context(|| format!("could not read permissions for {}", path.display()))?;
    let temp = temporary_path(path);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .with_context(|| format!("could not create {}", temp.display()))?;
    if let Some(permissions) = permissions {
        fs::set_permissions(&temp, permissions)
            .with_context(|| format!("could not preserve permissions on {}", temp.display()))?;
    }
    file.write_all(updated.as_bytes())
        .with_context(|| format!("could not write {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("could not flush {}", temp.display()))?;
    drop(file);
    fs::rename(&temp, path).with_context(|| format!("could not replace {}", path.display()))?;
    // Best-effort directory sync makes the rename durable on platforms that
    // support synchronizing directories. The successful rename remains the
    // authoritative user-visible result if a filesystem does not support it.
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env, fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_TEST_PATH: AtomicU64 = AtomicU64::new(0);

    fn temp_config() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_TEST_PATH.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "arena-next-log-config-{}-{timestamp}-{sequence}.config",
            std::process::id()
        ))
    }

    #[test]
    fn missing_sections_are_reported_and_enabled() {
        let path = temp_config();
        fs::write(&path, "[Power]\nLogLevel=1\nFilePrinting=true\nVerbose=1\n").unwrap();
        let before = inspect(&path).unwrap();
        assert!(before.change_required);
        assert!(!before.configuration_changed);
        assert!(!before.hearthstone_restart_required);

        let after = enable_file_logging(&path).unwrap();
        assert!(!after.change_required);
        assert!(after.configuration_changed);
        assert!(after.hearthstone_restart_required);
        assert!(after.components.iter().all(|component| component.enabled));

        let unchanged = enable_file_logging(&path).unwrap();
        assert!(!unchanged.configuration_changed);
        assert!(!unchanged.hearthstone_restart_required);
        let backup = latest_arena_next_backup(&path).unwrap().unwrap();
        fs::remove_file(backup).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn repairs_values_in_place_without_duplicate_sections() {
        let source = "[Arena]\r\n  LogLevel = 0 ; keep this comment\r\nFilePrinting=false # and this one\r\n[Custom]\r\nKeep=true\r\n";
        let updated = with_required_sections(source);
        assert_eq!(updated.matches("[Arena]").count(), 1);
        assert!(updated.contains(
            "[Arena]\r\n  LogLevel=1 ; keep this comment\r\nFilePrinting=true # and this one"
        ));
        assert!(updated.contains("[Custom]\r\nKeep=true"));
        assert!(
            inspect_from_source(&updated)
                .components
                .iter()
                .all(|status| status.enabled)
        );
    }

    #[test]
    fn preview_is_read_only_and_shows_the_proposed_patch() {
        let path = temp_config();
        let original = "[Power]\nLogLevel=0\nFilePrinting=false\nVerbose=0\n";
        fs::write(&path, original).unwrap();

        let preview = preview_file_logging(&path).unwrap();

        assert!(preview.would_change);
        assert!(preview.current.change_required);
        assert!(!preview.proposed.change_required);
        assert!(preview.unified_diff.contains("-LogLevel=0"));
        assert!(preview.unified_diff.contains("+LogLevel=1"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(latest_arena_next_backup(&path).unwrap().is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn latest_restore_only_uses_matching_app_backups_and_preserves_current_config() {
        let path = temp_config();
        let current = "[Custom]\nCurrent=true\n";
        let restored_contents = "[Power]\nLogLevel=1\nFilePrinting=true\nVerbose=1\n";
        fs::write(&path, current).unwrap();

        let filename = path.file_name().unwrap().to_str().unwrap();
        let older = path.with_file_name(format!("{filename}.arena-next-backup-7-100"));
        let newest = path.with_file_name(format!("{filename}.arena-next-backup-7-200"));
        let unrelated = path.with_file_name("other.config.arena-next-backup-7-999");
        let malformed = path.with_file_name(format!("{filename}.arena-next-backup-nope-999"));
        fs::write(&older, "older backup").unwrap();
        fs::write(&newest, restored_contents).unwrap();
        fs::write(&unrelated, "unrelated").unwrap();
        fs::write(&malformed, "not an accepted backup").unwrap();

        assert_eq!(
            latest_arena_next_backup(&path).unwrap(),
            Some(newest.clone())
        );
        assert!(restore_from_backup(&path, &malformed).is_err());
        assert!(restore_from_backup(&path, &unrelated).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), current);

        let outcome = restore_from_backup(&path, &newest).unwrap();
        assert!(outcome.configuration_changed);
        assert!(outcome.hearthstone_restart_required);
        assert_eq!(fs::read_to_string(&path).unwrap(), restored_contents);
        let preserved = outcome.preserved_current_backup.unwrap();
        assert_eq!(fs::read_to_string(&preserved).unwrap(), current);
        assert_eq!(fs::read_to_string(&newest).unwrap(), restored_contents);

        fs::remove_file(preserved).unwrap();
        fs::remove_file(older).unwrap();
        fs::remove_file(newest).unwrap();
        fs::remove_file(unrelated).unwrap();
        fs::remove_file(malformed).unwrap();
        fs::remove_file(path).unwrap();
    }

    fn inspect_from_source(source: &str) -> LoggingStatus {
        let sections = parse_sections(source);
        let components = REQUIRED_COMPONENTS
            .iter()
            .map(|component| component_status(component, sections.get(*component)))
            .collect::<Vec<_>>();
        LoggingStatus {
            path: PathBuf::from("fixture.config"),
            exists: true,
            configuration_changed: false,
            change_required: components.iter().any(|component| !component.enabled),
            hearthstone_restart_required: false,
            components,
        }
    }

    #[test]
    fn required_sections_raise_the_file_size_cap() {
        let updated = with_required_sections("");
        let status = inspect_from_source(&updated);
        for component in &status.components {
            assert_eq!(
                component.file_size_kb,
                Some(LOG_FILE_SIZE_KB),
                "{}",
                component.component
            );
            assert!(component.enabled, "{}", component.component);
        }
    }
}
