#![deny(unsafe_op_in_unsafe_fn)]

//! The lean native ArenaNext application.
//!
//! There is one local process: observer and AppKit overlay. Fixture replay,
//! diagnostics, and recovery checkpoints are built into this executable; it
//! does not need a second service or a browser runtime.

mod model;

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

#[cfg(target_os = "macos")]
use std::io::Write;

use anyhow::{Context, Result, bail};
#[cfg(target_os = "macos")]
use arena_analysis::{AnalysisFacts, AnalysisInput, analyze_deck, analyze_offer};
#[cfg(target_os = "macos")]
use arena_rules::{ArenaRulesManifest, ResolvedArenaRules};
#[cfg(target_os = "macos")]
use arena_scoring::RatingProvider;
use chrono::Utc;
use hs_card_data::{CardCache, CardResolution};
use hs_observer::{
    LOG_STALENESS_THRESHOLD, LiveObserver, LogStaleness, ObserverSnapshot,
    rotate_overlarge_component_logs,
};

#[derive(Clone, Debug, Default)]
struct Options {
    logs: Option<PathBuf>,
    cards: Option<PathBuf>,
    arena_rules: Option<PathBuf>,
    arena_mode: Option<String>,
    once: bool,
    demo: bool,
    inspect: bool,
    enable_logging: bool,
    capture_status: bool,
    request_screen_recording: bool,
    capture_window: bool,
    read_deck: bool,
    read_offer: bool,
    draft_fingerprints: Option<PathBuf>,
    ratings: Option<PathBuf>,
    import_heartharena: bool,
    import_hsreplay: bool,
    import_firestone: bool,
    firestone_format: Option<String>,
    ratings_output: Option<PathBuf>,
    doctor: bool,
    replay: bool,
    explain_card: Option<String>,
    analyze: bool,
    analysis_facts: Option<PathBuf>,
    analysis_offers: Vec<String>,
    json: bool,
    logging_command: Option<LoggingCommand>,
    logging_backup: Option<PathBuf>,
    logging_latest_backup: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoggingCommand {
    Inspect,
    Diff,
    Restore,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug)]
struct SharedObserverState {
    snapshot: ObserverSnapshot,
    last_error: Option<String>,
    checkpoint_error: Option<String>,
    external_draft: Option<model::ExternalDraftModel>,
    offer_overlay: Option<OfferOverlayState>,
    /// Newest required component log freshness of the followed session. The
    /// worker recomputes this after every attach and poll so a frozen writer
    /// set (see `hs_observer::LogStaleness`) is visible on the next overlay
    /// tick instead of an eternal "waiting" message.
    log_staleness: LogStaleness,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug)]
struct OfferOverlayState {
    badges: [OfferBadgeState; 3],
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug)]
struct OfferBadgeState {
    top_left_x: f64,
    top_left_y: f64,
    width: f64,
    height: f64,
    caption: String,
    detail: String,
    score: String,
    loading: bool,
}

fn main() -> Result<()> {
    #[cfg(target_os = "macos")]
    return run_macos(Options::parse()?);

    #[cfg(not(target_os = "macos"))]
    {
        let _ = Options::parse()?;
        bail!("ArenaNext's native overlay is currently implemented for macOS only")
    }
}

impl Options {
    fn parse() -> Result<Self> {
        Self::parse_from(env::args_os().skip(1))
    }

    fn parse_from(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self> {
        let mut options = Self::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.to_string_lossy().as_ref() {
                "--logs" => options.logs = Some(next_path(&mut arguments, "--logs")?),
                "--cards" => options.cards = Some(next_path(&mut arguments, "--cards")?),
                "--arena-rules" => {
                    options.arena_rules = Some(next_path(&mut arguments, "--arena-rules")?)
                }
                "--arena-mode" => {
                    options.arena_mode = Some(next_string(&mut arguments, "--arena-mode")?)
                }
                "--once" => options.once = true,
                "--demo" => options.demo = true,
                "--inspect" => options.inspect = true,
                "--enable-logging" => options.enable_logging = true,
                "--capture-status" => options.capture_status = true,
                "--request-screen-recording" => options.request_screen_recording = true,
                "--capture-window" => options.capture_window = true,
                "--read-deck" => options.read_deck = true,
                "--read-offer" => options.read_offer = true,
                "doctor" => options.doctor = true,
                "inspect" => options.inspect = true,
                "replay" => {
                    options.replay = true;
                    options.logs = Some(next_path(&mut arguments, "replay")?);
                }
                "explain-card" => {
                    options.explain_card = Some(next_string(&mut arguments, "explain-card")?)
                }
                "analyze" => options.analyze = true,
                "--analysis-facts" => {
                    options.analysis_facts = Some(next_path(&mut arguments, "--analysis-facts")?)
                }
                "--offer" => options
                    .analysis_offers
                    .push(next_string(&mut arguments, "--offer")?),
                "logging" => {
                    if options.logging_command.is_some() {
                        bail!("logging may be specified only once");
                    }
                    let command = next_string(&mut arguments, "logging")?;
                    options.logging_command = Some(match command.as_str() {
                        "inspect" => LoggingCommand::Inspect,
                        "diff" => LoggingCommand::Diff,
                        "restore" => LoggingCommand::Restore,
                        _ => bail!(
                            "unknown logging command {command:?}; use logging inspect, logging diff, or logging restore"
                        ),
                    });
                }
                "--backup" => options.logging_backup = Some(next_path(&mut arguments, "--backup")?),
                "--latest" => options.logging_latest_backup = true,
                "--json" => options.json = true,
                "--draft-fingerprints" => {
                    options.draft_fingerprints =
                        Some(next_path(&mut arguments, "--draft-fingerprints")?)
                }
                "--ratings" => options.ratings = Some(next_path(&mut arguments, "--ratings")?),
                "import-heartharena" => options.import_heartharena = true,
                "import-hsreplay" => options.import_hsreplay = true,
                "import-firestone" => options.import_firestone = true,
                "--firestone-format" => {
                    options.firestone_format =
                        Some(next_string(&mut arguments, "--firestone-format")?)
                }
                "--output" => options.ratings_output = Some(next_path(&mut arguments, "--output")?),
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--version" | "-V" => {
                    println!("ArenaNext {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                unexpected => bail!("unknown argument `{unexpected}`; use --help"),
            }
        }
        options.validate()?;
        Ok(options)
    }

    fn validate(&self) -> Result<()> {
        let import_commands = [
            self.import_heartharena,
            self.import_hsreplay,
            self.import_firestone,
        ]
        .into_iter()
        .filter(|flag| *flag)
        .count();
        if import_commands > 1 {
            bail!("only one import command may run at a time");
        }
        if self.ratings_output.is_some() && import_commands == 0 {
            bail!("--output is valid only with an import command");
        }
        if self.firestone_format.is_some() && !self.import_firestone {
            bail!("--firestone-format is valid only with import-firestone");
        }
        if let Some(format) = &self.firestone_format {
            match format.as_str() {
                "arena" | "arena-underground" => {}
                other => {
                    bail!("unknown Firestone format {other:?}; use arena or arena-underground")
                }
            }
        }
        if import_commands > 0 {
            let conflicts = self.logs.is_some()
                || self.cards.is_some()
                || self.arena_rules.is_some()
                || self.arena_mode.is_some()
                || self.once
                || self.demo
                || self.inspect
                || self.enable_logging
                || self.capture_status
                || self.request_screen_recording
                || self.capture_window
                || self.read_deck
                || self.read_offer
                || self.draft_fingerprints.is_some()
                || self.ratings.is_some()
                || self.doctor
                || self.replay
                || self.explain_card.is_some()
                || self.analyze
                || self.analysis_facts.is_some()
                || !self.analysis_offers.is_empty();
            if conflicts {
                bail!("import commands may be combined only with --output and --json");
            }
        }
        if self.analysis_facts.is_some() || !self.analysis_offers.is_empty() {
            if !self.analyze {
                bail!("--analysis-facts and --offer are valid only with analyze");
            }
        }
        if self.analyze {
            let conflicts = self.once
                || self.demo
                || self.inspect
                || self.enable_logging
                || self.capture_status
                || self.request_screen_recording
                || self.capture_window
                || self.read_deck
                || self.read_offer
                || self.draft_fingerprints.is_some()
                || self.doctor
                || self.replay
                || self.explain_card.is_some();
            if conflicts {
                bail!(
                    "analyze may be combined only with log, card, rules, analysis-facts, offer, ratings, and JSON options"
                );
            }
        }
        let Some(command) = self.logging_command else {
            if self.logging_backup.is_some() || self.logging_latest_backup {
                bail!("--backup and --latest are valid only with logging restore");
            }
            return Ok(());
        };

        let has_other_action = self.logs.is_some()
            || self.cards.is_some()
            || self.arena_rules.is_some()
            || self.arena_mode.is_some()
            || self.once
            || self.demo
            || self.inspect
            || self.enable_logging
            || self.capture_status
            || self.request_screen_recording
            || self.capture_window
            || self.read_deck
            || self.read_offer
            || self.draft_fingerprints.is_some()
            || self.ratings.is_some()
            || self.import_heartharena
            || self.import_hsreplay
            || self.import_firestone
            || self.firestone_format.is_some()
            || self.ratings_output.is_some()
            || self.doctor
            || self.replay
            || self.explain_card.is_some()
            || self.analyze
            || self.analysis_facts.is_some()
            || !self.analysis_offers.is_empty();
        if has_other_action {
            bail!("logging commands cannot be combined with another ArenaNext action");
        }

        match command {
            LoggingCommand::Restore => {
                match (self.logging_backup.is_some(), self.logging_latest_backup) {
                    (true, false) | (false, true) => Ok(()),
                    (false, false) => bail!(
                        "logging restore requires either --backup PATH or --latest; it never guesses a file to restore"
                    ),
                    (true, true) => {
                        bail!("logging restore accepts either --backup PATH or --latest, not both")
                    }
                }
            }
            LoggingCommand::Inspect | LoggingCommand::Diff => {
                if self.logging_backup.is_some() || self.logging_latest_backup {
                    bail!("--backup and --latest are valid only with logging restore");
                }
                Ok(())
            }
        }
    }
}

fn next_path(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &str,
) -> Result<PathBuf> {
    arguments
        .next()
        .map(PathBuf::from)
        .with_context(|| format!("{flag} requires a path"))
}

fn next_string(
    arguments: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &str,
) -> Result<String> {
    arguments
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("{flag} requires a value"))
}

fn print_help() {
    println!(
        "Logging commands: logging inspect | logging diff | logging restore --backup PATH | logging restore --latest\n\
         Inspect and diff are read-only. Restore accepts exactly one ArenaNext-created backup and never restarts Hearthstone.\n"
    );
    println!("Deck reader: --read-deck (local macOS Vision OCR; never writes a captured image)\n");
    println!(
        "ArenaNext — small native Hearthstone Arena overlay\n\nUSAGE:\n  arena-next [--logs SESSION_DIR] [--cards CARD_CACHE] [--arena-rules RULES.json] [--arena-mode MODE] [--once] [--demo]\n  arena-next import-heartharena [--output RATINGS.json] [--json]\n  arena-next import-hsreplay [--output RATINGS.json] [--json]\n  arena-next import-firestone [--firestone-format arena|arena-underground] [--output RATINGS.json] [--json]\n  arena-next doctor [--logs SESSION_DIR] [--cards CARD_CACHE] [--arena-rules RULES.json] [--arena-mode MODE] [--json]\n  arena-next replay SESSION_DIR [--cards CARD_CACHE] [--arena-rules RULES.json] [--arena-mode MODE]\n  arena-next explain-card CARD_ID [--logs SESSION_DIR] [--cards CARD_CACHE] [--arena-rules RULES.json] [--arena-mode MODE] [--json]\n  arena-next analyze [--logs SESSION_DIR] [--cards CARD_CACHE] [--arena-rules RULES.json] [--analysis-facts FACTS.json] [--offer CARD_ID]... [--ratings RATINGS.json]\n\nOPTIONS:\n  import-heartharena            Explicitly fetch and cache all public HearthArena class scores\n  import-hsreplay               Explicitly fetch and cache the public HSReplay arena card stats\n  import-firestone              Explicitly fetch and cache Firestone's per-class arena card stats\n  --firestone-format FORMAT     Firestone arena format bucket (arena or arena-underground; default arena)\n  --output PATH                 Override the default rating-cache destination for the import\n  --logs PATH                  Read this log-session directory instead of discovering Hearthstone\n  --cards PATH                 Read this ArenaNext card-data cache instead of the default\n  --arena-rules PATH           Read a local versioned Arena rules manifest; never downloads data\n  --arena-mode ID              Select a manifest mode when it has no unambiguous default\n  --analysis-facts PATH        Read a local versioned semantic-facts file for analyze\n  --offer CARD_ID              Analyze one offered card counterfactually; repeat for each offer\n  --draft-fingerprints PATH    Enable local draft matching from this fingerprint catalog\n  --ratings PATH               Override the default cached ratings file\n  --once                       Print the current overlay model and exit; never opens a window\n  --demo                       Open a native static demo without touching Hearthstone\n  inspect, --inspect           Print paths, rules, and logging status; never changes a file\n  doctor                       Print a read-only installation, log, deck, catalog, and capture report\n  replay SESSION_DIR           Print a deterministic JSON state snapshot and exit\n  explain-card CARD_ID         Explain metadata and bounded log provenance for one card ID\n  analyze                      Emit deterministic deck profile and offered-card deltas; never calls an AI\n  --json                       Request JSON from diagnostic commands\n  --enable-logging             Explicitly merge required log.config settings and create a backup\n  --capture-status             Print Screen Recording/window-capture status; never prompts\n  --request-screen-recording   Explicitly ask macOS for Screen Recording access\n  --capture-window             Capture one current Hearthstone window in memory; never writes it\n  -h, --help                   Show this help\n  -V, --version                Show the application version\n\nArenaNext never changes Hearthstone logging automatically. `--enable-logging`\nonly merges the five required sections, writes atomically, creates a backup,\nand never restarts Hearthstone. Network access occurs only for the explicit\nimport-heartharena command; normal overlay startup uses the local cache."
    );
    println!(
        "Card preview exception: hovering a deck row explicitly fetches that card's 256px render once and caches it locally."
    );
}

#[cfg(target_os = "macos")]
fn run_macos(options: Options) -> Result<()> {
    use arena_next_macos_overlay::{
        OfferScoreBadge, OverlayBounds, OverlayCommand, OverlayHost, TickControl,
        hearthstone_is_frontmost,
    };

    if options.import_heartharena {
        return run_heartharena_import(&options);
    }
    if options.import_hsreplay {
        return run_hsreplay_import(&options);
    }
    if options.import_firestone {
        return run_firestone_import(&options);
    }

    if let Some(command) = options.logging_command {
        return run_logging_command(&options, command);
    }
    if options.doctor {
        return doctor(&options);
    }
    if options.analyze {
        return analyze(&options);
    }
    if let Some(card_id) = options.explain_card.as_deref() {
        return explain_card(&options, card_id);
    }
    if options.replay {
        let snapshot = open_observer_full(&options)?.snapshot();
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }

    if (options.draft_fingerprints.is_some() || options.ratings.is_some())
        && (options.demo || options.once)
    {
        bail!(
            "draft fingerprint matching runs only in the live native overlay, not with --demo or --once"
        )
    }

    let capture_requested = options.capture_status
        || options.request_screen_recording
        || options.capture_window
        || options.read_deck
        || options.read_offer;
    if capture_requested
        && (options.inspect
            || options.enable_logging
            || options.once
            || options.demo
            || options.logs.is_some()
            || options.cards.is_some()
            || options.arena_rules.is_some()
            || options.arena_mode.is_some()
            || options.draft_fingerprints.is_some()
            || options.ratings.is_some())
    {
        bail!(
            "capture commands cannot be combined with log, card, inspection, demo, or overlay commands"
        );
    }
    if capture_requested {
        return run_capture_command(&options);
    }

    if options.inspect && options.enable_logging {
        bail!("--inspect and --enable-logging cannot be used together")
    }
    if options.inspect {
        return print_inspection(&options);
    }
    if options.enable_logging {
        return enable_logging();
    }

    if options.once {
        let native_model = if options.demo {
            demo_model()
        } else {
            let observer = open_observer(&options)?;
            let mut model = model::from_snapshot(&observer.snapshot());
            if let LogStaleness::Stale { age_secs } =
                hs_observer::session_staleness(observer.session(), LOG_STALENESS_THRESHOLD)
            {
                model.lines.push(format!(
                    "Hearthstone log activity stopped {} min ago; restart Hearthstone to restore deck/card detection",
                    age_secs / 60
                ));
            }
            model
        };
        println!("{}", native_model.title);
        for line in native_model.lines {
            println!("{line}");
        }
        for row in native_model.deck_rows {
            println!(
                "{:>2}  {}{}",
                row.mana_cost
                    .map(|cost| cost.to_string())
                    .unwrap_or_else(|| "—".to_owned()),
                row.name,
                if row.count > 1 {
                    format!(" ×{}", row.count)
                } else {
                    String::new()
                }
            );
        }
        return Ok(());
    }

    // Initialize AppKit before starting a possible ScreenCaptureKit worker.
    // Normal startup intentionally does not request capture permission; the
    // worker only attempts a direct window frame during an unresolved draft.
    let native_model = if options.demo {
        demo_model()
    } else {
        starting_model()
    };
    let overlay = OverlayHost::new(
        OverlayBounds::new(24.0, 64.0, 360.0, 700.0),
        &appkit_model(native_model),
    )?;
    let (observer_state, activation_generation) = if options.demo {
        (None, None)
    } else {
        let (state, generation) = start_observer(&options)?;
        (Some(state), Some(generation))
    };
    let tracks_hearthstone_activity = !options.demo;
    let mut manually_hidden = false;
    let mut interactive = false;
    let mut popup_visible = false;
    let mut hearthstone_was_frontmost = false;
    let mut requested_card_art = BTreeSet::new();
    overlay.run_with_tick(Duration::from_millis(200), move |host| {
        if let Some(command) = host.take_command()? {
            match command {
                OverlayCommand::ToggleVisibility => manually_hidden = !manually_hidden,
                OverlayCommand::ToggleInteraction => {
                    interactive = !interactive;
                    host.set_interactive(interactive)?;
                }
                OverlayCommand::ShowPopup => popup_visible = !popup_visible,
                OverlayCommand::FixDeckHelp => {
                    popup_visible = false;
                    host.hide_popup()?;
                    host.show_fix_deck_help()?;
                }
                OverlayCommand::Quit => return Ok(TickControl::Stop),
            }
        }
        let model = observer_state
            .as_ref()
            .map(current_model)
            .unwrap_or_else(demo_model);
        host.update_model(&appkit_model(model))?;
        if let Some(card_id) = host.hovered_card_id()?
            && requested_card_art.insert(card_id.clone())
            && !default_card_art_path(&card_id).is_file()
        {
            thread::spawn(move || {
                if let Err(error) = download_card_art(&card_id) {
                    app_log(format!(
                        "card preview download failed for {card_id}: {error:#}"
                    ));
                }
            });
        }
        let offer_overlay = observer_state
            .as_ref()
            .and_then(|state| state.read().ok()?.offer_overlay.clone());
        let popup = popup_model(observer_state.as_ref(), !manually_hidden, interactive);
        host.update_popup(&popup)?;
        if popup_visible {
            host.show_popup()?;
        } else {
            host.hide_popup()?;
        }
        let hearthstone_frontmost = !tracks_hearthstone_activity || hearthstone_is_frontmost()?;
        if hearthstone_frontmost && !hearthstone_was_frontmost {
            if let Some(generation) = &activation_generation {
                generation.fetch_add(1, Ordering::Relaxed);
            }
        }
        hearthstone_was_frontmost = hearthstone_frontmost;
        if !manually_hidden && hearthstone_frontmost {
            host.show()?;
            if let Some(offer_overlay) = offer_overlay {
                let badges = offer_overlay.badges.map(|badge| {
                    Ok(OfferScoreBadge {
                        bounds: host.bounds_from_top_left(
                            badge.top_left_x,
                            badge.top_left_y,
                            badge.width,
                            badge.height,
                        )?,
                        caption: badge.caption,
                        detail: badge.detail,
                        score: badge.score,
                        loading: badge.loading,
                    })
                });
                let [left, middle, right] = badges;
                host.show_offer_scores(&[left?, middle?, right?])?;
            } else {
                host.hide_offer_scores()?;
            }
        } else {
            host.hide()?;
        }
        Ok::<TickControl, arena_next_macos_overlay::OverlayError>(TickControl::Continue)
    })?;
    Ok(())
}

/// Run the deliberately opt-in direct-window capture diagnostic.
///
/// This is kept separate from normal application startup so merely launching
/// ArenaNext neither asks for Screen Recording nor takes a screenshot. The
/// image never reaches disk: this command only verifies the small native
/// ScreenCaptureKit boundary that draft recognition will consume.
#[cfg(target_os = "macos")]
fn run_capture_command(options: &Options) -> Result<()> {
    use arena_next_macos_capture::{CaptureOptions, MacosWindowCapture};

    let capture = MacosWindowCapture::new();
    let permission_before = capture.screen_recording_permission();
    let permission_after_request = options
        .request_screen_recording
        .then(|| capture.request_screen_recording_access());
    let permission = permission_after_request.unwrap_or(permission_before);
    let capabilities = capture.capabilities();

    let mut output = serde_json::json!({
        "screenRecording": permission_json(permission),
        "capabilities": {
            "hearthstoneWindowDiscovery": feature_json(capabilities.hearthstone_window_discovery),
            "directWindowCapture": feature_json(capabilities.direct_window_capture),
            "fullDesktopCapture": feature_json(capabilities.full_desktop_capture),
            "processMemoryInspection": feature_json(capabilities.process_memory_inspection),
            "inputInjection": feature_json(capabilities.input_injection),
            "requiresScreenRecordingPermission": capabilities.requires_screen_recording_permission,
        },
        "capture": {
            "attempted": options.capture_window || options.read_deck || options.read_offer,
            "storedOnDisk": false,
        },
    });

    if (options.capture_window || options.read_deck || options.read_offer)
        && permission.is_granted()
    {
        // ScreenCaptureKit image capture from a bare command-line process
        // needs an AppKit runtime. The normal overlay creates it first; this
        // explicit diagnostic initializes it on the macOS main thread.
        capture.initialize_appkit_runtime()?;
        let windows = capture.find_hearthstone_windows()?;
        output["windows"] =
            serde_json::Value::Array(windows.iter().map(game_window_json).collect::<Vec<_>>());
        if let Some(window) = windows.first() {
            if options.read_offer {
                let offers = capture.capture_draft_offer_text(
                    window,
                    CaptureOptions {
                        timeout: Duration::from_secs(30),
                        ..CaptureOptions::default()
                    },
                )?;
                let frame = &offers.frame;
                output["capture"] = serde_json::json!({
                    "attempted": true,
                    "captured": true,
                    "storedOnDisk": false,
                    "windowId": frame.window_id.0,
                    "widthPx": frame.width_px,
                    "heightPx": frame.height_px,
                    "draftOfferText": offers.offers.iter().map(|slot| slot.iter().map(|item| serde_json::json!({
                        "text": item.text,
                        "confidence": item.confidence,
                    })).collect::<Vec<_>>()).collect::<Vec<_>>(),
                });
            } else if options.read_deck {
                let sidebar = capture.capture_deck_sidebar(
                    window,
                    CaptureOptions {
                        timeout: Duration::from_secs(30),
                        ..CaptureOptions::default()
                    },
                )?;
                let frame = &sidebar.frame;
                let cards = load_cards(None)?;
                let observations = sidebar
                    .text
                    .iter()
                    .map(|item| {
                        arena_draft::SidebarTextObservation::new(&item.text, item.confidence)
                    })
                    .collect::<Vec<_>>();
                let deck = arena_draft::interpret_deck_sidebar(&observations, &cards);
                output["capture"] = serde_json::json!({
                    "attempted": true,
                    "captured": true,
                    "storedOnDisk": false,
                    "windowId": frame.window_id.0,
                    "widthPx": frame.width_px,
                    "heightPx": frame.height_px,
                    "bytesPerRow": frame.bytes_per_row,
                    "pixelFormat": "bgra8",
                    "byteLength": frame.pixels.len(),
                    "deck": deck,
                    "deckSidebarText": sidebar.text.iter().map(|item| serde_json::json!({
                        "text": item.text,
                        "confidence": item.confidence,
                        "bounds": {
                            "x": item.x,
                            "y": item.y,
                            "width": item.width,
                            "height": item.height,
                        },
                    })).collect::<Vec<_>>(),
                });
            } else {
                let frame = capture.capture_window(window, CaptureOptions::default())?;
                output["capture"] = serde_json::json!({
                    "attempted": true,
                    "captured": true,
                    "storedOnDisk": false,
                    "windowId": frame.window_id.0,
                    "widthPx": frame.width_px,
                    "heightPx": frame.height_px,
                    "bytesPerRow": frame.bytes_per_row,
                    "pixelFormat": "bgra8",
                    "byteLength": frame.pixels.len(),
                });
            }
        } else {
            output["capture"] = serde_json::json!({
                "attempted": true,
                "captured": false,
                "storedOnDisk": false,
                "reason": "no_shareable_hearthstone_window",
            });
        }
    } else if options.capture_window || options.read_deck || options.read_offer {
        output["capture"] = serde_json::json!({
            "attempted": true,
            "captured": false,
            "storedOnDisk": false,
            "reason": "screen_recording_permission_required",
        });
    }

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(target_os = "macos")]
fn permission_json(
    permission: arena_next_macos_capture::ScreenRecordingPermission,
) -> &'static str {
    match permission {
        arena_next_macos_capture::ScreenRecordingPermission::Granted => "granted",
        arena_next_macos_capture::ScreenRecordingPermission::Required => "required",
    }
}

#[cfg(target_os = "macos")]
fn run_heartharena_import(options: &Options) -> Result<()> {
    let output_path = options
        .ratings_output
        .clone()
        .unwrap_or_else(default_heartharena_ratings_path);
    let mut response = ureq::get(arena_scoring::HEARTHARENA_TIERLIST_URL)
        .header(
            "User-Agent",
            concat!(
                "ArenaNext/",
                env!("CARGO_PKG_VERSION"),
                " (local rating importer)"
            ),
        )
        .call()
        .context("could not fetch the public HearthArena tier list")?;
    let source = response
        .body_mut()
        .read_to_string()
        .context("could not read the HearthArena tier-list response")?;
    let imported = arena_scoring::import_heartharena_html(&source)?;
    arena_scoring::write_local_rating_file(&output_path, &imported)?;
    let output = serde_json::json!({
        "provider": imported.provider,
        "source": arena_scoring::HEARTHARENA_TIERLIST_URL,
        "dataTimestamp": imported.data_timestamp,
        "dataVersion": imported.data_version,
        "ratingRows": imported.ratings.len(),
        "output": output_path,
        "networkPolicy": "explicit import only; normal overlay startup is cache-only",
    });
    if options.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "Imported {} HearthArena class/card scores to {} ({})",
            imported.ratings.len(),
            output_path.display(),
            imported
                .data_version
                .as_deref()
                .unwrap_or("version unavailable")
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_hsreplay_import(options: &Options) -> Result<()> {
    let output_path = options
        .ratings_output
        .clone()
        .unwrap_or_else(default_hsreplay_ratings_path);
    // hsreplay.net's Cloudflare edge fingerprints the TLS ClientHello and
    // serves HTTP 403 to non-browser TLS stacks (rustls, Node's OpenSSL) even
    // with a browser User-Agent and forced HTTP/1.1. The macOS system curl
    // (SecureTransport) passes, so this one import shells out to it; all other
    // ArenaNext network use stays on ureq. Verified 2026-08-05: this exact
    // command consistently returns HTTP 200 JSON.
    const HSREPLAY_CHROME_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
         AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";
    let output = std::process::Command::new("curl")
        .args(["--http1.1", "-sS", "--compressed", "-A", HSREPLAY_CHROME_UA])
        .arg(arena_scoring::HSREPLAY_ARENA_CARD_STATS_URL)
        .output()
        .context("could not run the system curl binary")?;
    if !output.status.success() {
        anyhow::bail!(
            "could not fetch the public HSReplay arena card stats (curl exit {})",
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        );
    }
    let source = std::str::from_utf8(&output.stdout)
        .context("the HSReplay card-stats response was not valid UTF-8")?;
    let imported = arena_scoring::import_hsreplay_json(source)?;
    arena_scoring::write_local_rating_file(&output_path, &imported)?;
    let output = serde_json::json!({
        "provider": imported.provider,
        "source": arena_scoring::HSREPLAY_ARENA_CARD_STATS_URL,
        "dataTimestamp": imported.data_timestamp,
        "dataVersion": imported.data_version,
        "ratingRows": imported.ratings.len(),
        "output": output_path,
        "networkPolicy": "explicit import only; normal overlay startup is cache-only",
    });
    if options.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "Imported {} HSReplay class/card scores to {} ({})",
            imported.ratings.len(),
            output_path.display(),
            imported
                .data_version
                .as_deref()
                .unwrap_or("version unavailable")
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn run_firestone_import(options: &Options) -> Result<()> {
    let format = options.firestone_format.as_deref().unwrap_or("arena");
    let output_path = options
        .ratings_output
        .clone()
        .unwrap_or_else(default_firestone_ratings_path);
    let mut files = Vec::with_capacity(arena_scoring::FIRESTONE_CLASS_SLUGS.len());
    for slug in arena_scoring::FIRESTONE_CLASS_SLUGS {
        let url = format!(
            "{}/{format}/last-patch/{slug}.gz.json",
            arena_scoring::FIRESTONE_CARDS_BASE_URL
        );
        let mut response = ureq::get(&url)
            .header(
                "User-Agent",
                concat!(
                    "ArenaNext/",
                    env!("CARGO_PKG_VERSION"),
                    " (local rating importer)"
                ),
            )
            .call()
            .with_context(|| format!("could not fetch Firestone card stats for {slug}"))?;
        let source = response
            .body_mut()
            .read_to_string()
            .with_context(|| format!("could not read the Firestone response for {slug}"))?;
        files.push(arena_scoring::import_firestone_json(&source)?);
    }
    let imported = arena_scoring::merge_local_rating_files("Firestone", files)?;
    arena_scoring::write_local_rating_file(&output_path, &imported)?;
    let output = serde_json::json!({
        "provider": imported.provider,
        "source": format!(
            "{}/{{format}}/last-patch/{{slug}}.gz.json",
            arena_scoring::FIRESTONE_CARDS_BASE_URL
        ),
        "format": format,
        "dataTimestamp": imported.data_timestamp,
        "ratingRows": imported.ratings.len(),
        "output": output_path,
        "networkPolicy": "explicit import only; normal overlay startup is cache-only",
    });
    if options.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "Imported {} Firestone class/card scores to {} ({format})",
            imported.ratings.len(),
            output_path.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn feature_json(availability: arena_next_macos_capture::FeatureAvailability) -> serde_json::Value {
    use arena_next_macos_capture::FeatureAvailability;

    match availability {
        FeatureAvailability::Available => serde_json::json!({ "status": "available" }),
        FeatureAvailability::PermissionRequired => {
            serde_json::json!({ "status": "permission_required" })
        }
        FeatureAvailability::DisabledByDesign(reason) => {
            serde_json::json!({ "status": "disabled_by_design", "reason": reason })
        }
        FeatureAvailability::Unsupported(reason) => {
            serde_json::json!({ "status": "unsupported", "reason": reason })
        }
    }
}

#[cfg(target_os = "macos")]
fn game_window_json(window: &arena_next_macos_capture::GameWindow) -> serde_json::Value {
    serde_json::json!({
        "id": window.id.0,
        "ownerBundleId": window.owner_bundle_id,
        "ownerName": window.owner_name,
        "title": window.title,
        "framePoints": {
            "x": window.frame.x_points,
            "y": window.frame.y_points,
            "width": window.frame.width_points,
            "height": window.frame.height_points,
        },
        "windowLayer": window.window_layer,
        "onScreen": window.on_screen,
        "active": window.active,
    })
}

#[cfg(target_os = "macos")]
fn demo_model() -> model::NativeOverlayModel {
    model::NativeOverlayModel {
        title: "ArenaNext · native overlay".to_owned(),
        lines: vec![
            "No browser runtime".to_owned(),
            "Click-through · all Spaces · fullscreen auxiliary".to_owned(),
        ],
        deck_rows: Vec::new(),
    }
}

#[cfg(target_os = "macos")]
fn starting_model() -> model::NativeOverlayModel {
    model::NativeOverlayModel {
        title: "ArenaNext · starting".to_owned(),
        lines: vec!["Attaching to Hearthstone logs…".to_owned()],
        deck_rows: Vec::new(),
    }
}

const DRAFT_CAPTURE_INTERVAL: Duration = Duration::from_millis(800);
const STALE_RATING_AFTER_DAYS: i64 = 60;

/// Identifies one unresolved visible draft offer without putting any
/// screen-derived data into the deterministic log reducer. A new logged pick
/// advances the phase-local counter and therefore resets the accumulator, so
/// evidence from different normal-draft or Redraft *pick rounds* is never
/// combined.
#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct DraftKey {
    draft_deck_id: Option<String>,
    phase: hs_state::ArenaDraftPhase,
    pick_number: u8,
    phase_pick_count: u8,
}

/// App-owned, optional direct-window draft matcher.
///
/// The native app constructs this only when the user supplies a local,
/// versioned fingerprint catalog. No normal startup download, image corpus,
/// browser process, desktop screenshot, or capture permission prompt is
/// involved. The matcher's output remains presentation-only so a log replay
/// cannot accidentally persist a visual guess as Hearthstone fact.
#[cfg(target_os = "macos")]
struct DraftRecognitionWorker {
    recognizer: arena_draft::DraftRecognizer,
    accumulator: Option<arena_draft::ConfidenceAccumulator>,
    current_key: Option<DraftKey>,
    last_capture_at: Option<Instant>,
    latest: Option<model::ExternalDraftModel>,
    capture: arena_next_macos_capture::MacosWindowCapture,
    cards: CardCache,
    ratings: Option<arena_scoring::CompositeRatingProvider>,
}

/// Reads the committed deck sidebar independently of draft-offer matching.
/// It never requests permission and accepts a baseline only after two equal,
/// complete reads. A clipped/scrolling list is therefore diagnostic evidence,
/// never a destructive replacement.
#[cfg(target_os = "macos")]
struct DeckSidebarWorker {
    capture: arena_next_macos_capture::MacosWindowCapture,
    cards: CardCache,
    current_run: Option<String>,
    pending: Option<(BTreeMap<String, u8>, u8)>,
    pending_count: Option<arena_draft::DeckCount>,
    last_capture_at: Option<Instant>,
    last_activation_generation: u64,
}

#[cfg(target_os = "macos")]
struct DeckSidebarUpdate {
    card_ids: Option<Vec<String>>,
    observed_slots: u16,
    expected_slots: u16,
}

/// Reads the three visible card-title ribbons with Apple Vision, resolves
/// them against the local collectible-card catalog, and emits presentation-
/// only scores after two identical frames. It never mutates reducer state.
#[cfg(target_os = "macos")]
struct OfferOcrWorker {
    capture: arena_next_macos_capture::MacosWindowCapture,
    cards: CardCache,
    ratings: Option<arena_scoring::CompositeRatingProvider>,
    facts: AnalysisFacts,
    current_key: Option<DraftKey>,
    pending: Option<[String; 3]>,
    latest: Option<OfferOverlayState>,
    current_window: Option<arena_next_macos_capture::GameWindow>,
    confirmed: bool,
    last_capture_at: Option<Instant>,
}

#[cfg(target_os = "macos")]
impl OfferOcrWorker {
    fn from_options(options: &Options, cards: CardCache) -> Result<Self> {
        Ok(Self {
            capture: arena_next_macos_capture::MacosWindowCapture::new(),
            cards,
            ratings: load_live_ratings(options)?,
            facts: options
                .analysis_facts
                .as_deref()
                .map(AnalysisFacts::load)
                .transpose()?
                .unwrap_or_else(AnalysisFacts::empty),
            current_key: None,
            pending: None,
            latest: None,
            current_window: None,
            confirmed: false,
            last_capture_at: None,
        })
    }

    fn reset(&mut self) {
        self.current_key = None;
        self.pending = None;
        self.latest = None;
        self.current_window = None;
        self.confirmed = false;
        self.last_capture_at = None;
    }

    fn update(&mut self, snapshot: &ObserverSnapshot) -> Option<OfferOverlayState> {
        let Some(key) = unresolved_draft_key(snapshot) else {
            self.reset();
            return None;
        };
        if self.current_key.as_ref() != Some(&key) {
            self.current_key = Some(key);
            self.pending = None;
            self.current_window = self
                .capture
                .find_hearthstone_windows()
                .ok()
                .and_then(|windows| windows.into_iter().next());
            self.latest = self.current_window.as_ref().and_then(|window| {
                self.status_overlay(window, "Reading offer…\nScanning card titles")
                    .ok()
            });
            self.last_capture_at = None;
            self.confirmed = false;
        }
        if self.confirmed {
            return self.latest.clone();
        }
        if self
            .last_capture_at
            .is_some_and(|last| last.elapsed() < DRAFT_CAPTURE_INTERVAL)
        {
            return self.latest.clone();
        }
        self.last_capture_at = Some(Instant::now());
        let (ids, window) = match self.capture_once(snapshot.hero_class) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("ArenaNext offer OCR retry: {error:#}");
                if let Some(window) = &self.current_window {
                    self.latest = self
                        .status_overlay(window, "Reading offer…\nScanning card titles")
                        .ok();
                }
                return self.latest.clone();
            }
        };
        self.current_window = Some(window.clone());
        if self.pending.as_ref() != Some(&ids) {
            self.pending = Some(ids);
            self.latest = self
                .status_overlay(&window, "Reading offer…\nConfirming match")
                .ok();
            return self.latest.clone();
        }
        self.pending = None;
        self.latest = self.render(snapshot, ids, window).ok();
        self.confirmed = self.latest.is_some();
        self.latest.clone()
    }

    fn status_overlay(
        &self,
        window: &arena_next_macos_capture::GameWindow,
        text: &str,
    ) -> Result<OfferOverlayState> {
        let (caption, detail) = text.split_once('\n').unwrap_or((text, ""));
        let geometry = arena_draft::DraftCropGeometry::default().score_badge_geometry()?;
        Ok(OfferOverlayState {
            badges: std::array::from_fn(|index| {
                offer_badge_state(
                    window,
                    geometry.badges[index],
                    caption.to_uppercase(),
                    detail.to_uppercase(),
                    String::new(),
                    true,
                )
            }),
        })
    }

    fn capture_once(
        &self,
        hero_class: Option<hs_state::HeroClass>,
    ) -> Result<([String; 3], arena_next_macos_capture::GameWindow)> {
        use arena_next_macos_capture::CaptureOptions;

        if !self.capture.screen_recording_permission().is_granted() {
            anyhow::bail!("Screen Recording permission is not granted");
        }
        let windows = self.capture.find_hearthstone_windows()?;
        let window = windows
            .first()
            .context("no shareable Hearthstone window is available")?
            .clone();
        let capture = self.capture.capture_draft_offer_text(
            &window,
            CaptureOptions {
                timeout: Duration::from_secs(12),
                ..CaptureOptions::default()
            },
        )?;
        write_offer_ocr_audit(&capture)?;
        let mut ids = Vec::with_capacity(3);
        for slot in &capture.offers {
            ids.push(resolve_offer_ocr_slot(
                slot,
                &self.cards,
                self.ratings.as_ref(),
                hero_class,
            )?);
        }
        Ok((ids.try_into().expect("three OCR offer slots"), window))
    }

    fn render(
        &self,
        snapshot: &ObserverSnapshot,
        ids: [String; 3],
        window: arena_next_macos_capture::GameWindow,
    ) -> Result<OfferOverlayState> {
        let geometry = arena_draft::DraftCropGeometry::default().score_badge_geometry()?;
        let input = AnalysisInput {
            deck: snapshot
                .deck
                .iter()
                .map(|entry| hs_state::DeckCard {
                    card_id: entry.card_id.clone(),
                    count: entry.count,
                })
                .collect(),
            expected_slots: snapshot.deck_state.expected_slots,
        };
        let deck_is_exact = matches!(
            snapshot.deck_state.completeness,
            hs_state::DeckCompleteness::Complete
        );
        let badges = std::array::from_fn(|index| {
            let card_id = &ids[index];
            let card_name = self
                .cards
                .get(card_id)
                .map(|card| card.name.as_str())
                .unwrap_or(card_id);
            let (detail, deck_score) = if let Some(provider) = &self.ratings
                && deck_is_exact
            {
                let score = arena_scoring::score_offer(
                    provider,
                    snapshot.hero_class,
                    &input,
                    card_id,
                    &self.cards,
                    &self.facts,
                );
                let base = score
                    .base_rating
                    .as_ref()
                    .map(|rating| format!("{:.0}", rating.value))
                    .unwrap_or_else(|| "—".to_owned());
                let deck = score
                    .deck_score
                    .map(|value| format!("{value:.0}"))
                    .unwrap_or_else(|| "—".to_owned());
                let modifier = format!("{:+.0}", score.adjustment);
                let per_source = score
                    .provider_ratings
                    .iter()
                    .map(|evidence| {
                        format!(
                            "{} {:.0}",
                            short_provider_label(&evidence.provider.provider),
                            evidence.rating.value
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("  ");
                let weapon_warning = score
                    .adjustments
                    .iter()
                    .any(|adjustment| {
                        adjustment.kind == arena_scoring::AdjustmentKind::TooManyWeaponCharges
                    })
                    .then_some("  ·  WEAPONS");
                let source_line = (!per_source.is_empty())
                    .then_some(format!("  ·  {per_source}"))
                    .unwrap_or_default();
                (
                    format!(
                        "BASE {base}  ·  MOD {modifier}{source_line}{}",
                        weapon_warning.unwrap_or("")
                    ),
                    deck,
                )
            } else if let Some(provider) = &self.ratings {
                let base = provider
                    .rating(card_id, snapshot.hero_class)
                    .map(|rating| format!("{:.0}", rating.value))
                    .unwrap_or_else(|| "—".to_owned());
                (format!("BASE {base}  ·  DECK PAUSED"), "—".to_owned())
            } else {
                ("BASE —  ·  MOD —".to_owned(), "—".to_owned())
            };
            let normalized = geometry.badges[index];
            offer_badge_state(
                &window,
                normalized,
                card_name.to_owned(),
                detail,
                deck_score,
                false,
            )
        });
        Ok(OfferOverlayState { badges })
    }
}

#[cfg(target_os = "macos")]
fn write_offer_ocr_audit(capture: &arena_next_macos_capture::DraftOfferTextCapture) -> Result<()> {
    // This deliberately retains only the small title ribbons sent to Vision,
    // never the full Hearthstone frame. Each new attempt atomically replaces
    // the previous audit so private screen data cannot accumulate unbounded.
    const STRIP_X: f64 = 0.155;
    const STRIP_Y_FROM_BOTTOM: f64 = 0.535;
    const STRIP_WIDTH: f64 = 0.510;
    const STRIP_HEIGHT: f64 = 0.125;
    let directory = default_app_data_dir().join("ocr-audit");
    fs::create_dir_all(&directory)?;
    let frame = &capture.frame;
    let top = ((1.0 - STRIP_Y_FROM_BOTTOM - STRIP_HEIGHT) * f64::from(frame.height_px))
        .round()
        .clamp(0.0, f64::from(frame.height_px)) as u32;
    let height = (STRIP_HEIGHT * f64::from(frame.height_px)).round().max(1.0) as u32;
    for slot in 0..3_u32 {
        let x = ((STRIP_X + STRIP_WIDTH * f64::from(slot) / 3.0) * f64::from(frame.width_px))
            .round()
            .clamp(0.0, f64::from(frame.width_px)) as u32;
        let width = (STRIP_WIDTH * f64::from(frame.width_px) / 3.0)
            .round()
            .max(1.0) as u32;
        let ppm = bgra_crop_as_ppm(frame, x, top, width, height)?;
        atomic_write(&directory.join(format!("offer-{}.ppm", slot + 1)), &ppm)?;
    }
    let audit = serde_json::json!({
        "capturedAt": Utc::now(),
        "frame": { "width": frame.width_px, "height": frame.height_px },
        "retention": "rolling latest attempt; title ribbons only",
        "offers": capture.offers.iter().enumerate().map(|(slot, items)| serde_json::json!({
            "slot": slot + 1,
            "recognized": items.iter().map(|item| serde_json::json!({
                "text": item.text,
                "confidence": item.confidence,
                "bounds": { "x": item.x, "y": item.y, "width": item.width, "height": item.height },
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    atomic_write(
        &directory.join("latest-offer.json"),
        &serde_json::to_vec_pretty(&audit)?,
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn bgra_crop_as_ppm(
    frame: &arena_next_macos_capture::CapturedFrame,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    let width = width.min(frame.width_px.saturating_sub(x));
    let height = height.min(frame.height_px.saturating_sub(y));
    if width == 0 || height == 0 {
        anyhow::bail!("OCR audit crop is outside the captured frame");
    }
    let mut output = format!("P6\n{width} {height}\n255\n").into_bytes();
    output.reserve((width as usize) * (height as usize) * 3);
    for row in y..y + height {
        let row_start = row as usize * frame.bytes_per_row;
        for column in x..x + width {
            let offset = row_start + column as usize * 4;
            let pixel = frame
                .pixels
                .get(offset..offset + 4)
                .context("OCR audit crop exceeded the BGRA buffer")?;
            output.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    Ok(output)
}

#[cfg(target_os = "macos")]
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn short_provider_label(provider: &str) -> &str {
    if provider.starts_with("HearthArena") {
        "HA"
    } else if provider.starts_with("HSReplay") {
        "HSR"
    } else if provider.starts_with("Firestone") {
        "FS"
    } else {
        "?"
    }
}

#[cfg(target_os = "macos")]
fn offer_badge_state(
    window: &arena_next_macos_capture::GameWindow,
    normalized: arena_draft::NormalizedRect,
    caption: String,
    detail: String,
    score: String,
    loading: bool,
) -> OfferBadgeState {
    OfferBadgeState {
        top_left_x: window.frame.x_points + f64::from(normalized.x) * window.frame.width_points,
        top_left_y: window.frame.y_points + f64::from(normalized.y) * window.frame.height_points,
        width: f64::from(normalized.width) * window.frame.width_points,
        height: f64::from(normalized.height) * window.frame.height_points,
        caption,
        detail,
        score,
        loading,
    }
}

#[cfg(target_os = "macos")]
fn resolve_offer_ocr_slot(
    observations: &[arena_next_macos_capture::RecognizedText],
    cards: &CardCache,
    ratings: Option<&arena_scoring::CompositeRatingProvider>,
    hero_class: Option<hs_state::HeroClass>,
) -> Result<String> {
    let mut variants = observations
        .iter()
        .map(|item| item.text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    if variants.len() > 1 {
        variants.push(variants.join(" "));
    }
    let mut matches = variants
        .iter()
        .flat_map(|text| cards.find_collectible_by_ocr_name(text))
        .map(|card| card.id.clone())
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    if matches.is_empty() {
        let mut near = variants
            .iter()
            .flat_map(|text| {
                // Vision occasionally turns the final syllable of a clear,
                // reasonably long title into two substitutions (for example
                // `Motion Denied` -> `Motion Dentea`). Keep short names at a
                // one-edit radius, and accept the wider radius below only
                // when the best resulting card ID is unique.
                let normalized_len = text
                    .chars()
                    .filter(|character| character.is_alphanumeric())
                    .count();
                let maximum_distance = if normalized_len >= 10 { 2 } else { 1 };
                cards.find_collectible_by_ocr_name_near(text, maximum_distance)
            })
            .map(|(card, distance)| (card.id.clone(), distance))
            .collect::<Vec<_>>();
        if let Some(best_distance) = near.iter().map(|(_, distance)| *distance).min() {
            near.retain(|(_, distance)| *distance == best_distance);
            near.sort();
            near.dedup();
            matches = near.into_iter().map(|(card_id, _)| card_id).collect();
        }
    }
    if matches.len() > 1 {
        let rated = matches
            .iter()
            .filter(|card_id| {
                ratings.is_some_and(|provider| provider.rating(card_id, hero_class).is_some())
            })
            .cloned()
            .collect::<Vec<_>>();
        if rated.len() == 1 {
            matches = rated;
        }
    }
    match matches.as_slice() {
        [card_id] => Ok(card_id.clone()),
        [] => anyhow::bail!("offer title did not exactly match a collectible card"),
        _ => anyhow::bail!("offer title matched multiple collectible card IDs"),
    }
}

#[cfg(target_os = "macos")]
impl DeckSidebarWorker {
    fn new(cards: CardCache) -> Self {
        Self {
            capture: arena_next_macos_capture::MacosWindowCapture::new(),
            cards,
            current_run: None,
            pending: None,
            pending_count: None,
            last_capture_at: None,
            last_activation_generation: 0,
        }
    }

    fn reset(&mut self) {
        self.current_run = None;
        self.pending = None;
        self.pending_count = None;
        self.last_capture_at = None;
    }

    fn update(
        &mut self,
        snapshot: &ObserverSnapshot,
        activation_generation: u64,
        picks_enabled: bool,
    ) -> Option<DeckSidebarUpdate> {
        let run = snapshot.run.draft_deck_id.clone();
        let new_run = run != self.current_run;
        if new_run {
            self.current_run = run.clone();
            self.pending = None;
            self.pending_count = None;
            self.last_capture_at = None;
        }
        let activated = activation_generation != self.last_activation_generation;
        if activated {
            self.last_activation_generation = activation_generation;
            self.pending = None;
            self.pending_count = None;
            self.last_capture_at = None;
        }
        run.as_ref()?;

        // Before a baseline, keep retrying while the hero/package screens
        // transition. Afterward, capture only on activation or a new run.
        if picks_enabled
            && !activated
            && !new_run
            && self.pending.is_none()
            && self.pending_count.is_none()
        {
            return None;
        }
        let interval = if self.pending.is_some() || self.pending_count.is_some() {
            Duration::from_millis(400)
        } else {
            Duration::from_secs(2)
        };
        if self
            .last_capture_at
            .is_some_and(|last| last.elapsed() < interval)
        {
            return None;
        }
        self.last_capture_at = Some(Instant::now());

        let reading = self.capture_once().ok()?;
        let count = reading.count?;
        let count_confirmed = self.pending_count == Some(count);
        self.pending_count = Some(count);
        let Some(counts) = reading.authoritative_counts() else {
            return count_confirmed.then_some(DeckSidebarUpdate {
                card_ids: None,
                observed_slots: u16::from(count.observed),
                expected_slots: u16::from(count.capacity),
            });
        };
        let signature = (counts, count.observed);
        if self.pending.as_ref() != Some(&signature) {
            self.pending = Some(signature);
            return None;
        }
        self.pending = None;
        self.pending_count = None;

        let mut card_ids = Vec::with_capacity(usize::from(count.observed));
        for (card_id, quantity) in signature.0 {
            card_ids.extend(std::iter::repeat_n(card_id, usize::from(quantity)));
        }
        (card_ids.len() == usize::from(count.observed)).then_some(DeckSidebarUpdate {
            card_ids: Some(card_ids),
            observed_slots: u16::from(count.observed),
            expected_slots: u16::from(count.capacity),
        })
    }

    fn capture_once(&self) -> Result<arena_draft::SidebarDeckRead> {
        use arena_next_macos_capture::CaptureOptions;

        if !self.capture.screen_recording_permission().is_granted() {
            anyhow::bail!("Screen Recording permission is not granted");
        }
        let windows = self.capture.find_hearthstone_windows()?;
        let window = windows
            .first()
            .context("no shareable Hearthstone window is available")?;
        let capture = self.capture.capture_deck_sidebar(
            window,
            CaptureOptions {
                timeout: Duration::from_secs(30),
                ..CaptureOptions::default()
            },
        )?;
        let observations = capture
            .text
            .into_iter()
            .map(|item| arena_draft::SidebarTextObservation::new(item.text, item.confidence))
            .collect::<Vec<_>>();
        Ok(arena_draft::interpret_deck_sidebar(
            &observations,
            &self.cards,
        ))
    }
}

#[cfg(target_os = "macos")]
impl DraftRecognitionWorker {
    fn from_options(options: &Options, cards: CardCache) -> Result<Option<Self>> {
        let Some(path) = options.draft_fingerprints.as_deref() else {
            return Ok(None);
        };
        let source = std::fs::read_to_string(path).with_context(|| {
            format!(
                "could not read draft fingerprint catalog {}",
                path.display()
            )
        })?;
        let catalog: arena_draft::FingerprintCatalog =
            serde_json::from_str(&source).with_context(|| {
                format!(
                    "could not parse draft fingerprint catalog {}",
                    path.display()
                )
            })?;
        let recognizer = arena_draft::DraftRecognizer::new(
            arena_draft::DraftCropGeometry::default(),
            catalog,
            arena_draft::MatcherConfig::default(),
        )
        .map_err(|error| anyhow::anyhow!("invalid draft fingerprint catalog: {error}"))?;
        let ratings = load_live_ratings(options)?;

        Ok(Some(Self {
            recognizer,
            accumulator: None,
            current_key: None,
            last_capture_at: None,
            latest: None,
            capture: arena_next_macos_capture::MacosWindowCapture::new(),
            cards,
            ratings,
        }))
    }

    fn update(&mut self, snapshot: &ObserverSnapshot) -> Option<model::ExternalDraftModel> {
        if let Some(status) = redraft_capture_status(snapshot) {
            self.reset();
            return Some(draft_status(visible_draft_pick_number(snapshot), status));
        }
        let Some(key) = unresolved_draft_key(snapshot) else {
            self.reset();
            return None;
        };
        if self.current_key.as_ref() != Some(&key) {
            // After `Client chooses` the old card art can remain visible for
            // a short transition. Delay this *next* capture one interval so
            // evidence is never accidentally carried from the previous offer
            // into the next normal-draft or Redraft three-card pick round.
            // The first offer is still captured immediately.
            let replacing_offer = self.current_key.is_some();
            let phase = key.phase.clone();
            self.current_key = Some(key);
            self.accumulator = None;
            self.last_capture_at = replacing_offer.then(Instant::now);
            self.latest = Some(model::ExternalDraftModel {
                pick_number: visible_draft_pick_number(snapshot),
                lines: vec![match phase {
                    hs_state::ArenaDraftPhase::Redrafting => {
                        "Looking for the three Redraft cards…".to_owned()
                    }
                    _ => "Looking for the three draft cards…".to_owned(),
                }],
            });
        }

        let due = self
            .last_capture_at
            .is_none_or(|last| last.elapsed() >= DRAFT_CAPTURE_INTERVAL);
        if due {
            self.last_capture_at = Some(Instant::now());
            self.latest = Some(self.capture_once(snapshot));
        }
        self.latest.clone()
    }

    fn reset(&mut self) {
        self.accumulator = None;
        self.current_key = None;
        self.last_capture_at = None;
        self.latest = None;
    }

    fn capture_once(&mut self, snapshot: &ObserverSnapshot) -> model::ExternalDraftModel {
        use arena_next_macos_capture::CaptureOptions;

        let pick_number = visible_draft_pick_number(snapshot);

        if !self.capture.screen_recording_permission().is_granted() {
            return draft_status(
                pick_number,
                "Screen Recording permission is required for draft detection. Run --request-screen-recording explicitly.",
            );
        }
        let windows = match self.capture.find_hearthstone_windows() {
            Ok(windows) => windows,
            Err(error) => {
                return draft_status(pick_number, format!("Draft capture unavailable: {error}"));
            }
        };
        let Some(window) = windows.first() else {
            return draft_status(
                pick_number,
                "No shareable Hearthstone window is available for draft detection.",
            );
        };
        let frame = match self
            .capture
            .capture_window(window, CaptureOptions::default())
        {
            Ok(frame) => frame,
            Err(error) => {
                return draft_status(pick_number, format!("Draft capture unavailable: {error}"));
            }
        };
        let raw = arena_draft::RawFrame {
            width: frame.width_px,
            height: frame.height_px,
            bytes_per_row: frame.bytes_per_row,
            pixel_format: arena_draft::PixelFormat::Bgra8,
            pixels: frame.pixels,
        };
        let detection = match self.recognizer.detect_frame(&raw) {
            Ok(detection) => detection,
            Err(error) => {
                return draft_status(pick_number, format!("Draft image was not usable: {error}"));
            }
        };
        if self.accumulator.is_none() {
            self.accumulator = Some(
                arena_draft::ConfidenceAccumulator::for_offer_count(
                    // The selected crop layout owns the count. This avoids a
                    // hidden three-card assumption if a future calibrated
                    // offer layout is supplied.
                    arena_draft::AggregationConfig::default(),
                    detection.minimum_confidence,
                    detection.expected_offer_count,
                )
                .expect("default draft aggregation configuration is valid"),
            );
        }
        let accumulator = self
            .accumulator
            .as_mut()
            .expect("draft accumulator was initialized above");
        if let Err(error) = accumulator.observe(&detection) {
            return draft_status(pick_number, format!("Draft evidence was rejected: {error}"));
        }
        match accumulator.finish() {
            Ok(aggregate) => self.render_detection(snapshot, aggregate),
            Err(error) => draft_status(
                pick_number,
                format!("Draft evidence is incomplete: {error}"),
            ),
        }
    }

    fn render_detection(
        &self,
        snapshot: &ObserverSnapshot,
        aggregate: arena_draft::AggregatedDetection,
    ) -> model::ExternalDraftModel {
        use arena_draft::OfferRecommendation;
        use arena_scoring::RatingProvider;

        let recommendation = aggregate.recommendation();
        let ready = matches!(&recommendation, OfferRecommendation::Ready { .. });
        let mut lines = Vec::new();
        for (slot, offer) in aggregate.detection.offers.iter().enumerate() {
            let Some(candidate) = offer.candidates.first() else {
                lines.push(format!("Offer {}: unrecognized", slot + 1));
                continue;
            };
            let confidence = (candidate.confidence * 100.0).round().clamp(0.0, 100.0) as u8;
            let prefix = if ready { "" } else { "Candidate: " };
            let mut line = format!(
                "{prefix}{} · {confidence}%",
                display_card(&self.cards, &candidate.card_id)
            );
            if let Some(ratings) = &self.ratings {
                if let Some(rating) = ratings.rating(&candidate.card_id, snapshot.hero_class) {
                    line.push_str(&format!(" · {:.1}", rating.value));
                    if let Some(label) = rating.label {
                        line.push_str(&format!(" {label}"));
                    }
                    if let Some(sample_size) = rating.sample_size {
                        line.push_str(&format!(" (n={sample_size})"));
                    }
                } else {
                    line.push_str(" · No rating");
                }
            }
            lines.push(line);
        }
        if !ready {
            lines.push(format!(
                "Recognition withheld: {}",
                recommendation_reason(&recommendation)
            ));
        }
        if let Some(ratings) = &self.ratings {
            lines.push(rating_metadata_line(ratings));
        }
        model::ExternalDraftModel {
            pick_number: visible_draft_pick_number(snapshot),
            lines,
        }
    }
}

#[cfg(target_os = "macos")]
fn unresolved_draft_key(snapshot: &ObserverSnapshot) -> Option<DraftKey> {
    if snapshot.mode != hs_state::GameMode::Arena || snapshot.draft.current_offer.is_some() {
        return None;
    }
    let phase = snapshot.run.draft_phase.clone();
    if !phase.accepts_card_offers() {
        return None;
    }
    // A current-deck tail resync establishes that the normal Draft offer UI
    // is active, even though it cannot prove the historical pick number.
    // It is safe to recognize that visible three-card offer using epoch zero;
    // subsequent logged picks advance `phase_pick_count` and create a fresh
    // recognition epoch. Do not invent pick 1 merely to enable recognition.
    //
    // Redraft is different: its later five-card discard review has no
    // reliably identifiable log boundary, so unknown Redraft progress must
    // remain withheld by its explicit policy gate below.
    if phase == hs_state::ArenaDraftPhase::Redrafting
        && !snapshot.draft.redraft.accepts_normal_draft_capture()
    {
        return None;
    }
    Some(DraftKey {
        draft_deck_id: snapshot.run.draft_deck_id.clone(),
        phase,
        pick_number: visible_draft_pick_number(snapshot),
        phase_pick_count: snapshot.draft.phase_pick_count,
    })
}

/// Redraft's five discard choices are a deck-review action, not a five-card
/// draft offer. The reducer moves to `AwaitingDiscardReview` only after the
/// selected local rules policy says all normal pick rounds have completed;
/// this function then stops normal three-card screen crops without pretending
/// logs identified the review surface itself.
#[cfg(target_os = "macos")]
fn redraft_capture_status(snapshot: &ObserverSnapshot) -> Option<String> {
    use hs_state::RedraftStage;

    let redraft = &snapshot.draft.redraft;
    if snapshot.run.draft_phase == hs_state::ArenaDraftPhase::Redrafting
        && !redraft.pick_progress_known
    {
        return Some(
            "Redraft current deck was resynced, but pick progress is unknown; direct-window matching is withheld."
                .to_owned(),
        );
    }
    match redraft.stage {
        RedraftStage::Inactive => None,
        RedraftStage::PickingOffers if redraft.accepts_normal_draft_capture() => None,
        RedraftStage::PickingOffers => Some(
            "Redraft detected, but the selected Arena rules declare no Redraft pick/discard contract; direct-window matching is withheld."
                .to_owned(),
        ),
        RedraftStage::AwaitingDiscardReview => Some(format!(
            "Redraft pick rounds complete; choose {} cards to discard. Deck-review detection is not calibrated.",
            redraft
                .discard_count_required
                .map(|count| count.to_string())
                .unwrap_or_else(|| "the required".to_owned())
        )),
        RedraftStage::ReviewingDiscards => Some(format!(
            "Redraft deck review: {} discard selections observed; {} required. This is not a draft offer.",
            redraft.discarded_card_ids.len(),
            redraft
                .discard_count_required
                .map(|count| count.to_string())
                .unwrap_or_else(|| "count unknown".to_owned())
        )),
        RedraftStage::Complete => Some(
            "Redraft discard review submitted; waiting for an authoritative deck update."
                .to_owned(),
        ),
    }
}

/// A zero is intentionally surfaced as unknown after a current-deck tail
/// resync. It must never be converted into a fictional first pick.
#[cfg(target_os = "macos")]
fn visible_draft_pick_number(snapshot: &ObserverSnapshot) -> u8 {
    if snapshot.draft.has_exact_phase_progress() {
        snapshot.draft.pick_number
    } else {
        0
    }
}

#[cfg(target_os = "macos")]
fn draft_status(pick_number: u8, line: impl Into<String>) -> model::ExternalDraftModel {
    model::ExternalDraftModel {
        pick_number,
        lines: vec![line.into()],
    }
}

#[cfg(target_os = "macos")]
fn display_card(cards: &CardCache, card_id: &str) -> String {
    match cards.resolve(card_id) {
        CardResolution::Resolved { card } => format!("{} ({})", card.name, card.id),
        CardResolution::Unrevealed => "Unrevealed card".to_owned(),
        CardResolution::NonCardEntity { .. }
        | CardResolution::MissingMetadata { .. }
        | CardResolution::InvalidCardId { .. } => card_id.to_owned(),
    }
}

#[cfg(target_os = "macos")]
fn recommendation_reason(recommendation: &arena_draft::OfferRecommendation) -> String {
    use arena_draft::{OfferRecommendation, RecommendationWithheldReason};

    match recommendation {
        OfferRecommendation::Ready { .. } => "ready".to_owned(),
        OfferRecommendation::Withheld { reason } => match reason {
            RecommendationWithheldReason::ExpectedOfferCount { expected, actual } => {
                format!("expected {expected} offer items, observed {actual}")
            }
            RecommendationWithheldReason::MissingCandidate { slot } => {
                format!("offer {} has no candidate", slot + 1)
            }
            RecommendationWithheldReason::LowConfidence {
                slot,
                confidence,
                required,
            } => format!(
                "offer {} confidence {:.0}% is below {:.0}%",
                slot + 1,
                confidence * 100.0,
                required * 100.0
            ),
            RecommendationWithheldReason::InsufficientObservations { observed, required } => {
                format!("{observed}/{required} stable frames")
            }
            RecommendationWithheldReason::InvalidConfidenceThreshold => {
                "invalid confidence threshold".to_owned()
            }
        },
    }
}

#[cfg(target_os = "macos")]
fn rating_metadata_line(ratings: &arena_scoring::CompositeRatingProvider) -> String {
    use arena_scoring::RatingProvider;

    let metadata = ratings.metadata();
    let mut details = vec![
        format!("Ratings: {}", metadata.provider),
        metadata.data_timestamp.to_rfc3339(),
    ];
    if let Some(season) = &metadata.arena_season {
        details.push(format!("season {season}"));
    }
    if let Some(version) = &metadata.data_version {
        details.push(format!("v{version}"));
    }
    let age_days = Utc::now()
        .signed_duration_since(metadata.data_timestamp)
        .num_days();
    if age_days > STALE_RATING_AFTER_DAYS {
        details.push(format!("STALE ({age_days} days old)"));
    }
    details.join(" · ")
}

#[cfg(target_os = "macos")]
fn start_observer(options: &Options) -> Result<(Arc<RwLock<SharedObserverState>>, Arc<AtomicU64>)> {
    app_log(format!(
        "ArenaNext {} overlay worker starting",
        env!("CARGO_PKG_VERSION")
    ));
    let cards = load_cards(options.cards.as_deref())?;
    let draft_worker = DraftRecognitionWorker::from_options(options, cards.clone())?;
    let offer_ocr_worker = OfferOcrWorker::from_options(options, cards.clone())?;
    let sidebar_worker = DeckSidebarWorker::new(cards.clone());
    let activation_generation = Arc::new(AtomicU64::new(0));
    let state = Arc::new(RwLock::new(SharedObserverState {
        snapshot: hs_observer::resolve_snapshot(hs_state::ArenaSnapshot::empty(), &cards),
        last_error: None,
        checkpoint_error: None,
        // ScreenCaptureKit work begins only after this state moves onto the
        // dedicated observer thread below. The AppKit main thread merely
        // renders its latest text model.
        external_draft: None,
        offer_overlay: None,
        log_staleness: LogStaleness::NoLogs,
    }));
    let worker_state = Arc::clone(&state);
    let worker_activation_generation = Arc::clone(&activation_generation);
    let worker_options = options.clone();
    let checkpoint_path = default_observer_checkpoint_path();
    thread::Builder::new()
        .name("arena-next-log-observer".to_owned())
        .spawn(move || {
            let mut draft_worker = draft_worker;
            let mut offer_ocr_worker = offer_ocr_worker;
            let mut sidebar_worker = sidebar_worker;
            loop {
                let (mut observer, _checkpoint_restore) = match open_observer_with_checkpoint(
                    &worker_options,
                    cards.clone(),
                    &checkpoint_path,
                ) {
                    Ok(observer) => observer,
                    Err(error) => {
                        app_log(format!("observer attach failed: {error}"));
                        if let Ok(mut latest) = worker_state.write() {
                            record_staleness(&mut latest, LogStaleness::NoLogs);
                            latest.last_error = Some(error.to_string());
                            latest.external_draft = None;
                            latest.offer_overlay = None;
                        }
                        // Keep the small app launchable before Hearthstone
                        // starts, and retry without blocking AppKit.
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };
                // Checkpoint persistence is a recovery optimization, never a
                // reason to stop observing game logs. A failed write is shown
                // as a local diagnostic and retried after the next change.
                let mut checkpoint_error = observer
                    .write_checkpoint(&checkpoint_path)
                    .err()
                    .map(|error| error.to_string());
                let mut snapshot = observer.snapshot();
                if let Some(update) = sidebar_worker.update(
                    &snapshot,
                    worker_activation_generation.load(Ordering::Relaxed),
                    observer.arena_picks_enabled(),
                ) {
                    let applied = if let Some(card_ids) = update.card_ids {
                        observer.apply_complete_sidebar_baseline(
                            card_ids,
                            update.observed_slots,
                            update.expected_slots,
                        )
                    } else {
                        observer
                            .apply_sidebar_capacity(update.observed_slots, update.expected_slots)
                    };
                    if applied.is_ok() {
                        checkpoint_error = observer
                            .write_checkpoint(&checkpoint_path)
                            .err()
                            .map(|error| error.to_string());
                        snapshot = observer.snapshot();
                    }
                }
                let external_draft = draft_worker
                    .as_mut()
                    .and_then(|worker| worker.update(&snapshot));
                let offer_overlay = offer_ocr_worker.update(&snapshot);
                let staleness = observer.session_staleness(LOG_STALENESS_THRESHOLD);
                app_log(format!(
                    "attached method={:?} staleness={} session={}",
                    observer.attach_method(),
                    staleness_label(&staleness),
                    observer.session().display()
                ));
                if let Ok(mut latest) = worker_state.write() {
                    record_staleness(&mut latest, staleness);
                    latest.snapshot = snapshot;
                    latest.last_error = None;
                    latest.checkpoint_error = checkpoint_error.clone();
                    latest.external_draft = external_draft;
                    latest.offer_overlay = offer_overlay;
                }

                loop {
                    match observer.poll() {
                        Ok(result) => {
                            if result.changed {
                                checkpoint_error = observer
                                    .write_checkpoint(&checkpoint_path)
                                    .err()
                                    .map(|error| error.to_string());
                            }
                            let mut snapshot = observer.snapshot();
                            if let Some(update) = sidebar_worker.update(
                                &snapshot,
                                worker_activation_generation.load(Ordering::Relaxed),
                                observer.arena_picks_enabled(),
                            ) {
                                let applied = if let Some(card_ids) = update.card_ids {
                                    observer.apply_complete_sidebar_baseline(
                                        card_ids,
                                        update.observed_slots,
                                        update.expected_slots,
                                    )
                                } else {
                                    observer.apply_sidebar_capacity(
                                        update.observed_slots,
                                        update.expected_slots,
                                    )
                                };
                                if applied.is_ok() {
                                    checkpoint_error = observer
                                        .write_checkpoint(&checkpoint_path)
                                        .err()
                                        .map(|error| error.to_string());
                                    snapshot = observer.snapshot();
                                }
                            }
                            let external_draft = draft_worker
                                .as_mut()
                                .and_then(|worker| worker.update(&snapshot));
                            let offer_overlay = offer_ocr_worker.update(&snapshot);
                            let staleness = observer.session_staleness(LOG_STALENESS_THRESHOLD);
                            if let Ok(mut latest) = worker_state.write() {
                                record_staleness(&mut latest, staleness);
                                latest.snapshot = snapshot;
                                latest.last_error = None;
                                latest.checkpoint_error = checkpoint_error.clone();
                                latest.external_draft = external_draft;
                                latest.offer_overlay = offer_overlay;
                            }
                            // Proactive rotation: keep every component log below
                            // the game's hardcoded 10000KB cap before its own
                            // truncation stalls all writers (see
                            // `rotate_overlarge_component_logs`). This runs
                            // after `poll()` consumed the new bytes, so the
                            // next poll simply re-attaches the retained tail.
                            let rotation_outcome =
                                rotate_overlarge_component_logs(observer.session());
                            for rotation in rotation_outcome.rotations {
                                app_log(format!(
                                    "rotated {} from {} bytes, retained {}",
                                    rotation.component,
                                    rotation.previous_bytes,
                                    rotation.retained_bytes
                                ));
                            }
                            // A failed rotation leaves the file climbing toward
                            // the cap, so it must never be silent.
                            for failure in rotation_outcome.failures {
                                app_log(format!("rotation failed: {failure}"));
                            }
                        }
                        Err(error) => {
                            // Start a clean attachment attempt after a fatal
                            // I/O error. `LiveObserver` already handles normal
                            // rotation/truncation internally before this path.
                            app_log(format!("observer poll error: {error}"));
                            draft_worker.as_mut().map(DraftRecognitionWorker::reset);
                            sidebar_worker.reset();
                            offer_ocr_worker.reset();
                            if let Ok(mut latest) = worker_state.write() {
                                latest.last_error = Some(error.to_string());
                                latest.checkpoint_error = checkpoint_error.clone();
                                latest.external_draft = None;
                                latest.offer_overlay = None;
                            }
                            break;
                        }
                    }
                    // File reads and ScreenCaptureKit calls occur on this
                    // worker, never in AppKit's main loop.
                    thread::sleep(Duration::from_millis(250));
                }
                // Avoid a tight reconnect loop if the game exits or a log
                // directory becomes temporarily unreadable.
                thread::sleep(Duration::from_secs(1));
            }
        })
        .context("could not start the local Hearthstone log observer")?;
    Ok((state, activation_generation))
}

#[cfg(target_os = "macos")]
fn open_observer(options: &Options) -> Result<LiveObserver> {
    let cards = load_cards(options.cards.as_deref())?;
    open_observer_with_cards(options, cards)
}

#[cfg(target_os = "macos")]
fn open_observer_with_cards(options: &Options, cards: CardCache) -> Result<LiveObserver> {
    let rules = resolve_arena_rules(options)?;
    open_observer_with_cards_and_rules(options, cards, rules.as_ref())
}

/// Full deterministic history is intentionally opt-in. The live overlay,
/// doctor, and `--once` use current-deck tail attach; only replay/provenance
/// commands pay the cost of reconstructing old log history.
#[cfg(target_os = "macos")]
fn open_observer_full(options: &Options) -> Result<LiveObserver> {
    let cards = load_cards(options.cards.as_deref())?;
    let rules = resolve_arena_rules(options)?;
    let expected_deck_slots = rules.as_ref().map(|rules| rules.expected_deck_slots);
    let session = if let Some(session) = &options.logs {
        session.clone()
    } else {
        let paths = hs_paths::discover_macos()?;
        paths
            .latest_session
            .context("no Hearthstone log session found; pass --logs SESSION_DIR for replay")?
    };
    let mut observer = LiveObserver::attach_full_replay_with_expected_deck_slots(
        session,
        cards,
        expected_deck_slots,
    )?;
    observer.set_redraft_policy(redraft_policy_from_rules(rules.as_ref()))?;
    Ok(observer)
}

/// Attach the normal overlay worker with a validated local checkpoint. This
/// is deliberately not used by `doctor`, `replay`, or `--once`: those commands
/// remain deterministic read-only diagnostics over the supplied logs.
#[cfg(target_os = "macos")]
fn open_observer_with_checkpoint(
    options: &Options,
    cards: CardCache,
    checkpoint_path: &std::path::Path,
) -> Result<(LiveObserver, hs_observer::CheckpointRestoreStatus)> {
    let rules = resolve_arena_rules(options)?;
    let expected_deck_slots = rules.as_ref().map(|rules| rules.expected_deck_slots);
    let attached = if let Some(session) = &options.logs {
        LiveObserver::attach_with_checkpoint_and_expected_deck_slots(
            session,
            cards.clone(),
            checkpoint_path,
            expected_deck_slots,
        )
    } else {
        let paths = hs_paths::discover_macos()?;
        LiveObserver::attach_discovered_with_checkpoint_and_expected_deck_slots(
            &paths,
            cards.clone(),
            checkpoint_path,
            expected_deck_slots,
        )
        .context("no Hearthstone log session found; launch Hearthstone, pass --logs, or use --demo")
    }?;
    let (mut observer, checkpoint_status) = attached;
    let tail_snapshot = observer.snapshot();
    let needs_bounded_history_recovery = tail_snapshot.run.draft_deck_id.is_some()
        && tail_snapshot.deck_state.observed_slots > 0
        && (tail_snapshot.run.draft_phase == hs_state::ArenaDraftPhase::Unknown
            || tail_snapshot.run.state_origin == hs_state::ArenaStateOrigin::AuthoritativeResync);
    if needs_bounded_history_recovery {
        // Arena snapshots enumerate distinct IDs, not copy counts. A tail
        // resync therefore cannot distinguish ×1 from ×2 and must never be
        // promoted to a complete deck merely because later picks bring its
        // arithmetic total to 30. Replay the event-producing logs once to
        // recover proven multiplicities and lifecycle history, then persist
        // the resulting checkpoint normally.
        let session = observer.session().to_path_buf();
        observer = if options.logs.is_some() {
            // A user-supplied --logs pins the tracker to one explicit session.
            LiveObserver::attach_full_replay_with_expected_deck_slots(
                session,
                cards,
                expected_deck_slots,
            )?
        } else {
            // Discovered attach must keep rolling onto a newer session once
            // Hearthstone starts one; a fixed full-replay observer would pin
            // the tracker to the stale session and never follow the game.
            let paths = hs_paths::discover_macos()?;
            LiveObserver::attach_full_replay_and_follow_discovered_with_expected_deck_slots(
                &paths,
                cards,
                expected_deck_slots,
            )?
        };
    }
    observer.set_redraft_policy(redraft_policy_from_rules(rules.as_ref()))?;
    Ok((observer, checkpoint_status))
}

/// Resolve only a user-supplied local manifest. No default manifest is
/// searched for and no network/cache updater is involved: without this option
/// the reducer receives no configured expected deck size.
#[cfg(target_os = "macos")]
fn resolve_arena_rules(options: &Options) -> Result<Option<ResolvedArenaRules>> {
    let Some(path) = options.arena_rules.as_deref() else {
        if options.arena_mode.is_some() {
            bail!("--arena-mode requires --arena-rules PATH");
        }
        return Ok(None);
    };
    ArenaRulesManifest::load(path)?
        .resolve(options.arena_mode.as_deref())
        .map(Some)
}

/// Translate the local manifest contract into the pure core's policy type.
/// The core intentionally does not depend on `arena-rules`; it receives only
/// this selected, validated fact. The current Underground client contract is
/// five replacements; an explicit rules manifest can override that fallback
/// when Blizzard changes the mode.
#[cfg(target_os = "macos")]
fn redraft_policy_from_rules(
    rules: Option<&ResolvedArenaRules>,
) -> Option<hs_state::RedraftPolicy> {
    Some(
        rules
            .and_then(|rules| rules.redraft.as_ref())
            .map(|policy| hs_state::RedraftPolicy {
                pick_rounds: policy.pick_rounds,
                discard_count: policy.discard_count,
            })
            .unwrap_or(hs_state::RedraftPolicy {
                pick_rounds: 5,
                discard_count: 5,
            }),
    )
}

#[cfg(target_os = "macos")]
fn open_observer_with_cards_and_rules(
    options: &Options,
    cards: CardCache,
    rules: Option<&ResolvedArenaRules>,
) -> Result<LiveObserver> {
    let expected_deck_slots = rules.map(|rules| rules.expected_deck_slots);
    let mut observer = if let Some(session) = &options.logs {
        LiveObserver::attach_with_expected_deck_slots(session, cards, expected_deck_slots)
    } else {
        let paths = hs_paths::discover_macos()?;
        let logging_hint = paths
            .log_config
            .as_deref()
            .and_then(|path| hs_log_config::inspect(path).ok())
            .filter(|status| status.change_required)
            .map(|status| {
                format!(
                    " Hearthstone logging is not fully enabled at {}; rerun with --enable-logging, then restart Hearthstone yourself.",
                    status.path.display()
                )
            })
            .unwrap_or_default();
        LiveObserver::attach_discovered_with_expected_deck_slots(
            &paths,
            cards,
            expected_deck_slots,
        )
        .with_context(|| {
            format!(
                "no Hearthstone log session found; launch Hearthstone, pass --logs, or use --demo.{logging_hint}"
            )
        })
    }?;
    observer.set_redraft_policy(redraft_policy_from_rules(rules))?;
    Ok(observer)
}

#[cfg(target_os = "macos")]
fn run_logging_command(options: &Options, command: LoggingCommand) -> Result<()> {
    let paths = hs_paths::discover_macos()?;
    let config = paths
        .log_config
        .as_deref()
        .context("could not determine the conventional Hearthstone log.config path")?;

    match command {
        LoggingCommand::Inspect => {
            let status = hs_log_config::inspect(config)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "operation": "inspect",
                    "logging": status,
                    "message": "Read-only inspection: ArenaNext did not modify log.config or restart Hearthstone.",
                }))?
            );
        }
        LoggingCommand::Diff => {
            let preview = hs_log_config::preview_file_logging(config)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "operation": "diff",
                    "preview": preview,
                    "message": "Read-only preview: no directory, temporary file, backup, or log.config was written.",
                }))?
            );
        }
        LoggingCommand::Restore => {
            let backup = match (&options.logging_backup, options.logging_latest_backup) {
                (Some(path), false) => path.clone(),
                (None, true) => hs_log_config::latest_arena_next_backup(config)?
                    .context("no valid ArenaNext-created log.config backup was found beside the configured file")?,
                // Options::validate guarantees these branches are unreachable.
                _ => bail!("invalid logging restore arguments"),
            };
            let restore = hs_log_config::restore_from_backup(config, &backup)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "operation": "restore",
                    "restore": restore,
                    "message": "ArenaNext did not restart Hearthstone. Restart it yourself only if restore.hearthstoneRestartRequired is true.",
                }))?
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn print_inspection(options: &Options) -> Result<()> {
    let paths = hs_paths::discover_macos()?;
    let logging = paths
        .log_config
        .as_deref()
        .map(hs_log_config::inspect)
        .transpose()?;
    let rules = resolve_arena_rules(options)?;
    let cards = load_cards(options.cards.as_deref())?;
    let observer = open_observer_with_cards_and_rules(options, cards, rules.as_ref());
    let snapshot = observer.as_ref().ok().map(LiveObserver::snapshot);
    let attach_method = observer.as_ref().ok().map(LiveObserver::attach_method);
    let attach_diagnostics = observer
        .as_ref()
        .ok()
        .map(|observer| observer.attach_diagnostics().clone());
    let log_staleness = observer.as_ref().ok().map(|observer| {
        hs_observer::session_staleness(observer.session(), LOG_STALENESS_THRESHOLD)
    });
    let observer_error = observer.err().map(|error| error.to_string());
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "paths": paths,
            "logging": logging,
            "arenaRules": rules,
            "attachMethod": attach_method,
            "attachDiagnostics": attach_diagnostics,
            "logStaleness": log_staleness,
            "deck": snapshot.as_ref().map(|snapshot| &snapshot.deck_state),
            "observerError": observer_error,
        }))?
    );
    Ok(())
}

/// Read-only whole-system report intended to make integration breakage visible
/// before a user opens an overlay. It deliberately does not prompt for
/// Screen Recording or capture a frame; those actions remain explicit.
#[cfg(target_os = "macos")]
fn doctor(options: &Options) -> Result<()> {
    use arena_next_macos_capture::MacosWindowCapture;

    let paths = hs_paths::discover_macos()?;
    let logging = paths
        .log_config
        .as_deref()
        .map(hs_log_config::inspect)
        .transpose()?;
    let card_path = options
        .cards
        .clone()
        .unwrap_or_else(default_card_cache_path);
    let card_cache_exists = card_path.is_file();
    let checkpoint_path = default_observer_checkpoint_path();
    let cards = load_cards(options.cards.as_deref())?;
    let rules = resolve_arena_rules(options)?;
    let observer = open_observer_with_cards_and_rules(options, cards.clone(), rules.as_ref());
    let snapshot = observer.as_ref().ok().map(LiveObserver::snapshot);
    let attach_method = observer.as_ref().ok().map(LiveObserver::attach_method);
    let attach_diagnostics = observer
        .as_ref()
        .ok()
        .map(|observer| observer.attach_diagnostics().clone());
    let session = options.logs.clone().or(paths.latest_session.clone());
    let arena_log_age_ms = session
        .as_deref()
        .and_then(|directory| file_age_millis(&directory.join("Arena.log")));
    let log_staleness = observer
        .as_ref()
        .ok()
        .map(|observer| hs_observer::session_staleness(observer.session(), LOG_STALENESS_THRESHOLD))
        .or_else(|| {
            session
                .as_deref()
                .map(|directory| hs_observer::session_staleness(directory, LOG_STALENESS_THRESHOLD))
        });
    let observer_error = observer.err().map(|error| error.to_string());
    let missing_metadata_ids = snapshot
        .as_ref()
        .map(|snapshot| {
            snapshot
                .deck
                .iter()
                .filter_map(|card| match &card.resolution {
                    CardResolution::MissingMetadata { card_id } => {
                        Some((card_id.clone(), card.count))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let capture = MacosWindowCapture::new();
    let screen_permission = capture.screen_recording_permission();
    let hearthstone_windows = if screen_permission.is_granted() {
        capture
            .find_hearthstone_windows()
            .map(|windows| serde_json::json!({ "status": "checked", "count": windows.len() }))
            .unwrap_or_else(
                |error| serde_json::json!({ "status": "unavailable", "error": error.to_string() }),
            )
    } else {
        serde_json::json!({ "status": "permission_required" })
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "installation": {
                "status": if paths.install_dir.is_some() { "found" } else { "not_found" },
                "path": paths.install_dir,
            },
            "logSession": {
                "status": if session.is_some() { "found" } else { "not_found" },
                "path": session,
                "arenaLogAgeMs": arena_log_age_ms,
            },
            "logging": logging,
            "arenaRules": rules,
            "attachMethod": attach_method,
            "attachDiagnostics": attach_diagnostics,
            "logStaleness": log_staleness,
            "deck": snapshot.as_ref().map(|snapshot| serde_json::json!({
                "phase": snapshot.run.draft_phase,
                "stateOrigin": snapshot.run.state_origin,
                "draftHistory": snapshot.draft.history_status,
                "draftPhaseProgress": snapshot.draft.phase_progress_status,
                "expectedSlots": snapshot.deck_state.expected_slots,
                "observedSlots": snapshot.deck_state.observed_slots,
                "unobservedSlots": snapshot.deck_state.unobserved_slots,
                "completeness": snapshot.deck_state.completeness,
                "missingMetadata": missing_metadata_ids,
            })),
            "observerError": observer_error,
            "catalog": {
                "path": card_path,
                "exists": card_cache_exists,
                "source": cards.source,
                "version": cards.data_version,
                "updatedAt": cards.updated_at,
                "cardCount": cards.len(),
            },
            "stateCheckpoint": {
                "path": checkpoint_path,
                "exists": checkpoint_path.is_file(),
                "usedBy": "normal overlay startup validates this checkpoint first; checkpoint-free diagnostics report tail_snapshot or awaiting_snapshot. The replay command is the explicit full-history path.",
            },
            "capture": {
                "screenRecording": permission_json(screen_permission),
                "hearthstoneWindow": hearthstone_windows,
                "frame": "not_tested; run --capture-window explicitly",
                "fullscreenOverlay": "available",
            },
        }))?
    );
    Ok(())
}

/// Emit the deterministic facts that an optional AI explanation layer may
/// later consume. This command does not make a recommendation, call a model,
/// or send any deck data over the network.
#[cfg(target_os = "macos")]
fn analyze(options: &Options) -> Result<()> {
    use arena_scoring::RatingProvider;

    let cards = load_cards(options.cards.as_deref())?;
    let facts = options
        .analysis_facts
        .as_deref()
        .map(AnalysisFacts::load)
        .transpose()?
        .unwrap_or_else(AnalysisFacts::empty);
    let observer = open_observer_with_cards(options, cards.clone())?;
    let snapshot = observer.snapshot();
    let input = AnalysisInput {
        deck: snapshot
            .deck
            .iter()
            .map(|entry| hs_state::DeckCard {
                card_id: entry.card_id.clone(),
                count: entry.count,
            })
            .collect(),
        expected_slots: snapshot.deck_state.expected_slots,
    };
    let profile = analyze_deck(&input, &cards, &facts);
    let ratings = load_live_ratings(options)?;
    let offers = options
        .analysis_offers
        .iter()
        .map(|card_id| {
            let rating_evidence = ratings.as_ref().and_then(|provider| {
                provider.rating(card_id, snapshot.hero_class).map(|rating| {
                    serde_json::json!({
                        "composite": {
                            "provider": provider.metadata(),
                            "rating": rating,
                        },
                        "perSource": provider.provider_ratings(card_id, snapshot.hero_class),
                    })
                })
            });
            serde_json::json!({
                "cardId": card_id,
                "metadata": cards.resolve(card_id),
                "providerEvidence": rating_evidence,
                "analysis": analyze_offer(&input, card_id, &cards, &facts),
            })
        })
        .collect::<Vec<_>>();
    let output = serde_json::json!({
        "schemaVersion": 1,
        "mode": snapshot.mode,
        "heroClass": snapshot.hero_class,
        "arenaPhase": snapshot.run.draft_phase,
        "pickNumber": snapshot.draft.pick_number,
        "deckProfile": profile,
        "offers": offers,
        "dataPlanes": {
            "cardMetadata": {
                "source": cards.source,
                "version": cards.data_version,
                "updatedAt": cards.updated_at,
                "cardCount": cards.len(),
            },
            "analysisFacts": {
                "configured": options.analysis_facts.is_some(),
                "source": facts.source,
                "version": facts.data_version,
                "cardCount": facts.len(),
            },
            "ratings": ratings.as_ref().map(|provider| provider.metadata()),
        },
        "ai": {
            "called": false,
            "reason": "ArenaNext emits deterministic evidence only; an AI provider is not configured by this command.",
        },
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

#[cfg(target_os = "macos")]
fn explain_card(options: &Options, card_id: &str) -> Result<()> {
    let cards = load_cards(options.cards.as_deref())?;
    let resolution = cards.resolve(card_id);
    let observer = open_observer_full(options);
    let (observations, deck_count, observer_error) = match observer {
        Ok(observer) => {
            let observations = observer
                .card_observations(card_id)
                .map(|sources| sources.to_vec())
                .unwrap_or_default();
            let deck_count = observer
                .snapshot()
                .deck
                .iter()
                .find(|card| card.card_id == card_id)
                .map(|card| card.count);
            (observations, deck_count, None)
        }
        Err(error) => (Vec::new(), None, Some(error.to_string())),
    };
    let report = serde_json::json!({
        "cardId": card_id,
        "catalog": {
            "status": match &resolution {
                CardResolution::Resolved { .. } => "resolved",
                CardResolution::MissingMetadata { .. } => "missing",
                CardResolution::Unrevealed => "unrevealed",
                CardResolution::NonCardEntity { .. } => "non_card_entity",
                CardResolution::InvalidCardId { .. } => "invalid_id",
            },
            "source": cards.source,
            "version": cards.data_version,
            "updatedAt": cards.updated_at,
            "resolution": resolution,
        },
        "observedDeckCount": deck_count,
        "observedIn": observations,
        "arenaEligibility": "unknown (no Arena-pool manifest configured)",
        "artStatus": "unknown (no recognition-art catalog query configured)",
        "ratingStatus": "unavailable (no local rating provider supplied)",
        "observerError": observer_error,
    });
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let catalog_status = report["catalog"]["status"].as_str().unwrap_or("unknown");
        println!("Card ID: {card_id}");
        println!("Catalog status: {catalog_status}");
        println!("Catalog version: {}", cards.data_version);
        println!("Arena eligibility: unknown");
        println!("Art status: unknown");
        println!("Rating status: unavailable");
        if observations.is_empty() {
            println!("Observed in: no current-session record");
        } else {
            for source in observations {
                println!(
                    "Observed in: {} at byte offset {}",
                    source.component, source.byte_offset
                );
            }
        }
        if let Some(error) = observer_error {
            println!("Observer: {error}");
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn file_age_millis(path: &std::path::Path) -> Option<u128> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|age| age.as_millis())
}

#[cfg(target_os = "macos")]
fn enable_logging() -> Result<()> {
    let paths = hs_paths::discover_macos()?;
    let path = paths
        .log_config
        .as_deref()
        .context("could not determine a Hearthstone log.config path")?;
    let before = hs_log_config::inspect(path)?;
    let after = hs_log_config::enable_file_logging(path)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "before": before,
            "after": after,
            "message": "ArenaNext did not restart Hearthstone. Restart it yourself only if after.hearthstoneRestartRequired is true.",
        }))?
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn current_model(state: &Arc<RwLock<SharedObserverState>>) -> model::NativeOverlayModel {
    let Ok(state) = state.read() else {
        return model::NativeOverlayModel {
            title: "ArenaNext · observer unavailable".to_owned(),
            lines: vec!["Observer state lock was unavailable".to_owned()],
            deck_rows: Vec::new(),
        };
    };
    let mut model =
        model::from_snapshot_with_external_draft(&state.snapshot, state.external_draft.as_ref());
    if let LogStaleness::Stale { age_secs } = state.log_staleness {
        model.title = "ArenaNext · Hearthstone logs stalled".to_owned();
        let minutes = age_secs / 60;
        model.lines.push(format!(
            "Hearthstone log activity stopped {minutes} min ago; restart Hearthstone to restore deck/card detection"
        ));
    }
    if let Some(error) = &state.last_error {
        model.lines.push(format!("Log observer: {error}"));
    }
    if let Some(error) = &state.checkpoint_error {
        model
            .lines
            .push(format!("Local recovery checkpoint: {error}"));
    }
    model
}

#[cfg(target_os = "macos")]
fn appkit_model(model: model::NativeOverlayModel) -> arena_next_macos_overlay::OverlayModel {
    arena_next_macos_overlay::OverlayModel {
        title: model.title,
        lines: model.lines,
        deck_rows: model
            .deck_rows
            .into_iter()
            .map(|row| arena_next_macos_overlay::DeckRow {
                preview_image_path: default_card_art_path(&row.card_id)
                    .is_file()
                    .then(|| default_card_art_path(&row.card_id).display().to_string()),
                card_id: row.card_id,
                mana_cost: row.mana_cost,
                name: row.name,
                count: row.count,
            })
            .collect(),
    }
}

#[cfg(target_os = "macos")]
fn default_card_art_path(card_id: &str) -> PathBuf {
    default_app_data_dir()
        .join("card-renders")
        .join(format!("{card_id}.png"))
}

#[cfg(target_os = "macos")]
fn download_card_art(card_id: &str) -> Result<()> {
    if !card_id
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        bail!("invalid card ID for preview cache");
    }
    let path = default_card_art_path(card_id);
    if path.is_file() {
        return Ok(());
    }
    let url = format!("https://art.hearthstonejson.com/v1/render/latest/enUS/256x/{card_id}.png");
    let mut response = ureq::get(&url)
        .header(
            "User-Agent",
            concat!(
                "HearthAI/",
                env!("CARGO_PKG_VERSION"),
                " (hover card preview)"
            ),
        )
        .call()
        .with_context(|| format!("could not fetch {url}"))?;
    let bytes = response
        .body_mut()
        .read_to_vec()
        .context("could not read card preview response")?;
    let parent = path.parent().context("card preview path has no parent")?;
    fs::create_dir_all(parent)?;
    atomic_write(&path, &bytes)
}

#[cfg(target_os = "macos")]
fn popup_model(
    state: Option<&Arc<RwLock<SharedObserverState>>>,
    overlay_visible: bool,
    interaction_enabled: bool,
) -> arena_popup::StatusPopupModel {
    use arena_popup::{ArenaPhase, DeckSummary, HearthstoneStatus, Progress, RunStats};

    let Some(state) = state else {
        return arena_popup::StatusPopupModel {
            hearthstone: HearthstoneStatus::NotDetected,
            overlay_visible,
            interaction_enabled,
            ..Default::default()
        };
    };
    let Ok(state) = state.read() else {
        return arena_popup::StatusPopupModel {
            hearthstone: HearthstoneStatus::PermissionRequired,
            overlay_visible,
            interaction_enabled,
            ..Default::default()
        };
    };
    let snapshot = &state.snapshot;
    let arena_phase = match snapshot.run.draft_phase {
        hs_state::ArenaDraftPhase::Drafting => ArenaPhase::Draft,
        hs_state::ArenaDraftPhase::Redrafting => ArenaPhase::Redraft,
        hs_state::ArenaDraftPhase::ActiveDeck => ArenaPhase::ActiveDeck,
        hs_state::ArenaDraftPhase::Rewards => ArenaPhase::Rewards,
        _ => ArenaPhase::Unknown,
    };
    let active_run = matches!(snapshot.mode, hs_state::GameMode::Arena)
        && !matches!(arena_phase, ArenaPhase::Unknown | ArenaPhase::Rewards);
    let pick_progress = (snapshot.draft.pick_number > 0).then(|| Progress {
        current: snapshot.draft.pick_number,
        total: 30,
    });
    arena_popup::StatusPopupModel {
        hearthstone: if matches!(snapshot.mode, hs_state::GameMode::Unknown) {
            HearthstoneStatus::NotDetected
        } else {
            HearthstoneStatus::Running
        },
        deck: DeckSummary {
            hero: snapshot
                .hero_class
                .map(model::hero_label)
                .map(str::to_owned),
            observed: snapshot.deck_state.observed_slots,
            expected: snapshot.deck_state.expected_slots,
        },
        arena_phase,
        pick_progress,
        // Run results are not yet exposed by the log reducer; keep them
        // explicitly zero rather than inventing a record.
        run: RunStats::default(),
        active_run,
        overlay_visible,
        interaction_enabled,
    }
}

fn load_cards(path: Option<&std::path::Path>) -> Result<CardCache> {
    let path = path
        .map(PathBuf::from)
        .unwrap_or_else(default_card_cache_path);
    if path.is_file() {
        CardCache::load(&path)
    } else {
        Ok(CardCache::empty())
    }
}

fn default_card_cache_path() -> PathBuf {
    default_app_data_dir().join("card-data.json")
}

fn default_heartharena_ratings_path() -> PathBuf {
    default_app_data_dir().join("heartharena-ratings.json")
}

fn default_hsreplay_ratings_path() -> PathBuf {
    default_app_data_dir().join("hsreplay-ratings.json")
}

fn default_firestone_ratings_path() -> PathBuf {
    default_app_data_dir().join("firestone-ratings.json")
}

fn default_rating_cache_paths() -> Vec<PathBuf> {
    vec![
        default_heartharena_ratings_path(),
        default_hsreplay_ratings_path(),
        default_firestone_ratings_path(),
    ]
}

#[cfg(target_os = "macos")]
fn load_live_ratings(options: &Options) -> Result<Option<arena_scoring::CompositeRatingProvider>> {
    let providers = if let Some(path) = &options.ratings {
        vec![arena_scoring::LocalJsonRatingProvider::load(path)?]
    } else {
        let mut providers = Vec::new();
        for path in default_rating_cache_paths() {
            if path.is_file() {
                providers.push(arena_scoring::LocalJsonRatingProvider::load(path)?);
            }
        }
        providers
    };
    if providers.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        arena_scoring::CompositeRatingProvider::from_providers(providers)?,
    ))
}

fn default_observer_checkpoint_path() -> PathBuf {
    default_app_data_dir().join("observer-checkpoint.json")
}

fn default_app_data_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/Application Support/ArenaNext")
}

#[cfg(target_os = "macos")]
static LAST_APP_LOG_ENTRY: Mutex<Option<String>> = Mutex::new(None);

/// The client otherwise writes only to stdout/stderr, which go nowhere when
/// the app is launched from Finder. This file is what makes a later silent
/// failure diagnosable. Identical consecutive entries are coalesced so a
/// repeating retry loop cannot grow the file unboundedly.
#[cfg(target_os = "macos")]
fn default_app_log_path() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/Logs/ArenaNext/app.log")
}

#[cfg(target_os = "macos")]
fn app_log(entry: impl AsRef<str>) {
    let entry = entry.as_ref();
    let mut last = match LAST_APP_LOG_ENTRY.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if last.as_deref() == Some(entry) {
        return;
    }
    *last = Some(entry.to_owned());
    drop(last);
    let path = default_app_log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {entry}", Utc::now().to_rfc3339());
    }
}

#[cfg(target_os = "macos")]
fn staleness_label(staleness: &LogStaleness) -> String {
    match staleness {
        LogStaleness::Live => "live".to_owned(),
        LogStaleness::NoLogs => "no-logs".to_owned(),
        LogStaleness::Stale { age_secs } => format!("stale-{age_secs}s"),
    }
}

/// Record a recomputed staleness, logging the transition so the app log shows
/// when a live session's writers first stopped advancing.
#[cfg(target_os = "macos")]
fn record_staleness(latest: &mut SharedObserverState, staleness: LogStaleness) {
    if latest.log_staleness != staleness {
        app_log(format!(
            "log staleness {} -> {}",
            staleness_label(&latest.log_staleness),
            staleness_label(&staleness)
        ));
        latest.log_staleness = staleness;
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn parse_options(arguments: &[&str]) -> Result<Options> {
        Options::parse_from(arguments.iter().map(OsString::from))
    }

    #[test]
    fn logging_subcommands_require_an_explicit_safe_restore_selector() {
        let inspect = parse_options(&["logging", "inspect"]).unwrap();
        assert_eq!(inspect.logging_command, Some(LoggingCommand::Inspect));
        let diff = parse_options(&["logging", "diff"]).unwrap();
        assert_eq!(diff.logging_command, Some(LoggingCommand::Diff));

        assert!(parse_options(&["logging", "restore"]).is_err());
        assert!(parse_options(&["logging", "restore", "--latest", "--backup", "x"]).is_err());
        assert!(parse_options(&["logging", "inspect", "--latest"]).is_err());
        assert!(parse_options(&["logging", "restore", "--latest", "--once"]).is_err());

        let explicit = parse_options(&["logging", "restore", "--backup", "/tmp/backup"]).unwrap();
        assert_eq!(explicit.logging_command, Some(LoggingCommand::Restore));
        assert_eq!(explicit.logging_backup, Some(PathBuf::from("/tmp/backup")));
        assert!(!explicit.logging_latest_backup);

        let latest = parse_options(&["logging", "restore", "--latest"]).unwrap();
        assert_eq!(latest.logging_command, Some(LoggingCommand::Restore));
        assert!(latest.logging_backup.is_none());
        assert!(latest.logging_latest_backup);
    }

    #[test]
    fn analysis_command_keeps_facts_and_offers_local_and_explicit() {
        let options = parse_options(&[
            "analyze",
            "--logs",
            "fixture-session",
            "--analysis-facts",
            "facts.json",
            "--offer",
            "CARD_A",
            "--offer",
            "CARD_B",
            "--ratings",
            "ratings.json",
        ])
        .unwrap();
        assert!(options.analyze);
        assert_eq!(options.analysis_facts, Some(PathBuf::from("facts.json")));
        assert_eq!(options.analysis_offers, vec!["CARD_A", "CARD_B"]);
        assert_eq!(options.ratings, Some(PathBuf::from("ratings.json")));
        assert!(parse_options(&["--offer", "CARD_A"]).is_err());
        assert!(parse_options(&["analyze", "--demo"]).is_err());
    }

    fn unresolved_draft_snapshot() -> ObserverSnapshot {
        ObserverSnapshot {
            schema_version: hs_observer::OBSERVER_SNAPSHOT_SCHEMA_VERSION,
            state_schema_version: hs_state::SNAPSHOT_SCHEMA_VERSION,
            mode: hs_state::GameMode::Arena,
            hero_class: Some(hs_state::HeroClass::Mage),
            deck: Vec::new(),
            remaining_deck: Vec::new(),
            deck_state: hs_state::DeckState::default(),
            run: hs_state::ArenaRunState {
                draft_deck_id: Some("fixture-draft".to_owned()),
                deck_snapshot_complete: false,
                state_origin: hs_state::ArenaStateOrigin::Replay,
                draft_mode: Some("DRAFTING".to_owned()),
                draft_phase: hs_state::ArenaDraftPhase::Drafting,
            },
            draft: hs_state::DraftState {
                history_status: hs_state::DraftHistoryStatus::Complete,
                phase_progress_status: hs_state::DraftPhaseProgressStatus::Complete,
                pick_number: 1,
                ..hs_state::DraftState::default()
            },
            game: hs_state::GameState::default(),
        }
    }

    #[test]
    fn screen_recognition_tracks_normal_and_configured_redraft_pick_rounds() {
        let mut snapshot = unresolved_draft_snapshot();
        let first = unresolved_draft_key(&snapshot).expect("initial draft offer is eligible");
        assert_eq!(first.pick_number, 1);
        assert_eq!(first.phase, hs_state::ArenaDraftPhase::Drafting);

        // A tail-resynced normal Draft may not know its historical pick
        // number, but the current three-card offer is still safe to inspect.
        // Epoch zero is deliberately visible as unknown rather than invented
        // as pick one; a subsequent logged selection changes the epoch.
        snapshot.draft.phase_progress_status = hs_state::DraftPhaseProgressStatus::Unknown;
        snapshot.draft.pick_number = 0;
        snapshot.draft.phase_pick_count = 0;
        let resynced = unresolved_draft_key(&snapshot).expect("normal tail-resync is eligible");
        assert_eq!(resynced.pick_number, 0);
        assert_eq!(resynced.phase_pick_count, 0);
        snapshot.draft.phase_pick_count = 1;
        assert_ne!(resynced, unresolved_draft_key(&snapshot).unwrap());

        snapshot.draft.phase_progress_status = hs_state::DraftPhaseProgressStatus::Complete;
        snapshot.draft.pick_number = 1;
        snapshot.draft.phase_pick_count = 0;

        // A logged pick must start a new capture epoch rather than disabling
        // recognition for the rest of the draft.
        snapshot.draft.selected = Some("CS2_029".to_owned());
        snapshot.draft.phase_pick_count = 1;
        snapshot.draft.pick_number = 2;
        let second = unresolved_draft_key(&snapshot).expect("second draft offer is eligible");
        assert_eq!(second.pick_number, 2);
        assert_ne!(first, second);

        snapshot.run.draft_mode = Some("REDRAFTING".to_owned());
        snapshot.run.draft_phase = hs_state::ArenaDraftPhase::Redrafting;
        snapshot.draft.phase_pick_count = 0;
        snapshot.draft.pick_number = 1;
        snapshot.draft.redraft = hs_state::RedraftProgress {
            stage: hs_state::RedraftStage::PickingOffers,
            pick_rounds_required: Some(5),
            pick_progress_known: true,
            pick_rounds_completed: 0,
            discard_count_required: Some(5),
            discarded_card_ids: Vec::new(),
        };
        let redraft = unresolved_draft_key(&snapshot).expect("redraft offer is eligible");
        assert_eq!(redraft.pick_number, 1);
        assert_eq!(redraft.phase, hs_state::ArenaDraftPhase::Redrafting);

        snapshot.draft.phase_progress_status = hs_state::DraftPhaseProgressStatus::Unknown;
        snapshot.draft.pick_number = 0;
        snapshot.draft.redraft.pick_progress_known = false;
        assert!(unresolved_draft_key(&snapshot).is_none());
        snapshot.draft.phase_progress_status = hs_state::DraftPhaseProgressStatus::Complete;
        snapshot.draft.pick_number = 1;
        snapshot.draft.redraft.pick_progress_known = true;

        snapshot.draft.phase_pick_count = 5;
        snapshot.draft.pick_number = 6;
        snapshot.draft.redraft.pick_rounds_completed = 5;
        snapshot.draft.redraft.stage = hs_state::RedraftStage::AwaitingDiscardReview;
        assert!(unresolved_draft_key(&snapshot).is_none());
        assert!(
            redraft_capture_status(&snapshot)
                .expect("review boundary should be visible")
                .contains("choose 5 cards to discard")
        );

        snapshot.run.draft_mode = Some("ACTIVE_DRAFT_DECK".to_owned());
        snapshot.run.draft_phase = hs_state::ArenaDraftPhase::ActiveDeck;
        assert!(unresolved_draft_key(&snapshot).is_none());
    }

    #[test]
    fn explicit_catalog_and_ratings_load_without_capture_or_network_access() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let options = Options {
            draft_fingerprints: Some(
                root.join("fixtures/draft/fingerprint-catalog.schema-fixture.json"),
            ),
            ratings: Some(root.join("fixtures/ratings/sample-ratings.json")),
            ..Options::default()
        };
        let cards = CardCache::load(root.join("fixtures/card-data/sample-cards.json")).unwrap();
        assert!(
            DraftRecognitionWorker::from_options(&options, cards)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn local_rules_manifest_is_applied_to_fixture_replay() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let options = Options {
            logs: Some(root.join("fixtures/logs/sample-arena-session")),
            cards: Some(root.join("fixtures/card-data/sample-cards.json")),
            arena_rules: Some(root.join("fixtures/arena-rules/sample-season.json")),
            ..Options::default()
        };

        let snapshot = open_observer(&options).unwrap().snapshot();
        assert_eq!(snapshot.deck_state.expected_slots, Some(30));
        assert_eq!(snapshot.deck_state.observed_slots, 8);
        assert_eq!(snapshot.deck_state.unobserved_slots, Some(22));
        assert!(matches!(
            snapshot.deck_state.completeness,
            hs_state::DeckCompleteness::Partial {
                reason: hs_state::PartialDeckReason::UnobservedSlots
            }
        ));
    }
}
