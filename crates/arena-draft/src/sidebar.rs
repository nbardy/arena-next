//! Pure interpretation and conservative reconciliation of deck-sidebar OCR.
//!
//! The capture adapter owns Vision (or another OCR engine). This module only
//! accepts text observations and card metadata, so its decisions are fixture
//! testable and platform-independent.

use std::collections::{BTreeMap, BTreeSet};

use hs_card_data::{CardCache, CardMetadata};
use serde::{Deserialize, Serialize};

pub const ARENA_DECK_CAPACITY: u8 = 30;

#[derive(Clone, Debug, PartialEq)]
pub struct SidebarTextObservation {
    pub text: String,
    pub confidence: f32,
}

impl SidebarTextObservation {
    pub fn new(text: impl Into<String>, confidence: f32) -> Self {
        Self {
            text: text.into(),
            confidence,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckCount {
    pub observed: u8,
    pub capacity: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidebarCard {
    pub card_id: String,
    pub name: String,
    pub quantity: u8,
    pub ocr_confidence: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarDeckStatus {
    /// The OCR crop is not proven to be Hearthstone's deck panel.
    Unanchored,
    /// The panel was found, but its `N/30` counter was not read.
    MissingCount,
    /// Every card represented by the counter is visible and resolved.
    Complete,
    /// A valid panel and counter were read, but only a lower bound is visible.
    Partial,
    /// Visible quantities contradict the counter or the counter is impossible.
    Inconsistent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SidebarIssue {
    AmbiguousCardName {
        text: String,
        candidate_ids: Vec<String>,
    },
    InvalidQuantity {
        text: String,
    },
    InvalidDeckCount {
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidebarDeckRead {
    pub anchored: bool,
    pub count: Option<DeckCount>,
    pub visible_cards: Vec<SidebarCard>,
    pub visible_quantity: u16,
    pub status: SidebarDeckStatus,
    pub issues: Vec<SidebarIssue>,
}

impl SidebarDeckRead {
    /// Returns canonical counts only when this read proves the whole deck is
    /// visible. A partial/scrolling panel is never exposed as authoritative.
    pub fn authoritative_counts(&self) -> Option<BTreeMap<String, u8>> {
        (self.status == SidebarDeckStatus::Complete).then(|| self.card_counts())
    }

    /// Visible lower bounds. Repeated OCR rows for the same ID are aggregated.
    pub fn card_counts(&self) -> BTreeMap<String, u8> {
        let mut counts = BTreeMap::new();
        for card in &self.visible_cards {
            let quantity = counts.entry(card.card_id.clone()).or_default();
            *quantity = u8::saturating_add(*quantity, card.quantity);
        }
        counts
    }
}

/// Turns unordered OCR strings from the sidebar crop into a validated read.
/// Card names must exactly match the local catalog, ignoring ASCII case.
pub fn interpret_deck_sidebar(
    observations: &[SidebarTextObservation],
    cards: &CardCache,
) -> SidebarDeckRead {
    let anchored = observations
        .iter()
        .any(|observation| is_deck_anchor(&observation.text));
    let mut issues = Vec::new();
    let mut count = None;
    for observation in observations {
        match parse_deck_count(&observation.text) {
            CountParse::Valid(value) if count.is_none() => count = Some(value),
            CountParse::Invalid => issues.push(SidebarIssue::InvalidDeckCount {
                text: observation.text.clone(),
            }),
            CountParse::NotCount | CountParse::Valid(_) => {}
        }
    }

    let mut visible_cards = Vec::new();
    if anchored {
        for observation in observations {
            if is_deck_anchor(&observation.text)
                || !matches!(parse_deck_count(&observation.text), CountParse::NotCount)
            {
                continue;
            }
            let quantity = match split_quantity(&observation.text) {
                QuantityParse::Valid { name, quantity } => (name, quantity),
                QuantityParse::Invalid => {
                    issues.push(SidebarIssue::InvalidQuantity {
                        text: observation.text.clone(),
                    });
                    continue;
                }
            };
            let candidates = eligible_candidates(cards.find_by_name(quantity.0));
            match candidates.as_slice() {
                [card] => visible_cards.push(SidebarCard {
                    card_id: card.id.clone(),
                    name: card.name.clone(),
                    quantity: quantity.1,
                    ocr_confidence: observation.confidence,
                }),
                [] => {}
                _ => issues.push(SidebarIssue::AmbiguousCardName {
                    text: quantity.0.to_owned(),
                    candidate_ids: candidates.into_iter().map(|card| card.id.clone()).collect(),
                }),
            }
        }
    }

    let visible_quantity = visible_cards
        .iter()
        .map(|card| u16::from(card.quantity))
        .sum();
    let status = if !anchored {
        SidebarDeckStatus::Unanchored
    } else if let Some(count) = count {
        if count.capacity != ARENA_DECK_CAPACITY || count.observed > count.capacity {
            SidebarDeckStatus::Inconsistent
        } else if visible_quantity == u16::from(count.observed) {
            SidebarDeckStatus::Complete
        } else if visible_quantity < u16::from(count.observed) {
            SidebarDeckStatus::Partial
        } else {
            SidebarDeckStatus::Inconsistent
        }
    } else {
        SidebarDeckStatus::MissingCount
    };

    SidebarDeckRead {
        anchored,
        count,
        visible_cards,
        visible_quantity,
        status,
        issues,
    }
}

fn eligible_candidates(candidates: Vec<&CardMetadata>) -> Vec<&CardMetadata> {
    let non_heroes: Vec<_> = candidates
        .into_iter()
        .filter(|card| !card.id.starts_with("HERO_"))
        .collect();
    let collectible: Vec<_> = non_heroes
        .iter()
        .copied()
        .filter(|card| card.collectible == Some(true))
        .collect();
    let selected = if collectible.is_empty() {
        non_heroes
    } else {
        collectible
    };
    let mut seen = BTreeSet::new();
    selected
        .into_iter()
        .filter(|card| seen.insert(card.id.as_str()))
        .collect()
}

fn is_deck_anchor(text: &str) -> bool {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .eq_ignore_ascii_case("Your Deck")
}

enum CountParse {
    Valid(DeckCount),
    Invalid,
    NotCount,
}

fn parse_deck_count(text: &str) -> CountParse {
    let compact: String = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let Some((observed, capacity)) = compact.split_once('/') else {
        return CountParse::NotCount;
    };
    if observed.is_empty()
        || capacity.is_empty()
        || !observed.chars().all(|value| value.is_ascii_digit())
        || !capacity.chars().all(|value| value.is_ascii_digit())
    {
        return CountParse::Invalid;
    }
    match (observed.parse(), capacity.parse()) {
        (Ok(observed), Ok(capacity)) => CountParse::Valid(DeckCount { observed, capacity }),
        _ => CountParse::Invalid,
    }
}

enum QuantityParse<'a> {
    Valid { name: &'a str, quantity: u8 },
    Invalid,
}

fn split_quantity(text: &str) -> QuantityParse<'_> {
    let text = text.trim();
    let compact_multipliers = [" x ", " X ", " × "];
    for separator in compact_multipliers {
        if let Some((name, digits)) = text.rsplit_once(separator) {
            return parsed_quantity(name, digits);
        }
    }
    let Some(separator) = text.rfind(char::is_whitespace) else {
        return QuantityParse::Valid {
            name: text,
            quantity: 1,
        };
    };
    let (name, suffix) = text.split_at(separator);
    let suffix = suffix.trim();
    let digits = suffix
        .strip_prefix('x')
        .or_else(|| suffix.strip_prefix('X'))
        .or_else(|| suffix.strip_prefix('×'));
    let Some(digits) = digits else {
        return QuantityParse::Valid {
            name: text,
            quantity: 1,
        };
    };
    parsed_quantity(name, digits)
}

fn parsed_quantity<'a>(name: &'a str, digits: &str) -> QuantityParse<'a> {
    match digits.parse::<u8>() {
        Ok(quantity)
            if quantity > 0 && quantity <= ARENA_DECK_CAPACITY && !name.trim().is_empty() =>
        {
            QuantityParse::Valid {
                name: name.trim(),
                quantity,
            }
        }
        _ => QuantityParse::Invalid,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationKind {
    /// A complete sidebar read replaced the prior reconstruction.
    AuthoritativeReplace,
    /// A partial read only raised known per-card lower bounds.
    PartialLowerBoundMerge,
    /// The read supplied no safe state change.
    Unchanged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SidebarReconciliation {
    pub kind: ReconciliationKind,
    pub counts: BTreeMap<String, u8>,
}

/// Reconciles OCR with log-derived state without ever treating a clipped or
/// scrolling sidebar as the complete deck.
pub fn reconcile_deck_sidebar(
    known: &BTreeMap<String, u8>,
    read: &SidebarDeckRead,
) -> SidebarReconciliation {
    if let Some(counts) = read.authoritative_counts() {
        return SidebarReconciliation {
            kind: ReconciliationKind::AuthoritativeReplace,
            counts,
        };
    }
    let Some(counter) = read
        .count
        .filter(|_| read.status == SidebarDeckStatus::Partial)
    else {
        return SidebarReconciliation {
            kind: ReconciliationKind::Unchanged,
            counts: known.clone(),
        };
    };

    let mut merged = known.clone();
    for (card_id, visible_quantity) in read.card_counts() {
        let quantity = merged.entry(card_id).or_default();
        *quantity = (*quantity).max(visible_quantity);
    }
    let total: u16 = merged.values().copied().map(u16::from).sum();
    if total > u16::from(counter.observed) {
        SidebarReconciliation {
            kind: ReconciliationKind::Unchanged,
            counts: known.clone(),
        }
    } else {
        let changed = merged != *known;
        SidebarReconciliation {
            kind: if changed {
                ReconciliationKind::PartialLowerBoundMerge
            } else {
                ReconciliationKind::Unchanged
            },
            counts: merged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> CardCache {
        CardCache::from_json(
            r#"[
                {"id":"TIME_003","name":"Portal Vanguard","cost":3,"collectible":true},
                {"id":"TIME_045","name":"Whelp of the Infinite","cost":3,"collectible":true},
                {"id":"END_036","name":"Morchie","cost":4,"collectible":true},
                {"id":"TIME_002","name":"Aeon Wizard","cost":5,"collectible":true},
                {"id":"TIME_004","name":"Conflux Crasher","cost":7,"collectible":true},
                {"id":"TOKEN_1","name":"Shared Name","collectible":false},
                {"id":"CARD_1","name":"Shared Name","collectible":true},
                {"id":"CARD_2","name":"Ambiguous","collectible":true},
                {"id":"CARD_3","name":"Ambiguous","collectible":true}
            ]"#,
        )
        .unwrap()
    }

    fn observations(lines: &[&str]) -> Vec<SidebarTextObservation> {
        lines
            .iter()
            .map(|line| SidebarTextObservation::new(*line, 0.95))
            .collect()
    }

    #[test]
    fn exact_five_card_sidebar_is_authoritative() {
        let read = interpret_deck_sidebar(
            &observations(&[
                "Your Deck",
                "Portal Vanguard",
                "Whelp of the Infinite",
                "Morchie",
                "Aeon Wizard",
                "Conflux Crasher",
                "5/30",
                "Cards",
            ]),
            &cache(),
        );
        assert_eq!(read.status, SidebarDeckStatus::Complete);
        assert_eq!(read.visible_quantity, 5);
        assert_eq!(read.authoritative_counts().unwrap()["TIME_002"], 1);
    }

    #[test]
    fn quantities_are_aggregated_and_support_multiply_sign() {
        let read = interpret_deck_sidebar(
            &observations(&["YOUR   DECK", "Morchie x2", "Aeon Wizard ×2", "4 / 30"]),
            &cache(),
        );
        assert_eq!(read.status, SidebarDeckStatus::Complete);
        assert_eq!(read.card_counts()["END_036"], 2);
        assert_eq!(read.card_counts()["TIME_002"], 2);
    }

    #[test]
    fn quantity_with_ocr_separated_multiplier_is_supported() {
        let read = interpret_deck_sidebar(
            &observations(&["Your Deck", "Morchie x 2", "2/30"]),
            &cache(),
        );
        assert_eq!(read.status, SidebarDeckStatus::Complete);
        assert_eq!(read.card_counts()["END_036"], 2);
    }

    #[test]
    fn scrolled_sidebar_is_partial_and_not_authoritative() {
        let read = interpret_deck_sidebar(
            &observations(&["Your Deck", "Morchie x2", "Aeon Wizard", "17/30"]),
            &cache(),
        );
        assert_eq!(read.status, SidebarDeckStatus::Partial);
        assert_eq!(read.authoritative_counts(), None);
    }

    #[test]
    fn partial_read_only_merges_lower_bounds_and_never_removes() {
        let read = interpret_deck_sidebar(
            &observations(&["Your Deck", "Morchie x2", "Aeon Wizard", "17/30"]),
            &cache(),
        );
        let known = BTreeMap::from([("TIME_045".to_owned(), 1), ("END_036".to_owned(), 1)]);
        let reconciled = reconcile_deck_sidebar(&known, &read);
        assert_eq!(reconciled.kind, ReconciliationKind::PartialLowerBoundMerge);
        assert_eq!(reconciled.counts["TIME_045"], 1);
        assert_eq!(reconciled.counts["END_036"], 2);
        assert_eq!(reconciled.counts["TIME_002"], 1);
    }

    #[test]
    fn partial_merge_that_would_exceed_counter_is_rejected() {
        let read = interpret_deck_sidebar(
            &observations(&["Your Deck", "Morchie x2", "5/30"]),
            &cache(),
        );
        let known = BTreeMap::from([
            ("TIME_045".to_owned(), 2),
            ("TIME_002".to_owned(), 2),
            ("END_036".to_owned(), 1),
        ]);
        let reconciled = reconcile_deck_sidebar(&known, &read);
        assert_eq!(reconciled.kind, ReconciliationKind::Unchanged);
        assert_eq!(reconciled.counts, known);
    }

    #[test]
    fn complete_read_replaces_incorrect_log_state() {
        let read = interpret_deck_sidebar(
            &observations(&["Your Deck", "Morchie", "Aeon Wizard", "2/30"]),
            &cache(),
        );
        let known = BTreeMap::from([("WRONG_001".to_owned(), 4)]);
        let reconciled = reconcile_deck_sidebar(&known, &read);
        assert_eq!(reconciled.kind, ReconciliationKind::AuthoritativeReplace);
        assert!(!reconciled.counts.contains_key("WRONG_001"));
        assert_eq!(reconciled.counts.len(), 2);
    }

    #[test]
    fn missing_anchor_or_counter_cannot_change_known_state() {
        let known = BTreeMap::from([("END_036".to_owned(), 1)]);
        for lines in [&["Morchie", "1/30"][..], &["Your Deck", "Morchie"][..]] {
            let read = interpret_deck_sidebar(&observations(lines), &cache());
            let reconciled = reconcile_deck_sidebar(&known, &read);
            assert_eq!(reconciled.kind, ReconciliationKind::Unchanged);
            assert_eq!(reconciled.counts, known);
        }
    }

    #[test]
    fn impossible_counter_and_overfull_visible_list_are_inconsistent() {
        let wrong_capacity =
            interpret_deck_sidebar(&observations(&["Your Deck", "Morchie", "1/40"]), &cache());
        assert_eq!(wrong_capacity.status, SidebarDeckStatus::Inconsistent);

        let overfull = interpret_deck_sidebar(
            &observations(&["Your Deck", "Morchie x2", "1/30"]),
            &cache(),
        );
        assert_eq!(overfull.status, SidebarDeckStatus::Inconsistent);
    }

    #[test]
    fn collectible_candidate_wins_over_same_named_token() {
        let read = interpret_deck_sidebar(
            &observations(&["Your Deck", "Shared Name", "1/30"]),
            &cache(),
        );
        assert_eq!(read.status, SidebarDeckStatus::Complete);
        assert_eq!(read.visible_cards[0].card_id, "CARD_1");
    }

    #[test]
    fn genuinely_ambiguous_name_is_not_guessed() {
        let read =
            interpret_deck_sidebar(&observations(&["Your Deck", "Ambiguous", "1/30"]), &cache());
        assert_eq!(read.status, SidebarDeckStatus::Partial);
        assert!(matches!(
            read.issues.as_slice(),
            [SidebarIssue::AmbiguousCardName { candidate_ids, .. }] if candidate_ids == &["CARD_2", "CARD_3"]
        ));
    }

    #[test]
    fn malformed_quantity_is_not_silently_counted_as_one() {
        let read = interpret_deck_sidebar(
            &observations(&["Your Deck", "Morchie x0", "1/30"]),
            &cache(),
        );
        assert_eq!(read.status, SidebarDeckStatus::Partial);
        assert!(matches!(
            read.issues.as_slice(),
            [SidebarIssue::InvalidQuantity { .. }]
        ));
    }
}
