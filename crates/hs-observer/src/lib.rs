#![deny(unsafe_op_in_unsafe_fn)]

//! Small, local Hearthstone log observer shared by the diagnostic CLI and the
//! native application.
//!
//! This crate is deliberately UI-free. It tails only conventional log files,
//! keeps a deterministic reducer, and returns a fully resolved snapshot. It
//! never modifies Hearthstone or reads process memory. It rewrites a component
//! log in place only to rotate an overlarge file before the game's own
//! truncation stalls its writers ([`rotate_overlarge_component_logs`]).

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use anyhow::{Context, Result};
use hs_card_data::{CardCache, CardResolution};
use hs_log_parser::{
    ComponentLogReader, HearthstoneLogParser, LogComponent, LogCursor, ParserCheckpoint,
    REQUIRED_COMPONENTS, RawLogLine, component_can_emit_events, find_last_line_containing,
    tail_cursor, timestamp_key,
};
use hs_paths::GamePaths;
use hs_state::{
    ArenaReducer, ArenaReducerCheckpoint, ArenaSnapshot, DeckCard, EventSource, GameEvent,
    RedraftPolicy, is_real_card_id,
};
use serde::{Deserialize, Serialize};

/// Public snapshot schema. Increment it if a field changes incompatibly.
pub const OBSERVER_SNAPSHOT_SCHEMA_VERSION: u32 = 5;

/// Persisted observer format. Any incompatible parser/reducer/cursor change
/// must increment this, causing a deliberately safe current-deck resync
/// instead of a best-effort resume from stale state.
pub const OBSERVER_CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const MAX_CHECKPOINT_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum Arena bytes parsed after a completed authoritative deck snapshot
/// at cold live startup. The raw reverse search itself is not capped: it walks
/// fixed-size byte chunks until it finds the newest snapshot sentinel, without
/// materializing historical log records.
pub const TAIL_SNAPSHOT_SUFFIX_BYTES: u64 = 256 * 1024;
/// A valid warm checkpoint may advance through a small newly appended suffix,
/// but it must not turn ordinary live startup back into a historical replay
/// when the application has been stopped for a long session.
const CHECKPOINT_CATCH_UP_BYTES: u64 = TAIL_SNAPSHOT_SUFFIX_BYTES;
/// A complete Arena deck snapshot is normally only a few kilobytes. This
/// bound makes a malformed log fail closed into `awaiting_snapshot` rather
/// than turning live attach into another history replay.
const TAIL_SNAPSHOT_PARSE_BYTES: u64 = 512 * 1024;
const ARENA_SNAPSHOT_SENTINEL: &[u8] = b"Draft Deck ID:";
/// A later `OnBegin` means the game has started a new Arena run whose deck
/// snapshot may not have arrived yet. Treat any later Arena `OnBegin` line as
/// a conservative invalidation of an older snapshot; waiting is preferable to
/// briefly showing the prior run's deck.
const ARENA_RUN_START_SENTINEL: &[u8] = b"OnBegin";
static CHECKPOINT_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedDeckCard {
    pub card_id: String,
    pub count: u8,
    pub resolution: CardResolution,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObserverSnapshot {
    /// Version for the observer envelope. `state_schema_version` is owned by
    /// `hs-state` and versioned separately.
    pub schema_version: u32,
    pub state_schema_version: u32,
    pub mode: hs_state::GameMode,
    pub hero_class: Option<hs_state::HeroClass>,
    pub deck: Vec<ResolvedDeckCard>,
    /// Per-game remaining local deck with presentation metadata joined.
    pub remaining_deck: Vec<ResolvedDeckCard>,
    pub deck_state: hs_state::DeckState,
    pub run: hs_state::ArenaRunState,
    pub draft: hs_state::DraftState,
    pub game: hs_state::GameState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PollResult {
    /// A new record was consumed, a session changed, or the current deck was
    /// resynchronized after truncation/rotation.
    pub changed: bool,
    /// A tailing hazard (rotation, late component, or out-of-order record)
    /// caused a current-deck resync instead of a fatal observer error.
    pub recovered_from_rotation: bool,
    /// The observer attached to a newer session directory.
    pub switched_session: bool,
    /// A rotation or a newly appearing session is not yet stable enough to
    /// replay. The prior verified snapshot remains available; call `poll`
    /// again rather than treating this as a fatal observer error.
    pub recovery_pending: bool,
}

/// Result of attempting a warm restore. `Rejected` is non-fatal: callers get
/// a fresh bounded current-state attach in that case, never an unchecked
/// checkpoint or an implicit history replay.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum CheckpointRestoreStatus {
    Restored,
    NotFound,
    Rejected { reason: String },
}

/// How the current observer state was established.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMethod {
    VerifiedCheckpoint,
    /// A current authoritative deck was hydrated from the newest completed
    /// Arena snapshot; historic draft/game state was intentionally not read.
    TailSnapshot,
    /// A new run began after the newest completed deck snapshot. Only the
    /// bounded Arena suffix beginning at that run marker was replayed, which
    /// recovers an in-progress hero/card draft without reviving the old deck.
    TailRun,
    /// No completed current snapshot was available. The observer is tailing
    /// for one, with reduced/unknown product state rather than stale history.
    AwaitingSnapshot,
    FullReplay,
}

/// Lightweight evidence for why startup did or did not read history.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachDiagnostics {
    pub method: AttachMethod,
    pub snapshot_byte_offset: Option<u64>,
    pub snapshot_bytes_parsed: u64,
    pub arena_suffix_bytes_skipped: u64,
    pub arena_suffix_truncated: bool,
    /// A later raw Arena `OnBegin` was found after the newest otherwise
    /// complete snapshot, so that snapshot was deliberately not hydrated.
    pub snapshot_invalidated_by_newer_run: bool,
    /// Bytes consumed after a validated checkpoint before the restored state
    /// was made visible. This is always bounded by
    /// [`CHECKPOINT_CATCH_UP_BYTES`].
    pub checkpoint_suffix_bytes_replayed: u64,
    /// Live tail attach starts LoadingScreen, Power, Zone, and Asset at their
    /// final complete-line cursor; no historical gameplay events are parsed.
    pub non_arena_components_started_at_tail: bool,
}

impl Default for AttachMethod {
    fn default() -> Self {
        Self::FullReplay
    }
}

/// Whether a session's newest required component log is still advancing.
///
/// Observed failure mode (2026-08): a component reaching Hearthstone's hard
/// size cap (commonly the 10 MB `Zone.log` ceiling) stalls the client's log
/// writers, freezing every component log mid-session while the game keeps
/// running. Deck/card detection then silently stops forever. Surfacing this
/// as explicit state lets the overlay and doctor tell the player to restart
/// Hearthstone instead of showing an eternal "waiting" screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum LogStaleness {
    /// The newest required component log advanced within the threshold.
    Live,
    /// The newest required component log last advanced `age_secs` seconds ago.
    Stale { age_secs: u64 },
    /// The session has no supported component log to measure.
    NoLogs,
}

/// Default threshold for [`session_staleness`]. Ten minutes without any
/// required component write is treated as stalled logging, not an idle game.
pub const LOG_STALENESS_THRESHOLD: Duration = Duration::from_secs(10 * 60);

#[derive(Debug)]
struct LogReplayRequired {
    path: Option<PathBuf>,
    reason: &'static str,
}

impl fmt::Display for LogReplayRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(formatter, "{}; {}", path.display(), self.reason),
            None => formatter.write_str(self.reason),
        }
    }
}

impl Error for LogReplayRequired {}

/// One durable cursor together with enough file evidence to prove that the
/// bytes before it still belong to the state being resumed.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedCursor {
    cursor: LogCursor,
    file_identity: FileIdentity,
    tail_checkpoint: Option<FileCheckpoint>,
}

/// On-disk warm-start state. This is private on purpose: consumers should use
/// `attach_with_checkpoint` rather than attempting to deserialize and trust a
/// checkpoint themselves.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ObserverCheckpoint {
    schema_version: u32,
    observer_snapshot_schema_version: u32,
    state_schema_version: u32,
    /// Canonical directory identity, not merely a session-directory basename.
    session_path: String,
    session_id: String,
    cursors: BTreeMap<LogComponent, PersistedCursor>,
    last_line_order: Option<RawLineOrder>,
    /// A source witness binds the saved ordering cursor to actual log bytes.
    last_source: Option<EventSource>,
    parser: ParserCheckpoint,
    reducer: ArenaReducerCheckpoint,
    /// Corruption detector for the serialized state. It is not a security
    /// signature; source/file validation below is the authority for resume.
    integrity_hash: u64,
}

impl ObserverCheckpoint {
    fn from_observer(observer: &SessionObserver) -> Result<Self> {
        let mut cursors = BTreeMap::new();
        for (component, cursor) in &observer.cursors {
            let file_identity = observer
                .file_identities
                .get(component)
                .cloned()
                .context("observer cursor had no file identity")?;
            cursors.insert(
                *component,
                PersistedCursor {
                    cursor: *cursor,
                    file_identity,
                    tail_checkpoint: observer.checkpoints.get(component).cloned(),
                },
            );
        }
        let mut checkpoint = Self {
            schema_version: OBSERVER_CHECKPOINT_SCHEMA_VERSION,
            observer_snapshot_schema_version: OBSERVER_SNAPSHOT_SCHEMA_VERSION,
            state_schema_version: observer.reducer.snapshot().schema_version,
            session_path: canonical_session_key(&observer.session)?,
            session_id: session_id(&observer.session),
            cursors,
            last_line_order: observer.last_line_order,
            last_source: observer.last_source.clone(),
            parser: observer.parser.checkpoint(),
            reducer: observer.reducer.checkpoint(),
            integrity_hash: 0,
        };
        checkpoint.integrity_hash = checkpoint.expected_integrity_hash()?;
        Ok(checkpoint)
    }

    fn expected_integrity_hash(&self) -> Result<u64> {
        // Serialize every field except the stored checksum in a fixed order.
        // BTreeMap makes the resulting local corruption detector stable.
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Integrity<'a> {
            schema_version: u32,
            observer_snapshot_schema_version: u32,
            state_schema_version: u32,
            session_path: &'a str,
            session_id: &'a str,
            cursors: &'a BTreeMap<LogComponent, PersistedCursor>,
            last_line_order: &'a Option<RawLineOrder>,
            last_source: &'a Option<EventSource>,
            parser: &'a ParserCheckpoint,
            reducer: &'a ArenaReducerCheckpoint,
        }
        let bytes = serde_json::to_vec(&Integrity {
            schema_version: self.schema_version,
            observer_snapshot_schema_version: self.observer_snapshot_schema_version,
            state_schema_version: self.state_schema_version,
            session_path: &self.session_path,
            session_id: &self.session_id,
            cursors: &self.cursors,
            last_line_order: &self.last_line_order,
            last_source: &self.last_source,
            parser: &self.parser,
            reducer: &self.reducer,
        })
        .context("could not serialize observer checkpoint integrity payload")?;
        Ok(fnv1a64(&bytes))
    }
}

/// Stateful tailer for one concrete log-session directory.
pub struct SessionObserver {
    session: PathBuf,
    parser: HearthstoneLogParser,
    reducer: ArenaReducer,
    cursors: BTreeMap<LogComponent, LogCursor>,
    file_identities: BTreeMap<LogComponent, FileIdentity>,
    checkpoints: BTreeMap<LogComponent, FileCheckpoint>,
    last_line_order: Option<RawLineOrder>,
    last_source: Option<EventSource>,
    attach_method: AttachMethod,
    attach_diagnostics: AttachDiagnostics,
}

struct CompletedTailSnapshot {
    draft_deck_id: String,
    hero_card_id: Option<String>,
    card_ids: Vec<String>,
    draft_mode: Option<String>,
    completion_cursor: LogCursor,
    bytes_parsed: u64,
    parser: HearthstoneLogParser,
}

enum SnapshotParse {
    Complete(CompletedTailSnapshot),
    /// A newest snapshot start exists but did not reach a verified completion
    /// marker inside the compact parse budget. Preserve its parser/reducer
    /// state so appended records can finish it; do not expose it as an
    /// authoritative current deck.
    Incomplete {
        parser: HearthstoneLogParser,
        reducer: ArenaReducer,
        bytes_parsed: u64,
    },
}

/// Parse one newest Arena snapshot forward from its raw reverse-located
/// sentinel. This is intentionally the only line-by-line work on cold live
/// attach; it stops at the first verified completion marker.
fn parse_snapshot_at(
    session: &Path,
    arena_path: &Path,
    start: u64,
    expected_deck_slots: Option<u16>,
) -> Result<SnapshotParse> {
    let mut reader = ComponentLogReader::open(
        LogComponent::Arena,
        arena_path,
        LogCursor {
            byte_offset: start,
            line_number: 0,
        },
    )?;
    let mut parser = HearthstoneLogParser::default();
    let mut reducer = ArenaReducer::with_expected_deck_slots(expected_deck_slots);
    let mut draft_deck_id = None;
    let mut hero_card_id = None;
    let mut card_ids = Vec::new();
    let mut draft_mode = None;
    let mut started = false;

    while let Some(line) = reader.next_complete_line()? {
        let bytes_parsed = reader.cursor().byte_offset.saturating_sub(start);
        if bytes_parsed > TAIL_SNAPSHOT_PARSE_BYTES {
            return Ok(SnapshotParse::Incomplete {
                parser,
                reducer,
                bytes_parsed,
            });
        }
        let events = parser.parse_line(&line);
        if !started {
            let Some((id, hero)) = events.iter().find_map(|event| match event {
                GameEvent::ArenaDeckSnapshotStarted {
                    draft_deck_id,
                    hero_card_id,
                } => Some((draft_deck_id.clone(), hero_card_id.clone())),
                _ => None,
            }) else {
                // A raw string match is not grammar proof. Treat it as an
                // awaiting snapshot rather than accepting an arbitrary line.
                return Ok(SnapshotParse::Incomplete {
                    parser,
                    reducer,
                    bytes_parsed,
                });
            };
            started = true;
            draft_deck_id = Some(id);
            hero_card_id = hero;
        }

        for event in &events {
            match event {
                GameEvent::ArenaDeckSnapshotCard { card_id } => card_ids.push(card_id.clone()),
                GameEvent::ArenaDraftMode { mode } => draft_mode = Some(mode.clone()),
                _ => {}
            }
        }
        let completed = events
            .iter()
            .any(|event| matches!(event, GameEvent::ArenaDeckSnapshotCompleted));
        let source = event_source(session, &line);
        reducer.apply_sourced_line(source, events);
        if completed {
            return Ok(SnapshotParse::Complete(CompletedTailSnapshot {
                draft_deck_id: draft_deck_id.expect("snapshot start was required"),
                hero_card_id,
                card_ids,
                draft_mode,
                completion_cursor: reader.cursor(),
                bytes_parsed,
                parser,
            }));
        }
    }

    Ok(SnapshotParse::Incomplete {
        parser,
        reducer,
        bytes_parsed: reader.cursor().byte_offset.saturating_sub(start),
    })
}

impl SessionObserver {
    /// Explicit deterministic historical replay. Live startup should normally
    /// use [`Self::attach_current_state`] instead.
    pub fn attach(session: impl Into<PathBuf>) -> Result<Self> {
        Self::attach_with_expected_deck_slots(session, None)
    }

    /// Explicit deterministic historical replay with an optional local deck
    /// size rule. This is the API used by fixture replay and diagnostics that
    /// promise complete historical state.
    pub fn attach_with_expected_deck_slots(
        session: impl Into<PathBuf>,
        expected_deck_slots: Option<u16>,
    ) -> Result<Self> {
        Self::attach_full_with_expected_deck_slots(session, expected_deck_slots)
    }

    /// Attach a live observer from the newest authoritative Arena deck
    /// snapshot. This does not replay Power/Zone/Asset history and does not
    /// claim historic draft selections are available. If no complete current
    /// snapshot can be proven, it returns an `awaiting_snapshot` observer that
    /// tails new records rather than silently replaying old history.
    pub fn attach_current_state(session: impl Into<PathBuf>) -> Result<Self> {
        Self::attach_current_state_with_expected_deck_slots(session, None)
    }

    pub fn attach_current_state_with_expected_deck_slots(
        session: impl Into<PathBuf>,
        expected_deck_slots: Option<u16>,
    ) -> Result<Self> {
        let session = session.into();
        validate_log_session(&session)?;
        Self::attach_tail_snapshot(&session, expected_deck_slots)
    }

    fn attach_tail_snapshot(session: &Path, expected_deck_slots: Option<u16>) -> Result<Self> {
        let arena_path = session.join(LogComponent::Arena.filename());
        let mut file_lengths = BTreeMap::new();
        let mut file_identities = BTreeMap::new();
        let mut cursors = BTreeMap::new();
        let mut checkpoints = BTreeMap::new();
        for component in REQUIRED_COMPONENTS {
            let path = session.join(component.filename());
            if !path.is_file() {
                continue;
            }
            let identity = file_identity(&path)?;
            let length = fs::metadata(&path)
                .with_context(|| format!("could not stat {}", path.display()))?
                .len();
            let cursor = tail_cursor(&path)?;
            if let Some(checkpoint) = file_checkpoint(&path, cursor.byte_offset)? {
                checkpoints.insert(component, checkpoint);
            }
            file_lengths.insert(component, length);
            file_identities.insert(component, identity);
            cursors.insert(component, cursor);
        }

        let mut parser = HearthstoneLogParser::default();
        let mut reducer = ArenaReducer::with_expected_deck_slots(expected_deck_slots);
        let mut method = AttachMethod::AwaitingSnapshot;
        let mut diagnostics = AttachDiagnostics {
            method,
            non_arena_components_started_at_tail: true,
            ..AttachDiagnostics::default()
        };

        if arena_path.is_file() {
            let latest_run_start =
                find_last_line_containing(&arena_path, ARENA_RUN_START_SENTINEL)?;
            if let Some(marker) = find_last_line_containing(&arena_path, ARENA_SNAPSHOT_SENTINEL)? {
                diagnostics.snapshot_byte_offset = Some(marker.line_start);
                // A raw `OnBegin` after the snapshot could be a new run. It
                // is deliberately treated as invalidation even if a future
                // client adds an unrelated Arena `OnBegin` variant: that can
                // only delay hydration, never show a stale deck.
                let superseded_by_new_run = latest_run_start
                    .is_some_and(|run_start| run_start.line_start > marker.line_start);
                diagnostics.snapshot_invalidated_by_newer_run = superseded_by_new_run;
                if marker.complete && !superseded_by_new_run {
                    match parse_snapshot_at(
                        session,
                        &arena_path,
                        marker.line_start,
                        expected_deck_slots,
                    )? {
                        SnapshotParse::Complete(snapshot) => {
                            diagnostics.snapshot_bytes_parsed = snapshot.bytes_parsed;
                            let arena_cursor = cursors
                                .get(&LogComponent::Arena)
                                .copied()
                                .unwrap_or_default();
                            let suffix_bytes = arena_cursor
                                .byte_offset
                                .saturating_sub(snapshot.completion_cursor.byte_offset);
                            reducer.apply(GameEvent::ArenaAuthoritativeResync {
                                draft_deck_id: snapshot.draft_deck_id,
                                hero_card_id: snapshot.hero_card_id,
                                card_ids: snapshot.card_ids,
                                draft_mode: snapshot.draft_mode,
                            });
                            parser = snapshot.parser;
                            if suffix_bytes <= TAIL_SNAPSHOT_SUFFIX_BYTES {
                                let mut reader = ComponentLogReader::open(
                                    LogComponent::Arena,
                                    &arena_path,
                                    snapshot.completion_cursor,
                                )?;
                                while let Some(line) = reader.next_complete_line()? {
                                    let source = event_source(session, &line);
                                    reducer.apply_sourced_line(source, parser.parse_line(&line));
                                }
                                // Preserve the cursor at the verified final
                                // complete line. It equals `arena_cursor`
                                // unless Hearthstone wrote while attaching,
                                // which is detected below.
                                cursors.insert(LogComponent::Arena, reader.cursor());
                                if let Some(checkpoint) =
                                    file_checkpoint(&arena_path, reader.cursor().byte_offset)?
                                {
                                    checkpoints.insert(LogComponent::Arena, checkpoint);
                                }
                            } else {
                                diagnostics.arena_suffix_bytes_skipped = suffix_bytes;
                                diagnostics.arena_suffix_truncated = true;
                            }
                            method = AttachMethod::TailSnapshot;
                            diagnostics.method = method;
                        }
                        SnapshotParse::Incomplete {
                            parser: scanned_parser,
                            reducer: scanned_reducer,
                            bytes_parsed,
                        } => {
                            parser = scanned_parser;
                            reducer = scanned_reducer;
                            diagnostics.snapshot_bytes_parsed = bytes_parsed;
                        }
                    }
                }
            }

            // A player can restart the overlay after `OnBegin` but before the
            // new run has produced its first authoritative deck snapshot. In
            // that window, replay only the small suffix rooted at the newest
            // complete run marker. The parser must confirm the marker by
            // producing a draft deck ID; a coincidental `OnBegin` string is
            // therefore left in the conservative awaiting state.
            if method == AttachMethod::AwaitingSnapshot
                && let Some(run_start) = latest_run_start
                && run_start.complete
            {
                let arena_cursor = cursors
                    .get(&LogComponent::Arena)
                    .copied()
                    .unwrap_or_default();
                let suffix_bytes = arena_cursor
                    .byte_offset
                    .saturating_sub(run_start.line_start);
                if suffix_bytes <= TAIL_SNAPSHOT_SUFFIX_BYTES {
                    let mut run_parser = HearthstoneLogParser::default();
                    let mut run_reducer =
                        ArenaReducer::with_expected_deck_slots(expected_deck_slots);
                    let mut reader = ComponentLogReader::open(
                        LogComponent::Arena,
                        &arena_path,
                        LogCursor {
                            byte_offset: run_start.line_start,
                            line_number: 0,
                        },
                    )?;
                    while let Some(line) = reader.next_complete_line()? {
                        let source = event_source(session, &line);
                        run_reducer.apply_sourced_line(source, run_parser.parse_line(&line));
                    }
                    if run_reducer.snapshot().run.draft_deck_id.is_some() {
                        parser = run_parser;
                        reducer = run_reducer;
                        cursors.insert(LogComponent::Arena, reader.cursor());
                        if let Some(checkpoint) =
                            file_checkpoint(&arena_path, reader.cursor().byte_offset)?
                        {
                            checkpoints.insert(LogComponent::Arena, checkpoint);
                        }
                        method = AttachMethod::TailRun;
                        diagnostics.method = method;
                    }
                } else {
                    diagnostics.arena_suffix_bytes_skipped = suffix_bytes;
                    diagnostics.arena_suffix_truncated = true;
                }
            }
        }

        // Never accept a moving boundary: retry the tiny tail attach on the
        // next observer-loop iteration rather than losing lines written while
        // we located the raw marker and snapshot.
        for component in file_lengths.keys() {
            let path = session.join(component.filename());
            let identity_after = file_identity(&path)?;
            let length_after = fs::metadata(&path)?.len();
            if file_identities.get(component) != Some(&identity_after)
                || file_lengths.get(component) != Some(&length_after)
            {
                return Err(LogReplayRequired {
                    path: Some(path),
                    reason: "changed while building tail snapshot; retrying is safer",
                }
                .into());
            }
        }

        Ok(Self {
            session: session.to_path_buf(),
            parser,
            reducer,
            cursors,
            file_identities,
            checkpoints,
            // The tail snapshot intentionally did not establish a global
            // historical ordering cursor. Appended lines start a fresh
            // ordering epoch instead of triggering a pointless full replay.
            last_line_order: None,
            last_source: None,
            attach_method: method,
            attach_diagnostics: diagnostics,
        })
    }

    /// Explicit complete-history pathway. Keeping it separate from live
    /// attach makes it impossible for a future tail optimization or recovery
    /// fallback to silently turn ordinary startup into history replay.
    fn attach_full_with_expected_deck_slots(
        session: impl Into<PathBuf>,
        expected_deck_slots: Option<u16>,
    ) -> Result<Self> {
        let session = session.into();
        validate_log_session(&session)?;
        let mut cursors = BTreeMap::new();
        let mut file_identities = BTreeMap::new();
        let mut checkpoints = BTreeMap::new();
        let mut readers = BTreeMap::new();
        for component in REQUIRED_COMPONENTS {
            let path = session.join(component.filename());
            if path.is_file() {
                let identity_before = file_identity(&path)?;
                let mut reader = ComponentLogReader::open(component, &path, LogCursor::default())?;
                if !component_can_emit_events(component) {
                    // Zone and Asset are retained as tail cursors, but the
                    // current parser emits no reducer events for them. Do not
                    // materialize hundreds of thousands of inert raw records
                    // merely to attach to an existing long game session.
                    reader.fast_forward_inert_component()?;
                }
                readers.insert(component, reader);
                file_identities.insert(component, identity_before);
            }
        }
        if readers.is_empty() {
            anyhow::bail!(
                "{} contains no readable Hearthstone component logs",
                session.display()
            );
        }

        // K-way merge one line from each event-producing component. This is
        // the same timestamp/component/line ordering as `sort_lines`, but it
        // bounds replay memory to a handful of log lines instead of the full
        // Power/Zone history.
        let mut parser = HearthstoneLogParser::default();
        let mut reducer = ArenaReducer::with_expected_deck_slots(expected_deck_slots);
        let mut pending = BTreeMap::new();
        for component in REQUIRED_COMPONENTS {
            if component_can_emit_events(component)
                && let Some(reader) = readers.get_mut(&component)
                && let Some(line) = reader.next_event_candidate()?
            {
                pending.insert(component, line);
            }
        }
        let mut last_line_order = None;
        let mut last_source = None;
        while let Some(component) = next_pending_component(&pending) {
            let line = pending
                .remove(&component)
                .expect("pending component must contain the selected log line");
            last_line_order = Some(raw_line_order(&line));
            let source = event_source(&session, &line);
            last_source = Some(source.clone());
            let events = parser.parse_line(&line);
            reducer.apply_sourced_line(source, events);
            if let Some(next) = readers
                .get_mut(&component)
                .expect("selected component must have an open log reader")
                .next_event_candidate()?
            {
                pending.insert(component, next);
            }
        }

        for (component, reader) in readers {
            let path = session.join(component.filename());
            let identity_after = file_identity(&path)?;
            if file_identities.get(&component) != Some(&identity_after) {
                return Err(LogReplayRequired {
                    path: Some(path),
                    reason: "changed while attaching; retrying is safer",
                }
                .into());
            }
            let cursor = reader.cursor();
            cursors.insert(component, cursor);
            file_identities.insert(component, identity_after);
            if let Some(checkpoint) = file_checkpoint(&path, cursor.byte_offset)? {
                checkpoints.insert(component, checkpoint);
            }
        }
        Ok(Self {
            session,
            parser,
            reducer,
            cursors,
            file_identities,
            checkpoints,
            last_line_order,
            last_source,
            attach_method: AttachMethod::FullReplay,
            attach_diagnostics: AttachDiagnostics {
                method: AttachMethod::FullReplay,
                ..AttachDiagnostics::default()
            },
        })
    }

    /// Restore a validated warm checkpoint when possible. Any malformed,
    /// stale, rotated, or source-mismatched checkpoint is rejected and this
    /// method performs a reduced current-state tail attach instead of replaying
    /// history. Explicit callers can still invoke [`Self::attach`] for full
    /// deterministic replay.
    pub fn attach_with_checkpoint(
        session: impl Into<PathBuf>,
        checkpoint_path: impl AsRef<Path>,
    ) -> Result<(Self, CheckpointRestoreStatus)> {
        Self::attach_with_checkpoint_and_expected_deck_slots(session, checkpoint_path, None)
    }

    /// Like [`Self::attach_with_checkpoint`], while explicitly applying the
    /// currently selected local Arena-rules deck size after the saved reducer
    /// internals have been validated. This prevents a stale checkpoint from
    /// silently retaining an old season manifest. `None` deliberately clears
    /// a previously configured manifest value; an authoritative completed
    /// deck snapshot may still supply its own inferred size.
    pub fn attach_with_checkpoint_and_expected_deck_slots(
        session: impl Into<PathBuf>,
        checkpoint_path: impl AsRef<Path>,
        expected_deck_slots: Option<u16>,
    ) -> Result<(Self, CheckpointRestoreStatus)> {
        let session = session.into();
        let checkpoint_path = checkpoint_path.as_ref();
        match read_checkpoint(checkpoint_path) {
            Ok(None) => Ok((
                Self::attach_current_state_with_expected_deck_slots(session, expected_deck_slots)?,
                CheckpointRestoreStatus::NotFound,
            )),
            Ok(Some(checkpoint)) => {
                match restore_checkpoint(&session, checkpoint, expected_deck_slots) {
                    Ok(mut observer) => {
                        let suffix_bytes = match observer.checkpoint_suffix_bytes() {
                            Ok(bytes) => bytes,
                            Err(error) if is_replay_required(&error) => {
                                return Ok((
                                    Self::attach_current_state_with_expected_deck_slots(
                                        session,
                                        expected_deck_slots,
                                    )?,
                                    CheckpointRestoreStatus::Rejected {
                                        reason: format!(
                                            "checkpoint changed before suffix catch-up: {error}"
                                        ),
                                    },
                                ));
                            }
                            Err(error) => return Err(error),
                        };
                        if suffix_bytes > CHECKPOINT_CATCH_UP_BYTES {
                            return Ok((
                                Self::attach_current_state_with_expected_deck_slots(
                                    session,
                                    expected_deck_slots,
                                )?,
                                CheckpointRestoreStatus::Rejected {
                                    reason: format!(
                                        "validated checkpoint has {suffix_bytes} newly appended event-log bytes; live attach uses bounded current-state resync"
                                    ),
                                },
                            ));
                        }
                        match observer.poll() {
                            Ok(_) => {
                                observer.attach_diagnostics.checkpoint_suffix_bytes_replayed =
                                    suffix_bytes;
                                Ok((observer, CheckpointRestoreStatus::Restored))
                            }
                            // A valid checkpoint can still race a rotation,
                            // newly appearing component, or an ordering
                            // boundary. Re-establish product state from the
                            // newest snapshot instead of exposing stale
                            // checkpoint state or replaying history.
                            Err(error) if is_replay_required(&error) => Ok((
                                Self::attach_current_state_with_expected_deck_slots(
                                    session,
                                    expected_deck_slots,
                                )?,
                                CheckpointRestoreStatus::Rejected {
                                    reason: format!(
                                        "checkpoint suffix could not be resumed safely: {error}"
                                    ),
                                },
                            )),
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Ok((
                        Self::attach_current_state_with_expected_deck_slots(
                            session,
                            expected_deck_slots,
                        )?,
                        CheckpointRestoreStatus::Rejected {
                            reason: error.to_string(),
                        },
                    )),
                }
            }
            Err(error) => Ok((
                Self::attach_current_state_with_expected_deck_slots(session, expected_deck_slots)?,
                CheckpointRestoreStatus::Rejected {
                    reason: error.to_string(),
                },
            )),
        }
    }

    /// Atomically persist a compact checkpoint after a successful attach or
    /// poll. It contains no card-data cache and no screen capture; only local
    /// parser/reducer state and file witnesses required for safe resumption.
    pub fn write_checkpoint(&self, path: impl AsRef<Path>) -> Result<()> {
        let checkpoint = ObserverCheckpoint::from_observer(self)?;
        write_checkpoint_atomically(path.as_ref(), &checkpoint)
    }

    /// Count only the event-producing bytes added after a validated
    /// checkpoint boundary. Inert Zone/Asset growth is advanced cheaply by
    /// `poll` and must not make a live restart pay a history-replay cost.
    fn checkpoint_suffix_bytes(&self) -> Result<u64> {
        let mut total = 0_u64;
        for component in REQUIRED_COMPONENTS {
            if !component_can_emit_events(component) {
                continue;
            }
            let path = self.session.join(component.filename());
            let Some(cursor) = self.cursors.get(&component).copied() else {
                // This component did not exist when the checkpoint was
                // written. `validate_checkpoint` already rejects it if it
                // has appeared now; if it is still absent there is nothing
                // to catch up.
                continue;
            };
            if !path.is_file() {
                return Err(LogReplayRequired {
                    path: Some(path),
                    reason: "disappeared before checkpoint suffix catch-up",
                }
                .into());
            }
            let length = fs::metadata(&path)
                .with_context(|| format!("could not stat {}", path.display()))?
                .len();
            if length < cursor.byte_offset {
                return Err(LogReplayRequired {
                    path: Some(path),
                    reason: "was truncated before checkpoint suffix catch-up",
                }
                .into());
            }
            total = total
                .checked_add(length - cursor.byte_offset)
                .ok_or_else(|| anyhow::anyhow!("checkpoint suffix byte count overflow"))?;
        }
        Ok(total)
    }

    pub fn session(&self) -> &Path {
        &self.session
    }

    pub const fn attach_method(&self) -> AttachMethod {
        self.attach_method
    }

    pub fn attach_diagnostics(&self) -> &AttachDiagnostics {
        &self.attach_diagnostics
    }

    pub fn cursors(&self) -> &BTreeMap<LogComponent, LogCursor> {
        &self.cursors
    }

    pub fn state(&self) -> &ArenaSnapshot {
        self.reducer.snapshot()
    }

    /// Apply the Redraft contract from the currently selected local rules
    /// manifest. Logs do not encode the pick-round/discard policy, so this is
    /// deliberately caller-provided and retained by `LiveObserver` during
    /// recovery/session rollover.
    pub fn set_redraft_policy(&mut self, policy: Option<RedraftPolicy>) -> Result<()> {
        self.reducer
            .set_redraft_policy(policy)
            .map_err(anyhow::Error::msg)
    }

    pub fn card_observations(&self, card_id: &str) -> Option<&[EventSource]> {
        self.reducer.card_observations(card_id)
    }

    pub const fn arena_picks_enabled(&self) -> bool {
        self.parser.arena_picks_enabled()
    }

    /// Replaces only the current deck multiset from a complete, validated
    /// Hearthstone sidebar reading, then enables future Arena pick records.
    ///
    /// The caller must prove that every card counted by the visible `N/30`
    /// marker is present in `card_ids`. A partial/scrolling sidebar is rejected
    /// and therefore can never delete hidden cards. Hero, run, phase, and draft
    /// history are preserved.
    pub fn apply_complete_sidebar_baseline(
        &mut self,
        card_ids: Vec<String>,
        observed_slots: u16,
        expected_slots: u16,
    ) -> Result<bool> {
        if self.reducer.snapshot().run.draft_deck_id.is_none() {
            anyhow::bail!("cannot apply a deck sidebar before an Arena run is identified");
        }
        if card_ids.len() != usize::from(observed_slots) {
            anyhow::bail!(
                "sidebar is partial: {} visible card slots for {observed_slots} observed",
                card_ids.len()
            );
        }
        if expected_slots == 0 || observed_slots > expected_slots {
            anyhow::bail!(
                "sidebar count {observed_slots}/{expected_slots} is not a valid deck capacity"
            );
        }
        if card_ids.is_empty() || card_ids.iter().any(|card_id| !is_real_card_id(card_id)) {
            anyhow::bail!("sidebar baseline contains no cards or a non-card entity");
        }

        let before = self.reducer.snapshot().deck.clone();
        let gate_was_enabled = self.parser.arena_picks_enabled();
        self.reducer.set_expected_deck_slots(Some(expected_slots));
        self.reducer.apply(GameEvent::DeckList { card_ids });
        self.parser
            .enable_arena_picks_after_authoritative_baseline();
        Ok(before != self.reducer.snapshot().deck || !gate_was_enabled)
    }

    /// Consumes appended complete lines. Any rotation, late component, or
    /// cross-file ordering hazard returns an internal recovery-required error;
    /// `LiveObserver` turns that into a safe current-deck resync.
    pub fn poll(&mut self) -> Result<bool> {
        let mut readers = BTreeMap::new();
        for component in REQUIRED_COMPONENTS {
            let path = self.session.join(component.filename());
            if !path.is_file() {
                if self.file_identities.contains_key(&component) {
                    return Err(LogReplayRequired {
                        path: Some(path),
                        reason: "disappeared or rotated",
                    }
                    .into());
                }
                continue;
            }
            let identity_before = file_identity(&path)?;
            match self.file_identities.get(&component) {
                Some(expected) if expected != &identity_before => {
                    return Err(LogReplayRequired {
                        path: Some(path),
                        reason: "was replaced or rotated",
                    }
                    .into());
                }
                Some(_) => {}
                None => {
                    // A component that appears after attachment can contain
                    // records earlier than lines reduced from another file.
                    // LiveObserver responds with a fresh current-state
                    // resync rather than trusting this suffix ordering.
                    return Err(LogReplayRequired {
                        path: Some(path),
                        reason: "appeared after attachment",
                    }
                    .into());
                }
            }
            let cursor = self.cursors.get(&component).copied().unwrap_or_default();
            let file_len = fs::metadata(&path)
                .with_context(|| format!("could not stat {}", path.display()))?
                .len();
            if file_len < cursor.byte_offset {
                return Err(LogReplayRequired {
                    path: Some(path),
                    reason: "was truncated or rotated",
                }
                .into());
            }
            if let Some(checkpoint) = self.checkpoints.get(&component) {
                if !checkpoint_matches(&path, checkpoint)? {
                    return Err(LogReplayRequired {
                        path: Some(path),
                        reason: "changed before its saved cursor",
                    }
                    .into());
                }
            }
            let mut reader = ComponentLogReader::open(component, &path, cursor)
                .with_context(|| format!("could not tail {}", path.display()))?;
            if !component_can_emit_events(component) {
                reader
                    .fast_forward_inert_component()
                    .with_context(|| format!("could not advance {}", path.display()))?;
            }
            readers.insert(component, reader);
        }

        // Merge only event-producing records. The observer still advances the
        // other required logs above, but they cannot affect reducer order
        // until the parser explicitly gains events for those components.
        let mut pending = BTreeMap::new();
        for component in REQUIRED_COMPONENTS {
            if component_can_emit_events(component)
                && let Some(reader) = readers.get_mut(&component)
                && let Some(line) = reader.next_event_candidate()?
            {
                pending.insert(component, line);
            }
        }
        let mut consumed_event_line = false;
        while let Some(component) = next_pending_component(&pending) {
            let line = pending
                .remove(&component)
                .expect("pending component must contain the selected log line");
            if self
                .last_line_order
                .is_some_and(|last| raw_line_order(&line) < last)
            {
                return Err(LogReplayRequired {
                    path: None,
                    reason: "a newly appended record sorts before an already reduced record",
                }
                .into());
            }
            self.last_line_order = Some(raw_line_order(&line));
            let source = event_source(&self.session, &line);
            self.last_source = Some(source.clone());
            let events = self.parser.parse_line(&line);
            self.reducer.apply_sourced_line(source, events);
            consumed_event_line = true;
            if let Some(next) = readers
                .get_mut(&component)
                .expect("selected component must have an open log reader")
                .next_event_candidate()?
            {
                pending.insert(component, next);
            }
        }

        let mut advanced_cursor = false;
        for (component, reader) in readers {
            let path = self.session.join(component.filename());
            let identity_after = file_identity(&path)?;
            if self.file_identities.get(&component) != Some(&identity_after) {
                return Err(LogReplayRequired {
                    path: Some(path),
                    reason: "changed while reading",
                }
                .into());
            }
            if let Some(checkpoint) = self.checkpoints.get(&component) {
                if !checkpoint_matches(&path, checkpoint)? {
                    return Err(LogReplayRequired {
                        path: Some(path),
                        reason: "changed while reading",
                    }
                    .into());
                }
            }
            let next_cursor = reader.cursor();
            advanced_cursor |= self.cursors.get(&component) != Some(&next_cursor);
            self.cursors.insert(component, next_cursor);
            if let Some(checkpoint) = file_checkpoint(&path, next_cursor.byte_offset)? {
                self.checkpoints.insert(component, checkpoint);
            } else {
                self.checkpoints.remove(&component);
            }
        }
        Ok(consumed_event_line || advanced_cursor)
    }

    pub fn resolved_snapshot(&self, cards: &CardCache) -> ObserverSnapshot {
        resolve_snapshot(self.reducer.snapshot().clone(), cards)
    }
}

/// Rebuild a session observer from a checkpoint only after proving that its
/// state belongs to the exact current files. Failure is intentionally ordinary
/// control flow: the public attach API immediately falls back to replay.
fn restore_checkpoint(
    session: &Path,
    checkpoint: ObserverCheckpoint,
    expected_deck_slots: Option<u16>,
) -> Result<SessionObserver> {
    validate_checkpoint(session, &checkpoint)?;
    let parser = HearthstoneLogParser::from_checkpoint(checkpoint.parser)
        .map_err(anyhow::Error::msg)
        .context("checkpoint parser state was invalid")?;
    let mut reducer = ArenaReducer::from_checkpoint(checkpoint.reducer)
        .map_err(anyhow::Error::msg)
        .context("checkpoint reducer state was invalid")?;
    // The manifest belongs to the current application configuration, not to
    // a historical session checkpoint. Apply it only after validating the
    // saved reducer's own consistency.
    reducer.set_expected_deck_slots(expected_deck_slots);
    let mut cursors = BTreeMap::new();
    let mut file_identities = BTreeMap::new();
    let mut checkpoints = BTreeMap::new();
    for (component, persisted) in checkpoint.cursors {
        cursors.insert(component, persisted.cursor);
        file_identities.insert(component, persisted.file_identity);
        if let Some(tail_checkpoint) = persisted.tail_checkpoint {
            checkpoints.insert(component, tail_checkpoint);
        }
    }
    Ok(SessionObserver {
        session: session.to_path_buf(),
        parser,
        reducer,
        cursors,
        file_identities,
        checkpoints,
        last_line_order: checkpoint.last_line_order,
        last_source: checkpoint.last_source,
        attach_method: AttachMethod::VerifiedCheckpoint,
        attach_diagnostics: AttachDiagnostics {
            method: AttachMethod::VerifiedCheckpoint,
            ..AttachDiagnostics::default()
        },
    })
}

fn validate_checkpoint(session: &Path, checkpoint: &ObserverCheckpoint) -> Result<()> {
    if checkpoint.schema_version != OBSERVER_CHECKPOINT_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported observer checkpoint schema {}; expected {}",
            checkpoint.schema_version,
            OBSERVER_CHECKPOINT_SCHEMA_VERSION
        );
    }
    if checkpoint.observer_snapshot_schema_version != OBSERVER_SNAPSHOT_SCHEMA_VERSION {
        anyhow::bail!(
            "checkpoint observer snapshot schema {} does not match {}",
            checkpoint.observer_snapshot_schema_version,
            OBSERVER_SNAPSHOT_SCHEMA_VERSION
        );
    }
    if checkpoint.state_schema_version != hs_state::SNAPSHOT_SCHEMA_VERSION {
        anyhow::bail!(
            "checkpoint state schema {} does not match {}",
            checkpoint.state_schema_version,
            hs_state::SNAPSHOT_SCHEMA_VERSION
        );
    }
    if checkpoint.integrity_hash != checkpoint.expected_integrity_hash()? {
        anyhow::bail!("checkpoint integrity checksum did not match");
    }
    validate_log_session(session)?;
    if checkpoint.session_path != canonical_session_key(session)? {
        anyhow::bail!("checkpoint belongs to a different log-session directory");
    }
    if checkpoint.session_id != session_id(session) {
        anyhow::bail!("checkpoint session identity did not match");
    }

    for component in REQUIRED_COMPONENTS {
        let path = session.join(component.filename());
        match (checkpoint.cursors.get(&component), path.is_file()) {
            (Some(persisted), true) => {
                let identity = file_identity(&path)?;
                if identity != persisted.file_identity {
                    anyhow::bail!("{} file identity changed", path.display());
                }
                let length = fs::metadata(&path)
                    .with_context(|| format!("could not stat {}", path.display()))?
                    .len();
                if length < persisted.cursor.byte_offset {
                    anyhow::bail!("{} is shorter than its saved cursor", path.display());
                }
                match (&persisted.tail_checkpoint, persisted.cursor.byte_offset) {
                    (Some(tail), cursor) => {
                        let expected_start = cursor.saturating_sub(CHECKPOINT_BYTES);
                        let expected_length = usize::try_from(cursor - expected_start)
                            .expect("checkpoint tail length fits usize");
                        if tail.start != expected_start || tail.bytes.len() != expected_length {
                            anyhow::bail!(
                                "{} had an invalid saved tail checkpoint",
                                path.display()
                            );
                        }
                        if !checkpoint_matches(&path, tail)? {
                            anyhow::bail!("{} changed before its saved cursor", path.display());
                        }
                    }
                    (None, 0) => {}
                    (None, _) => anyhow::bail!(
                        "{} has a nonzero cursor without a tail checkpoint",
                        path.display()
                    ),
                }
            }
            (Some(_), false) => anyhow::bail!("{} disappeared", path.display()),
            // A newly present component can contain earlier timestamped
            // records, so a suffix-only resume would not be deterministic.
            (None, true) => anyhow::bail!("{} appeared after checkpoint", path.display()),
            (None, false) => {}
        }
    }

    match (&checkpoint.last_line_order, &checkpoint.last_source) {
        (Some(order), Some(source)) => {
            validate_source_witness(session, checkpoint, source)?;
            let component = component_from_source(source)?;
            let actual = (
                timestamp_key(&read_source_raw(session, source)?),
                component,
                source.byte_offset,
            );
            if *order != actual {
                anyhow::bail!("checkpoint final source ordering witness did not match");
            }
        }
        (None, None) => {}
        _ => anyhow::bail!("checkpoint had incomplete final source witness"),
    }

    // Bounded provenance is user-visible through `explain-card`; validate it
    // as well so diagnostics never point at source bytes from another run.
    for source in checkpoint.reducer.observation_sources() {
        validate_source_witness(session, checkpoint, &source)?;
    }
    Ok(())
}

fn validate_source_witness(
    session: &Path,
    checkpoint: &ObserverCheckpoint,
    source: &EventSource,
) -> Result<()> {
    if source.session_id != checkpoint.session_id {
        anyhow::bail!("checkpoint source belongs to a different session");
    }
    let component = component_from_source(source)?;
    let cursor = checkpoint
        .cursors
        .get(&component)
        .context("checkpoint source component had no cursor")?;
    if source.byte_offset >= cursor.cursor.byte_offset {
        anyhow::bail!("checkpoint source lies at or after its saved cursor");
    }
    let raw = read_source_raw(session, source)?;
    if fnv1a64(raw.as_bytes()) != source.line_hash {
        anyhow::bail!("checkpoint source contents did not match");
    }
    Ok(())
}

fn component_from_source(source: &EventSource) -> Result<LogComponent> {
    let component = LogComponent::from_filename(&source.component).with_context(|| {
        format!(
            "unsupported checkpoint source component {}",
            source.component
        )
    })?;
    if component.filename() != source.component {
        anyhow::bail!("checkpoint source component was not a canonical log filename");
    }
    Ok(component)
}

/// Read exactly one complete log record at a known byte offset. This bounded
/// witness check avoids rescanning a 69 MB session merely to validate state.
fn read_source_raw(session: &Path, source: &EventSource) -> Result<String> {
    let component = component_from_source(source)?;
    let path = session.join(component.filename());
    let mut file = fs::File::open(&path)
        .with_context(|| format!("could not open {} for source validation", path.display()))?;
    file.seek(SeekFrom::Start(source.byte_offset))?;
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let count = file.read(&mut byte)?;
        if count == 0 {
            anyhow::bail!("checkpoint source ended before a complete log line");
        }
        if byte[0] == b'\n' {
            break;
        }
        // Hearthstone log records are small. A hard bound prevents a corrupt
        // checkpoint from asking the resume path to materialize a huge file.
        if bytes.len() >= 1024 * 1024 {
            anyhow::bail!("checkpoint source line exceeded 1 MiB");
        }
        bytes.push(byte[0]);
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes).context("checkpoint source was not UTF-8")
}

fn canonical_session_key(session: &Path) -> Result<String> {
    fs::canonicalize(session)
        .with_context(|| format!("could not canonicalize {}", session.display()))
        .map(|path| path.to_string_lossy().into_owned())
}

fn session_id(session: &Path) -> String {
    session
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| session.display().to_string())
}

fn read_checkpoint(path: &Path) -> Result<Option<ObserverCheckpoint>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("could not stat {}", path.display()));
        }
    };
    if metadata.len() > MAX_CHECKPOINT_BYTES {
        anyhow::bail!(
            "checkpoint {} exceeds the {} byte safety limit",
            path.display(),
            MAX_CHECKPOINT_BYTES
        );
    }
    let bytes = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("could not parse {}", path.display()))
        .map(Some)
}

fn write_checkpoint_atomically(path: &Path, checkpoint: &ObserverCheckpoint) -> Result<()> {
    let bytes =
        serde_json::to_vec(checkpoint).context("could not serialize observer checkpoint")?;
    if u64::try_from(bytes.len()).expect("usize fits u64") > MAX_CHECKPOINT_BYTES {
        anyhow::bail!("observer checkpoint exceeded the configured size limit");
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("could not create checkpoint directory {}", parent.display()))?;
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("arena-next-observer");
    for attempt in 0..16_u64 {
        let sequence = CHECKPOINT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = parent.join(format!(
            ".{stem}.checkpoint-{}-{nanos}-{sequence}-{attempt}.tmp",
            std::process::id()
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not create {}", temporary.display()));
            }
        };
        let write_result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path).with_context(|| {
                format!(
                    "could not atomically replace {} with {}",
                    path.display(),
                    temporary.display()
                )
            })?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return write_result;
    }
    anyhow::bail!("could not allocate a unique temporary checkpoint path");
}

/// A session observer that can follow the current Hearthstone session and
/// safely recover from a log rotation. The caller decides when to call `poll`,
/// which keeps the crate runtime-agnostic and tiny.
pub struct LiveObserver {
    tailer: SessionObserver,
    cards: CardCache,
    log_root: Option<PathBuf>,
    follow_latest_session: bool,
    expected_deck_slots: Option<u16>,
    redraft_policy: Option<RedraftPolicy>,
}

impl LiveObserver {
    pub fn attach(session: impl Into<PathBuf>, cards: CardCache) -> Result<Self> {
        Self::attach_with_expected_deck_slots(session, cards, None)
    }

    /// Attach to one explicit log session with a local rules-derived expected
    /// deck size. The value is retained across tail recovery and session
    /// rollover so diagnostics and the overlay cannot silently lose partial
    /// deck accounting.
    pub fn attach_with_expected_deck_slots(
        session: impl Into<PathBuf>,
        cards: CardCache,
        expected_deck_slots: Option<u16>,
    ) -> Result<Self> {
        Ok(Self {
            tailer: SessionObserver::attach_current_state_with_expected_deck_slots(
                session,
                expected_deck_slots,
            )?,
            cards,
            log_root: None,
            follow_latest_session: false,
            expected_deck_slots,
            redraft_policy: None,
        })
    }

    /// Explicit complete-history attach for replay/export/diagnostic callers.
    /// Normal live application startup deliberately does not use this path.
    pub fn attach_full_replay_with_expected_deck_slots(
        session: impl Into<PathBuf>,
        cards: CardCache,
        expected_deck_slots: Option<u16>,
    ) -> Result<Self> {
        Ok(Self {
            tailer: SessionObserver::attach_with_expected_deck_slots(session, expected_deck_slots)?,
            cards,
            log_root: None,
            follow_latest_session: false,
            expected_deck_slots,
            redraft_policy: None,
        })
    }

    /// Full-history attach that still rolls forward onto a newer session when
    /// Hearthstone starts one. The live overlay uses this only when a
    /// bounded-history recovery must reconstruct proven copy counts before it
    /// resumes normal polling; replacing a session-following observer with
    /// [`Self::attach_full_replay_with_expected_deck_slots`] would pin the
    /// tracker to a stale session and silently stop following the game.
    pub fn attach_full_replay_and_follow_discovered_with_expected_deck_slots(
        paths: &GamePaths,
        cards: CardCache,
        expected_deck_slots: Option<u16>,
    ) -> Result<Self> {
        let session = paths
            .log_root
            .as_deref()
            .and_then(newest_live_session)
            .or(paths.latest_session.clone())
            .context("no Hearthstone log session found")?;
        Ok(Self {
            tailer: SessionObserver::attach_with_expected_deck_slots(session, expected_deck_slots)?,
            cards,
            log_root: paths.log_root.clone(),
            follow_latest_session: true,
            expected_deck_slots,
            redraft_policy: None,
        })
    }

    /// Attach with a persisted warm-start checkpoint. The caller owns the
    /// checkpoint location; this crate never writes beside Hearthstone logs.
    pub fn attach_with_checkpoint(
        session: impl Into<PathBuf>,
        cards: CardCache,
        checkpoint_path: impl AsRef<Path>,
    ) -> Result<(Self, CheckpointRestoreStatus)> {
        Self::attach_with_checkpoint_and_expected_deck_slots(session, cards, checkpoint_path, None)
    }

    /// Like [`Self::attach_with_checkpoint`], retaining the active local
    /// Arena-rules value across ordinary polling recovery and future writes.
    pub fn attach_with_checkpoint_and_expected_deck_slots(
        session: impl Into<PathBuf>,
        cards: CardCache,
        checkpoint_path: impl AsRef<Path>,
        expected_deck_slots: Option<u16>,
    ) -> Result<(Self, CheckpointRestoreStatus)> {
        let (tailer, checkpoint_status) =
            SessionObserver::attach_with_checkpoint_and_expected_deck_slots(
                session,
                checkpoint_path,
                expected_deck_slots,
            )?;
        Ok((
            Self {
                tailer,
                cards,
                log_root: None,
                follow_latest_session: false,
                expected_deck_slots,
                redraft_policy: None,
            },
            checkpoint_status,
        ))
    }

    /// Attaches to the already-discovered newest session. Discovery is
    /// read-only; callers retain responsibility for explicit log-config edits.
    pub fn attach_discovered(paths: &GamePaths, cards: CardCache) -> Result<Self> {
        Self::attach_discovered_with_expected_deck_slots(paths, cards, None)
    }

    /// Like [`Self::attach_discovered`], with a caller-selected local
    /// expected deck size retained when the live session changes.
    pub fn attach_discovered_with_expected_deck_slots(
        paths: &GamePaths,
        cards: CardCache,
        expected_deck_slots: Option<u16>,
    ) -> Result<Self> {
        let session = paths
            .log_root
            .as_deref()
            .and_then(newest_live_session)
            .or(paths.latest_session.clone())
            .context("no Hearthstone log session found")?;
        Ok(Self {
            tailer: SessionObserver::attach_current_state_with_expected_deck_slots(
                session,
                expected_deck_slots,
            )?,
            cards,
            log_root: paths.log_root.clone(),
            follow_latest_session: true,
            expected_deck_slots,
            redraft_policy: None,
        })
    }

    /// Discover the current session, then use a checkpoint only if it proves
    /// it belongs to that exact directory. A session rollover naturally falls
    /// back to current-deck resync and can overwrite the caller's checkpoint
    /// afterward.
    pub fn attach_discovered_with_checkpoint_and_expected_deck_slots(
        paths: &GamePaths,
        cards: CardCache,
        checkpoint_path: impl AsRef<Path>,
        expected_deck_slots: Option<u16>,
    ) -> Result<(Self, CheckpointRestoreStatus)> {
        let session = paths
            .log_root
            .as_deref()
            .and_then(newest_live_session)
            .or(paths.latest_session.clone())
            .context("no Hearthstone log session found")?;
        let (tailer, checkpoint_status) =
            SessionObserver::attach_with_checkpoint_and_expected_deck_slots(
                session,
                checkpoint_path,
                expected_deck_slots,
            )?;
        Ok((
            Self {
                tailer,
                cards,
                log_root: paths.log_root.clone(),
                follow_latest_session: true,
                expected_deck_slots,
                redraft_policy: None,
            },
            checkpoint_status,
        ))
    }

    pub fn session(&self) -> &Path {
        self.tailer.session()
    }

    /// Staleness of the currently followed session. Applications call this
    /// after each poll so a frozen writer set becomes visible immediately.
    pub fn session_staleness(&self, threshold: Duration) -> LogStaleness {
        session_staleness(self.session(), threshold)
    }

    pub const fn attach_method(&self) -> AttachMethod {
        self.tailer.attach_method()
    }

    pub fn attach_diagnostics(&self) -> &AttachDiagnostics {
        self.tailer.attach_diagnostics()
    }

    pub fn cursors(&self) -> &BTreeMap<LogComponent, LogCursor> {
        self.tailer.cursors()
    }

    pub fn snapshot(&self) -> ObserverSnapshot {
        self.tailer.resolved_snapshot(&self.cards)
    }

    pub fn card_observations(&self, card_id: &str) -> Option<&[EventSource]> {
        self.tailer.card_observations(card_id)
    }

    pub const fn arena_picks_enabled(&self) -> bool {
        self.tailer.arena_picks_enabled()
    }

    pub fn replace_cards(&mut self, cards: CardCache) {
        self.cards = cards;
    }

    /// Applies a complete visible-sidebar baseline and opens the parser gate
    /// only for log choices appended after that verified boundary.
    pub fn apply_complete_sidebar_baseline(
        &mut self,
        card_ids: Vec<String>,
        observed_slots: u16,
        expected_slots: u16,
    ) -> Result<bool> {
        self.tailer
            .apply_complete_sidebar_baseline(card_ids, observed_slots, expected_slots)
    }

    /// Set or clear the selected mode's Redraft policy. Clearing is
    /// meaningful: it disables normal crop capture once callers inspect the
    /// snapshot rather than leaving an old season's five-round assumption in
    /// memory.
    pub fn set_redraft_policy(&mut self, policy: Option<RedraftPolicy>) -> Result<()> {
        self.tailer.set_redraft_policy(policy)?;
        self.redraft_policy = policy;
        Ok(())
    }

    /// Save the current validated observer boundary. Applications normally
    /// call this after the initial attach and after a poll that reported a
    /// change; the checkpoint stays entirely in their app-data directory.
    pub fn write_checkpoint(&self, path: impl AsRef<Path>) -> Result<()> {
        self.tailer.write_checkpoint(path)
    }

    /// Poll and atomically save only when a cursor/session/state changed.
    /// A write failure is returned to the caller but does not invalidate the
    /// already-coherent in-memory observer.
    pub fn poll_and_write_checkpoint(&mut self, path: impl AsRef<Path>) -> Result<PollResult> {
        let result = self.poll()?;
        if result.changed {
            self.write_checkpoint(path)?;
        }
        Ok(result)
    }

    pub fn poll(&mut self) -> Result<PollResult> {
        let mut result = PollResult::default();
        if self.follow_latest_session {
            if let Some(root) = self.log_root.as_deref() {
                if let Some(next_session) = newest_live_session(root) {
                    if next_session != self.tailer.session
                        && session_activity_time(&next_session)
                            > session_activity_time(self.tailer.session())
                    {
                        match SessionObserver::attach_current_state_with_expected_deck_slots(
                            next_session,
                            self.expected_deck_slots,
                        ) {
                            Ok(mut tailer) => {
                                tailer.set_redraft_policy(self.redraft_policy)?;
                                self.tailer = tailer;
                                result.changed = true;
                                result.switched_session = true;
                            }
                            // Hearthstone can create a directory before all
                            // component files settle. Keep the prior verified
                            // state and retry the switch on the next poll.
                            Err(error) if is_replay_required(&error) => {
                                result.recovery_pending = true;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
            }
        }

        match self.tailer.poll() {
            Ok(changed) => result.changed |= changed,
            Err(error) if is_replay_required(&error) => {
                let session = self.tailer.session.clone();
                match SessionObserver::attach_current_state_with_expected_deck_slots(
                    session,
                    self.expected_deck_slots,
                ) {
                    Ok(mut tailer) => {
                        tailer.set_redraft_policy(self.redraft_policy)?;
                        self.tailer = tailer;
                        result.changed = true;
                        result.recovered_from_rotation = true;
                    }
                    // Do not turn a transient file-rotation window into an
                    // application failure. The existing snapshot remains a
                    // coherent prior state while we retry next poll.
                    Err(replay_error) if is_replay_required(&replay_error) => {
                        result.recovery_pending = true;
                    }
                    Err(replay_error) => return Err(replay_error),
                }
            }
            Err(error) => return Err(error),
        }
        Ok(result)
    }
}

fn is_replay_required(error: &anyhow::Error) -> bool {
    error.downcast_ref::<LogReplayRequired>().is_some()
}

pub fn resolve_snapshot(snapshot: ArenaSnapshot, cards: &CardCache) -> ObserverSnapshot {
    let resolve_deck = |deck: Vec<DeckCard>| {
        deck.into_iter()
            .map(|DeckCard { card_id, count }| ResolvedDeckCard {
                resolution: cards.resolve(&card_id),
                card_id,
                count,
            })
            .collect::<Vec<_>>()
    };
    let remaining_deck = resolve_deck(snapshot.game.remaining_deck.clone());
    ObserverSnapshot {
        schema_version: OBSERVER_SNAPSHOT_SCHEMA_VERSION,
        state_schema_version: snapshot.schema_version,
        mode: snapshot.mode,
        hero_class: snapshot.hero_class,
        deck: resolve_deck(snapshot.deck),
        remaining_deck,
        deck_state: snapshot.deck_state,
        run: snapshot.run,
        draft: snapshot.draft,
        game: snapshot.game,
    }
}

/// Selects the next item in the same deterministic order used by the
/// historical allocation-heavy sort path. `BTreeMap` retains at most one
/// pending line for each of the five components, making this a tiny bounded
/// k-way merge during initial session replay and tailing.
fn next_pending_component(pending: &BTreeMap<LogComponent, RawLogLine>) -> Option<LogComponent> {
    pending
        .iter()
        .min_by(|(_, left), (_, right)| raw_line_order(left).cmp(&raw_line_order(right)))
        .map(|(component, _)| *component)
}

// A byte offset totally orders records within one component, making the
// line-number component redundant. Omitting it lets a checkpoint validate the
// final ordering witness by reading one source line at one byte offset.
type RawLineOrder = (Option<u64>, LogComponent, u64);

fn raw_line_order(line: &RawLogLine) -> RawLineOrder {
    (line.timestamp_key, line.component, line.byte_offset)
}

/// Build a stable, local idempotency identity for one physical log line. A
/// byte offset alone is insufficient across session directories; the small
/// FNV-1a checksum additionally exposes an in-place rewrite at the same
/// offset. This is deliberately not a cryptographic integrity check.
fn event_source(session: &Path, line: &RawLogLine) -> EventSource {
    EventSource {
        session_id: session
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| session.display().to_string()),
        component: line.component.filename().to_owned(),
        byte_offset: line.byte_offset,
        line_hash: fnv1a64(line.raw.as_bytes()),
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

/// The final bytes before a durable cursor catch a copy-truncate rotation
/// where a file is overwritten in place and has already grown beyond its old
/// byte offset by the next poll. Inode checks alone cannot see that case.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileCheckpoint {
    start: u64,
    bytes: Vec<u8>,
}

const CHECKPOINT_BYTES: u64 = 64;

fn file_identity(path: &Path) -> Result<FileIdentity> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("could not read metadata for {}", path.display()))?;
    #[cfg(unix)]
    {
        Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(FileIdentity {
            created: metadata.created().ok(),
        })
    }
}

fn file_checkpoint(path: &Path, cursor: u64) -> Result<Option<FileCheckpoint>> {
    if cursor == 0 {
        return Ok(None);
    }
    let start = cursor.saturating_sub(CHECKPOINT_BYTES);
    let length = usize::try_from(cursor - start).expect("checkpoint length fits usize");
    let mut file = fs::File::open(path)
        .with_context(|| format!("could not open {} for cursor verification", path.display()))?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    Ok(Some(FileCheckpoint { start, bytes }))
}

fn checkpoint_matches(path: &Path, checkpoint: &FileCheckpoint) -> Result<bool> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("could not open {} for cursor verification", path.display()))?;
    file.seek(SeekFrom::Start(checkpoint.start))?;
    let mut bytes = vec![0; checkpoint.bytes.len()];
    match file.read_exact(&mut bytes) {
        Ok(()) => Ok(bytes == checkpoint.bytes),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("could not verify cursor for {}", path.display()))
        }
    }
}

fn validate_log_session(session: &Path) -> Result<()> {
    if !session.is_dir() {
        return Err(LogReplayRequired {
            path: Some(session.to_path_buf()),
            reason: "is not a Hearthstone log-session directory",
        }
        .into());
    }
    if !REQUIRED_COMPONENTS
        .iter()
        .any(|component| session.join(component.filename()).is_file())
    {
        return Err(LogReplayRequired {
            path: Some(session.to_path_buf()),
            reason: "does not contain a supported Hearthstone component log",
        }
        .into());
    }
    Ok(())
}

fn newest_live_session(log_root: &Path) -> Option<PathBuf> {
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
            candidates.extend(
                second_level
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir()),
            );
        }
    }
    candidates
        .into_iter()
        .filter(|candidate| validate_log_session(candidate).is_ok())
        .max_by_key(|candidate| session_activity_time(candidate))
}

fn session_activity_time(session: &Path) -> SystemTime {
    REQUIRED_COMPONENTS
        .iter()
        .filter_map(|component| fs::metadata(session.join(component.filename())).ok())
        .filter_map(|metadata| metadata.modified().ok())
        .max()
        .unwrap_or(UNIX_EPOCH)
}

/// Classify a session by how recently its newest required component log was
/// modified. This is the tripwire for the stall failure mode described on
/// [`LogStaleness`]: a live game whose writers died shows `Stale` while the
/// process and the prior snapshot remain intact.
pub fn session_staleness(session: &Path, threshold: Duration) -> LogStaleness {
    let newest = session_activity_time(session);
    if newest == UNIX_EPOCH {
        return LogStaleness::NoLogs;
    }
    match SystemTime::now().duration_since(newest) {
        Ok(age) if age > threshold => LogStaleness::Stale {
            age_secs: age.as_secs(),
        },
        _ => LogStaleness::Live,
    }
}

/// Hearthstone's own hardcoded per-component log cap (10000 KB).
///
/// When a component reaches it, the client prints "Truncating log, which has
/// reached the size limit of 10000KB" and then stalls every writer for the
/// rest of the session while the game keeps running. The macOS client ignores
/// the `FileSize` key in `log.config` (verified 2026-08-06: a session launched
/// with `FileSize=200000` still truncated Zone.log at 10000KB), so the cap
/// cannot be raised from configuration. The observer keeps every component
/// below this cap by rotating the file itself before the client's own
/// truncation path is ever triggered.
pub const GAME_LOG_CAP_BYTES: u64 = 10_000 * 1024;

/// Rotate a component log once it exceeds this size. The margin below
/// [`GAME_LOG_CAP_BYTES`] absorbs the client's periodic size-check overshoot
/// and the bytes the game writes between observer polls (~4.8 KB at the
/// observed ~1.1 MB/min Zone.log growth over a 250 ms poll).
pub const ROTATE_AT_BYTES: u64 = 9 * 1024 * 1024 + 512 * 1024;

/// Bytes retained from the newest end of a rotated log. This is purely a
/// hysteresis budget: after rotation the file sits at [`ROTATION_KEEP_BYTES`]
/// and is not rotated again until it regrows past [`ROTATE_AT_BYTES`], a gap
/// of ~7.5 MB (~7 minutes of gameplay at the observed growth rate). The
/// observer does not replay the retained bytes — its resync resumes at the
/// tail's last line — so the deck survives rotation because `Arena.log` is
/// never rotated, not because Zone history is preserved.
pub const ROTATION_KEEP_BYTES: u64 = 2 * 1024 * 1024;

/// One component log rotated in place by [`rotate_overlarge_component_logs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComponentRotation {
    pub component: &'static str,
    pub previous_bytes: u64,
    pub retained_bytes: u64,
}

/// What [`rotate_overlarge_component_logs`] did on one pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RotationOutcome {
    pub rotations: Vec<ComponentRotation>,
    /// Per-component failures. A failure on one log never aborts the pass for
    /// the others, so a sticky error cannot silently leave a later overlarge
    /// file (e.g. Zone.log) unattempted. The caller must surface these: a
    /// failed rotation leaves the file climbing toward the client's 10 MB cap.
    pub failures: Vec<String>,
}

/// Rotate every required component log that has grown past [`ROTATE_AT_BYTES`],
/// retaining only its newest [`ROTATION_KEEP_BYTES`]. Rotation is a deliberate
/// in-place rewrite: it keeps the file identity (inode) stable so the game
/// keeps appending, and the observer already recovers from the resulting file
/// shrink through its normal copy-truncate resync path. `Arena.log` is never
/// rotated — it holds the drafted-deck record, and the deck must survive a
/// rotation — so a rotation can never lose a deck, only in-game zone history
/// that the observer does not replay anyway. Per-component failures are
/// reported in the outcome rather than aborting the pass.
pub fn rotate_overlarge_component_logs(session: &Path) -> RotationOutcome {
    let mut outcome = RotationOutcome::default();
    for component in REQUIRED_COMPONENTS {
        // Arena is the authoritative drafted-deck record and is small. Keep
        // the "never rotate Arena" guarantee in code, not just in docs.
        if component == LogComponent::Arena {
            continue;
        }
        let path = session.join(component.filename());
        match rotate_component_log(&path) {
            Ok(Some((previous_bytes, retained_bytes))) => {
                outcome.rotations.push(ComponentRotation {
                    component: component.filename(),
                    previous_bytes,
                    retained_bytes,
                })
            }
            Ok(None) => {}
            Err(error) => outcome
                .failures
                .push(format!("{}: {error:#}", path.display())),
        }
    }
    outcome
}

fn rotate_component_log(path: &Path) -> Result<Option<(u64, u64)>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not stat {} before rotation", path.display()));
        }
    };
    let length = metadata.len();
    if length <= ROTATE_AT_BYTES {
        return Ok(None);
    }
    let keep = length.min(ROTATION_KEEP_BYTES);
    let start = length - keep;
    let mut tail = vec![0_u8; keep as usize];
    {
        let mut file = fs::File::open(path)
            .with_context(|| format!("could not open {} for rotation", path.display()))?;
        file.seek(SeekFrom::Start(start))
            .with_context(|| format!("could not seek {} for rotation", path.display()))?;
        file.read_exact(&mut tail)
            .with_context(|| format!("could not read {} tail for rotation", path.display()))?;
    }
    // Drop a leading partial record so the retained tail always starts at a
    // newline boundary and the observer's resync never sees a torn line.
    if let Some(first_newline) = tail.iter().position(|&byte| byte == b'\n') {
        tail.drain(..first_newline + 1);
    }
    // Rewrite the head of the existing file in place, then trim the stale
    // tail with set_len. Unlike `truncate(true)` + `write_all`, the file is
    // never exposed empty to the game's concurrent append writer. Lines the
    // game appends during the rewrite window are cut by set_len (they fall
    // beyond the retained tail), but that is the only loss and it is bounded:
    // the observer already consumed every one of those lines in the poll that
    // just ran, so rotation never costs tracker data.
    let mut out = OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("could not open {} for rotation write", path.display()))?;
    out.seek(SeekFrom::Start(0))
        .with_context(|| format!("could not seek {} for rotation write", path.display()))?;
    out.write_all(&tail)
        .with_context(|| format!("could not write rotated {}", path.display()))?;
    out.set_len(tail.len() as u64)
        .with_context(|| format!("could not trim rotated {}", path.display()))?;
    out.sync_all()
        .with_context(|| format!("could not sync rotated {}", path.display()))?;
    Ok(Some((length, tail.len() as u64)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env, fs,
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/logs/sample-arena-session")
    }

    fn fixture_cards() -> CardCache {
        CardCache::load(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/card-data/sample-cards.json"),
        )
        .unwrap()
    }

    fn temp_session() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = env::temp_dir().join(format!(
            "arena-next-observer-{}-{suffix}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn arena_line(message: &str) -> String {
        format!("D 12:00:00.0000000 {message}\n")
    }

    fn sample_component_lines(count: usize) -> Vec<u8> {
        (0..count)
            .flat_map(|index| format!("D 12:00:00.0000000 record {index}\n").into_bytes())
            .collect()
    }

    fn arena_snapshot(deck_id: &str, card_id: &str, mode: &str) -> String {
        format!(
            concat!(
                "D 12:00:00.0000000 Draft Deck ID: {deck_id}, Hero Card = HERO_08\n",
                "D 12:00:01.0000000 Draft deck contains card {card_id}\n",
                "D 12:00:02.0000000 SetDraftMode - {mode}\n"
            ),
            deck_id = deck_id,
            card_id = card_id,
            mode = mode,
        )
    }

    #[test]
    fn staleness_distinguishes_live_stale_and_missing_sessions() {
        let session = temp_session();
        assert_eq!(
            session_staleness(&session, Duration::from_secs(60)),
            LogStaleness::NoLogs
        );

        fs::write(session.join("Arena.log"), "D 12:00:00.0000000 x\n").unwrap();
        assert_eq!(
            session_staleness(&session, Duration::from_secs(3600)),
            LogStaleness::Live
        );
        // A zero threshold makes any existing log stale: the tripwire fires
        // as soon as the newest mtime stops advancing.
        assert!(matches!(
            session_staleness(&session, Duration::ZERO),
            LogStaleness::Stale { .. }
        ));
    }

    #[test]
    fn rotation_trims_an_overlarge_component_to_a_newline_aligned_tail() {
        let session = temp_session();
        let path = session.join(LogComponent::Zone.filename());
        let bytes = sample_component_lines(500_000);
        assert!(bytes.len() > ROTATE_AT_BYTES as usize);
        fs::write(&path, &bytes).unwrap();

        let rotated = rotate_component_log(&path).unwrap().unwrap().0;
        assert_eq!(rotated, bytes.len() as u64);

        let rotated = fs::read(&path).unwrap();
        assert!(rotated.len() < bytes.len());
        assert!(rotated.len() <= ROTATION_KEEP_BYTES as usize);
        assert!(rotated.first() == Some(&b'D'));
        assert_eq!(rotated.last(), Some(&b'\n'));
        // The retained tail is a suffix of the original records: every line
        // still parses and the newest record survives.
        let tail_text = String::from_utf8(rotated).unwrap();
        assert!(tail_text.ends_with("record 499999\n"));
        assert!(tail_text.lines().count() > 1);
        for line in tail_text.lines() {
            assert!(line.starts_with("D 12:00:00.0000000 record "));
        }
    }

    #[test]
    fn rotation_leaves_small_and_missing_component_logs_untouched() {
        let session = temp_session();
        let small = sample_component_lines(3);
        let arena = session.join(LogComponent::Arena.filename());
        fs::write(&arena, &small).unwrap();

        assert_eq!(rotate_component_log(&arena).unwrap(), None);
        assert_eq!(fs::read(&arena).unwrap(), small);

        assert_eq!(
            rotate_component_log(&session.join(LogComponent::Zone.filename())).unwrap(),
            None
        );
    }

    #[test]
    fn rotate_overlarge_component_logs_skips_components_without_a_file_and_reports_rotations() {
        let session = temp_session();
        let bytes = sample_component_lines(500_000);
        let zone = session.join(LogComponent::Zone.filename());
        fs::write(&zone, &bytes).unwrap();
        let arena = session.join(LogComponent::Arena.filename());
        fs::write(&arena, "D 12:00:00.0000000 keep\n").unwrap();

        let outcome = rotate_overlarge_component_logs(&session);
        assert!(outcome.failures.is_empty());
        let zone_rotation = outcome
            .rotations
            .iter()
            .find(|rotation| rotation.component == LogComponent::Zone.filename())
            .unwrap();
        assert_eq!(zone_rotation.previous_bytes, bytes.len() as u64);
        assert_eq!(
            zone_rotation.retained_bytes as usize,
            fs::metadata(&zone).unwrap().len() as usize
        );
        // Arena.log stays untouched: it holds the drafted deck.
        assert_eq!(fs::read(&arena).unwrap(), b"D 12:00:00.0000000 keep\n");
        // The rotated tail is still readable as complete records.
        let tail = String::from_utf8(fs::read(&zone).unwrap()).unwrap();
        assert!(tail.ends_with("record 499999\n"));
    }

    #[test]
    fn arena_log_is_never_rotated_even_when_overlarge() {
        let session = temp_session();
        let bytes = sample_component_lines(500_000);
        let arena = session.join(LogComponent::Arena.filename());
        fs::write(&arena, &bytes).unwrap();

        let outcome = rotate_overlarge_component_logs(&session);
        assert!(outcome.rotations.is_empty());
        assert!(outcome.failures.is_empty());
        assert_eq!(fs::metadata(&arena).unwrap().len() as usize, bytes.len());
        fs::remove_dir_all(session).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn one_component_rotation_failure_does_not_abort_the_others() {
        use std::os::unix::fs::PermissionsExt;

        let session = temp_session();
        let bytes = sample_component_lines(500_000);
        for filename in [
            LogComponent::Zone.filename(),
            LogComponent::Power.filename(),
        ] {
            fs::write(session.join(filename), &bytes).unwrap();
        }
        let power = session.join(LogComponent::Power.filename());
        fs::set_permissions(&power, fs::Permissions::from_mode(0o000)).unwrap();

        let outcome = rotate_overlarge_component_logs(&session);

        // The unreadable Power.log failed alone; Zone.log was still rotated.
        let rotated: Vec<_> = outcome.rotations.iter().map(|r| r.component).collect();
        assert!(rotated.contains(&LogComponent::Zone.filename()));
        // Self-guard for a root test runner that can read a 0o000 file.
        if fs::read(&power).is_err() {
            assert!(!rotated.contains(&LogComponent::Power.filename()));
            assert!(outcome.failures.iter().any(|f| f.contains("Power.log")));
        }
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn fixture_replay_resolves_cards_and_retains_duplicates() {
        let observer = SessionObserver::attach(fixture_path()).unwrap();
        let snapshot = observer.resolved_snapshot(&fixture_cards());
        let duplicate = snapshot
            .deck
            .iter()
            .find(|card| card.card_id == "REV_840")
            .unwrap();
        assert_eq!(duplicate.count, 2);
        assert!(matches!(
            duplicate.resolution,
            CardResolution::Resolved { .. }
        ));
    }

    #[test]
    fn complete_sidebar_baseline_replaces_previews_and_enables_only_later_picks() {
        let session = temp_session();
        let arena_log = session.join("Arena.log");
        fs::write(
            &arena_log,
            concat!(
                "D 12:00:00.0000000 OnBegin - Got new draft deck with ID: 42\n",
                "D 12:00:01.0000000 Client chooses: Preview (EX1_116)\n"
            ),
        )
        .unwrap();
        let mut observer =
            SessionObserver::attach_with_expected_deck_slots(&session, Some(30)).unwrap();
        assert_eq!(observer.state().deck_state.observed_slots, 0);
        assert!(!observer.arena_picks_enabled());

        assert!(
            observer
                .apply_complete_sidebar_baseline(
                    vec!["CS2_029".into(), "CS2_024".into(), "CS2_024".into()],
                    3,
                    30,
                )
                .unwrap()
        );
        assert_eq!(observer.state().deck_state.observed_slots, 3);
        assert!(observer.arena_picks_enabled());

        let checkpoint = session.join("observer-checkpoint.json");
        observer.write_checkpoint(&checkpoint).unwrap();
        let (mut observer, status) =
            SessionObserver::attach_with_checkpoint_and_expected_deck_slots(
                &session,
                &checkpoint,
                Some(30),
            )
            .unwrap();
        assert_eq!(status, CheckpointRestoreStatus::Restored);
        assert_eq!(observer.state().deck_state.observed_slots, 3);
        assert!(observer.arena_picks_enabled());

        let mut file = OpenOptions::new().append(true).open(&arena_log).unwrap();
        file.write_all(b"D 12:00:02.0000000 Client chooses: Later Pick (EX1_116)\n")
            .unwrap();
        file.sync_all().unwrap();
        assert!(observer.poll().unwrap());
        assert_eq!(observer.state().deck_state.observed_slots, 4);
        assert_eq!(
            observer
                .state()
                .deck
                .iter()
                .find(|card| card.card_id == "CS2_024")
                .unwrap()
                .count,
            2
        );
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn cold_replay_recovers_package_plus_all_logged_picks() {
        let session = temp_session();
        let arena_log = session.join("Arena.log");
        let mut lines = String::from(
            "D 12:00:00.0000000 DraftManager.OnChoicesAndContents - Draft Deck ID: 42, Hero Card = HERO_03\n",
        );
        for card_id in ["END_036", "TIME_002", "TIME_003", "TIME_004", "TIME_045"] {
            lines.push_str(&format!(
                "D 12:00:00.1000000 DraftManager.OnChoicesAndContents - Draft deck contains card {card_id}\n"
            ));
        }
        lines.push_str("D 12:00:01.0000000 SetDraftMode - DRAFTING\n");
        for index in 0..25 {
            lines.push_str(&format!(
                "D 12:00:{:02}.0000000 Client chooses: Pick {index} (PICK_{index:02})\n",
                index + 2
            ));
        }
        lines.push_str("D 12:00:30.0000000 SetDraftMode - ACTIVE_DRAFT_DECK\n");
        fs::write(&arena_log, lines).unwrap();

        let observer = SessionObserver::attach_with_expected_deck_slots(&session, None).unwrap();
        assert_eq!(observer.state().deck_state.observed_slots, 30);
        assert_eq!(observer.state().deck_state.expected_slots, Some(30));
        assert!(matches!(
            observer.state().deck_state.completeness,
            hs_state::DeckCompleteness::Complete
        ));

        let checkpoint = session.join("observer-checkpoint.json");
        observer.write_checkpoint(&checkpoint).unwrap();
        let (restored, status) = SessionObserver::attach_with_checkpoint_and_expected_deck_slots(
            &session,
            &checkpoint,
            None,
        )
        .unwrap();
        assert_eq!(status, CheckpointRestoreStatus::Restored);
        assert_eq!(restored.state().deck_state.observed_slots, 30);
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn partial_sidebar_baseline_is_rejected_without_mutating_or_opening_gate() {
        let session = temp_session();
        fs::write(
            session.join("Arena.log"),
            "D 12:00:00.0000000 OnBegin - Got new draft deck with ID: 42\n",
        )
        .unwrap();
        let mut observer = SessionObserver::attach(&session).unwrap();
        assert!(
            observer
                .apply_complete_sidebar_baseline(vec!["CS2_029".into()], 5, 30)
                .is_err()
        );
        assert_eq!(observer.state().deck_state.observed_slots, 0);
        assert!(!observer.arena_picks_enabled());
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn local_expected_slot_rule_makes_a_partial_deck_truthful_without_a_default() {
        let observer = LiveObserver::attach_with_expected_deck_slots(
            fixture_path(),
            fixture_cards(),
            Some(30),
        )
        .unwrap();
        let deck_state = observer.snapshot().deck_state;
        assert_eq!(deck_state.expected_slots, Some(30));
        assert_eq!(deck_state.observed_slots, 8);
        assert_eq!(deck_state.unobserved_slots, Some(22));
        assert!(matches!(
            deck_state.completeness,
            hs_state::DeckCompleteness::Partial {
                reason: hs_state::PartialDeckReason::UnobservedSlots
            }
        ));

        // The same logs without local rules retain the existing
        // authoritative-snapshot inference (8), proving no observer default
        // was introduced by the rules API.
        let without_rule = LiveObserver::attach(fixture_path(), fixture_cards())
            .unwrap()
            .snapshot()
            .deck_state;
        assert_eq!(without_rule.expected_slots, Some(8));
    }

    #[test]
    fn tail_snapshot_hydrates_current_deck_without_power_history() {
        let session = temp_session();
        let old_arena_prefix = format!(
            "D 09:00:00.0000000 old diagnostic {}\n",
            "x".repeat(96 * 1024)
        );
        fs::write(
            session.join("Arena.log"),
            format!(
                "{old_arena_prefix}{}",
                concat!(
                    "D 11:00:00.0000000 Draft Deck ID: 2, Hero Card = HERO_08\n",
                    "D 11:00:01.0000000 Draft deck contains card CS2_029\n",
                    "D 11:00:02.0000000 Draft deck contains card CS2_024\n",
                    "D 11:00:03.0000000 SetDraftMode - ACTIVE_DRAFT_DECK\n"
                )
            ),
        )
        .unwrap();
        // A live tail attach must not replay this history. A full replay would
        // leave `game.active` true; the product-state attach deliberately
        // starts gameplay state unknown/inactive until a new append arrives.
        fs::write(
            session.join("Power.log"),
            format!(
                "D 08:00:00.0000000 GameState.DebugPrintPower() - CREATE_GAME {}\n",
                "z".repeat(2 * 1024 * 1024)
            ),
        )
        .unwrap();

        let observer =
            SessionObserver::attach_current_state_with_expected_deck_slots(&session, Some(30))
                .unwrap();
        assert_eq!(observer.attach_method(), AttachMethod::TailSnapshot);
        assert_eq!(observer.state().run.draft_deck_id.as_deref(), Some("2"));
        assert_eq!(observer.state().deck_state.observed_slots, 2);
        assert!(
            observer
                .state()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_029")
        );
        assert!(matches!(
            observer.state().draft.history_status,
            hs_state::DraftHistoryStatus::Partial { .. }
        ));
        assert!(observer.state().draft.selections.is_empty());
        assert!(!observer.state().game.active);
        assert_eq!(
            observer
                .cursors()
                .get(&LogComponent::Power)
                .unwrap()
                .byte_offset,
            fs::metadata(session.join("Power.log")).unwrap().len()
        );
        let diagnostics = observer.attach_diagnostics();
        assert!(diagnostics.snapshot_byte_offset.is_some());
        assert!(diagnostics.snapshot_bytes_parsed < 8 * 1024);
        assert!(diagnostics.non_arena_components_started_at_tail);

        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn tail_snapshot_reports_when_a_large_arena_suffix_is_skipped() {
        let session = temp_session();
        let suffix = format!(
            "{}{}",
            arena_line("Client chooses: Fireball (CS2_029)"),
            arena_line(&format!(
                "inert diagnostic {}",
                "x".repeat(usize::try_from(TAIL_SNAPSHOT_SUFFIX_BYTES + 1).unwrap())
            ))
        );
        fs::write(
            session.join("Arena.log"),
            format!(
                "{}{}",
                arena_snapshot("1", "CS2_024", "ACTIVE_DRAFT_DECK"),
                suffix
            ),
        )
        .unwrap();

        let observer = SessionObserver::attach_current_state(&session).unwrap();
        let diagnostics = observer.attach_diagnostics();
        assert_eq!(observer.attach_method(), AttachMethod::TailSnapshot);
        assert!(diagnostics.arena_suffix_truncated);
        assert!(diagnostics.arena_suffix_bytes_skipped > TAIL_SNAPSHOT_SUFFIX_BYTES);
        assert!(observer.state().draft.selected.is_none());
        assert!(
            observer
                .state()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_024")
        );

        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn newest_incomplete_snapshot_never_reuses_an_older_deck() {
        let session = temp_session();
        fs::write(
            session.join("Arena.log"),
            concat!(
                "D 10:00:00.0000000 Draft Deck ID: 1, Hero Card = HERO_08\n",
                "D 10:00:01.0000000 Draft deck contains card CS2_029\n",
                "D 10:00:02.0000000 SetDraftMode - ACTIVE_DRAFT_DECK\n",
                "D 11:00:00.0000000 Draft Deck ID: 2, Hero Card = HERO_08\n",
                "D 11:00:01.0000000 Draft deck contains card CS2_024\n"
            ),
        )
        .unwrap();

        let observer = SessionObserver::attach_current_state(&session).unwrap();
        assert_eq!(observer.attach_method(), AttachMethod::AwaitingSnapshot);
        assert_eq!(observer.state().run.draft_deck_id.as_deref(), Some("2"));
        assert!(!observer.state().run.deck_snapshot_complete);
        assert!(
            observer
                .state()
                .deck
                .iter()
                .all(|card| card.card_id != "CS2_029")
        );
        assert!(
            observer
                .state()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_024")
        );

        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn newer_run_start_never_reuses_a_prior_run_snapshot() {
        let session = temp_session();
        fs::write(
            session.join("Arena.log"),
            concat!(
                "D 10:00:00.0000000 Draft Deck ID: 1, Hero Card = HERO_08\n",
                "D 10:00:01.0000000 Draft deck contains card CS2_029\n",
                "D 10:00:02.0000000 SetDraftMode - ACTIVE_DRAFT_DECK\n",
                "D 11:00:00.0000000 DraftManager.OnBegin - Got new draft deck with ID: 2\n",
                "D 11:00:01.0000000 SetDraftMode - DRAFTING\n",
                "D 11:00:02.0000000 Client chooses: Valeera Sanguinar (HERO_03)\n"
            ),
        )
        .unwrap();

        let observer = SessionObserver::attach_current_state(&session).unwrap();
        assert_eq!(observer.attach_method(), AttachMethod::TailRun);
        assert!(observer.state().deck.is_empty());
        assert_eq!(observer.state().run.draft_deck_id.as_deref(), Some("2"));
        assert_eq!(observer.state().draft.phase_pick_count, 0);
        assert!(observer.state().hero_class.is_none());
        assert!(
            observer
                .attach_diagnostics()
                .snapshot_invalidated_by_newer_run
        );

        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn checkpoint_catches_up_a_new_run_before_exposing_restored_state() {
        let session = temp_session();
        let arena_log = session.join("Arena.log");
        let checkpoint = session.join("observer-checkpoint.json");
        fs::write(
            &arena_log,
            arena_snapshot("1", "CS2_029", "ACTIVE_DRAFT_DECK"),
        )
        .unwrap();
        let observer = SessionObserver::attach_current_state(&session).unwrap();
        observer.write_checkpoint(&checkpoint).unwrap();

        let mut file = OpenOptions::new().append(true).open(&arena_log).unwrap();
        file.write_all(
            b"D 12:01:00.0000000 DraftManager.OnBegin - Got new draft deck with ID: 2\n",
        )
        .unwrap();
        file.sync_all().unwrap();

        let (restored, status) =
            SessionObserver::attach_with_checkpoint(&session, &checkpoint).unwrap();
        assert_eq!(status, CheckpointRestoreStatus::Restored);
        assert_eq!(restored.attach_method(), AttachMethod::VerifiedCheckpoint);
        assert!(
            restored
                .attach_diagnostics()
                .checkpoint_suffix_bytes_replayed
                > 0
        );
        assert_eq!(restored.state().run.draft_deck_id.as_deref(), Some("2"));
        assert!(restored.state().deck.is_empty());

        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn oversized_checkpoint_suffix_uses_tail_resync_instead_of_history_catch_up() {
        let session = temp_session();
        let checkpoint = session.join("observer-checkpoint.json");
        fs::write(
            session.join("Arena.log"),
            arena_snapshot("1", "CS2_029", "ACTIVE_DRAFT_DECK"),
        )
        .unwrap();
        let power_log = session.join("Power.log");
        fs::write(&power_log, arena_line("inert power diagnostic")).unwrap();
        let observer = SessionObserver::attach_current_state(&session).unwrap();
        observer.write_checkpoint(&checkpoint).unwrap();

        let mut power = OpenOptions::new().append(true).open(&power_log).unwrap();
        power
            .write_all(
                format!(
                    "D 12:01:00.0000000 GameState.DebugPrintPower() - CREATE_GAME {}\n",
                    "z".repeat(usize::try_from(CHECKPOINT_CATCH_UP_BYTES + 1).unwrap())
                )
                .as_bytes(),
            )
            .unwrap();
        power.sync_all().unwrap();

        let (resynced, status) =
            SessionObserver::attach_with_checkpoint(&session, &checkpoint).unwrap();
        assert!(matches!(
            status,
            CheckpointRestoreStatus::Rejected { ref reason }
                if reason.contains("newly appended event-log bytes")
        ));
        assert_eq!(resynced.attach_method(), AttachMethod::TailSnapshot);
        assert!(
            resynced
                .state()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_029")
        );
        assert!(!resynced.state().game.active);
        assert_eq!(
            resynced
                .cursors()
                .get(&LogComponent::Power)
                .expect("Power cursor should be retained")
                .byte_offset,
            fs::metadata(&power_log).unwrap().len()
        );

        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn verified_checkpoint_resumes_suffix_and_parser_state() {
        let session = temp_session();
        let arena_log = session.join("Arena.log");
        let checkpoint = session.join("observer-checkpoint.json");
        fs::write(
            &arena_log,
            concat!(
                "D 12:00:00.0000000 Draft Deck ID: 77, Hero Card = HERO_08\n",
                "D 12:00:01.0000000 Draft deck contains card CS2_029\n"
            ),
        )
        .unwrap();

        let observer =
            SessionObserver::attach_with_expected_deck_slots(&session, Some(30)).unwrap();
        assert_eq!(observer.state().deck_state.observed_slots, 1);
        observer.write_checkpoint(&checkpoint).unwrap();

        let (mut restored, status) =
            SessionObserver::attach_with_checkpoint_and_expected_deck_slots(
                &session,
                &checkpoint,
                Some(30),
            )
            .unwrap();
        assert_eq!(status, CheckpointRestoreStatus::Restored);
        assert_eq!(restored.state().deck_state.expected_slots, Some(30));
        assert_eq!(
            restored
                .card_observations("CS2_029")
                .expect("checkpoint retains bounded diagnostic provenance")
                .len(),
            1
        );

        // This line is only meaningful while `reading_arena_deck` remains
        // true, which proves the parser's state crossed the restart boundary
        // along with the reducer and cursor.
        let mut file = OpenOptions::new().append(true).open(&arena_log).unwrap();
        file.write_all(b"D 12:00:02.0000000 Draft deck contains card CS2_024\n")
            .unwrap();
        file.sync_all().unwrap();
        assert!(restored.poll().unwrap());
        assert_eq!(restored.state().deck_state.observed_slots, 2);
        assert!(
            restored
                .state()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_024")
        );
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn source_mismatch_rejects_checkpoint_and_never_restores_stale_history() {
        let session = temp_session();
        let arena_log = session.join("Arena.log");
        let checkpoint = session.join("observer-checkpoint.json");
        let inert_tail = format!("D 12:00:01.0000000 inert diagnostic {}\n", "x".repeat(160));
        fs::write(
            &arena_log,
            format!(
                "{}{}{}",
                arena_line("DraftManager.OnChosen(): hero=HERO_08"),
                inert_tail,
                inert_tail
            ),
        )
        .unwrap();
        let observer = SessionObserver::attach(&session).unwrap();
        observer.write_checkpoint(&checkpoint).unwrap();

        // Keep the file length and final 64 bytes unchanged. A tail-only
        // witness would accept this rewrite; validating the saved source line
        // must reject the stale checkpoint. Live attach then waits for a new
        // authoritative snapshot instead of secretly replaying history.
        let rewritten = fs::read_to_string(&arena_log)
            .unwrap()
            .replace("HERO_08", "HERO_03");
        fs::write(&arena_log, rewritten).unwrap();

        let (resynced, status) =
            SessionObserver::attach_with_checkpoint(&session, &checkpoint).unwrap();
        assert!(matches!(status, CheckpointRestoreStatus::Rejected { .. }));
        assert_eq!(resynced.attach_method(), AttachMethod::AwaitingSnapshot);
        assert!(resynced.state().draft.selected.is_none());
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn checkpoint_uses_current_rule_instead_of_saved_manifest_value() {
        let session = temp_session();
        let checkpoint = session.join("observer-checkpoint.json");
        fs::write(
            session.join("Arena.log"),
            arena_line("Client chooses: Fireball (CS2_029)"),
        )
        .unwrap();
        let observer =
            SessionObserver::attach_with_expected_deck_slots(&session, Some(30)).unwrap();
        observer.write_checkpoint(&checkpoint).unwrap();

        let (restored, status) = SessionObserver::attach_with_checkpoint_and_expected_deck_slots(
            &session,
            &checkpoint,
            Some(40),
        )
        .unwrap();
        assert_eq!(status, CheckpointRestoreStatus::Restored);
        assert_eq!(restored.state().deck_state.expected_slots, Some(40));
        assert_eq!(restored.state().deck_state.unobserved_slots, Some(40));
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn waits_for_partial_lines_then_consumes_the_completed_line() {
        let session = temp_session();
        let path = session.join("Arena.log");
        fs::write(&path, "D 12:00:00.0000000 SetDraftMode - DRAFTING").unwrap();
        let mut observer = SessionObserver::attach(&session).unwrap();
        assert!(observer.state().deck.is_empty());

        fs::write(&path, "D 12:00:00.0000000 SetDraftMode - DRAFTING\n").unwrap();
        assert!(observer.poll().unwrap());
        assert_eq!(
            observer.state().run.draft_phase,
            hs_state::ArenaDraftPhase::Drafting
        );
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn live_observer_resyncs_current_deck_after_log_truncation() {
        let session = temp_session();
        let path = session.join("Arena.log");
        fs::write(&path, arena_snapshot("1", "CS2_029", "ACTIVE_DRAFT_DECK")).unwrap();
        let mut observer = LiveObserver::attach(&session, fixture_cards()).unwrap();

        fs::write(&path, arena_snapshot("2", "CS2_024", "ACTIVE_DRAFT_DECK")).unwrap();
        let result = observer.poll().unwrap();
        assert!(result.recovered_from_rotation);
        assert_eq!(observer.snapshot().run.draft_deck_id.as_deref(), Some("2"));
        assert!(
            observer
                .snapshot()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_024")
        );
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn live_observer_detects_an_in_place_rewrite_that_is_longer_than_cursor() {
        let session = temp_session();
        let path = session.join("Arena.log");
        fs::write(&path, arena_snapshot("1", "CS2_029", "ACTIVE_DRAFT_DECK")).unwrap();
        let mut observer = LiveObserver::attach(&session, fixture_cards()).unwrap();

        // `fs::write` keeps the inode and the replacement is intentionally
        // longer than the previous cursor. A length-only tailer would treat
        // this as an append and corrupt the state.
        fs::write(
            &path,
            format!(
                "{}{}",
                arena_snapshot("2", "CS2_024", "ACTIVE_DRAFT_DECK"),
                arena_line(&format!("inert diagnostic {}", "x".repeat(256)))
            ),
        )
        .unwrap();

        let result = observer.poll().unwrap();
        assert!(result.recovered_from_rotation);
        assert_eq!(observer.snapshot().run.draft_deck_id.as_deref(), Some("2"));
        assert!(
            observer
                .snapshot()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_024")
        );
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn live_observer_replays_when_a_component_appears_after_attachment() {
        let session = temp_session();
        fs::write(
            session.join("Arena.log"),
            arena_snapshot("1", "CS2_029", "ACTIVE_DRAFT_DECK"),
        )
        .unwrap();
        let mut observer = LiveObserver::attach(&session, fixture_cards()).unwrap();
        fs::write(
            session.join("LoadingScreen.log"),
            arena_line("LoadingScreen.OnSceneLoaded - ARENA"),
        )
        .unwrap();

        let result = observer.poll().unwrap();
        assert!(result.recovered_from_rotation);
        assert_eq!(observer.snapshot().mode, hs_state::GameMode::Arena);
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn rotation_then_resync_preserves_the_deck_across_the_full_observer_cycle() {
        let session = temp_session();
        fs::write(
            session.join(LogComponent::Arena.filename()),
            arena_snapshot("1", "CS2_029", "ACTIVE_DRAFT_DECK"),
        )
        .unwrap();
        let zone = session.join(LogComponent::Zone.filename());
        let zone_bytes = sample_component_lines(500_000);
        assert!(zone_bytes.len() > ROTATE_AT_BYTES as usize);
        fs::write(&zone, &zone_bytes).unwrap();
        let mut observer = LiveObserver::attach(&session, fixture_cards()).unwrap();

        assert!(
            observer
                .snapshot()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_029")
        );

        // This is exactly what the app worker does after each poll: rotate any
        // overlarge component, then let the observer's next poll resync.
        let outcome = rotate_overlarge_component_logs(&session);
        assert!(outcome.failures.is_empty());
        assert_eq!(outcome.rotations.len(), 1);
        assert_eq!(
            outcome.rotations[0].component,
            LogComponent::Zone.filename()
        );
        assert_eq!(outcome.rotations[0].previous_bytes, zone_bytes.len() as u64);

        let result = observer.poll().unwrap();
        assert!(result.recovered_from_rotation);
        // The drafted deck survived the rotation: Arena.log is never rotated
        // and the resync re-attached the retained Zone.log tail.
        assert_eq!(observer.snapshot().run.draft_deck_id.as_deref(), Some("1"));
        assert!(
            observer
                .snapshot()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_029")
        );
        assert!(
            fs::metadata(&zone).unwrap().len() <= ROTATE_AT_BYTES,
            "rotated log must be below the next rotation threshold"
        );
        assert!(
            fs::metadata(&session.join(LogComponent::Arena.filename()))
                .unwrap()
                .len()
                > 0,
            "Arena.log must be untouched by rotation"
        );
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn live_observer_retains_last_snapshot_during_a_transient_rotation_window() {
        let session = temp_session();
        let path = session.join("Arena.log");
        fs::write(&path, arena_snapshot("1", "CS2_029", "ACTIVE_DRAFT_DECK")).unwrap();
        let mut observer = LiveObserver::attach(&session, fixture_cards()).unwrap();

        fs::remove_file(&path).unwrap();
        let pending = observer.poll().unwrap();
        assert!(pending.recovery_pending);
        assert!(
            observer
                .snapshot()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_029")
        );

        fs::write(&path, arena_snapshot("2", "CS2_024", "ACTIVE_DRAFT_DECK")).unwrap();
        let recovered = observer.poll().unwrap();
        assert!(recovered.recovered_from_rotation);
        assert!(
            observer
                .snapshot()
                .deck
                .iter()
                .any(|card| card.card_id == "CS2_024")
        );
        fs::remove_dir_all(session).unwrap();
    }

    #[test]
    fn full_replay_and_follow_observer_switches_to_a_newer_session() {
        let root = temp_session();
        let old = root.join("old");
        fs::create_dir_all(&old).unwrap();
        fs::write(
            old.join("Arena.log"),
            arena_snapshot("1", "CS2_029", "ACTIVE_DRAFT_DECK"),
        )
        .unwrap();
        let paths = GamePaths {
            install_dir: None,
            log_config: None,
            log_roots_checked: vec![root.clone()],
            log_root: Some(root.clone()),
            latest_session: Some(old.clone()),
        };
        let mut observer =
            LiveObserver::attach_full_replay_and_follow_discovered_with_expected_deck_slots(
                &paths,
                fixture_cards(),
                None,
            )
            .unwrap();
        assert_eq!(observer.session(), old);

        // Hearthstone starts a fresh session while the observer is attached.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let current = root.join("current");
        fs::create_dir_all(&current).unwrap();
        fs::write(
            current.join("Arena.log"),
            arena_snapshot("2", "CS2_024", "ACTIVE_DRAFT_DECK"),
        )
        .unwrap();

        // Regression guard for the live overlay's bounded-history recovery:
        // replacing a session-following observer with a fixed full-replay
        // attach used to pin the tracker to the old session forever, which
        // also stopped proactive rotation of the new session's logs.
        let result = observer.poll().unwrap();
        assert!(result.switched_session);
        assert_eq!(observer.session(), current);
        assert_eq!(observer.snapshot().run.draft_deck_id.as_deref(), Some("2"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn newest_live_session_prefers_a_new_partial_session() {
        let root = temp_session();
        let old = root.join("old");
        let current = root.join("current");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&current).unwrap();
        fs::write(
            old.join("Arena.log"),
            arena_line("Client chooses: Fireball (CS2_029)"),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(
            current.join("LoadingScreen.log"),
            arena_line("LoadingScreen.OnSceneLoaded - ARENA"),
        )
        .unwrap();

        assert_eq!(newest_live_session(&root), Some(current));
        fs::remove_dir_all(root).unwrap();
    }
}
