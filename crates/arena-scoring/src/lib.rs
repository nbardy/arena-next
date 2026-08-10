#![deny(unsafe_op_in_unsafe_fn)]

//! Pluggable Arena rating providers. v1 intentionally supports only local,
//! user-supplied JSON imports rather than undocumented remote endpoints.

use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result};
use arena_analysis::{AnalysisFacts, AnalysisInput, analyze_deck};
use chrono::{DateTime, NaiveDate, Utc};
use hs_card_data::{CardCache, CardMetadata};
use hs_state::HeroClass;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderMetadata {
    pub provider: String,
    pub data_timestamp: DateTime<Utc>,
    pub arena_season: Option<String>,
    pub data_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CardRating {
    pub card_id: String,
    pub value: f32,
    pub sample_size: Option<u64>,
    pub label: Option<String>,
}

/// One provider's own rating for a card, with its own scale (HearthArena tier
/// scores vs win-rate percentages). Every consumer that wants to show more
/// than a single combined number uses this evidence list.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRating {
    pub provider: ProviderMetadata,
    pub rating: CardRating,
}

pub trait RatingProvider: Send + Sync {
    fn metadata(&self) -> &ProviderMetadata;
    fn rating(&self, card_id: &str, class: Option<HeroClass>) -> Option<CardRating>;

    /// The individual provider ratings behind `rating`. The default is the
    /// single source itself; a composite provider returns one entry per
    /// source so per-source scales never disappear behind the joined number.
    fn provider_ratings(&self, card_id: &str, class: Option<HeroClass>) -> Vec<ProviderRating> {
        self.rating(card_id, class)
            .map(|rating| ProviderRating {
                provider: self.metadata().clone(),
                rating,
            })
            .into_iter()
            .collect()
    }
}

/// The two values shown for a draft offer. A deck score never exists without
/// provider evidence: local curve and synergy facts adjust a rating, but are
/// not themselves a card-rating system.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfferScore {
    pub card_id: String,
    pub base_rating: Option<CardRating>,
    pub provider_ratings: Vec<ProviderRating>,
    pub deck_score: Option<f32>,
    pub adjustment: f32,
    pub adjustments: Vec<ScoreAdjustment>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScoreAdjustment {
    pub kind: AdjustmentKind,
    pub delta: f32,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjustmentKind {
    MissingTwoDrop,
    CrowdedCurveBand,
    VerifiedSynergy,
    SupportedPayoff,
    UnsupportedPayoff,
    RepeatedCard,
    TooManyWeaponCharges,
    AdjustmentLimit,
}

const MAX_DECK_ADJUSTMENT: f32 = 12.0;

/// Adjust a provider rating using only observed deck metadata and explicit,
/// versioned analysis facts. The result is deterministic and every numeric
/// change is represented by an adjustment row.
pub fn score_offer(
    provider: &dyn RatingProvider,
    class: Option<HeroClass>,
    input: &AnalysisInput,
    card_id: &str,
    catalog: &CardCache,
    facts: &AnalysisFacts,
) -> OfferScore {
    let base_rating = provider.rating(card_id, class);
    let Some(base_value) = base_rating.as_ref().map(|rating| rating.value) else {
        return OfferScore {
            card_id: card_id.to_owned(),
            base_rating: None,
            provider_ratings: Vec::new(),
            deck_score: None,
            adjustment: 0.0,
            adjustments: Vec::new(),
        };
    };

    let profile = analyze_deck(input, catalog, facts);
    let mut adjustments = Vec::new();
    if let Some(card) = catalog.get(card_id) {
        add_curve_adjustment(input, card, catalog, &profile.mana_curve, &mut adjustments);
        add_weapon_pressure_adjustment(input, card, catalog, &mut adjustments);
    }
    add_semantic_adjustments(card_id, facts, &profile.synergy_counts, &mut adjustments);
    add_repeat_adjustment(input, card_id, &mut adjustments);

    let raw_adjustment = adjustments.iter().map(|item| item.delta).sum::<f32>();
    let adjustment = raw_adjustment.clamp(-MAX_DECK_ADJUSTMENT, MAX_DECK_ADJUSTMENT);
    if adjustment != raw_adjustment {
        adjustments.push(ScoreAdjustment {
            kind: AdjustmentKind::AdjustmentLimit,
            delta: adjustment - raw_adjustment,
            detail: format!(
                "Deck adjustments are limited to ±{MAX_DECK_ADJUSTMENT:.0} rating points"
            ),
        });
    }

    OfferScore {
        card_id: card_id.to_owned(),
        base_rating,
        provider_ratings: provider.provider_ratings(card_id, class),
        deck_score: Some(base_value + adjustment),
        adjustment,
        adjustments,
    }
}

fn add_curve_adjustment(
    input: &AnalysisInput,
    card: &CardMetadata,
    catalog: &CardCache,
    curve: &arena_analysis::ManaCurve,
    adjustments: &mut Vec<ScoreAdjustment>,
) {
    let known_slots = curve
        .zero_to_two
        .saturating_add(curve.three)
        .saturating_add(curve.four_to_six)
        .saturating_add(curve.seven_plus);
    let two_drops = input
        .deck
        .iter()
        .filter(|entry| {
            catalog
                .get(&entry.card_id)
                .is_some_and(|card| card.cost == Some(2) && is_board_card(card))
        })
        .fold(0_u16, |total, entry| {
            total.saturating_add(u16::from(entry.count))
        });
    let is_board_two = card.cost == Some(2) && is_board_card(card);
    if known_slots >= 4 && two_drops.saturating_mul(5) < known_slots && is_board_two {
        adjustments.push(ScoreAdjustment {
            kind: AdjustmentKind::MissingTwoDrop,
            delta: 4.0,
            detail: format!(
                "Only {two_drops} two-cost board card(s) in {known_slots} known deck slots"
            ),
        });
        return;
    }

    let Some(cost) = card.cost else {
        return;
    };
    // Generous ceilings prevent normal curve shaping from dominating card
    // quality. They only penalize a clearly crowded band after a useful sample.
    let (band_name, before_count, maximum_percent) = match cost {
        0..=2 => ("0-2", curve.zero_to_two, 45_u32),
        3 => ("3", curve.three, 35),
        4..=6 => ("4-6", curve.four_to_six, 55),
        _ => ("7+", curve.seven_plus, 25),
    };
    let after_slots = u32::from(known_slots) + 1;
    let after_count = u32::from(before_count) + 1;
    if known_slots >= 5 && after_count * 100 > after_slots * maximum_percent {
        adjustments.push(ScoreAdjustment {
            kind: AdjustmentKind::CrowdedCurveBand,
            delta: -3.0,
            detail: format!(
                "The {band_name}-cost band would contain {after_count} of {after_slots} known slots"
            ),
        });
    }
}

fn is_board_card(card: &CardMetadata) -> bool {
    card.card_type
        .as_deref()
        .is_some_and(|kind| matches!(kind.to_ascii_uppercase().as_str(), "MINION" | "WEAPON"))
}

fn add_weapon_pressure_adjustment(
    input: &AnalysisInput,
    offered: &CardMetadata,
    catalog: &CardCache,
    adjustments: &mut Vec<ScoreAdjustment>,
) {
    if !is_weapon(offered) {
        return;
    }
    let Some(offered_charges) = offered.durability else {
        // Missing metadata must not turn into an invented weapon penalty.
        return;
    };
    let (weapon_count, charge_count) =
        input
            .deck
            .iter()
            .fold((0_u16, 0_u16), |(weapons, charges), entry| {
                let Some(card) = catalog.get(&entry.card_id).filter(|card| is_weapon(card)) else {
                    return (weapons, charges);
                };
                let copies = u16::from(entry.count);
                (
                    weapons.saturating_add(copies),
                    charges.saturating_add(
                        u16::from(card.durability.unwrap_or(0)).saturating_mul(copies),
                    ),
                )
            });
    let after_weapons = weapon_count.saturating_add(1);
    let after_charges = charge_count.saturating_add(u16::from(offered_charges));
    // Two ordinary weapons / roughly six swings remain unpenalized. Beyond
    // that, additional base durability competes for hand slots and hero attack
    // turns. The adjustment is intentionally modest and capped.
    if after_weapons >= 3 && after_charges > 6 {
        let excess_charges = after_charges - 6;
        let delta = -f32::from((excess_charges + 1).min(5));
        adjustments.push(ScoreAdjustment {
            kind: AdjustmentKind::TooManyWeaponCharges,
            delta,
            detail: format!(
                "Would reach {after_weapons} weapons and {after_charges} base weapon charges"
            ),
        });
    }
}

fn is_weapon(card: &CardMetadata) -> bool {
    card.card_type
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("WEAPON"))
}

fn add_semantic_adjustments(
    card_id: &str,
    facts: &AnalysisFacts,
    deck_synergies: &BTreeMap<String, u16>,
    adjustments: &mut Vec<ScoreAdjustment>,
) {
    if !facts.contains(card_id) {
        return;
    }
    let tags = facts.tags(card_id);
    for category in tags.iter().filter_map(|tag| tag.strip_prefix("synergy:")) {
        let support = deck_synergies.get(category).copied().unwrap_or(0);
        if support > 0 {
            let delta = f32::from(support.min(3));
            adjustments.push(ScoreAdjustment {
                kind: AdjustmentKind::VerifiedSynergy,
                delta,
                detail: format!("Matches {support} observed `{category}` synergy card(s)"),
            });
        }
    }
    for category in tags.iter().filter_map(|tag| tag.strip_prefix("requires:")) {
        let support = deck_synergies.get(category).copied().unwrap_or(0);
        if support == 0 {
            adjustments.push(ScoreAdjustment {
                kind: AdjustmentKind::UnsupportedPayoff,
                delta: -5.0,
                detail: format!("Requires `{category}` support, but none is observed"),
            });
        } else {
            adjustments.push(ScoreAdjustment {
                kind: AdjustmentKind::SupportedPayoff,
                delta: 3.0,
                detail: format!("Its `{category}` requirement has {support} observed enabler(s)"),
            });
        }
    }
}

fn add_repeat_adjustment(
    input: &AnalysisInput,
    card_id: &str,
    adjustments: &mut Vec<ScoreAdjustment>,
) {
    let existing = input
        .deck
        .iter()
        .filter(|entry| entry.card_id == card_id)
        .fold(0_u16, |total, entry| {
            total.saturating_add(u16::from(entry.count))
        });
    // Do not penalize the ordinary second copy. Beyond that, apply a small,
    // capped diversity adjustment rather than assuming duplicates are bad.
    if existing >= 2 {
        let delta = -f32::from((existing - 1).min(2));
        adjustments.push(ScoreAdjustment {
            kind: AdjustmentKind::RepeatedCard,
            delta,
            detail: format!("The deck already contains {existing} copies"),
        });
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalRatingFile {
    pub provider: String,
    pub data_timestamp: DateTime<Utc>,
    pub arena_season: Option<String>,
    pub data_version: Option<String>,
    pub ratings: Vec<LocalRatingRow>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalRatingRow {
    pub card_id: String,
    pub class: Option<HeroClass>,
    pub value: f32,
    pub sample_size: Option<u64>,
    pub label: Option<String>,
}

pub const HEARTHARENA_TIERLIST_URL: &str = "https://www.heartharena.com/tierlist";

pub const HSREPLAY_ARENA_CARD_STATS_URL: &str =
    "https://hsreplay.net/api/v1/arena/card_stats/free/?format=json";

pub const FIRESTONE_CARDS_BASE_URL: &str = "https://static.zerotoheroes.com/api/arena/stats/cards";

pub const FIRESTONE_CLASS_SLUGS: [&str; 11] = [
    "mage",
    "hunter",
    "warrior",
    "paladin",
    "shaman",
    "priest",
    "warlock",
    "rogue",
    "druid",
    "demonhunter",
    "deathknight",
];

#[derive(Debug, Deserialize)]
struct HsReplayCardStats {
    data: BTreeMap<String, Vec<HsReplayCardStatsRow>>,
    #[serde(default)]
    selected_params: Vec<String>,
    metadata: Option<HsReplayMetadata>,
}

#[derive(Debug, Deserialize)]
struct HsReplayCardStatsRow {
    card_id: String,
    drawn_win_rate: f32,
    num_games: u64,
}

#[derive(Debug, Deserialize)]
struct HsReplayMetadata {
    meta_period_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FirestoneCardStats {
    last_updated: DateTime<Utc>,
    context: String,
    stats: Vec<FirestoneCardStatsRow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FirestoneCardStatsRow {
    card_id: String,
    stats: FirestoneCardStatsValues,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FirestoneCardStatsValues {
    played: u64,
    played_then_win: u64,
}

/// Per-card arena win rates from HSReplay's public card-stats endpoint. The
/// `ALL` bucket becomes the generic fallback rows; each class bucket becomes
/// class-specific rows, matching the local file's lookup order. The chosen
/// signal is `drawn_win_rate` (games where the card was actually drawn), the
/// same measure the official HDT arena helper scores on; the payload carries
/// no server timestamp, so the import time is recorded as the freshness bound.
pub fn import_hsreplay_json(source: &str) -> Result<LocalRatingFile> {
    let value: HsReplayCardStats =
        serde_json::from_str(source).context("HSReplay card-stats response did not parse")?;
    let mut rows = BTreeMap::<(String, Option<HeroClass>), LocalRatingRow>::new();
    for (bucket, cards) in &value.data {
        let class = hsreplay_bucket_class(bucket);
        for card in cards {
            let card_id = card.card_id.trim().to_owned();
            if card_id.is_empty()
                || !card.drawn_win_rate.is_finite()
                || !(0.0..=100.0).contains(&card.drawn_win_rate)
            {
                anyhow::bail!("HSReplay returned an invalid card ID or win rate");
            }
            let row = LocalRatingRow {
                card_id: card_id.clone(),
                class,
                value: card.drawn_win_rate,
                sample_size: Some(card.num_games),
                label: Some("drawn win rate".to_owned()),
            };
            rows.insert((card_id, class), row);
        }
    }
    if rows.len() < 500 {
        anyhow::bail!(
            "HSReplay response is incomplete: only {} unique class/card scores",
            rows.len()
        );
    }
    let data_version = value
        .selected_params
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" + ");
    Ok(LocalRatingFile {
        provider: "HSReplay Arena".to_owned(),
        data_timestamp: Utc::now(),
        arena_season: value
            .metadata
            .and_then(|metadata| metadata.meta_period_id)
            .map(|period| format!("period {period}")),
        data_version: (!data_version.is_empty()).then_some(data_version),
        ratings: rows.into_values().collect(),
    })
}

fn hsreplay_bucket_class(bucket: &str) -> Option<HeroClass> {
    if bucket == "ALL" {
        return None;
    }
    HeroClass::from_log(bucket)
}

/// Per-card arena win rates from one Firestone class file. Firestone publishes
/// one file per class, so an importer merges the eleven class files with
/// [`merge_local_rating_files`]. The value is the played win rate (games where
/// the card was played) expressed as a percentage; cards with no played games
/// are omitted rather than treated as a 0% signal.
pub fn import_firestone_json(source: &str) -> Result<LocalRatingFile> {
    let value: FirestoneCardStats =
        serde_json::from_str(source).context("Firestone card-stats response did not parse")?;
    let class = HeroClass::from_log(&value.context)
        .with_context(|| format!("unknown Firestone class `{}`", value.context))?;
    let mut rows = Vec::with_capacity(value.stats.len());
    for card in value.stats {
        let played = card.stats.played;
        let played_then_win = card.stats.played_then_win;
        if played == 0 {
            continue;
        }
        let card_id = card.card_id.trim().to_owned();
        if card_id.is_empty() {
            anyhow::bail!("Firestone returned an empty card ID");
        }
        let win_rate = played_then_win as f32 * 100.0 / played as f32;
        if !win_rate.is_finite() || !(0.0..=100.0).contains(&win_rate) {
            anyhow::bail!("Firestone returned an invalid played win rate for {card_id}");
        }
        rows.push(LocalRatingRow {
            card_id,
            class: Some(class),
            value: win_rate,
            sample_size: Some(played),
            label: Some("played win rate".to_owned()),
        });
    }
    Ok(LocalRatingFile {
        provider: "Firestone".to_owned(),
        data_timestamp: value.last_updated,
        arena_season: None,
        data_version: Some("last-patch".to_owned()),
        ratings: rows,
    })
}

/// Combine independently fetched source files into one local rating cache,
/// keeping the most recent data timestamp and rejecting conflicting scores
/// for the same class/card pair.
pub fn merge_local_rating_files(
    provider: &str,
    files: Vec<LocalRatingFile>,
) -> Result<LocalRatingFile> {
    let mut merged = BTreeMap::<(String, Option<HeroClass>), LocalRatingRow>::new();
    let mut data_timestamp = None;
    for file in files {
        data_timestamp = Some(
            data_timestamp
                .map(|existing: DateTime<Utc>| existing.max(file.data_timestamp))
                .unwrap_or(file.data_timestamp),
        );
        for row in file.ratings {
            let key = (row.card_id.clone(), row.class);
            let value = row.value;
            if let Some(previous) = merged.insert(key, row) {
                if previous.value != value {
                    anyhow::bail!(
                        "Conflicting scores for {} from merged rating files",
                        previous.card_id
                    );
                }
            }
        }
    }
    Ok(LocalRatingFile {
        provider: provider.to_owned(),
        data_timestamp: data_timestamp.context("no rating files to merge")?,
        arena_season: None,
        data_version: None,
        ratings: merged.into_values().collect(),
    })
}

/// Imports every class-specific and Neutral score from one public HearthArena
/// tier-list response. Card IDs come from the site's own render URLs, avoiding
/// ambiguous display-name matching. An incomplete response is rejected before
/// it can replace a previously verified local cache.
pub fn import_heartharena_html(source: &str) -> Result<LocalRatingFile> {
    let section =
        Regex::new(r#"(?s)<section class="tab tierlist[^"]*" id="([^"]+)">(.*?)</section>"#)?;
    let card = Regex::new(
        r#"(?s)data-card-image="[^"]*/([^/"?]+)\.(?:webp|png)"[^>]*>.*?</dt>\s*<dd[^>]*>\s*([0-9]+)"#,
    )?;
    let changelog = Regex::new(
        r#"<h3 class="table-header">\s*([0-9]{1,2}/[0-9]{1,2}/[0-9]{2,4}):\s*([^<]+)</h3>"#,
    )?;

    let expected_sections = [
        "death-knight",
        "demon-hunter",
        "druid",
        "hunter",
        "mage",
        "paladin",
        "priest",
        "rogue",
        "shaman",
        "warlock",
        "warrior",
        "any",
    ];
    let mut observed_sections = BTreeMap::new();
    let mut rows = BTreeMap::<(String, Option<HeroClass>), LocalRatingRow>::new();
    for capture in section.captures_iter(source) {
        let section_id = &capture[1];
        let Some(class) = heartharena_section_class(section_id) else {
            continue;
        };
        let mut count = 0_usize;
        for rating in card.captures_iter(&capture[2]) {
            let card_id = rating[1].trim().to_owned();
            let value = rating[2].parse::<f32>()?;
            if card_id.is_empty() || !value.is_finite() || !(0.0..=200.0).contains(&value) {
                anyhow::bail!("HearthArena returned an invalid card ID or score");
            }
            let row = LocalRatingRow {
                card_id: card_id.clone(),
                class,
                value,
                sample_size: None,
                label: Some(heartharena_score_label(value).to_owned()),
            };
            let key = (card_id, class);
            if let Some(previous) = rows.insert(key, row) {
                if previous.value != value {
                    anyhow::bail!(
                        "HearthArena returned conflicting scores for {}",
                        previous.card_id
                    );
                }
            }
            count += 1;
        }
        observed_sections.insert(section_id.to_owned(), count);
    }
    for section_id in expected_sections {
        let count = observed_sections.get(section_id).copied().unwrap_or(0);
        if count < 40 {
            anyhow::bail!(
                "HearthArena response is incomplete: section `{section_id}` has only {count} scores"
            );
        }
    }
    if rows.len() < 1_000 {
        anyhow::bail!(
            "HearthArena response is incomplete: only {} unique class/card scores",
            rows.len()
        );
    }

    let (data_timestamp, data_version) = changelog
        .captures(source)
        .and_then(|capture| {
            let date = NaiveDate::parse_from_str(&capture[1], "%m/%d/%y")
                .or_else(|_| NaiveDate::parse_from_str(&capture[1], "%m/%d/%Y"))
                .ok()?;
            let timestamp = date.and_hms_opt(0, 0, 0)?.and_utc();
            Some((timestamp, format!("{}: {}", &capture[1], capture[2].trim())))
        })
        .context("HearthArena response did not contain a parseable changelog date")?;

    Ok(LocalRatingFile {
        provider: "HearthArena public tier list".to_owned(),
        data_timestamp,
        arena_season: None,
        data_version: Some(data_version),
        ratings: rows.into_values().collect(),
    })
}

pub fn write_local_rating_file(path: impl AsRef<Path>, ratings: &LocalRatingFile) -> Result<()> {
    let path = path.as_ref();
    let parent = path.parent().context("rating cache path has no parent")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "could not create rating cache directory {}",
            parent.display()
        )
    })?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(ratings)?).with_context(|| {
        format!(
            "could not write temporary rating cache {}",
            temporary.display()
        )
    })?;
    fs::rename(&temporary, path)
        .with_context(|| format!("could not replace rating cache {}", path.display()))?;
    Ok(())
}

fn heartharena_section_class(section: &str) -> Option<Option<HeroClass>> {
    Some(match section {
        "death-knight" => Some(HeroClass::DeathKnight),
        "demon-hunter" => Some(HeroClass::DemonHunter),
        "druid" => Some(HeroClass::Druid),
        "hunter" => Some(HeroClass::Hunter),
        "mage" => Some(HeroClass::Mage),
        "paladin" => Some(HeroClass::Paladin),
        "priest" => Some(HeroClass::Priest),
        "rogue" => Some(HeroClass::Rogue),
        "shaman" => Some(HeroClass::Shaman),
        "warlock" => Some(HeroClass::Warlock),
        "warrior" => Some(HeroClass::Warrior),
        "any" => None,
        _ => return None,
    })
}

fn heartharena_score_label(value: f32) -> &'static str {
    match value as u16 {
        100.. => "Great",
        90..=99 => "Good",
        80..=89 => "Above average",
        70..=79 => "Average",
        60..=69 => "Below average",
        40..=59 => "Bad",
        _ => "Terrible",
    }
}

#[derive(Clone, Debug)]
pub struct LocalJsonRatingProvider {
    metadata: ProviderMetadata,
    ratings: BTreeMap<(String, Option<HeroClass>), CardRating>,
}

impl LocalJsonRatingProvider {
    pub fn from_file(file: LocalRatingFile) -> Self {
        let metadata = ProviderMetadata {
            provider: file.provider,
            data_timestamp: file.data_timestamp,
            arena_season: file.arena_season,
            data_version: file.data_version,
        };
        let ratings = file
            .ratings
            .into_iter()
            .map(|row| {
                let key = (row.card_id.clone(), row.class);
                let rating = CardRating {
                    card_id: row.card_id,
                    value: row.value,
                    sample_size: row.sample_size,
                    label: row.label,
                };
                (key, rating)
            })
            .collect();
        Self { metadata, ratings }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let input = fs::read_to_string(path)
            .with_context(|| format!("could not read rating file {}", path.display()))?;
        let file: LocalRatingFile = serde_json::from_str(&input)
            .with_context(|| format!("could not parse rating file {}", path.display()))?;
        Ok(Self::from_file(file))
    }
}

impl RatingProvider for LocalJsonRatingProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn rating(&self, card_id: &str, class: Option<HeroClass>) -> Option<CardRating> {
        self.ratings
            .get(&(card_id.to_owned(), class))
            .or_else(|| self.ratings.get(&(card_id.to_owned(), None)))
            .cloned()
    }
}

/// The values behind [`LocalJsonRatingProvider::rating`], used as the scale
/// anchor when this provider joins a composite.
pub fn local_rating_values(provider: &LocalJsonRatingProvider) -> Vec<f32> {
    provider
        .ratings
        .values()
        .map(|rating| rating.value)
        .collect()
}

/// Joins several providers into one 0-100 score per card while keeping each
/// source's own rating visible. Composite membership follows the per-source
/// lookup order (class-specific first, generic fallback second), so a card
/// missing from a class-specific provider still joins on its generic value.
///
/// Normalization is outlier-aware and matches the HDT arena-helper recipe: each
/// provider's value is centered on its robust median and scaled by its robust
/// spread (MAD), then pushed through a logistic curve. A card exactly at the
/// provider's median maps to 50; a card a few MADs away saturates toward 0 or
/// 100 but is never clamped, so outliers stay outliers instead of being
/// squeezed into a fixed range. Because every source is normalized before the
/// equal-weight mean, the composite never inherits a unit mismatch between
/// HearthArena tier scores and win-rate percentages.
#[derive(Clone, Debug)]
pub struct CompositeRatingProvider {
    sources: Vec<CompositeSource>,
    metadata: ProviderMetadata,
}

#[derive(Clone, Debug)]
struct CompositeSource {
    provider: LocalJsonRatingProvider,
    center: f32,
    spread: f32,
}

impl CompositeRatingProvider {
    pub fn from_providers(providers: Vec<LocalJsonRatingProvider>) -> Result<Self> {
        anyhow::ensure!(
            !providers.is_empty(),
            "a composite rating provider needs at least one source"
        );
        let mut sources = Vec::with_capacity(providers.len());
        let mut provider_names = Vec::with_capacity(providers.len());
        let mut data_timestamps = Vec::with_capacity(providers.len());
        for provider in providers {
            let (center, spread) = robust_scale(&local_rating_values(&provider));
            provider_names.push(provider.metadata.provider.clone());
            data_timestamps.push(provider.metadata.data_timestamp);
            sources.push(CompositeSource {
                provider,
                center,
                spread,
            });
        }
        let metadata = ProviderMetadata {
            provider: format!("Composite: {}", provider_names.join(" + ")),
            data_timestamp: data_timestamps
                .into_iter()
                .min()
                .context("no providers to composite")?,
            arena_season: None,
            data_version: None,
        };
        Ok(Self { sources, metadata })
    }
}

impl RatingProvider for CompositeRatingProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn rating(&self, card_id: &str, class: Option<HeroClass>) -> Option<CardRating> {
        let mut total = 0.0_f32;
        let mut joined = 0_u32;
        let mut sample_size = 0_u64;
        for source in &self.sources {
            let Some(rating) = source.provider.rating(card_id, class) else {
                continue;
            };
            total += normalize_score(rating.value, source.center, source.spread);
            joined += 1;
            sample_size = sample_size.saturating_add(rating.sample_size.unwrap_or(0));
        }
        if joined == 0 {
            return None;
        }
        Some(CardRating {
            card_id: card_id.to_owned(),
            value: total / joined as f32,
            sample_size: (sample_size > 0).then_some(sample_size),
            label: Some(format!("mean of {joined} normalized source scores")),
        })
    }

    fn provider_ratings(&self, card_id: &str, class: Option<HeroClass>) -> Vec<ProviderRating> {
        self.sources
            .iter()
            .filter_map(|source| {
                source
                    .provider
                    .rating(card_id, class)
                    .map(|rating| ProviderRating {
                        provider: source.provider.metadata().clone(),
                        rating,
                    })
            })
            .collect()
    }
}

/// Robust location and spread of a provider's rating values: the median and a
/// MAD scaled to a consistent estimator of the standard deviation. A zero
/// spread (all scores identical) falls back to a floor so normalization stays
/// defined rather than dividing by zero.
fn robust_scale(values: &[f32]) -> (f32, f32) {
    if values.is_empty() {
        return (0.0, 1.0);
    }
    let mut sorted: Vec<f32> = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let center = sorted[sorted.len() / 2];
    let mut deviations: Vec<f32> = sorted.iter().map(|value| (value - center).abs()).collect();
    deviations.sort_by(|a, b| a.total_cmp(b));
    let mad = deviations[deviations.len() / 2];
    let spread = (mad * 1.4826).max(1.0);
    debug_assert!(spread.is_finite() && spread > 0.0);
    (center, spread)
}

fn normalize_score(value: f32, center: f32, spread: f32) -> f32 {
    let z = (value - center) / spread;
    (50.0 + 50.0 * z.tanh()).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hs_state::DeckCard;
    use std::{
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct FixtureProvider {
        metadata: ProviderMetadata,
        values: BTreeMap<String, f32>,
    }

    impl FixtureProvider {
        fn with_ratings(ids: &[&str]) -> Self {
            Self {
                metadata: ProviderMetadata {
                    provider: "fixture".into(),
                    data_timestamp: "2026-01-01T00:00:00Z".parse().unwrap(),
                    arena_season: Some("test".into()),
                    data_version: Some("1".into()),
                },
                values: ids.iter().map(|id| ((*id).to_owned(), 70.0)).collect(),
            }
        }
    }

    impl RatingProvider for FixtureProvider {
        fn metadata(&self) -> &ProviderMetadata {
            &self.metadata
        }

        fn rating(&self, card_id: &str, _class: Option<HeroClass>) -> Option<CardRating> {
            self.values.get(card_id).map(|value| CardRating {
                card_id: card_id.to_owned(),
                value: *value,
                sample_size: Some(100),
                label: None,
            })
        }
    }

    fn catalog() -> CardCache {
        CardCache::from_json(
            r#"[
              {"id":"TWO","name":"Board Two","cost":2,"type":"MINION"},
              {"id":"TWO_SPELL","name":"Two Spell","cost":2,"type":"SPELL"},
              {"id":"THREE","name":"Three","cost":3,"type":"MINION"},
              {"id":"FIVE","name":"Five","cost":5,"type":"MINION"},
              {"id":"SEVEN","name":"Seven","cost":7,"type":"MINION"},
              {"id":"SUPPORT","name":"Elemental","cost":4,"type":"MINION"},
              {"id":"PAYOFF","name":"Payoff","cost":3,"type":"MINION"},
              {"id":"MULTI","name":"Multi Payoff","cost":3,"type":"MINION"}
              ,{"id":"BLADE_TWO","name":"Short Blade","cost":2,"durability":2,"type":"WEAPON"}
              ,{"id":"BLADE_THREE","name":"Long Blade","cost":3,"durability":3,"type":"WEAPON"}
              ,{"id":"BLADE_UNKNOWN","name":"Unknown Blade","cost":3,"type":"WEAPON"}
            ]"#,
        )
        .unwrap()
    }

    fn facts() -> AnalysisFacts {
        AnalysisFacts::from_json(
            r#"{
              "schemaVersion":1,
              "source":"fixture",
              "dataVersion":"1",
              "cards":[
                {"cardId":"TWO","tags":[]},
                {"cardId":"TWO_SPELL","tags":[]},
                {"cardId":"THREE","tags":[]},
                {"cardId":"FIVE","tags":[]},
                {"cardId":"SEVEN","tags":[]},
                {"cardId":"SUPPORT","tags":["synergy:elemental"]},
                {"cardId":"PAYOFF","tags":["requires:elemental"]},
                {"cardId":"MULTI","tags":["requires:a","requires:b","requires:c"]}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn heartharena_import_reads_all_classes_by_exact_card_id() {
        let sections = [
            "death-knight",
            "demon-hunter",
            "druid",
            "hunter",
            "mage",
            "paladin",
            "priest",
            "rogue",
            "shaman",
            "warlock",
            "warrior",
            "any",
        ];
        let mut html = String::new();
        for section in sections {
            html.push_str(&format!(
                "<section class=\"tab tierlist {section}\" id=\"{section}\">"
            ));
            for index in 0..100 {
                html.push_str(&format!(
                    "<dt data-card-image=\"https://cdn.heartharena.com/images/renders/enUS/CARD_{index:03}.webp\">Card {index}</dt><dd class=\"score score_95\">95</dd>"
                ));
            }
            html.push_str("</section>");
        }
        html.push_str("<h3 class=\"table-header\">7/1/26: Post-Release Update</h3>");
        let imported = import_heartharena_html(&html).unwrap();
        assert_eq!(imported.ratings.len(), 1_200);
        assert_eq!(
            imported.data_timestamp.to_rfc3339(),
            "2026-07-01T00:00:00+00:00"
        );
        assert_eq!(
            imported
                .ratings
                .iter()
                .find(|row| row.card_id == "CARD_000" && row.class == Some(HeroClass::Rogue))
                .unwrap()
                .value,
            95.0
        );
    }

    #[test]
    fn heartharena_import_rejects_partial_page() {
        assert!(import_heartharena_html("<html>temporarily unavailable</html>").is_err());
    }

    #[test]
    fn hsreplay_import_uses_drawn_win_rate_and_all_bucket_as_generic() {
        let mut rows = String::new();
        for index in 0..520 {
            rows.push_str(&format!(
                "{{\"card_id\":\"C{index:03}\",\"drawn_win_rate\":50.0,\"num_games\":100}}"
            ));
            rows.push(',');
        }
        rows.push_str("{\"card_id\":\"C001\",\"drawn_win_rate\":48.0,\"num_games\":90}");
        let source = format!(
            r#"{{
                "data": {{
                    "ALL": [{rows}],
                    "MAGE": [{{"card_id":"C001","drawn_win_rate":48.0,"num_games":90}}]
                }},
                "selected_params": ["BGT_UNDERGROUND_ARENA", "LAST_4_DAYS"],
                "metadata": {{"meta_period_id": 16}}
            }}"#
        );
        let imported = import_hsreplay_json(&source).unwrap();
        assert_eq!(imported.ratings.len(), 521);
        assert_eq!(imported.arena_season.as_deref(), Some("period 16"));
        assert_eq!(
            imported.data_version.as_deref(),
            Some("BGT_UNDERGROUND_ARENA + LAST_4_DAYS")
        );
        let generic = imported
            .ratings
            .iter()
            .find(|row| row.card_id == "C001" && row.class.is_none())
            .unwrap();
        assert_eq!(generic.value, 48.0);
        assert_eq!(generic.sample_size, Some(90));
        assert_eq!(generic.label.as_deref(), Some("drawn win rate"));
        let mage = imported
            .ratings
            .iter()
            .find(|row| row.class == Some(HeroClass::Mage))
            .unwrap();
        assert_eq!(mage.value, 48.0);
        assert_eq!(mage.sample_size, Some(90));
    }

    #[test]
    fn hsreplay_import_rejects_an_incomplete_response() {
        let source = r#"{"data":{"ALL":[{"card_id":"C1","drawn_win_rate":50.0,"num_games":10}]}}"#;
        assert!(import_hsreplay_json(source).is_err());
    }

    #[test]
    fn firestone_import_uses_played_win_rate_and_skips_unplayed() {
        let source = r#"{
            "lastUpdated": "2026-08-05T11:25:40Z",
            "context": "mage",
            "stats": [
                {"cardId":"C1","context":"mage","stats":{"played":200,"playedThenWin":130}},
                {"cardId":"C2","context":"mage","stats":{"played":0,"playedThenWin":0}},
                {"cardId":"C3","context":"mage","stats":{"played":40,"playedThenWin":40}}
            ]
        }"#;
        let imported = import_firestone_json(source).unwrap();
        assert_eq!(imported.ratings.len(), 2);
        assert_eq!(imported.provider, "Firestone");
        assert_eq!(
            imported.data_timestamp.to_rfc3339(),
            "2026-08-05T11:25:40+00:00"
        );
        let c1 = imported
            .ratings
            .iter()
            .find(|row| row.card_id == "C1")
            .unwrap();
        assert!((c1.value - 65.0).abs() < 1e-4, "got {}", c1.value);
        assert_eq!(c1.sample_size, Some(200));
        assert_eq!(c1.class, Some(HeroClass::Mage));
        let c3 = imported
            .ratings
            .iter()
            .find(|row| row.card_id == "C3")
            .unwrap();
        assert!((c3.value - 100.0).abs() < 1e-4, "got {}", c3.value);
    }

    #[test]
    fn merging_firestone_files_keeps_newest_timestamp_and_rejects_conflicts() {
        let file = |card_id: &str, class: Option<HeroClass>, value: f32, timestamp: &str| {
            LocalRatingFile {
                provider: "Firestone".to_owned(),
                data_timestamp: timestamp.parse().unwrap(),
                arena_season: None,
                data_version: None,
                ratings: vec![LocalRatingRow {
                    card_id: card_id.to_owned(),
                    class,
                    value,
                    sample_size: Some(10),
                    label: None,
                }],
            }
        };
        let mage = file("C1", Some(HeroClass::Mage), 60.0, "2026-08-05T00:00:00Z");
        let hunter = file("C2", Some(HeroClass::Hunter), 70.0, "2026-08-05T12:00:00Z");
        let merged = merge_local_rating_files("Firestone", vec![mage.clone(), hunter]).unwrap();
        assert_eq!(merged.ratings.len(), 2);
        assert_eq!(
            merged.data_timestamp.to_rfc3339(),
            "2026-08-05T12:00:00+00:00"
        );

        let conflict = file("C1", Some(HeroClass::Mage), 61.0, "2026-08-05T12:00:00Z");
        assert!(merge_local_rating_files("Firestone", vec![mage, conflict]).is_err());
    }

    #[test]
    fn composite_normalizes_sources_then_joins_outliers_remain_outliers() {
        let tier = LocalJsonRatingProvider::from_file(LocalRatingFile {
            provider: "HearthArena public tier list".to_owned(),
            data_timestamp: "2026-01-01T00:00:00Z".parse().unwrap(),
            arena_season: None,
            data_version: None,
            ratings: (0..100)
                .map(|index| LocalRatingRow {
                    card_id: format!("C{index:03}"),
                    class: None,
                    value: 73.0 + index as f32 * 0.5,
                    sample_size: None,
                    label: None,
                })
                .collect(),
        });
        let winrate = LocalJsonRatingProvider::from_file(LocalRatingFile {
            provider: "HSReplay Arena".to_owned(),
            data_timestamp: "2026-01-01T00:00:00Z".parse().unwrap(),
            arena_season: None,
            data_version: None,
            ratings: (0..100)
                .map(|index| LocalRatingRow {
                    card_id: format!("C{index:03}"),
                    class: None,
                    value: 50.0 + index as f32 * 0.1,
                    sample_size: Some(100),
                    label: None,
                })
                .collect(),
        });
        let composite = CompositeRatingProvider::from_providers(vec![tier, winrate]).unwrap();

        // Both sources rank C099 highest; its joined score must saturate high
        // even though its native tier value (122.5) is far outside the
        // 1-145 tier scale midpoint. The median card sits near 50 in each
        // source, so the composite anchors there.
        let best = composite.rating("C099", None).unwrap().value;
        let median_card = composite.rating("C049", None).unwrap().value;
        assert!(best > 80.0, "outlier must stay an outlier, got {best}");
        assert!(
            (median_card - 50.0).abs() < 10.0,
            "median card should sit near 50, got {median_card}"
        );

        // Per-source scales stay visible behind the joined number.
        let sources = composite.provider_ratings("C099", None);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].provider.provider, "HearthArena public tier list");
        assert_eq!(sources[1].provider.provider, "HSReplay Arena");
        assert!(sources[0].rating.value > sources[1].rating.value);
    }

    #[test]
    fn composite_joins_a_card_missing_from_one_source() {
        let tier = LocalJsonRatingProvider::from_file(LocalRatingFile {
            provider: "HearthArena public tier list".to_owned(),
            data_timestamp: "2026-01-01T00:00:00Z".parse().unwrap(),
            arena_season: None,
            data_version: None,
            ratings: (0..100)
                .map(|index| LocalRatingRow {
                    card_id: format!("C{index:03}"),
                    class: None,
                    value: 70.0,
                    sample_size: None,
                    label: None,
                })
                .collect(),
        });
        let winrate = LocalJsonRatingProvider::from_file(LocalRatingFile {
            provider: "HSReplay Arena".to_owned(),
            data_timestamp: "2026-01-01T00:00:00Z".parse().unwrap(),
            arena_season: None,
            data_version: None,
            ratings: (0..99)
                .map(|index| LocalRatingRow {
                    card_id: format!("C{index:03}"),
                    class: None,
                    value: 50.0,
                    sample_size: Some(100),
                    label: None,
                })
                .collect(),
        });
        let composite = CompositeRatingProvider::from_providers(vec![tier, winrate]).unwrap();

        // C099 is missing from the second source; the composite still joins on
        // the one available source instead of dropping the card entirely.
        assert!(composite.rating("C099", None).is_some());
        assert_eq!(composite.provider_ratings("C099", None).len(), 1);
    }

    fn input(cards: &[(&str, u8)]) -> AnalysisInput {
        AnalysisInput {
            deck: cards
                .iter()
                .map(|(card_id, count)| DeckCard {
                    card_id: (*card_id).to_owned(),
                    count: *count,
                })
                .collect(),
            expected_slots: Some(30),
        }
    }

    fn adjustment(score: &OfferScore, kind: AdjustmentKind) -> Option<f32> {
        score
            .adjustments
            .iter()
            .find(|item| item.kind == kind)
            .map(|item| item.delta)
    }

    #[test]
    fn class_specific_rating_wins_with_generic_fallback() {
        let file = env::temp_dir().join(format!(
            "arena-next-ratings-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &file,
            r#"{"provider":"fixture","dataTimestamp":"2026-01-01T00:00:00Z","arenaSeason":"test","ratings":[{"cardId":"CS2_029","value":1.0},{"cardId":"CS2_029","class":"mage","value":2.0}]}"#,
        )
        .unwrap();
        let provider = LocalJsonRatingProvider::load(&file).unwrap();
        assert_eq!(
            provider
                .rating("CS2_029", Some(HeroClass::Mage))
                .unwrap()
                .value,
            2.0
        );
        assert_eq!(
            provider
                .rating("CS2_029", Some(HeroClass::Hunter))
                .unwrap()
                .value,
            1.0
        );
        fs::remove_file(file).unwrap();
    }

    #[test]
    fn missing_two_drops_boosts_only_a_two_cost_board_card() {
        let provider = FixtureProvider::with_ratings(&["TWO", "TWO_SPELL"]);
        let deck = input(&[("THREE", 2), ("FIVE", 2), ("SEVEN", 1)]);
        let board_two = score_offer(&provider, None, &deck, "TWO", &catalog(), &facts());
        assert_eq!(board_two.base_rating.as_ref().unwrap().value, 70.0);
        assert_eq!(board_two.deck_score, Some(74.0));
        assert_eq!(
            adjustment(&board_two, AdjustmentKind::MissingTwoDrop),
            Some(4.0)
        );

        let spell = score_offer(&provider, None, &deck, "TWO_SPELL", &catalog(), &facts());
        assert_eq!(adjustment(&spell, AdjustmentKind::MissingTwoDrop), None);
    }

    #[test]
    fn a_crowded_curve_band_is_penalized() {
        let provider = FixtureProvider::with_ratings(&["FIVE"]);
        let score = score_offer(
            &provider,
            None,
            &input(&[("FIVE", 4), ("THREE", 2)]),
            "FIVE",
            &catalog(),
            &facts(),
        );
        assert_eq!(
            adjustment(&score, AdjustmentKind::CrowdedCurveBand),
            Some(-3.0)
        );
        assert_eq!(score.deck_score, Some(65.0));
    }

    #[test]
    fn excessive_weapon_pressure_counts_copies_and_base_durability() {
        let provider = FixtureProvider::with_ratings(&["BLADE_TWO", "BLADE_THREE"]);
        let score = score_offer(
            &provider,
            None,
            &input(&[("BLADE_THREE", 2)]),
            "BLADE_TWO",
            &catalog(),
            &facts(),
        );
        assert_eq!(
            adjustment(&score, AdjustmentKind::TooManyWeaponCharges),
            Some(-3.0)
        );
        assert!(score.adjustments.iter().any(|item| {
            item.kind == AdjustmentKind::TooManyWeaponCharges
                && item.detail.contains("3 weapons")
                && item.detail.contains("8 base weapon charges")
        }));
    }

    #[test]
    fn two_weapons_and_unknown_durability_do_not_invent_a_penalty() {
        let provider = FixtureProvider::with_ratings(&["BLADE_TWO", "BLADE_UNKNOWN"]);
        let normal = score_offer(
            &provider,
            None,
            &input(&[("BLADE_TWO", 1)]),
            "BLADE_TWO",
            &catalog(),
            &facts(),
        );
        assert_eq!(
            adjustment(&normal, AdjustmentKind::TooManyWeaponCharges),
            None
        );
        let unknown = score_offer(
            &provider,
            None,
            &input(&[("BLADE_THREE", 3)]),
            "BLADE_UNKNOWN",
            &catalog(),
            &facts(),
        );
        assert_eq!(
            adjustment(&unknown, AdjustmentKind::TooManyWeaponCharges),
            None
        );
    }

    #[test]
    fn verified_support_and_payoff_requirements_change_the_deck_score() {
        let provider = FixtureProvider::with_ratings(&["SUPPORT", "PAYOFF"]);
        let supported_deck = input(&[("SUPPORT", 2), ("FIVE", 2)]);
        let support = score_offer(
            &provider,
            None,
            &supported_deck,
            "SUPPORT",
            &catalog(),
            &facts(),
        );
        assert_eq!(
            adjustment(&support, AdjustmentKind::VerifiedSynergy),
            Some(2.0)
        );

        let payoff = score_offer(
            &provider,
            None,
            &supported_deck,
            "PAYOFF",
            &catalog(),
            &facts(),
        );
        assert_eq!(
            adjustment(&payoff, AdjustmentKind::SupportedPayoff),
            Some(3.0)
        );

        let unsupported = score_offer(
            &provider,
            None,
            &input(&[("FIVE", 2), ("THREE", 2)]),
            "PAYOFF",
            &catalog(),
            &facts(),
        );
        assert_eq!(
            adjustment(&unsupported, AdjustmentKind::UnsupportedPayoff),
            Some(-5.0)
        );
    }

    #[test]
    fn missing_provider_rating_never_invents_a_base_or_deck_score() {
        let score = score_offer(
            &FixtureProvider::with_ratings(&[]),
            None,
            &input(&[("FIVE", 5)]),
            "TWO",
            &catalog(),
            &facts(),
        );
        assert!(score.base_rating.is_none());
        assert!(score.deck_score.is_none());
        assert_eq!(score.adjustment, 0.0);
        assert!(score.adjustments.is_empty());
    }

    #[test]
    fn duplicate_rows_and_counts_are_aggregated_for_repeat_adjustment() {
        let provider = FixtureProvider::with_ratings(&["SUPPORT"]);
        let score = score_offer(
            &provider,
            None,
            &input(&[("SUPPORT", 1), ("SUPPORT", 2)]),
            "SUPPORT",
            &catalog(),
            &facts(),
        );
        assert_eq!(
            adjustment(&score, AdjustmentKind::VerifiedSynergy),
            Some(3.0)
        );
        assert_eq!(adjustment(&score, AdjustmentKind::RepeatedCard), Some(-2.0));
        assert!(
            score
                .adjustments
                .iter()
                .all(|item| !item.detail.trim().is_empty())
        );
        assert_eq!(
            score.adjustments.iter().map(|item| item.delta).sum::<f32>(),
            score.adjustment
        );
    }

    #[test]
    fn adjustment_limit_is_bounded_and_visible_in_the_reasons() {
        let score = score_offer(
            &FixtureProvider::with_ratings(&["MULTI"]),
            None,
            &AnalysisInput::default(),
            "MULTI",
            &catalog(),
            &facts(),
        );
        assert_eq!(score.adjustment, -MAX_DECK_ADJUSTMENT);
        assert_eq!(score.deck_score, Some(58.0));
        assert_eq!(
            adjustment(&score, AdjustmentKind::AdjustmentLimit),
            Some(3.0)
        );
        assert_eq!(
            score.adjustments.iter().map(|item| item.delta).sum::<f32>(),
            score.adjustment
        );
    }
}
