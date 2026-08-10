#![deny(unsafe_op_in_unsafe_fn)]

//! Typed parsing of Hearthstone log sessions.
//!
//! The parser owns syntax recognition only. `hs-state` owns state transitions,
//! which makes fixture replay deterministic and keeps UI code out of the log
//! integration.

use std::{
    cmp::Ordering,
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
    sync::LazyLock,
};

use hs_state::{GameEvent, is_real_card_id};
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogComponent {
    LoadingScreen,
    Power,
    Zone,
    Arena,
    Asset,
}

impl LogComponent {
    pub fn filename(self) -> &'static str {
        match self {
            Self::LoadingScreen => "LoadingScreen.log",
            Self::Power => "Power.log",
            Self::Zone => "Zone.log",
            Self::Arena => "Arena.log",
            Self::Asset => "Asset.log",
        }
    }

    pub fn from_filename(filename: &str) -> Option<Self> {
        match filename.strip_suffix(".log").unwrap_or(filename) {
            "LoadingScreen" => Some(Self::LoadingScreen),
            "Power" => Some(Self::Power),
            "Zone" => Some(Self::Zone),
            "Arena" => Some(Self::Arena),
            "Asset" => Some(Self::Asset),
            _ => None,
        }
    }
}

pub const REQUIRED_COMPONENTS: [LogComponent; 5] = [
    LogComponent::LoadingScreen,
    LogComponent::Power,
    LogComponent::Zone,
    LogComponent::Arena,
    LogComponent::Asset,
];

/// Whether the current parser produces reducer events for this component.
///
/// We still tail every [`REQUIRED_COMPONENTS`] file, including the components
/// that presently have no state transitions. Callers can use this to advance
/// those cursors without retaining every raw line from a long session. Update
/// this function whenever [`HearthstoneLogParser::parse_line`] gains a new
/// component branch that emits events.
pub const fn component_can_emit_events(component: LogComponent) -> bool {
    matches!(
        component,
        LogComponent::LoadingScreen | LogComponent::Power | LogComponent::Arena
    )
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawLogLine {
    pub component: LogComponent,
    pub line_number: u64,
    pub byte_offset: u64,
    pub timestamp_key: Option<u64>,
    pub message: String,
    pub raw: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogCursor {
    pub byte_offset: u64,
    pub line_number: u64,
}

/// Complete records from a bounded suffix of one component log.
///
/// The first physical record is deliberately omitted when the suffix begins
/// in the middle of a line. `line_number` is zero for these records: a bounded
/// reverse read knows byte provenance exactly, but does not count the whole
/// prefix merely to manufacture a line number. Consumers that need a durable
/// identity must use `byte_offset`.
#[derive(Clone, Debug)]
pub struct ComponentTail {
    pub lines: Vec<RawLogLine>,
    /// Position immediately after the final complete record. A reader opened
    /// here will re-read a partial EOF record once Hearthstone completes it.
    pub cursor: LogCursor,
    /// Whether the suffix includes byte zero of the file.
    pub covers_file_start: bool,
}

/// Raw reverse-search block size. It is intentionally independent of the
/// observer's product-state suffix budget: a large Arena log may require
/// several cheap byte reads to locate its newest snapshot marker, but never a
/// full line-by-line parse just to find that marker.
pub const REVERSE_SEARCH_CHUNK_BYTES: usize = 64 * 1024;

/// A raw marker line located by [`find_last_line_containing`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReverseLineMatch {
    pub line_start: u64,
    /// False means the marker currently lives in an unterminated EOF line.
    /// Consumers must treat it as pending rather than accepting it as an
    /// authoritative record.
    pub complete: bool,
}

/// Locate the start byte of the newest log line containing `needle`.
///
/// This scans raw bytes from EOF in fixed-size blocks, overlaps the blocks so
/// a marker split across a boundary is not missed, then walks backward only
/// far enough to find that line's preceding newline. It does *not* validate
/// Hearthstone grammar; callers must parse the returned line before trusting
/// it. `None` means no matching line exists. The newest matching line is
/// returned even if it is still unterminated so callers cannot accidentally
/// fall back to an older snapshot while Hearthstone is writing a new one.
pub fn find_last_line_containing(
    path: impl AsRef<Path>,
    needle: &[u8],
) -> io::Result<Option<ReverseLineMatch>> {
    if needle.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reverse-search needle must be nonempty",
        ));
    }
    let path = path.as_ref();
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let overlap = u64::try_from(needle.len().saturating_sub(1))
        .map_err(|_| io::Error::other("reverse-search overlap does not fit u64"))?;
    let mut primary_end = file_len;

    while primary_end > 0 {
        let primary_start = primary_end.saturating_sub(REVERSE_SEARCH_CHUNK_BYTES as u64);
        let read_end = primary_end.saturating_add(overlap).min(file_len);
        let length = usize::try_from(read_end - primary_start)
            .map_err(|_| io::Error::other("reverse-search block length does not fit usize"))?;
        let mut bytes = vec![0_u8; length];
        file.seek(SeekFrom::Start(primary_start))?;
        file.read_exact(&mut bytes)?;

        let primary_length = usize::try_from(primary_end - primary_start)
            .map_err(|_| io::Error::other("reverse-search primary length does not fit usize"))?;
        let mut search_end = bytes.len();
        while search_end >= needle.len() {
            let Some(relative) = find_subslice_from_end(&bytes[..search_end], needle) else {
                break;
            };
            // A match that starts in the right overlap belongs to the later
            // block we already searched. Ignore it here to make progress.
            if relative < primary_length {
                let marker = primary_start
                    .checked_add(u64::try_from(relative).map_err(|_| {
                        io::Error::other("reverse-search match offset does not fit u64")
                    })?)
                    .ok_or_else(|| io::Error::other("reverse-search match offset overflow"))?;
                let line_start = find_line_start(&mut file, marker)?;
                return Ok(Some(ReverseLineMatch {
                    line_start,
                    complete: line_is_complete(&mut file, marker)?,
                }));
            }
            search_end = relative;
        }
        primary_end = primary_start;
    }
    Ok(None)
}

/// Return the cursor immediately after the last complete line without
/// allocating or decoding all prior records. A partial EOF line remains
/// unread so the incremental tailer can parse it once Hearthstone finishes
/// writing it.
pub fn tail_cursor(path: impl AsRef<Path>) -> io::Result<LogCursor> {
    let path = path.as_ref();
    let mut file = File::open(path)?;
    let mut search_end = file.metadata()?.len();
    let mut buffer = vec![0_u8; REVERSE_SEARCH_CHUNK_BYTES];
    loop {
        let start = search_end.saturating_sub(REVERSE_SEARCH_CHUNK_BYTES as u64);
        let length = usize::try_from(search_end - start)
            .map_err(|_| io::Error::other("tail cursor chunk length does not fit usize"))?;
        if length == 0 {
            return Ok(LogCursor::default());
        }
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..length])?;
        if let Some(index) = buffer[..length].iter().rposition(|byte| *byte == b'\n') {
            return Ok(LogCursor {
                byte_offset: start
                    .checked_add(
                        u64::try_from(index + 1)
                            .map_err(|_| io::Error::other("tail cursor index does not fit u64"))?,
                    )
                    .ok_or_else(|| io::Error::other("tail cursor offset overflow"))?,
                line_number: 0,
            });
        }
        if start == 0 {
            return Ok(LogCursor::default());
        }
        search_end = start;
    }
}

fn find_subslice_from_end(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

fn find_line_start(file: &mut File, offset: u64) -> io::Result<u64> {
    let mut search_end = offset;
    let mut buffer = vec![0_u8; REVERSE_SEARCH_CHUNK_BYTES];
    loop {
        let start = search_end.saturating_sub(REVERSE_SEARCH_CHUNK_BYTES as u64);
        let length = usize::try_from(search_end - start)
            .map_err(|_| io::Error::other("line-start chunk length does not fit usize"))?;
        if length == 0 {
            return Ok(0);
        }
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..length])?;
        if let Some(index) = buffer[..length].iter().rposition(|byte| *byte == b'\n') {
            return start
                .checked_add(
                    u64::try_from(index + 1)
                        .map_err(|_| io::Error::other("line-start index does not fit u64"))?,
                )
                .ok_or_else(|| io::Error::other("line-start offset overflow"));
        }
        if start == 0 {
            return Ok(0);
        }
        search_end = start;
    }
}

fn line_is_complete(file: &mut File, marker: u64) -> io::Result<bool> {
    let file_len = file.metadata()?.len();
    let mut offset = marker;
    let mut buffer = vec![0_u8; REVERSE_SEARCH_CHUNK_BYTES];
    while offset < file_len {
        let remaining = file_len - offset;
        let length = usize::try_from(remaining.min(REVERSE_SEARCH_CHUNK_BYTES as u64))
            .map_err(|_| io::Error::other("line-completeness chunk length does not fit usize"))?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut buffer[..length])?;
        if buffer[..length].contains(&b'\n') {
            return Ok(true);
        }
        offset = offset
            .checked_add(
                u64::try_from(length)
                    .map_err(|_| io::Error::other("line-completeness advance does not fit u64"))?,
            )
            .ok_or_else(|| io::Error::other("line-completeness offset overflow"))?;
    }
    Ok(false)
}

/// Read at most `max_bytes` from the end of a component log without treating
/// an incomplete record as a valid event.
///
/// `None` means the bounded window begins inside one over-large record and
/// contains no newline from which a safe cursor can be established. Callers
/// must fall back to a normal forward replay in that case. This helper never
/// reads before its requested window and never invents a line boundary.
pub fn read_component_tail(
    component: LogComponent,
    path: impl AsRef<Path>,
    max_bytes: u64,
) -> io::Result<Option<ComponentTail>> {
    if max_bytes == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "component tail limit must be nonzero",
        ));
    }

    let path = path.as_ref();
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let start = file_len.saturating_sub(max_bytes);
    let length = usize::try_from(file_len - start)
        .map_err(|_| io::Error::other("component tail length does not fit usize"))?;
    let mut bytes = vec![0_u8; length];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut bytes)?;

    if bytes.is_empty() {
        return Ok(Some(ComponentTail {
            lines: Vec::new(),
            cursor: LogCursor::default(),
            covers_file_start: true,
        }));
    }

    // A suffix beginning part way through a record may not parse its first
    // bytes as a line. Skip to the next proven delimiter instead.
    let first_record_start = if start == 0 {
        0
    } else {
        let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n') else {
            return Ok(None);
        };
        first_newline + 1
    };
    let complete_end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1);
    let Some(complete_end) = complete_end else {
        // A whole-file partial record has no durable line yet. In contrast to
        // a suffix partial record, byte zero is still a known safe cursor.
        return Ok((start == 0).then_some(ComponentTail {
            lines: Vec::new(),
            cursor: LogCursor::default(),
            covers_file_start: true,
        }));
    };

    let mut lines = Vec::new();
    let mut record_start = first_record_start;
    while record_start < complete_end {
        let Some(relative_end) = bytes[record_start..complete_end]
            .iter()
            .position(|byte| *byte == b'\n')
        else {
            break;
        };
        let newline = record_start + relative_end;
        let raw_bytes = &bytes[record_start..newline];
        let raw = std::str::from_utf8(raw_bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} contains non-UTF-8 log data", path.display()),
            )
        })?;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let (timestamp_key, message) = split_timestamp(raw);
        lines.push(RawLogLine {
            component,
            line_number: 0,
            byte_offset: start
                .checked_add(u64::try_from(record_start).map_err(|_| {
                    io::Error::other("component tail record offset does not fit u64")
                })?)
                .ok_or_else(|| io::Error::other("component tail record offset overflow"))?,
            timestamp_key,
            message: message.to_owned(),
            raw: raw.to_owned(),
        });
        record_start = newline + 1;
    }

    Ok(Some(ComponentTail {
        lines,
        cursor: LogCursor {
            byte_offset: start
                .checked_add(
                    u64::try_from(complete_end)
                        .map_err(|_| io::Error::other("component tail cursor does not fit u64"))?,
                )
                .ok_or_else(|| io::Error::other("component tail cursor overflow"))?,
            line_number: 0,
        },
        covers_file_start: start == 0,
    }))
}

/// Incremental reader for one Hearthstone component log.
///
/// Unlike [`read_component_file_from`], this keeps at most one raw line in
/// memory. It is intended for long-running sessions where `Power.log` and
/// `Zone.log` can grow to tens of megabytes. An unterminated final line is
/// deliberately rewound to the durable cursor so a later append is parsed as
/// one complete record rather than two malformed records.
pub struct ComponentLogReader {
    component: LogComponent,
    reader: BufReader<File>,
    cursor: LogCursor,
    raw: String,
}

impl ComponentLogReader {
    /// Opens a component at a previously durable cursor.
    pub fn open(
        component: LogComponent,
        path: impl AsRef<Path>,
        cursor: LogCursor,
    ) -> io::Result<Self> {
        let file = File::open(path)?;
        // Hearthstone logs can be tens of megabytes. A larger bounded buffer
        // noticeably reduces syscall/UTF-8 boundary overhead without adding a
        // runtime dependency or retaining session history in memory.
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        reader.seek(SeekFrom::Start(cursor.byte_offset))?;
        Ok(Self {
            component,
            reader,
            cursor,
            raw: String::new(),
        })
    }

    /// Reads the next complete record, retaining no previous record.
    pub fn next_complete_line(&mut self) -> io::Result<Option<RawLogLine>> {
        let Some((byte_offset, line_number)) = self.read_next_complete_record()? else {
            return Ok(None);
        };
        Ok(Some(self.materialize_line(byte_offset, line_number)))
    }

    /// Reads forward until the next record which could produce a state event.
    ///
    /// Most Power and LoadingScreen lines are irrelevant to Arena state. This
    /// avoids timestamp regex work and two string copies for those lines while
    /// still advancing the durable cursor through every complete record.
    pub fn next_event_candidate(&mut self) -> io::Result<Option<RawLogLine>> {
        loop {
            let Some((byte_offset, line_number)) = self.read_next_complete_record()? else {
                return Ok(None);
            };
            let untrimmed = self.raw.trim_end_matches(['\n', '\r']);
            if component_might_emit_event(self.component, untrimmed) {
                return Ok(Some(self.materialize_line(byte_offset, line_number)));
            }
        }
    }

    fn read_next_complete_record(&mut self) -> io::Result<Option<(u64, u64)>> {
        self.raw.clear();
        let bytes = self.reader.read_line(&mut self.raw)?;
        if bytes == 0 {
            return Ok(None);
        }
        if !self.raw.ends_with('\n') {
            // `BufRead::read_line` consumes a partial trailing record. Seek
            // back to the last complete cursor so a later call can re-read it
            // after Hearthstone finishes writing the line.
            self.reader.seek(SeekFrom::Start(self.cursor.byte_offset))?;
            return Ok(None);
        }

        let byte_offset = self.cursor.byte_offset;
        let line_number = self
            .cursor
            .line_number
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Hearthstone log line number overflow"))?;
        self.cursor = LogCursor {
            byte_offset: byte_offset
                .checked_add(u64::try_from(bytes).map_err(|_| {
                    io::Error::other("Hearthstone log byte offset does not fit u64")
                })?)
                .ok_or_else(|| io::Error::other("Hearthstone log byte offset overflow"))?,
            line_number,
        };
        Ok(Some((byte_offset, line_number)))
    }

    fn materialize_line(&self, byte_offset: u64, line_number: u64) -> RawLogLine {
        let untrimmed = self.raw.trim_end_matches(['\n', '\r']);
        let (timestamp_key, message) = split_timestamp(untrimmed);
        RawLogLine {
            component: self.component,
            line_number,
            byte_offset,
            timestamp_key,
            message: message.to_owned(),
            raw: untrimmed.to_owned(),
        }
    }

    /// Advances through complete records without allocating `RawLogLine`s or
    /// parsing timestamps. This is used for components that currently do not
    /// produce state events but still need a durable tail cursor.
    pub fn skip_complete_lines(&mut self) -> io::Result<()> {
        while self.read_next_complete_record()?.is_some() {}
        Ok(())
    }

    /// Fast cursor advance for a component that currently emits no parser
    /// events. It seeks backward only through the final buffer-sized chunks to
    /// find the last newline, retaining a partial EOF line for a future append
    /// but avoiding a full historical line scan of Zone/Asset on every attach.
    ///
    /// `line_number` is intentionally reset: callers use this only for
    /// inert components, so no raw event or ordering key is ever emitted from
    /// the cursor. If that component gains parser grammar in the future it
    /// becomes event-producing and a fresh attach replays it normally.
    pub fn fast_forward_inert_component(&mut self) -> io::Result<()> {
        const SEARCH_CHUNK_BYTES: usize = 64 * 1024;

        let file_len = self.reader.get_ref().metadata()?.len();
        if file_len == 0 {
            self.reader.seek(SeekFrom::Start(0))?;
            self.cursor = LogCursor::default();
            return Ok(());
        }

        let mut search_end = file_len;
        let mut buffer = vec![0_u8; SEARCH_CHUNK_BYTES];
        let cursor = loop {
            let start = search_end.saturating_sub(SEARCH_CHUNK_BYTES as u64);
            let length = usize::try_from(search_end - start)
                .map_err(|_| io::Error::other("inert log chunk length does not fit usize"))?;
            self.reader.seek(SeekFrom::Start(start))?;
            self.reader.read_exact(&mut buffer[..length])?;
            if let Some(index) = buffer[..length].iter().rposition(|byte| *byte == b'\n') {
                break start
                    .checked_add(u64::try_from(index + 1).map_err(|_| {
                        io::Error::other("inert log newline index does not fit u64")
                    })?)
                    .ok_or_else(|| io::Error::other("inert log cursor overflow"))?;
            }
            if start == 0 {
                break 0;
            }
            search_end = start;
        };
        self.reader.seek(SeekFrom::Start(cursor))?;
        self.cursor = LogCursor {
            byte_offset: cursor,
            line_number: 0,
        };
        Ok(())
    }

    /// Returns the durable position immediately after the last complete line.
    pub const fn cursor(&self) -> LogCursor {
        self.cursor
    }
}

/// Cheap conservative pre-filter for the current parser grammar.
///
/// Returning `true` only means a record is worth timestamp parsing; the real
/// parser still validates syntax. Returning `false` means the record cannot
/// currently change reducer state. Update this with
/// [`HearthstoneLogParser::parse_line`] whenever a new event grammar is added.
fn component_might_emit_event(component: LogComponent, raw: &str) -> bool {
    match component {
        LogComponent::Arena => {
            raw.contains("Draft Deck ID:")
                || raw.contains("Draft deck contains card")
                || raw.contains("Client chooses:")
                || raw.contains("OnBegin")
                || raw.contains("OnRedraftBegin")
                || raw.contains("OnChosen()")
                || raw.contains("SetDraftMode")
        }
        LogComponent::LoadingScreen => {
            raw.contains("ARENA")
                || raw.contains("HUB -> GAMEPLAY")
                || raw.contains("GAMEPLAY -> HUB")
        }
        LogComponent::Power => {
            raw.contains("CREATE_GAME") || (raw.contains("PLAYSTATE") && raw.contains("WON"))
        }
        LogComponent::Zone | LogComponent::Asset => false,
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedEvent {
    pub component: LogComponent,
    pub line_number: u64,
    pub byte_offset: u64,
    pub timestamp_key: Option<u64>,
    pub event: GameEvent,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseReport {
    pub raw_line_count: usize,
    pub events: Vec<ParsedEvent>,
}

pub fn parse_session(session_dir: impl AsRef<Path>) -> io::Result<ParseReport> {
    let session_dir = session_dir.as_ref();
    let mut lines = Vec::new();
    for component in REQUIRED_COMPONENTS {
        let path = session_dir.join(component.filename());
        if path.is_file() {
            lines.extend(read_component_file(component, path)?);
        }
    }
    lines.sort_by(compare_raw_lines);

    let raw_line_count = lines.len();
    let mut parser = HearthstoneLogParser::default();
    let mut events = Vec::new();
    for line in lines {
        events.extend(
            parser
                .parse_line(&line)
                .into_iter()
                .map(|event| ParsedEvent {
                    component: line.component,
                    line_number: line.line_number,
                    byte_offset: line.byte_offset,
                    timestamp_key: line.timestamp_key,
                    event,
                }),
        );
    }
    Ok(ParseReport {
        raw_line_count,
        events,
    })
}

pub fn read_component_file(
    component: LogComponent,
    path: impl AsRef<Path>,
) -> io::Result<Vec<RawLogLine>> {
    Ok(read_component_file_from(component, path, LogCursor::default())?.0)
}

/// Reads only complete lines at or after a durable byte cursor. The returned
/// cursor deliberately leaves an unterminated final line unread so a later
/// append cannot be split into two malformed events.
pub fn read_component_file_from(
    component: LogComponent,
    path: impl AsRef<Path>,
    cursor: LogCursor,
) -> io::Result<(Vec<RawLogLine>, LogCursor)> {
    let mut reader = ComponentLogReader::open(component, path, cursor)?;
    let mut lines = Vec::new();
    while let Some(line) = reader.next_complete_line()? {
        lines.push(line);
    }
    Ok((lines, reader.cursor()))
}

fn compare_raw_lines(left: &RawLogLine, right: &RawLogLine) -> Ordering {
    left.timestamp_key
        .cmp(&right.timestamp_key)
        .then_with(|| left.component.cmp(&right.component))
        .then_with(|| left.line_number.cmp(&right.line_number))
        .then_with(|| left.byte_offset.cmp(&right.byte_offset))
}

/// Return the deterministic timestamp ordering key used by the incremental
/// observer. This is also used to validate a persisted checkpoint's final
/// source witness without rescanning an entire log file.
pub fn timestamp_key(raw: &str) -> Option<u64> {
    split_timestamp(raw).0
}

fn split_timestamp(raw: &str) -> (Option<u64>, &str) {
    static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:[A-Z]\s+)?(?P<hour>\d{1,2}):(?P<minute>\d{2}):(?P<second>\d{2})\.(?P<fraction>\d+)\s+(?P<message>.*)$")
            .expect("valid timestamp regex")
    });
    let Some(captures) = TIMESTAMP.captures(raw) else {
        return (None, raw);
    };
    let fraction = captures
        .name("fraction")
        .map(|capture| capture.as_str())
        .unwrap_or("0");
    let normalized_fraction = format!("{fraction:0<7}");
    let key = format!(
        "{:02}{:02}{:02}{}",
        captures["hour"].parse::<u8>().unwrap_or_default(),
        captures["minute"].parse::<u8>().unwrap_or_default(),
        captures["second"].parse::<u8>().unwrap_or_default(),
        &normalized_fraction[..7]
    )
    .parse::<u64>()
    .ok();
    (
        key,
        captures
            .name("message")
            .map(|capture| capture.as_str())
            .unwrap_or(raw),
    )
}

// Schema 6 persists the explicit OnRedraftBegin boundary. Current clients
// repeat the retained-card snapshot after that marker without a closing mode
// line; remembering the boundary lets subsequent choices remain observable.
pub const PARSER_CHECKPOINT_SCHEMA_VERSION: u32 = 6;

/// The small amount of parser state that affects the meaning of a later log
/// line. For example, Arena deck-list lines are meaningful only after the
/// parser has seen a matching deck-snapshot start line. It is therefore part
/// of a warm-restart checkpoint rather than something callers may discard.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParserCheckpoint {
    pub schema_version: u32,
    reading_arena_deck: bool,
    arena_picks_enabled: bool,
    redraft_boundary_pending: bool,
}

#[derive(Default)]
pub struct HearthstoneLogParser {
    reading_arena_deck: bool,
    // `Client chooses` is ambiguous on hero and Legendary Group screens: a
    // preview click and a committed choice have the same log grammar. It is
    // safe only after a caller has established an authoritative visual deck
    // baseline and explicitly opened this gate for subsequent normal picks.
    arena_picks_enabled: bool,
    redraft_boundary_pending: bool,
}

impl HearthstoneLogParser {
    pub fn checkpoint(&self) -> ParserCheckpoint {
        ParserCheckpoint {
            schema_version: PARSER_CHECKPOINT_SCHEMA_VERSION,
            reading_arena_deck: self.reading_arena_deck,
            arena_picks_enabled: self.arena_picks_enabled,
            redraft_boundary_pending: self.redraft_boundary_pending,
        }
    }

    /// Recreate a parser from a checkpoint only when that checkpoint was
    /// written by this parser schema. Callers must still validate the log
    /// sources/cursors before trusting the surrounding observer state.
    pub fn from_checkpoint(checkpoint: ParserCheckpoint) -> Result<Self, String> {
        if checkpoint.schema_version != PARSER_CHECKPOINT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported parser checkpoint schema {}; expected {}",
                checkpoint.schema_version, PARSER_CHECKPOINT_SCHEMA_VERSION
            ));
        }
        Ok(Self {
            reading_arena_deck: checkpoint.reading_arena_deck,
            arena_picks_enabled: checkpoint.arena_picks_enabled,
            redraft_boundary_pending: checkpoint.redraft_boundary_pending,
        })
    }

    /// Allow subsequent real-card `Client chooses` records to become Arena
    /// picks after the caller has reconciled an authoritative visual sidebar
    /// baseline. The gate is persisted in [`ParserCheckpoint`] and is closed
    /// automatically at every observed Arena run/snapshot boundary.
    pub fn enable_arena_picks_after_authoritative_baseline(&mut self) {
        self.arena_picks_enabled = true;
    }

    /// Close the `Client chooses` gate, for example when the visual baseline
    /// becomes stale before a new run marker reaches the log.
    pub fn disable_arena_picks(&mut self) {
        self.arena_picks_enabled = false;
    }

    pub const fn arena_picks_enabled(&self) -> bool {
        self.arena_picks_enabled
    }

    pub fn parse_line(&mut self, line: &RawLogLine) -> Vec<GameEvent> {
        match line.component {
            LogComponent::Arena => self.parse_arena_line(&line.message),
            LogComponent::LoadingScreen => self.parse_loading_screen_line(&line.message),
            LogComponent::Power => self.parse_power_line(&line.message),
            LogComponent::Zone | LogComponent::Asset => Vec::new(),
        }
    }

    fn parse_arena_line(&mut self, message: &str) -> Vec<GameEvent> {
        static DRAFT_DECK: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"Draft Deck ID:\s*(?P<id>\d+)(?:,\s*Hero Card\s*=\s*(?P<hero>HERO_[A-Z0-9_]+))?",
            )
            .expect("valid arena deck regex")
        });
        static DRAFT_CARD: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"Draft deck contains card\s+(?P<card>[A-Z0-9_]+)")
                .expect("valid draft card regex")
        });
        static ON_BEGIN: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"OnBegin\s*-\s*Got new draft deck with ID:\s*(?P<id>\d+)")
                .expect("valid begin regex")
        });
        static ON_REDRAFT_BEGIN: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"OnRedraftBegin\s*-\s*Got new redraft deck with ID:\s*\d+")
                .expect("valid redraft begin regex")
        });
        static ON_CHOSEN: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"OnChosen\(\):\s*hero=(?P<hero>HERO_[A-Z0-9_]+)")
                .expect("valid chosen regex")
        });
        static DRAFT_MODE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"SetDraftMode\s*-\s*(?P<mode>[A-Z_]+)").expect("valid mode regex")
        });
        static CLIENT_CHOICE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"Client chooses:\s*.*\((?P<card>[A-Z0-9_]+)\)\s*$")
                .expect("valid client choice regex")
        });

        if let Some(captures) = DRAFT_DECK.captures(message) {
            if self.redraft_boundary_pending {
                // Hearthstone repeats the retained-card snapshot after
                // OnRedraftBegin but does not close it with SetDraftMode.
                // The preceding snapshot is already authoritative.
                return Vec::new();
            }
            self.reading_arena_deck = true;
            self.arena_picks_enabled = false;
            return vec![GameEvent::ArenaDeckSnapshotStarted {
                draft_deck_id: captures["id"].to_owned(),
                hero_card_id: captures
                    .name("hero")
                    .map(|capture| capture.as_str().to_owned()),
            }];
        }
        if let Some(captures) = ON_BEGIN.captures(message) {
            self.reading_arena_deck = false;
            self.arena_picks_enabled = false;
            self.redraft_boundary_pending = false;
            return vec![GameEvent::ArenaRunStarted {
                draft_deck_id: captures["id"].to_owned(),
            }];
        }
        if ON_REDRAFT_BEGIN.is_match(message) {
            self.reading_arena_deck = false;
            self.arena_picks_enabled = true;
            self.redraft_boundary_pending = true;
            return Vec::new();
        }
        if let Some(captures) = ON_CHOSEN.captures(message) {
            return vec![GameEvent::HeroCard {
                card_id: captures["hero"].to_owned(),
            }];
        }
        if let Some(captures) = DRAFT_CARD.captures(message) {
            if self.redraft_boundary_pending {
                return Vec::new();
            }
            return self
                .reading_arena_deck
                .then(|| GameEvent::ArenaDeckSnapshotCard {
                    card_id: captures["card"].to_owned(),
                })
                .into_iter()
                .collect();
        }
        if let Some(captures) = CLIENT_CHOICE.captures(message) {
            let card_id = &captures["card"];
            if self.redraft_boundary_pending {
                self.redraft_boundary_pending = false;
                self.arena_picks_enabled = true;
            }
            return (self.arena_picks_enabled && is_real_card_id(card_id))
                .then(|| GameEvent::ArenaPick {
                    card_id: card_id.to_owned(),
                })
                .into_iter()
                .collect();
        }
        if let Some(captures) = DRAFT_MODE.captures(message) {
            let mode = captures["mode"].to_owned();
            if self.redraft_boundary_pending && mode == "REDRAFTING" {
                self.redraft_boundary_pending = false;
                self.arena_picks_enabled = true;
            }
            let mut events = Vec::new();
            if self.reading_arena_deck
                && matches!(
                    mode.as_str(),
                    "DRAFTING" | "ACTIVE_DRAFT_DECK" | "REDRAFTING"
                )
            {
                self.reading_arena_deck = false;
                events.push(GameEvent::ArenaDeckSnapshotCompleted);
                // The deck snapshot and following DRAFTING/REDRAFTING marker
                // prove that subsequent real-card choices are actual picks,
                // not unconfirmed hero/package previews.
                if matches!(mode.as_str(), "DRAFTING" | "REDRAFTING") {
                    self.arena_picks_enabled = true;
                }
            }
            events.push(GameEvent::ArenaDraftMode { mode });
            return events;
        }
        Vec::new()
    }

    fn parse_loading_screen_line(&mut self, message: &str) -> Vec<GameEvent> {
        let upper = message.to_ascii_uppercase();
        let mut events = Vec::new();
        if upper.contains("ARENA") {
            events.push(GameEvent::GameMode {
                raw_mode: "ARENA".into(),
            });
        }
        if upper.contains("HUB -> GAMEPLAY") {
            events.push(GameEvent::GameStarted);
        } else if upper.contains("GAMEPLAY -> HUB") {
            events.push(GameEvent::GameEnded {
                result: hs_state::GameResult::Unknown,
            });
        }
        events
    }

    fn parse_power_line(&mut self, message: &str) -> Vec<GameEvent> {
        let upper = message.to_ascii_uppercase();
        if upper.contains("CREATE_GAME") {
            return vec![GameEvent::GameStarted];
        }
        if upper.contains("PLAYSTATE") && upper.contains("WON") {
            return vec![GameEvent::GameEnded {
                result: hs_state::GameResult::Unknown,
            }];
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn parses_current_arena_snapshot_grammar_and_retains_duplicate_lines() {
        let mut parser = HearthstoneLogParser::default();
        let source = [
            "D 19:14:43.0000000 DraftManager.OnChoicesAndContents - Draft Deck ID: 507495951, Hero Card = HERO_08",
            "D 19:14:43.1000000 DraftManager.OnChoicesAndContents - Draft deck contains card REV_840",
            "D 19:14:43.2000000 DraftManager.OnChoicesAndContents - Draft deck contains card REV_840",
            "D 19:14:43.3000000 SetDraftMode - ACTIVE_DRAFT_DECK",
        ];
        let events = source
            .iter()
            .enumerate()
            .flat_map(|(index, raw)| {
                let (_, message) = split_timestamp(raw);
                parser.parse_line(&RawLogLine {
                    component: LogComponent::Arena,
                    line_number: index as u64 + 1,
                    byte_offset: 0,
                    timestamp_key: None,
                    message: message.into(),
                    raw: (*raw).into(),
                })
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            events[0],
            GameEvent::ArenaDeckSnapshotStarted { .. }
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, GameEvent::ArenaDeckSnapshotCard { .. }))
                .count(),
            2
        );
        assert!(matches!(
            events.last(),
            Some(GameEvent::ArenaDraftMode { .. })
        ));
    }

    #[test]
    fn parses_the_current_redraft_lifecycle_marker() {
        let mut parser = HearthstoneLogParser::default();
        let (_, message) = split_timestamp("D 00:20:04.8512350 SetDraftMode - REDRAFTING");
        let events = parser.parse_line(&RawLogLine {
            component: LogComponent::Arena,
            line_number: 1,
            byte_offset: 0,
            timestamp_key: None,
            message: message.into(),
            raw: "D 00:20:04.8512350 SetDraftMode - REDRAFTING".into(),
        });

        assert_eq!(
            events,
            vec![GameEvent::ArenaDraftMode {
                mode: "REDRAFTING".into()
            }]
        );
    }

    #[test]
    fn client_choice_preview_is_not_a_committed_arena_pick() {
        let mut parser = HearthstoneLogParser::default();
        for raw in [
            "D 23:39:43.7648410 Client chooses: Valeera Sanguinar (HERO_03)",
            "D 23:44:31.5659390 Client chooses: Morchie (END_036)",
        ] {
            let (_, message) = split_timestamp(raw);
            let events = parser.parse_line(&RawLogLine {
                component: LogComponent::Arena,
                line_number: 1,
                byte_offset: 0,
                timestamp_key: None,
                message: message.into(),
                raw: raw.into(),
            });
            assert!(events.is_empty());
        }
    }

    #[test]
    fn client_choices_become_picks_only_after_an_authoritative_baseline() {
        let mut parser = HearthstoneLogParser::default();
        let choice = arena_line("Client chooses: Whelp of the Infinite (TIME_045)");

        assert!(parser.parse_line(&choice).is_empty());
        parser.enable_arena_picks_after_authoritative_baseline();
        assert_eq!(
            parser.parse_line(&choice),
            vec![GameEvent::ArenaPick {
                card_id: "TIME_045".into(),
            }]
        );
    }

    #[test]
    fn hero_client_choices_never_become_arena_picks_even_when_enabled() {
        let mut parser = HearthstoneLogParser::default();
        parser.enable_arena_picks_after_authoritative_baseline();

        assert!(
            parser
                .parse_line(&arena_line("Client chooses: Valeera Sanguinar (HERO_03)"))
                .is_empty()
        );
        assert!(parser.arena_picks_enabled());
    }

    #[test]
    fn a_new_run_or_deck_snapshot_closes_the_client_choice_gate() {
        for boundary in [
            "DraftManager.OnBegin - Got new draft deck with ID: 99",
            "DraftManager.OnChoicesAndContents - Draft Deck ID: 99, Hero Card = HERO_03",
        ] {
            let mut parser = HearthstoneLogParser::default();
            parser.enable_arena_picks_after_authoritative_baseline();
            assert!(!parser.parse_line(&arena_line(boundary)).is_empty());
            assert!(!parser.arena_picks_enabled());
            assert!(
                parser
                    .parse_line(&arena_line("Client chooses: Fireball (CS2_029)"))
                    .is_empty()
            );
        }
    }

    #[test]
    fn parser_checkpoint_persists_the_explicit_client_choice_gate() {
        let mut parser = HearthstoneLogParser::default();
        parser.enable_arena_picks_after_authoritative_baseline();
        let checkpoint = parser.checkpoint();
        assert_eq!(checkpoint.schema_version, 6);

        let mut restored = HearthstoneLogParser::from_checkpoint(checkpoint).unwrap();
        assert!(restored.arena_picks_enabled());
        assert_eq!(
            restored.parse_line(&arena_line("Client chooses: Fireball (CS2_029)")),
            vec![GameEvent::ArenaPick {
                card_id: "CS2_029".into(),
            }]
        );

        let stale = ParserCheckpoint {
            schema_version: 2,
            reading_arena_deck: false,
            arena_picks_enabled: true,
            redraft_boundary_pending: false,
        };
        assert!(HearthstoneLogParser::from_checkpoint(stale).is_err());
    }

    fn arena_line(message: &str) -> RawLogLine {
        RawLogLine {
            component: LogComponent::Arena,
            line_number: 1,
            byte_offset: 0,
            timestamp_key: None,
            message: message.into(),
            raw: message.into(),
        }
    }

    #[test]
    fn reads_and_orders_a_session() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!("arena-next-parser-{suffix}"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("Arena.log"),
            "D 19:14:43.0000000 DraftManager.OnChosen(): hero=HERO_08\nD 19:14:44.0000000 Client chooses: Fireball (CS2_029)\n",
        )
        .unwrap();
        let report = parse_session(&directory).unwrap();
        assert_eq!(report.raw_line_count, 2);
        assert_eq!(report.events.len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn streaming_reader_rewinds_an_unterminated_tail() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!("arena-next-stream-reader-{suffix}"));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("Arena.log");
        fs::write(
            &path,
            "D 19:14:43.0000000 Client chooses: Fireball (CS2_029)\npartial",
        )
        .unwrap();

        let mut reader =
            ComponentLogReader::open(LogComponent::Arena, &path, LogCursor::default()).unwrap();
        assert!(reader.next_complete_line().unwrap().is_some());
        let cursor_after_complete_line = reader.cursor();
        assert!(reader.next_complete_line().unwrap().is_none());
        assert_eq!(reader.cursor(), cursor_after_complete_line);

        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b" tail\n").unwrap();
        let trailing = reader.next_complete_line().unwrap().unwrap();
        assert_eq!(trailing.raw, "partial tail");
        assert!(reader.next_complete_line().unwrap().is_none());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn inert_cursor_fast_forward_retains_only_an_unterminated_tail() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!("arena-next-inert-reader-{suffix}"));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("Zone.log");
        fs::write(&path, "old one\nold two\npartial").unwrap();

        let mut reader =
            ComponentLogReader::open(LogComponent::Zone, &path, LogCursor::default()).unwrap();
        reader.fast_forward_inert_component().unwrap();
        assert_eq!(
            reader.cursor().byte_offset,
            "old one\nold two\n".len() as u64
        );
        assert_eq!(reader.cursor().line_number, 0);

        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b" tail\n").unwrap();
        let trailing = reader.next_complete_line().unwrap().unwrap();
        assert_eq!(trailing.raw, "partial tail");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bounded_tail_uses_only_complete_lines_and_real_byte_offsets() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!("arena-next-tail-reader-{suffix}"));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("Power.log");
        fs::write(
            &path,
            "D 00:00:00.0000000 old\nD 00:00:01.0000000 keep\npartial",
        )
        .unwrap();

        // The bounded window begins inside the first line, so `old` is
        // omitted rather than fabricated from a partial prefix. The final
        // unterminated record remains outside the durable cursor.
        let tail = read_component_tail(LogComponent::Power, &path, 35)
            .unwrap()
            .unwrap();
        assert!(!tail.covers_file_start);
        assert_eq!(tail.lines.len(), 1);
        assert_eq!(tail.lines[0].message, "keep");
        assert_eq!(
            tail.cursor.byte_offset,
            "D 00:00:00.0000000 old\nD 00:00:01.0000000 keep\n".len() as u64
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reverse_marker_search_finds_newest_line_without_parsing_the_prefix() {
        use std::io::Write;

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!("arena-next-reverse-search-{suffix}"));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("Arena.log");
        let prefix = format!(
            "D 00:00:00.0000000 inert {}\n",
            "x".repeat(REVERSE_SEARCH_CHUNK_BYTES)
        );
        let newest = "D 00:00:01.0000000 Draft Deck ID: 42, Hero Card = HERO_08\n";
        fs::write(&path, format!("{prefix}{newest}partial")).unwrap();

        let marker = find_last_line_containing(&path, b"Draft Deck ID:")
            .unwrap()
            .expect("newest marker should be found");
        assert_eq!(marker.line_start, prefix.len() as u64);
        assert!(marker.complete);
        assert_eq!(
            tail_cursor(&path).unwrap().byte_offset,
            (prefix.len() + newest.len()) as u64
        );

        // A newer unterminated marker wins over the older complete one, so a
        // caller can wait rather than hydrating stale deck state.
        let partial_marker = "D 00:00:02.0000000 Draft Deck ID:";
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(partial_marker.as_bytes()).unwrap();
        let newest_marker = find_last_line_containing(&path, b"Draft Deck ID:")
            .unwrap()
            .expect("partial newest marker should be visible");
        assert!(!newest_marker.complete);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn only_event_components_need_timestamp_parsing_during_streaming_replay() {
        assert!(component_can_emit_events(LogComponent::Arena));
        assert!(component_can_emit_events(LogComponent::LoadingScreen));
        assert!(component_can_emit_events(LogComponent::Power));
        assert!(!component_can_emit_events(LogComponent::Asset));
        assert!(!component_can_emit_events(LogComponent::Zone));
    }
}
