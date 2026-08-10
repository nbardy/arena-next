//! Pure native-overlay presentation model.
//!
//! This is deliberately not AppKit code. The macOS host consumes its title and
//! lines, but fixture tests can assert the exact user-visible state without a
//! running window server.

use hs_card_data::CardResolution;
use hs_observer::{ObserverSnapshot, ResolvedDeckCard};

const MAX_DECK_LINES: usize = 12;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NativeOverlayModel {
    pub title: String,
    pub lines: Vec<String>,
}

/// Presentation produced by the app-owned screen-recognition worker.
///
/// It intentionally contains only already-resolved text. The pure model does
/// not import capture APIs, a rating provider, or card metadata; that keeps
/// its fixture tests independent of macOS and prevents a stale visual result
/// from mutating the deterministic log reducer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExternalDraftModel {
    pub pick_number: u8,
    pub lines: Vec<String>,
}

pub fn from_snapshot(snapshot: &ObserverSnapshot) -> NativeOverlayModel {
    from_snapshot_with_external_draft(snapshot, None)
}

pub fn from_snapshot_with_external_draft(
    snapshot: &ObserverSnapshot,
    external_draft: Option<&ExternalDraftModel>,
) -> NativeOverlayModel {
    let mode = match snapshot.mode {
        hs_state::GameMode::Arena => "Arena",
        hs_state::GameMode::Other => "Hearthstone",
        hs_state::GameMode::Unknown => "Waiting for Hearthstone",
    };
    let hero = snapshot
        .hero_class
        .map(hero_label)
        .unwrap_or("Unknown hero");
    let deck_total = usize::from(snapshot.deck_state.observed_slots);
    let unresolved_redraft = matches!(
        snapshot.deck_state.completeness,
        hs_state::DeckCompleteness::Partial {
            reason: hs_state::PartialDeckReason::RedraftPendingDeckReview
        }
    );
    let active_redraft = snapshot.run.draft_phase == hs_state::ArenaDraftPhase::Redrafting
        && snapshot.draft.redraft.stage == hs_state::RedraftStage::PickingOffers
        && snapshot.draft.redraft.pick_progress_known;
    let mut lines = vec![if unresolved_redraft {
        format!("{hero} · Redraft reconciliation required")
    } else if active_redraft {
        let completed = usize::from(snapshot.draft.redraft.pick_rounds_completed);
        let retained = deck_total.saturating_sub(completed);
        match snapshot.draft.redraft.pick_rounds_required {
            Some(required) => {
                format!("{hero} · {retained} retained + {completed}/{required} replacements")
            }
            None => format!("{hero} · {retained} retained + {completed} replacements"),
        }
    } else {
        match snapshot.deck_state.expected_slots {
            Some(expected) => format!("{hero} · {deck_total}/{expected} cards observed"),
            None => format!("{hero} · {deck_total} cards observed"),
        }
    }];

    if let Some(unobserved) = snapshot
        .deck_state
        .unobserved_slots
        .filter(|slots| *slots > 0)
    {
        if active_redraft {
            lines.push(format!(
                "{unobserved} Redraft replacement{} remaining",
                if unobserved == 1 { "" } else { "s" }
            ));
        } else {
            lines.push(format!("{unobserved} deck slots not yet observed"));
        }
    }
    if unresolved_redraft {
        let replacements = snapshot.draft.redraft.pick_rounds_completed;
        lines.push(format!(
            "{replacements} replacements captured; {replacements} removals unknown"
        ));
        lines.push("Deck-adjusted scoring paused until Fix Deck completes".to_owned());
    }
    if matches!(
        snapshot.draft.history_status,
        hs_state::DraftHistoryStatus::Partial {
            reason: hs_state::DraftHistoryPartialReason::AuthoritativeResync
        }
    ) {
        lines.push("Current deck resynced; earlier draft history is unavailable".to_owned());
    }
    if !snapshot.draft.has_exact_phase_progress()
        && snapshot.run.draft_phase == hs_state::ArenaDraftPhase::Drafting
    {
        lines.push("Current draft pick number is unknown; detecting the visible offer".to_owned());
    }
    if snapshot.run.draft_phase == hs_state::ArenaDraftPhase::Redrafting
        && external_draft.is_none()
        && let Some(redraft_status) = redraft_status_line(&snapshot.draft.redraft)
    {
        lines.push(redraft_status);
    }

    if snapshot.game.active {
        lines.push("Game in progress".to_owned());
    }

    if let Some(external_draft) = external_draft {
        if external_draft.pick_number == 0 {
            lines.push(format!(
                "{} pick progress unknown",
                snapshot.run.draft_phase.label()
            ));
        } else {
            lines.push(format!(
                "{} pick {}",
                snapshot.run.draft_phase.label(),
                external_draft.pick_number
            ));
        }
        lines.extend(external_draft.lines.iter().cloned());
    } else if let Some(offer) = &snapshot.draft.current_offer {
        lines.push(format!(
            "{} pick {}",
            offer_kind_label(offer.kind),
            offer.pick_number
        ));
        lines.extend(offer.items.iter().map(offer_item_line));
        if let Some(selected) = &snapshot.draft.selected {
            lines.push(format!("Picked: {selected}"));
        }
    } else if !snapshot.draft.offers.is_empty() {
        lines.push(format!(
            "{} pick {}",
            snapshot.run.draft_phase.label(),
            snapshot.draft.pick_number
        ));
        lines.extend(snapshot.draft.offers.iter().map(|offer| {
            let confidence = (offer.confidence * 100.0).round().clamp(0.0, 100.0) as u8;
            format!("{} · {confidence}%", offer.card_id)
        }));
        if let Some(selected) = &snapshot.draft.selected {
            lines.push(format!("Picked: {selected}"));
        }
    } else if unresolved_redraft {
        lines.push("The provisional card union is not the current deck".to_owned());
    } else if snapshot.deck.is_empty() {
        lines.push("Waiting for an Arena deck or draft".to_owned());
    } else {
        lines.push("Current deck".to_owned());
        lines.extend(snapshot.deck.iter().take(MAX_DECK_LINES).map(deck_line));
        if snapshot.deck.len() > MAX_DECK_LINES {
            lines.push(format!(
                "… {} more unique cards",
                snapshot.deck.len() - MAX_DECK_LINES
            ));
        }
    }

    NativeOverlayModel {
        title: format!("ArenaNext · {mode}"),
        lines,
    }
}

fn redraft_status_line(progress: &hs_state::RedraftProgress) -> Option<String> {
    use hs_state::RedraftStage;

    match progress.stage {
        RedraftStage::Inactive => None,
        RedraftStage::PickingOffers if !progress.pick_progress_known => Some(
            "Redraft progress unknown after current-deck resync; capture is withheld".to_owned(),
        ),
        RedraftStage::PickingOffers => match progress.pick_rounds_required {
            Some(rounds) => Some(format!(
                "Redraft pick {}/{}",
                progress.pick_rounds_completed.saturating_add(1),
                rounds
            )),
            None => Some("Redraft policy unavailable; capture is withheld".to_owned()),
        },
        RedraftStage::AwaitingDiscardReview => Some(format!(
            "Redraft picks complete; choose {} cards to discard",
            progress
                .discard_count_required
                .map(|count| count.to_string())
                .unwrap_or_else(|| "the required".to_owned())
        )),
        RedraftStage::ReviewingDiscards => Some(format!(
            "Redraft review: {} discard selections observed",
            progress.discarded_card_ids.len()
        )),
        RedraftStage::Complete => Some("Redraft review submitted; awaiting deck update".to_owned()),
    }
}

fn deck_line(card: &ResolvedDeckCard) -> String {
    match &card.resolution {
        CardResolution::Resolved { card: metadata } => {
            format!("{} ×{}", metadata.name, card.count)
        }
        CardResolution::Unrevealed => format!("Unrevealed card ×{}", card.count),
        CardResolution::NonCardEntity { .. } => format!("Non-card entity ×{}", card.count),
        CardResolution::MissingMetadata { card_id } => {
            format!("{card_id} · metadata unavailable ×{}", card.count)
        }
        CardResolution::InvalidCardId { card_id } => {
            format!("{card_id} · invalid card ID ×{}", card.count)
        }
    }
}

fn offer_kind_label(kind: hs_state::OfferKind) -> &'static str {
    match kind {
        hs_state::OfferKind::Heroes => "Hero choice",
        hs_state::OfferKind::Cards => "Draft pick",
        hs_state::OfferKind::RedraftPick => "Redraft pick",
        hs_state::OfferKind::Package => "Package choice",
        hs_state::OfferKind::Unknown => "Arena choice",
    }
}

fn offer_item_line(item: &hs_state::OfferItem) -> String {
    match item {
        hs_state::OfferItem::Card { card_id } => card_id.clone(),
        hs_state::OfferItem::Hero { card_id } => format!("Hero: {card_id}"),
        hs_state::OfferItem::HeroPower { card_id } => format!("Hero power: {card_id}"),
        hs_state::OfferItem::Package {
            key_card_id,
            contents,
        } => format!("Package: {key_card_id} (+{} cards)", contents.len()),
        hs_state::OfferItem::Unknown { label } => label
            .as_deref()
            .map(|label| format!("Unknown: {label}"))
            .unwrap_or_else(|| "Unknown offer item".to_owned()),
    }
}

pub(crate) fn hero_label(hero: hs_state::HeroClass) -> &'static str {
    match hero {
        hs_state::HeroClass::DeathKnight => "Death Knight",
        hs_state::HeroClass::DemonHunter => "Demon Hunter",
        hs_state::HeroClass::Druid => "Druid",
        hs_state::HeroClass::Hunter => "Hunter",
        hs_state::HeroClass::Mage => "Mage",
        hs_state::HeroClass::Paladin => "Paladin",
        hs_state::HeroClass::Priest => "Priest",
        hs_state::HeroClass::Rogue => "Rogue",
        hs_state::HeroClass::Shaman => "Shaman",
        hs_state::HeroClass::Warlock => "Warlock",
        hs_state::HeroClass::Warrior => "Warrior",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_card_data::CardCache;
    use hs_observer::SessionObserver;
    use std::path::PathBuf;

    #[test]
    fn fixture_snapshot_renders_resolved_card_names() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let observer =
            SessionObserver::attach(root.join("fixtures/logs/sample-arena-session")).unwrap();
        let cards = CardCache::load(root.join("fixtures/card-data/sample-cards.json")).unwrap();
        let model = from_snapshot(&observer.resolved_snapshot(&cards));

        assert_eq!(model.title, "ArenaNext · Arena");
        assert!(model.lines.iter().any(|line| line == "Fireball ×1"));
    }

    #[test]
    fn external_draft_model_overrides_unresolved_log_offer_text() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let observer =
            SessionObserver::attach(root.join("fixtures/logs/sample-arena-session")).unwrap();
        let cards = CardCache::load(root.join("fixtures/card-data/sample-cards.json")).unwrap();
        let model = from_snapshot_with_external_draft(
            &observer.resolved_snapshot(&cards),
            Some(&ExternalDraftModel {
                pick_number: 3,
                lines: vec!["Fireball · 95% · 82.5 Strong".to_owned()],
            }),
        );

        assert!(model.lines.iter().any(|line| line == "Draft pick 3"));
        assert!(
            model
                .lines
                .iter()
                .any(|line| line == "Fireball · 95% · 82.5 Strong")
        );
    }

    #[test]
    fn discard_review_is_rendered_as_review_not_a_five_card_offer() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let observer =
            SessionObserver::attach(root.join("fixtures/logs/sample-arena-session")).unwrap();
        let cards = CardCache::load(root.join("fixtures/card-data/sample-cards.json")).unwrap();
        let mut snapshot = observer.resolved_snapshot(&cards);
        snapshot.run.draft_phase = hs_state::ArenaDraftPhase::Redrafting;
        snapshot.draft.redraft = hs_state::RedraftProgress {
            stage: hs_state::RedraftStage::AwaitingDiscardReview,
            pick_rounds_required: Some(5),
            pick_progress_known: true,
            pick_rounds_completed: 5,
            discard_count_required: Some(5),
            discarded_card_ids: Vec::new(),
        };

        let model = from_snapshot(&snapshot);
        assert!(
            model
                .lines
                .iter()
                .any(|line| line == "Redraft picks complete; choose 5 cards to discard")
        );
        assert!(
            !model
                .lines
                .iter()
                .any(|line| line.contains("five-card offer"))
        );
    }

    #[test]
    fn active_redraft_distinguishes_retained_cards_from_replacements() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let observer =
            SessionObserver::attach(root.join("fixtures/logs/sample-arena-session")).unwrap();
        let cards = CardCache::load(root.join("fixtures/card-data/sample-cards.json")).unwrap();
        let mut snapshot = observer.resolved_snapshot(&cards);
        snapshot.run.draft_phase = hs_state::ArenaDraftPhase::Redrafting;
        snapshot.deck_state.expected_slots = Some(30);
        snapshot.deck_state.observed_slots = 29;
        snapshot.deck_state.unobserved_slots = Some(1);
        snapshot.draft.redraft = hs_state::RedraftProgress {
            stage: hs_state::RedraftStage::PickingOffers,
            pick_rounds_required: Some(5),
            pick_progress_known: true,
            pick_rounds_completed: 4,
            discard_count_required: Some(5),
            discarded_card_ids: Vec::new(),
        };

        let model = from_snapshot(&snapshot);
        assert_eq!(model.lines[0], "Mage · 25 retained + 4/5 replacements");
        assert_eq!(model.lines[1], "1 Redraft replacement remaining");
    }
}
