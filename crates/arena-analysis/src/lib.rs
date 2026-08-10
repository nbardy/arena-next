#![deny(unsafe_op_in_unsafe_fn)]

//! Deterministic, local Arena deck analysis.
//!
//! This crate deliberately does not infer mechanics from card names or ask a
//! model to calculate a deck. It consumes only explicit metadata costs and
//! verified `analysisTags` from the versioned card catalog. Missing metadata
//! remains visible as uncertainty in the resulting profile.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, bail};
use hs_card_data::CardCache;
use hs_state::DeckCard;
use serde::{Deserialize, Serialize};

pub const ANALYSIS_SCHEMA_VERSION: u32 = 1;
pub const ANALYSIS_FACTS_SCHEMA_VERSION: u32 = 1;

/// A stable vocabulary for the generic deck features shown by the first
/// version. Catalogs may additionally contain `synergy:<name>` and
/// `requires:<name>` tags for season-specific packages.
pub const FEATURE_TAGS: &[&str] = &[
    "removal",
    "board_clear",
    "card_draw",
    "discover",
    "generation",
    "reach",
    "taunt",
    "stabilization",
    "token",
    "deathrattle",
];

/// A versioned, local semantic-facts plane. It is intentionally independent
/// from card metadata: refreshing card names/costs cannot silently change a
/// verified removal, tribal, or package fact.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisFactsFile {
    pub schema_version: u32,
    pub source: String,
    pub data_version: String,
    pub cards: Vec<AnalysisFactCard>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisFactCard {
    pub card_id: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Immutable lookup table for verified local semantic facts.
#[derive(Clone, Debug, Default)]
pub struct AnalysisFacts {
    cards: BTreeMap<String, BTreeSet<String>>,
    pub source: String,
    pub data_version: String,
}

impl AnalysisFacts {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .with_context(|| format!("could not read analysis facts {}", path.display()))?;
        Self::from_json(&contents)
            .with_context(|| format!("could not parse analysis facts {}", path.display()))
    }

    pub fn from_json(contents: &str) -> Result<Self> {
        let file: AnalysisFactsFile = serde_json::from_str(contents)?;
        if file.schema_version != ANALYSIS_FACTS_SCHEMA_VERSION {
            bail!(
                "unsupported analysis facts schemaVersion {}; expected {}",
                file.schema_version,
                ANALYSIS_FACTS_SCHEMA_VERSION
            );
        }
        if file.source.trim().is_empty() {
            bail!("analysis facts source must not be empty");
        }
        if file.data_version.trim().is_empty() {
            bail!("analysis facts dataVersion must not be empty");
        }
        let mut cards = BTreeMap::new();
        for card in file.cards {
            let card_id = card.card_id.trim();
            if card_id.is_empty() {
                bail!("analysis facts contains an empty cardId");
            }
            let tags = card
                .tags
                .into_iter()
                .map(|tag| normalize_fact_tag(&tag))
                .collect::<Result<BTreeSet<_>>>()?;
            if cards.insert(card_id.to_owned(), tags).is_some() {
                bail!("analysis facts contains duplicate cardId `{card_id}`");
            }
        }
        Ok(Self {
            cards,
            source: file.source,
            data_version: file.data_version,
        })
    }

    /// Returns whether a card has an explicit semantic-facts record. An empty
    /// tag list means "known to have no listed facts", not "facts missing".
    pub fn contains(&self, card_id: &str) -> bool {
        self.cards.contains_key(card_id.trim())
    }

    pub fn tags(&self, card_id: &str) -> BTreeSet<String> {
        self.cards.get(card_id.trim()).cloned().unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisInput {
    /// The deck facts from the reducer. Counts are never synthesized here.
    pub deck: Vec<DeckCard>,
    /// An optional season/rules value. It is used only to qualify profile
    /// certainty; the analyzer never assumes a 30-card Arena deck.
    pub expected_slots: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckProfile {
    pub schema_version: u32,
    pub observed_slots: u16,
    pub expected_slots: Option<u16>,
    pub mana_curve: ManaCurve,
    pub early_game_density: u16,
    pub removal: u16,
    pub board_clears: u16,
    pub card_draw: u16,
    pub discover_and_generation: u16,
    pub reach: u16,
    pub taunts: u16,
    pub stabilization: u16,
    /// Counts tags that are meaningful to future draft choices. This is
    /// intentionally a map rather than a closed enum so a season manifest can
    /// add `synergy:corpse`, `synergy:excavate`, etc. without a code release.
    pub synergy_counts: BTreeMap<String, u16>,
    pub expected_speed: DeckSpeed,
    pub weaknesses: Vec<DeckWeakness>,
    pub uncertainty: AnalysisUncertainty,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManaCurve {
    pub zero_to_two: u16,
    pub three: u16,
    pub four_to_six: u16,
    pub seven_plus: u16,
    pub unknown_cost: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeckSpeed {
    Tempo,
    Midrange,
    Control,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeckWeakness {
    InsufficientEarlyBoardPresence,
    LimitedRemoval,
    LimitedCardDraw,
    ExpensiveCurve,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisUncertainty {
    pub missing_metadata_ids: Vec<String>,
    /// Cards whose costs/names are known but whose semantic-facts record is
    /// absent. Feature counts must not be treated as complete in this state.
    pub missing_analysis_fact_ids: Vec<String>,
    pub unknown_cost_slots: u16,
    pub deck_slots_not_observed: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferAnalysis {
    pub card_id: String,
    pub deck_delta: DeckDelta,
    pub verified_synergies: Vec<VerifiedSignal>,
    pub verified_anti_synergies: Vec<VerifiedSignal>,
    pub draft_optionality: DraftOptionality,
    pub uncertainty: AnalysisUncertainty,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckDelta {
    pub zero_to_two: i16,
    pub three: i16,
    pub four_to_six: i16,
    pub seven_plus: i16,
    pub unknown_cost: i16,
    pub removal: i16,
    pub board_clears: i16,
    pub card_draw: i16,
    pub discover_and_generation: i16,
    pub reach: i16,
    pub taunts: i16,
    pub stabilization: i16,
    pub synergy_counts: BTreeMap<String, i16>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedSignal {
    pub category: String,
    pub observed_before_pick: u16,
    pub observed_after_pick: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftOptionality {
    /// A narrow synergy or requirement is an explicit commitment. A generic
    /// card has `open` commitment rather than a made-up numeric confidence.
    pub commitment: DraftCommitment,
    /// Categories whose future payoffs become more useful after the pick.
    pub future_priorities: Vec<String>,
    /// True only when the offer adds no explicit synergy/requires tag.
    pub preserves_open_paths: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftCommitment {
    Open,
    Light,
    Moderate,
    Strong,
    Unknown,
}

/// Analyze the observed deck using only explicit cached metadata.
pub fn analyze_deck(
    input: &AnalysisInput,
    catalog: &CardCache,
    facts: &AnalysisFacts,
) -> DeckProfile {
    let mut accumulator = Accumulator::default();
    for entry in &input.deck {
        accumulator.add(entry, catalog, facts);
    }
    accumulator.profile(input.expected_slots)
}

/// Calculate the counterfactual of selecting one offered card. The base deck
/// remains unchanged, which makes repeated candidate analysis deterministic.
pub fn analyze_offer(
    input: &AnalysisInput,
    offered_card_id: impl Into<String>,
    catalog: &CardCache,
    facts: &AnalysisFacts,
) -> OfferAnalysis {
    let offered_card_id = offered_card_id.into();
    let before = analyze_deck(input, catalog, facts);
    let after_input = AnalysisInput {
        deck: {
            let mut deck = input.deck.clone();
            if let Some(existing) = deck
                .iter_mut()
                .find(|entry| entry.card_id == offered_card_id)
            {
                existing.count = existing.count.saturating_add(1);
            } else {
                deck.push(DeckCard {
                    card_id: offered_card_id.clone(),
                    count: 1,
                });
            }
            deck
        },
        expected_slots: input.expected_slots,
    };
    let after = analyze_deck(&after_input, catalog, facts);

    let offered_tags = facts.tags(&offered_card_id);
    let verified_synergies = offered_tags
        .iter()
        .filter_map(|tag| tag.strip_prefix("synergy:"))
        .map(|category| signal(category, &before, &after))
        .filter(|signal| signal.observed_before_pick > 0)
        .collect();
    let verified_anti_synergies = offered_tags
        .iter()
        .filter_map(|tag| tag.strip_prefix("requires:"))
        .filter(|category| before.synergy_counts.get(*category).copied().unwrap_or(0) == 0)
        .map(|category| signal(category, &before, &after))
        .collect();

    let future_priorities = offered_tags
        .iter()
        .filter_map(|tag| {
            tag.strip_prefix("synergy:")
                .or_else(|| tag.strip_prefix("requires:"))
        })
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let structural_tag_count = future_priorities.len();
    let requires_count = offered_tags
        .iter()
        .filter(|tag| tag.starts_with("requires:"))
        .count();
    let commitment = match (
        catalog.get(&offered_card_id),
        facts.contains(&offered_card_id),
    ) {
        (None, _) | (_, false) => DraftCommitment::Unknown,
        (Some(_), true) if structural_tag_count == 0 => DraftCommitment::Open,
        (Some(_), true) if requires_count > 0 && structural_tag_count > 1 => {
            DraftCommitment::Strong
        }
        (Some(_), true) if requires_count > 0 => DraftCommitment::Moderate,
        (Some(_), true) if structural_tag_count > 1 => DraftCommitment::Moderate,
        (Some(_), true) => DraftCommitment::Light,
    };

    OfferAnalysis {
        card_id: offered_card_id,
        deck_delta: diff_profiles(&before, &after),
        verified_synergies,
        verified_anti_synergies,
        draft_optionality: DraftOptionality {
            commitment,
            preserves_open_paths: structural_tag_count == 0,
            future_priorities,
        },
        uncertainty: after.uncertainty,
    }
}

#[derive(Default)]
struct Accumulator {
    observed_slots: u16,
    curve: ManaCurve,
    feature_counts: BTreeMap<String, u16>,
    synergy_counts: BTreeMap<String, u16>,
    missing_metadata_ids: BTreeSet<String>,
    missing_analysis_fact_ids: BTreeSet<String>,
}

impl Accumulator {
    fn add(&mut self, entry: &DeckCard, catalog: &CardCache, facts: &AnalysisFacts) {
        let count = u16::from(entry.count);
        self.observed_slots = self.observed_slots.saturating_add(count);
        let Some(metadata) = catalog.get(&entry.card_id) else {
            self.curve.unknown_cost = self.curve.unknown_cost.saturating_add(count);
            self.missing_metadata_ids.insert(entry.card_id.clone());
            return;
        };

        match metadata.cost {
            Some(0..=2) => self.curve.zero_to_two = self.curve.zero_to_two.saturating_add(count),
            Some(3) => self.curve.three = self.curve.three.saturating_add(count),
            Some(4..=6) => self.curve.four_to_six = self.curve.four_to_six.saturating_add(count),
            Some(_) => self.curve.seven_plus = self.curve.seven_plus.saturating_add(count),
            None => self.curve.unknown_cost = self.curve.unknown_cost.saturating_add(count),
        }

        if !facts.contains(&entry.card_id) {
            self.missing_analysis_fact_ids.insert(entry.card_id.clone());
            return;
        }
        for tag in facts.tags(&entry.card_id) {
            if FEATURE_TAGS.contains(&tag.as_str()) {
                *self.feature_counts.entry(tag).or_default() += count;
            } else if let Some(category) = tag.strip_prefix("synergy:") {
                *self.synergy_counts.entry(category.to_owned()).or_default() += count;
            }
        }
    }

    fn profile(self, expected_slots: Option<u16>) -> DeckProfile {
        let early_game_density = self.curve.zero_to_two;
        let removal = self.feature("removal");
        let board_clears = self.feature("board_clear");
        let card_draw = self.feature("card_draw");
        let discover_and_generation = self
            .feature("discover")
            .saturating_add(self.feature("generation"));
        let reach = self.feature("reach");
        let taunts = self.feature("taunt");
        let stabilization = self.feature("stabilization");
        let known_cost_slots = self.observed_slots.saturating_sub(self.curve.unknown_cost);
        let missing_slots =
            expected_slots.map(|expected| expected.saturating_sub(self.observed_slots));
        let has_material_uncertainty = known_cost_slots < 8
            || self.curve.unknown_cost > 0
            || missing_slots.unwrap_or(0) > 0
            || !self.missing_analysis_fact_ids.is_empty();
        let expected_speed = if has_material_uncertainty {
            DeckSpeed::Uncertain
        } else if self.curve.zero_to_two >= self.curve.seven_plus.saturating_add(4) {
            DeckSpeed::Tempo
        } else if self.curve.seven_plus >= self.curve.zero_to_two.saturating_add(3) {
            DeckSpeed::Control
        } else {
            DeckSpeed::Midrange
        };

        let mut weaknesses = Vec::new();
        if known_cost_slots >= 8 && early_game_density.saturating_mul(4) < known_cost_slots {
            weaknesses.push(DeckWeakness::InsufficientEarlyBoardPresence);
        }
        if self.missing_analysis_fact_ids.is_empty() && known_cost_slots >= 10 && removal == 0 {
            weaknesses.push(DeckWeakness::LimitedRemoval);
        }
        if self.missing_analysis_fact_ids.is_empty() && known_cost_slots >= 12 && card_draw == 0 {
            weaknesses.push(DeckWeakness::LimitedCardDraw);
        }
        if known_cost_slots >= 8 && self.curve.seven_plus > self.curve.zero_to_two {
            weaknesses.push(DeckWeakness::ExpensiveCurve);
        }
        let unknown_cost_slots = self.curve.unknown_cost;

        DeckProfile {
            schema_version: ANALYSIS_SCHEMA_VERSION,
            observed_slots: self.observed_slots,
            expected_slots,
            mana_curve: self.curve,
            early_game_density,
            removal,
            board_clears,
            card_draw,
            discover_and_generation,
            reach,
            taunts,
            stabilization,
            synergy_counts: self.synergy_counts,
            expected_speed,
            weaknesses,
            uncertainty: AnalysisUncertainty {
                missing_metadata_ids: self.missing_metadata_ids.into_iter().collect(),
                missing_analysis_fact_ids: self.missing_analysis_fact_ids.into_iter().collect(),
                unknown_cost_slots,
                deck_slots_not_observed: missing_slots,
            },
        }
    }

    fn feature(&self, feature: &str) -> u16 {
        self.feature_counts.get(feature).copied().unwrap_or(0)
    }
}

fn normalize_fact_tag(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        bail!("analysis facts contains an empty tag");
    }
    let category = normalized
        .strip_prefix("synergy:")
        .or_else(|| normalized.strip_prefix("requires:"));
    if let Some(category) = category {
        if category.is_empty()
            || !category.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
            })
        {
            bail!("analysis facts tag `{value}` has an invalid category");
        }
    }
    Ok(normalized)
}

fn signal(category: &str, before: &DeckProfile, after: &DeckProfile) -> VerifiedSignal {
    VerifiedSignal {
        category: category.to_owned(),
        observed_before_pick: before.synergy_counts.get(category).copied().unwrap_or(0),
        observed_after_pick: after.synergy_counts.get(category).copied().unwrap_or(0),
    }
}

fn diff_profiles(before: &DeckProfile, after: &DeckProfile) -> DeckDelta {
    let mut synergy_counts = BTreeMap::new();
    for key in before
        .synergy_counts
        .keys()
        .chain(after.synergy_counts.keys())
    {
        let difference = i16::try_from(after.synergy_counts.get(key).copied().unwrap_or(0))
            .unwrap_or(i16::MAX)
            - i16::try_from(before.synergy_counts.get(key).copied().unwrap_or(0))
                .unwrap_or(i16::MAX);
        if difference != 0 {
            synergy_counts.insert(key.clone(), difference);
        }
    }
    DeckDelta {
        zero_to_two: signed_delta(after.mana_curve.zero_to_two, before.mana_curve.zero_to_two),
        three: signed_delta(after.mana_curve.three, before.mana_curve.three),
        four_to_six: signed_delta(after.mana_curve.four_to_six, before.mana_curve.four_to_six),
        seven_plus: signed_delta(after.mana_curve.seven_plus, before.mana_curve.seven_plus),
        unknown_cost: signed_delta(
            after.mana_curve.unknown_cost,
            before.mana_curve.unknown_cost,
        ),
        removal: signed_delta(after.removal, before.removal),
        board_clears: signed_delta(after.board_clears, before.board_clears),
        card_draw: signed_delta(after.card_draw, before.card_draw),
        discover_and_generation: signed_delta(
            after.discover_and_generation,
            before.discover_and_generation,
        ),
        reach: signed_delta(after.reach, before.reach),
        taunts: signed_delta(after.taunts, before.taunts),
        stabilization: signed_delta(after.stabilization, before.stabilization),
        synergy_counts,
    }
}

fn signed_delta(after: u16, before: u16) -> i16 {
    i16::try_from(after).unwrap_or(i16::MAX) - i16::try_from(before).unwrap_or(i16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> CardCache {
        CardCache::from_json(
            r#"[
              {"id":"EARLY","name":"Early","cost":2},
              {"id":"VALUE","name":"Value","cost":5},
              {"id":"PAYOFF","name":"Payoff","cost":3},
              {"id":"GENERIC","name":"Generic","cost":4}
            ]"#,
        )
        .unwrap()
    }

    fn facts() -> AnalysisFacts {
        AnalysisFacts::from_json(
            r#"{
              "schemaVersion": 1,
              "source": "fixture",
              "dataVersion": "1",
              "cards": [
                {"cardId":"EARLY","tags":["removal","synergy:elemental"]},
                {"cardId":"VALUE","tags":["card_draw","taunt"]},
                {"cardId":"PAYOFF","tags":["requires:elemental","synergy:elemental"]},
                {"cardId":"GENERIC","tags":[]}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn profile_uses_explicit_metadata_and_reports_unknowns() {
        let input = AnalysisInput {
            deck: vec![
                DeckCard {
                    card_id: "EARLY".into(),
                    count: 3,
                },
                DeckCard {
                    card_id: "VALUE".into(),
                    count: 2,
                },
                DeckCard {
                    card_id: "MISSING_001".into(),
                    count: 1,
                },
            ],
            expected_slots: Some(8),
        };
        let profile = analyze_deck(&input, &catalog(), &facts());
        assert_eq!(profile.observed_slots, 6);
        assert_eq!(profile.mana_curve.zero_to_two, 3);
        assert_eq!(profile.mana_curve.four_to_six, 2);
        assert_eq!(profile.mana_curve.unknown_cost, 1);
        assert_eq!(profile.removal, 3);
        assert_eq!(profile.card_draw, 2);
        assert_eq!(profile.synergy_counts.get("elemental"), Some(&3));
        assert_eq!(profile.uncertainty.deck_slots_not_observed, Some(2));
        assert_eq!(
            profile.uncertainty.missing_metadata_ids,
            vec!["MISSING_001".to_owned()]
        );
        assert!(profile.uncertainty.missing_analysis_fact_ids.is_empty());
        assert_eq!(profile.expected_speed, DeckSpeed::Uncertain);
    }

    #[test]
    fn counterfactual_is_deterministic_and_exposes_support_requirements() {
        let input = AnalysisInput {
            deck: vec![DeckCard {
                card_id: "EARLY".into(),
                count: 3,
            }],
            expected_slots: None,
        };
        let cache = catalog();
        let analysis_facts = facts();
        let payoff = analyze_offer(&input, "PAYOFF", &cache, &analysis_facts);
        assert_eq!(payoff.deck_delta.three, 1);
        assert_eq!(payoff.deck_delta.synergy_counts.get("elemental"), Some(&1));
        assert_eq!(payoff.verified_synergies.len(), 1);
        assert!(payoff.verified_anti_synergies.is_empty());
        assert_eq!(
            payoff.draft_optionality.commitment,
            DraftCommitment::Moderate
        );
        assert_eq!(
            payoff.draft_optionality.future_priorities,
            vec!["elemental"]
        );

        let generic = analyze_offer(&input, "GENERIC", &cache, &analysis_facts);
        assert!(generic.draft_optionality.preserves_open_paths);
        assert_eq!(generic.draft_optionality.commitment, DraftCommitment::Open);
        assert_eq!(
            analyze_deck(&input, &cache, &analysis_facts).observed_slots,
            3
        );
    }

    #[test]
    fn missing_offer_metadata_is_explicit_not_an_invented_delta() {
        let input = AnalysisInput::default();
        let result = analyze_offer(&input, "UNKNOWN_CARD", &catalog(), &facts());
        assert_eq!(result.deck_delta.unknown_cost, 1);
        assert_eq!(
            result.draft_optionality.commitment,
            DraftCommitment::Unknown
        );
        assert_eq!(
            result.uncertainty.missing_metadata_ids,
            vec!["UNKNOWN_CARD".to_owned()]
        );
    }

    #[test]
    fn missing_facts_withhold_semantic_claims_without_losing_curve_data() {
        let input = AnalysisInput {
            deck: vec![DeckCard {
                card_id: "GENERIC".into(),
                count: 1,
            }],
            expected_slots: None,
        };
        let profile = analyze_deck(&input, &catalog(), &AnalysisFacts::empty());
        assert_eq!(profile.mana_curve.four_to_six, 1);
        assert_eq!(
            profile.uncertainty.missing_analysis_fact_ids,
            vec!["GENERIC"]
        );
        assert_eq!(profile.expected_speed, DeckSpeed::Uncertain);
        assert_eq!(
            analyze_offer(&input, "GENERIC", &catalog(), &AnalysisFacts::empty())
                .draft_optionality
                .commitment,
            DraftCommitment::Unknown
        );
    }

    #[test]
    fn facts_validation_rejects_duplicate_ids_and_bad_requirement_tags() {
        assert!(AnalysisFacts::from_json(
            r#"{"schemaVersion":1,"source":"x","dataVersion":"1","cards":[{"cardId":"A","tags":[]},{"cardId":"A","tags":[]}]}"#
        )
        .is_err());
        assert!(AnalysisFacts::from_json(
            r#"{"schemaVersion":1,"source":"x","dataVersion":"1","cards":[{"cardId":"A","tags":["requires:"]}]}"#
        )
        .is_err());
    }
}
