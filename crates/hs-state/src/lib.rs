#![deny(unsafe_op_in_unsafe_fn)]

//! Deterministic, UI-independent Hearthstone Arena state.
//!
//! The reducer intentionally retains only facts that can be reconstructed from
//! logs. Presentation metadata (names, mana costs, ratings) is joined later by
//! the daemon, which prevents missing card metadata from becoming a fake card.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

// Version 7 adds the persisted `DraftOfferSource::Manual` variant. A newer
// checkpoint containing a manual correction must not be restored by an older
// binary that cannot deserialize that source; live attach safely resyncs it.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 10;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArenaSnapshot {
    pub schema_version: u32,
    pub mode: GameMode,
    pub hero_class: Option<HeroClass>,
    pub deck: Vec<DeckCard>,
    /// Health of the reconstructed deck, independent of card metadata.
    /// `deck` retains raw observed IDs/counts; metadata is joined only by the
    /// observer/UI boundary.
    pub deck_state: DeckState,
    pub run: ArenaRunState,
    pub draft: DraftState,
    pub game: GameState,
}

impl ArenaSnapshot {
    pub fn empty() -> Self {
        Self {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GameMode {
    Arena,
    #[default]
    Unknown,
    Other,
}

impl GameMode {
    pub fn from_log(value: &str) -> Self {
        let normalized = value.trim().to_ascii_uppercase();
        if normalized.contains("ARENA") {
            Self::Arena
        } else if normalized.is_empty() || normalized == "UNKNOWN" {
            Self::Unknown
        } else {
            Self::Other
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeroClass {
    DeathKnight,
    DemonHunter,
    Druid,
    Hunter,
    Mage,
    Paladin,
    Priest,
    Rogue,
    Shaman,
    Warlock,
    Warrior,
}

impl HeroClass {
    pub fn from_log(value: &str) -> Option<Self> {
        match value
            .trim()
            .to_ascii_uppercase()
            .replace([' ', '-'], "")
            .as_str()
        {
            "DEATHKNIGHT" => Some(Self::DeathKnight),
            "DEMONHUNTER" => Some(Self::DemonHunter),
            "DRUID" => Some(Self::Druid),
            "HUNTER" => Some(Self::Hunter),
            "MAGE" => Some(Self::Mage),
            "PALADIN" => Some(Self::Paladin),
            "PRIEST" => Some(Self::Priest),
            "ROGUE" => Some(Self::Rogue),
            "SHAMAN" => Some(Self::Shaman),
            "WARLOCK" => Some(Self::Warlock),
            "WARRIOR" => Some(Self::Warrior),
            _ => None,
        }
    }

    pub fn from_hero_card_id(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "HERO_01" => Some(Self::Warrior),
            "HERO_02" => Some(Self::Shaman),
            "HERO_03" => Some(Self::Rogue),
            "HERO_04" => Some(Self::Paladin),
            "HERO_05" => Some(Self::Hunter),
            "HERO_06" => Some(Self::Druid),
            "HERO_07" => Some(Self::Warlock),
            "HERO_08" => Some(Self::Mage),
            "HERO_09" => Some(Self::Priest),
            "HERO_10" => Some(Self::DemonHunter),
            "HERO_11" => Some(Self::DeathKnight),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckCard {
    pub card_id: String,
    pub count: u8,
}

/// Truthful reconstruction status for a deck. The expected slot count is
/// supplied by an Arena rules/season manifest or inferred from a completed
/// authoritative deck snapshot; it is never a hard-coded reducer constant.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckState {
    pub expected_slots: Option<u16>,
    pub observed_slots: u16,
    /// `None` means the game/rules have not told us the expected deck size,
    /// not that there are zero unknown cards.
    pub unobserved_slots: Option<u16>,
    pub completeness: DeckCompleteness,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum DeckCompleteness {
    #[default]
    Unknown,
    Complete,
    Partial {
        reason: PartialDeckReason,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PartialDeckReason {
    ExpectedSlotsUnknown,
    UnobservedSlots,
    ObservedSlotsExceedExpected,
    RedraftPendingDeckReview,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArenaRunState {
    pub draft_deck_id: Option<String>,
    pub deck_snapshot_complete: bool,
    /// Whether the current state was built by replaying run history or by a
    /// verified, authoritative current-deck resync. The latter is valid for
    /// deck tracking but intentionally does not imply past draft history.
    #[serde(default)]
    pub state_origin: ArenaStateOrigin,
    /// The exact client string is retained for diagnostics and forward
    /// compatibility. Product logic should use `draft_phase` instead.
    pub draft_mode: Option<String>,
    #[serde(default)]
    pub draft_phase: ArenaDraftPhase,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArenaStateOrigin {
    #[default]
    Replay,
    AuthoritativeResync,
}

/// The raw Arena lifecycle state reported by Hearthstone.
///
/// `REDRAFTING` starts a new sequence of normal draft *rounds* against an
/// already-built deck. It is not the later deck-review/discard screen; that
/// distinction lives in [`RedraftProgress`], because current logs do not
/// authoritatively mark the review screen.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArenaDraftPhase {
    #[default]
    Unknown,
    Drafting,
    Redrafting,
    ActiveDeck,
    Rewards,
    Other,
}

impl ArenaDraftPhase {
    pub fn from_log(value: &str) -> Self {
        match value.trim().to_ascii_uppercase().as_str() {
            "DRAFTING" => Self::Drafting,
            "REDRAFTING" => Self::Redrafting,
            "ACTIVE_DRAFT_DECK" => Self::ActiveDeck,
            "IN_REWARDS" => Self::Rewards,
            "" => Self::Unknown,
            _ => Self::Other,
        }
    }

    pub fn accepts_card_offers(&self) -> bool {
        matches!(self, Self::Drafting | Self::Redrafting)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Redrafting => "Redraft",
            Self::Drafting => "Draft",
            _ => "Draft",
        }
    }
}

/// A mode/season-provided Redraft contract.
///
/// The pure reducer does not assume a global "five". A caller obtains this
/// from a selected local Arena-rules manifest and applies it explicitly. This
/// lets the reducer stop treating the deck-review screen as another normal
/// three-card offer once the configured draft rounds are complete.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedraftPolicy {
    pub pick_rounds: u8,
    pub discard_count: u8,
}

impl RedraftPolicy {
    pub fn validate(self) -> Result<(), String> {
        if self.pick_rounds == 0 || self.discard_count == 0 {
            return Err("redraft pick rounds and discard count must both be positive".to_owned());
        }
        Ok(())
    }
}

/// Progress through the two distinct stages of a Redraft.
///
/// `AwaitingDiscardReview` deliberately means the configured count of normal
/// draft rounds is complete. It does *not* claim that the process has seen a
/// review screen: a later capture/manual adapter must assert that separately.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedraftProgress {
    #[serde(default)]
    pub stage: RedraftStage,
    /// Copied from the currently selected local rules policy, if one exists.
    pub pick_rounds_required: Option<u8>,
    /// Whether `pick_rounds_completed` is known from an observed Redraft
    /// start/pick sequence. A tail-resynced current deck may know it is in
    /// `REDRAFTING` without knowing whether the visible screen is a normal
    /// offer or the later discard review.
    #[serde(default)]
    pub pick_progress_known: bool,
    pub pick_rounds_completed: u8,
    /// Copied from the currently selected local rules policy, if one exists.
    pub discard_count_required: Option<u8>,
    /// Known review selections. They are distinct from draft additions and
    /// may contain repeated IDs when multiple copies are discarded.
    #[serde(default)]
    pub discarded_card_ids: Vec<String>,
}

impl RedraftProgress {
    /// Normal three-card capture is valid only while an explicit policy says
    /// there are still Redraft pick rounds remaining. With no policy we
    /// withhold capture rather than guessing where the discard review begins.
    pub fn accepts_normal_draft_capture(&self) -> bool {
        self.pick_progress_known
            && matches!(self.stage, RedraftStage::PickingOffers)
            && self
                .pick_rounds_required
                .is_some_and(|rounds| self.pick_rounds_completed < rounds)
    }

    fn awaits_authoritative_deck(&self) -> bool {
        !matches!(self.stage, RedraftStage::Inactive)
    }
}

/// Redraft's logical stage. It is intentionally separate from
/// [`ArenaDraftPhase`], which preserves the raw client `SetDraftMode` value.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RedraftStage {
    #[default]
    Inactive,
    /// The configured sequence of ordinary three-choice card-pick rounds.
    PickingOffers,
    /// All configured pick rounds are logged; do not reuse normal crop
    /// geometry for whatever comes next.
    AwaitingDiscardReview,
    /// A future calibrated capture/manual adapter explicitly observed the
    /// choose-cards-to-discard review screen.
    ReviewingDiscards,
    /// The review adapter observed submission. The fresh deck snapshot is
    /// still authoritative; local discard selections never mutate it.
    Complete,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftState {
    /// Completeness of all selections made before the current state. A
    /// complete authoritative deck snapshot can hydrate the *deck* without
    /// recreating the earlier choice sequence, so consumers must render this
    /// independently of deck completeness.
    #[serde(default)]
    pub history_status: DraftHistoryStatus,
    /// Whether `pick_number` and `phase_pick_count` are exact for the current
    /// phase. A tail resync leaves this unknown until a trustworthy phase
    /// boundary is observed.
    #[serde(default)]
    pub phase_progress_status: DraftPhaseProgressStatus,
    /// The one-based number of the next/current offer in the active drafting
    /// phase when `phase_progress_status` is `complete`; otherwise zero means
    /// the number is intentionally unknown. It resets when the client
    /// transitions from normal drafting to a Redraft rather than continuing
    /// from the original 30 picks.
    pub pick_number: u8,
    /// Completed picks in the current phase only. `selections` remains the
    /// complete run history; this counter is what drives draft-capture epochs.
    #[serde(default)]
    pub phase_pick_count: u8,
    /// Redraft additions and later discard-review selections are different
    /// actions. This state makes the boundary explicit instead of modelling a
    /// five-card review as a malformed draft offer.
    #[serde(default)]
    pub redraft: RedraftProgress,
    /// The current offer is deliberately variable-sized and typed. A normal
    /// card draft happens to show three choices today, but hero, package, and
    /// Redraft flows must not be forced through a `[Card; 3]` model.
    #[serde(default)]
    pub current_offer: Option<CurrentOffer>,
    /// Compatibility projection for existing card-only renderers. New logic
    /// should prefer `current_offer` so non-card/package items are retained.
    #[serde(default)]
    pub offers: Vec<DraftOffer>,
    pub selected: Option<String>,
    pub selections: Vec<String>,
}

impl DraftState {
    pub fn has_exact_phase_progress(&self) -> bool {
        self.phase_progress_status == DraftPhaseProgressStatus::Complete
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum DraftHistoryStatus {
    /// No run boundary or authoritative resync has established the history
    /// guarantee. This is conservative for older snapshots/log sessions.
    #[default]
    Unknown,
    Complete,
    Partial {
        reason: DraftHistoryPartialReason,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftHistoryPartialReason {
    AuthoritativeResync,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftPhaseProgressStatus {
    #[default]
    Unknown,
    Complete,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftOffer {
    pub card_id: String,
    pub confidence: f32,
    pub source: DraftOfferSource,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfferKind {
    Heroes,
    #[default]
    Cards,
    /// One normal three-card round during Redraft.
    RedraftPick,
    Package,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentOffer {
    pub kind: OfferKind,
    pub pick_number: u8,
    pub items: Vec<OfferItem>,
    pub confidence: f32,
    pub source: DraftOfferSource,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OfferItem {
    Card {
        card_id: String,
    },
    Hero {
        card_id: String,
    },
    HeroPower {
        card_id: String,
    },
    Package {
        key_card_id: String,
        contents: Vec<String>,
    },
    /// Recognition candidates belong to the presentation layer, not this
    /// deterministic log-derived state. The core records only that an item
    /// could not be identified authoritatively.
    Unknown {
        label: Option<String>,
    },
}

impl OfferItem {
    pub fn card_id(&self) -> Option<&str> {
        match self {
            Self::Card { card_id } | Self::Hero { card_id } | Self::HeroPower { card_id } => {
                Some(card_id)
            }
            Self::Package { key_card_id, .. } => Some(key_card_id),
            Self::Unknown { .. } => None,
        }
    }

    fn is_valid(&self) -> bool {
        match self {
            Self::Card { card_id } | Self::Hero { card_id } | Self::HeroPower { card_id } => {
                !card_id.trim().is_empty()
            }
            Self::Package {
                key_card_id,
                contents,
            } => !key_card_id.trim().is_empty() && !contents.is_empty(),
            Self::Unknown { .. } => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftOfferSource {
    Log,
    ScreenCapture,
    /// A user explicitly corrected the visible offer. This is presentation
    /// evidence only; it never adds a card to the deck or substitutes for the
    /// later `Client chooses` log record.
    Manual,
    Fixture,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameState {
    pub active: bool,
    pub result: Option<GameResult>,
    /// Known cards still in the local player's deck for the active game.
    /// This is a per-game projection; `ArenaSnapshot::deck` remains the
    /// authoritative constructed Arena deck and is never consumed by draws.
    #[serde(default)]
    pub remaining_deck: Vec<DeckCard>,
    #[serde(default)]
    pub initial_deck_size: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GameResult {
    Win,
    Loss,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum GameEvent {
    GameMode {
        raw_mode: String,
    },
    HeroClass {
        raw_class: String,
    },
    HeroCard {
        card_id: String,
    },
    DeckList {
        card_ids: Vec<String>,
    },
    DeckCard {
        entity_id: Option<u32>,
        card_id: String,
    },
    /// A new Arena run, as opposed to a later authoritative snapshot of an
    /// already-existing Arena deck. This is the only log boundary that
    /// intentionally resets the whole Arena-owned state, which makes it a
    /// safe bounded-tail replay anchor.
    ArenaRunStarted {
        draft_deck_id: String,
    },
    /// Synthetic, explicitly-authorized hydration from a verified completed
    /// Arena deck snapshot. This is for bounded cold attach only: it records
    /// current deck/phase facts but intentionally clears historical draft
    /// selections and marks their completeness as partial.
    ArenaAuthoritativeResync {
        draft_deck_id: String,
        hero_card_id: Option<String>,
        card_ids: Vec<String>,
        /// Exact current `SetDraftMode` text when the attach proof observed
        /// it; `None` means phase is unknown rather than guessed.
        draft_mode: Option<String>,
    },
    ArenaDeckSnapshotStarted {
        draft_deck_id: String,
        hero_card_id: Option<String>,
    },
    ArenaDeckSnapshotCard {
        card_id: String,
    },
    ArenaDeckSnapshotCompleted,
    ArenaDraftMode {
        mode: String,
    },
    CardRevealed {
        entity_id: u32,
        card_id: String,
        zone: Option<String>,
    },
    /// One known local-player entity crossed the friendly deck boundary.
    /// Entity identity makes repeated Zone.log render/update lines idempotent.
    FriendlyCardZoneChanged {
        entity_id: u32,
        card_id: String,
        from: String,
        to: String,
    },
    ArenaOffer {
        pick_number: Option<u8>,
        kind: OfferKind,
        items: Vec<OfferItem>,
        confidence: f32,
        source: DraftOfferSource,
    },
    ArenaPick {
        card_id: String,
    },
    /// An explicitly observed Redraft deck-review screen. Current log grammar
    /// does not produce this on its own; a future calibrated capture or manual
    /// correction layer must emit it deliberately.
    ArenaRedraftDiscardReviewStarted,
    /// One selected card in the distinct choose-cards-to-discard review.
    /// This never mutates the local deck count: the later authoritative deck
    /// snapshot remains the only truth for removals.
    ArenaRedraftDiscardSelected {
        card_id: String,
    },
    /// Replaces the in-progress Redraft discard selection with a user-edited
    /// list. This is intentionally distinct from the append-only observed
    /// selection event above: a player can change their mind before submit.
    /// The later authoritative deck snapshot remains the only source of
    /// truth for the resulting deck.
    ArenaRedraftDiscardSelectionsReplaced {
        card_ids: Vec<String>,
    },
    /// The review UI submitted its selection. It is still not an
    /// authoritative deck mutation.
    ArenaRedraftDiscardReviewCompleted,
    GameStarted,
    GameEnded {
        result: GameResult,
    },
}

/// A validated local interaction at the draft/review boundary.
///
/// This is deliberately separate from raw log events. A native UI may use it
/// to replace a weak screen-recognition result, mark an item unknown, or
/// record the user's Redraft discard choices while detection is unavailable.
/// None of these actions creates a draft pick or changes deck counts: the
/// Hearthstone log and a later authoritative deck snapshot retain that role.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum ManualDraftAction {
    /// Replace the currently visible offer with typed user-entered items.
    /// `OfferItem::Unknown` is the supported way to mark a slot unknown.
    ReplaceOffer {
        kind: OfferKind,
        items: Vec<OfferItem>,
    },
    /// Explicitly confirm that the separate Redraft discard-review surface is
    /// visible. This is permitted only after the configured ordinary pick
    /// rounds are known complete.
    BeginRedraftDiscardReview,
    /// Replace the pending discard list. It may contain fewer than the
    /// required number while the user edits it, but never more.
    SetRedraftDiscardSelections { card_ids: Vec<String> },
    /// Mark the review submitted after exactly the configured number of
    /// discards has been selected. This still does not mutate the deck.
    CompleteRedraftDiscardReview,
}

/// Rejection reason for a local draft interaction. Keeping this typed gives a
/// future native UI a truthful message instead of silently accepting an
/// action that would corrupt the draft/review boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManualDraftActionError {
    NotInDraftPhase {
        phase: ArenaDraftPhase,
    },
    InvalidOfferKindForPhase {
        phase: ArenaDraftPhase,
        kind: OfferKind,
    },
    RedraftOfferCaptureWithheld {
        stage: RedraftStage,
    },
    EmptyOffer,
    InvalidOfferItem {
        index: usize,
    },
    RedraftDiscardReviewNotReady {
        stage: RedraftStage,
    },
    RedraftDiscardCountUnknown,
    TooManyRedraftDiscards {
        maximum: u8,
        received: usize,
    },
    InvalidRedraftDiscardCard {
        index: usize,
    },
    IncompleteRedraftDiscardSelection {
        required: u8,
        selected: usize,
    },
}

impl std::fmt::Display for ManualDraftActionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInDraftPhase { phase } => {
                write!(formatter, "manual draft action is unavailable in {phase:?}")
            }
            Self::InvalidOfferKindForPhase { phase, kind } => write!(
                formatter,
                "manual {kind:?} offer is invalid while the Arena phase is {phase:?}"
            ),
            Self::RedraftOfferCaptureWithheld { stage } => write!(
                formatter,
                "manual Redraft offer correction is withheld during {stage:?}"
            ),
            Self::EmptyOffer => formatter.write_str("manual offer must contain at least one item"),
            Self::InvalidOfferItem { index } => {
                write!(formatter, "manual offer item {} is invalid", index + 1)
            }
            Self::RedraftDiscardReviewNotReady { stage } => write!(
                formatter,
                "Redraft discard review action is unavailable during {stage:?}"
            ),
            Self::RedraftDiscardCountUnknown => formatter
                .write_str("Redraft discard count is unavailable without a local rules policy"),
            Self::TooManyRedraftDiscards { maximum, received } => write!(
                formatter,
                "Redraft review allows at most {maximum} discards, received {received}"
            ),
            Self::InvalidRedraftDiscardCard { index } => {
                write!(formatter, "Redraft discard card {} is invalid", index + 1)
            }
            Self::IncompleteRedraftDiscardSelection { required, selected } => write!(
                formatter,
                "Redraft review requires {required} discards before submit, selected {selected}"
            ),
        }
    }
}

impl std::error::Error for ManualDraftActionError {}

/// Immutable provenance for one parsed log record. The reducer deduplicates
/// by this identity rather than by card ID: two different draft picks may
/// legitimately select the same card, while a repeated record at the same
/// source location must be idempotent.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSource {
    pub session_id: String,
    pub component: String,
    pub byte_offset: u64,
    pub line_hash: u64,
}

/// A reducer is deliberately replayable: applying the same events in the same
/// order always produces the same public snapshot.
#[derive(Clone, Debug, Default)]
pub struct ArenaReducer {
    snapshot: ArenaSnapshot,
    deck_counts: BTreeMap<String, u8>,
    snapshot_prior_counts: Option<BTreeMap<String, u8>>,
    snapshot_card_occurrences: BTreeMap<String, u8>,
    deck_entities: BTreeMap<u32, String>,
    game_deck_counts: BTreeMap<String, u8>,
    game_deck_entities: BTreeMap<u32, GameDeckEntity>,
    seen_event_sources: BTreeSet<EventSource>,
    card_observations: BTreeMap<String, Vec<EventSource>>,
    configured_expected_deck_slots: Option<u16>,
    inferred_expected_deck_slots: Option<u16>,
    configured_redraft_policy: Option<RedraftPolicy>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GameDeckEntity {
    card_id: String,
    in_deck: bool,
}

/// Persistable reducer internals required to continue reducing a log suffix
/// after a verified warm restart. A public snapshot on its own is not enough:
/// entity IDs prevent repeated card-reveal records from changing deck counts,
/// and the configured/inferred deck-size sources determine deck health.
///
/// `seen_event_sources` is deliberately not serialized. A validated observer
/// checkpoint resumes *after* a durable byte cursor, so no pre-checkpoint
/// physical line is replayed. Keeping an unbounded session-wide source set in
/// the checkpoint would make restart time and disk use grow with log history.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArenaReducerCheckpoint {
    snapshot: ArenaSnapshot,
    deck_counts: BTreeMap<String, u8>,
    #[serde(default)]
    snapshot_prior_counts: Option<BTreeMap<String, u8>>,
    #[serde(default)]
    snapshot_card_occurrences: BTreeMap<String, u8>,
    deck_entities: BTreeMap<u32, String>,
    #[serde(default)]
    game_deck_counts: BTreeMap<String, u8>,
    #[serde(default)]
    game_deck_entities: BTreeMap<u32, GameDeckEntity>,
    card_observations: BTreeMap<String, Vec<EventSource>>,
    configured_expected_deck_slots: Option<u16>,
    inferred_expected_deck_slots: Option<u16>,
    configured_redraft_policy: Option<RedraftPolicy>,
}

impl ArenaReducerCheckpoint {
    /// Bounded provenance retained for diagnostics. The observer validates
    /// these sources against the current log files before restoring them.
    pub fn observation_sources(&self) -> Vec<EventSource> {
        self.card_observations.values().flatten().cloned().collect()
    }
}

impl ArenaReducer {
    pub fn new() -> Self {
        Self {
            snapshot: ArenaSnapshot::empty(),
            ..Self::default()
        }
    }

    /// Construct a reducer with a caller-provided mode/season rule. The core
    /// deliberately has no implicit "30 cards" default.
    pub fn with_expected_deck_slots(expected_slots: Option<u16>) -> Self {
        let mut reducer = Self::new();
        reducer.configured_expected_deck_slots = expected_slots;
        reducer.sync_deck();
        reducer
    }

    pub fn set_expected_deck_slots(&mut self, expected_slots: Option<u16>) {
        self.configured_expected_deck_slots = expected_slots;
        self.sync_deck();
    }

    /// Apply the selected local mode's Redraft contract. The policy is kept
    /// outside the parser because logs do not define this product rule. A
    /// caller may update it after replay (for example after loading a rules
    /// manifest); existing Redraft progress is then recomputed safely.
    pub fn set_redraft_policy(&mut self, policy: Option<RedraftPolicy>) -> Result<(), String> {
        if let Some(policy) = policy {
            policy.validate()?;
        }
        self.configured_redraft_policy = policy;
        self.sync_redraft_policy();
        self.sync_deck();
        Ok(())
    }

    /// Apply one explicit, locally initiated draft/review action.
    ///
    /// This is the only public route for manual correction. It validates the
    /// current Arena phase before changing presentation evidence, derives the
    /// pick number from reducer state (rather than trusting the UI), and
    /// prevents Redraft's discard-review screen from being represented as a
    /// normal card offer. The action never changes deck counts.
    pub fn apply_manual_action(
        &mut self,
        action: ManualDraftAction,
    ) -> Result<(), ManualDraftActionError> {
        match action {
            ManualDraftAction::ReplaceOffer { kind, items } => self.apply_manual_offer(kind, items),
            ManualDraftAction::BeginRedraftDiscardReview => {
                let stage = self.snapshot.draft.redraft.stage.clone();
                if stage != RedraftStage::AwaitingDiscardReview {
                    return Err(ManualDraftActionError::RedraftDiscardReviewNotReady { stage });
                }
                self.apply(GameEvent::ArenaRedraftDiscardReviewStarted);
                Ok(())
            }
            ManualDraftAction::SetRedraftDiscardSelections { card_ids } => {
                let stage = self.snapshot.draft.redraft.stage.clone();
                if stage != RedraftStage::ReviewingDiscards {
                    return Err(ManualDraftActionError::RedraftDiscardReviewNotReady { stage });
                }
                let required = self
                    .snapshot
                    .draft
                    .redraft
                    .discard_count_required
                    .ok_or(ManualDraftActionError::RedraftDiscardCountUnknown)?;
                if card_ids.len() > usize::from(required) {
                    return Err(ManualDraftActionError::TooManyRedraftDiscards {
                        maximum: required,
                        received: card_ids.len(),
                    });
                }
                if let Some((index, _)) = card_ids
                    .iter()
                    .enumerate()
                    .find(|(_, card_id)| !is_real_card_id(card_id))
                {
                    return Err(ManualDraftActionError::InvalidRedraftDiscardCard { index });
                }
                self.apply(GameEvent::ArenaRedraftDiscardSelectionsReplaced { card_ids });
                Ok(())
            }
            ManualDraftAction::CompleteRedraftDiscardReview => {
                let stage = self.snapshot.draft.redraft.stage.clone();
                if stage != RedraftStage::ReviewingDiscards {
                    return Err(ManualDraftActionError::RedraftDiscardReviewNotReady { stage });
                }
                let required = self
                    .snapshot
                    .draft
                    .redraft
                    .discard_count_required
                    .ok_or(ManualDraftActionError::RedraftDiscardCountUnknown)?;
                let selected = self.snapshot.draft.redraft.discarded_card_ids.len();
                if selected != usize::from(required) {
                    return Err(ManualDraftActionError::IncompleteRedraftDiscardSelection {
                        required,
                        selected,
                    });
                }
                self.apply(GameEvent::ArenaRedraftDiscardReviewCompleted);
                Ok(())
            }
        }
    }

    fn apply_manual_offer(
        &mut self,
        kind: OfferKind,
        items: Vec<OfferItem>,
    ) -> Result<(), ManualDraftActionError> {
        if items.is_empty() {
            return Err(ManualDraftActionError::EmptyOffer);
        }
        if let Some((index, _)) = items.iter().enumerate().find(|(_, item)| !item.is_valid()) {
            return Err(ManualDraftActionError::InvalidOfferItem { index });
        }

        let phase = self.snapshot.run.draft_phase.clone();
        match phase {
            ArenaDraftPhase::Drafting if kind != OfferKind::RedraftPick => {}
            ArenaDraftPhase::Drafting => {
                return Err(ManualDraftActionError::InvalidOfferKindForPhase { phase, kind });
            }
            ArenaDraftPhase::Redrafting => {
                if !self.snapshot.draft.redraft.accepts_normal_draft_capture() {
                    return Err(ManualDraftActionError::RedraftOfferCaptureWithheld {
                        stage: self.snapshot.draft.redraft.stage.clone(),
                    });
                }
                if kind != OfferKind::RedraftPick {
                    return Err(ManualDraftActionError::InvalidOfferKindForPhase { phase, kind });
                }
            }
            _ => return Err(ManualDraftActionError::NotInDraftPhase { phase }),
        }

        let pick_number = self
            .snapshot
            .draft
            .has_exact_phase_progress()
            .then(|| {
                self.snapshot
                    .draft
                    .phase_pick_count
                    .saturating_add(1)
                    .max(1)
            })
            .unwrap_or(0);
        self.apply(GameEvent::ArenaOffer {
            pick_number: Some(pick_number),
            kind,
            items,
            confidence: 1.0,
            source: DraftOfferSource::Manual,
        });
        Ok(())
    }

    pub fn from_snapshot(snapshot: ArenaSnapshot) -> Self {
        let deck_counts = snapshot
            .deck
            .iter()
            .map(|card| (card.card_id.clone(), card.count))
            .collect();
        let inferred_expected_deck_slots = snapshot.deck_state.expected_slots;
        let game_deck_counts = snapshot
            .game
            .remaining_deck
            .iter()
            .map(|card| (card.card_id.clone(), card.count))
            .collect();
        Self {
            snapshot,
            deck_counts,
            snapshot_prior_counts: None,
            snapshot_card_occurrences: BTreeMap::new(),
            deck_entities: BTreeMap::new(),
            game_deck_counts,
            game_deck_entities: BTreeMap::new(),
            seen_event_sources: BTreeSet::new(),
            card_observations: BTreeMap::new(),
            configured_expected_deck_slots: None,
            inferred_expected_deck_slots,
            configured_redraft_policy: None,
        }
    }

    /// Capture the reducer state needed for a verified suffix-only resume.
    /// This checkpoint intentionally excludes the unbounded in-memory
    /// idempotency set; see [`ArenaReducerCheckpoint`] for why the observer's
    /// cursor/source validation makes that safe.
    pub fn checkpoint(&self) -> ArenaReducerCheckpoint {
        ArenaReducerCheckpoint {
            snapshot: self.snapshot.clone(),
            deck_counts: self.deck_counts.clone(),
            snapshot_prior_counts: self.snapshot_prior_counts.clone(),
            snapshot_card_occurrences: self.snapshot_card_occurrences.clone(),
            deck_entities: self.deck_entities.clone(),
            game_deck_counts: self.game_deck_counts.clone(),
            game_deck_entities: self.game_deck_entities.clone(),
            card_observations: self.card_observations.clone(),
            configured_expected_deck_slots: self.configured_expected_deck_slots,
            inferred_expected_deck_slots: self.inferred_expected_deck_slots,
            configured_redraft_policy: self.configured_redraft_policy,
        }
    }

    /// Restore a checkpoint only if its internal reducer invariants still
    /// hold. The observer performs the separate file/session/source checks
    /// before calling this method; malformed or self-contradictory state is
    /// rejected so callers can fall back to a full replay.
    pub fn from_checkpoint(checkpoint: ArenaReducerCheckpoint) -> Result<Self, String> {
        const MAX_SOURCES_PER_CARD: usize = 8;

        if checkpoint.snapshot.schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Err(format!(
                "unsupported state schema {}; expected {}",
                checkpoint.snapshot.schema_version, SNAPSHOT_SCHEMA_VERSION
            ));
        }
        if checkpoint
            .deck_counts
            .iter()
            .any(|(card_id, count)| !is_real_card_id(card_id) || *count == 0)
        {
            return Err("checkpoint contains an invalid deck count".to_owned());
        }
        if checkpoint
            .deck_entities
            .values()
            .any(|card_id| !is_real_card_id(card_id))
        {
            return Err("checkpoint contains an invalid deck entity".to_owned());
        }
        if checkpoint
            .card_observations
            .iter()
            .any(|(card_id, sources)| {
                !is_real_card_id(card_id)
                    || sources.len() > MAX_SOURCES_PER_CARD
                    || sources.iter().any(|source| {
                        source.session_id.trim().is_empty() || source.component.trim().is_empty()
                    })
            })
        {
            return Err("checkpoint contains invalid card provenance".to_owned());
        }
        if let Some(policy) = checkpoint.configured_redraft_policy {
            policy.validate()?;
        }

        let expected_snapshot = checkpoint.snapshot.clone();
        let mut reducer = Self {
            snapshot: checkpoint.snapshot,
            deck_counts: checkpoint.deck_counts,
            snapshot_prior_counts: checkpoint.snapshot_prior_counts,
            snapshot_card_occurrences: checkpoint.snapshot_card_occurrences,
            deck_entities: checkpoint.deck_entities,
            game_deck_counts: checkpoint.game_deck_counts,
            game_deck_entities: checkpoint.game_deck_entities,
            seen_event_sources: BTreeSet::new(),
            card_observations: checkpoint.card_observations,
            configured_expected_deck_slots: checkpoint.configured_expected_deck_slots,
            inferred_expected_deck_slots: checkpoint.inferred_expected_deck_slots,
            configured_redraft_policy: checkpoint.configured_redraft_policy,
        };
        reducer.sync_deck();
        reducer.sync_game_deck();
        if reducer.snapshot != expected_snapshot {
            return Err("checkpoint snapshot does not match reducer internals".to_owned());
        }
        Ok(reducer)
    }

    pub fn snapshot(&self) -> &ArenaSnapshot {
        &self.snapshot
    }

    pub fn into_snapshot(self) -> ArenaSnapshot {
        self.snapshot
    }

    pub fn apply_all<I>(&mut self, events: I)
    where
        I: IntoIterator<Item = GameEvent>,
    {
        for event in events {
            self.apply(event);
        }
    }

    /// Applies all events emitted from one physical log record exactly once.
    /// A record can produce more than one state event (for example a deck
    /// completion followed by a lifecycle transition), so the *line* rather
    /// than an individual card ID is the idempotency unit.
    pub fn apply_sourced_line(&mut self, source: EventSource, events: Vec<GameEvent>) {
        if events.is_empty() || !self.seen_event_sources.insert(source.clone()) {
            return;
        }
        for event in &events {
            self.record_card_observations(&source, event);
        }
        self.apply_all(events);
    }

    /// Bounded current-session provenance for a card ID. This is used by
    /// diagnostics such as `explain-card`; it is deliberately independent of
    /// metadata resolution and does not create catalog entries.
    pub fn card_observations(&self, card_id: &str) -> Option<&[EventSource]> {
        self.card_observations
            .get(card_id)
            .map(|sources| sources.as_slice())
    }

    pub fn apply(&mut self, event: GameEvent) {
        match event {
            GameEvent::GameMode { raw_mode } => self.snapshot.mode = GameMode::from_log(&raw_mode),
            GameEvent::HeroClass { raw_class } => {
                self.snapshot.hero_class = HeroClass::from_log(&raw_class)
            }
            GameEvent::HeroCard { card_id } => {
                self.snapshot.hero_class = HeroClass::from_hero_card_id(&card_id)
                    .or_else(|| HeroClass::from_log(&card_id));
            }
            GameEvent::DeckList { card_ids } => {
                self.deck_counts.clear();
                self.deck_entities.clear();
                for card_id in card_ids {
                    self.add_card(card_id);
                }
            }
            GameEvent::DeckCard { entity_id, card_id } => {
                if !is_real_card_id(&card_id) {
                    return;
                }
                if let Some(entity_id) = entity_id {
                    if self.deck_entities.contains_key(&entity_id) {
                        return;
                    }
                    self.deck_entities.insert(entity_id, card_id.clone());
                }
                self.add_card(card_id);
            }
            GameEvent::ArenaRunStarted { draft_deck_id } => {
                // `OnBegin - Got new draft deck` is a run boundary, not just
                // another view of the prior deck. Reset every Arena-owned
                // field so a tail replay beginning at this source has the
                // same state as a complete replay with older runs before it.
                self.snapshot = ArenaSnapshot::empty();
                self.deck_counts.clear();
                self.snapshot_prior_counts = None;
                self.snapshot_card_occurrences.clear();
                self.deck_entities.clear();
                self.game_deck_counts.clear();
                self.game_deck_entities.clear();
                self.card_observations.clear();
                self.inferred_expected_deck_slots = None;
                self.snapshot.mode = GameMode::Arena;
                self.snapshot.run.draft_deck_id = Some(draft_deck_id);
                self.snapshot.run.deck_snapshot_complete = false;
                self.snapshot.run.state_origin = ArenaStateOrigin::Replay;
                self.snapshot.draft.history_status = DraftHistoryStatus::Complete;
                self.snapshot.draft.phase_progress_status = DraftPhaseProgressStatus::Complete;
            }
            GameEvent::ArenaAuthoritativeResync {
                draft_deck_id,
                hero_card_id,
                card_ids,
                draft_mode,
            } => {
                // This event is intentionally narrower than replaying a run:
                // the caller has proven a complete current deck snapshot, not
                // every historic `Client chooses` record. Hydrate only facts
                // that snapshot/current-mode evidence establishes.
                self.snapshot = ArenaSnapshot::empty();
                self.deck_counts.clear();
                self.snapshot_prior_counts = None;
                self.snapshot_card_occurrences.clear();
                self.deck_entities.clear();
                self.game_deck_counts.clear();
                self.game_deck_entities.clear();
                self.card_observations.clear();
                self.inferred_expected_deck_slots = None;

                self.snapshot.mode = GameMode::Arena;
                self.snapshot.run.draft_deck_id = Some(draft_deck_id);
                self.snapshot.run.deck_snapshot_complete = true;
                self.snapshot.run.state_origin = ArenaStateOrigin::AuthoritativeResync;
                self.snapshot.draft.history_status = DraftHistoryStatus::Partial {
                    reason: DraftHistoryPartialReason::AuthoritativeResync,
                };
                self.snapshot.draft.phase_progress_status = DraftPhaseProgressStatus::Unknown;
                self.snapshot.draft.pick_number = 0;
                self.snapshot.draft.phase_pick_count = 0;

                if let Some(hero_card_id) = hero_card_id {
                    self.snapshot.hero_class = HeroClass::from_hero_card_id(&hero_card_id)
                        .or_else(|| HeroClass::from_log(&hero_card_id));
                }
                for card_id in card_ids {
                    self.add_card(card_id);
                }
                if self.configured_expected_deck_slots.is_none() {
                    self.inferred_expected_deck_slots = Some(self.observed_slot_count());
                }

                if let Some(mode) = draft_mode {
                    let phase = ArenaDraftPhase::from_log(&mode);
                    self.snapshot.run.draft_mode = Some(mode);
                    self.snapshot.run.draft_phase = phase.clone();
                    if phase == ArenaDraftPhase::Redrafting {
                        self.begin_redraft_with_unknown_progress();
                    }
                }
            }
            GameEvent::ArenaDeckSnapshotStarted {
                draft_deck_id,
                hero_card_id,
            } => {
                let preserve_expected_slots = self.snapshot.run.draft_deck_id.as_deref()
                    == Some(draft_deck_id.as_str())
                    && matches!(
                        self.snapshot.run.draft_phase,
                        ArenaDraftPhase::ActiveDeck | ArenaDraftPhase::Redrafting
                    );
                self.snapshot.mode = GameMode::Arena;
                self.snapshot_prior_counts =
                    preserve_expected_slots.then(|| self.deck_counts.clone());
                self.snapshot_card_occurrences.clear();
                self.deck_counts.clear();
                self.deck_entities.clear();
                self.card_observations.clear();
                if !preserve_expected_slots {
                    self.inferred_expected_deck_slots = None;
                }
                self.snapshot.run.draft_deck_id = Some(draft_deck_id);
                self.snapshot.run.deck_snapshot_complete = false;
                // A fresh authoritative deck snapshot supersedes any prior
                // Redraft additions/review selections. Do not locally apply
                // removals from a review screen; this snapshot is the source
                // of truth.
                self.snapshot.draft.redraft = RedraftProgress::default();
                if let Some(hero_card_id) = hero_card_id {
                    self.snapshot.hero_class = HeroClass::from_hero_card_id(&hero_card_id)
                        .or_else(|| HeroClass::from_log(&hero_card_id));
                }
            }
            GameEvent::ArenaDeckSnapshotCard { card_id } => {
                self.snapshot.mode = GameMode::Arena;
                if is_real_card_id(&card_id) {
                    // `Draft deck contains card` is a unique-card inventory:
                    // duplicate copies are collapsed to one log line. During
                    // a same-run refresh, preserve the last proven count for
                    // every listed ID instead of silently turning ×2 into ×1.
                    // Older fixtures/clients may repeat a line, so take the
                    // greater of the prior proven count and line occurrences.
                    let occurrences = self
                        .snapshot_card_occurrences
                        .entry(card_id.clone())
                        .or_default();
                    *occurrences = occurrences.saturating_add(1);
                    let prior_count = self
                        .snapshot_prior_counts
                        .as_ref()
                        .and_then(|counts| counts.get(&card_id))
                        .copied()
                        .unwrap_or(1);
                    self.deck_counts
                        .insert(card_id, prior_count.max(*occurrences));
                }
            }
            GameEvent::ArenaDeckSnapshotCompleted => {
                self.snapshot.run.deck_snapshot_complete = true;
                self.snapshot_prior_counts = None;
                self.snapshot_card_occurrences.clear();
                if self.configured_expected_deck_slots.is_none()
                    && self.inferred_expected_deck_slots.is_none()
                {
                    self.inferred_expected_deck_slots = Some(self.observed_slot_count());
                }
            }
            GameEvent::ArenaDraftMode { mode } => {
                self.snapshot.mode = GameMode::Arena;
                let phase = ArenaDraftPhase::from_log(&mode);
                let previous_phase = self.snapshot.run.draft_phase.clone();
                let entering_new_offer_phase =
                    phase.accepts_card_offers() && previous_phase != phase;
                self.snapshot.run.draft_mode = Some(mode);
                self.snapshot.run.draft_phase = phase.clone();
                if entering_new_offer_phase {
                    self.snapshot.draft.phase_pick_count = 0;
                    self.snapshot.draft.current_offer = None;
                    self.snapshot.draft.offers.clear();
                    self.snapshot.draft.selected = None;

                    // A Redraft changes an already-authoritative deck. Its
                    // first stage is a configured number of normal card-pick
                    // rounds; the later discard review is explicitly *not*
                    // another five-card offer.
                    if phase == ArenaDraftPhase::Redrafting {
                        self.snapshot.run.deck_snapshot_complete = false;
                        self.begin_redraft();
                    } else if phase == ArenaDraftPhase::Drafting {
                        // A normal new draft is not a continuation of the
                        // previous run's Redraft review state. The package
                        // snapshot is an authoritative starting baseline, not
                        // the completed draft; following picks extend it.
                        self.snapshot.run.deck_snapshot_complete = false;
                        self.snapshot.draft.redraft = RedraftProgress::default();
                    }
                    self.snapshot.draft.pick_number =
                        if self.snapshot.draft.has_exact_phase_progress() {
                            1
                        } else {
                            0
                        };
                }
                if phase == ArenaDraftPhase::ActiveDeck && self.observed_slot_count() > 0 {
                    if previous_phase == ArenaDraftPhase::Redrafting {
                        // Current clients finish the swap by returning
                        // directly to ACTIVE_DRAFT_DECK after the replacement
                        // picks. That mode line is the completion boundary;
                        // do not leave the UI waiting for a second discard
                        // review that may never be logged.
                        self.snapshot.draft.redraft.stage = RedraftStage::Complete;
                    }
                    // ACTIVE_DRAFT_DECK is the logged completion boundary.
                    // When no local season rule supplied a capacity, the
                    // fully replayed deck at this boundary is authoritative.
                    if self.configured_expected_deck_slots.is_none()
                        && previous_phase != ArenaDraftPhase::Redrafting
                    {
                        self.inferred_expected_deck_slots = Some(self.observed_slot_count());
                    }
                    self.snapshot.run.deck_snapshot_complete = self
                        .configured_expected_deck_slots
                        .or(self.inferred_expected_deck_slots)
                        .is_some_and(|expected| expected == self.observed_slot_count());
                }
            }
            GameEvent::CardRevealed {
                entity_id,
                card_id,
                zone,
            } => {
                if zone
                    .as_deref()
                    .is_some_and(|zone| zone.eq_ignore_ascii_case("DECK"))
                    && is_real_card_id(&card_id)
                    && !self.deck_entities.contains_key(&entity_id)
                {
                    self.deck_entities.insert(entity_id, card_id.clone());
                    self.add_card(card_id);
                }
            }
            GameEvent::FriendlyCardZoneChanged {
                entity_id,
                card_id,
                from,
                to,
            } => {
                if self.snapshot.game.active && is_real_card_id(&card_id) {
                    let from_deck = from.eq_ignore_ascii_case("DECK");
                    let to_deck = to.eq_ignore_ascii_case("DECK");
                    let prior = self.game_deck_entities.get(&entity_id).cloned();
                    if from_deck && !to_deck && prior.as_ref().is_none_or(|entity| entity.in_deck) {
                        self.remove_game_deck_card(&card_id);
                    } else if !from_deck
                        && to_deck
                        && prior.as_ref().is_none_or(|entity| !entity.in_deck)
                    {
                        *self.game_deck_counts.entry(card_id.clone()).or_default() += 1;
                    }
                    if from_deck != to_deck {
                        self.game_deck_entities.insert(
                            entity_id,
                            GameDeckEntity {
                                card_id,
                                in_deck: to_deck,
                            },
                        );
                    }
                }
            }
            GameEvent::ArenaOffer {
                pick_number,
                kind,
                items,
                confidence,
                source,
            } => {
                let items = items
                    .into_iter()
                    .filter(OfferItem::is_valid)
                    .collect::<Vec<_>>();
                if !items.is_empty() {
                    let pick_number = pick_number.unwrap_or_else(|| {
                        self.snapshot
                            .draft
                            .has_exact_phase_progress()
                            .then(|| {
                                self.snapshot
                                    .draft
                                    .phase_pick_count
                                    .saturating_add(1)
                                    .max(1)
                            })
                            .unwrap_or(0)
                    });
                    self.snapshot.draft.pick_number = pick_number;
                    self.snapshot.draft.offers = items
                        .iter()
                        .filter_map(|item| match item {
                            OfferItem::Card { card_id } => Some(DraftOffer {
                                card_id: card_id.clone(),
                                confidence,
                                source,
                            }),
                            _ => None,
                        })
                        .collect();
                    self.snapshot.draft.current_offer = Some(CurrentOffer {
                        kind,
                        pick_number,
                        items,
                        confidence,
                        source,
                    });
                    self.snapshot.draft.selected = None;
                }
            }
            GameEvent::ArenaPick { card_id } => {
                if !is_real_card_id(&card_id) {
                    return;
                }

                // Once the configured Redraft offer rounds are complete,
                // `Client chooses` is not safely attributable to an extra
                // card addition. Current logs do not identify the later
                // discard-review action, so preserve the authoritative deck
                // and wait for an explicit review event or a fresh snapshot.
                if self.snapshot.run.draft_phase == ArenaDraftPhase::Redrafting
                    && !matches!(
                        self.snapshot.draft.redraft.stage,
                        RedraftStage::PickingOffers
                    )
                {
                    return;
                }

                self.snapshot.mode = GameMode::Arena;
                self.snapshot.draft.selected = Some(card_id.clone());
                self.snapshot.draft.selections.push(card_id.clone());
                if self.snapshot.run.draft_phase.accepts_card_offers() {
                    self.snapshot.draft.phase_pick_count =
                        self.snapshot.draft.phase_pick_count.saturating_add(1);
                    self.snapshot.draft.pick_number = self
                        .snapshot
                        .draft
                        .has_exact_phase_progress()
                        .then(|| {
                            self.snapshot
                                .draft
                                .phase_pick_count
                                .saturating_add(1)
                                .max(1)
                        })
                        .unwrap_or(0);
                    self.snapshot.draft.current_offer = None;
                    self.snapshot.draft.offers.clear();
                    if self.snapshot.run.draft_phase == ArenaDraftPhase::Redrafting {
                        self.snapshot.draft.redraft.pick_rounds_completed =
                            self.snapshot.draft.phase_pick_count;
                        self.advance_redraft_after_pick();
                    }
                }
                // A new draft pick is a useful provisional deck update until a
                // later authoritative Arena snapshot replaces the deck. This
                // applies to Redraft additions: the client subsequently
                // discards/reorders cards in its deck editor, so we never
                // claim this provisional local count is the final deck.
                let post_resync_draft_pick = self.snapshot.run.state_origin
                    == ArenaStateOrigin::AuthoritativeResync
                    && self.snapshot.run.draft_phase.accepts_card_offers();
                if !self.snapshot.run.deck_snapshot_complete || post_resync_draft_pick {
                    self.add_card(card_id);
                    // The resynced snapshot was authoritative only up to its
                    // capture boundary. Once a later pick arrives, the local
                    // deck contains a provisional suffix until Hearthstone
                    // supplies the next complete snapshot.
                    if post_resync_draft_pick {
                        self.snapshot.run.deck_snapshot_complete = false;
                    }
                }
            }
            GameEvent::ArenaRedraftDiscardReviewStarted => {
                // Only an explicit adapter can assert the review surface. A
                // raw mode transition or exhausted pick counter is not proof
                // that this screen is currently visible.
                if self.snapshot.draft.redraft.stage == RedraftStage::AwaitingDiscardReview {
                    self.snapshot.draft.redraft.stage = RedraftStage::ReviewingDiscards;
                    self.snapshot.draft.current_offer = None;
                    self.snapshot.draft.offers.clear();
                    self.snapshot.draft.selected = None;
                }
            }
            GameEvent::ArenaRedraftDiscardSelected { card_id } => {
                if is_real_card_id(&card_id)
                    && self.snapshot.draft.redraft.stage == RedraftStage::ReviewingDiscards
                {
                    // Preserve duplicates: selecting two copies of the same
                    // card is legitimate. We intentionally do not decrement
                    // deck_counts, because the review can be changed before
                    // submission and logs/screen evidence are not the final
                    // deck authority.
                    let within_configured_limit = self
                        .snapshot
                        .draft
                        .redraft
                        .discard_count_required
                        .is_none_or(|required| {
                            self.snapshot.draft.redraft.discarded_card_ids.len()
                                < usize::from(required)
                        });
                    if within_configured_limit {
                        self.snapshot.draft.redraft.discarded_card_ids.push(card_id);
                    }
                }
            }
            GameEvent::ArenaRedraftDiscardSelectionsReplaced { card_ids } => {
                if self.snapshot.draft.redraft.stage == RedraftStage::ReviewingDiscards
                    && card_ids.iter().all(|card_id| is_real_card_id(card_id))
                    && self
                        .snapshot
                        .draft
                        .redraft
                        .discard_count_required
                        .is_some_and(|required| card_ids.len() <= usize::from(required))
                {
                    // This is a pending UI selection, not an asserted deck
                    // mutation. Replacing supports a user changing their
                    // mind before submitting the review.
                    self.snapshot.draft.redraft.discarded_card_ids = card_ids;
                }
            }
            GameEvent::ArenaRedraftDiscardReviewCompleted => {
                let selected = self.snapshot.draft.redraft.discarded_card_ids.len();
                if self.snapshot.draft.redraft.stage == RedraftStage::ReviewingDiscards
                    && self
                        .snapshot
                        .draft
                        .redraft
                        .discard_count_required
                        .is_some_and(|required| selected == usize::from(required))
                {
                    self.snapshot.draft.redraft.stage = RedraftStage::Complete;
                }
            }
            GameEvent::GameStarted => {
                if !self.snapshot.game.active {
                    self.snapshot.game.active = true;
                    self.snapshot.game.result = None;
                    self.game_deck_counts = self.deck_counts.clone();
                    self.game_deck_entities.clear();
                    self.snapshot.game.initial_deck_size = self.game_deck_slot_count();
                }
            }
            GameEvent::GameEnded { result } => {
                self.snapshot.game.active = false;
                self.snapshot.game.result = Some(result);
            }
        }
        self.sync_deck();
        self.sync_game_deck();
    }

    fn add_card(&mut self, card_id: String) {
        if is_real_card_id(&card_id) {
            *self.deck_counts.entry(card_id).or_default() += 1;
        }
    }

    fn remove_game_deck_card(&mut self, card_id: &str) {
        let Some(count) = self.game_deck_counts.get_mut(card_id) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.game_deck_counts.remove(card_id);
        }
    }

    fn sync_game_deck(&mut self) {
        self.snapshot.game.remaining_deck = self
            .game_deck_counts
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(card_id, count)| DeckCard {
                card_id: card_id.clone(),
                count: *count,
            })
            .collect();
    }

    fn game_deck_slot_count(&self) -> u16 {
        self.game_deck_counts.values().fold(0_u16, |total, count| {
            total.saturating_add(u16::from(*count))
        })
    }

    fn begin_redraft(&mut self) {
        self.snapshot.draft.phase_progress_status = DraftPhaseProgressStatus::Complete;
        self.begin_redraft_with_progress(true);
    }

    fn begin_redraft_with_unknown_progress(&mut self) {
        self.begin_redraft_with_progress(false);
    }

    fn begin_redraft_with_progress(&mut self, pick_progress_known: bool) {
        let inferred_replacements = self
            .configured_expected_deck_slots
            .or(self.inferred_expected_deck_slots)
            .and_then(|expected| expected.checked_sub(self.observed_slot_count()))
            .and_then(|missing| u8::try_from(missing).ok())
            .filter(|missing| *missing > 0);
        let (pick_rounds_required, discard_count_required) = self
            .configured_redraft_policy
            .map(|policy| (Some(policy.pick_rounds), Some(policy.discard_count)))
            .unwrap_or((inferred_replacements, inferred_replacements));
        self.snapshot.draft.redraft = RedraftProgress {
            stage: RedraftStage::PickingOffers,
            pick_rounds_required,
            pick_progress_known,
            pick_rounds_completed: 0,
            discard_count_required,
            discarded_card_ids: Vec::new(),
        };
    }

    fn advance_redraft_after_pick(&mut self) {
        let redraft = &mut self.snapshot.draft.redraft;
        if redraft.stage != RedraftStage::PickingOffers || !redraft.pick_progress_known {
            return;
        }
        if redraft
            .pick_rounds_required
            .is_some_and(|rounds| redraft.pick_rounds_completed >= rounds)
        {
            // This only closes normal-card capture. It does not claim the
            // discard review screen has been observed yet.
            redraft.stage = RedraftStage::AwaitingDiscardReview;
        }
    }

    fn sync_redraft_policy(&mut self) {
        let inferred_replacements = self
            .configured_expected_deck_slots
            .or(self.inferred_expected_deck_slots)
            .and_then(|expected| expected.checked_sub(self.observed_slot_count()))
            .and_then(|missing| u8::try_from(missing).ok())
            // On restore, the working deck already includes every Redraft
            // pick observed so far. Recover the original replacement count,
            // not merely the number still missing from the 30-card deck.
            .and_then(|missing| {
                if self.snapshot.run.draft_phase == ArenaDraftPhase::Redrafting
                    && self.snapshot.draft.redraft.pick_progress_known
                {
                    missing.checked_add(self.snapshot.draft.phase_pick_count)
                } else {
                    Some(missing)
                }
            })
            .filter(|missing| *missing > 0);
        let (pick_rounds_required, discard_count_required) = self
            .configured_redraft_policy
            .map(|policy| (Some(policy.pick_rounds), Some(policy.discard_count)))
            .unwrap_or((inferred_replacements, inferred_replacements));
        let redraft = &mut self.snapshot.draft.redraft;
        redraft.pick_rounds_required = pick_rounds_required;
        redraft.discard_count_required = discard_count_required;

        if redraft.pick_progress_known {
            redraft.pick_rounds_completed = self.snapshot.draft.phase_pick_count;
            if redraft
                .pick_rounds_required
                .is_some_and(|rounds| redraft.pick_rounds_completed >= rounds)
            {
                if redraft.stage == RedraftStage::PickingOffers {
                    redraft.stage = RedraftStage::AwaitingDiscardReview;
                    self.snapshot.draft.current_offer = None;
                    self.snapshot.draft.offers.clear();
                    self.snapshot.draft.selected = None;
                }
            } else if redraft.stage == RedraftStage::AwaitingDiscardReview {
                // A stale restore may have closed capture using only the
                // remaining-slot count. Re-open it when the recovered total
                // proves more Redraft rounds remain.
                redraft.stage = RedraftStage::PickingOffers;
            }
        }
    }

    fn sync_deck(&mut self) {
        self.snapshot.deck = self
            .deck_counts
            .iter()
            .map(|(card_id, count)| DeckCard {
                card_id: card_id.clone(),
                count: *count,
            })
            .collect();

        let observed_slots = self.observed_slot_count();
        let expected_slots = self
            .configured_expected_deck_slots
            .or(self.inferred_expected_deck_slots);
        let unobserved_slots =
            expected_slots.map(|expected| expected.saturating_sub(observed_slots));
        let completeness = if self.snapshot.draft.redraft.awaits_authoritative_deck()
            && !self.snapshot.run.deck_snapshot_complete
        {
            DeckCompleteness::Partial {
                reason: PartialDeckReason::RedraftPendingDeckReview,
            }
        } else {
            match expected_slots {
                None if observed_slots == 0 => DeckCompleteness::Unknown,
                None => DeckCompleteness::Partial {
                    reason: PartialDeckReason::ExpectedSlotsUnknown,
                },
                Some(expected) if observed_slots == expected => DeckCompleteness::Complete,
                Some(expected) if observed_slots < expected => DeckCompleteness::Partial {
                    reason: PartialDeckReason::UnobservedSlots,
                },
                Some(_) => DeckCompleteness::Partial {
                    reason: PartialDeckReason::ObservedSlotsExceedExpected,
                },
            }
        };
        self.snapshot.deck_state = DeckState {
            expected_slots,
            observed_slots,
            unobserved_slots,
            completeness,
        };
    }

    fn observed_slot_count(&self) -> u16 {
        self.deck_counts.values().fold(0_u16, |total, count| {
            total.saturating_add(u16::from(*count))
        })
    }

    fn record_card_observations(&mut self, source: &EventSource, event: &GameEvent) {
        const MAX_SOURCES_PER_CARD: usize = 8;

        let mut record = |card_id: &str| {
            if !is_real_card_id(card_id) {
                return;
            }
            let sources = self
                .card_observations
                .entry(card_id.to_owned())
                .or_default();
            if sources.len() == MAX_SOURCES_PER_CARD {
                sources.remove(0);
            }
            sources.push(source.clone());
        };

        match event {
            GameEvent::DeckList { card_ids } => {
                for card_id in card_ids {
                    record(card_id);
                }
            }
            GameEvent::DeckCard { card_id, .. }
            | GameEvent::ArenaDeckSnapshotCard { card_id }
            | GameEvent::ArenaPick { card_id }
            | GameEvent::ArenaRedraftDiscardSelected { card_id }
            | GameEvent::CardRevealed { card_id, .. } => record(card_id),
            GameEvent::FriendlyCardZoneChanged { card_id, .. } => record(card_id),
            GameEvent::ArenaOffer { items, .. } => {
                for item in items {
                    if let Some(card_id) = item.card_id() {
                        record(card_id);
                    }
                }
            }
            GameEvent::GameMode { .. }
            | GameEvent::HeroClass { .. }
            | GameEvent::HeroCard { .. }
            | GameEvent::ArenaRunStarted { .. }
            | GameEvent::ArenaAuthoritativeResync { .. }
            | GameEvent::ArenaDeckSnapshotStarted { .. }
            | GameEvent::ArenaDeckSnapshotCompleted
            | GameEvent::ArenaDraftMode { .. }
            | GameEvent::ArenaRedraftDiscardReviewStarted
            | GameEvent::ArenaRedraftDiscardSelectionsReplaced { .. }
            | GameEvent::ArenaRedraftDiscardReviewCompleted
            | GameEvent::GameStarted
            | GameEvent::GameEnded { .. } => {}
        }
    }
}

pub fn is_real_card_id(card_id: &str) -> bool {
    let trimmed = card_id.trim();
    let normalized = trimmed.to_ascii_uppercase();
    !trimmed.is_empty()
        && !normalized.starts_with("HERO_")
        && !matches!(
            normalized.as_str(),
            "0" | "UNKNOWN" | "INVALID" | "NONE" | ""
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_duplicate_entity_reveals_and_non_cards() {
        let mut reducer = ArenaReducer::new();
        reducer.apply(GameEvent::CardRevealed {
            entity_id: 7,
            card_id: "CS2_029".into(),
            zone: Some("DECK".into()),
        });
        reducer.apply(GameEvent::CardRevealed {
            entity_id: 7,
            card_id: "CS2_029".into(),
            zone: Some("DECK".into()),
        });
        reducer.apply(GameEvent::DeckCard {
            entity_id: Some(8),
            card_id: "UNKNOWN".into(),
        });
        reducer.apply(GameEvent::ArenaPick {
            card_id: "HERO_03".into(),
        });

        assert_eq!(
            reducer.snapshot().deck,
            vec![DeckCard {
                card_id: "CS2_029".into(),
                count: 1,
            }]
        );
    }

    #[test]
    fn active_game_tracks_draws_mulligans_and_duplicate_zone_lines() {
        let mut reducer = ArenaReducer::new();
        reducer.apply(GameEvent::DeckList {
            card_ids: vec!["CARD_A".into(), "CARD_A".into(), "CARD_B".into()],
        });
        reducer.apply(GameEvent::GameStarted);
        assert_eq!(reducer.snapshot().game.initial_deck_size, 3);

        let draw = GameEvent::FriendlyCardZoneChanged {
            entity_id: 41,
            card_id: "CARD_A".into(),
            from: "DECK".into(),
            to: "HAND".into(),
        };
        reducer.apply(draw.clone());
        reducer.apply(draw);
        assert_eq!(
            reducer.snapshot().game.remaining_deck,
            vec![
                DeckCard {
                    card_id: "CARD_A".into(),
                    count: 1,
                },
                DeckCard {
                    card_id: "CARD_B".into(),
                    count: 1,
                },
            ]
        );

        reducer.apply(GameEvent::FriendlyCardZoneChanged {
            entity_id: 41,
            card_id: "CARD_A".into(),
            from: "HAND".into(),
            to: "DECK".into(),
        });
        assert_eq!(
            reducer
                .snapshot()
                .game
                .remaining_deck
                .iter()
                .find(|card| card.card_id == "CARD_A")
                .map(|card| card.count),
            Some(2)
        );

        // A generated card shuffled into the deck is part of what remains.
        reducer.apply(GameEvent::FriendlyCardZoneChanged {
            entity_id: 99,
            card_id: "CARD_C".into(),
            from: "HAND".into(),
            to: "DECK".into(),
        });
        assert!(
            reducer
                .snapshot()
                .game
                .remaining_deck
                .iter()
                .any(|card| card.card_id == "CARD_C")
        );

        // The constructed Arena deck remains unchanged by gameplay.
        assert_eq!(
            reducer
                .snapshot()
                .deck
                .iter()
                .map(|card| card.count)
                .sum::<u8>(),
            3
        );
    }

    #[test]
    fn retains_typed_offers_without_treating_redraft_review_as_an_offer() {
        let mut reducer = ArenaReducer::new();
        reducer.apply(GameEvent::ArenaOffer {
            pick_number: Some(1),
            kind: OfferKind::Cards,
            items: vec![
                OfferItem::Card {
                    card_id: "A".into(),
                },
                OfferItem::Card {
                    card_id: "B".into(),
                },
            ],
            confidence: 1.0,
            source: DraftOfferSource::Log,
        });
        assert_eq!(
            reducer
                .snapshot()
                .draft
                .current_offer
                .as_ref()
                .expect("two-item offer should be retained")
                .items
                .len(),
            2
        );

        reducer.apply(GameEvent::ArenaOffer {
            pick_number: Some(1),
            kind: OfferKind::RedraftPick,
            items: vec![
                OfferItem::Card {
                    card_id: "A".into(),
                },
                OfferItem::Card {
                    card_id: "B".into(),
                },
                OfferItem::Card {
                    card_id: "C".into(),
                },
            ],
            confidence: 0.82,
            source: DraftOfferSource::ScreenCapture,
        });
        let current = reducer
            .snapshot()
            .draft
            .current_offer
            .as_ref()
            .expect("three-item redraft pick should be retained");
        assert_eq!(current.kind, OfferKind::RedraftPick);
        assert_eq!(current.items.len(), 3);
        assert_eq!(reducer.snapshot().draft.offers.len(), 3);
        assert_eq!(reducer.snapshot().draft.offers[0].confidence, 0.82);
    }

    #[test]
    fn manual_offer_correction_replaces_recognition_without_creating_a_pick() {
        let mut reducer = ArenaReducer::new();
        reducer.apply(GameEvent::ArenaRunStarted {
            draft_deck_id: "manual-offer".into(),
        });
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "DRAFTING".into(),
        });
        reducer.apply(GameEvent::ArenaOffer {
            pick_number: Some(1),
            kind: OfferKind::Cards,
            items: vec![
                OfferItem::Card {
                    card_id: "WRONG_A".into(),
                },
                OfferItem::Card {
                    card_id: "WRONG_B".into(),
                },
            ],
            confidence: 0.81,
            source: DraftOfferSource::ScreenCapture,
        });

        reducer
            .apply_manual_action(ManualDraftAction::ReplaceOffer {
                kind: OfferKind::Cards,
                items: vec![
                    OfferItem::Card {
                        card_id: "CORRECT_A".into(),
                    },
                    OfferItem::Unknown {
                        label: Some("second card not recognized".into()),
                    },
                    OfferItem::Card {
                        card_id: "CORRECT_C".into(),
                    },
                ],
            })
            .unwrap();

        let offer = reducer
            .snapshot()
            .draft
            .current_offer
            .as_ref()
            .expect("manual offer should replace the recognition result");
        assert_eq!(offer.source, DraftOfferSource::Manual);
        assert_eq!(offer.pick_number, 1);
        assert_eq!(offer.items.len(), 3);
        assert!(matches!(offer.items[1], OfferItem::Unknown { .. }));
        assert!(reducer.snapshot().draft.selections.is_empty());
        assert!(reducer.snapshot().deck.is_empty());

        assert_eq!(
            reducer.apply_manual_action(ManualDraftAction::ReplaceOffer {
                kind: OfferKind::RedraftPick,
                items: vec![OfferItem::Card {
                    card_id: "NOT_A_NORMAL_DRAFT_OFFER".into(),
                }],
            }),
            Err(ManualDraftActionError::InvalidOfferKindForPhase {
                phase: ArenaDraftPhase::Drafting,
                kind: OfferKind::RedraftPick,
            })
        );
    }

    #[test]
    fn manual_action_json_fixture_round_trips() {
        let fixture = r#"
        {
          "action": "replace_offer",
          "kind": "cards",
          "items": [
            { "kind": "card", "card_id": "EX1_277" },
            { "kind": "unknown", "label": "middle card unreadable" },
            { "kind": "card", "card_id": "CS2_029" }
          ]
        }
        "#;
        let action: ManualDraftAction = serde_json::from_str(fixture).unwrap();
        assert_eq!(
            action,
            ManualDraftAction::ReplaceOffer {
                kind: OfferKind::Cards,
                items: vec![
                    OfferItem::Card {
                        card_id: "EX1_277".into(),
                    },
                    OfferItem::Unknown {
                        label: Some("middle card unreadable".into()),
                    },
                    OfferItem::Card {
                        card_id: "CS2_029".into(),
                    },
                ],
            }
        );
        assert_eq!(
            serde_json::to_value(&action).unwrap(),
            serde_json::from_str::<serde_json::Value>(fixture).unwrap()
        );
    }

    #[test]
    fn a_pick_after_a_completed_snapshot_does_not_inflate_the_deck() {
        let mut reducer = ArenaReducer::new();
        reducer.apply(GameEvent::ArenaDeckSnapshotStarted {
            draft_deck_id: "42".into(),
            hero_card_id: Some("HERO_08".into()),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCard {
            card_id: "CS2_029".into(),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCompleted);
        reducer.apply(GameEvent::ArenaPick {
            card_id: "CS2_029".into(),
        });

        assert_eq!(reducer.snapshot().deck[0].count, 1);
        assert_eq!(
            reducer.snapshot().draft.selected.as_deref(),
            Some("CS2_029")
        );
    }

    #[test]
    fn drafting_and_redrafting_have_independent_offer_epochs() {
        let mut reducer = ArenaReducer::new();
        reducer.apply(GameEvent::ArenaRunStarted {
            draft_deck_id: "42".into(),
        });

        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "DRAFTING".into(),
        });
        assert_eq!(
            reducer.snapshot().run.draft_phase,
            ArenaDraftPhase::Drafting
        );
        assert_eq!(reducer.snapshot().draft.pick_number, 1);
        assert_eq!(reducer.snapshot().draft.phase_pick_count, 0);

        reducer.apply(GameEvent::ArenaPick {
            card_id: "CS2_029".into(),
        });
        assert_eq!(reducer.snapshot().draft.pick_number, 2);
        assert_eq!(reducer.snapshot().draft.phase_pick_count, 1);

        // Repeated lifecycle messages are not new drafts and must not erase
        // the current pick epoch.
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "DRAFTING".into(),
        });
        assert_eq!(reducer.snapshot().draft.phase_pick_count, 1);

        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "REDRAFTING".into(),
        });
        assert_eq!(
            reducer.snapshot().run.draft_phase,
            ArenaDraftPhase::Redrafting
        );
        assert_eq!(reducer.snapshot().draft.pick_number, 1);
        assert_eq!(reducer.snapshot().draft.phase_pick_count, 0);
        assert!(reducer.snapshot().draft.selected.is_none());

        reducer.apply(GameEvent::ArenaPick {
            card_id: "CS2_024".into(),
        });
        assert_eq!(reducer.snapshot().draft.pick_number, 2);
        assert_eq!(reducer.snapshot().draft.phase_pick_count, 1);
        assert_eq!(reducer.snapshot().draft.selections.len(), 2);
    }

    #[test]
    fn redraft_pick_is_provisional_until_a_fresh_deck_snapshot_arrives() {
        let mut reducer = ArenaReducer::new();
        reducer.apply(GameEvent::ArenaDeckSnapshotStarted {
            draft_deck_id: "42".into(),
            hero_card_id: Some("HERO_08".into()),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCard {
            card_id: "CS2_029".into(),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCompleted);
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "REDRAFTING".into(),
        });
        reducer.apply(GameEvent::ArenaPick {
            card_id: "CS2_024".into(),
        });

        assert!(!reducer.snapshot().run.deck_snapshot_complete);
        assert_eq!(
            reducer.snapshot().deck,
            vec![
                DeckCard {
                    card_id: "CS2_024".into(),
                    count: 1,
                },
                DeckCard {
                    card_id: "CS2_029".into(),
                    count: 1,
                },
            ]
        );
    }

    #[test]
    fn configured_redraft_is_five_three_card_rounds_then_a_distinct_review() {
        let mut reducer = ArenaReducer::new();
        reducer
            .set_redraft_policy(Some(RedraftPolicy {
                pick_rounds: 5,
                discard_count: 5,
            }))
            .unwrap();
        reducer.apply(GameEvent::ArenaDeckSnapshotStarted {
            draft_deck_id: "42".into(),
            hero_card_id: Some("HERO_08".into()),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCard {
            card_id: "BASE_01".into(),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCompleted);
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "REDRAFTING".into(),
        });

        assert_eq!(
            reducer.snapshot().draft.redraft,
            RedraftProgress {
                stage: RedraftStage::PickingOffers,
                pick_rounds_required: Some(5),
                pick_progress_known: true,
                pick_rounds_completed: 0,
                discard_count_required: Some(5),
                discarded_card_ids: Vec::new(),
            }
        );
        assert!(
            reducer
                .snapshot()
                .draft
                .redraft
                .accepts_normal_draft_capture()
        );

        for round in 1..=5 {
            reducer.apply(GameEvent::ArenaOffer {
                pick_number: Some(round),
                kind: OfferKind::RedraftPick,
                items: vec![
                    OfferItem::Card {
                        card_id: format!("OFFER_{round}_A"),
                    },
                    OfferItem::Card {
                        card_id: format!("OFFER_{round}_B"),
                    },
                    OfferItem::Card {
                        card_id: format!("OFFER_{round}_C"),
                    },
                ],
                confidence: 0.9,
                source: DraftOfferSource::Fixture,
            });
            assert_eq!(
                reducer
                    .snapshot()
                    .draft
                    .current_offer
                    .as_ref()
                    .expect("each redraft round has an offer")
                    .items
                    .len(),
                3
            );
            reducer.apply(GameEvent::ArenaPick {
                card_id: format!("PICK_{round}"),
            });
        }

        let after_picks = reducer.snapshot();
        assert_eq!(after_picks.draft.phase_pick_count, 5);
        assert_eq!(after_picks.draft.redraft.pick_rounds_completed, 5);
        assert_eq!(
            after_picks.draft.redraft.stage,
            RedraftStage::AwaitingDiscardReview
        );
        assert!(!after_picks.draft.redraft.accepts_normal_draft_capture());
        assert!(after_picks.draft.current_offer.is_none());
        assert_eq!(after_picks.deck_state.observed_slots, 6);

        // A sixth ambiguous `Client chooses` line must not be misread as an
        // extra draft card after the configured rounds are over.
        reducer.apply(GameEvent::ArenaPick {
            card_id: "NOT_A_SIXTH_PICK".into(),
        });
        assert_eq!(reducer.snapshot().draft.phase_pick_count, 5);
        assert_eq!(reducer.snapshot().draft.selections.len(), 5);
        assert_eq!(reducer.snapshot().deck_state.observed_slots, 6);

        // The deck-review screen is a separate state, not a five-card offer.
        reducer.apply(GameEvent::ArenaRedraftDiscardReviewStarted);
        reducer.apply(GameEvent::ArenaRedraftDiscardSelected {
            card_id: "BASE_01".into(),
        });
        reducer.apply(GameEvent::ArenaRedraftDiscardSelected {
            card_id: "BASE_01".into(),
        });
        assert_eq!(
            reducer.snapshot().draft.redraft.stage,
            RedraftStage::ReviewingDiscards
        );
        assert_eq!(
            reducer.snapshot().draft.redraft.discarded_card_ids,
            vec!["BASE_01", "BASE_01"]
        );
        assert!(reducer.snapshot().draft.current_offer.is_none());
        assert!(reducer.snapshot().draft.offers.is_empty());
        // Selection is provisional evidence only. It cannot locally remove a
        // card or claim an authoritative 30-card deck.
        assert_eq!(reducer.snapshot().deck_state.observed_slots, 6);

        // A review cannot be completed early. The native manual protocol
        // replaces the editable selection instead of appending fake picks.
        assert_eq!(
            reducer.apply_manual_action(ManualDraftAction::CompleteRedraftDiscardReview),
            Err(ManualDraftActionError::IncompleteRedraftDiscardSelection {
                required: 5,
                selected: 2,
            })
        );
        assert_eq!(
            reducer.apply_manual_action(ManualDraftAction::SetRedraftDiscardSelections {
                card_ids: vec![
                    "BASE_01".into(),
                    "PICK_1".into(),
                    "PICK_2".into(),
                    "PICK_3".into(),
                    "PICK_4".into(),
                    "PICK_5".into(),
                ],
            }),
            Err(ManualDraftActionError::TooManyRedraftDiscards {
                maximum: 5,
                received: 6,
            })
        );
        reducer
            .apply_manual_action(ManualDraftAction::SetRedraftDiscardSelections {
                card_ids: vec![
                    "BASE_01".into(),
                    "PICK_1".into(),
                    "PICK_2".into(),
                    "PICK_3".into(),
                    "PICK_4".into(),
                ],
            })
            .unwrap();
        reducer
            .apply_manual_action(ManualDraftAction::CompleteRedraftDiscardReview)
            .unwrap();
        assert_eq!(
            reducer.snapshot().draft.redraft.stage,
            RedraftStage::Complete
        );
    }

    #[test]
    fn restoring_mid_redraft_infers_total_rounds_not_only_remaining_slots() {
        let mut reducer = ArenaReducer::with_expected_deck_slots(Some(30));
        reducer.apply(GameEvent::ArenaDeckSnapshotStarted {
            draft_deck_id: "42".into(),
            hero_card_id: Some("HERO_08".into()),
        });
        for index in 0..25 {
            reducer.apply(GameEvent::ArenaDeckSnapshotCard {
                card_id: format!("BASE_{index:02}"),
            });
        }
        reducer.apply(GameEvent::ArenaDeckSnapshotCompleted);
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "REDRAFTING".into(),
        });
        for index in 0..4 {
            reducer.apply(GameEvent::ArenaPick {
                card_id: format!("PICK_{index:02}"),
            });
        }

        assert_eq!(reducer.snapshot().deck_state.observed_slots, 29);
        assert_eq!(
            reducer.snapshot().draft.redraft.pick_rounds_required,
            Some(5)
        );
        assert_eq!(
            reducer.snapshot().draft.redraft.stage,
            RedraftStage::PickingOffers
        );

        // Startup reapplies the selected rules policy. With no explicit
        // policy, inference must preserve all five rounds across the restore.
        reducer.set_redraft_policy(None).unwrap();

        assert_eq!(
            reducer.snapshot().draft.redraft.pick_rounds_required,
            Some(5)
        );
        assert_eq!(reducer.snapshot().draft.redraft.pick_rounds_completed, 4);
        assert_eq!(
            reducer.snapshot().draft.redraft.stage,
            RedraftStage::PickingOffers
        );

        reducer.apply(GameEvent::ArenaPick {
            card_id: "PICK_04".into(),
        });
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "ACTIVE_DRAFT_DECK".into(),
        });

        assert_eq!(reducer.snapshot().deck_state.observed_slots, 30);
        assert!(reducer.snapshot().run.deck_snapshot_complete);
        assert_eq!(
            reducer.snapshot().draft.redraft.stage,
            RedraftStage::Complete
        );
    }

    #[test]
    fn same_run_unique_card_snapshot_preserves_proven_duplicate_counts() {
        let mut reducer = ArenaReducer::with_expected_deck_slots(Some(3));
        reducer.apply(GameEvent::ArenaDeckSnapshotStarted {
            draft_deck_id: "42".into(),
            hero_card_id: Some("HERO_08".into()),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCard {
            card_id: "CARD_A".into(),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCard {
            card_id: "CARD_B".into(),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCompleted);
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "DRAFTING".into(),
        });
        reducer.apply(GameEvent::ArenaPick {
            card_id: "CARD_A".into(),
        });
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "ACTIVE_DRAFT_DECK".into(),
        });

        reducer.apply(GameEvent::ArenaDeckSnapshotStarted {
            draft_deck_id: "42".into(),
            hero_card_id: Some("HERO_08".into()),
        });
        // Hearthstone emits each distinct ID once, even for CARD_A ×2.
        reducer.apply(GameEvent::ArenaDeckSnapshotCard {
            card_id: "CARD_A".into(),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCard {
            card_id: "CARD_B".into(),
        });
        reducer.apply(GameEvent::ArenaDeckSnapshotCompleted);

        assert_eq!(
            reducer.snapshot().deck,
            vec![
                DeckCard {
                    card_id: "CARD_A".into(),
                    count: 2,
                },
                DeckCard {
                    card_id: "CARD_B".into(),
                    count: 1,
                },
            ]
        );
        assert_eq!(reducer.snapshot().deck_state.observed_slots, 3);
    }

    #[test]
    fn manual_redraft_review_requires_the_known_pick_boundary_and_exact_discard_count() {
        let mut reducer = ArenaReducer::new();
        reducer
            .set_redraft_policy(Some(RedraftPolicy {
                pick_rounds: 5,
                discard_count: 5,
            }))
            .unwrap();
        reducer.apply(GameEvent::ArenaRunStarted {
            draft_deck_id: "manual-redraft".into(),
        });
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "REDRAFTING".into(),
        });

        // The fifth normal pick is the boundary. Before it, neither a manual
        // review nor a manual card-offer correction can pretend the discard
        // screen is the normal three-card screen.
        assert_eq!(
            reducer.apply_manual_action(ManualDraftAction::BeginRedraftDiscardReview),
            Err(ManualDraftActionError::RedraftDiscardReviewNotReady {
                stage: RedraftStage::PickingOffers,
            })
        );
        for pick in 0..5 {
            reducer.apply(GameEvent::ArenaPick {
                card_id: format!("PICK_{pick}"),
            });
        }
        assert_eq!(
            reducer.snapshot().draft.redraft.stage,
            RedraftStage::AwaitingDiscardReview
        );
        assert_eq!(
            reducer.apply_manual_action(ManualDraftAction::ReplaceOffer {
                kind: OfferKind::RedraftPick,
                items: vec![OfferItem::Card {
                    card_id: "NOT_THE_REVIEW".into(),
                }],
            }),
            Err(ManualDraftActionError::RedraftOfferCaptureWithheld {
                stage: RedraftStage::AwaitingDiscardReview,
            })
        );

        reducer
            .apply_manual_action(ManualDraftAction::BeginRedraftDiscardReview)
            .unwrap();
        reducer
            .apply_manual_action(ManualDraftAction::SetRedraftDiscardSelections {
                card_ids: vec![
                    "PICK_0".into(),
                    "PICK_1".into(),
                    "PICK_2".into(),
                    "PICK_3".into(),
                    "PICK_4".into(),
                ],
            })
            .unwrap();
        reducer
            .apply_manual_action(ManualDraftAction::CompleteRedraftDiscardReview)
            .unwrap();

        assert_eq!(
            reducer.snapshot().draft.redraft.stage,
            RedraftStage::Complete
        );
        // Even after a local review submission the deck stays as it was until
        // Hearthstone emits a fresh authoritative deck snapshot.
        assert_eq!(reducer.snapshot().deck_state.observed_slots, 5);
    }

    #[test]
    fn redraft_capture_stays_withheld_until_a_local_policy_is_configured() {
        let mut reducer = ArenaReducer::new();
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "REDRAFTING".into(),
        });
        assert_eq!(
            reducer.snapshot().draft.redraft.stage,
            RedraftStage::PickingOffers
        );
        assert!(
            !reducer
                .snapshot()
                .draft
                .redraft
                .accepts_normal_draft_capture()
        );

        for index in 0..5 {
            reducer.apply(GameEvent::ArenaPick {
                card_id: format!("PICK_{index}"),
            });
        }
        // Config can arrive after a fixture/full replay. Recompute the
        // boundary without replaying or treating the review as a sixth offer.
        reducer
            .set_redraft_policy(Some(RedraftPolicy {
                pick_rounds: 5,
                discard_count: 5,
            }))
            .unwrap();
        assert_eq!(
            reducer.snapshot().draft.redraft.stage,
            RedraftStage::AwaitingDiscardReview
        );
        assert_eq!(reducer.snapshot().draft.redraft.pick_rounds_completed, 5);
    }

    #[test]
    fn authoritative_resync_hydrates_current_deck_without_faking_draft_history() {
        let mut reducer = ArenaReducer::new();
        reducer.apply(GameEvent::ArenaRunStarted {
            draft_deck_id: "old-run".into(),
        });
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "DRAFTING".into(),
        });
        reducer.apply(GameEvent::ArenaPick {
            card_id: "OLD_PICK".into(),
        });

        reducer.apply(GameEvent::ArenaAuthoritativeResync {
            draft_deck_id: "current-run".into(),
            hero_card_id: Some("HERO_08".into()),
            card_ids: vec!["CS2_029".into(), "CS2_029".into(), "CS2_024".into()],
            draft_mode: Some("DRAFTING".into()),
        });

        let snapshot = reducer.snapshot();
        assert_eq!(snapshot.run.draft_deck_id.as_deref(), Some("current-run"));
        assert_eq!(
            snapshot.run.state_origin,
            ArenaStateOrigin::AuthoritativeResync
        );
        assert!(snapshot.run.deck_snapshot_complete);
        assert_eq!(snapshot.hero_class, Some(HeroClass::Mage));
        assert_eq!(
            snapshot.draft.history_status,
            DraftHistoryStatus::Partial {
                reason: DraftHistoryPartialReason::AuthoritativeResync,
            }
        );
        assert_eq!(
            snapshot.draft.phase_progress_status,
            DraftPhaseProgressStatus::Unknown
        );
        assert_eq!(snapshot.draft.pick_number, 0);
        assert_eq!(snapshot.draft.phase_pick_count, 0);
        assert!(snapshot.draft.selections.is_empty());
        assert!(snapshot.draft.current_offer.is_none());
        assert_eq!(
            snapshot.deck,
            vec![
                DeckCard {
                    card_id: "CS2_024".into(),
                    count: 1,
                },
                DeckCard {
                    card_id: "CS2_029".into(),
                    count: 2,
                },
            ]
        );

        // A later live draft pick is retained as a suffix observation and
        // makes the deck provisional, but it cannot upgrade pre-resync
        // history or invent an absolute pick number.
        reducer.apply(GameEvent::ArenaPick {
            card_id: "CS2_033".into(),
        });
        let after_pick = reducer.snapshot();
        assert_eq!(after_pick.draft.selections, vec!["CS2_033"]);
        assert_eq!(after_pick.draft.pick_number, 0);
        assert_eq!(after_pick.draft.phase_pick_count, 1);
        assert!(!after_pick.run.deck_snapshot_complete);
        assert_eq!(after_pick.deck_state.observed_slots, 4);
        assert_eq!(
            after_pick.draft.history_status,
            DraftHistoryStatus::Partial {
                reason: DraftHistoryPartialReason::AuthoritativeResync,
            }
        );
    }

    #[test]
    fn resynced_redraft_withholds_capture_until_progress_is_proven() {
        let mut reducer = ArenaReducer::new();
        reducer
            .set_redraft_policy(Some(RedraftPolicy {
                pick_rounds: 5,
                discard_count: 5,
            }))
            .unwrap();
        reducer.apply(GameEvent::ArenaAuthoritativeResync {
            draft_deck_id: "current-run".into(),
            hero_card_id: Some("HERO_08".into()),
            card_ids: vec!["CS2_029".into()],
            draft_mode: Some("REDRAFTING".into()),
        });

        let snapshot = reducer.snapshot();
        assert_eq!(snapshot.run.draft_phase, ArenaDraftPhase::Redrafting);
        assert!(snapshot.run.deck_snapshot_complete);
        assert_eq!(snapshot.draft.redraft.pick_rounds_required, Some(5));
        assert!(!snapshot.draft.redraft.pick_progress_known);
        assert!(!snapshot.draft.redraft.accepts_normal_draft_capture());
        assert_eq!(snapshot.draft.pick_number, 0);

        // A later line can still extend the provisional deck suffix, but its
        // count must not be treated as the absolute Redraft round number and
        // cannot enable screen capture.
        reducer.apply(GameEvent::ArenaPick {
            card_id: "CS2_024".into(),
        });
        let after_pick = reducer.snapshot();
        assert_eq!(after_pick.deck_state.observed_slots, 2);
        assert!(!after_pick.run.deck_snapshot_complete);
        assert_eq!(after_pick.draft.pick_number, 0);
        assert_eq!(after_pick.draft.phase_pick_count, 1);
        assert!(!after_pick.draft.redraft.pick_progress_known);
        assert!(!after_pick.draft.redraft.accepts_normal_draft_capture());
    }

    #[test]
    fn sourced_records_are_idempotent_but_same_card_at_new_offset_is_a_pick() {
        let mut reducer = ArenaReducer::new();
        reducer.apply(GameEvent::ArenaDraftMode {
            mode: "DRAFTING".into(),
        });

        let first = EventSource {
            session_id: "session-a".into(),
            component: "Arena.log".into(),
            byte_offset: 128,
            line_hash: 1,
        };
        let second_same_card = EventSource {
            byte_offset: 256,
            line_hash: 2,
            ..first.clone()
        };
        // Insert a lexicographically later unrelated source first. This
        // catches an accidental "last item in the set" provenance lookup.
        reducer.apply_sourced_line(
            EventSource {
                component: "Zone.log".into(),
                byte_offset: 1,
                line_hash: 99,
                ..first.clone()
            },
            vec![GameEvent::GameMode {
                raw_mode: "ARENA".into(),
            }],
        );
        let pick = || {
            vec![GameEvent::ArenaPick {
                card_id: "CS2_029".into(),
            }]
        };

        reducer.apply_sourced_line(first.clone(), pick());
        reducer.apply_sourced_line(first, pick());
        reducer.apply_sourced_line(second_same_card, pick());

        assert_eq!(
            reducer.snapshot().draft.selections,
            vec!["CS2_029".to_owned(), "CS2_029".to_owned()]
        );
        assert_eq!(reducer.snapshot().draft.phase_pick_count, 2);
        assert_eq!(
            reducer.snapshot().deck,
            vec![DeckCard {
                card_id: "CS2_029".into(),
                count: 2,
            }]
        );
        assert_eq!(
            reducer
                .card_observations("CS2_029")
                .expect("card provenance should be retained")
                .iter()
                .map(|source| source.byte_offset)
                .collect::<Vec<_>>(),
            vec![128, 256]
        );
        assert!(
            reducer
                .card_observations("CS2_029")
                .expect("card provenance should be retained")
                .iter()
                .all(|source| source.component == "Arena.log")
        );
    }

    #[test]
    fn partial_deck_slots_are_explicit_without_a_hard_coded_arena_size() {
        let mut reducer = ArenaReducer::with_expected_deck_slots(Some(30));
        reducer.apply(GameEvent::DeckList {
            card_ids: (0..25).map(|index| format!("CARD_{index:02}")).collect(),
        });

        assert_eq!(reducer.snapshot().deck_state.expected_slots, Some(30));
        assert_eq!(reducer.snapshot().deck_state.observed_slots, 25);
        assert_eq!(reducer.snapshot().deck_state.unobserved_slots, Some(5));
        assert_eq!(
            reducer.snapshot().deck_state.completeness,
            DeckCompleteness::Partial {
                reason: PartialDeckReason::UnobservedSlots,
            }
        );
    }
}
