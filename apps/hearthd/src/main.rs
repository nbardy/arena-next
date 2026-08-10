#![deny(unsafe_op_in_unsafe_fn)]

//! `hearthd`: the long-running, UI-independent Arena state daemon.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::{
    fs::{FileTypeExt, MetadataExt, PermissionsExt},
    net::UnixStream,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use hs_card_data::{CardCache, CardResolution, import_hearthstonejson};
use hs_log_config::{LoggingStatus, enable_file_logging, inspect as inspect_log_config};
use hs_log_parser::{
    HearthstoneLogParser, LogComponent, LogCursor, REQUIRED_COMPONENTS, RawLogLine, parse_session,
    read_component_file_from,
};
use hs_paths::{GamePaths, discover_macos};
use hs_state::{ArenaReducer, ArenaSnapshot, DeckCard};
use serde::Serialize;
use tokio::{io::AsyncWriteExt, net::UnixListener, sync::RwLock, time};
use tracing::{info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "hearthd",
    version,
    about = "ArenaNext Hearthstone state daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect macOS Hearthstone discovery and replay the current session when available.
    Inspect(InspectArgs),
    /// Replay a saved log session into one deterministic JSON snapshot.
    Replay(ReplayArgs),
    /// Inspect or explicitly enable the relevant Hearthstone logging sections.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage the refreshable local card metadata cache.
    Cards {
        #[command(subcommand)]
        command: CardsCommand,
    },
    /// Tail the current session and serve snapshots through a local Unix socket.
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
struct InspectArgs {
    /// Override the live session directory.
    #[arg(long)]
    logs: Option<PathBuf>,
    /// Versioned card-cache JSON used to resolve card names and costs.
    #[arg(long)]
    cards: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ReplayArgs {
    /// A session directory containing Arena.log and optionally the other component logs.
    #[arg(long)]
    logs: PathBuf,
    /// Versioned card-cache JSON used to resolve card names and costs.
    #[arg(long)]
    cards: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Status {
        /// Explicit log.config path; auto-discovered when omitted.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Enable {
        /// Explicit log.config path; auto-discovered when omitted.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Required acknowledgement before changing a Blizzard-owned file.
        #[arg(long)]
        write: bool,
    },
}

#[derive(Debug, Subcommand)]
enum CardsCommand {
    Status {
        /// Versioned card-cache JSON; defaults to ArenaNext's application-data cache.
        #[arg(long)]
        cards: Option<PathBuf>,
    },
    Refresh {
        /// Destination for the normalized cache.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Public HearthstoneJSON card endpoint. The server redirects `latest` to a build.
        #[arg(
            long,
            default_value = "https://api.hearthstonejson.com/v1/latest/enUS/cards.collectible.json"
        )]
        url: String,
    },
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Override the live session directory.
    #[arg(long)]
    logs: Option<PathBuf>,
    /// Versioned card-cache JSON used to resolve card names and costs.
    #[arg(long)]
    cards: Option<PathBuf>,
    /// Local Unix socket path for JSON snapshot requests.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Persisted snapshot file, useful for UI restarts and diagnostics.
    #[arg(long)]
    state_file: Option<PathBuf>,
    /// Poll interval. ArenaNext replays once at start, then tails only appended lines.
    #[arg(long, default_value_t = 500)]
    poll_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolvedDeckCard {
    card_id: String,
    count: u8,
    resolution: CardResolution,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolvedSnapshot {
    schema_version: u32,
    mode: hs_state::GameMode,
    hero_class: Option<hs_state::HeroClass>,
    deck: Vec<ResolvedDeckCard>,
    run: hs_state::ArenaRunState,
    draft: hs_state::DraftState,
    game: hs_state::GameState,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplayOutput {
    session: PathBuf,
    raw_line_count: usize,
    event_count: usize,
    card_cache: CardCacheStatus,
    snapshot: ResolvedSnapshot,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CardCacheStatus {
    source: String,
    data_version: String,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    card_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CardCacheOutput {
    path: PathBuf,
    exists: bool,
    cache: CardCacheStatus,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InspectOutput {
    paths: GamePaths,
    logging: LoggingStatus,
    replay: Option<ReplayOutput>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedState {
    session: PathBuf,
    cursors: BTreeMap<LogComponent, LogCursor>,
    snapshot: ResolvedSnapshot,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Inspect(args) => run_inspect(args),
        Command::Replay(args) => run_replay(args),
        Command::Config { command } => run_config(command),
        Command::Cards { command } => run_cards(command).await,
        Command::Serve(args) => run_serve(args).await,
    }
}

fn run_inspect(args: InspectArgs) -> Result<()> {
    let paths = discover_macos()?;
    let config_path = paths
        .log_config
        .clone()
        .context("could not determine a log.config location")?;
    let logging = inspect_log_config(config_path)?;
    let session = args.logs.or(paths.latest_session.clone());
    let cards = load_card_cache(args.cards.as_deref())?;
    let replay = session
        .as_deref()
        .map(|session| replay_session(session, &cards))
        .transpose()?;
    print_json(&InspectOutput {
        paths,
        logging,
        replay,
    })
}

fn run_replay(args: ReplayArgs) -> Result<()> {
    let cards = load_card_cache(args.cards.as_deref())?;
    print_json(&replay_session(&args.logs, &cards)?)
}

fn run_config(command: ConfigCommand) -> Result<()> {
    let path = match &command {
        ConfigCommand::Status { path } => path.clone(),
        ConfigCommand::Enable { path, .. } => path.clone(),
    }
    .or(discover_macos()?.log_config)
    .context("could not determine a log.config location")?;

    match command {
        ConfigCommand::Status { .. } => print_json(&inspect_log_config(path)?),
        ConfigCommand::Enable { write: false, .. } => {
            bail!(
                "refusing to modify {}; rerun with `config enable --write`",
                path.display()
            )
        }
        ConfigCommand::Enable { write: true, .. } => print_json(&enable_file_logging(path)?),
    }
}

async fn run_cards(command: CardsCommand) -> Result<()> {
    match command {
        CardsCommand::Status { cards } => {
            let path = cards.unwrap_or_else(default_card_cache_path);
            let exists = path.is_file();
            let cache = if exists {
                CardCache::load(&path)?
            } else {
                CardCache::empty()
            };
            print_json(&CardCacheOutput {
                path,
                exists,
                cache: card_cache_status(&cache),
            })
        }
        CardsCommand::Refresh { output, url } => {
            let path = output.unwrap_or_else(default_card_cache_path);
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(45))
                .build()
                .context("could not initialize the card-data HTTP client")?;
            let response = client
                .get(&url)
                .send()
                .await
                .with_context(|| format!("could not fetch {url}"))?
                .error_for_status()
                .with_context(|| format!("card source rejected request for {url}"))?;
            let final_url = response.url().to_string();
            let source_version = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim_matches('"').to_owned())
                .unwrap_or_else(|| {
                    final_url
                        .split("/v1/")
                        .nth(1)
                        .and_then(|path| path.split('/').next())
                        .unwrap_or("latest")
                        .to_owned()
                });
            let body = response
                .text()
                .await
                .context("could not read card-data response")?;
            let cache = import_hearthstonejson(&body, final_url, source_version)?;
            cache.write_file(&path)?;
            print_json(&CardCacheOutput {
                path,
                exists: true,
                cache: card_cache_status(&cache),
            })
        }
    }
}

fn replay_session(session: &Path, cards: &CardCache) -> Result<ReplayOutput> {
    let report = parse_session(session)
        .with_context(|| format!("could not parse Hearthstone session {}", session.display()))?;
    let mut reducer = ArenaReducer::new();
    reducer.apply_all(report.events.iter().map(|event| event.event.clone()));
    Ok(ReplayOutput {
        session: session.to_path_buf(),
        raw_line_count: report.raw_line_count,
        event_count: report.events.len(),
        card_cache: card_cache_status(cards),
        snapshot: resolve_snapshot(reducer.into_snapshot(), cards),
    })
}

fn load_card_cache(path: Option<&Path>) -> Result<CardCache> {
    let path = path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_card_cache_path);
    if path.is_file() {
        CardCache::load(path)
    } else {
        Ok(CardCache::empty())
    }
}

fn card_cache_status(cards: &CardCache) -> CardCacheStatus {
    CardCacheStatus {
        source: cards.source.clone(),
        data_version: cards.data_version.clone(),
        updated_at: cards.updated_at,
        card_count: cards.len(),
    }
}

fn resolve_snapshot(snapshot: ArenaSnapshot, cards: &CardCache) -> ResolvedSnapshot {
    ResolvedSnapshot {
        schema_version: snapshot.schema_version,
        mode: snapshot.mode,
        hero_class: snapshot.hero_class,
        deck: snapshot
            .deck
            .into_iter()
            .map(|DeckCard { card_id, count }| ResolvedDeckCard {
                resolution: cards.resolve(&card_id),
                card_id,
                count,
            })
            .collect(),
        run: snapshot.run,
        draft: snapshot.draft,
        game: snapshot.game,
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

struct SessionTailer {
    session: PathBuf,
    parser: HearthstoneLogParser,
    reducer: ArenaReducer,
    cursors: BTreeMap<LogComponent, LogCursor>,
    file_identities: BTreeMap<LogComponent, FileIdentity>,
    checkpoints: BTreeMap<LogComponent, FileCheckpoint>,
    last_line_order: Option<RawLineOrder>,
}

impl SessionTailer {
    fn attach(session: PathBuf) -> Result<Self> {
        validate_log_session(&session)?;
        let mut cursors = BTreeMap::new();
        let mut file_identities = BTreeMap::new();
        let mut checkpoints = BTreeMap::new();
        let mut lines = Vec::new();
        for component in REQUIRED_COMPONENTS {
            let path = session.join(component.filename());
            if path.is_file() {
                let identity_before = file_identity(&path)?;
                let (component_lines, cursor) =
                    read_component_file_from(component, &path, LogCursor::default())?;
                let identity_after = file_identity(&path)?;
                if identity_before != identity_after {
                    bail!(
                        "{} changed while ArenaNext was attaching; retrying is safer",
                        path.display()
                    );
                }
                cursors.insert(component, cursor);
                file_identities.insert(component, identity_after);
                if let Some(checkpoint) = file_checkpoint(&path, cursor.byte_offset)? {
                    checkpoints.insert(component, checkpoint);
                }
                lines.extend(component_lines);
            }
        }
        if cursors.is_empty() {
            bail!(
                "{} contains no readable Hearthstone component logs",
                session.display()
            );
        }
        sort_lines(&mut lines);
        let last_line_order = lines.last().map(raw_line_order);
        let mut parser = HearthstoneLogParser::default();
        let mut reducer = ArenaReducer::new();
        for line in lines {
            reducer.apply_all(parser.parse_line(&line));
        }
        Ok(Self {
            session,
            parser,
            reducer,
            cursors,
            file_identities,
            checkpoints,
            last_line_order,
        })
    }

    fn poll(&mut self) -> Result<bool> {
        let mut lines = Vec::new();
        for component in REQUIRED_COMPONENTS {
            let path = self.session.join(component.filename());
            if !path.is_file() {
                if self.file_identities.contains_key(&component) {
                    bail!("{} disappeared or rotated", path.display());
                }
                continue;
            }
            let identity_before = file_identity(&path)?;
            match self.file_identities.get(&component) {
                Some(expected) if expected != &identity_before => {
                    bail!("{} was replaced or rotated", path.display());
                }
                Some(_) => {}
                None => {
                    // A component that appeared after attachment can contain
                    // records earlier than lines already reduced from another
                    // file. Full replay is the only deterministic ordering.
                    bail!(
                        "{} appeared after attachment; replaying the session",
                        path.display()
                    );
                }
            }
            let cursor = self.cursors.get(&component).copied().unwrap_or_default();
            let file_len = fs::metadata(&path)?.len();
            if file_len < cursor.byte_offset {
                bail!("{} was truncated or rotated", path.display());
            }
            if let Some(checkpoint) = self.checkpoints.get(&component) {
                if !checkpoint_matches(&path, checkpoint)? {
                    bail!(
                        "{} changed before its saved cursor; replaying the session",
                        path.display()
                    );
                }
            }
            let (component_lines, next_cursor) =
                read_component_file_from(component, &path, cursor)?;
            let identity_after = file_identity(&path)?;
            if identity_before != identity_after {
                bail!(
                    "{} changed while ArenaNext was reading it; replaying the session",
                    path.display()
                );
            }
            if let Some(checkpoint) = self.checkpoints.get(&component) {
                if !checkpoint_matches(&path, checkpoint)? {
                    bail!(
                        "{} changed while ArenaNext was reading it; replaying the session",
                        path.display()
                    );
                }
            }
            self.cursors.insert(component, next_cursor);
            if let Some(checkpoint) = file_checkpoint(&path, next_cursor.byte_offset)? {
                self.checkpoints.insert(component, checkpoint);
            } else {
                self.checkpoints.remove(&component);
            }
            lines.extend(component_lines);
        }
        if lines.is_empty() {
            return Ok(false);
        }
        sort_lines(&mut lines);
        if self
            .last_line_order
            .is_some_and(|last| raw_line_order(&lines[0]) < last)
        {
            bail!(
                "a newly appended log record sorts before an already reduced record; replaying the session"
            );
        }
        self.last_line_order = lines.last().map(raw_line_order);
        for line in lines {
            self.reducer.apply_all(self.parser.parse_line(&line));
        }
        Ok(true)
    }

    fn snapshot(&self, cards: &CardCache) -> ResolvedSnapshot {
        resolve_snapshot(self.reducer.snapshot().clone(), cards)
    }
}

fn sort_lines(lines: &mut [RawLogLine]) {
    lines.sort_by(|left, right| {
        left.timestamp_key
            .cmp(&right.timestamp_key)
            .then_with(|| left.component.cmp(&right.component))
            .then_with(|| left.line_number.cmp(&right.line_number))
            .then_with(|| left.byte_offset.cmp(&right.byte_offset))
    });
}

type RawLineOrder = (Option<u64>, LogComponent, u64, u64);

fn raw_line_order(line: &RawLogLine) -> RawLineOrder {
    (
        line.timestamp_key,
        line.component,
        line.line_number,
        line.byte_offset,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

/// The final bytes before each durable cursor catch copy-truncate rotation
/// where a file is replaced in place and happens to be at least as long as
/// the old cursor by the next poll. Inode checks alone cannot see that case.
#[derive(Clone, Debug, Eq, PartialEq)]
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
        // A shortening race is handled as a normal replay condition instead
        // of propagating an opaque EOF error through the daemon loop.
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("could not verify cursor for {}", path.display()))
        }
    }
}

fn validate_log_session(session: &Path) -> Result<()> {
    if !session.is_dir() {
        bail!(
            "{} is not a Hearthstone log-session directory",
            session.display()
        );
    }
    if !REQUIRED_COMPONENTS
        .iter()
        .any(|component| session.join(component.filename()).is_file())
    {
        bail!(
            "{} does not contain any supported Hearthstone component logs",
            session.display()
        );
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

async fn run_serve(args: ServeArgs) -> Result<()> {
    // An explicit session is useful for fixture replay and must not require a
    // local Hearthstone installation or even a configured macOS home path.
    // Live discovery remains macOS-specific and only runs when the caller
    // actually asks us to choose a session.
    let dynamic_session = args.logs.is_none();
    let (session, log_root) = select_serve_session(args.logs.clone())?;
    let cards = load_card_cache(args.cards.as_deref())?;
    let uses_default_storage = args.socket.is_none() || args.state_file.is_none();
    let socket = args.socket.unwrap_or_else(default_socket_path);
    let state_file = args.state_file.unwrap_or_else(default_state_path);
    if uses_default_storage {
        ensure_private_app_data_dir()?;
    }
    let mut tailer = SessionTailer::attach(session.clone())?;
    let initial = tailer.snapshot(&cards);
    let shared_snapshot = Arc::new(RwLock::new(initial));
    let listener = bind_socket(&socket)?;
    let socket_identity = file_identity(&socket)?;
    publish_snapshot(&shared_snapshot, &state_file, &tailer, &cards).await;
    info!(socket = %socket.display(), session = %session.display(), "hearthd attached");

    let mut interval = time::interval(Duration::from_millis(args.poll_ms.max(100)));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                if dynamic_session {
                    if let Some(root) = log_root.as_deref() {
                        if let Some(next_session) = newest_live_session(root) {
                            if next_session != tailer.session
                                && session_activity_time(&next_session) > session_activity_time(&tailer.session)
                            {
                                match SessionTailer::attach(next_session.clone()) {
                                    Ok(next_tailer) => {
                                        info!(session = %next_session.display(), "switching to new Hearthstone session");
                                        tailer = next_tailer;
                                        // A fresh session might not append another line before the
                                        // next poll. Publish immediately so clients never keep the
                                        // previous run's snapshot indefinitely.
                                        publish_snapshot(&shared_snapshot, &state_file, &tailer, &cards).await;
                                    }
                                    Err(error) => {
                                        // A directory may appear before Hearthstone has finished
                                        // creating its files. Keep serving the known-good session
                                        // and retry on the next interval instead of exiting.
                                        warn!(
                                            session = %next_session.display(),
                                            error = %error,
                                            "new Hearthstone session is not stable enough to attach"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                match tailer.poll() {
                    Ok(true) => {
                        publish_snapshot(&shared_snapshot, &state_file, &tailer, &cards).await;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        warn!(error = %error, "tail cursor invalid; replaying current session");
                        let session = tailer.session.clone();
                        match SessionTailer::attach(session) {
                            Ok(replayed) => {
                                tailer = replayed;
                                publish_snapshot(&shared_snapshot, &state_file, &tailer, &cards).await;
                            }
                            Err(replay_error) => {
                                // Log rotation can briefly leave a session without a complete
                                // component set. Retain the last verified snapshot and retry
                                // rather than taking the observer down.
                                warn!(
                                    error = %replay_error,
                                    "could not replay current Hearthstone session yet; retaining last snapshot"
                                );
                            }
                        }
                    }
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((mut stream, _)) => {
                        let snapshot = shared_snapshot.read().await.clone();
                        tokio::spawn(async move {
                            if let Ok(payload) = serde_json::to_vec(&snapshot) {
                                let _ = time::timeout(Duration::from_secs(2), async {
                                    stream.write_all(&payload).await?;
                                    stream.write_all(b"\n").await
                                })
                                .await;
                            }
                        });
                    }
                    Err(error) => warn!(error = %error, "local snapshot socket accept failed"),
                }
            }
            _ = wait_for_shutdown() => {
                info!("hearthd shutting down");
                break;
            }
        }
    }
    drop(listener);
    remove_owned_socket(&socket, socket_identity);
    Ok(())
}

fn select_serve_session(explicit: Option<PathBuf>) -> Result<(PathBuf, Option<PathBuf>)> {
    match explicit {
        Some(session) => Ok((session, None)),
        None => {
            let discovered = discover_macos()?;
            let session = discovered
                .log_root
                .as_deref()
                .and_then(newest_live_session)
                .or(discovered.latest_session.clone())
                .context("no Hearthstone log session found; launch Hearthstone or pass --logs")?;
            Ok((session, discovered.log_root))
        }
    }
}

async fn publish_snapshot(
    shared_snapshot: &Arc<RwLock<ResolvedSnapshot>>,
    state_file: &Path,
    tailer: &SessionTailer,
    cards: &CardCache,
) {
    let snapshot = tailer.snapshot(cards);
    // The log session remains the source of truth. A full disk or a stale
    // custom state path must not stop live observation or make clients lose
    // the last verified in-memory snapshot.
    if let Err(error) = persist_state(state_file, tailer, snapshot.clone()) {
        warn!(
            path = %state_file.display(),
            error = %error,
            "could not persist hearthd diagnostic state"
        );
    }
    *shared_snapshot.write().await = snapshot;
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                warn!(error = %error, "could not install SIGTERM handler; waiting for Ctrl-C");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn persist_state(path: &Path, tailer: &SessionTailer, snapshot: ResolvedSnapshot) -> Result<()> {
    let parent = path.parent().context("state file has no parent")?;
    fs::create_dir_all(parent)?;
    let serialized = serde_json::to_vec_pretty(&PersistedState {
        session: tailer.session.clone(),
        cursors: tailer.cursors.clone(),
        snapshot,
    })?;
    atomic_write(path, &serialized)
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("path has no UTF-8 file name")?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for attempt in 0..16_u8 {
        let temporary = parent.join(format!(
            ".{file_name}.{}.{}.{}.tmp",
            std::process::id(),
            nonce,
            attempt
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
        let write_result = (|| -> io::Result<()> {
            file.write_all(contents)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| {
                format!(
                    "could not atomically replace {} from {}",
                    path.display(),
                    temporary.display()
                )
            });
        }
        return Ok(());
    }
    bail!(
        "could not reserve a unique temporary state file beside {}",
        path.display()
    )
}

fn bind_socket(path: &Path) -> Result<UnixListener> {
    let parent = path.parent().context("socket path has no parent")?;
    fs::create_dir_all(parent)?;
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            bail!("refusing to replace non-socket path {}", path.display());
        }
        match UnixStream::connect(path) {
            Ok(_) => bail!(
                "a live local service is already listening at {}; refusing to displace it",
                path.display()
            ),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                fs::remove_file(path)
                    .with_context(|| format!("could not remove stale socket {}", path.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "could not verify whether the existing socket {} is stale; refusing to replace it",
                        path.display()
                    )
                });
            }
        }
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("could not bind {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("could not restrict local socket {}", path.display()))?;
    Ok(listener)
}

fn remove_owned_socket(path: &Path, identity: FileIdentity) {
    let Ok(current_identity) = file_identity(path) else {
        return;
    };
    if current_identity != identity {
        return;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_socket() {
        let _ = fs::remove_file(path);
    }
}

fn default_socket_path() -> PathBuf {
    app_data_dir().join("hearthd.sock")
}

fn default_state_path() -> PathBuf {
    app_data_dir().join("state.json")
}

fn default_card_cache_path() -> PathBuf {
    app_data_dir().join("card-data.json")
}

fn ensure_private_app_data_dir() -> Result<PathBuf> {
    let path = app_data_dir();
    fs::create_dir_all(&path).with_context(|| {
        format!(
            "could not create ArenaNext data directory {}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "could not restrict ArenaNext data directory {} to this user",
            path.display()
        )
    })?;
    Ok(path)
}

fn app_data_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/Application Support/ArenaNext")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env, thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/logs/sample-arena-session")
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("arena-next-hearthd-{name}-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn complete_arena_line(message: &str) -> String {
        format!("D 19:14:43.0000000 {message}\n")
    }

    fn unique_socket_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // macOS Unix-domain paths are short; `$TMPDIR` commonly expands to a
        // long per-user path, so keep the test endpoint directly under /tmp.
        PathBuf::from("/tmp").join(format!("an-{name}-{}-{nonce}.sock", std::process::id()))
    }

    #[test]
    fn fixture_replay_retains_duplicate_arena_cards() {
        let cards = CardCache::load(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/card-data/sample-cards.json"),
        )
        .unwrap();
        let replay = replay_session(&fixture_path(), &cards).unwrap();
        let duplicate = replay
            .snapshot
            .deck
            .iter()
            .find(|card| card.card_id == "REV_840")
            .unwrap();
        assert_eq!(duplicate.count, 2);
    }

    #[test]
    fn fixture_replay_matches_the_sanitized_expected_state() {
        let report = parse_session(fixture_path()).unwrap();
        let mut reducer = ArenaReducer::new();
        reducer.apply_all(report.events.into_iter().map(|event| event.event));
        let expected: ArenaSnapshot = serde_json::from_str(
            &fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/expected-state/sample-arena-session.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(reducer.into_snapshot(), expected);
    }

    #[test]
    fn refuses_to_replace_an_ordinary_socket_path() {
        let directory = unique_temp_dir("ordinary-socket-path");
        let path = directory.join("hearthd.sock");
        fs::write(&path, "ordinary file").unwrap();
        assert!(bind_socket(&path).is_err());
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[tokio::test]
    async fn does_not_displace_a_live_snapshot_socket() {
        let path = unique_socket_path("live");
        let first = bind_socket(&path).unwrap();

        let error = bind_socket(&path).unwrap_err();
        assert!(error.to_string().contains("refusing to displace"));
        assert!(path.exists());

        drop(first);
        fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn safely_reuses_a_stale_snapshot_socket_with_private_permissions() {
        let path = unique_socket_path("stale");
        let first = bind_socket(&path).unwrap();
        drop(first);

        let second = bind_socket(&path).unwrap();
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let identity = file_identity(&path).unwrap();
        drop(second);
        remove_owned_socket(&path, identity);
        assert!(!path.exists());
    }

    #[test]
    fn tailer_replays_when_a_component_appears_after_attachment() {
        let directory = unique_temp_dir("late-component");
        fs::write(
            directory.join("Arena.log"),
            complete_arena_line("DraftManager.OnChosen(): hero=HERO_08"),
        )
        .unwrap();
        let mut tailer = SessionTailer::attach(directory.clone()).unwrap();
        fs::write(
            directory.join("LoadingScreen.log"),
            complete_arena_line("LoadingScreen.OnSceneLoaded - ARENA"),
        )
        .unwrap();

        assert!(tailer.poll().is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tailer_detects_copy_truncate_even_when_the_replacement_is_longer() {
        let directory = unique_temp_dir("copy-truncate");
        let arena_log = directory.join("Arena.log");
        fs::write(
            &arena_log,
            complete_arena_line("DraftManager.OnChosen(): hero=HERO_08"),
        )
        .unwrap();
        let mut tailer = SessionTailer::attach(directory.clone()).unwrap();

        // `fs::write` retains the inode but rewrites it in place. The new
        // content is deliberately longer than the saved byte cursor, so a
        // length-only tailer would incorrectly consume it as an append.
        fs::write(&arena_log, complete_arena_line(&"x".repeat(256))).unwrap();
        assert!(tailer.poll().is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tailer_keeps_an_unterminated_line_for_the_next_poll() {
        let directory = unique_temp_dir("partial-line");
        let arena_log = directory.join("Arena.log");
        let complete = complete_arena_line("DraftManager.OnChosen(): hero=HERO_08");
        let partial = "D 19:14:44.0000000 SetDraftMode - DRA";
        fs::write(&arena_log, format!("{complete}{partial}")).unwrap();
        let mut tailer = SessionTailer::attach(directory.clone()).unwrap();

        let mut append = OpenOptions::new().append(true).open(&arena_log).unwrap();
        append.write_all(b"FTING\n").unwrap();
        assert!(tailer.poll().unwrap());
        assert_eq!(
            tailer.reducer.snapshot().run.draft_mode.as_deref(),
            Some("DRAFTING")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn newer_partial_session_is_preferred_over_an_old_complete_session() {
        let root = unique_temp_dir("newest-session");
        let old = root.join("old");
        let current = root.join("current");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&current).unwrap();
        fs::write(
            old.join("Arena.log"),
            complete_arena_line("DraftManager.OnChosen(): hero=HERO_08"),
        )
        .unwrap();
        fs::write(old.join("Power.log"), complete_arena_line("CREATE_GAME")).unwrap();
        thread::sleep(Duration::from_millis(20));
        fs::write(
            current.join("LoadingScreen.log"),
            complete_arena_line("LoadingScreen.OnSceneLoaded - ARENA"),
        )
        .unwrap();

        assert_eq!(newest_live_session(&root), Some(current));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_serve_session_does_not_invoke_platform_discovery() {
        let directory = unique_temp_dir("explicit-session");
        let (session, log_root) = select_serve_session(Some(directory.clone())).unwrap();
        assert_eq!(session, directory);
        assert_eq!(log_root, None);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_state_write_replaces_only_the_requested_file() {
        let directory = unique_temp_dir("atomic-state");
        let path = directory.join("state.json");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .count(),
            1
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
