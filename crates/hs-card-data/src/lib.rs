#![deny(unsafe_op_in_unsafe_fn)]

//! Versioned card metadata cache with explicit resolution states.

use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const CARD_CACHE_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CardMetadata {
    pub id: String,
    pub name: String,
    pub cost: Option<u8>,
    /// Printed base durability for weapons. This is the number of ordinary
    /// attacks the weapon supplies before card-text modifiers or buffs.
    #[serde(default)]
    pub durability: Option<u8>,
    #[serde(default, alias = "type")]
    pub card_type: Option<String>,
    #[serde(default)]
    pub classes: Vec<String>,
    #[serde(default)]
    pub collectible: Option<bool>,
    #[serde(default)]
    pub set: Option<String>,
    #[serde(default)]
    pub dbf_id: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CardCacheFile {
    pub format_version: u32,
    pub source: String,
    pub data_version: String,
    pub updated_at: DateTime<Utc>,
    pub cards: Vec<CardMetadata>,
}

#[derive(Clone, Debug, Default)]
pub struct CardCache {
    cards: BTreeMap<String, CardMetadata>,
    pub source: String,
    pub data_version: String,
    pub updated_at: Option<DateTime<Utc>>,
}

impl CardCache {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let source = fs::read_to_string(path)
            .with_context(|| format!("could not read card cache {}", path.display()))?;
        Self::from_json(&source)
            .with_context(|| format!("could not parse card cache {}", path.display()))
    }

    pub fn from_json(source: &str) -> Result<Self> {
        let input: CardCacheInput = serde_json::from_str(source)?;
        let (cards, source_name, data_version, updated_at) = match input {
            CardCacheInput::Cache(cache) => {
                if cache.format_version != CARD_CACHE_FORMAT_VERSION {
                    anyhow::bail!(
                        "unsupported card cache format {}; expected {}",
                        cache.format_version,
                        CARD_CACHE_FORMAT_VERSION
                    );
                }
                (
                    cache.cards,
                    cache.source,
                    cache.data_version,
                    Some(cache.updated_at),
                )
            }
            CardCacheInput::RawCards(cards) => {
                (cards, "raw-json-import".into(), "unknown".into(), None)
            }
        };
        let cards = cards
            .into_iter()
            // A cache entry without an ID or display name is not usable card
            // metadata. Dropping it makes a later lookup explicitly report
            // `MissingMetadata` instead of creating a fake zero-cost,
            // nameless card in the overlay.
            .filter(|card| !card.id.trim().is_empty() && !card.name.trim().is_empty())
            .map(|card| (card.id.clone(), card))
            .collect();
        Ok(Self {
            cards,
            source: source_name,
            data_version,
            updated_at,
        })
    }

    pub fn resolve(&self, card_id: &str) -> CardResolution {
        let card_id = card_id.trim();
        if card_id.is_empty() || matches!(card_id.to_ascii_uppercase().as_str(), "UNKNOWN" | "0") {
            return CardResolution::Unrevealed;
        }
        if card_id.starts_with("HERO_") || card_id.starts_with("GAME_") {
            return CardResolution::NonCardEntity {
                entity_id: card_id.to_owned(),
            };
        }
        if !is_valid_card_id(card_id) {
            return CardResolution::InvalidCardId {
                card_id: card_id.to_owned(),
            };
        }
        self.cards
            .get(card_id)
            .cloned()
            .map(|card| CardResolution::Resolved { card })
            .unwrap_or_else(|| CardResolution::MissingMetadata {
                card_id: card_id.to_owned(),
            })
    }

    /// Borrowing metadata lookup for diagnostics and deterministic analysis.
    /// Unlike insertion-oriented map APIs, this cannot create a cache entry.
    pub fn get(&self, card_id: &str) -> Option<&CardMetadata> {
        self.cards.get(card_id.trim())
    }

    /// Finds every metadata row whose display name exactly matches `name`.
    /// Multiple IDs are possible for collectible, token, and historical variants.
    pub fn find_by_name(&self, name: &str) -> Vec<&CardMetadata> {
        let name = name.trim();
        self.cards
            .values()
            .filter(|card| card.name.eq_ignore_ascii_case(name))
            .collect()
    }

    /// Finds collectible cards after applying the small punctuation/spacing
    /// normalization needed for local OCR. This never uses fuzzy edit
    /// distance: a visually similar but different card name must not become a
    /// draft recommendation.
    pub fn find_collectible_by_ocr_name(&self, name: &str) -> Vec<&CardMetadata> {
        let normalized = normalize_ocr_name(name);
        if normalized.is_empty() {
            return Vec::new();
        }
        self.cards
            .values()
            .filter(|card| card.collectible.unwrap_or(false))
            .filter(|card| normalize_ocr_name(&card.name) == normalized)
            .collect()
    }

    /// Returns collectible names within a deliberately tiny OCR correction
    /// radius. Callers should try exact matching first and accept this result
    /// only when the best card ID is unique.
    pub fn find_collectible_by_ocr_name_near(
        &self,
        name: &str,
        maximum_distance: usize,
    ) -> Vec<(&CardMetadata, usize)> {
        let normalized = normalize_ocr_name(name);
        if normalized.chars().count() < 6 {
            return Vec::new();
        }
        self.cards
            .values()
            .filter(|card| card.collectible.unwrap_or(false))
            .filter_map(|card| {
                let distance = levenshtein(&normalized, &normalize_ocr_name(&card.name));
                (distance <= maximum_distance).then_some((card, distance))
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    pub fn write_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let cache = CardCacheFile {
            format_version: CARD_CACHE_FORMAT_VERSION,
            source: self.source.clone(),
            data_version: self.data_version.clone(),
            updated_at: self.updated_at.unwrap_or_else(Utc::now),
            cards: self.cards.values().cloned().collect(),
        };
        let serialized = serde_json::to_vec_pretty(&cache)?;
        let parent = path
            .parent()
            .context("card cache path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serialized)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("could not replace {}", path.display()))?;
        Ok(())
    }
}

fn normalize_ocr_name(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|character| match character {
            '\u{2018}' | '\u{2019}' | '\u{02bc}' | '`' => Some('\''),
            character if character.is_alphanumeric() || character == '\'' => Some(character),
            character if character.is_whitespace() || character == '-' => Some(' '),
            _ => None,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn levenshtein(left: &str, right: &str) -> usize {
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_character) in left.chars().enumerate() {
        let mut current = Vec::with_capacity(right.len() + 1);
        current.push(left_index + 1);
        for (right_index, right_character) in right.iter().enumerate() {
            current.push(
                (current[right_index] + 1)
                    .min(previous[right_index + 1] + 1)
                    .min(previous[right_index] + usize::from(left_character != *right_character)),
            );
        }
        previous = current;
    }
    previous[right.len()]
}

/// Converts the public HearthstoneJSON card array into ArenaNext's small,
/// versioned cache format. The downloader lives in `hearthd`, keeping this
/// crate deterministic and usable in fixture tests.
pub fn import_hearthstonejson(
    source_json: &str,
    source_url: impl Into<String>,
    data_version: impl Into<String>,
) -> Result<CardCache> {
    let raw_cards: Vec<HearthstoneJsonCard> = serde_json::from_str(source_json)?;
    let cards = raw_cards
        .into_iter()
        .filter(|card| !card.id.trim().is_empty() && !card.name.trim().is_empty())
        .map(|card| {
            let classes = if card.classes.is_empty() {
                card.card_class.into_iter().collect()
            } else {
                card.classes
            };
            // Current HearthstoneJSON exports ordinary weapon durability in
            // `health`; a few legacy CORE rows still use `durability`.
            let durability = card
                .card_type
                .as_deref()
                .filter(|kind| kind.eq_ignore_ascii_case("WEAPON"))
                .and_then(|_| card.health.or(card.durability.filter(|value| *value > 0)));
            let metadata = CardMetadata {
                id: card.id.clone(),
                name: card.name,
                cost: card.cost,
                durability,
                card_type: card.card_type,
                classes,
                collectible: card.collectible,
                set: card.set,
                dbf_id: card.dbf_id,
            };
            (card.id, metadata)
        })
        .collect();
    Ok(CardCache {
        cards,
        source: source_url.into(),
        data_version: data_version.into(),
        updated_at: Some(Utc::now()),
    })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HearthstoneJsonCard {
    id: String,
    name: String,
    cost: Option<u8>,
    durability: Option<u8>,
    health: Option<u8>,
    #[serde(rename = "type")]
    card_type: Option<String>,
    #[serde(default)]
    classes: Vec<String>,
    #[serde(rename = "cardClass")]
    card_class: Option<String>,
    collectible: Option<bool>,
    set: Option<String>,
    dbf_id: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum CardCacheInput {
    Cache(CardCacheFile),
    RawCards(Vec<CardMetadata>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum CardResolution {
    Resolved { card: CardMetadata },
    Unrevealed,
    NonCardEntity { entity_id: String },
    MissingMetadata { card_id: String },
    InvalidCardId { card_id: String },
}

fn is_valid_card_id(value: &str) -> bool {
    value.len() >= 3
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_name_lookup_normalizes_apostrophes_and_spacing_but_stays_exact() {
        let cache = CardCache::from_json(
            r#"[{"id":"JAIL_503","name":"Blackpaw's Whip","cost":3,"type":"WEAPON","collectible":true},{"id":"TOKEN_1","name":"Blackpaw's Whip","collectible":false}]"#,
        )
        .unwrap();
        let matches = cache.find_collectible_by_ocr_name("  Blackpaw\u{2019}s   Whip ");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "JAIL_503");
        assert!(
            cache
                .find_collectible_by_ocr_name("Blackpaw Whip")
                .is_empty()
        );
    }

    #[test]
    fn bounded_ocr_name_lookup_repairs_one_character_only() {
        let cache = CardCache::from_json(
            r#"[{"id":"JAIL_703","name":"Gullible Guard","collectible":true},{"id":"OTHER","name":"Gullible Bard","collectible":true}]"#,
        )
        .unwrap();
        let matches = cache.find_collectible_by_ocr_name_near("Gullible Guara", 1);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0.id, "JAIL_703");
        assert_eq!(matches[0].1, 1);
        assert!(
            cache
                .find_collectible_by_ocr_name_near("Gullible Gxxxx", 1)
                .is_empty()
        );
    }

    #[test]
    fn missing_metadata_does_not_become_a_zero_cost_card() {
        let cache = CardCache::empty();
        assert_eq!(
            cache.resolve("NEW_999"),
            CardResolution::MissingMetadata {
                card_id: "NEW_999".into()
            }
        );
        assert_eq!(
            cache.resolve("HERO_08"),
            CardResolution::NonCardEntity {
                entity_id: "HERO_08".into()
            }
        );
    }

    #[test]
    fn unknown_lookup_is_non_mutating_even_when_repeated() {
        let cache =
            CardCache::from_json(r#"[{"id":"CS2_029","name":"Fireball","cost":4}]"#).unwrap();
        let before = cache.len();

        assert!(cache.get("TLC_123").is_none());
        assert!(matches!(
            cache.resolve("TLC_123"),
            CardResolution::MissingMetadata { .. }
        ));
        assert!(cache.get("TLC_123").is_none());
        assert_eq!(cache.len(), before);
    }

    #[test]
    fn malformed_empty_name_is_missing_metadata_not_a_resolved_card() {
        let cache = CardCache::from_json(r#"[{"id":"TLC_123","name":"","cost":0}]"#).unwrap();

        assert_eq!(
            cache.resolve("TLC_123"),
            CardResolution::MissingMetadata {
                card_id: "TLC_123".into()
            }
        );
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn accepts_cache_file_format() {
        let cache = CardCache::from_json(
            r#"{"formatVersion":1,"source":"test","dataVersion":"1","updatedAt":"2026-01-01T00:00:00Z","cards":[{"id":"CS2_029","name":"Fireball","cost":4}]}"#,
        )
        .unwrap();
        assert!(matches!(
            cache.resolve("CS2_029"),
            CardResolution::Resolved { .. }
        ));
    }

    #[test]
    fn imports_hearthstonejson_card_shapes() {
        let cache = import_hearthstonejson(
            r#"[{"id":"EX1_116","name":"Leeroy Jenkins","cost":5,"type":"MINION","cardClass":"NEUTRAL","collectible":true,"set":"EXPERT1","dbfId":559},{"id":"WEAPON_1","name":"Test Blade","cost":3,"durability":0,"health":2,"type":"WEAPON","cardClass":"ROGUE","collectible":true}]"#,
            "https://api.hearthstonejson.com/v1/latest/enUS/cards.collectible.json",
            "fixture-build",
        )
        .unwrap();
        let CardResolution::Resolved { card } = cache.resolve("EX1_116") else {
            panic!("expected imported card");
        };
        assert_eq!(card.cost, Some(5));
        assert_eq!(card.card_type.as_deref(), Some("MINION"));
        assert_eq!(card.classes, ["NEUTRAL"]);
        let CardResolution::Resolved { card: weapon } = cache.resolve("WEAPON_1") else {
            panic!("expected imported weapon");
        };
        assert_eq!(weapon.durability, Some(2));
    }
}
